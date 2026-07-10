//! Service management - starting, stopping, and managing Docker containers

use crate::parser::{
    EphFile, PortMapping, Service, ServiceSource, resolve_interpolations,
    resolve_interpolations_tracked,
};
use crate::proc;
use crate::workspace::Workspace;
use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ContainerSummaryStateEnum, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveVolumeOptionsBuilder, StopContainerOptionsBuilder,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

/// One `${service.property}` reference that runtime state could not resolve.
///
/// Keeping the reference structured lets every execution boundary report the
/// exact missing service property without searching a partially expanded
/// string again.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnresolvedReference {
    /// Service named by the interpolation.
    pub service: String,
    /// Property named by the interpolation.
    pub property: String,
}

/// One environment variable that cannot be passed to a child safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedEnvVar {
    /// Environment variable name.
    pub name: String,
    /// Missing references in first-occurrence order, without duplicates.
    pub references: Vec<UnresolvedReference>,
}

/// Failure to resolve a complete environment.
///
/// `resolved` is retained so `eph env` can still print safe assignments and
/// explicit unsets before returning a failure status. Execution paths must use
/// the `Ok` value and therefore cannot accidentally launch with partial data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedEnvironment {
    /// Variables that were completely resolved, in declaration order.
    pub resolved: Vec<(String, String)>,
    /// Variables containing one or more unavailable references.
    pub unresolved: Vec<UnresolvedEnvVar>,
}

impl std::fmt::Display for UnresolvedEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not resolve environment")?;
        for (index, variable) in self.unresolved.iter().enumerate() {
            if index == 0 {
                write!(f, ": ")?;
            } else {
                write!(f, "; ")?;
            }
            write!(f, "{} requires ", variable.name)?;
            for (reference_index, reference) in variable.references.iter().enumerate() {
                if reference_index > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "${{{}.{}}}", reference.service, reference.property)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for UnresolvedEnvironment {}

// ============================================================================
// Running Service Info
// ============================================================================

/// Runtime information about a running service.
///
/// Returned by [`ServiceManager::start_services`] and friends, and queried for
/// connection details via [`host`](Self::host), [`port`](Self::port), and
/// [`named_port`](Self::named_port) when expanding interpolations.
#[derive(Debug, Clone)]
pub struct RunningService {
    /// Service name (matches the `.eph` section header).
    #[allow(dead_code)]
    pub name: String,
    /// Map of port name (or `"default"`) to the assigned host port.
    pub ports: HashMap<String, u16>,
}

impl RunningService {
    /// Get the host for this service (always localhost for now)
    #[must_use]
    pub fn host(&self) -> &str {
        "localhost"
    }

    /// Get the primary port (first port or named "default")
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.ports
            .get("default")
            .copied()
            .or_else(|| self.ports.values().next().copied())
    }

    /// Get a named port
    #[must_use]
    pub fn named_port(&self, name: &str) -> Option<u16> {
        self.ports.get(name).copied()
    }
}

/// Options controlling how `eph logs` renders a service's output.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Keep streaming new output as it is produced (like `tail -f`).
    pub follow: bool,
    /// Show only the last `N` lines before streaming/returning.
    pub tail: Option<usize>,
}

/// Resolve the top-level `KEY=VALUE` environment variables declared in an
/// [`EphFile`] against the currently running services.
///
/// This expands `${service.host}`, `${service.port}`, and `${service.port.NAME}`
/// interpolations using the assigned host ports in `running`. A reference to a
/// service that is not in `running` is left as the literal `${...}` placeholder,
/// matching [`resolve_interpolations`]. Execution boundaries should use
/// [`resolve_env_vars_strict`] so a partial environment cannot reach a child.
///
/// This permissive form remains available to lifecycle planning code that may
/// resolve again after another service starts.
#[must_use]
pub fn resolve_env_vars(
    eph: &EphFile,
    running: &HashMap<String, RunningService>,
) -> Vec<(String, String)> {
    eph.env_vars
        .iter()
        .map(|var| (var.name.clone(), resolve_against(&var.value, running)))
        .collect()
}

/// Expand `${service.host}`, `${service.port}`, and `${service.port.NAME}`
/// interpolations in `value` against the assigned host ports in `running`.
///
/// A reference to a service that is not in `running` (or a property it does not
/// expose) is left as the literal `${...}` placeholder, matching
/// [`resolve_interpolations`]. Use [`resolve_against_strict`] before passing the
/// result to a process, container, or Compose invocation.
#[must_use]
pub fn resolve_against(value: &str, running: &HashMap<String, RunningService>) -> String {
    resolve_interpolations(value, |service, property| {
        resolve_property(service, property, running)
    })
}

/// The single lookup shared by [`resolve_against`] and
/// [`resolve_against_tracked`]: `${service.host}`, `${service.port}`, and
/// `${service.port.NAME}` resolve against `running`; anything else (an unknown
/// service or property) is `None`.
fn resolve_property(
    service: &str,
    property: &str,
    running: &HashMap<String, RunningService>,
) -> Option<String> {
    let svc = running.get(service)?;
    match property {
        "host" => Some(svc.host().to_string()),
        "port" => svc.port().map(|p| p.to_string()),
        prop if prop.starts_with("port.") => svc.named_port(&prop[5..]).map(|p| p.to_string()),
        _ => None,
    }
}

/// Like [`resolve_against`], but also returns every unresolved
/// `${service.property}` reference in `value`.
fn resolve_against_tracked(
    value: &str,
    running: &HashMap<String, RunningService>,
) -> (String, Vec<(String, String)>) {
    resolve_interpolations_tracked(value, |service, property| {
        resolve_property(service, property, running)
    })
}

/// Resolve a value completely or return its unavailable references.
///
/// References are reported in first-occurrence order and deduplicated. The
/// returned `String` therefore proves that it contains no unresolved eph
/// interpolation and is safe to pass across an execution boundary.
pub fn resolve_against_strict(
    value: &str,
    running: &HashMap<String, RunningService>,
) -> std::result::Result<String, Vec<UnresolvedReference>> {
    let (resolved, references) = resolve_against_tracked(value, running);
    let mut unique = Vec::new();
    for (service, property) in references {
        let reference = UnresolvedReference { service, property };
        if !unique.contains(&reference) {
            unique.push(reference);
        }
    }
    if unique.is_empty() {
        Ok(resolved)
    } else {
        Err(unique)
    }
}

/// Resolve every top-level environment variable before execution.
///
/// The `Ok` variant is the complete environment in declaration order. On
/// failure, [`UnresolvedEnvironment`] retains the safe subset for shell output
/// while making the structured misses available for diagnostics and unsets.
pub fn resolve_env_vars_strict(
    eph: &EphFile,
    running: &HashMap<String, RunningService>,
) -> std::result::Result<Vec<(String, String)>, UnresolvedEnvironment> {
    let mut resolved = Vec::with_capacity(eph.env_vars.len());
    let mut unresolved = Vec::new();
    for var in &eph.env_vars {
        match resolve_against_strict(&var.value, running) {
            Ok(value) => resolved.push((var.name.clone(), value)),
            Err(references) => unresolved.push(UnresolvedEnvVar {
                name: var.name.clone(),
                references,
            }),
        }
    }
    if unresolved.is_empty() {
        Ok(resolved)
    } else {
        Err(UnresolvedEnvironment {
            resolved,
            unresolved,
        })
    }
}

/// Build the `EPH_*` metadata variables describing the workspace and the
/// running services.
///
/// These let a hook or `eph run` command address eph's own resources without
/// re-deriving them: `EPH_WORKSPACE_ID`, `EPH_WORKSPACE_ROOT`,
/// `EPH_CONTAINER_PREFIX`, and per service `EPH_<SERVICE>_HOST`,
/// `EPH_<SERVICE>_PORT`, `EPH_<SERVICE>_PORT_<NAME>`, and
/// `EPH_<SERVICE>_CONTAINER`. Service names are upper-cased with `-` replaced by
/// `_` so they are valid shell identifiers (e.g. `auth-db` -> `EPH_AUTH_DB_PORT`).
fn eph_metadata_env(
    workspace: &Workspace,
    running: &HashMap<String, RunningService>,
) -> Vec<(String, String)> {
    let mut vars = vec![
        ("EPH_WORKSPACE_ID".to_string(), workspace.id.clone()),
        (
            "EPH_WORKSPACE_ROOT".to_string(),
            workspace.path.display().to_string(),
        ),
        (
            "EPH_CONTAINER_PREFIX".to_string(),
            workspace.container_prefix(),
        ),
    ];

    for (name, svc) in running {
        let key = name.to_uppercase().replace('-', "_");
        vars.push((format!("EPH_{key}_HOST"), svc.host().to_string()));
        if let Some(port) = svc.port() {
            vars.push((format!("EPH_{key}_PORT"), port.to_string()));
        }
        for (port_name, port) in &svc.ports {
            if port_name != "default" {
                let pkey = port_name.to_uppercase().replace('-', "_");
                vars.push((format!("EPH_{key}_PORT_{pkey}"), port.to_string()));
            }
        }
        vars.push((
            format!("EPH_{key}_CONTAINER"),
            workspace.container_name(name),
        ));
    }

    vars
}

/// Summary of resources removed by [`ServiceManager::clean`].
#[derive(Debug, Default)]
pub struct CleanSummary {
    /// Number of services stopped and removed.
    pub services_removed: usize,
    /// Number of named volumes removed.
    pub volumes_removed: usize,
    /// Whether the persisted state directory was removed.
    pub state_removed: bool,
}

// ============================================================================
// Service State (Persistence)
// ============================================================================

/// Which backend is running a service, and the handle needed to manage it.
///
/// This replaces a stringly-typed id that previously encoded the backend by
/// prefix (a bare Docker container id, `pid:<n>`, or `compose:<project>`) and
/// hand-discriminated it with `strip_prefix` / `starts_with`. Making the three
/// cases distinct variants keeps the parsing in one place and makes an illegal
/// combination (e.g. a process backend with a container id) unrepresentable.
///
/// It is the single source of truth for a `run=` service's PID: there is no
/// longer a parallel `processes` map to keep in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Backend {
    /// A Docker container (`image=` / `dockerfile=`), by container id.
    Container { id: String },
    /// A `run=` shell command, by process id and, for new state, process
    /// identity. Non-zero because a real PID never is, and PID 0 is special on
    /// Unix (signaling it targets the caller's own process group), so the type
    /// forbids it outright.
    Process {
        pid: NonZeroU32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<proc::ProcessIdentity>,
    },
    /// A docker-compose project (`compose=`), by project name.
    Compose { project: String },
}

impl Backend {
    /// True if a recorded `Process` backend still refers to the process it
    /// tracked: the PID is alive and, when an identity was recorded at spawn
    /// time, that identity still matches. Guards every liveness probe against
    /// PID reuse; a bare `is_alive` would happily claim an unrelated process
    /// that inherited the number after a reboot, and teardown would then
    /// signal that innocent process. Backends other than `Process` return
    /// `false` (they have no PID to probe).
    fn process_is_alive(&self) -> bool {
        let Backend::Process { pid, identity } = self else {
            return false;
        };
        if !proc::is_alive(*pid) {
            return false;
        }
        match identity {
            Some(expected) => proc::identity_matches(*pid, expected),
            // Legacy state (written before identities were recorded): PID
            // presence is the best signal available.
            None => true,
        }
    }

    /// Parse a pre-typed-`Backend` state id back into a [`Backend`].
    ///
    /// Earlier versions stored a single `container_id` string that encoded the
    /// backend by prefix: `compose:<project>`, `pid:<n>`, or a bare Docker
    /// container id. This lets [`ServiceState::load`] migrate an on-disk state
    /// file written by such a version, so an in-place upgrade does not orphan
    /// running services or wedge `eph down` / `eph clean`.
    fn from_legacy_id(id: &str) -> Result<Self> {
        if let Some(project) = id.strip_prefix("compose:") {
            Ok(Backend::Compose {
                project: project.to_string(),
            })
        } else if let Some(pid) = id.strip_prefix("pid:") {
            let pid: NonZeroU32 = pid
                .parse()
                .with_context(|| format!("invalid legacy process id in state: {id:?}"))?;
            Ok(Backend::Process {
                pid,
                identity: None,
            })
        } else {
            Ok(Backend::Container { id: id.to_string() })
        }
    }
}

/// Persistent state for a workspace's services
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ServiceState {
    /// Running services keyed by service name
    pub(crate) services: HashMap<String, ServiceStateEntry>,
    /// The host ports last assigned to each `run=` service's auto ports
    /// (`port=auto`), keyed by service name then port name. Unlike
    /// [`services`](Self::services), this is *not* cleared by `eph down`, so the
    /// next `eph up` can reuse the same port and keep the app's URL stable across
    /// restarts and reboots. `eph clean` resets it along with the rest of state.
    #[serde(default)]
    pub(crate) auto_ports: HashMap<String, HashMap<String, u16>>,
}

/// State entry for a single service
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ServiceStateEntry {
    pub(crate) backend: Backend,
    pub(crate) ports: HashMap<String, u16>,
}

/// Deserialize accepting either the current schema (`backend`) or the legacy
/// one (a `container_id` string), so an on-disk state file written before the
/// [`Backend`] enum landed still loads after an upgrade. New writes always use
/// `backend` (see the derived [`Serialize`]); the legacy top-level `processes`
/// map is ignored, since the PID it held is recovered from `pid:<n>`.
impl<'de> Deserialize<'de> for ServiceStateEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Repr {
            #[serde(default)]
            backend: Option<Backend>,
            #[serde(default)]
            container_id: Option<String>,
            ports: HashMap<String, u16>,
        }

        let repr = Repr::deserialize(deserializer)?;
        let backend = match (repr.backend, repr.container_id) {
            (Some(backend), _) => backend,
            (None, Some(id)) => Backend::from_legacy_id(&id).map_err(serde::de::Error::custom)?,
            (None, None) => return Err(serde::de::Error::missing_field("backend")),
        };
        Ok(ServiceStateEntry {
            backend,
            ports: repr.ports,
        })
    }
}

impl ServiceState {
    /// Load state from disk.
    ///
    /// A missing file is an empty state. A file that exists but does not parse
    /// (a crash mid-write before atomic saves existed, manual editing, disk
    /// corruption) is quarantined rather than fatal: the broken file is moved
    /// aside to `state.json.corrupt` and an empty state is returned, so `eph
    /// clean` (the reset everyone reaches for at that point) can still run.
    /// Teardown recovers the containers themselves from Docker by name, so the
    /// only thing genuinely lost with the file is any `run=` service's PID.
    pub(crate) async fn load(workspace: &Workspace) -> Result<Self> {
        let path = state_file_path(workspace)?;

        if !path.exists() {
            return Ok(ServiceState::default());
        }

        let contents = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read state file: {}", path.display()))?;

        match serde_json::from_str(&contents) {
            Ok(state) => Ok(state),
            Err(e) => {
                let quarantine = path.with_extension("json.corrupt");
                warn!(
                    "state file {} is corrupt ({}); moving it to {} and continuing \
                     with empty state. Containers are recovered from Docker by \
                     name, but a `run=` service started before the corruption may \
                     need to be stopped by hand.",
                    path.display(),
                    e,
                    quarantine.display()
                );
                tokio::fs::rename(&path, &quarantine)
                    .await
                    .with_context(|| {
                        format!("failed to quarantine corrupt state file {}", path.display())
                    })?;
                Ok(ServiceState::default())
            }
        }
    }

    /// Save state to disk atomically: serialize to a sibling temp file, then
    /// rename it over the real one, so a crash mid-write can never leave a
    /// truncated `state.json` behind (rename replaces the destination on both
    /// Unix and Windows).
    pub(crate) async fn save(&self, workspace: &Workspace) -> Result<()> {
        let path = state_file_path(workspace)?;

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create state directory: {}", parent.display())
            })?;
        }

        let contents = serde_json::to_string_pretty(self).context("failed to serialize state")?;

        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, contents)
            .await
            .with_context(|| format!("failed to write state file: {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("failed to replace state file: {}", path.display()))?;

        Ok(())
    }
}

/// An exclusive, per-workspace lock over state-mutating commands (`up`,
/// `down`, `clean`), held for the duration of the operation.
///
/// Backed by an OS advisory lock (`flock` / `LockFileEx`) on a file in the
/// workspace's state directory, so it releases automatically when the process
/// exits for any reason: a killed `eph up` can never wedge the next command.
/// Two overlapping `eph up` runs used to each spawn services and then race
/// their `state.json` writes, with the loser's processes leaked untracked;
/// with the lock the second command simply waits.
pub(crate) struct WorkspaceLock {
    lock: fd_lock::RwLock<std::fs::File>,
}

impl WorkspaceLock {
    /// Open (creating if needed) the lock file for `workspace`.
    ///
    /// The file lives NEXT TO the workspace's state directory, not inside it:
    /// `eph clean` deletes the state directory while holding this lock, and on
    /// Windows a directory cannot be fully removed while an open, locked file
    /// lives in it.
    pub(crate) fn open(workspace: &Workspace) -> Result<Self> {
        let dir = workspace.state_dir()?;
        let parent = dir
            .parent()
            .context("workspace state directory has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create state root: {}", parent.display()))?;
        let dir_name = dir
            .file_name()
            .context("workspace state directory has no name")?
            .to_string_lossy()
            .into_owned();
        let path = parent.join(format!("{dir_name}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open workspace lock: {}", path.display()))?;
        Ok(Self {
            lock: fd_lock::RwLock::new(file),
        })
    }

    /// Acquire the exclusive lock, blocking until it is free. Logs a notice
    /// first when another eph command currently holds it, so a user watching a
    /// stalled `eph up` knows what it is waiting for.
    pub(crate) fn acquire(&mut self) -> Result<fd_lock::RwLockWriteGuard<'_, std::fs::File>> {
        // Probe without blocking purely to decide whether to log the notice; a
        // probe guard acquired here is dropped immediately and the real
        // acquisition below re-takes the lock.
        match self.lock.try_write() {
            Ok(_probe) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                info!("another eph command is running in this workspace; waiting for it");
            }
            Err(e) => return Err(e).context("failed to acquire workspace lock"),
        }
        self.lock
            .write()
            .context("failed to acquire workspace lock")
    }
}

