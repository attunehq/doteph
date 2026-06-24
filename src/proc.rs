//! Cross-platform process and shell helpers.
//!
//! eph runs `run=` services, `post-start`/`pre-stop` hooks, and shell health
//! checks through the platform shell, and it manages the lifecycle of detached
//! `run=` processes by PID. Both of those were POSIX-only: command strings went
//! through `sh -c`, and liveness/teardown shelled out to `kill`. Native Windows
//! has neither, so this module hides the platform split behind a small surface
//! and the rest of the service layer stays platform-agnostic.
//!
//! - Shell: `sh -c <cmd>` on Unix (unchanged), `cmd /C <cmd>` on Windows. `cmd`
//!   is the closest analog to `sh -c`: it takes a single command string, the
//!   child inherits the environment, and it returns the command's own exit code.
//! - Liveness: native, via the process group (Unix) or [`sysinfo`] (Windows), so
//!   no POSIX `kill` or Windows `taskkill` binary is needed.
//!
//! Teardown kills the **whole process tree** a `run=` shell spawns, not just the
//! tracked wrapper PID. A compound command (`a && b`, a pipeline, a backgrounded
//! child) makes the shell fork children; signaling only the wrapper would orphan
//! them, and they would survive `eph down` / `eph clean`. eph is a daemonless
//! CLI (the `eph up` that spawns a service exits, and a separate `eph down`
//! reads the PID from state and tears it down), so the teardown mechanism has to
//! be addressable across processes by something already in state, the PID:
//!
//! - Unix: the shell is spawned as the leader of a new process group (see
//!   [`prepare_detached`]) whose PGID equals its PID. Teardown signals the group
//!   (`SIGTERM` then `SIGKILL`, the historical sequence), reaching every
//!   descendant in one **race-free** call. `killpg` works from any process, so a
//!   later `eph down` needs nothing but the PID.
//! - Windows: there is no equivalent an unrelated `eph down` can address. A Job
//!   Object would be the natural fit, but a named job cannot be reattached after
//!   the `eph up` that created it exits: closing the last handle releases the
//!   object's name immediately (even while its processes run), so a later
//!   `OpenJobObject` by name fails. Keeping the name alive needs a persistent
//!   handle holder, which a daemonless CLI does not have. Teardown therefore
//!   walks the live process table and terminates the wrapper together with every
//!   process that descends from it. A child spawned after that snapshot can
//!   escape, the accepted limitation of snapshot-based teardown.
//!
//! On Unix the same descendant walk is the fallback for a service recorded
//! before eph grouped its shells (legacy on-disk state, where the wrapper leads
//! no group).

use std::num::NonZeroU32;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};
use tokio::process::Command as TokioCommand;

/// Build a [`TokioCommand`] that runs `cmd` through the platform shell.
///
/// Only the program and its "run this command string" flag are set; the caller
/// adds the working directory, environment, stdio, and spawns it. On Unix this
/// is `sh -c <cmd>` (eph's historical behavior, preserved byte-for-byte); on
/// Windows it is `cmd /C <cmd>`.
///
/// Note that this makes eph provide *a* shell on each platform, not a portable
/// one: a command string written for `sh` (pipes, `$VAR`, `&&`) will not
/// necessarily run under `cmd`. The portability of the command itself is the
/// `.eph` author's responsibility, the same way it always was on Unix.
pub fn shell_command(cmd: &str) -> TokioCommand {
    #[cfg(unix)]
    let (program, flag) = ("sh", "-c");
    #[cfg(windows)]
    let (program, flag) = ("cmd", "/C");

    let mut command = TokioCommand::new(program);
    command.arg(flag).arg(cmd);
    command
}

