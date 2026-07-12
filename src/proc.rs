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
//!
//! # Handle inheritance (Windows)
//!
//! A detached `run=` service must receive **only** its three stdio handles.
//! Unix gets this for free: Rust opens every descriptor `O_CLOEXEC`, so a
//! `fork`/`exec` child sees nothing but the remapped fds 0-2. Windows does
//! not: `std::process` (and tokio on top of it) calls `CreateProcess` with
//! `bInheritHandles=TRUE` whenever stdio is redirected, which copies *every*
//! inheritable handle in eph into the child, and the shell passes them on to
//! the whole service tree. The worst of those are the stdin/stdout/stderr
//! pipe handles eph's own caller created inheritable and passed in: a
//! long-lived service tree then holds the caller's pipe write-ends open, so
//! anything capturing eph's output (`eph up | tee`, a PowerShell pipeline, a
//! test harness's `.output()`) blocks waiting for EOF long after eph exited.
//!
//! Two layers close this:
//!
//! - [`spawn_captured`] spawns the detached service through raw
//!   `CreateProcessW` with a `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming
//!   exactly the three stdio handles, so nothing else can be inherited no
//!   matter what handles exist in eph's process. (Rust std does not expose
//!   the attribute list on stable, hence the raw call; see [`win`].)
//! - [`disinherit_std_handles`] clears `HANDLE_FLAG_INHERIT` on eph's own
//!   std handles once at startup, so shorter-lived children spawned through
//!   std/tokio (hooks, health checks, the update worker) cannot re-leak them
//!   either, including to grandchildren their shells leave behind.

use std::fs::File;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, Signal, System};
use tokio::process::Command as TokioCommand;

/// Stable process facts recorded when eph starts a `run=` service.
///
/// A PID alone can be reused after the original process exits. Prune uses this
/// snapshot to prove the current process table entry is still the shell eph
/// launched before sending it a signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub(crate) start_time: u64,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) exe: Option<PathBuf>,
    pub(crate) cmd: Vec<String>,
}

impl ProcessIdentity {
    /// Snapshot whatever the OS exposes about `process`, however little that
    /// is. Callers that need the snapshot to actually distinguish a PID reuse
    /// apply [`Self::is_recordable`] on top (see [`identity`]).
    fn from_process_raw(process: &Process) -> Self {
        ProcessIdentity {
            start_time: process.start_time(),
            cwd: process.cwd().map(PathBuf::from),
            exe: process.exe().map(PathBuf::from),
            cmd: process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect(),
        }
    }

    fn is_recordable(&self) -> bool {
        self.start_time != 0 && (self.cwd.is_some() || self.exe.is_some() || !self.cmd.is_empty())
    }

    /// Whether `other` plausibly describes the same process, tolerating the
    /// ways two honest snapshots of one process can disagree.
    ///
    /// Exact equality is the wrong test here: sysinfo's `start_time` can
    /// jitter by a second between two queries of the same process, and a
    /// field the OS declined to expose on one query (a just-spawned process's
    /// `cwd`/`exe`/`cmd` on Windows, for instance) comes back `None`/empty
    /// rather than wrong. A false mismatch is not the safe direction: it
    /// makes teardown treat a live service as already dead and leak it, and
    /// makes prune skip a process it should reap. So `start_time` matches
    /// within one second, and a `cwd`/`exe`/`cmd` missing on either side is
    /// unknown rather than a conflict. Fields present on both sides must
    /// still agree exactly, which keeps the PID-reuse guard: a recycled PID
    /// slips through only with the same exe, cwd, and command line inside the
    /// same one-second start window.
    fn matches(&self, other: &Self) -> bool {
        fn known_fields_agree<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }

        self.start_time.abs_diff(other.start_time) <= 1
            && known_fields_agree(&self.cwd, &other.cwd)
            && known_fields_agree(&self.exe, &other.exe)
            && (self.cmd.is_empty() || other.cmd.is_empty() || self.cmd == other.cmd)
    }
}

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