fn state_file_path(workspace: &Workspace) -> Result<PathBuf> {
    Ok(workspace.state_dir()?.join("state.json"))
}

// ============================================================================
// Docker Client
// ============================================================================

/// Information about an existing container
pub(crate) struct ContainerInfo {
    pub(crate) id: String,
    pub(crate) is_running: bool,
    pub(crate) ports: HashMap<String, u16>,
}

/// Re-key raw container port bindings by their declared names.
///
/// `get_container` exposes host ports keyed by the container-port number (e.g.
/// `"9000"`) plus a positional `"default"`. This maps those onto the
/// user-facing names from the `.eph` file (`api`, `console`) so that
/// `${svc.port.<name>}` interpolation resolves. Ports with no declared name
/// fall back to `"default"`, matching the fresh-create path.
///
/// Used by both the create path (`run_image`) and the restart path
/// (`start_service`), so a container that is merely restarted rather than
/// recreated keeps its named ports.
fn map_named_ports(declared: &[PortMapping], raw: &HashMap<String, u16>) -> HashMap<String, u16> {
    let mut named = HashMap::new();
    for port_mapping in declared {
        let key = port_mapping.container_port.to_string();
        if let Some(&host_port) = raw.get(&key) {
            let name = port_mapping
                .name
                .clone()
                .unwrap_or_else(|| "default".to_string());
            named.insert(name, host_port);
        }
    }
    named
}

/// Outcome of waiting for a freshly-spawned `run=` process to become ready.
///
/// Distinguishes the case worth retrying (the process exited and its output
/// names a port conflict) from a clean readiness and an unrelated crash, so
/// [`ServiceManager::start_shell_command`] only re-launches on a fresh port when
/// re-launching could actually help.
#[derive(Debug, PartialEq, Eq)]
enum ReadyOutcome {
    /// The process is up (healthcheck passed, or it survived the startup grace
    /// period when no healthcheck is defined).
    Ready,
    /// The process exited during startup and its captured log looks like a port
    /// conflict (an "address already in use"-style message).
    PortConflict,
    /// The process exited during startup for some other reason.
    Exited,
}

/// Substrings (matched case-insensitively against a dead process's captured log)
/// that indicate it failed because its port was already taken. Covers the common
/// runtimes' phrasings: Node's `EADDRINUSE`, libc's "address already in use"
/// (Rust/Python/Go/.NET), and the "port already in use" wording several dev
/// servers print.
const PORT_CONFLICT_MARKERS: &[&str] = &[
    // Broadest phrasing -- covers "address already in use" (Go/Python/Rust/libc)
    // and "port <N> is already in use" (Vite and friends).
    "already in use",
    // BSD/macOS sometimes drops "already".
    "address in use",
    // Node's error code, in case it prints the code without the prose.
    "eaddrinuse",
];

/// Reserve a free TCP port on loopback for each declared mapping, returning the
/// name -> host-port map to hand the spawned process.
///
/// Fixed ports (`port=3000`) are used verbatim. Auto ports (`port=auto`) reuse
/// the previously-assigned port from `prev` when it is still free -- so a
/// restart keeps the same URL -- and otherwise take a fresh OS-assigned port.
/// Every reserved port is held (the listeners stay bound) until the whole map is
/// built, so two mappings never collide on the same number; the listeners are
/// then dropped together just before the caller spawns the process. That leaves
/// a small window in which another process could steal the port, which the
/// caller closes by re-launching on a fresh port if the process dies on a
/// conflict.
fn allocate_ports(
    declared: &[PortMapping],
    prev: Option<&HashMap<String, u16>>,
) -> Result<HashMap<String, u16>> {
    // Hold every reservation open until the map is fully built so the ports are
    // distinct; dropped on return, just before the process is spawned.
    let mut held: Vec<std::net::TcpListener> = Vec::new();
    let mut assigned: HashMap<String, u16> = HashMap::new();

    for mapping in declared {
        let name = mapping
            .name
            .clone()
            .unwrap_or_else(|| "default".to_string());

        if !mapping.auto {
            assigned.insert(name, mapping.container_port);
            continue;
        }

        // Prefer the port this mapping had last time, if it is still bindable and
        // not already taken by an earlier mapping in this same service, so URLs
        // stay stable across `eph down` / `eph up`.
        let reused = prev
            .and_then(|p| p.get(&name))
            .copied()
            .filter(|p| !assigned.values().any(|a| a == p))
            .and_then(|p| {
                std::net::TcpListener::bind(("127.0.0.1", p))
                    .ok()
                    .map(|l| (p, l))
            });

        let port = match reused {
            Some((p, listener)) => {
                held.push(listener);
                p
            }
            None => {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                    .context("failed to allocate a free host port")?;
                let p = listener
                    .local_addr()
                    .context("failed to read the allocated host port")?
                    .port();
                held.push(listener);
                p
            }
        };
        assigned.insert(name, port);
    }

    drop(held);
    Ok(assigned)
}

/// Whether `declared` contains at least one auto-allocated port, i.e. the
/// service is a managed app whose process eph may re-launch on a fresh port.
fn has_auto_port(declared: &[PortMapping]) -> bool {
    declared.iter().any(|p| p.auto)
}

fn process_backend(name: &str, pid: NonZeroU32) -> Backend {
    let identity = proc::identity(pid);
    if identity.is_none() {
        warn!(
            "could not record process identity for run= service {}; `eph system prune` will skip PID {}",
            name, pid
        );
    }
    Backend::Process { pid, identity }
}

/// Split a `command=` override into an argv vector, or `None` when the service
/// declares no override.
///
/// `command=` is freeform user input from the `.eph` file, so a malformed value
/// (most commonly an unbalanced quote) is a parse error, surfaced here at
/// startup with a message naming the service. The previous behavior fell back
/// to passing the entire unparsed string as a single argument, which made the
/// container fail later with a confusing error far from the real cause; failing
/// closed matches the repo's parse-don't-validate posture.
fn parse_command_override(
    name: &str,
    command_override: Option<&str>,
) -> Result<Option<Vec<String>>> {
    command_override
        .map(|c| {
            shell_words::split(c)
                .map_err(|e| anyhow!("invalid command override for service '{}': {}", name, e))
        })
        .transpose()
}

/// The byte length of a leading Windows drive prefix in `spec`, or `None` when
/// there is none.
///
/// Recognizes the plain `C:` form and the verbatim `\\?\C:` form. The colon in a
/// drive prefix is part of the source path, not the source/destination separator
/// that a volume spec uses, so callers skip past this prefix before scanning for
/// the real separator. Docker's own Windows client special-cases a single leading
/// drive letter the same way; matching it means a one-character named volume like
/// `x:/data` is read as drive `x:`, which is the accepted trade for drive-letter
/// support.
fn windows_drive_prefix_len(spec: &str) -> Option<usize> {
    let bytes = spec.as_bytes();
    // Plain drive: `C:`.
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(2);
    }
    // Verbatim drive: `\\?\C:`.
    if bytes.len() >= 6
        && bytes.starts_with(br"\\?\")
        && bytes[4].is_ascii_alphabetic()
        && bytes[5] == b':'
    {
        return Some(6);
    }
    None
}

/// Split a volume spec into its source and the remainder (`destination[:mode]`).
///
/// The separator is the first `:` that is not a Windows drive colon, so
/// `C:\path:/data` splits before `/data` rather than on the drive colon. Returns
/// `None` when the spec has no such separator (a source-only spec, passed through
/// unchanged so Docker reports the malformed mount itself).
fn split_volume_source(spec: &str) -> Option<(&str, &str)> {
    let skip = windows_drive_prefix_len(spec).unwrap_or(0);
    let sep = spec[skip..].find(':')? + skip;
    Some((&spec[..sep], &spec[sep + 1..]))
}

/// Whether a volume-spec source denotes a host path (a bind mount) rather than a
/// named volume.
///
/// Host paths are Unix absolute (`/`), workspace-relative (`.`), any
/// backslash-prefixed Windows path (UNC `\\server\share` or verbatim `\\?\`), or
/// a Windows drive-letter path (`C:\` or `C:/`). Everything else is a named
/// volume namespaced to the workspace.
fn is_host_path_source(source: &str) -> bool {
    source.starts_with('/')
        || source.starts_with('.')
        || source.starts_with('\\')
        || windows_drive_prefix_len(source).is_some()
}

/// Resolve one `volumes` entry into a Docker `-v` bind spec.
///
/// A host-path source (see [`is_host_path_source`]) is a bind mount: a leading
/// `.` is resolved relative to the workspace root, while an absolute path
/// (including a Windows drive-letter path like `C:\data`) is used as is. Anything
/// else is a named volume, namespaced to this workspace and service (via
/// [`Workspace::volume_name`]) so two workspaces, or two services, never collide
/// on a shared volume. A spec without a `:<container_path>` half is passed
/// through unchanged, so Docker reports the malformed mount itself.
///
/// The source/destination split is Windows-aware: a leading drive colon
/// (`C:\...` or `\\?\C:\...`) is part of the source, not the field separator, so
/// the drive colon is never mistaken for it (see [`split_volume_source`]).
///
/// # Errors
///
/// Returns an error if the resolved host source is a Windows extended-length
/// (`\\?\`) path that Docker cannot use as a mount source (see
/// [`reject_verbatim_bind_source`]), or if a relative source resolves to a path
/// that is not valid UTF-8.
fn resolve_volume_spec(spec: &str, workspace: &Workspace, service_name: &str) -> Result<String> {
    // Source-only or empty-source specs are passed through: Docker reports the
    // malformed mount itself rather than eph fabricating a bogus named volume.
    let Some((source, rest)) = split_volume_source(spec) else {
        return Ok(spec.to_string());
    };
    if source.is_empty() {
        return Ok(spec.to_string());
    }

    if is_host_path_source(source) {
        // Host-path bind mount.
        let host_path = if source.starts_with('.') {
            let joined = workspace.path.join(source);
            // to_str, not to_string_lossy: a lossy replacement in a bind source
            // would silently mount the wrong host path.
            joined
                .to_str()
                .with_context(|| {
                    format!("bind mount source {} is not valid UTF-8", joined.display())
                })?
                .to_string()
        } else {
            source.to_string()
        };
        reject_verbatim_bind_source(&host_path)?;
        Ok(format!("{host_path}:{rest}"))
    } else {
        // Named volume, namespaced to the workspace + service.
        let volume_name = workspace.volume_name(service_name, source);
        Ok(format!("{volume_name}:{rest}"))
    }
}

/// Reject a Windows extended-length ("verbatim") bind-mount source.
///
/// Docker's Windows volume parser rejects the `\\?\C:\...` and `\\?\UNC\...`
/// forms that `std`'s canonicalization emits, responding with a garbled
/// `\?\C%!(EXTRA string=is not a valid Windows path)` (the `%!(EXTRA ...)` is an
/// upstream moby `fmt` artifact). eph normalizes the workspace root away from
/// that form in [`Workspace::from_path`] via `dunce::canonicalize`, so this only
/// fires for a path long enough to have no ordinary Win32 representation: the
/// root keeps its `\\?\` prefix and a relative bind resolved against it inherits
/// it. Fail closed with an actionable message rather than forwarding a source the
/// daemon will only reject cryptically.
fn reject_verbatim_bind_source(source: &str) -> Result<()> {
    if source.starts_with(r"\\?\") {
        bail!(
            "bind mount source `{source}` is a Windows extended-length (\\\\?\\) path, \
             which Docker cannot use as a mount source. This happens when the workspace \
             path is long enough to require that prefix; move the workspace to a shorter \
             path and run eph again."
        );
    }
    Ok(())
}

/// Poll `probe` until it yields a result or `timeout_dur` elapses, sleeping
/// `interval` between attempts.
///
/// `probe` returns `Ok(Some(value))` to finish with that value, `Ok(None)` to
/// keep waiting, or `Err` to abort immediately. On timeout this returns a
/// single, consistent lowercase "failed to become healthy" error, so every
/// readiness path (Docker exec, `run=` shell probe, compose) shares one home
/// for the wait semantics and one error message.
///
/// The probe owns the command details and any success/`debug!` logging, since
/// what "ready" means (and whether it is even healthy, versus a classified
/// early exit) differs per backend. This only owns the start log, the timeout,
/// the sleep, and the failure message.
async fn wait_until_ready<T>(
    name: &str,
    timeout_dur: Duration,
    interval: Duration,
    mut probe: impl AsyncFnMut() -> Result<Option<T>>,
) -> Result<T> {
    info!(
        "Waiting for {} to be healthy (timeout: {}s)",
        name,
        timeout_dur.as_secs()
    );

    let polled = timeout(timeout_dur, async {
        loop {
            if let Some(value) = probe().await? {
                return Ok::<T, anyhow::Error>(value);
            }
            sleep(interval).await;
        }
    })
    .await;

    match polled {
        Ok(inner) => inner,
        Err(_) => bail!(
            "service {} failed to become healthy within {}s",
            name,
            timeout_dur.as_secs()
        ),
    }
}

/// The order services are brought up in.
///
/// Delegates to [`EphFile::start_order`], the single source of truth for start
/// sequencing: in roles mode it is the role graph's topological order, and in
/// legacy mode declaration order with `run=` services last. `start_services`
/// uses it to pick the phase-1 order, and `stop_all` / `clean` tear down in its
/// reverse, so a dependent is always stopped before the dependency it relies on
/// (its `pre-stop` hook sees the dependency still up).
fn start_order(eph: &EphFile) -> Vec<&String> {
    eph.start_order()
}

/// Whether a dead process's captured `log` names a port conflict, matched
/// case-insensitively against [`PORT_CONFLICT_MARKERS`]. Used to decide whether
/// re-launching the service on a fresh port could help.
fn log_indicates_port_conflict(log: &str) -> bool {
    let lower = log.to_ascii_lowercase();
    PORT_CONFLICT_MARKERS.iter().any(|m| lower.contains(m))
}

/// Where a service's logs come from, in the owned form [`stream_logs`] hands to
/// each per-service task.
enum LogSource {
    /// A captured `run=` log file at this path.
    File(PathBuf),
    /// A `docker ...` invocation whose piped output is the log (the args after
    /// the `docker` program, e.g. `["logs", "--follow", "<container>"]`).
    Docker(Vec<String>),
}

/// One complete log line, tagged with the service it came from.
struct LogLine {
    service: String,
    line: String,
}

/// Read a `run=` service's captured log file line by line, sending each whole
/// line to `tx`. When `follow` is set, keeps tailing appended bytes (holding any
/// partial trailing line until its newline arrives) until the task is aborted.
///
/// The file is never loaded whole: `--tail N` seeks to the start of the last `N`
/// lines via a bounded backward scan, and the forward read is chunked, so memory
/// stays bounded even for an unbounded long-running service's log.
async fn stream_file_lines(
    name: String,
    path: PathBuf,
    follow: bool,
    tail: Option<usize>,
    tx: mpsc::Sender<LogLine>,
) -> Result<()> {
    // A missing file just means the service has not started yet -- not an error.
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Ok(());
    };
    let len = file.seek(SeekFrom::End(0)).unwrap_or(0);

    // Start at the last `tail` lines, or the whole file. tail_start_offset scans
    // backward in blocks, so we read about `tail` lines' worth, not the whole file.
    let start = match tail {
        Some(n) => tail_start_offset(&mut file, len, n).unwrap_or(0),
        None => 0,
    };
    let mut offset = start;
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Ok(());
    }

    // Read forward in chunks, emitting complete lines as they appear. Bytes are
    // buffered (not decoded) until a line completes, so a multi-byte UTF-8 char
    // straddling a chunk boundary is never split into replacement characters.
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    // `while let Ok` treats a read error as EOF, ending the dump.
    while let Ok(read) = file.read(&mut buf) {
        if read == 0 {
            break;
        }
        offset += read as u64;
        pending.extend_from_slice(&buf[..read]);
        if !drain_complete_lines(&name, &mut pending, &tx).await {
            return Ok(());
        }
    }

    if !follow {
        // A trailing line with no newline is still real output.
        if !pending.is_empty() {
            let _ = tx.send(line_for(&name, decode_log_line(&pending))).await;
        }
        return Ok(());
    }

    // Follow: poll for appended bytes, carrying `pending` across polls so a
    // partial line is only emitted once its newline arrives.
    loop {
        sleep(Duration::from_millis(200)).await;

        let len = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            // The file may briefly vanish if the workspace is cleaned mid-follow.
            Err(_) => continue,
        };
        // A shorter file was truncated/rotated (e.g. the service restarted):
        // start over from the new beginning.
        if len < offset {
            offset = 0;
            pending.clear();
        }
        if len <= offset {
            continue;
        }

        let Ok(mut file) = std::fs::File::open(&path) else {
            continue;
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        while let Ok(read) = file.read(&mut buf) {
            if read == 0 {
                break;
            }
            offset += read as u64;
            pending.extend_from_slice(&buf[..read]);
            if !drain_complete_lines(&name, &mut pending, &tx).await {
                return Ok(());
            }
        }
    }
}

