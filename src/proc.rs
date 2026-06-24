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
//! - Liveness and termination: handled through [`sysinfo`] rather than eph
//!   shelling out to a POSIX `kill`. On Unix the signals map to the historical
//!   behavior (`SIGTERM` then `SIGKILL`); on Windows, where POSIX signals do not
//!   exist, both graceful and forced stops become a hard terminate (sysinfo uses
//!   the built-in `taskkill /F` for that, so no extra setup or WSL is needed).
//!
//! Teardown kills the whole process tree rooted at the tracked PID, not just
//! that one process. This matters most on Windows, where the tracked PID is the
//! `cmd /C` wrapper and the real service runs as its child: killing only the
//! wrapper would orphan the service. On Unix it also covers a `sh -c` that
//! stayed alive as the parent of a backgrounded or compound command. The tree is
//! discovered by walking parent links in a process snapshot (`sysinfo`), so it
//! works across separate `eph` invocations (an `eph down` reads the PID from
//! state and reconstructs the tree); a process spawned after the snapshot can
//! still escape, which is an accepted limitation of snapshot-based teardown.

use std::collections::{HashMap, HashSet};
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

/// Refresh a fresh [`System`] so it knows only about `pid`, returning it
/// alongside the `sysinfo` [`Pid`]. Nothing beyond bare existence is collected
/// (`ProcessRefreshKind::nothing()`), since callers only ask "is it there?" and
/// "kill it"; `remove_dead_processes` is `true` so a process that has exited is
/// dropped and therefore reads as not-present.
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

/// Whether a process with `pid` is currently alive.
///
/// Replaces the historical `kill -0 <pid>` probe with a native lookup, so it
/// needs no external binary and behaves the same on Unix and Windows.
pub fn is_alive(pid: NonZeroU32) -> bool {
    let (system, pid) = snapshot(pid);
    system.process(pid).is_some()
}

/// Collect `root` and all of its descendant PIDs from a process snapshot.
///
/// Builds a parent-to-children index over every process in `system`, then walks
/// it breadth-first from `root`. A `seen` set guards against the cycle that PID
/// reuse could otherwise fabricate. `root` is always included even if it is no
/// longer present, so callers still attempt to signal it.
fn process_tree(system: &System, root: Pid) -> Vec<Pid> {
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

/// Send `signal` to every process in the tree rooted at `pid` (the tracked
/// process plus all of its descendants).
///
/// Each process is signaled with `signal`; where the platform does not support
/// it (Windows has no `SIGTERM`), `kill_with` returns `None` and we fall back to
/// a hard kill. Best-effort throughout: a process that has already exited is a
/// no-op, mirroring the ignored error from the old `kill`.
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
/// On Unix this sends `SIGTERM` (matching the old `kill <pid>`) to every process
/// in the tree; on Windows, where POSIX signals do not exist, it is a hard
/// terminate (the same forced stop as [`force_kill`]). Killing the whole tree
/// rather than just `pid` is what keeps a `cmd /C` (Windows) or backgrounded
/// `sh -c` (Unix) wrapper from orphaning the real service.
pub fn terminate(pid: NonZeroU32) {
    signal_tree(pid, Signal::Term);
}

/// Forcibly kill the process tree rooted at `pid` (`SIGKILL` on Unix, a forced
/// terminate on Windows). Best-effort, mirroring the old `kill -9 <pid>`: a
/// process that is already gone is a no-op.
pub fn force_kill(pid: NonZeroU32) {
    signal_tree(pid, Signal::Kill);
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
    /// The kill is delivered out of band (through `sysinfo`, not the `Child`
    /// handle), so this reaps the child afterward. That reap matters on Unix: a
    /// killed-but-unwaited child lingers as a zombie, which still occupies a slot
    /// in the process table and so reads as "alive". In real eph usage there is
    /// no zombie (the short-lived CLI exits and `init` reaps the detached child);
    /// the test process, being the long-lived parent, must reap explicitly.
    ///
    /// The timeout is the real assertion that the kill *worked*: the sleeper
    /// would exit on its own in ~30s, so reaping within a few seconds proves the
    /// kill ended it rather than it simply running to completion.
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

    /// Regression for the orphaned-child bug: a command run through the platform
    /// shell (as `run=` does) that itself launches a long-lived child. The
    /// tracked PID is the shell wrapper (`cmd` on Windows, `sh` on Unix), not the
    /// child, so a single-PID kill would leave the child running. Tearing down
    /// through the real path (`force_kill`) must take the whole tree.
    #[tokio::test]
    async fn force_kill_reaps_the_whole_shell_tree() {
        // The inner command spawns a child that outlives a single-PID kill. On
        // Windows `cmd /C` runs (and waits on) `ping` as a child process; on Unix
        // backgrounding plus `wait` keeps `sh` alive as the child's parent
        // instead of exec-replacing itself.
        let inner = if cfg!(windows) {
            "ping -n 300 127.0.0.1 >NUL"
        } else {
            "sleep 300 & wait"
        };

        let mut wrapper = shell_command(inner).spawn().unwrap();
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
}