/// Configure `command` so a detached `run=` shell heads its own process group
/// (Unix), letting teardown signal the whole tree it forks instead of just the
/// wrapper.
///
/// On Unix the child becomes the leader of a new process group whose PGID equals
/// its PID (`process_group(0)`); [`terminate`]/[`force_kill`] then signal that
/// group. This is set **only** on the detached service spawn, not on hooks or
/// health checks: those are awaited in the foreground, and putting them in their
/// own group would stop the terminal's Ctrl-C (`SIGINT`) from reaching them.
///
/// On Windows there is no pre-spawn step (teardown walks the descendant tree, see
/// the module docs), so this is a no-op.
pub fn prepare_detached(command: &mut TokioCommand) {
    // A PGID of 0 makes the child its own group leader (PGID == PID).
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Refresh a fresh [`System`] so it knows only about `pid`, returning it
/// alongside the `sysinfo` [`Pid`]. Nothing beyond bare existence is collected
/// (`ProcessRefreshKind::nothing()`), since callers only ask "is it there?";
/// `remove_dead_processes` is `true` so a process that has exited is dropped and
/// therefore reads as not-present.
fn snapshot(pid: NonZeroU32) -> (System, Pid) {
    let pid = Pid::from_u32(pid.get());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    (system, pid)
}

/// Whether a process with `pid` is present in the OS process table (a native
/// lookup that replaces the historical `kill -0 <pid>` probe).
fn pid_in_table(pid: NonZeroU32) -> bool {
    let (system, pid) = snapshot(pid);
    system.process(pid).is_some()
}

/// Whether the `run=` service tracked as `pid` is still alive.
///
/// On Unix this probes the process **group** (`killpg(pid, 0)`), not just the
/// leader PID, so a service whose shell exited but left a backgrounded child
/// running in the group still reads as alive (otherwise `eph up` would spawn a
/// duplicate). `EPERM` means the group exists but is not ours to signal (still
/// alive); any other error (chiefly `ESRCH`, no such group) means the wrapper was
/// recorded before eph grouped its shells (legacy state) or is truly gone, so we
/// fall back to probing the bare PID. On Windows the `cmd /C` wrapper stays alive
/// as the parent of its child, so the PID probe is sufficient.
pub fn is_alive(pid: NonZeroU32) -> bool {
    #[cfg(unix)]
    {
        let raw = pid.get() as libc::pid_t;
        // SAFETY: `killpg` with signal 0 performs the permission/existence checks
        // of signaling without delivering a signal; it takes plain integers and
        // has no memory-safety preconditions.
        if unsafe { libc::killpg(raw, 0) } == 0 {
            return true;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            // Group exists but we may not signal it: still present.
            Some(libc::EPERM) => true,
            // No such group: legacy non-grouped wrapper (or truly gone). Probe the
            // PID directly to tell those apart.
            _ => pid_in_table(pid),
        }
    }
    #[cfg(not(unix))]
    {
        pid_in_table(pid)
    }
}

/// Collect `root` and all of its descendant PIDs from a process snapshot.
///
/// Builds a parent-to-children index over every process in `system`, then walks
/// it breadth-first from `root`. A `seen` set guards against the cycle that PID
/// reuse could otherwise fabricate. `root` is always included even if it is no
/// longer present, so callers still attempt to signal it.
fn process_tree(system: &System, root: Pid) -> Vec<Pid> {
    use std::collections::{HashMap, HashSet};

    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }

    let mut tree = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        tree.push(pid);
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    tree
}

/// Signal every process in the `sysinfo` descendant tree rooted at `pid`.
///
/// This is Windows's primary teardown and Unix's fallback for a wrapper recorded
/// before eph grouped its shells (legacy state). Where the platform does not
/// support `signal` (Windows has no `SIGTERM`), `kill_with` returns `None` and we
/// fall back to a hard kill. Best-effort throughout: a process that has already
/// exited is a no-op.
///
/// A child spawned after the snapshot can still escape this walk, the accepted
/// limitation of snapshot-based teardown (the Unix process-group path does not
/// have it).
fn signal_tree(pid: NonZeroU32, signal: Signal) {
    // A full snapshot is needed (not just `pid`): the parent links of every
    // process are what let us find the descendants to signal.
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);

    for target in process_tree(&system, Pid::from_u32(pid.get())) {
        if let Some(process) = system.process(target)
            && process.kill_with(signal).is_none()
        {
            process.kill();
        }
    }
}