/// Run a `docker ...` command and stream its output line by line to `tx`.
///
/// stdout and stderr are read concurrently (both carry useful output, e.g. many
/// servers log to stderr), so a line is sent the moment it completes on either.
/// The child is spawned with `kill_on_drop` so aborting this task also kills the
/// underlying `docker logs -f`.
///
/// Returns an error if `docker` cannot be spawned or exits non-zero (a removed
/// container, a daemon that is down, a malformed compose file), so the caller can
/// fail the overall `eph logs` rather than masking it as ordinary output. Any
/// error text `docker` printed to stderr is still streamed through `tx` first.
async fn stream_docker_lines(
    name: String,
    args: Vec<String>,
    tx: mpsc::Sender<LogLine>,
) -> Result<()> {
    let label = if args.first().map(String::as_str) == Some("compose") {
        "docker compose logs"
    } else {
        "docker logs"
    };

    let mut child = TokioCommand::new("docker")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!("failed to run `{label}` for service '{name}' (is docker on PATH?)")
        })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (name_out, name_err) = (name.clone(), name.clone());
    let (tx_out, tx_err) = (tx.clone(), tx);

    let read_stdout = async move {
        if let Some(stream) = stdout {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx_out.send(line_for(&name_out, line)).await.is_err() {
                    break;
                }
            }
        }
    };
    let read_stderr = async move {
        if let Some(stream) = stderr {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx_err.send(line_for(&name_err, line)).await.is_err() {
                    break;
                }
            }
        }
    };

    tokio::join!(read_stdout, read_stderr);

    let status = child
        .wait()
        .await
        .with_context(|| format!("failed waiting on `{label}` for service '{name}'"))?;
    if !status.success() {
        let code = status.code().map_or_else(
            || " (terminated by signal)".to_string(),
            |c| format!(" (exit {c})"),
        );
        bail!("`{label}` for service '{name}' failed{code}");
    }
    Ok(())
}

/// Build a [`LogLine`] for `service` from an owned `line`.
fn line_for(service: &str, line: String) -> LogLine {
    LogLine {
        service: service.to_string(),
        line,
    }
}

