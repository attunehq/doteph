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

/// Ask the process with `pid` to terminate gracefully.
///
/// On Unix this sends `SIGTERM` (matching the old `kill <pid>`); on Windows,
/// where POSIX signals do not exist, [`Signal::Term`] is unsupported and
/// `kill_with` returns `None`, so it falls back to a hard terminate (the same
/// forced stop as [`force_kill`]). Best-effort: a process that has already
/// exited is a no-op, mirroring the ignored error from the old `kill`.
pub fn terminate(pid: NonZeroU32) {
    let (system, pid) = snapshot(pid);
    if let Some(process) = system.process(pid)
        && process.kill_with(Signal::Term).is_none()
    {
        // The platform has no SIGTERM (Windows): fall back to a hard kill.
        process.kill();
    }
}

/// Forcibly kill the process with `pid` (`SIGKILL` on Unix, a forced terminate
/// on Windows). Best-effort, mirroring the old `kill -9 <pid>`: a process that
/// is already gone is a no-op.
pub fn force_kill(pid: NonZeroU32) {
    let (system, pid) = snapshot(pid);
    if let Some(process) = system.process(pid) {
        process.kill();
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
}