/// The small surface eph's startup path needs from a live `run=` child.
///
/// Two spawn shapes implement it: the foreground (`eph dev`) child is a plain
/// [`tokio::process::Child`], while the detached background child is a
/// [`CapturedChild`], which on Windows is not a std/tokio child at all (see
/// [`spawn_captured`]). Startup only ever asks "has it exited yet?" while
/// waiting for readiness and "kill it" when a fresh spawn cannot be recorded
/// safely, so that is the whole trait.
pub(crate) trait SpawnedChild {
    /// Whether the process has exited (non-blocking).
    fn has_exited(&mut self) -> bool;

    /// Terminate the process and wait until it is gone. A process that
    /// already exited is a success, matching `tokio::process::Child::kill`.
    async fn kill(&mut self) -> io::Result<()>;
}

impl SpawnedChild for tokio::process::Child {
    fn has_exited(&mut self) -> bool {
        matches!(self.try_wait(), Ok(Some(_)))
    }

    async fn kill(&mut self) -> io::Result<()> {
        tokio::process::Child::kill(self).await
    }
}

/// A detached background `run=` child: a [`tokio::process::Child`] on Unix, a
/// raw handle-owning child on Windows (see [`win::RawChild`]).
///
/// Deliberately **not** killed on drop: a detached service's whole point is to
/// outlive the `eph up` that spawned it, and the startup path drops this
/// handle once the service is recorded in state.
pub(crate) struct CapturedChild {
    #[cfg(not(windows))]
    inner: tokio::process::Child,
    #[cfg(windows)]
    inner: win::RawChild,
}

impl SpawnedChild for CapturedChild {
    fn has_exited(&mut self) -> bool {
        #[cfg(not(windows))]
        {
            matches!(self.inner.try_wait(), Ok(Some(_)))
        }
        #[cfg(windows)]
        {
            self.inner.has_exited()
        }
    }

    async fn kill(&mut self) -> io::Result<()> {
        #[cfg(not(windows))]
        {
            self.inner.kill().await
        }
        #[cfg(windows)]
        {
            // Termination completes fast enough that blocking the runtime
            // thread for it is fine; see `RawChild::kill`.
            self.inner.kill()
        }
    }
}

/// Spawn `cmd` through the platform shell as a detached background service:
/// stdin from the null device, stdout/stderr to the provided log files,
/// working directory `cwd`, and `env` overlaid on eph's own environment.
///
/// On Unix this is the ordinary tokio spawn plus [`prepare_detached`]'s
/// process group. On Windows it is a raw `CreateProcessW` that restricts
/// handle inheritance to exactly the three stdio handles; see the module docs
/// for why the std/tokio spawn cannot be used for a long-lived child there.
pub(crate) fn spawn_captured(
    cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    stdout: File,
    stderr: File,
) -> io::Result<(CapturedChild, NonZeroU32)> {
    #[cfg(not(windows))]
    {
        let mut command = shell_command(cmd);
        command
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::from(stderr));
        prepare_detached(&mut command);
        let child = command.spawn()?;
        // A freshly spawned child always has a PID; `id()` only returns `None`
        // after it has been awaited to completion.
        let pid = child
            .id()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| io::Error::other("spawned process has no PID"))?;
        Ok((CapturedChild { inner: child }, pid))
    }
    #[cfg(windows)]
    {
        let child = win::spawn_captured(cmd, cwd, env, stdout, stderr)?;
        let pid = child.pid();
        Ok((CapturedChild { inner: child }, pid))
    }
}