/// Create `dir` (and any missing parents) for captured logs, owner-only (0700)
/// on Unix since the logs it holds can contain secrets. Idempotent: an existing
/// directory is left as-is.
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Create (truncating) a captured-log file, owner-only (0600) on Unix since it
/// can contain secrets. Mirrors `File::create` otherwise.
fn create_private_log_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Drain every complete (newline-terminated) line from `pending` and send it to
/// `tx`, leaving any unterminated trailing bytes in the buffer for the next read.
/// Returns `false` if the receiver has hung up.
async fn drain_complete_lines(
    name: &str,
    pending: &mut Vec<u8>,
    tx: &mpsc::Sender<LogLine>,
) -> bool {
    while let Some(idx) = pending.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = pending.drain(..=idx).collect();
        if tx
            .send(line_for(name, decode_log_line(&line_bytes)))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Decode one log line's bytes, dropping a trailing `\n` (and a preceding `\r`
/// for CRLF). Bytes are decoded lossily so a stray non-UTF-8 byte does not abort
/// `eph logs`.
fn decode_log_line(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Byte offset at which the last `n` lines of an open file begin.
///
/// Scans backward from `len` in fixed-size blocks, counting line breaks, so only
/// about `n` lines' worth of data is read rather than the whole (possibly huge)
/// file. A single trailing newline is treated as terminating the last line, not
/// as starting an empty one. Returns 0 when the file has `n` or fewer lines, and
/// `len` when `n` is 0.
fn tail_start_offset(file: &mut std::fs::File, len: u64, n: usize) -> std::io::Result<u64> {
    if n == 0 || len == 0 {
        return if n == 0 { Ok(len) } else { Ok(0) };
    }

    const BLOCK: u64 = 8192;
    let mut pos = len;
    let mut newlines = 0usize;
    // Until we have examined the file's final byte, a newline there ends the last
    // line rather than introducing a new one, so it must not be counted.
    let mut at_file_end = true;

    while pos > 0 {
        let read_size = BLOCK.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_size as usize];
        file.read_exact(&mut chunk)?;

        for i in (0..chunk.len()).rev() {
            if chunk[i] != b'\n' {
                at_file_end = false;
                continue;
            }
            if at_file_end && pos + i as u64 == len - 1 {
                // The file's trailing newline: skip it, don't count it.
                at_file_end = false;
                continue;
            }
            at_file_end = false;
            newlines += 1;
            if newlines == n {
                // The last n lines begin just past this newline.
                return Ok(pos + i as u64 + 1);
            }
        }
    }
    Ok(0)
}

/// Docker client wrapper
pub(crate) struct DockerClient {
    client: Docker,
}

impl DockerClient {
    /// Connect to Docker daemon
    pub(crate) async fn connect() -> Result<Self> {
        let client = Docker::connect_with_local_defaults()
            .context("failed to connect to docker (is docker running?)")?;

        // Verify connection
        client
            .ping()
            .await
            .context("failed to ping docker daemon")?;

        Ok(DockerClient { client })
    }

    /// Get information about a container by name
    pub(crate) async fn get_container(&self, name: &str) -> Result<Option<ContainerInfo>> {
        let filters: HashMap<String, Vec<String>> =
            HashMap::from([("name".to_string(), vec![name.to_string()])]);

        let containers = self
            .client
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .context("failed to list containers")?;

        // Find exact match (Docker's name filter is a prefix match)
        let container = containers.into_iter().find(|c| {
            c.names
                .as_ref()
                .is_some_and(|names| names.iter().any(|n| n == &format!("/{}", name)))
        });

        let Some(container) = container else {
            return Ok(None);
        };

        let is_running = container.state == Some(ContainerSummaryStateEnum::RUNNING);

        // Extract port mappings
        let mut ports = HashMap::new();
        if let Some(port_bindings) = container.ports {
            for port in port_bindings {
                if let Some(public_port) = port.public_port {
                    let private_port = port.private_port;
                    // Use private port as the key name for now
                    ports.insert(private_port.to_string(), public_port);
                    // Also set as "default" if it's the first one
                    if ports.len() == 1 {
                        ports.insert("default".to_string(), public_port);
                    }
                }
            }
        }

        Ok(Some(ContainerInfo {
            id: container.id.unwrap_or_default(),
            is_running,
            ports,
        }))
    }

    /// Return whether any container in a `docker compose` project is running.
    ///
    /// Compose-backed services are not named `eph-<id>-<service>` like the
    /// containers eph creates directly; `docker compose` names them
    /// `<project>-<service>-N`. They are therefore looked up by the
    /// `com.docker.compose.project` label rather than by container name, so that
    /// [`ServiceManager::status`] can recognize a running compose service and
    /// expose its ports for interpolation.
    pub(crate) async fn compose_project_running(&self, project: &str) -> Result<bool> {
        let filters: HashMap<String, Vec<String>> = HashMap::from([(
            "label".to_string(),
            vec![format!("com.docker.compose.project={project}")],
        )]);

        let containers = self
            .client
            .list_containers(Some(
                // all(false): only currently-running containers are reported.
                ListContainersOptionsBuilder::new()
                    .all(false)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .context("failed to list compose project containers")?;

        Ok(!containers.is_empty())
    }

    /// Start an existing container
    pub(crate) async fn start_container(&self, id: &str) -> Result<()> {
        self.client
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .context("failed to start container")?;
        Ok(())
    }

    /// Stop a container. Returns `true` if a running container was actually
    /// stopped, `false` if there was nothing running under that name.
    pub(crate) async fn stop_container(&self, name: &str) -> Result<bool> {
        if let Some(info) = self.get_container(name).await?
            && info.is_running
        {
            info!("Stopping container {}", name);
            self.client
                .stop_container(
                    &info.id,
                    Some(StopContainerOptionsBuilder::new().t(10).build()),
                )
                .await
                .context("failed to stop container")?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Remove a container. Returns `true` if a container existed and was
    /// removed, `false` if there was nothing under that name.
    pub(crate) async fn remove_container(&self, name: &str) -> Result<bool> {
        if let Some(info) = self.get_container(name).await? {
            info!("Removing container {}", name);
            self.client
                .remove_container(
                    &info.id,
                    Some(RemoveContainerOptionsBuilder::new().force(true).build()),
                )
                .await
                .context("failed to remove container")?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Remove a named volume, ignoring "not found" errors. Returns `true` if
    /// the volume existed and was removed, `false` if it was already gone.
    pub(crate) async fn remove_volume(&self, name: &str) -> Result<bool> {
        use bollard::errors::Error as BollardError;

        info!("Removing volume {}", name);
        match self
            .client
            .remove_volume(
                name,
                Some(RemoveVolumeOptionsBuilder::new().force(true).build()),
            )
            .await
        {
            Ok(()) => Ok(true),
            // Volume already gone (or never created) - treat as success.
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("failed to remove volume {}", name)),
        }
    }

    /// Names of all containers (running or not) whose name starts with
    /// `prefix`. Docker reports names with a leading `/`, which is stripped.
    /// Used by `eph clean`'s leftover sweep.
    pub(crate) async fn containers_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let containers = self
            .client
            .list_containers(Some(ListContainersOptionsBuilder::new().all(true).build()))
            .await
            .context("failed to list containers")?;
        let mut names = Vec::new();
        for container in containers {
            for name in container.names.unwrap_or_default() {
                let name = name.strip_prefix('/').unwrap_or(&name);
                if name.starts_with(prefix) {
                    names.push(name.to_string());
                    break;
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Names of all volumes whose name starts with `prefix`. Used by `eph
    /// clean`'s leftover sweep.
    pub(crate) async fn volumes_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let volumes = self
            .client
            .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>)
            .await
            .context("failed to list volumes")?;
        let mut names: Vec<String> = volumes
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.name)
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Execute a command inside a running container
    pub(crate) async fn exec_in_container(&self, container_id: &str, cmd: &[&str]) -> Result<i64> {
        use bollard::exec::StartExecResults;
        use bollard::models::ExecConfig;

        let exec = self
            .client
            .create_exec(
                container_id,
                ExecConfig {
                    cmd: Some(cmd.iter().map(ToString::to_string).collect()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("failed to create exec")?;

        let start_result = self
            .client
            .start_exec(&exec.id, None)
            .await
            .context("failed to start exec")?;

        // Consume output
        if let StartExecResults::Attached { mut output, .. } = start_result {
            while let Some(msg) = output.next().await {
                if let Err(e) = msg {
                    warn!("Exec output error: {}", e);
                }
            }
        }

        // Get exit code
        let inspect = self
            .client
            .inspect_exec(&exec.id)
            .await
            .context("failed to inspect exec")?;

        Ok(inspect.exit_code.unwrap_or(-1))
    }

    /// Pull an image and run it as a container.
    ///
    /// `cmd` is the already-parsed `command=` override (see
    /// [`parse_command_override`]), validated by the caller before any
    /// container reuse so a malformed value fails closed on every start path.
    ///
    /// Returns the [`RunningService`] connection info plus the created
    /// container's id, which the caller needs to probe health and to record the
    /// [`Backend::Container`] in state.
    pub(crate) async fn run_image(
        &self,
        container_name: &str,
        image: &str,
        service: &Service,
        workspace: &Workspace,
        cmd: Option<Vec<String>>,
        running: &HashMap<String, RunningService>,
    ) -> Result<(RunningService, String)> {
        // Pull image if needed
        self.ensure_image(image).await?;

        // Build port bindings - let Docker assign host ports
        let mut exposed_ports: Vec<String> = Vec::new();
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();

        for port_mapping in &service.ports {
            let container_port = format!("{}/tcp", port_mapping.container_port);
            exposed_ports.push(container_port.clone());
            // Empty host port = random assignment
            port_bindings.insert(
                container_port,
                Some(vec![PortBinding {
                    // Bind to loopback only: published ports must be reachable
                    // from localhost (RunningService::host() returns "localhost"),
                    // not from the entire network.
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: None, // Random port
                }]),
            );
        }

        // Build environment variables
        // Resolve `${service.property}` references in env values against the
        // services already up, exactly as the `run=` path does (`app_env`).
        // These used to be passed into the container verbatim, so the same
        // documented syntax silently shipped a literal `${postgres.port}`
        // string. Note the resolved host/port are as seen FROM THE HOST;
        // container-to-container traffic may need `host.docker.internal`.
        let env: Vec<String> = service
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, resolve_against(v, running)))
            .collect();

        // Build volume bindings
        let binds: Vec<String> = service
            .volumes
            .iter()
            .map(|v| resolve_volume_spec(v, workspace, &service.name))
            .collect::<Result<Vec<_>>>()?;

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            binds: Some(binds),
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(image.to_string()),
            exposed_ports: Some(exposed_ports),
            env: Some(env),
            host_config: Some(host_config),
            cmd,
            ..Default::default()
        };

        // Create container
        debug!("Creating container {} from image {}", container_name, image);
        let response = self
            .client
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.to_string()),
                    ..Default::default()
                }),
                config,
            )
            .await
            .context("failed to create container")?;

        // Start container
        self.client
            .start_container(
                &response.id,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .context("failed to start container")?;

        // Get assigned ports
        let info = self
            .get_container(container_name)
            .await?
            .context("container disappeared after creation")?;

        // Map port names
        let named_ports = map_named_ports(&service.ports, &info.ports);

        Ok((
            RunningService {
                name: service.name.clone(),
                ports: named_ports,
            },
            response.id,
        ))
    }

    /// Build from Dockerfile and run
    pub(crate) async fn build_and_run(
        &self,
        container_name: &str,
        dockerfile_path: &std::path::Path,
        service: &Service,
        workspace: &Workspace,
        cmd: Option<Vec<String>>,
        running: &HashMap<String, RunningService>,
    ) -> Result<(RunningService, String)> {
        let image_tag = format!("eph-{}-{}", workspace.short_id, service.name);

        // Determine build context
        let build_context = if let Some(ctx) = &service.build_context {
            workspace.path.join(ctx)
        } else {
            dockerfile_path
                .parent()
                .unwrap_or(dockerfile_path)
                .to_path_buf()
        };

        // Build image
        info!(
            "Building image {} from {}",
            image_tag,
            dockerfile_path.display()
        );

        let output = TokioCommand::new("docker")
            .args([
                "build",
                "-t",
                &image_tag,
                "-f",
                &dockerfile_path.to_string_lossy(),
                &build_context.to_string_lossy(),
            ])
            .output()
            .await
            .context("failed to run docker build")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker build failed:\n{}", stderr);
        }

        // Now run like a normal image
        self.run_image(container_name, &image_tag, service, workspace, cmd, running)
            .await
    }

    /// Ensure an image is available locally
    async fn ensure_image(&self, image: &str) -> Result<()> {
        // Check if image exists
        if self.client.inspect_image(image).await.is_ok() {
            debug!("Image {} already exists", image);
            return Ok(());
        }

        info!("Pulling image {}", image);
        let mut stream = self.client.create_image(
            Some(CreateImageOptionsBuilder::new().from_image(image).build()),
            None,
            None,
        );

        while let Some(result) = stream.next().await {
            result.context("failed to pull image")?;
        }

        Ok(())
    }
}

// ============================================================================
// Service Manager
// ============================================================================

/// Which lifecycle hooks a bring-up runs.
///
/// `eph up` uses [`All`](Hooks::All) (or [`None`](Hooks::None) under
/// `--skip-hooks`). [`PreStartOnly`](Hooks::PreStartOnly) exists for `eph
/// dev`, which starts its backing services first and the foreground app after:
/// pre-start hooks stay interleaved per service exactly as under `up`, but the
/// post-start phase is deferred until the app is up too, preserving the rule
/// that a post-start hook may reference any service's assigned port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hooks {
    /// Run pre-start hooks interleaved per service and post-start hooks once
    /// everything is healthy.
    All,
    /// Run no hooks at all (`--skip-hooks`).
    None,
    /// Run pre-start hooks interleaved per service, but skip the post-start
    /// phase; the caller runs it later via
    /// [`ServiceManager::run_all_post_start`].
    PreStartOnly,
}

impl Hooks {
    /// The hooks `eph up`-style commands run for a `--skip-hooks` flag.
    #[must_use]
    pub fn from_skip_flag(skip_hooks: bool) -> Self {
        if skip_hooks { Hooks::None } else { Hooks::All }
    }
}

/// Manager for all services in a workspace.
///
/// Owns the [`Workspace`], a Docker connection, and the persisted service
/// state, and drives the service lifecycle (start, stop, status, clean).
/// Construct one with [`ServiceManager::new`].
pub struct ServiceManager {
    workspace: Workspace,
    docker: DockerClient,
    state: ServiceState,
}

impl ServiceManager {
    /// Create a new service manager for a workspace.
    ///
    /// Connects to the local Docker daemon and loads any persisted state for
    /// the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the Docker daemon cannot be reached, or if the
    /// persisted state file exists but cannot be read or parsed.
    pub async fn new(workspace: Workspace) -> Result<Self> {
        let docker = DockerClient::connect().await?;
        workspace.save_metadata().await?;
        let state = ServiceState::load(&workspace).await?;
        Ok(ServiceManager {
            workspace,
            docker,
            state,
        })
    }

    /// Start every service defined in the [`EphFile`] and persist state.
    ///
    /// Convenience wrapper over [`start_services`](Self::start_services) with no
    /// filter.
    ///
    /// # Errors
    ///
    /// Returns an error if any service fails to start or if state cannot be
    /// saved.
    pub async fn start_all(&mut self, eph: &EphFile) -> Result<HashMap<String, RunningService>> {
        self.start_services(eph, &[], Hooks::All).await
    }

    /// Start the requested services (or all of them when `filter` is empty),
    /// running `pre-start` hooks before each service comes up and `post-start`
    /// hooks once every service is healthy.
    ///
    /// Startup happens in two phases:
    ///
    /// 1. In start order, each target service runs its `pre-start` hooks and is
    ///    then created (or reused) and waited on until healthy. A `pre-start`
    ///    hook sees the services already up at that point, but not its own
    ///    not-yet-assigned port; it is the place for prep the service depends on
    ///    (codegen, a generated config).
    /// 2. Every target service's `post-start` hooks run with the fully-resolved
    ///    environment.
    ///
    /// Deferring `post-start` to phase 2 means such a hook can reference any
    /// service in the workspace -- a database migration whose `DATABASE_URL`
    /// interpolates `${postgres.port}` resolves correctly even though, within a
    /// single `eph up`, postgres might have been created before the service whose
    /// hook needs it.
    ///
    /// `pre-start` and `post-start` hooks run on **every** `eph up`, not only
    /// when a service is freshly created. Hooks are therefore expected to be
    /// idempotent (a migration that no-ops when already applied, an
    /// `INSERT ... ON CONFLICT` seed); use [`eph run`](crate) for one-off,
    /// non-idempotent operations.
    ///
    /// `hooks` selects which hook phases run; see [`Hooks`].
    ///
    /// # Errors
    ///
    /// Returns an error if a service name in `filter` is unknown, if a
    /// `pre-start` hook fails (the service it precedes is not started), if any
    /// service fails to start, if a `post-start` hook fails, or if state cannot
    /// be saved.
    pub async fn start_services(
        &mut self,
        eph: &EphFile,
        filter: &[String],
        hooks: Hooks,
    ) -> Result<HashMap<String, RunningService>> {
        // One state-mutating eph command per workspace at a time. Without this,
        // two overlapping `eph up` runs each spawn services and race their
        // state writes, and the loser's processes leak untracked.
        let mut lock = WorkspaceLock::open(&self.workspace)?;
        let _guard = lock.acquire()?;
        // Re-read state under the lock: another command may have finished
        // between this manager's construction and lock acquisition.
        self.state = ServiceState::load(&self.workspace).await?;

        // Resolve the target set: every service, or just the requested ones (in
        // the order requested). post-start hooks run in a second phase once all
        // of these are healthy, so the phase-1 start order does not affect
        // whether a hook's cross-service references resolve.
        // Backing services (image/dockerfile/compose) start before run= apps so
        // a managed app's environment can reference the services it depends on
        // (e.g. ${postgres.port}) at spawn time. `start_order` encodes this for
        // the full set (and is mirrored, reversed, by teardown); a filtered
        // request keeps the requested order but applies the same command-last
        // rule with a stable sort.
        let targets: Vec<&String> = if filter.is_empty() {
            start_order(eph)
        } else {
            for name in filter {
                if !eph.services.contains_key(name) {
                    bail!("unknown service: {}", name);
                }
            }
            // Keep the requested subset, but bring them up in the global start
            // order (topological in roles mode, command-last in legacy mode)
            // rather than the order the names were passed, so a filtered `eph up`
            // still respects dependencies.
            let wanted: HashSet<&str> = filter.iter().map(String::as_str).collect();
            start_order(eph)
                .into_iter()
                .filter(|name| wanted.contains(name.as_str()))
                .collect()
        };

        // Phase 1: run each target's pre-start hook, then create or reuse it,
        // waiting for health.
        //
        // A pre-start hook runs immediately before its own service is created, so
        // preparatory work the service depends on -- codegen a Go server needs to
        // compile, a config file a container mounts -- completes before the thing
        // that consumes it boots. Because start_order brings backing services up
        // before run= apps, a run= app's pre-start already sees those services'
        // ports; it cannot see its own not-yet-assigned port. `resolved` tracks
        // the live environment as it grows: it starts from services already up
        // (a filtered `eph up` of one service, say) and each freshly started
        // service is merged in so later pre-start hooks see earlier ones.
        let mut running = HashMap::new();
        let mut resolved = self.status().await?;
        for name in &targets {
            let service = &eph.services[*name];
            if matches!(hooks, Hooks::All | Hooks::PreStartOnly) {
                self.run_service_pre_start(eph, &resolved, service).await?;
            }
            let result = self.create_service(name, service, eph, &resolved).await?;
            // Persist after every service, not once at the end: if a later
            // target's pre-start hook or creation fails, `eph up` exits with
            // this in-memory state discarded, and anything not yet on disk is
            // a process or container that `eph down` cannot see. run= services
            // used to leak exactly this way.
            self.state.save(&self.workspace).await?;
            resolved.insert((*name).clone(), result.clone());
            running.insert((*name).clone(), result);
        }

        if !matches!(hooks, Hooks::All) {
            return Ok(running);
        }

        // Phase 2: run post-start hooks with the full environment. `resolved`
        // already merges the services running from a previous `up` with the ones
        // just started, so cross-service references resolve even on a filtered
        // `eph up <one-service>`.
        for name in &targets {
            self.run_service_post_start(eph, &resolved, &eph.services[*name])
                .await?;
        }

        Ok(running)
    }

    /// Run one service's `pre-start` hooks against an already-resolved set of
    /// running services.
    ///
    /// A no-op when the service declares no hooks. Runs before the service is
    /// created, so the resolved environment reflects only the services already
    /// up (never the service's own port). Shared by the `eph up` first phase and
    /// by [`run_pre_start_for`](Self::run_pre_start_for).
    async fn run_service_pre_start(
        &self,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
        service: &Service,
    ) -> Result<()> {
        if service.pre_start.is_empty() {
            return Ok(());
        }
        info!("Running pre-start hooks for {}", service.name);
        let env = self.hook_env(eph, running, service);
        for cmd in &service.pre_start {
            self.run_hook(cmd, &env)
                .await
                .with_context(|| format!("pre-start hook failed for service '{}'", service.name))?;
        }
        Ok(())
    }

    /// Run a single named service's `pre-start` hooks against the services
    /// currently up.
    ///
    /// `eph dev` calls this for the foreground app immediately before starting
    /// it, mirroring `eph up`'s interleaving: the hook sees every backing
    /// service already up (so `${postgres.port}` resolves), never the app's own
    /// not-yet-assigned port. `eph dev` used to run every service's pre-start
    /// up front instead, before anything existed, so the same hook resolved
    /// differently under `dev` than under `up`.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is unknown or a `pre-start` hook fails.
    pub async fn run_pre_start_for(&self, eph: &EphFile, name: &str) -> Result<()> {
        let service = eph
            .services
            .get(name)
            .with_context(|| format!("unknown service: {name}"))?;
        let running = self.status().await?;
        self.run_service_pre_start(eph, &running, service).await
    }

    /// Run one service's `post-start` hooks against an already-resolved set of
    /// running services.
    ///
    /// A no-op when the service declares no hooks. Shared by the `eph up` second
    /// phase and by [`run_all_post_start`](Self::run_all_post_start), so the
    /// seeding semantics (resolved environment injected, a failing hook aborts)
    /// are identical however the service was brought up.
    async fn run_service_post_start(
        &self,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
        service: &Service,
    ) -> Result<()> {
        if service.post_start.is_empty() {
            return Ok(());
        }
        info!("Running post-start hooks for {}", service.name);
        let env = self.hook_env(eph, running, service);
        for cmd in &service.post_start {
            self.run_hook(cmd, &env).await.with_context(|| {
                format!("post-start hook failed for service '{}'", service.name)
            })?;
        }
        Ok(())
    }

    /// Run every declared service's `post-start` hooks once, after all services
    /// are healthy.
    ///
    /// `eph dev` calls this once the backing services and the foreground app are
    /// all up, preserving the `eph up` guarantee that a hook may reference any
    /// service's assigned port (a seed whose `DATABASE_URL` interpolates
    /// `${postgres.port}`, say). Hooks run in start order (topological in roles
    /// mode, matching `eph up`) against a single resolved snapshot of the running
    /// services.
    ///
    /// # Errors
    ///
    /// Returns an error if any `post-start` hook fails.
    pub async fn run_all_post_start(&self, eph: &EphFile) -> Result<()> {
        let running = self.status().await?;
        // Run in start order (topological in roles mode), matching `eph up`, so a
        // dependency role's post-start hook runs before a dependent's even when
        // the services are declared out of role order. `run_all_pre_start` already
        // does this; keep the two consistent.
        for name in start_order(eph) {
            self.run_service_post_start(eph, &running, &eph.services[name])
                .await?;
        }
        Ok(())
    }

    /// Start a single service, reusing an already-running instance if present.
    ///
    /// Docker-backed services (`image`/`dockerfile`) are created or restarted and
    /// waited on until healthy; `run` services spawn a process and `compose`
    /// services shell out to `docker compose`. Idempotent: a service that is
    /// already running is detected and returned without starting a duplicate.
    ///
    /// # Errors
    ///
    /// Returns an error if the image cannot be pulled or built, the container or
    /// process cannot be started, or the service fails its healthcheck within
    /// the configured timeout.
    ///
    /// This only brings the service to a healthy state; `post-start` hooks are
    /// run separately by [`start_services`](Self::start_services) once every
    /// service is up.
    async fn create_service(
        &mut self,
        name: &str,
        service: &Service,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
    ) -> Result<RunningService> {
        let container_name = self.workspace.container_name(name);

        // Validate (and parse) the command override up front, before any
        // existing-container reuse/restart fast path below. Otherwise an edited
        // `.eph` with a malformed `command=` could still "succeed" by reusing a
        // stale container, defeating the fail-closed intent. Only image and
        // dockerfile services use it; for the rest this is `None`.
        let command = parse_command_override(name, service.command_override.as_deref())?;

        // Dedup run= (shell command) services: the Docker-based guard below
        // explicitly skips ServiceSource::Command, so without this check running
        // `eph up` twice would spawn a second process and orphan the first.
        // Probe the tracked PID the same way status() does.
        if matches!(service.source, ServiceSource::Command(_))
            && let Some(entry) = self.state.services.get(name)
            && let Backend::Process { pid, .. } = &entry.backend
            && entry.backend.process_is_alive()
        {
            info!("Service {} already running (PID {})", name, pid);
            return Ok(RunningService {
                name: name.to_string(),
                ports: entry.ports.clone(),
            });
        }

        // Check if already running (for Docker-based services)
        if !matches!(
            service.source,
            ServiceSource::Command(_) | ServiceSource::Compose(_)
        ) && let Some(existing) = self.docker.get_container(&container_name).await?
        {
            if existing.is_running {
                info!("Service {} already running", name);
                // Re-map declared port names onto the raw host ports, exactly as
                // the fresh-create path does, so named-port interpolation keeps
                // resolving across an `eph up` on an already-running container.
                let named_ports = map_named_ports(&service.ports, &existing.ports);
                // Record in state even for already-running containers
                self.state.services.insert(
                    name.to_string(),
                    ServiceStateEntry {
                        backend: Backend::Container { id: existing.id },
                        ports: named_ports.clone(),
                    },
                );
                return Ok(RunningService {
                    name: name.to_string(),
                    ports: named_ports,
                });
            } else {
                // Container exists but not running, start it
                info!("Starting existing container for {}", name);
                self.docker.start_container(&existing.id).await?;
                let refreshed = self
                    .docker
                    .get_container(&container_name)
                    .await?
                    .context("container disappeared after start")?;

                // Wait for health check
                self.wait_for_healthy(name, service, &refreshed.id).await?;

                // Re-map declared port names onto the refreshed host ports. The
                // restart path otherwise records raw container-port-number keys
                // (e.g. "9000"), which breaks `${svc.port.<name>}` after a
                // down/up cycle. Mirrors the fresh-create path.
                let named_ports = map_named_ports(&service.ports, &refreshed.ports);

                // Record in state
                self.state.services.insert(
                    name.to_string(),
                    ServiceStateEntry {
                        backend: Backend::Container { id: refreshed.id },
                        ports: named_ports.clone(),
                    },
                );

                return Ok(RunningService {
                    name: name.to_string(),
                    ports: named_ports,
                });
            }
        }

        // Create and start new service
        info!("Creating service {}", name);
        let (running, backend) = match &service.source {
            ServiceSource::Image(image) => {
                let (r, id) = self
                    .docker
                    .run_image(
                        &container_name,
                        image,
                        service,
                        &self.workspace,
                        command,
                        running,
                    )
                    .await?;

                // Wait for health check
                self.wait_for_healthy(name, service, &id).await?;

                (r, Backend::Container { id })
            }
            ServiceSource::Dockerfile(path) => {
                let dockerfile_path = self.workspace.path.join(path);
                let (r, id) = self
                    .docker
                    .build_and_run(
                        &container_name,
                        &dockerfile_path,
                        service,
                        &self.workspace,
                        command,
                        running,
                    )
                    .await?;

                // Wait for health check
                self.wait_for_healthy(name, service, &id).await?;

                (r, Backend::Container { id })
            }
            ServiceSource::Command(cmd) => {
                self.start_shell_command(name, cmd, service, eph).await?
            }
            ServiceSource::Compose(path) => {
                self.start_compose(name, path, service, running).await?
            }
        };

        // Record in state
        self.state.services.insert(
            name.to_string(),
            ServiceStateEntry {
                backend,
                ports: running.ports.clone(),
            },
        );

        // post-start hooks run in a later phase (see `start_services`), once
        // every service is healthy, so a hook can reference any service.
        Ok(running)
    }

    /// Wait for a service to become healthy
    async fn wait_for_healthy(
        &self,
        name: &str,
        service: &Service,
        container_id: &str,
    ) -> Result<()> {
        let Some(ref healthcheck) = service.healthcheck else {
            // No health check defined, just wait a bit
            sleep(Duration::from_millis(500)).await;
            return Ok(());
        };

        let timeout_dur = Duration::from_secs(service.ready_timeout_secs.unwrap_or(30));
        wait_until_ready(name, timeout_dur, Duration::from_secs(1), async || {
            // Parse healthcheck command. An empty command is treated as ready
            // immediately (nothing to probe).
            let parts: Vec<&str> = healthcheck.split_whitespace().collect();
            if parts.is_empty() {
                return Ok(Some(()));
            }

            let exit_code = self.docker.exec_in_container(container_id, &parts).await?;
            if exit_code == 0 {
                info!("Service {} is healthy", name);
                return Ok(Some(()));
            }

            debug!(
                "Health check for {} failed (exit {}), retrying...",
                name, exit_code
            );
            Ok(None)
        })
        .await
    }

    /// Start a `run=` (shell command) service: a host process eph launches and
    /// manages.
    ///
    /// Auto-allocated ports (`port=auto`) are reserved here -- reusing the
    /// service's previous ports when still free so URLs stay stable across
    /// restarts -- and injected into the process environment so it binds the port
    /// eph chose. Because eph owns launching the process, it closes the
    /// unavoidable gap between reserving a port and the process binding it: it
    /// watches for an early exit whose captured log names a port conflict and
    /// re-launches on a fresh port, up to a few attempts. Fixed-port and
    /// port-less commands keep the previous behavior -- spawned once, with an
    /// early exit ignored -- so this is purely additive for them.
    ///
    /// The process inherits eph's resolved environment (the variables `eph env`
    /// emits, plus `EPH_*` metadata and its own resolved `env.X`), so a managed
    /// app can reach the workspace's other services without `eval "$(eph env)"`
    /// first.
    async fn start_shell_command(
        &mut self,
        name: &str,
        cmd: &str,
        service: &Service,
        eph: &EphFile,
    ) -> Result<(RunningService, Backend)> {
        info!("Starting shell command for {}: {}", name, cmd);

        // The ports this service had on a previous `up`, reused for auto ports
        // when still free so the assigned URL is stable across restarts. Read
        // from `auto_ports`, which survives `eph down` (unlike `services`).
        let prev_ports = self.state.auto_ports.get(name).cloned();

        // Snapshot the other running services once so the app's environment can
        // interpolate their connection details (e.g. ${postgres.port}). This
        // service's own freshly-assigned ports are layered on per attempt below.
        let others = self.status().await?;

        // Only auto-port services are re-launchable: a fixed-port or port-less
        // command that dies did not lose a port race, so retrying would just mask
        // a real failure and (for fixed ports) we keep the historical behavior of
        // not treating an early exit as a startup failure at all.
        let has_auto = has_auto_port(&service.ports);
        let max_attempts: u32 = if has_auto { 4 } else { 1 };

        for attempt in 1..=max_attempts {
            // Reuse the previous ports only on the first attempt; a retry exists
            // precisely because a port collided, so it allocates fresh ones.
            let reuse = if attempt == 1 {
                prev_ports.as_ref()
            } else {
                None
            };
            let ports = allocate_ports(&service.ports, reuse)?;

            // Build the environment with this service's assigned ports visible, so
            // it can read its own ${<name>.port} alongside other services'.
            let mut running = others.clone();
            running.insert(
                name.to_string(),
                RunningService {
                    name: name.to_string(),
                    ports: ports.clone(),
                },
            );
            let env = self.app_env(eph, &running, service);

            // Resolve the healthcheck's ${...} against the same running set, so a
            // readiness check can name the app's assigned port as ${<name>.port}
            // (it also receives the env below, so `$PORT` works too).
            let healthcheck = service
                .healthcheck
                .as_deref()
                .map(|hc| resolve_against(hc, &running));

            let (mut child, pid) = self.spawn_command(name, cmd, &env, false)?;
            let backend = process_backend(name, pid);
            info!(
                "Started {} with PID {} (attempt {}/{})",
                name, pid, attempt, max_attempts
            );

            // Record PID and ports now so `eph status` / `eph env` reflect the
            // service even while we wait for it to become ready.
            self.state.services.insert(
                name.to_string(),
                ServiceStateEntry {
                    backend: backend.clone(),
                    ports: ports.clone(),
                },
            );

            match self
                .await_command_ready(
                    name,
                    healthcheck.as_deref(),
                    service.ready_timeout_secs,
                    &env,
                    &mut child,
                    has_auto,
                )
                .await?
            {
                ReadyOutcome::Ready => {
                    // Remember the auto ports so the next `up` reuses them for a
                    // stable URL, even across `eph down`.
                    if has_auto {
                        self.state
                            .auto_ports
                            .insert(name.to_string(), ports.clone());
                    }
                    return Ok((
                        RunningService {
                            name: name.to_string(),
                            ports,
                        },
                        backend,
                    ));
                }
                ReadyOutcome::PortConflict if attempt < max_attempts => {
                    warn!(
                        "Service {} exited on a port conflict; re-launching on a fresh port \
                         (attempt {}/{})",
                        name,
                        attempt + 1,
                        max_attempts
                    );
                    // Drop the dead entry so a stale PID is not left behind; the
                    // next attempt records a fresh one.
                    self.state.services.remove(name);
                }
                ReadyOutcome::PortConflict => {
                    self.state.services.remove(name);
                    bail!(
                        "service '{}' kept exiting on a port conflict after {} attempts; \
                         see `eph logs {}`",
                        name,
                        max_attempts,
                        name
                    );
                }
                ReadyOutcome::Exited => {
                    self.state.services.remove(name);
                    bail!(
                        "service '{}' exited during startup; see `eph logs {}`",
                        name,
                        name
                    );
                }
            }
        }

        // Every loop iteration returns, bails, or (only on a retryable conflict)
        // continues, and the final attempt's conflict bails, so this is
        // unreachable; it satisfies the type checker without a panic.
        bail!("service '{}' could not be started", name)
    }

    /// Spawn a `run=` command with `env` overlaid on eph's environment.
    ///
    /// Returns the live child -- so the caller can watch for an early exit -- and
    /// its PID. The child is not killed on drop.
    ///
    /// `foreground` selects the stdio wiring. Background services (the `eph up`
    /// path) get a null stdin and capture stdout/stderr to a per-run file; the
    /// foreground service (`eph dev`) inherits eph's own stdin/stdout/stderr so it
    /// is interactive and its output streams straight through. The per-branch
    /// comments explain why the background path must use a file and why that
    /// reasoning does not bind `eph dev`.
    fn spawn_command(
        &self,
        name: &str,
        cmd: &str,
        env: &[(String, String)],
        foreground: bool,
    ) -> Result<(tokio::process::Child, NonZeroU32)> {
        let mut command = proc::shell_command(cmd);
        command
            .current_dir(&self.workspace.path)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

        if foreground {
            // `eph dev` hands the app eph's own stdin, stdout, and stderr so it is
            // fully interactive and its output streams straight to the terminal or
            // the preview server. The pipe-inheritance hang that forces the
            // background path below to a file cannot happen here: eph stays
            // attached, holding the child until teardown, rather than returning
            // while the service keeps eph's stdout write-end open.
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        } else {
            // Capture stdout/stderr to a per-run file rather than inheriting eph's,
            // for two reasons: it is what `eph logs` reads, and it avoids a
            // pipe-inheritance hang where a long-lived service holding eph's
            // stdout/stderr write-ends would block anything capturing eph's output
            // after `eph up` returns. The file is truncated per spawn so it
            // reflects the current run; captured output can contain secrets, so the
            // dir and file are owner-only (0700/0600) on Unix.
            let log_path = self.workspace.log_file_path(name)?;
            if let Some(parent) = log_path.parent() {
                create_private_dir(parent).with_context(|| {
                    format!("failed to create logs directory: {}", parent.display())
                })?;
            }
            let log_file = create_private_log_file(&log_path)
                .with_context(|| format!("failed to open log file: {}", log_path.display()))?;
            let log_file_err = log_file
                .try_clone()
                .with_context(|| format!("failed to open log file: {}", log_path.display()))?;
            command
                .stdin(Stdio::null())
                .stdout(Stdio::from(log_file))
                .stderr(Stdio::from(log_file_err));
        }
        // Head the shell in its own process group (Unix) so teardown can signal
        // the whole tree it forks, not just this wrapper PID. A compound `run=`
        // command (`a && b`, a pipeline, a backgrounded child) otherwise leaves
        // orphans behind on `eph down` / `eph clean`. No-op on Windows, where
        // teardown walks the descendant tree instead (see `proc`).
        proc::prepare_detached(&mut command);
        let child = command
            .spawn()
            .with_context(|| format!("failed to start command: {}", cmd))?;

        // A freshly spawned child always has a PID; `id()` only returns `None`
        // after it has been awaited to completion. Treat the impossible case as
        // an error rather than coercing it to a meaningless `0`.
        let pid = child
            .id()
            .and_then(NonZeroU32::new)
            .with_context(|| format!("spawned process for '{}' has no PID", name))?;
        Ok((child, pid))
    }

    /// Wait for a freshly-spawned `run=` process to become ready.
    ///
    /// With a healthcheck, polls it until it passes or the ready timeout elapses
    /// (the timeout is a hard failure, as before). Without one, gives the process
    /// a brief grace period. When `detect_exit` is set (an auto-port service that
    /// may be re-launched), an exit during startup is reported as
    /// [`ReadyOutcome::PortConflict`] or [`ReadyOutcome::Exited`] depending on
    /// whether its log names a port conflict; when it is clear (fixed-port
    /// services), an early exit is ignored, preserving the historical behavior.
    ///
    /// `healthcheck` is already `${...}`-resolved, and `env` is the exact
    /// environment the app was spawned with, so a readiness check can reference
    /// the app's assigned port the same way the app does
    /// (`curl -sf http://localhost:$PORT/health`, or the eph-resolved
    /// `${web.port}`). Without this, an auto-port healthcheck would never see the
    /// port and would always time out.
    async fn await_command_ready(
        &self,
        name: &str,
        healthcheck: Option<&str>,
        ready_timeout_secs: Option<u64>,
        env: &[(String, String)],
        child: &mut tokio::process::Child,
        detect_exit: bool,
    ) -> Result<ReadyOutcome> {
        let Some(healthcheck) = healthcheck else {
            // Give the process a moment to start, then (if watching) classify an
            // early exit.
            sleep(Duration::from_millis(500)).await;
            if detect_exit && matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(self.classify_exit(name).await);
            }
            return Ok(ReadyOutcome::Ready);
        };

        let timeout_dur = Duration::from_secs(ready_timeout_secs.unwrap_or(30));
        wait_until_ready(name, timeout_dur, Duration::from_secs(1), async || {
            // A watched process that has already exited is classified (port
            // conflict vs other failure) rather than probed further.
            if detect_exit && matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(Some(self.classify_exit(name).await));
            }

            let output = proc::shell_command(healthcheck)
                .current_dir(&self.workspace.path)
                // Run the check with the same resolved environment the app got,
                // so it can reach the app's (possibly auto-allocated) port.
                .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .output()
                .await?;

            if output.status.success() {
                info!("Service {} is healthy", name);
                return Ok(Some(ReadyOutcome::Ready));
            }

            debug!("Health check for {} failed, retrying...", name);
            Ok(None)
        })
        .await
    }

    /// Classify why a freshly-spawned `run=` process exited by scanning its
    /// captured log for a [port-conflict marker](PORT_CONFLICT_MARKERS). The log
    /// is small here (the process only just started), so reading it whole is
    /// cheap.
    async fn classify_exit(&self, name: &str) -> ReadyOutcome {
        if let Ok(path) = self.workspace.log_file_path(name)
            && let Ok(contents) = tokio::fs::read_to_string(&path).await
            && log_indicates_port_conflict(&contents)
        {
            return ReadyOutcome::PortConflict;
        }
        ReadyOutcome::Exited
    }

    /// The environment for a managed `run=` app eph launches.
    ///
    /// This is the connection environment `eph run` and lifecycle hooks see
    /// (resolved top-level `.eph` variables + `EPH_*` metadata), plus the
    /// service's own `env.X` values with their `${...}` interpolations resolved --
    /// so an app can be handed its eph-assigned port via `env.PORT=${<name>.port}`
    /// and reach other services via the usual variables. Later entries win, so the
    /// service's own `env.X` shadow any top-level variable of the same name.
    fn app_env(
        &self,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
        service: &Service,
    ) -> Vec<(String, String)> {
        let mut env = self.command_env(eph, running);
        for (k, v) in &service.env {
            env.push((k.clone(), resolve_against(v, running)));
        }
        env
    }

    /// Start a docker-compose service
    async fn start_compose(
        &mut self,
        name: &str,
        compose_path: &str,
        service: &Service,
        running: &HashMap<String, RunningService>,
    ) -> Result<(RunningService, Backend)> {
        let compose_file = self.workspace.path.join(compose_path);
        let project_name = format!("eph-{}-{}", self.workspace.short_id, name);

        info!(
            "Starting docker-compose service {} from {}",
            name,
            compose_file.display()
        );

        // The service's env.X values, with ${service.property} references
        // resolved, exported into `docker compose`'s process environment.
        // Compose files consume them through their own `${VAR}` substitution.
        // env.X on a compose service used to be dropped entirely (never even
        // read), so the documented syntax silently did nothing here.
        let compose_env: Vec<(String, String)> = service
            .env
            .iter()
            .map(|(k, v)| (k.clone(), resolve_against(v, running)))
            .collect();

        // Start compose
        let output = TokioCommand::new("docker")
            .args([
                "compose",
                "-f",
                &compose_file.to_string_lossy(),
                "-p",
                &project_name,
                "up",
                "-d",
            ])
            .envs(compose_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&self.workspace.path)
            .output()
            .await
            .context("failed to run docker compose")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker compose failed:\n{}", stderr);
        }

        // Get port mappings from compose
        let mut ports = HashMap::new();
        for port_mapping in &service.ports {
            let port_name = port_mapping
                .name
                .clone()
                .unwrap_or_else(|| "default".to_string());

            // Try to get the actual mapped port from docker compose. Same
            // environment as the `up` call, so a compose file whose structure
            // depends on those variables parses identically.
            let port_output = TokioCommand::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file.to_string_lossy(),
                    "-p",
                    &project_name,
                    "port",
                    &port_name,
                    &port_mapping.container_port.to_string(),
                ])
                .envs(compose_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .output()
                .await;

            if let Ok(output) = port_output
                && output.status.success()
            {
                let port_str = String::from_utf8_lossy(&output.stdout);
                // Output is like "0.0.0.0:12345" or ":::12345"
                if let Some(port) = port_str.trim().rsplit(':').next()
                    && let Ok(p) = port.parse::<u16>()
                {
                    ports.insert(port_name.clone(), p);
                    continue;
                }
            }

            // `docker compose port` failed or did not parse. Fall back to the
            // declared container port, but say so: compose normally maps to a
            // random host port, so this fallback value is usually wrong and a
            // connection string interpolating it will not reach the service.
            warn!(
                "could not resolve the host port for '{}' port '{}' via `docker \
                 compose port`; using the declared container port {} (this is \
                 probably not the mapped host port)",
                name, port_name, port_mapping.container_port
            );
            ports.insert(port_name, port_mapping.container_port);
        }

        // Wait for health check if specified
        if let Some(ref healthcheck) = service.healthcheck {
            let timeout_dur = Duration::from_secs(service.ready_timeout_secs.unwrap_or(60));
            wait_until_ready(name, timeout_dur, Duration::from_secs(2), async || {
                let output = proc::shell_command(healthcheck)
                    .current_dir(&self.workspace.path)
                    .output()
                    .await?;

                if output.status.success() {
                    info!("Service {} is healthy", name);
                    return Ok(Some(()));
                }

                debug!("Health check for {} failed, retrying...", name);
                Ok(None)
            })
            .await?;
        }

        Ok((
            RunningService {
                name: name.to_string(),
                ports,
            },
            Backend::Compose {
                project: project_name,
            },
        ))
    }

    /// Stop all services (declared ones plus any recorded in state under a
    /// name no longer in the `.eph` file) and persist the result.
    ///
    /// When `remove` is true, also remove containers (and compose resources) so
    /// they do not accumulate.
    ///
    /// # Errors
    ///
    /// Returns an error if stopping a service fails (see
    /// [`stop_service`](Self::stop_service)) or if state cannot be saved.
    pub async fn stop_all(&mut self, eph: &EphFile, remove: bool, skip_hooks: bool) -> Result<()> {
        let mut lock = WorkspaceLock::open(&self.workspace)?;
        let _guard = lock.acquire()?;
        // Re-read state under the lock: it was loaded when this manager was
        // constructed, and another command may have finished in between.
        self.state = ServiceState::load(&self.workspace).await?;

        // Snapshot the running services once, before any teardown, so every
        // pre-stop and post-stop hook sees the full environment as it was when
        // `down` began.
        let running = self.status().await?;
        // Tear down in the reverse of the actual start order (see `start_order`,
        // which defers run= apps to the end), so a dependent is stopped before
        // the dependency it relies on and its pre-stop hook still sees that
        // dependency up.
        for name in start_order(eph).into_iter().rev() {
            let service = &eph.services[name];
            self.stop_service(name, service, remove, eph, &running, skip_hooks)
                .await?;
        }
        // State may also record services whose sections were renamed or
        // deleted since they started; stop those too, from their recorded
        // backends. Each stop_service/stop_orphan call removed its own state
        // entry, so there is no wholesale clear (which would silently forget
        // anything that failed to stop above).
        for name in self.orphaned_state_entries(eph) {
            self.stop_orphan(&name, remove).await?;
        }
        self.state.save(&self.workspace).await?;
        Ok(())
    }

    /// Stop a specific subset of services, in the reverse of the start order, so
    /// a dependent is always stopped before the dependency it relies on (its
    /// `pre-stop` hook still sees that dependency up).
    ///
    /// Used by a filtered `eph down` (explicit service names or `--role`) and by
    /// `eph dev` to tear down only the services it brought up while leaving any
    /// that were already running (a session hook's prewarmed dependencies) in
    /// place. Names not in `targets` are skipped; names in `targets` that are not
    /// running are a harmless no-op.
    pub async fn stop_selected(
        &mut self,
        eph: &EphFile,
        targets: &[String],
        remove: bool,
        skip_hooks: bool,
    ) -> Result<()> {
        let mut lock = WorkspaceLock::open(&self.workspace)?;
        let _guard = lock.acquire()?;
        // Re-read state under the lock (see stop_all).
        self.state = ServiceState::load(&self.workspace).await?;

        let wanted: HashSet<&str> = targets.iter().map(String::as_str).collect();
        // Snapshot running services once so every pre-stop/post-stop hook sees the
        // full environment as it was before teardown began.
        let running = self.status().await?;
        for name in start_order(eph).into_iter().rev() {
            if !wanted.contains(name.as_str()) {
                continue;
            }
            let service = &eph.services[name];
            self.stop_service(name, service, remove, eph, &running, skip_hooks)
                .await?;
        }
        self.state.save(&self.workspace).await?;
        Ok(())
    }

    /// Stop a single service, running its `pre-stop` hooks before it stops and
    /// its `post-stop` hooks after.
    ///
    /// When `remove` is true, also remove the underlying container after
    /// stopping it (compose uses `down`, which already removes containers;
    /// killing a `run` process already removes it).
    ///
    /// When `skip_hooks` is true, neither `pre-stop` nor `post-stop` hooks run --
    /// the escape hatch for a broken hook that would otherwise wedge teardown.
    ///
    /// Returns `true` when something was actually stopped or removed, `false`
    /// when the service turned out not to be up (so callers can report honest
    /// counts instead of counting declared services).
    ///
    /// # Errors
    ///
    /// Returns an error if a `pre-stop` hook fails (the service is left running
    /// so the hook can be retried), if a Docker stop or remove call fails for an
    /// `image`/`dockerfile` service, if `docker compose down` fails for a
    /// compose service that was up, or if a `post-stop` hook fails (the service
    /// is already stopped by then).
    pub async fn stop_service(
        &mut self,
        name: &str,
        service: &Service,
        remove: bool,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
        skip_hooks: bool,
    ) -> Result<bool> {
        // Run pre-stop hooks with the resolved environment, the same way
        // post-start hooks receive it. A failing hook aborts the teardown
        // (before the service is stopped), mirroring how a failing post-start
        // aborts `eph up`: if the pre-stop backup/drain did not succeed, you
        // almost certainly do not want the data to go away underneath it.
        //
        // Only run them for a service that is actually running: `stop_all`
        // iterates every service in the `.eph` file, so without this gate a
        // never-started or already-stopped service's pre-stop hook would run
        // (and, being fatal, could break `eph down` for an unrelated service).
        if !skip_hooks && running.contains_key(name) && !service.pre_stop.is_empty() {
            info!("Running pre-stop hooks for {}", name);
            let env = self.hook_env(eph, running, service);
            for cmd in &service.pre_stop {
                self.run_hook(cmd, &env)
                    .await
                    .with_context(|| format!("pre-stop hook failed for service '{}'", name))?;
            }
        }

        let stopped_something = match &service.source {
            ServiceSource::Command(_) => {
                // Kill the process, reading its PID from the recorded backend.
                if let Some(entry) = self.state.services.get(name)
                    && let Backend::Process { pid, .. } = &entry.backend
                {
                    let pid = *pid;
                    if entry.backend.process_is_alive() {
                        info!("Stopping process {} (PID {})", name, pid);
                        // Ask it to terminate gracefully (SIGTERM on Unix,
                        // TerminateProcess on Windows), then force-kill if it
                        // ignored the request. Both are best-effort: a process
                        // that exits in between is a no-op.
                        proc::terminate(pid);
                        sleep(Duration::from_secs(2)).await;
                        proc::force_kill(pid);
                        true
                    } else {
                        // Either already exited, or the PID now belongs to an
                        // unrelated process (identity mismatch). Never signal
                        // it; just drop the stale entry below.
                        debug!(
                            "process for {} (PID {}) is already gone or reused; \
                             nothing to stop",
                            name, pid
                        );
                        false
                    }
                } else {
                    false
                }
            }
            ServiceSource::Compose(path) => {
                // Only invoke `docker compose down` when eph has any record of
                // the project being up; a never-started service is a no-op,
                // matching the container path below. When it does run, a
                // failure is a real error (a broken compose file, a missing
                // compose plugin) and propagates: this used to be swallowed
                // wholesale, so `eph down` reported success while the compose
                // containers kept running.
                if running.contains_key(name) || self.state.services.contains_key(name) {
                    let compose_file = self.workspace.path.join(path);
                    let project_name = format!("eph-{}-{}", self.workspace.short_id, name);
                    info!("Stopping docker-compose service {}", name);
                    // Same env the compose file was brought up with, so its
                    // `${VAR}` substitutions parse the same way on the way down.
                    let compose_env: Vec<(String, String)> = service
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), resolve_against(v, running)))
                        .collect();
                    let output = TokioCommand::new("docker")
                        .args([
                            "compose",
                            "-f",
                            &compose_file.to_string_lossy(),
                            "-p",
                            &project_name,
                            "down",
                        ])
                        .envs(compose_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                        .output()
                        .await
                        .context("failed to run docker compose down")?;
                    if !output.status.success() {
                        bail!(
                            "`docker compose down` failed for service '{}':\n{}",
                            name,
                            String::from_utf8_lossy(&output.stderr).trim_end()
                        );
                    }
                    true
                } else {
                    false
                }
            }
            _ => {
                let container_name = self.workspace.container_name(name);
                let stopped = self.docker.stop_container(&container_name).await?;
                let removed = if remove {
                    self.docker.remove_container(&container_name).await?
                } else {
                    false
                };
                stopped || removed
            }
        };

        self.state.services.remove(name);

        // Run post-stop hooks after the service is fully stopped -- the place for
        // cleanup eph cannot do itself (deleting a scratch directory, tearing
        // down an external resource the service registered). They see the same
        // pre-teardown snapshot as pre-stop, so a hook can still reference the
        // now-stopped service's port for cleanup. Gated on the snapshot too, so a
        // never-started service in the `.eph` file does not trigger one.
        //
        // A failing post-stop aborts the remaining teardown, like pre-stop. The
        // difference: this service is already stopped, so re-running `eph down`
        // will not re-run its post-stop (the fresh snapshot no longer lists it).
        // Fix the cleanup and run it by hand, or use `--skip-hooks` to bypass.
        if !skip_hooks && running.contains_key(name) && !service.post_stop.is_empty() {
            info!("Running post-stop hooks for {}", name);
            let env = self.hook_env(eph, running, service);
            for cmd in &service.post_stop {
                self.run_hook(cmd, &env)
                    .await
                    .with_context(|| format!("post-stop hook failed for service '{}'", name))?;
            }
        }

        Ok(stopped_something)
    }

    /// Stop a service that exists only in recorded state, not in the current
    /// `.eph` file (its section was renamed or deleted since it started).
    /// Teardown works entirely from the recorded [`Backend`]; there is no
    /// [`Service`] definition anymore, so no hooks run and no volumes are
    /// known. These entries used to be invisible to `down` and `clean` (both
    /// iterated only declared services and then cleared state wholesale), so
    /// renaming a running service leaked its container permanently.
    ///
    /// Returns `true` when something was actually stopped or removed.
    async fn stop_orphan(&mut self, name: &str, remove: bool) -> Result<bool> {
        let Some(entry) = self.state.services.get(name) else {
            return Ok(false);
        };
        info!(
            "Stopping '{}', which is no longer defined in .eph but is recorded \
             as started by eph",
            name
        );
        let stopped = match entry.backend.clone() {
            Backend::Process { pid, .. } => {
                if entry.backend.process_is_alive() {
                    proc::terminate(pid);
                    sleep(Duration::from_secs(2)).await;
                    proc::force_kill(pid);
                    true
                } else {
                    false
                }
            }
            Backend::Compose { project } => {
                // No compose file path is recorded, but `docker compose down`
                // resolves the project's containers from their labels, so `-p`
                // alone is enough to tear it down.
                let output = TokioCommand::new("docker")
                    .args(["compose", "-p", &project, "down"])
                    .output()
                    .await
                    .context("failed to run docker compose down")?;
                if !output.status.success() {
                    bail!(
                        "`docker compose down` failed for removed service '{}':\n{}",
                        name,
                        String::from_utf8_lossy(&output.stderr).trim_end()
                    );
                }
                true
            }
            Backend::Container { .. } => {
                let container_name = self.workspace.container_name(name);
                let stopped = self.docker.stop_container(&container_name).await?;
                let removed = if remove {
                    self.docker.remove_container(&container_name).await?
                } else {
                    false
                };
                stopped || removed
            }
        };
        self.state.services.remove(name);
        Ok(stopped)
    }

    /// Names present in recorded state but absent from the `.eph` file, in a
    /// stable order.
    fn orphaned_state_entries(&self, eph: &EphFile) -> Vec<String> {
        let mut names: Vec<String> = self
            .state
            .services
            .keys()
            .filter(|name| !eph.services.contains_key(*name))
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Fully reset the workspace: stop and remove every service's container
    /// (or compose resources / process), remove every per-workspace named
    /// volume, clear in-memory state, and delete the persisted state file.
    ///
    /// Returns a [`CleanSummary`] describing what was removed.
    ///
    /// When `skip_hooks` is true, `pre-stop` and `post-stop` hooks are not run,
    /// so a broken hook cannot block the reset.
    ///
    /// # Errors
    ///
    /// Returns an error if a `pre-stop` or `post-stop` hook fails (unless
    /// `skip_hooks`), if stopping a service, removing a named volume, or deleting
    /// the state directory fails.
    pub async fn clean(&mut self, eph: &EphFile, skip_hooks: bool) -> Result<CleanSummary> {
        let mut lock = WorkspaceLock::open(&self.workspace)?;
        let _guard = lock.acquire()?;
        // Re-read state under the lock (see stop_all).
        self.state = ServiceState::load(&self.workspace).await?;

        let mut summary = CleanSummary::default();

        // Snapshot running services once so pre-stop and post-stop hooks see the
        // full environment as it was before teardown began.
        let running = self.status().await?;

        // Reverse of the actual start order, matching `stop_all`: tear a
        // dependent down before the dependency it relies on. The summary counts
        // what was actually stopped or removed, not what the file declares: a
        // `clean` of a workspace that never ran reports zeros.
        for name in start_order(eph).into_iter().rev() {
            let service = &eph.services[name];
            // Stop and remove the underlying resource for this service.
            if self
                .stop_service(name, service, true, eph, &running, skip_hooks)
                .await?
            {
                summary.services_removed += 1;
            }

            // Remove per-workspace named volumes. A volume entry is a named
            // volume (not a bind mount) when its source is not a host path (see
            // is_host_path_source, which also recognizes Windows drive-letter and
            // UNC sources). The source split is Windows-aware so a drive colon is
            // never mistaken for the source/destination separator. The real
            // Docker volume name is derived via Workspace::volume_name(service, base).
            for volume in &service.volumes {
                let base = split_volume_source(volume)
                    .map(|(source, _)| source)
                    .unwrap_or(volume);
                if is_host_path_source(base) {
                    continue; // bind mount, not a managed named volume
                }
                let volume_name = self.workspace.volume_name(name, base);
                if self.docker.remove_volume(&volume_name).await? {
                    summary.volumes_removed += 1;
                }
            }
        }

        // Stop anything recorded in state under a name no longer in the file
        // (a renamed or deleted section).
        for name in self.orphaned_state_entries(eph) {
            if self.stop_orphan(&name, true).await? {
                summary.services_removed += 1;
            }
        }

        // Finally, sweep Docker itself for leftovers carrying this workspace's
        // name prefix: containers and volumes from a service that was renamed
        // before state recorded it, or from a crash before state was written.
        // `clean` promises a full reset, so it cannot trust state (or the
        // current .eph file) to know everything that exists.
        let prefix = format!("eph-{}-", self.workspace.short_id);
        for container in self.docker.containers_with_prefix(&prefix).await? {
            info!("Removing leftover container {}", container);
            if self.docker.remove_container(&container).await? {
                summary.services_removed += 1;
            }
        }
        for volume in self.docker.volumes_with_prefix(&prefix).await? {
            info!("Removing leftover volume {}", volume);
            if self.docker.remove_volume(&volume).await? {
                summary.volumes_removed += 1;
            }
        }

        // Clear in-memory state. `clean` is a full reset, so unlike `down` it
        // also drops the remembered auto-port assignments, letting the next `up`
        // pick fresh ports.
        self.state.services.clear();
        self.state.auto_ports.clear();

        // Remove the persisted state file (and its directory).
        let state_dir = self.workspace.state_dir()?;
        if state_dir.exists() {
            tokio::fs::remove_dir_all(&state_dir)
                .await
                .with_context(|| {
                    format!("failed to remove state directory: {}", state_dir.display())
                })?;
            summary.state_removed = true;
        }

        Ok(summary)
    }

    /// Save the current in-memory service state to disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the state directory cannot be created or the state
    /// file cannot be serialized or written.
    pub async fn save_state(&self) -> Result<()> {
        self.state.save(&self.workspace).await
    }

    /// Start the foreground `run=` service for `eph dev`, inheriting eph's stdio.
    ///
    /// Unlike the backing `run=` path
    /// ([`start_shell_command`](Self::start_shell_command)), this hands the app
    /// eph's own stdin, stdout, and stderr, so it is fully interactive and its
    /// output streams straight to the terminal or the preview server rather than
    /// being captured to a log file. It returns the live child so the caller can
    /// wait on it (and reap it) to notice when the app exits, rather than polling
    /// a PID that a zombie would keep reading as alive.
    ///
    /// There is no port-conflict re-launch here (`detect_exit` is off): a
    /// foreground app owns its streams, so a bind failure should surface on the
    /// inherited stderr rather than silently retry on a different port (which
    /// would leave the app unreachable at the port `eph dev` expects to gate).
    /// Call it after the backing services are up so the app's environment can
    /// already interpolate their ports.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a `run=` service, the process cannot be
    /// spawned, or it fails its healthcheck within the ready timeout.
    pub async fn start_foreground(
        &mut self,
        eph: &EphFile,
        name: &str,
    ) -> Result<(RunningService, tokio::process::Child)> {
        let service = eph
            .services
            .get(name)
            .with_context(|| format!("unknown service: {name}"))?;
        let ServiceSource::Command(cmd) = &service.source else {
            bail!("service '{name}' is not a run= service, so `eph dev` cannot foreground it");
        };

        // Reuse the remembered auto port so the app's URL stays stable across
        // restarts. Snapshot the other running services so the app's env can
        // interpolate their ports.
        let prev_ports = self.state.auto_ports.get(name).cloned();
        let others = self.status().await?;
        let ports = allocate_ports(&service.ports, prev_ports.as_ref())?;

        let mut running = others;
        running.insert(
            name.to_string(),
            RunningService {
                name: name.to_string(),
                ports: ports.clone(),
            },
        );
        let env = self.app_env(eph, &running, service);
        let healthcheck = service
            .healthcheck
            .as_deref()
            .map(|hc| resolve_against(hc, &running));

        let (mut child, pid) = self.spawn_command(name, cmd, &env, true)?;
        let backend = process_backend(name, pid);
        info!("Started {} (foreground) with PID {}", name, pid);

        // Record before waiting so `eph status` and any teardown see the process
        // even while it is still coming up.
        self.state.services.insert(
            name.to_string(),
            ServiceStateEntry {
                backend,
                ports: ports.clone(),
            },
        );
        if has_auto_port(&service.ports) {
            self.state
                .auto_ports
                .insert(name.to_string(), ports.clone());
        }

        // Wait for readiness with no early-exit classification (`detect_exit =
        // false`): there is no captured log to scan, and the foreground app does
        // not get the port-conflict retry. A failed start drops the recorded PID
        // so no stale entry is left behind.
        if let Err(e) = self
            .await_command_ready(
                name,
                healthcheck.as_deref(),
                service.ready_timeout_secs,
                &env,
                &mut child,
                false,
            )
            .await
        {
            self.state.services.remove(name);
            return Err(e);
        }
        self.state.save(&self.workspace).await?;

        Ok((
            RunningService {
                name: name.to_string(),
                ports,
            },
            child,
        ))
    }

    /// Return the services that are currently running.
    ///
    /// Reconciles persisted state against the live Docker daemon (and tracked
    /// PIDs for `run` services), so only services that are actually up are
    /// included.
    ///
    /// # Errors
    ///
    /// Returns an error if querying the Docker daemon for a container fails.
    pub async fn status(&self) -> Result<HashMap<String, RunningService>> {
        let mut result = HashMap::new();

        for (name, entry) in &self.state.services {
            // Liveness is checked per backend: compose by project label,
            // run= by probing the PID, and Docker containers by name. A
            // service that is no longer running is simply omitted.
            let live = match &entry.backend {
                // Compose services are not named `eph-<id>-<name>`, so they are
                // checked by their project's label rather than by container
                // name. Without this they would never appear in `status` and
                // their ports could not be interpolated into `eph env`.
                Backend::Compose { project } => {
                    self.docker.compose_project_running(project).await?
                }
                // run= services are tracked by PID; probe it the same way
                // `eph up`'s dedup check does, identity included, so a PID
                // reused by an unrelated process does not read as "running".
                Backend::Process { .. } => entry.backend.process_is_alive(),
                Backend::Container { .. } => {
                    let container_name = self.workspace.container_name(name);
                    self.docker
                        .get_container(&container_name)
                        .await?
                        .is_some_and(|info| info.is_running)
                }
            };

            if live {
                // Use the saved state's ports (which have proper names) rather
                // than re-deriving them from the backend.
                result.insert(
                    name.clone(),
                    RunningService {
                        name: name.clone(),
                        ports: entry.ports.clone(),
                    },
                );
            }
        }

        Ok(result)
    }

    /// Stream or print a service's logs to stdout.
    ///
    /// The log source depends on the service's backend, so a single command
    /// works across all of them:
    ///
    /// - `run=` services are spawned by eph with their output captured to
    ///   `<state_dir>/logs/<service>.log`; that file is read here.
    /// - `image=` / `dockerfile=` services proxy `docker logs <container>`.
    /// - `compose=` services proxy `docker compose ... logs`.
    ///
    /// Logs are shown regardless of whether the service is currently running, so
    /// a `run=` service that died on startup still leaves an inspectable trace.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a defined service, if a log file cannot
    /// be read, or if a proxied `docker` invocation fails.
    pub async fn logs(&self, eph: &EphFile, name: &str, opts: &LogOptions) -> Result<()> {
        let service = eph
            .services
            .get(name)
            .with_context(|| format!("unknown service: {}", name))?;

        match &service.source {
            ServiceSource::Command(_) => self.logs_from_file(name, opts).await,
            ServiceSource::Compose(path) => self.logs_from_compose(name, path, opts).await,
            ServiceSource::Image(_) | ServiceSource::Dockerfile(_) => {
                let container_name = self.workspace.container_name(name);
                self.logs_from_container(&container_name, opts).await
            }
        }
    }

    /// Stream several services' logs as a single interleaved feed, invoking
    /// `on_line` once per complete line with `(service_name, line)`.
    ///
    /// Each service is read concurrently in its own task and lines are merged
    /// through a channel, so output is interleaved in arrival order the way
    /// `docker compose logs` is -- but `on_line` is only ever called with a
    /// whole line, so two services never interleave mid-line. At most one line
    /// per service is buffered in flight; the full output is never collected.
    ///
    /// Sources match [`logs`](Self::logs): `run=` services read their captured
    /// log file, while Docker- and compose-backed services read the piped output
    /// of `docker logs` / `docker compose logs`. Compose's own per-container
    /// prefix is stripped (`--no-log-prefix`) since the caller adds eph's
    /// `[service]` tag.
    ///
    /// When `opts.follow` is set this runs until interrupted (Ctrl-C); otherwise
    /// it returns once every source is exhausted. It also returns early if
    /// `on_line` reports a write error (e.g. a closed pipe from `eph logs | head`).
    ///
    /// # Errors
    ///
    /// Returns an error if any `name` is not a defined service. When the stream
    /// drains on its own (i.e. not interrupted by Ctrl-C or a closed pipe), it
    /// also returns the first per-service failure -- a `docker logs` /
    /// `docker compose logs` that could not be spawned or exited non-zero -- so
    /// the all-services path fails just as a single `eph logs <service>` does.
    /// All sources are still drained first, so the logs that did succeed are
    /// emitted before the error is reported.
    pub async fn stream_logs(
        &self,
        eph: &EphFile,
        names: &[String],
        opts: &LogOptions,
        mut on_line: impl FnMut(&str, &str) -> std::io::Result<()>,
    ) -> Result<()> {
        // Resolve each service to a fully-owned source up front, so the per-source
        // tasks below can move their work in without borrowing `self` or `eph`.
        let mut sources: Vec<(String, LogSource)> = Vec::with_capacity(names.len());
        for name in names {
            let service = eph
                .services
                .get(name)
                .with_context(|| format!("unknown service: {}", name))?;
            sources.push((name.clone(), self.log_source(name, service, opts)?));
        }

        // A small bounded channel applies natural backpressure: a noisy service
        // cannot run arbitrarily far ahead of the (single) consumer that writes.
        let (tx, mut rx) = mpsc::channel::<LogLine>(256);
        let mut tasks = JoinSet::new();
        for (name, source) in sources {
            let tx = tx.clone();
            let follow = opts.follow;
            let tail = opts.tail;
            tasks.spawn(async move {
                match source {
                    LogSource::File(path) => stream_file_lines(name, path, follow, tail, tx).await,
                    LogSource::Docker(args) => stream_docker_lines(name, args, tx).await,
                }
            });
        }
        // Drop our own sender so the channel closes once every task is done,
        // which is how the non-follow consumer loop below terminates.
        drop(tx);

        // Track *why* the loop ends: only a natural drain inspects task results.
        // An interrupt (Ctrl-C) or a closed reader pipe is an expected, success
        // exit and must not surface the spurious "killed" status of the docker
        // children we are about to abort.
        let mut aborted = false;
        loop {
            tokio::select! {
                // Only arm Ctrl-C while following; without --follow the consumer
                // ends naturally when the channel closes, and a stray Ctrl-C
                // should terminate the process the usual way.
                _ = tokio::signal::ctrl_c(), if opts.follow => {
                    aborted = true;
                    break;
                }
                recv = rx.recv() => match recv {
                    Some(LogLine { service, line }) => {
                        // A write error means the reader hung up (closed pipe);
                        // stop quietly rather than erroring.
                        if on_line(&service, &line).is_err() {
                            aborted = true;
                            break;
                        }
                    }
                    None => break,
                },
            }
        }

        if aborted {
            // Abort any still-running tasks. The docker children are spawned with
            // kill_on_drop, so aborting their task reaps the process too. Their
            // results are intentionally discarded -- the user asked to stop.
            tasks.shutdown().await;
            return Ok(());
        }

        // Drained naturally: every task has finished and dropped its sender.
        // Join them and surface the first failure (e.g. `docker logs` against a
        // removed container, or docker missing entirely) so the all-services
        // path exits non-zero like the single-service path does.
        let mut first_err: Option<anyhow::Error> = None;
        while let Some(joined) = tasks.join_next().await {
            let task_result = match joined {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("log reader task failed: {join_err}")),
            };
            if let Err(err) = task_result
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Resolve a service to the owned [`LogSource`] used by [`stream_logs`].
    fn log_source(&self, name: &str, service: &Service, opts: &LogOptions) -> Result<LogSource> {
        let source = match &service.source {
            ServiceSource::Command(_) => LogSource::File(self.workspace.log_file_path(name)?),
            ServiceSource::Image(_) | ServiceSource::Dockerfile(_) => {
                let mut args = vec!["logs".to_string()];
                if let Some(n) = opts.tail {
                    args.push("--tail".to_string());
                    args.push(n.to_string());
                }
                if opts.follow {
                    args.push("--follow".to_string());
                }
                args.push(self.workspace.container_name(name));
                LogSource::Docker(args)
            }
            ServiceSource::Compose(path) => {
                let compose_file = self.workspace.path.join(path);
                let project_name = format!("eph-{}-{}", self.workspace.short_id, name);
                let mut args = vec![
                    "compose".to_string(),
                    "-f".to_string(),
                    compose_file.to_string_lossy().into_owned(),
                    "-p".to_string(),
                    project_name,
                    "logs".to_string(),
                    "--no-color".to_string(),
                    "--no-log-prefix".to_string(),
                ];
                if let Some(n) = opts.tail {
                    args.push("--tail".to_string());
                    args.push(n.to_string());
                }
                if opts.follow {
                    args.push("--follow".to_string());
                }
                LogSource::Docker(args)
            }
        };
        Ok(source)
    }

    /// Read (and optionally follow) a `run=` service's captured log file.
    ///
    /// A missing file is not an error: it just means the service has not been
    /// started yet, so a hint is printed to stderr and the call returns `Ok`.
    async fn logs_from_file(&self, name: &str, opts: &LogOptions) -> Result<()> {
        let path = self.workspace.log_file_path(name)?;
        if !path.exists() {
            eprintln!(
                "eph: no logs for '{}' yet (run= output is captured to {} once started)",
                name,
                path.display()
            );
            return Ok(());
        }

        let stdout = std::io::stdout();
        let mut out = stdout.lock();

        // Dump the existing contents (the last N lines when --tail is set, else
        // the whole file), then remember where the file ends so --follow prints
        // only what is appended afterwards. Both the seek-to-tail and the dump
        // are bounded: tail_start_offset scans backward without loading the file,
        // and the dump streams raw bytes in chunks rather than buffering it all.
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open log file: {}", path.display()))?;
        let len = file
            .seek(SeekFrom::End(0))
            .with_context(|| format!("failed to read log file: {}", path.display()))?;
        let start = match opts.tail {
            Some(n) => tail_start_offset(&mut file, len, n)
                .with_context(|| format!("failed to read log file: {}", path.display()))?,
            None => 0,
        };
        file.seek(SeekFrom::Start(start))
            .context("failed to seek log file")?;

        let mut buf = [0u8; 8192];
        loop {
            let read = file.read(&mut buf).context("failed to read log file")?;
            if read == 0 {
                break;
            }
            out.write_all(&buf[..read])
                .context("failed to write logs to stdout")?;
        }
        out.flush().ok();

        if !opts.follow {
            return Ok(());
        }

        let mut offset = file
            .stream_position()
            .with_context(|| format!("failed to read log file: {}", path.display()))?;

        loop {
            // Wait a beat between polls, but break promptly on Ctrl-C so follow
            // is interruptible like `tail -f` / `docker logs -f`.
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                () = sleep(Duration::from_millis(200)) => {}
            }

            let len = match std::fs::metadata(&path) {
                Ok(meta) => meta.len(),
                // The file can briefly vanish if the workspace is cleaned out
                // from under a follow; treat that as nothing-new and keep polling.
                Err(_) => continue,
            };

            // A shorter file means it was truncated or rotated (e.g. the service
            // was restarted): reset to the new beginning rather than seeking past
            // the end.
            if len < offset {
                offset = 0;
            }
            if len > offset {
                let mut file = std::fs::File::open(&path)
                    .with_context(|| format!("failed to open log file: {}", path.display()))?;
                file.seek(SeekFrom::Start(offset))
                    .context("failed to seek log file")?;
                // Stream the appended delta in chunks rather than buffering it
                // whole: a burst of output between polls stays bounded to one
                // chunk of memory.
                let mut buf = [0u8; 8192];
                loop {
                    let read = file.read(&mut buf).context("failed to read log file")?;
                    if read == 0 {
                        break;
                    }
                    offset += read as u64;
                    out.write_all(&buf[..read])
                        .context("failed to write logs to stdout")?;
                }
                out.flush().ok();
            }
        }

        Ok(())
    }

    /// Proxy `docker logs` for an `image=` / `dockerfile=` service.
    async fn logs_from_container(&self, container_name: &str, opts: &LogOptions) -> Result<()> {
        let mut args = vec!["logs".to_string()];
        if let Some(n) = opts.tail {
            args.push("--tail".to_string());
            args.push(n.to_string());
        }
        if opts.follow {
            args.push("--follow".to_string());
        }
        args.push(container_name.to_string());

        // Inherit eph's stdio so `docker logs` writes straight to the terminal
        // and handles its own Ctrl-C while following.
        let status = TokioCommand::new("docker")
            .args(&args)
            .status()
            .await
            .context("failed to run `docker logs` (is docker on PATH?)")?;
        if !status.success() {
            bail!("`docker logs {}` failed", container_name);
        }
        Ok(())
    }

    /// Proxy `docker compose ... logs` for a `compose=` service.
    async fn logs_from_compose(
        &self,
        name: &str,
        compose_path: &str,
        opts: &LogOptions,
    ) -> Result<()> {
        let compose_file = self.workspace.path.join(compose_path);
        let project_name = format!("eph-{}-{}", self.workspace.short_id, name);

        let mut args = vec![
            "compose".to_string(),
            "-f".to_string(),
            compose_file.to_string_lossy().into_owned(),
            "-p".to_string(),
            project_name,
            "logs".to_string(),
        ];
        if let Some(n) = opts.tail {
            args.push("--tail".to_string());
            args.push(n.to_string());
        }
        if opts.follow {
            args.push("--follow".to_string());
        }

        let status = TokioCommand::new("docker")
            .args(&args)
            .status()
            .await
            .context("failed to run `docker compose logs` (is docker on PATH?)")?;
        if !status.success() {
            bail!("`docker compose logs` failed for {}", name);
        }
        Ok(())
    }

    /// Build the partially resolved environment used while lifecycle work is in
    /// progress. Execution boundaries must use [`Self::command_env_strict`] or
    /// resolve their final values with [`resolve_against_strict`].
    #[must_use]
    pub fn command_env(
        &self,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
    ) -> Vec<(String, String)> {
        let mut env = resolve_env_vars(eph, running);
        env.extend(eph_metadata_env(&self.workspace, running));
        env
    }

    /// Build the complete environment for `eph run`, rejecting unavailable
    /// top-level service references before a child process is created.
    pub fn command_env_strict(
        &self,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
    ) -> std::result::Result<Vec<(String, String)>, UnresolvedEnvironment> {
        let mut env = resolve_env_vars_strict(eph, running)?;
        env.extend(eph_metadata_env(&self.workspace, running));
        Ok(env)
    }

    /// The environment overlaid on a lifecycle hook (`pre-start`, `post-start`,
    /// `pre-stop`, `post-stop`).
    ///
    /// This is [`command_env`](Self::command_env) plus the owning service's own
    /// `env.X` values, which take precedence. A `post-start` hook for a database
    /// therefore sees both the resolved `DATABASE_URL` and the container-side
    /// `POSTGRES_USER` / `POSTGRES_DB` it was created with.
    fn hook_env(
        &self,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
        service: &Service,
    ) -> Vec<(String, String)> {
        let mut env = self.command_env(eph, running);
        // Resolve ${service.property} in the service's own env values, exactly
        // as the values the service itself received were resolved. A hook used
        // to get them raw, so `env.PORT=${web.port}` read as the literal
        // placeholder inside the very service's own post-start hook.
        env.extend(
            service
                .env
                .iter()
                .map(|(k, v)| (k.clone(), resolve_against(v, running))),
        );
        env
    }

    /// Run a hook command in the workspace directory with `env` overlaid on
    /// eph's own environment.
    ///
    /// The child inherits eph's process environment; the `env` pairs are set on
    /// top of it, so later entries (the owning service's `env.X`) win over the
    /// resolved top-level variables they may shadow.
    async fn run_hook(&self, cmd: &str, env: &[(String, String)]) -> Result<()> {
        let output = proc::shell_command(cmd)
            .current_dir(&self.workspace.path)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .await
            .with_context(|| format!("failed to execute hook: {}", cmd))?;

        if !output.status.success() {
            // Surface both streams: plenty of tools (migrators especially)
            // print the useful diagnostic to stdout, and reporting only stderr
            // used to hide it.
            let mut detail = String::new();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stdout.trim().is_empty() {
                detail.push_str("\nstdout:\n");
                detail.push_str(stdout.trim_end());
            }
            if !stderr.trim().is_empty() {
                detail.push_str("\nstderr:\n");
                detail.push_str(stderr.trim_end());
            }
            bail!("hook failed: {}{}", cmd, detail);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::EnvVar;

    fn running_with(name: &str, port: u16) -> HashMap<String, RunningService> {
        HashMap::from([(
            name.to_string(),
            RunningService {
                name: name.to_string(),
                ports: HashMap::from([("default".to_string(), port)]),
            },
        )])
    }

    fn eph_with_env(pairs: &[(&str, &str)]) -> EphFile {
        EphFile {
            env_vars: pairs
                .iter()
                .map(|(name, value)| EnvVar {
                    name: (*name).to_string(),
                    value: (*value).to_string(),
                })
                .collect(),
            services: Default::default(),
            roles_order: None,
        }
    }

    #[test]
    fn strict_resolution_reports_ordered_deduplicated_references() {
        let eph = eph_with_env(&[
            (
                "DATABASE_URL",
                "${db.port}/${cache.port}/${db.port}/${db.host}",
            ),
            ("READY", "yes"),
        ]);

        let error = resolve_env_vars_strict(&eph, &HashMap::new()).unwrap_err();

        assert_eq!(
            error.resolved,
            vec![("READY".to_string(), "yes".to_string())]
        );
        assert_eq!(error.unresolved.len(), 1);
        assert_eq!(error.unresolved[0].name, "DATABASE_URL");
        assert_eq!(
            error.unresolved[0].references,
            vec![
                UnresolvedReference {
                    service: "db".to_string(),
                    property: "port".to_string(),
                },
                UnresolvedReference {
                    service: "cache".to_string(),
                    property: "port".to_string(),
                },
                UnresolvedReference {
                    service: "db".to_string(),
                    property: "host".to_string(),
                },
            ]
        );
    }

    #[test]
    fn strict_resolution_returns_only_complete_values() {
        let eph = eph_with_env(&[("DATABASE_URL", "redis://${db.host}:${db.port}")]);

        let resolved = resolve_env_vars_strict(&eph, &running_with("db", 6379)).unwrap();

        assert_eq!(
            resolved,
            vec![(
                "DATABASE_URL".to_string(),
                "redis://localhost:6379".to_string(),
            )]
        );
    }

    #[test]
    fn strict_resolution_treats_escaped_interpolation_as_literal_text() {
        let eph = eph_with_env(&[("LITERAL", "cost is $${db.port} dollars")]);

        let resolved = resolve_env_vars_strict(&eph, &HashMap::new()).unwrap();

        assert_eq!(
            resolved,
            vec![(
                "LITERAL".to_string(),
                "cost is ${db.port} dollars".to_string(),
            )]
        );
    }

    fn port(name: Option<&str>, container_port: u16) -> PortMapping {
        PortMapping {
            name: name.map(str::to_string),
            container_port,
            auto: false,
        }
    }

    fn auto_port(name: Option<&str>) -> PortMapping {
        PortMapping {
            name: name.map(str::to_string),
            container_port: 0,
            auto: true,
        }
    }

    #[test]
    fn allocate_ports_uses_fixed_ports_verbatim() {
        let declared = vec![port(None, 3000), port(Some("api"), 4000)];
        let assigned = allocate_ports(&declared, None).unwrap();
        assert_eq!(assigned.get("default"), Some(&3000));
        assert_eq!(assigned.get("api"), Some(&4000));
    }

    #[test]
    fn allocate_ports_assigns_distinct_free_ports_for_auto() {
        let declared = vec![
            auto_port(None),
            auto_port(Some("hmr")),
            auto_port(Some("api")),
        ];
        let assigned = allocate_ports(&declared, None).unwrap();
        assert_eq!(assigned.len(), 3);

        // Every assigned port is non-zero and they are all distinct.
        let mut values: Vec<u16> = assigned.values().copied().collect();
        assert!(values.iter().all(|&p| p != 0));
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), 3, "auto ports must be distinct");
    }

    #[test]
    fn allocate_ports_reuses_previous_free_port() {
        // Pick a port the OS just told us is free, then ask for an auto port with
        // that as the previous assignment: it should be reused for a stable URL.
        let free = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let prev = HashMap::from([("default".to_string(), free)]);

        let assigned = allocate_ports(&[auto_port(None)], Some(&prev)).unwrap();
        assert_eq!(assigned.get("default"), Some(&free));
    }

    #[test]
    fn allocate_ports_skips_busy_previous_port() {
        // Hold a port so it is not bindable, then offer it as the previous
        // assignment: allocation must fall back to a different, free port.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        let prev = HashMap::from([("default".to_string(), busy)]);

        let assigned = allocate_ports(&[auto_port(None)], Some(&prev)).unwrap();
        assert_ne!(assigned.get("default"), Some(&busy));
        assert!(assigned.get("default").is_some_and(|&p| p != 0));
    }

    #[test]
    fn has_auto_port_detects_auto_mappings() {
        assert!(has_auto_port(&[port(None, 3000), auto_port(Some("api"))]));
        assert!(!has_auto_port(&[port(None, 3000), port(Some("api"), 4000)]));
        assert!(!has_auto_port(&[]));
    }

    #[test]
    fn parse_command_override_splits_and_passes_through_none() {
        // No override declared.
        assert_eq!(parse_command_override("web", None).unwrap(), None);

        // A well-formed override is split into argv, honoring quoting.
        assert_eq!(
            parse_command_override("web", Some(r#"sh -c "echo hi""#)).unwrap(),
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string()
            ])
        );

        // An empty override parses to an empty argv (no tokens), not an error.
        assert_eq!(
            parse_command_override("web", Some("")).unwrap(),
            Some(vec![])
        );
    }

    #[test]
    fn parse_command_override_fails_closed_on_unbalanced_quote() {
        // Regression for #15: an unbalanced quote must error at startup, naming
        // the service, rather than being smuggled through as one argv element.
        let err = parse_command_override("web", Some(r#"sh -c "echo hi"#))
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("invalid command override for service 'web':"),
            "got: {err}"
        );
    }

    /// A `Workspace` with fixed ids, built without touching the filesystem, so
    /// volume-spec resolution can be exercised without Docker or a real
    /// workspace directory.
    fn test_workspace(path: &str) -> Workspace {
        Workspace {
            path: PathBuf::from(path),
            id: "abcd1234ef567890".to_string(),
            short_id: "abcd1234".to_string(),
        }
    }

    #[test]
    fn resolve_volume_spec_namespaces_named_volumes() {
        let ws = test_workspace("/ws");
        // A bare name is namespaced to `eph-<short_id>-<service>-<name>` so two
        // workspaces or services never share a volume.
        assert_eq!(
            resolve_volume_spec("data:/var/lib/postgresql/data", &ws, "db").unwrap(),
            "eph-abcd1234-db-data:/var/lib/postgresql/data"
        );
    }

    #[test]
    fn resolve_volume_spec_passes_absolute_binds_through() {
        let ws = test_workspace("/ws");
        // An absolute host path is a bind mount used verbatim (not namespaced).
        assert_eq!(
            resolve_volume_spec("/host/path:/in/container", &ws, "db").unwrap(),
            "/host/path:/in/container"
        );
    }

    #[test]
    fn resolve_volume_spec_resolves_relative_binds_against_workspace() {
        let ws = test_workspace("/ws");
        // A leading `.` is resolved relative to the workspace root.
        let expected = format!(
            "{}:/in/container",
            PathBuf::from("/ws").join("./data").to_string_lossy()
        );
        assert_eq!(
            resolve_volume_spec("./data:/in/container", &ws, "db").unwrap(),
            expected
        );
    }

    #[test]
    fn resolve_volume_spec_passes_through_specs_without_a_container_path() {
        let ws = test_workspace("/ws");
        // No `:<container_path>` half: passed through unchanged (Docker reports
        // the malformed mount). Holds for both the named and host-path branches.
        assert_eq!(
            resolve_volume_spec("justaname", &ws, "db").unwrap(),
            "justaname"
        );
        assert_eq!(
            resolve_volume_spec("/abs/only", &ws, "db").unwrap(),
            "/abs/only"
        );
        assert_eq!(
            resolve_volume_spec("./rel/only", &ws, "db").unwrap(),
            "./rel/only"
        );
    }

    #[test]
    fn resolve_volume_spec_relative_bind_against_plain_windows_root_is_clean() {
        // Regression for #44: with the workspace path normalized to a plain
        // `C:\...` form (as dunce::canonicalize now yields), a relative bind
        // resolves to a source Docker accepts, with no `\\?\` prefix.
        let ws = test_workspace(r"C:\Users\me\project");
        let resolved = resolve_volume_spec("./seed:/docker-entrypoint-initdb.d", &ws, "postgres")
            .expect("plain Windows root must resolve cleanly");
        assert!(
            !resolved.starts_with(r"\\?\"),
            "resolved source must not carry the extended-length prefix: {resolved}"
        );
        assert!(resolved.ends_with(":/docker-entrypoint-initdb.d"));
    }

    #[test]
    fn resolve_volume_spec_rejects_verbatim_relative_source() {
        // Regression for #44: if the workspace path could not be normalized (a
        // genuine long path keeps the `\\?\` prefix), a relative bind that
        // resolves onto it is rejected here with an actionable error rather than
        // forwarded to Docker, which would reject it cryptically.
        let ws = test_workspace(r"\\?\C:\Users\me\project");
        let err = resolve_volume_spec("./seed:/in/container", &ws, "db")
            .expect_err("a verbatim source must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("extended-length"),
            "error should explain the extended-length path: {msg}"
        );
    }

    #[test]
    fn resolve_volume_spec_passes_windows_drive_absolute_bind_through() {
        // Regression for #52: an absolute Windows source starts with a drive
        // letter, not `/` or `.`, and the drive colon must not be mistaken for
        // the source/destination separator. Both `\` and `/` path separators are
        // valid after the drive colon on Windows.
        let ws = test_workspace(r"C:\ws");
        assert_eq!(
            resolve_volume_spec(r"C:\Users\me\data:/data", &ws, "db").unwrap(),
            r"C:\Users\me\data:/data"
        );
        assert_eq!(
            resolve_volume_spec("C:/Users/me/data:/data", &ws, "db").unwrap(),
            "C:/Users/me/data:/data"
        );
        // Any drive letter, not just `C:`, and case-insensitive.
        assert_eq!(
            resolve_volume_spec(r"X:\Data\seed:/data", &ws, "db").unwrap(),
            r"X:\Data\seed:/data"
        );
        assert_eq!(
            resolve_volume_spec(r"d:\data:/data", &ws, "db").unwrap(),
            r"d:\data:/data"
        );
    }

    #[test]
    fn resolve_volume_spec_preserves_mode_field() {
        // A trailing `:ro`/`:rw`/`:z` mode is part of the destination remainder
        // and must survive on every branch: relative, drive-letter, and named.
        let ws = test_workspace(r"C:\ws");
        assert_eq!(
            resolve_volume_spec(r"C:\data:/data:ro", &ws, "db").unwrap(),
            r"C:\data:/data:ro"
        );
        assert_eq!(
            resolve_volume_spec("/host/path:/data:rw", &ws, "db").unwrap(),
            "/host/path:/data:rw"
        );
        assert_eq!(
            resolve_volume_spec("data:/data:ro", &ws, "db").unwrap(),
            "eph-abcd1234-db-data:/data:ro"
        );
    }

    #[test]
    fn resolve_volume_spec_passes_unc_bind_through() {
        // A UNC source (`\\server\share\...`) is a host bind: it starts with a
        // backslash but is not the rejected verbatim `\\?\` form.
        let ws = test_workspace(r"C:\ws");
        assert_eq!(
            resolve_volume_spec(r"\\server\share\data:/data", &ws, "db").unwrap(),
            r"\\server\share\data:/data"
        );
    }

    #[test]
    fn resolve_volume_spec_rejects_verbatim_drive_bind() {
        // A verbatim `\\?\C:\...` absolute source is classified as a host bind
        // (the drive colon is skipped), then rejected with the extended-length
        // error rather than misparsed into a named volume named `\\?\C`.
        let ws = test_workspace(r"C:\ws");
        let err = resolve_volume_spec(r"\\?\C:\data:/data", &ws, "db")
            .expect_err("a verbatim drive source must be rejected");
        assert!(
            err.to_string().contains("extended-length"),
            "error should explain the extended-length path: {err}"
        );
    }

    #[test]
    fn resolve_volume_spec_passes_windows_drive_source_only_through() {
        // A drive source with no container path (`C:\data`, no `:/dest`) has no
        // real separator once the drive colon is skipped, so it passes through
        // unchanged rather than becoming a named volume named `C`.
        let ws = test_workspace(r"C:\ws");
        assert_eq!(
            resolve_volume_spec(r"C:\data", &ws, "db").unwrap(),
            r"C:\data"
        );
        assert_eq!(resolve_volume_spec("C:", &ws, "db").unwrap(), "C:");
    }

    #[test]
    fn resolve_volume_spec_passes_empty_source_through() {
        // A leading-colon spec (`:/data`) has an empty source: pass it through so
        // Docker reports it, rather than namespacing an empty volume name.
        let ws = test_workspace("/ws");
        assert_eq!(resolve_volume_spec(":/data", &ws, "db").unwrap(), ":/data");
    }

    #[test]
    fn is_host_path_source_classifies_every_source_shape() {
        // Host binds: Unix absolute, relative, drive-letter (both slash styles),
        // UNC, and verbatim drive.
        assert!(is_host_path_source("/host/path"));
        assert!(is_host_path_source("./rel"));
        assert!(is_host_path_source(r"C:\data"));
        assert!(is_host_path_source("C:/data"));
        assert!(is_host_path_source(r"\\server\share"));
        assert!(is_host_path_source(r"\\?\C:\data"));
        // Named volumes: bare names, including a non-drive `name:` shape.
        assert!(!is_host_path_source("data"));
        assert!(!is_host_path_source("pgdata"));
    }

    #[test]
    fn split_volume_source_skips_drive_colon() {
        // The drive colon is never the separator; the first non-drive colon is.
        assert_eq!(
            split_volume_source(r"C:\data:/dest"),
            Some((r"C:\data", "/dest"))
        );
        assert_eq!(
            split_volume_source(r"\\?\C:\data:/dest"),
            Some((r"\\?\C:\data", "/dest"))
        );
        assert_eq!(
            split_volume_source("data:/dest:ro"),
            Some(("data", "/dest:ro"))
        );
        // No non-drive separator: source-only, no split.
        assert_eq!(split_volume_source(r"C:\data"), None);
        assert_eq!(split_volume_source("justaname"), None);
    }

    #[test]
    fn log_indicates_port_conflict_matches_common_runtimes() {
        // Node, Go, Python, Rust, .NET / generic libc phrasings.
        assert!(log_indicates_port_conflict(
            "Error: listen EADDRINUSE: address already in use :::3000"
        ));
        assert!(log_indicates_port_conflict(
            "listen tcp 127.0.0.1:8080: bind: address already in use"
        ));
        assert!(log_indicates_port_conflict(
            "OSError: [Errno 98] Address already in use"
        ));
        assert!(log_indicates_port_conflict(
            "thread 'main' panicked: Address already in use (os error 98)"
        ));
        assert!(log_indicates_port_conflict("Port 5173 is already in use"));
    }

    #[test]
    fn log_indicates_port_conflict_ignores_unrelated_crashes() {
        assert!(!log_indicates_port_conflict(
            "TypeError: cannot read properties of undefined"
        ));
        assert!(!log_indicates_port_conflict("command not found: vite"));
        assert!(!log_indicates_port_conflict(""));
    }

    /// Raw bindings as produced by `get_container`: keyed by container-port
    /// number, plus a positional `"default"`.
    fn raw(pairs: &[(&str, u16)]) -> HashMap<String, u16> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn maps_declared_names_onto_host_ports() {
        let declared = vec![port(Some("api"), 9000), port(Some("console"), 9001)];
        let raw = raw(&[("9000", 32790), ("9001", 32791), ("default", 32790)]);

        let mapped = map_named_ports(&declared, &raw);

        assert_eq!(mapped.get("api"), Some(&32790));
        assert_eq!(mapped.get("console"), Some(&32791));
        // Raw container-port-number keys are dropped; only declared names remain.
        assert_eq!(mapped.get("9000"), None);
        assert_eq!(mapped.len(), 2);
    }

    #[test]
    fn unnamed_port_falls_back_to_default() {
        let declared = vec![port(None, 5432)];
        let raw = raw(&[("5432", 49153), ("default", 49153)]);

        let mapped = map_named_ports(&declared, &raw);

        assert_eq!(mapped.get("default"), Some(&49153));
        assert_eq!(mapped.len(), 1);
    }

    /// The bytes `tail_start_offset` would have us begin streaming at, as a
    /// string -- i.e. the raw tail of the file -- for terse assertions.
    fn tail_of(contents: &[u8], n: usize) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, contents).unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let len = file.seek(SeekFrom::End(0)).unwrap();
        let start = tail_start_offset(&mut file, len, n).unwrap();
        file.seek(SeekFrom::Start(start)).unwrap();
        let mut rest = Vec::new();
        file.read_to_end(&mut rest).unwrap();
        String::from_utf8_lossy(&rest).into_owned()
    }

    #[test]
    fn tail_start_offset_trims_to_last_n_lines() {
        assert_eq!(tail_of(b"a\nb\nc\nd\ne\n", 2), "d\ne\n");
        // No trailing newline on the last line.
        assert_eq!(tail_of(b"a\nb\nc\nd\ne", 2), "d\ne");
        assert_eq!(tail_of(b"a\nb\nc\nd\ne\n", 1), "e\n");
    }

    #[test]
    fn tail_start_offset_more_lines_than_file_returns_all() {
        assert_eq!(tail_of(b"only\ntwo\n", 100), "only\ntwo\n");
    }

    #[test]
    fn tail_start_offset_handles_empty_and_zero() {
        assert_eq!(tail_of(b"", 5), "");
        // tail 0 means "no lines": start at end of file.
        assert_eq!(tail_of(b"a\nb\n", 0), "");
    }

    #[test]
    fn tail_start_offset_spans_blocks() {
        // Force the backward scan across multiple 8 KiB blocks.
        let mut contents = String::new();
        for i in 0..5000 {
            contents.push_str(&format!("line-{i}\n"));
        }
        let tail = tail_of(contents.as_bytes(), 3);
        assert_eq!(tail, "line-4997\nline-4998\nline-4999\n");
    }

    #[test]
    fn decode_log_line_strips_line_endings() {
        assert_eq!(decode_log_line(b"hello\n"), "hello");
        assert_eq!(decode_log_line(b"hello\r\n"), "hello");
        assert_eq!(decode_log_line(b"hello"), "hello");
        assert_eq!(decode_log_line(b""), "");
        // A bare \r that is not part of CRLF is preserved as content.
        assert_eq!(decode_log_line(b"a\rb\n"), "a\rb");
    }

    #[test]
    fn decode_log_line_is_lossy_for_invalid_utf8() {
        // An invalid byte (0xFF) must not panic; it is replaced.
        let out = decode_log_line(&[b'o', b'k', 0xFF]);
        assert!(out.starts_with("ok"), "got {out:?}");
    }

    /// Regression for #14: re-keying the raw bindings reproduces the same map on
    /// the restart path that the fresh-create path produced, so a down/up cycle
    /// does not lose named ports.
    #[test]
    fn restart_remapping_matches_fresh_create() {
        let declared = vec![port(Some("api"), 9000), port(Some("console"), 9001)];
        // Fresh create and a later restart can land on different host ports, but
        // both go through the same name mapping. The keys must stay stable.
        let fresh = map_named_ports(&declared, &raw(&[("9000", 32790), ("9001", 32791)]));
        let restarted = map_named_ports(&declared, &raw(&[("9000", 40000), ("9001", 40001)]));

        let mut fresh_keys: Vec<_> = fresh.keys().cloned().collect();
        let mut restarted_keys: Vec<_> = restarted.keys().cloned().collect();
        fresh_keys.sort();
        restarted_keys.sort();
        assert_eq!(fresh_keys, vec!["api".to_string(), "console".to_string()]);
        assert_eq!(fresh_keys, restarted_keys);
    }

    #[test]
    fn declared_port_absent_from_bindings_is_skipped() {
        let declared = vec![port(Some("api"), 9000), port(Some("metrics"), 9999)];
        // The container never published 9999.
        let mapped = map_named_ports(&declared, &raw(&[("9000", 32790)]));

        assert_eq!(mapped.get("api"), Some(&32790));
        assert_eq!(mapped.get("metrics"), None);
        assert_eq!(mapped.len(), 1);
    }

    fn pid(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    /// The persisted backend representation is part of the on-disk state schema,
    /// so pin it down: each variant must round-trip and serialize to its
    /// snake_case, externally tagged form.
    #[test]
    fn backend_serde_round_trips_each_variant() {
        let cases = [
            (
                Backend::Container {
                    id: "abc123".to_string(),
                },
                r#"{"container":{"id":"abc123"}}"#,
            ),
            (
                Backend::Process {
                    pid: pid(4321),
                    identity: None,
                },
                r#"{"process":{"pid":4321}}"#,
            ),
            (
                Backend::Compose {
                    project: "eph-ab12-web".to_string(),
                },
                r#"{"compose":{"project":"eph-ab12-web"}}"#,
            ),
        ];

        for (backend, json) in cases {
            assert_eq!(serde_json::to_string(&backend).unwrap(), json);
            assert_eq!(serde_json::from_str::<Backend>(json).unwrap(), backend);
        }
    }

    /// A process backend can never carry PID 0 (it is not a real process, and on
    /// Unix signaling PID 0 targets the caller's own process group), so
    /// deserializing one is rejected rather than silently accepted.
    #[test]
    fn backend_process_rejects_zero_pid() {
        let err = serde_json::from_str::<Backend>(r#"{"process":{"pid":0}}"#);
        assert!(err.is_err(), "PID 0 must not deserialize: {err:?}");
    }

    /// A full state entry round-trips, confirming `backend` and `ports` are the
    /// only persisted fields after dropping the parallel `processes` map.
    #[test]
    fn state_entry_round_trips() {
        let entry = ServiceStateEntry {
            backend: Backend::Process {
                pid: pid(1234),
                identity: None,
            },
            ports: HashMap::from([("default".to_string(), 5173)]),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ServiceStateEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend, entry.backend);
        assert_eq!(back.ports, entry.ports);
    }

    /// Regression: a state file written before the `Backend` enum landed (the
    /// stringly-typed `container_id` plus a top-level `processes` map) must
    /// still load, so an in-place upgrade does not orphan running services or
    /// wedge `eph down` / `eph clean`. Each legacy id form maps to its variant,
    /// and the now-removed `processes` map is ignored.
    #[test]
    fn load_migrates_legacy_state_schema() {
        let legacy = r#"{
            "services": {
                "db":  { "container_id": "abc123def456", "ports": { "default": 5432 } },
                "web": { "container_id": "pid:4321", "ports": { "default": 5173 } },
                "stack": { "container_id": "compose:eph-ab12-stack", "ports": {} }
            },
            "processes": { "web": 4321 },
            "auto_ports": { "web": { "default": 5173 } }
        }"#;

        let state: ServiceState = serde_json::from_str(legacy).unwrap();

        assert_eq!(
            state.services["db"].backend,
            Backend::Container {
                id: "abc123def456".to_string()
            }
        );
        assert_eq!(
            state.services["web"].backend,
            Backend::Process {
                pid: pid(4321),
                identity: None
            }
        );
        assert_eq!(
            state.services["stack"].backend,
            Backend::Compose {
                project: "eph-ab12-stack".to_string()
            }
        );
        // The legacy `processes` map is dropped; its PID survives via the
        // migrated `Backend::Process`. `auto_ports` is unchanged.
        assert_eq!(state.auto_ports["web"]["default"], 5173);
    }

    /// `start_order` defers `run=` apps to the end (so backing services start
    /// first) while preserving declaration order within each group, and teardown
    /// is exactly its reverse, so a dependent stops before its dependency even
    /// when the app is declared before the service it depends on.
    #[test]
    fn start_order_defers_run_services_and_teardown_reverses_it() {
        // `app` (run=) is declared *before* `postgres` it depends on, plus a
        // second backing service to confirm intra-group order is kept.
        let eph = crate::parser::parse(
            r#"
[app]
run=./serve
port=auto

[postgres]
image=postgres:16

[redis]
image=redis:7
"#,
        )
        .unwrap();

        let order: Vec<&str> = start_order(&eph).iter().map(|s| s.as_str()).collect();
        // Backing services first (in declaration order), then the run= app.
        assert_eq!(order, ["postgres", "redis", "app"]);

        // Teardown reverses the start order: the app stops before postgres/redis.
        let teardown: Vec<&str> = start_order(&eph)
            .into_iter()
            .rev()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(teardown, ["app", "redis", "postgres"]);
    }

    /// In roles mode, `start_order` follows the role graph rather than the
    /// source-based heuristic: a `run=` service tagged as a dependency (a mock
    /// server, say) comes up before the app even though the legacy rule would
    /// defer every `run=` service to the end.
    #[test]
    fn start_order_follows_roles_over_the_run_last_heuristic() {
        let eph = crate::parser::parse(
            r#"
roles_order=dep,app

[web]
run=./serve
port=auto
role=app

[postgres]
image=postgres:16
role=dep

[mock-auth]
run=./mock-auth
role=dep
"#,
        )
        .unwrap();

        // Both dep services (including the run= mock) precede the run= app, in
        // declaration order within the dep role.
        let order: Vec<&str> = start_order(&eph).iter().map(|s| s.as_str()).collect();
        assert_eq!(order, ["postgres", "mock-auth", "web"]);
    }

    #[tokio::test]
    async fn wait_until_ready_returns_the_first_some_after_polling() {
        let mut calls = 0;
        let out: i32 = wait_until_ready(
            "svc",
            Duration::from_secs(5),
            Duration::from_millis(1),
            async || {
                calls += 1;
                // Pend twice, then become ready with a value.
                if calls >= 3 { Ok(Some(42)) } else { Ok(None) }
            },
        )
        .await
        .unwrap();
        assert_eq!(out, 42);
        assert_eq!(calls, 3, "probe should be polled until it returns Some");
    }

    #[tokio::test]
    async fn wait_until_ready_times_out_with_one_lowercase_message() {
        // A probe that never becomes ready must time out with the single,
        // lowercase "service ... failed to become healthy" message shared by
        // every readiness path (regression for the casing split in #19).
        let err = wait_until_ready::<()>(
            "svc",
            Duration::from_millis(30),
            Duration::from_millis(5),
            async || Ok(None),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "service svc failed to become healthy within 0s"
        );
    }

    #[tokio::test]
    async fn wait_until_ready_propagates_a_probe_error() {
        // An `Err` from the probe aborts immediately rather than waiting out the
        // timeout.
        let err = wait_until_ready::<()>(
            "svc",
            Duration::from_secs(5),
            Duration::from_millis(1),
            async || anyhow::bail!("probe blew up"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("probe blew up"), "got: {err}");
    }
}