/// Ask the process tree rooted at `pid` to terminate gracefully.
///
/// On Unix this sends `SIGTERM` to the wrapper's process group (matching the old
/// `kill <pid>` but reaching every descendant). On Windows, where POSIX signals
/// do not exist, it is a hard terminate of the whole tree (the same forced stop
/// as [`force_kill`]). Best-effort: a tree that has already exited is a no-op,
/// mirroring the ignored error from the old `kill`.
pub fn terminate(pid: NonZeroU32) {
    stop_tree(pid, Signal::Term);
}

/// Forcibly kill the process tree rooted at `pid` (`SIGKILL` to the process group
/// on Unix, a forced terminate of the descendant tree on Windows). Best-effort,
/// mirroring the old `kill -9 <pid>`: a process that is already gone is a no-op.
pub fn force_kill(pid: NonZeroU32) {
    stop_tree(pid, Signal::Kill);
}

/// Tear down the whole tree rooted at `pid`. On Unix this signals the wrapper's
/// process group (`signal` selects graceful `SIGTERM` vs forced `SIGKILL`),
/// falling back to the descendant walk only for legacy non-grouped state. On
/// Windows every stop is the descendant walk.
fn stop_tree(pid: NonZeroU32, signal: Signal) {
    #[cfg(unix)]
    {
        let sig = if matches!(signal, Signal::Kill) {
            libc::SIGKILL
        } else {
            libc::SIGTERM
        };
        let raw = pid.get() as libc::pid_t;
        // SAFETY: `killpg` takes plain integers and has no memory-safety
        // preconditions. It signals the process group led by the wrapper, which
        // reaches every process the shell forked.
        if unsafe { libc::killpg(raw, sig) } == 0 {
            return;
        }
        // No such group: a wrapper recorded before eph grouped its shells (legacy
        // on-disk state) leads no group, so fall back to the descendant walk to
        // still catch its children.
        signal_tree(pid, signal);
    }
    #[cfg(not(unix))]
    {
        signal_tree(pid, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Spawn a long-lived child directly (no shell wrapper), so the returned PID
    /// is the process under test rather than a `sh`/`cmd` parent. The exact
    /// command differs per platform but both block for ~30s without input.
    fn spawn_sleeper() -> tokio::process::Child {
        #[cfg(unix)]
        {
            TokioCommand::new("sleep").arg("30").spawn().unwrap()
        }
        #[cfg(windows)]
        {
            // `timeout` needs a console; `ping` to loopback is the portable way
            // to idle for a fixed span in a redirected/non-interactive child.
            TokioCommand::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn shell_command_runs_and_reports_exit_code() {
        // `exit N` is spelled the same in both `sh` and `cmd`.
        let status = shell_command("exit 0").status().await.unwrap();
        assert!(status.success());

        let status = shell_command("exit 3").status().await.unwrap();
        assert_eq!(status.code(), Some(3));
    }

    /// Assert that `kill` ended `child` promptly, then that its PID is gone.
    ///
    /// The kill is delivered out of band (not through the `Child` handle), so this
    /// reaps the child afterward. That reap matters on Unix: a killed-but-unwaited
    /// child lingers as a zombie, which still occupies a slot in the process table
    /// and so reads as "alive". In real eph usage there is no zombie (the
    /// short-lived CLI exits and `init` reaps the detached child); the test
    /// process, being the long-lived parent, must reap explicitly.
    ///
    /// The timeout is the real assertion that the kill *worked*: the sleeper would
    /// exit on its own in ~30s, so reaping within a few seconds proves the kill
    /// ended it rather than it simply running to completion.
    async fn assert_kill_ends(mut child: tokio::process::Child, kill: impl FnOnce(NonZeroU32)) {
        let pid = NonZeroU32::new(child.id().expect("freshly spawned child has a PID")).unwrap();
        assert!(is_alive(pid), "a just-spawned sleeper should be alive");

        kill(pid);

        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("kill should let the child be reaped well before it exits on its own")
            .expect("waiting on the killed child failed");
        assert!(
            !status.success(),
            "a killed process should not report a successful exit"
        );

        // Once reaped, the PID leaves the table.
        assert!(
            !is_alive(pid),
            "the killed process should no longer be alive"
        );
    }

    #[tokio::test]
    async fn force_kill_ends_the_process() {
        assert_kill_ends(spawn_sleeper(), force_kill).await;
    }

    #[tokio::test]
    async fn terminate_ends_the_process() {
        assert_kill_ends(spawn_sleeper(), terminate).await;
    }

    /// Poll the descendants of `root` until at least one appears, returning them
    /// (excluding `root` itself). Used to wait for a shell wrapper to spawn its
    /// child before we try to tear the tree down.
    async fn wait_for_descendants(root: NonZeroU32) -> Vec<NonZeroU32> {
        for _ in 0..50 {
            let mut system = System::new();
            system.refresh_processes(ProcessesToUpdate::All, true);
            let kids: Vec<NonZeroU32> = process_tree(&system, Pid::from_u32(root.get()))
                .into_iter()
                .filter(|p| p.as_u32() != root.get())
                .filter_map(|p| NonZeroU32::new(p.as_u32()))
                .collect();
            if !kids.is_empty() {
                return kids;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Vec::new()
    }

    async fn poll_until_gone(pid: NonZeroU32) -> bool {
        for _ in 0..100 {
            if !is_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Spawn a wrapper through the platform shell (as `run=` does) that itself
    /// launches a long-lived child, run `tear_down` against its PID, and assert
    /// the whole tree (every descendant, not just the wrapper) is gone. `detached`
    /// selects whether the wrapper is spawned through [`prepare_detached`], i.e.
    /// the primary path (Unix process group) versus the unsupervised path that a
    /// legacy state file or Windows exercises (the descendant walk).
    async fn assert_tree_torn_down(detached: bool) {
        // The inner command keeps a child alive that outlives a single-PID kill.
        // On Windows `cmd /C` runs (and waits on) `ping` as a child; on Unix
        // backgrounding plus `wait` keeps `sh` alive as the child's parent rather
        // than exec-replacing itself.
        let inner = if cfg!(windows) {
            "ping -n 300 127.0.0.1 >NUL"
        } else {
            "sleep 300 & wait"
        };

        let mut command = shell_command(inner);
        command.stdin(std::process::Stdio::null());
        if detached {
            prepare_detached(&mut command);
        }
        let mut wrapper = command.spawn().unwrap();
        let wrapper_pid =
            NonZeroU32::new(wrapper.id().expect("freshly spawned wrapper has a PID")).unwrap();

        let descendants = wait_for_descendants(wrapper_pid).await;
        assert!(
            !descendants.is_empty(),
            "the shell wrapper should have launched at least one child"
        );

        force_kill(wrapper_pid);
        let _ = tokio::time::timeout(Duration::from_secs(10), wrapper.wait()).await;

        // Every process in the tree, not just the wrapper, must be gone.
        for pid in descendants {
            assert!(
                poll_until_gone(pid).await,
                "descendant {pid} should have been killed along with the tree"
            );
        }
    }

    /// The primary teardown path: a `run=` service spawned through the real
    /// detached path. The tracked PID is the wrapper (`cmd`/`sh`), not the child,
    /// so a single-PID kill would orphan the child. On Unix this exercises the
    /// process group; on Windows the descendant walk.
    #[tokio::test]
    async fn force_kill_reaps_a_detached_shell_tree() {
        assert_tree_torn_down(true).await;
    }

    /// The fallback teardown path: a wrapper spawned WITHOUT the detached setup,
    /// standing in for legacy on-disk state recorded before eph grouped `run=`
    /// shells. Teardown must still reap the whole tree via the `sysinfo`
    /// descendant walk.
    #[tokio::test]
    async fn force_kill_reaps_an_unsupervised_shell_tree() {
        assert_tree_torn_down(false).await;
    }
}