/// Make eph's own standard handles non-inheritable. Windows only; a no-op
/// elsewhere. Called once at process startup, before anything spawns.
///
/// When eph's output is captured (`eph up | tee`, a test harness's
/// `.output()`), the caller hands eph *inheritable* pipe handles as its std
/// handles. Any child spawned through std/tokio with redirected stdio then
/// receives copies of them (`bInheritHandles=TRUE` copies every inheritable
/// handle), and a long-lived grandchild (a daemon a hook's shell leaves
/// behind, say) keeps the caller's pipe open after eph exits. Clearing
/// `HANDLE_FLAG_INHERIT` on the originals removes that whole class.
///
/// Clearing permanently (never toggling around a spawn, which would race
/// concurrent spawns) is safe because no child needs the *originals* to be
/// inheritable: for `Stdio::inherit`, std duplicates the current handle into
/// a fresh inheritable copy at spawn time, so interactive children (`eph
/// run`, `eph dev`) still receive working consoles. Failures are ignored;
/// this is hardening, and there is nothing useful to do if the OS declines.
pub fn disinherit_std_handles() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };

        for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: `GetStdHandle` is a pure lookup with no preconditions.
            let handle = unsafe { GetStdHandle(std_id) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            // SAFETY: clearing a flag on a handle this process owns; the
            // handle was just returned by the OS as live.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// Raw `CreateProcessW` spawn for detached `run=` services.
///
/// Exists because `std::process` cannot express "inherit only these handles"
/// on stable Rust: it passes `bInheritHandles=TRUE` whenever stdio is
/// redirected, leaking every inheritable handle in the process (module docs
/// have the full story). The one std facility that fixes this,
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` via `STARTUPINFOEX`, is only reachable
/// through `CreateProcessW` directly, so this module re-implements the small
/// slice of `Command` the detached spawn needs: `cmd /C <string>` with an
/// environment overlay, a working directory, and file-backed stdio. Quoting
/// and environment-merge semantics deliberately mirror std's so `run=`
/// strings behave exactly as they did through tokio.
#[cfg(windows)]
mod win {
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::File;
    use std::io;
    use std::num::NonZeroU32;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    /// A process spawned by [`spawn_captured`], tracked by its owned process
    /// handle. Dropping it closes the handle only; the process keeps running.
    pub(crate) struct RawChild {
        pid: NonZeroU32,
        process: OwnedHandle,
    }

    impl RawChild {
        pub(crate) fn pid(&self) -> NonZeroU32 {
            self.pid
        }

        /// Non-blocking exit probe: a process handle is signaled exactly when
        /// the process has exited.
        pub(crate) fn has_exited(&self) -> bool {
            // SAFETY: the owned handle is open, and a zero timeout makes this
            // a pure state query.
            unsafe {
                WaitForSingleObject(self.process.as_raw_handle() as HANDLE, 0) == WAIT_OBJECT_0
            }
        }

        /// Terminate the process and wait for the termination to complete,
        /// mirroring `tokio::process::Child::kill` (which kills, then reaps).
        pub(crate) fn kill(&self) -> io::Result<()> {
            let handle = self.process.as_raw_handle() as HANDLE;
            // SAFETY: the owned handle is open; exit code 1 marks a forced
            // stop, the same code tokio's kill produces on Windows.
            if unsafe { TerminateProcess(handle, 1) } == 0 {
                // TerminateProcess fails (ERROR_ACCESS_DENIED) on a process
                // that already exited; that is this call's success state.
                if self.has_exited() {
                    return Ok(());
                }
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the owned handle is open. Termination is asynchronous;
            // waiting (it completes in milliseconds) means the caller
            // observes the process actually gone, like tokio's kill().await.
            unsafe { WaitForSingleObject(handle, INFINITE) };
            Ok(())
        }
    }

    /// Owned attribute-list storage that is always deleted, even on an early
    /// error return between initialization and `CreateProcessW`.
    struct AttributeList {
        // `usize` elements keep the opaque list pointer-aligned; it stores
        // pointers internally.
        buffer: Vec<usize>,
    }

    impl AttributeList {
        /// Allocate and initialize a list with room for `count` attributes.
        fn new(count: u32) -> io::Result<Self> {
            let mut size = 0usize;
            // SAFETY: the sizing call takes a null list and reports the
            // required buffer size; it "fails" by design.
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
            if size == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
            // SAFETY: the buffer is at least `size` bytes and stays alive as
            // long as the list is used (it is owned by the returned value).
            if unsafe {
                InitializeProcThreadAttributeList(
                    buffer.as_mut_ptr().cast::<c_void>(),
                    count,
                    0,
                    &mut size,
                )
            } == 0
            {
                // Plain memory at this point: the struct (and with it the
                // Drop that calls DeleteProcThreadAttributeList, valid only
                // on an initialized list) is built exclusively on success.
                return Err(io::Error::last_os_error());
            }
            Ok(AttributeList { buffer })
        }

        fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buffer.as_mut_ptr().cast::<c_void>()
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            // SAFETY: `new` only returns initialized lists, and deletion is
            // the documented teardown for them.
            unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
        }
    }

    /// Spawn `cmd /C <cmd>` detached, with `env` overlaid on eph's
    /// environment, `cwd` as the working directory, stdin from `NUL`, and
    /// stdout/stderr bound to the given log files. The child inherits
    /// **only** those three handles.
    pub(super) fn spawn_captured(
        cmd: &str,
        cwd: &Path,
        env: &[(String, String)],
        stdout: File,
        stderr: File,
    ) -> io::Result<RawChild> {
        // The null device mirrors `Stdio::null()`: a service that reads stdin
        // sees EOF instead of blocking on a console it may not have.
        let stdin = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")?;

        let comspec = comspec();
        let application = wide_nul(comspec.as_os_str())?;
        let mut command_line = build_command_line(&comspec, cmd)?;
        let cwd_wide = wide_nul(cwd.as_os_str())?;
        let environment = environment_block(env)?;

        // The attribute list restricts inheritance to the listed handles, but
        // each listed handle must itself carry HANDLE_FLAG_INHERIT. These are
        // handles this spawn just created, so flagging them exposes nothing
        // else (and eph never spawns concurrently anyway).
        let handles: [HANDLE; 3] = [
            stdin.as_raw_handle() as HANDLE,
            stdout.as_raw_handle() as HANDLE,
            stderr.as_raw_handle() as HANDLE,
        ];
        for &handle in &handles {
            // SAFETY: each handle is owned by a live `File` in this scope.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }

        let mut attributes = AttributeList::new(1)?;
        // SAFETY: `handles` outlives the CreateProcessW call below (the
        // attribute list stores the pointer, not a copy), and the size is the
        // real byte size of the array.
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast::<c_void>(),
                size_of_val(&handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: plain-old-data structs the API fills in / reads.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[2];
        startup.lpAttributeList = attributes.as_ptr();

        // SAFETY: plain-old-data struct the API fills in.
        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // SAFETY: every pointer is to a live, NUL-terminated wide buffer or
        // initialized struct owned by this scope; `bInheritHandles=TRUE` is
        // required for the handle list to take effect, and the list caps what
        // is actually inherited.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_ptr().cast::<c_void>(),
                cwd_wide.as_ptr(),
                &startup.StartupInfo,
                &mut process_info,
            )
        };
        if created == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: on success both handles are live and owned by us; the
        // thread handle is not needed, the process handle is adopted.
        unsafe { CloseHandle(process_info.hThread) };
        // SAFETY: `hProcess` is an owned, open handle we must close exactly
        // once; `OwnedHandle` takes over that duty.
        let process = unsafe { OwnedHandle::from_raw_handle(process_info.hProcess.cast()) };
        let pid = NonZeroU32::new(process_info.dwProcessId)
            .ok_or_else(|| io::Error::other("CreateProcessW reported PID 0"))?;

        // The stdin/stdout/stderr `File`s drop here, closing eph's copies of
        // the handles; the child owns its inherited duplicates.
        Ok(RawChild { pid, process })
    }

    /// Absolute path of `cmd.exe`: `%ComSpec%` (the canonical pointer to the
    /// command interpreter), falling back to `%SystemRoot%\System32\cmd.exe`.
    ///
    /// Passing an absolute path as `lpApplicationName` skips `CreateProcessW`'s
    /// implicit search (application directory, then the *working directory*,
    /// then PATH), so a `cmd.exe` dropped into a workspace can never hijack a
    /// `run=` spawn.
    fn comspec() -> PathBuf {
        if let Some(comspec) = std::env::var_os("ComSpec") {
            return PathBuf::from(comspec);
        }
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        Path::new(&root).join(r"System32\cmd.exe")
    }

    /// Encode `value` as a NUL-terminated UTF-16 buffer, rejecting interior
    /// NULs (they would silently truncate the string at the API boundary).
    fn wide_nul(value: &OsStr) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = value.encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nul character in process arguments",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    /// Build the `CreateProcessW` command line `<cmd.exe> /C <raw>`, quoting
    /// each token with the same algorithm std uses for `Command::arg`, so a
    /// `run=` string reaches `cmd /C` byte-for-byte as it did when this spawn
    /// went through tokio.
    fn build_command_line(comspec: &Path, raw: &str) -> io::Result<Vec<u16>> {
        let mut line: Vec<u16> = Vec::new();
        append_arg(&mut line, comspec.as_os_str())?;
        line.push(u16::from(b' '));
        append_arg(&mut line, OsStr::new("/C"))?;
        line.push(u16::from(b' '));
        append_arg(&mut line, OsStr::new(raw))?;
        line.push(0);
        Ok(line)
    }

    /// std's argument-quoting algorithm: quote when the argument is empty or
    /// contains whitespace, escape `"` with a backslash, and double any run
    /// of backslashes that ends up before a quote.
    fn append_arg(line: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
        const QUOTE: u16 = b'"' as u16;
        const BACKSLASH: u16 = b'\\' as u16;

        if arg.encode_wide().any(|c| c == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nul character in process arguments",
            ));
        }
        let quote = arg.is_empty()
            || arg
                .encode_wide()
                .any(|c| c == u16::from(b' ') || c == u16::from(b'\t'));
        if quote {
            line.push(QUOTE);
        }
        let mut backslashes: usize = 0;
        for c in arg.encode_wide() {
            if c == BACKSLASH {
                backslashes += 1;
            } else {
                if c == QUOTE {
                    // One escaping backslash per preceding backslash, plus
                    // one for the quote itself (2n+1 total).
                    line.extend((0..=backslashes).map(|_| BACKSLASH));
                }
                backslashes = 0;
            }
            line.push(c);
        }
        if quote {
            // Double a trailing backslash run so it cannot escape the
            // closing quote (2n total).
            line.extend((0..backslashes).map(|_| BACKSLASH));
            line.push(QUOTE);
        }
        Ok(())
    }

    /// Build the UTF-16 environment block for `CREATE_UNICODE_ENVIRONMENT`:
    /// eph's own environment with `overlay` applied, entries `KEY=value\0`,
    /// block closed by an extra NUL.
    ///
    /// Matches `Command::envs` on top of an inherited environment: variable
    /// names compare case-insensitively (an overlay hit keeps the existing
    /// name's casing and replaces only the value, like a map insert), and the
    /// block is sorted by uppercased name as the Win32 docs require of a
    /// manually built block.
    fn environment_block(overlay: &[(String, String)]) -> io::Result<Vec<u16>> {
        fn normalized(key: &OsStr) -> String {
            key.to_string_lossy().to_uppercase()
        }

        let mut merged: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        for (key, value) in overlay {
            let target = normalized(OsStr::new(key));
            match merged.iter_mut().find(|(k, _)| normalized(k) == target) {
                Some(slot) => slot.1 = OsString::from(value),
                None => merged.push((OsString::from(key), OsString::from(value))),
            }
        }
        merged.sort_by_cached_key(|(key, _)| normalized(key));

        let mut block: Vec<u16> = Vec::new();
        for (key, value) in &merged {
            if key.encode_wide().chain(value.encode_wide()).any(|c| c == 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "nul character in environment",
                ));
            }
            block.extend(key.encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        // An empty block still needs its two terminating NULs.
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn rendered_line(raw: &str) -> String {
            let wide = build_command_line(Path::new(r"C:\Windows\System32\cmd.exe"), raw).unwrap();
            let end = wide.len() - 1;
            assert_eq!(wide[end], 0, "command line must be NUL-terminated");
            String::from_utf16(&wide[..end]).unwrap()
        }

        /// The quoting must reproduce what std's `Command::arg` builds, since
        /// `.eph` files were written against that behavior.
        #[test]
        fn command_line_quotes_like_std() {
            assert_eq!(
                rendered_line("ping"),
                r"C:\Windows\System32\cmd.exe /C ping"
            );
            assert_eq!(
                rendered_line("echo hi"),
                r#"C:\Windows\System32\cmd.exe /C "echo hi""#
            );
            // A quote is backslash-escaped, and backslashes before it double.
            assert_eq!(
                rendered_line(r#"echo "a b""#),
                r#"C:\Windows\System32\cmd.exe /C "echo \"a b\"""#
            );
            assert_eq!(
                rendered_line(r#"type c:\dir\"file""#),
                r#"C:\Windows\System32\cmd.exe /C "type c:\dir\\\"file\"""#
            );
            // A trailing backslash in a quoted argument doubles so it cannot
            // escape the closing quote.
            assert_eq!(
                rendered_line(r"dir C:\some path\"),
                r#"C:\Windows\System32\cmd.exe /C "dir C:\some path\\""#
            );
        }

        #[test]
        fn command_line_rejects_interior_nul() {
            assert!(build_command_line(Path::new("cmd.exe"), "echo \0 hi").is_err());
        }

        /// The overlay must *replace* an inherited variable whose name
        /// differs only by case; appending a second spelling would leave
        /// which one the child sees up to lookup order.
        #[test]
        fn environment_block_overrides_case_insensitively() {
            // Every Windows process has PATH (usually spelled "Path").
            let block = environment_block(&[
                ("path".to_string(), r"C:\replaced".to_string()),
                ("EPH_ENV_BLOCK_TEST".to_string(), "x".to_string()),
            ])
            .unwrap();
            let text = String::from_utf16(&block).unwrap();
            let entries: Vec<&str> = text.split('\0').filter(|e| !e.is_empty()).collect();

            let paths: Vec<&&str> = entries
                .iter()
                .filter(|e| e.to_uppercase().starts_with("PATH="))
                .collect();
            assert_eq!(paths.len(), 1, "case-differing overlay must not duplicate");
            assert!(paths[0].ends_with(r"=C:\replaced"));
            assert!(entries.contains(&"EPH_ENV_BLOCK_TEST=x"));

            // Compare by the variable *name*: sorting whole `KEY=value`
            // entries would rank `(` against `=` and disagree with the
            // key-only sort the block actually uses.
            fn key_of(entry: &str) -> String {
                entry
                    .split_once('=')
                    .map_or(entry, |(key, _)| key)
                    .to_uppercase()
            }
            let mut sorted = entries.clone();
            sorted.sort_by_key(|entry| key_of(entry));
            assert_eq!(entries, sorted, "block must be sorted by uppercased name");
        }
    }
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

/// Refresh a fresh [`System`] with everything it can learn about `pid` and
/// return its process entry's raw identity snapshot, `None` only when the
/// process is not in the table at all.
fn raw_identity(pid: NonZeroU32) -> Option<ProcessIdentity> {
    let pid = Pid::from_u32(pid.get());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::everything(),
    );
    system.process(pid).map(ProcessIdentity::from_process_raw)
}

/// Return the current identity for `pid`, when the platform exposes enough
/// process metadata to distinguish the entry from a later PID reuse.
///
/// This is the recording side, used when eph launches a `run=` service. A
/// just-spawned process can briefly expose metadata that conflicts with every
/// later snapshot. Persisting that transient view makes eph treat its own live
/// service as a reused PID forever. Recording therefore requires two consecutive
/// compatible snapshots with a real start time and at least one distinguishing
/// field. `None` tells the caller to store no identity rather than false proof.
pub(crate) fn identity(pid: NonZeroU32) -> Option<ProcessIdentity> {
    let mut previous: Option<ProcessIdentity> = None;

    for attempt in 0..5 {
        let current = raw_identity(pid).filter(ProcessIdentity::is_recordable);
        if let (Some(previous), Some(current)) = (&previous, &current)
            && current.matches(previous)
        {
            return Some(current.clone());
        }
        previous = current;
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    None
}

/// Whether `pid` still names the process represented by `expected` (see
/// [`ProcessIdentity::matches`] for what "the same process" tolerates).
///
/// The comparison side deliberately skips [`identity`]'s
/// distinguishing-fields filter: a loaded system can transiently expose
/// nothing about a live process (an opaque snapshot with only a start time),
/// and treating that as "not the same process" is the harmful direction, as
/// it makes teardown leak a live service and prune skip one it should reap.
/// An opaque snapshot still has to agree on the start time to match; only a
/// PID absent from the process table is a definite mismatch.
pub(crate) fn identity_matches(pid: NonZeroU32, expected: &ProcessIdentity) -> bool {
    raw_identity(pid).is_some_and(|current| current.matches(expected))
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
/// Descendants are stopped before their parents. Keeping the ancestry intact
/// until each child has been addressed prevents Windows from reparenting a live
/// child out of the captured tree during teardown.
fn signal_tree(pid: NonZeroU32, signal: Signal) {
    // A full snapshot is needed (not just `pid`): the parent links of every
    // process are what let us find the descendants to signal. Only the bare
    // enumeration is collected (`ProcessRefreshKind::nothing()`): parent PIDs
    // come with it, and collecting more (cwd, cmd, exe) opens and queries
    // every process on the machine, which turns teardown from milliseconds
    // into tens of seconds on a busy system.
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    for target in process_tree(&system, Pid::from_u32(pid.get()))
        .into_iter()
        .rev()
    {
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

    /// Poll `child` until it reports exited, failing the test after ~10s.
    async fn wait_until_exited(child: &mut CapturedChild) {
        for _ in 0..100 {
            if child.has_exited() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("captured child did not exit within 10s");
    }

    /// The detached spawn must deliver the same contract on both platforms:
    /// the shell runs the command string with the overlay env visible, from
    /// the requested working directory, with stdout captured to the log file.
    #[tokio::test]
    async fn spawn_captured_applies_env_cwd_and_log_capture() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("svc.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let log_err = log.try_clone().unwrap();

        #[cfg(unix)]
        let cmd = r#"echo "MARKER=$EPH_SPAWN_TEST" && pwd"#;
        #[cfg(windows)]
        let cmd = "echo MARKER=%EPH_SPAWN_TEST%& cd";

        let (mut child, pid) = spawn_captured(
            cmd,
            dir.path(),
            &[("EPH_SPAWN_TEST".to_string(), "grace-hopper".to_string())],
            log,
            log_err,
        )
        .unwrap();
        assert!(pid.get() > 0);
        wait_until_exited(&mut child).await;

        let output = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            output.contains("MARKER=grace-hopper"),
            "overlay env did not reach the child; log:\n{output}"
        );
        // `pwd`/`cd` prints the working directory; the temp dir's unique leaf
        // name avoids canonicalization mismatches (/private on macOS, 8.3
        // short names on Windows) that full-path equality would trip over.
        let leaf = dir.path().file_name().unwrap().to_string_lossy();
        assert!(
            output.contains(leaf.as_ref()),
            "child did not run in the requested cwd; log:\n{output}"
        );
    }

    #[tokio::test]
    async fn spawn_captured_child_reports_liveness_and_kill() {
        let dir = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(dir.path().join("svc.log")).unwrap();
        let log_err = log.try_clone().unwrap();

        #[cfg(unix)]
        let cmd = "sleep 30";
        #[cfg(windows)]
        let cmd = "ping -n 30 127.0.0.1 >NUL";

        let (mut child, pid) = spawn_captured(cmd, dir.path(), &[], log, log_err).unwrap();
        assert!(
            !child.has_exited(),
            "a just-spawned sleeper should be running"
        );
        assert!(is_alive(pid));

        child.kill().await.unwrap();
        assert!(
            child.has_exited(),
            "kill waits for termination, so the child must read as exited"
        );
    }

    /// Regression test for the Windows handle-inheritance leak: a long-lived
    /// `run=` service used to inherit *every* inheritable handle in eph,
    /// worst of all the stdout/stderr pipe handles a capturing caller
    /// (`eph up | tee`, a test harness's `.output()`) handed eph, so the
    /// caller's pipe read never saw EOF until the whole service tree died.
    ///
    /// An inheritable pipe created here stands in for the caller's capture
    /// pipe. After spawning a long-lived captured child and closing our write
    /// end, the read end must see EOF immediately; if the child inherited a
    /// copy of the write end, the read blocks for the child's lifetime.
    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_captured_inherits_only_its_stdio_handles() {
        use std::io::Read;
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let mut read_end: HANDLE = std::ptr::null_mut();
        let mut write_end: HANDLE = std::ptr::null_mut();
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: out-pointers to locals; `attrs` requests inheritable ends,
        // matching how a capturing parent creates the pipe it hands a child.
        assert_ne!(
            unsafe { CreatePipe(&mut read_end, &mut write_end, &attrs, 0) },
            0,
            "CreatePipe failed"
        );
        // SAFETY: CreatePipe returned two owned, open handles; each wrapper
        // takes over closing exactly one of them.
        let mut read_end = unsafe { std::fs::File::from_raw_handle(read_end.cast()) };
        let write_end = unsafe { OwnedHandle::from_raw_handle(write_end.cast()) };

        let dir = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(dir.path().join("svc.log")).unwrap();
        let log_err = log.try_clone().unwrap();
        let (mut child, _pid) =
            spawn_captured("ping -n 30 127.0.0.1 >NUL", dir.path(), &[], log, log_err).unwrap();

        // With no writer left, the read must resolve immediately: Ok(0) or
        // BrokenPipe both mean EOF on an anonymous pipe. A leaked write end
        // inside the child's tree keeps it unresolved for ~30s instead.
        drop(write_end);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 16];
            read_end.read(&mut buf)
        });
        let started = std::time::Instant::now();
        while !reader.is_finished() && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let leaked = !reader.is_finished();
        child.kill().await.unwrap();
        assert!(
            !leaked,
            "pipe read did not see EOF: an unrelated inheritable handle leaked \
             into the spawned service tree"
        );
        match reader.join().unwrap() {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => panic!("expected EOF on the probe pipe, got {other:?}"),
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

    fn sample_identity() -> ProcessIdentity {
        ProcessIdentity {
            start_time: 1000,
            cwd: Some(PathBuf::from("/work")),
            exe: Some(PathBuf::from("/bin/sleep")),
            cmd: vec!["sleep".to_string(), "30".to_string()],
        }
    }

    #[test]
    fn recordable_identity_requires_a_start_time_and_process_metadata() {
        let mut identity = sample_identity();
        assert!(identity.is_recordable());

        identity.start_time = 0;
        assert!(!identity.is_recordable());

        identity.start_time = 1000;
        identity.cwd = None;
        identity.exe = None;
        identity.cmd.clear();
        assert!(!identity.is_recordable());
    }

    #[test]
    fn identity_match_tolerates_start_time_jitter_of_one_second() {
        let recorded = sample_identity();
        let mut current = recorded.clone();
        current.start_time = recorded.start_time + 1;
        assert!(current.matches(&recorded));
        assert!(recorded.matches(&current));

        current.start_time = recorded.start_time + 2;
        assert!(
            !current.matches(&recorded),
            "two seconds apart is a different process, not jitter"
        );
    }

    #[test]
    fn identity_match_treats_missing_fields_as_unknown() {
        let recorded = sample_identity();
        let degraded = ProcessIdentity {
            start_time: recorded.start_time,
            cwd: None,
            exe: None,
            cmd: Vec::new(),
        };
        assert!(degraded.matches(&recorded));
        assert!(recorded.matches(&degraded));
    }

    #[test]
    fn identity_match_rejects_conflicting_known_fields() {
        let recorded = sample_identity();

        let mut other_cmd = recorded.clone();
        other_cmd.cmd = vec!["nginx".to_string()];
        assert!(!other_cmd.matches(&recorded));

        let mut other_exe = recorded.clone();
        other_exe.exe = Some(PathBuf::from("/bin/nginx"));
        assert!(!other_exe.matches(&recorded));

        let mut other_cwd = recorded.clone();
        other_cwd.cwd = Some(PathBuf::from("/elsewhere"));
        assert!(!other_cwd.matches(&recorded));
    }

    #[tokio::test]
    async fn identity_matches_the_spawned_process() {
        let mut child = spawn_sleeper();
        let pid = NonZeroU32::new(child.id().expect("freshly spawned child has a PID")).unwrap();

        let recorded = identity(pid).expect("test process should expose identity");
        // A loaded machine (a busy CI runner) can transiently fail to
        // enumerate a live process's entry; a short retry keeps this test
        // about identity semantics rather than scheduler weather. A genuine
        // mismatch still fails: retrying cannot make a wrong identity right.
        let mut matched = identity_matches(pid, &recorded);
        for _ in 0..20 {
            if matched {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            matched = identity_matches(pid, &recorded);
        }
        assert!(matched, "recorded identity should match the live process");

        force_kill(pid);
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    }

    /// Poll the descendants of `root` until at least one appears, returning them
    /// (excluding `root` itself). Used to wait for a shell wrapper to spawn its
    /// child before we try to tear the tree down.
    async fn wait_for_descendants(root: NonZeroU32) -> Vec<NonZeroU32> {
        for _ in 0..50 {
            let mut system = System::new();
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
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
