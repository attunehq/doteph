//! Service management - starting, stopping, and managing Docker containers

use crate::parser::{EphFile, PortMapping, Service, ServiceSource, resolve_interpolations};
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, ContainerSummaryStateEnum, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveVolumeOptionsBuilder, StopContainerOptionsBuilder,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

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
    /// Backend identifier: a Docker container id, `pid:<n>` for `run` services,
    /// or `compose:<project>` for compose services.
    pub container_id: String,
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
/// interpolations using the assigned host ports in `running`, and is exactly
/// the set of pairs that `eph env` emits. A reference to a service that is not
/// in `running` is left as the literal `${...}` placeholder, matching
/// [`resolve_interpolations`].
///
/// It is shared by `eph env` and by the lifecycle-hook / `eph run` machinery so
/// that a `post-start` hook, a `pre-stop` hook, and a developer's shell all see
/// the same resolved environment.
#[must_use]
pub fn resolve_env_vars(
    eph: &EphFile,
    running: &HashMap<String, RunningService>,
) -> Vec<(String, String)> {
    let resolver = |service: &str, property: &str| -> Option<String> {
        let svc = running.get(service)?;
        match property {
            "host" => Some(svc.host().to_string()),
            "port" => svc.port().map(|p| p.to_string()),
            prop if prop.starts_with("port.") => svc.named_port(&prop[5..]).map(|p| p.to_string()),
            _ => None,
        }
    };

    eph.env_vars
        .iter()
        .map(|var| {
            (
                var.name.clone(),
                resolve_interpolations(&var.value, resolver),
            )
        })
        .collect()
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

/// Persistent state for a workspace's services
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ServiceState {
    /// Running services keyed by service name
    pub(crate) services: HashMap<String, ServiceStateEntry>,
    /// Process IDs for shell command services
    #[serde(default)]
    pub(crate) processes: HashMap<String, u32>,
}

/// State entry for a single service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ServiceStateEntry {
    pub(crate) container_id: String,
    pub(crate) ports: HashMap<String, u16>,
}

impl ServiceState {
    /// Load state from disk
    pub(crate) async fn load(workspace: &Workspace) -> Result<Self> {
        let path = state_file_path(workspace)?;

        if !path.exists() {
            return Ok(ServiceState::default());
        }

        let contents = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read state file: {}", path.display()))?;

        let state: ServiceState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse state file: {}", path.display()))?;

        Ok(state)
    }

    /// Save state to disk
    pub(crate) async fn save(&self, workspace: &Workspace) -> Result<()> {
        let path = state_file_path(workspace)?;

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create state directory: {}", parent.display())
            })?;
        }

        let contents = serde_json::to_string_pretty(self).context("failed to serialize state")?;

        tokio::fs::write(&path, contents)
            .await
            .with_context(|| format!("failed to write state file: {}", path.display()))?;

        Ok(())
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
async fn stream_file_lines(
    name: String,
    path: PathBuf,
    follow: bool,
    tail: Option<usize>,
    tx: mpsc::Sender<LogLine>,
) -> Result<()> {
    // Snapshot the current contents: emit the complete lines (after --tail), and
    // when following, carry the unterminated trailing fragment forward so a line
    // split across the snapshot boundary is not emitted in two halves.
    let bytes = std::fs::read(&path).unwrap_or_default();
    let mut offset = bytes.len() as u64;
    let content = String::from_utf8_lossy(&bytes).into_owned();
    let mut segments: Vec<String> = content
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();
    // `split('\n')` always yields a final element: the text after the last
    // newline ("" when the file ends on a newline). That is the partial line.
    let mut partial = segments.pop().unwrap_or_default();

    if !follow && !partial.is_empty() {
        // Not following: a trailing line with no newline is still real output.
        segments.push(std::mem::take(&mut partial));
    }

    for line in apply_tail(segments, tail) {
        if tx.send(line_for(&name, line)).await.is_err() {
            return Ok(());
        }
    }

    if !follow {
        return Ok(());
    }

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
            partial.clear();
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
        let mut buf = Vec::new();
        let Ok(read) = file.read_to_end(&mut buf) else {
            continue;
        };
        offset += read as u64;
        partial.push_str(&String::from_utf8_lossy(&buf));

        // Emit only the complete lines now in the buffer; keep the rest until its
        // newline shows up on a later poll.
        while let Some(idx) = partial.find('\n') {
            let mut line: String = partial.drain(..=idx).collect();
            line.pop(); // the '\n'
            if line.ends_with('\r') {
                line.pop();
            }
            if tx.send(line_for(&name, line)).await.is_err() {
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

/// Keep only the last `tail` entries of `lines` (all of them when `tail` is
/// `None` or larger than the list).
fn apply_tail(mut lines: Vec<String>, tail: Option<usize>) -> Vec<String> {
    if let Some(n) = tail {
        let start = lines.len().saturating_sub(n);
        lines.drain(..start);
    }
    lines
}

/// Read a log file, optionally trimming to the last `tail` lines.
///
/// Bytes are decoded lossily so a stray non-UTF-8 byte in a service's output
/// does not abort `eph logs`. When `tail` is `Some(n)`, the last `n` lines are
/// returned with a trailing newline (so the next follow chunk starts on its own
/// line); when `None`, the file is returned verbatim. A request for more lines
/// than the file holds returns the whole file.
fn read_tail(path: &Path, tail: Option<usize>) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read log file: {}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes);

    match tail {
        Some(n) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(n);
            let mut tail_lines = lines[start..].join("\n");
            if !tail_lines.is_empty() {
                tail_lines.push('\n');
            }
            Ok(tail_lines)
        }
        None => Ok(content.into_owned()),
    }
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

    /// Stop a container
    pub(crate) async fn stop_container(&self, name: &str) -> Result<()> {
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
        }
        Ok(())
    }

    /// Remove a container
    pub(crate) async fn remove_container(&self, name: &str) -> Result<()> {
        if let Some(info) = self.get_container(name).await? {
            info!("Removing container {}", name);
            self.client
                .remove_container(
                    &info.id,
                    Some(RemoveContainerOptionsBuilder::new().force(true).build()),
                )
                .await
                .context("failed to remove container")?;
        }
        Ok(())
    }

    /// Remove a named volume, ignoring "not found" errors
    pub(crate) async fn remove_volume(&self, name: &str) -> Result<()> {
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
            Ok(()) => Ok(()),
            // Volume already gone (or never created) - treat as success.
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove volume {}", name)),
        }
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

    /// Pull an image and run it as a container
    pub(crate) async fn run_image(
        &self,
        container_name: &str,
        image: &str,
        service: &Service,
        workspace: &Workspace,
    ) -> Result<RunningService> {
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
        let env: Vec<String> = service
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // Build volume bindings
        let binds: Vec<String> = service
            .volumes
            .iter()
            .map(|v| {
                if v.starts_with('/') || v.starts_with('.') {
                    // Absolute or relative path - resolve relative to workspace
                    let parts: Vec<&str> = v.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let host_path = if parts[0].starts_with('.') {
                            workspace.path.join(parts[0]).to_string_lossy().to_string()
                        } else {
                            parts[0].to_string()
                        };
                        format!("{}:{}", host_path, parts[1])
                    } else {
                        v.clone()
                    }
                } else {
                    // Named volume - prefix with workspace
                    let parts: Vec<&str> = v.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let volume_name = workspace.volume_name(&service.name, parts[0]);
                        format!("{}:{}", volume_name, parts[1])
                    } else {
                        v.clone()
                    }
                }
            })
            .collect();

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            binds: Some(binds),
            ..Default::default()
        };

        // Handle command override
        let cmd = service
            .command_override
            .as_ref()
            .map(|c| shell_words::split(c).unwrap_or_else(|_| vec![c.clone()]));

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

        Ok(RunningService {
            name: service.name.clone(),
            container_id: response.id,
            ports: named_ports,
        })
    }

    /// Build from Dockerfile and run
    pub(crate) async fn build_and_run(
        &self,
        container_name: &str,
        dockerfile_path: &std::path::Path,
        service: &Service,
        workspace: &Workspace,
    ) -> Result<RunningService> {
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
        self.run_image(container_name, &image_tag, service, workspace)
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
        self.start_services(eph, &[], false).await
    }

    /// Start the requested services (or all of them when `filter` is empty),
    /// then run `post-start` hooks once every service is healthy.
    ///
    /// Startup happens in two phases:
    ///
    /// 1. Every target service is created (or reused) and waited on until
    ///    healthy. No hooks run yet.
    /// 2. Every target service's `post-start` hooks run with the fully-resolved
    ///    environment.
    ///
    /// Deferring the hooks to phase 2 means a hook can reference any service in
    /// the workspace -- a database migration whose `DATABASE_URL` interpolates
    /// `${postgres.port}` resolves correctly even though, within a single
    /// `eph up`, postgres might have been created before the service whose hook
    /// needs it.
    ///
    /// `post-start` hooks run on **every** `eph up`, not only when a service is
    /// freshly created. Hooks are therefore expected to be idempotent (a
    /// migration that no-ops when already applied, an `INSERT ... ON CONFLICT`
    /// seed); use [`eph run`](crate) for one-off, non-idempotent operations.
    ///
    /// When `skip_hooks` is true, phase 2 is skipped entirely: services are
    /// brought up healthy but no `post-start` hooks run.
    ///
    /// # Errors
    ///
    /// Returns an error if a service name in `filter` is unknown, if any service
    /// fails to start, if a `post-start` hook fails, or if state cannot be
    /// saved.
    pub async fn start_services(
        &mut self,
        eph: &EphFile,
        filter: &[String],
        skip_hooks: bool,
    ) -> Result<HashMap<String, RunningService>> {
        // Resolve the target set: every service, or just the requested ones (in
        // the order requested). post-start hooks run in a second phase once all
        // of these are healthy, so the phase-1 start order does not affect
        // whether a hook's cross-service references resolve.
        let targets: Vec<&String> = if filter.is_empty() {
            eph.services.keys().collect()
        } else {
            for name in filter {
                if !eph.services.contains_key(name) {
                    bail!("unknown service: {}", name);
                }
            }
            filter.iter().collect()
        };

        // Phase 1: create or reuse every target, waiting for health.
        let mut running = HashMap::new();
        for name in &targets {
            let service = &eph.services[*name];
            let result = self.create_service(name, service).await?;
            running.insert((*name).clone(), result);
        }

        // Persist before running hooks so a hook that itself shells out to eph,
        // or that fails, leaves accurate state behind.
        self.state.save(&self.workspace).await?;

        if skip_hooks {
            return Ok(running);
        }

        // Phase 2: run post-start hooks with the full environment. Merge the
        // services already running from a previous `up` so cross-service
        // references resolve even on a filtered `eph up <one-service>`.
        let mut resolved = self.status().await?;
        for (name, svc) in &running {
            resolved.insert(name.clone(), svc.clone());
        }

        for name in &targets {
            let service = &eph.services[*name];
            if service.post_start.is_empty() {
                continue;
            }
            info!("Running post-start hooks for {}", name);
            let env = self.hook_env(eph, &resolved, service);
            for cmd in &service.post_start {
                self.run_hook(cmd, &env)
                    .await
                    .with_context(|| format!("post-start hook failed for service '{}'", name))?;
            }
        }

        Ok(running)
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
    async fn create_service(&mut self, name: &str, service: &Service) -> Result<RunningService> {
        let container_name = self.workspace.container_name(name);

        // Dedup run= (shell command) services: the Docker-based guard below
        // explicitly skips ServiceSource::Command, so without this check running
        // `eph up` twice would spawn a second process and orphan the first.
        // Probe the tracked PID the same way status() does (`kill -0 <pid>`).
        if matches!(service.source, ServiceSource::Command(_))
            && let Some(&pid) = self.state.processes.get(name)
        {
            let alive = TokioCommand::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .await
                .is_ok_and(|o| o.status.success());
            if alive {
                info!("Service {} already running (PID {})", name, pid);
                let ports = self
                    .state
                    .services
                    .get(name)
                    .map(|entry| entry.ports.clone())
                    .unwrap_or_default();
                return Ok(RunningService {
                    name: name.to_string(),
                    container_id: format!("pid:{}", pid),
                    ports,
                });
            }
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
                        container_id: existing.id.clone(),
                        ports: named_ports.clone(),
                    },
                );
                return Ok(RunningService {
                    name: name.to_string(),
                    container_id: existing.id,
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
                        container_id: refreshed.id.clone(),
                        ports: named_ports.clone(),
                    },
                );

                return Ok(RunningService {
                    name: name.to_string(),
                    container_id: refreshed.id,
                    ports: named_ports,
                });
            }
        }

        // Create and start new service
        info!("Creating service {}", name);
        let running = match &service.source {
            ServiceSource::Image(image) => {
                let r = self
                    .docker
                    .run_image(&container_name, image, service, &self.workspace)
                    .await?;

                // Wait for health check
                self.wait_for_healthy(name, service, &r.container_id)
                    .await?;

                r
            }
            ServiceSource::Dockerfile(path) => {
                let dockerfile_path = self.workspace.path.join(path);
                let r = self
                    .docker
                    .build_and_run(&container_name, &dockerfile_path, service, &self.workspace)
                    .await?;

                // Wait for health check
                self.wait_for_healthy(name, service, &r.container_id)
                    .await?;

                r
            }
            ServiceSource::Command(cmd) => self.start_shell_command(name, cmd, service).await?,
            ServiceSource::Compose(path) => self.start_compose(name, path, service).await?,
        };

        // Record in state
        self.state.services.insert(
            name.to_string(),
            ServiceStateEntry {
                container_id: running.container_id.clone(),
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

        let timeout_secs = service.ready_timeout_secs.unwrap_or(30);
        let check_interval = Duration::from_secs(1);

        info!(
            "Waiting for {} to be healthy (timeout: {}s)",
            name, timeout_secs
        );

        let result = timeout(Duration::from_secs(timeout_secs), async {
            loop {
                // Parse healthcheck command
                let parts: Vec<&str> = healthcheck.split_whitespace().collect();
                if parts.is_empty() {
                    return Ok(());
                }

                let exit_code = self.docker.exec_in_container(container_id, &parts).await?;

                if exit_code == 0 {
                    info!("Service {} is healthy", name);
                    return Ok(());
                }

                debug!(
                    "Health check for {} failed (exit {}), retrying...",
                    name, exit_code
                );
                sleep(check_interval).await;
            }
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => bail!(
                "service {} failed to become healthy within {}s",
                name,
                timeout_secs
            ),
        }
    }

    /// Start a shell command service
    async fn start_shell_command(
        &mut self,
        name: &str,
        cmd: &str,
        service: &Service,
    ) -> Result<RunningService> {
        info!("Starting shell command for {}: {}", name, cmd);

        // Build environment
        let mut env_vars: HashMap<String, String> = std::env::vars().collect();
        for (k, v) in &service.env {
            env_vars.insert(k.clone(), v.clone());
        }

        // Capture the child's stdout/stderr to a per-service log file under the
        // workspace state dir, readable via `eph logs <service>`. This also
        // solves a pipe-inheritance hang: a run= service is long-lived, so if it
        // inherited eph's stdout/stderr it would keep those pipe write-ends open
        // after `eph up` returns, and any caller capturing eph's output (a test
        // harness, `eph up | tee`, a CI step) would block forever waiting for
        // EOF. A file write-end has no such reader, so redirecting to a file both
        // preserves the output and avoids the hang. The file is truncated on each
        // fresh spawn so the log reflects the current run; a still-running service
        // is reused above and never reaches this point, so its log is preserved.
        let log_path = self.workspace.log_file_path(name)?;
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create logs directory: {}", parent.display())
            })?;
        }
        let log_file = std::fs::File::create(&log_path)
            .with_context(|| format!("failed to open log file: {}", log_path.display()))?;
        let log_file_err = log_file
            .try_clone()
            .with_context(|| format!("failed to open log file: {}", log_path.display()))?;

        let child = TokioCommand::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace.path)
            .envs(&env_vars)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err))
            .spawn()
            .with_context(|| format!("failed to start command: {}", cmd))?;

        let pid = child.id().unwrap_or(0);
        info!("Started {} with PID {}", name, pid);

        // Record PID
        self.state.processes.insert(name.to_string(), pid);

        // For shell commands, we don't have container ports
        // The service should bind to ports specified in the config
        let mut ports = HashMap::new();
        for port_mapping in &service.ports {
            let port_name = port_mapping
                .name
                .clone()
                .unwrap_or_else(|| "default".to_string());
            // Shell commands use their declared ports directly
            ports.insert(port_name, port_mapping.container_port);
        }

        // Wait a bit for the process to start
        sleep(Duration::from_millis(500)).await;

        // Run health check if specified
        if let Some(ref healthcheck) = service.healthcheck {
            let timeout_secs = service.ready_timeout_secs.unwrap_or(30);
            info!(
                "Waiting for {} to be healthy (timeout: {}s)",
                name, timeout_secs
            );

            let result = timeout(Duration::from_secs(timeout_secs), async {
                loop {
                    let output = TokioCommand::new("sh")
                        .arg("-c")
                        .arg(healthcheck)
                        .current_dir(&self.workspace.path)
                        .output()
                        .await?;

                    if output.status.success() {
                        info!("Service {} is healthy", name);
                        return Ok::<_, anyhow::Error>(());
                    }

                    debug!("Health check for {} failed, retrying...", name);
                    sleep(Duration::from_secs(1)).await;
                }
            })
            .await;

            match result {
                Ok(inner) => inner?,
                Err(_) => bail!(
                    "Service {} failed to become healthy within {}s",
                    name,
                    timeout_secs
                ),
            }
        }

        Ok(RunningService {
            name: name.to_string(),
            container_id: format!("pid:{}", pid),
            ports,
        })
    }

    /// Start a docker-compose service
    async fn start_compose(
        &mut self,
        name: &str,
        compose_path: &str,
        service: &Service,
    ) -> Result<RunningService> {
        let compose_file = self.workspace.path.join(compose_path);
        let project_name = format!("eph-{}-{}", self.workspace.short_id, name);

        info!(
            "Starting docker-compose service {} from {}",
            name,
            compose_file.display()
        );

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

            // Try to get the actual mapped port from docker compose
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

            // Fallback to declared port
            ports.insert(port_name, port_mapping.container_port);
        }

        // Wait for health check if specified
        if let Some(ref healthcheck) = service.healthcheck {
            let timeout_secs = service.ready_timeout_secs.unwrap_or(60);
            info!(
                "Waiting for {} to be healthy (timeout: {}s)",
                name, timeout_secs
            );

            let result = timeout(Duration::from_secs(timeout_secs), async {
                loop {
                    let output = TokioCommand::new("sh")
                        .arg("-c")
                        .arg(healthcheck)
                        .current_dir(&self.workspace.path)
                        .output()
                        .await?;

                    if output.status.success() {
                        info!("Service {} is healthy", name);
                        return Ok::<_, anyhow::Error>(());
                    }

                    debug!("Health check for {} failed, retrying...", name);
                    sleep(Duration::from_secs(2)).await;
                }
            })
            .await;

            match result {
                Ok(inner) => inner?,
                Err(_) => bail!(
                    "Service {} failed to become healthy within {}s",
                    name,
                    timeout_secs
                ),
            }
        }

        Ok(RunningService {
            name: name.to_string(),
            container_id: format!("compose:{}", project_name),
            ports,
        })
    }

    /// Stop all services, clear in-memory state, and persist the result.
    ///
    /// When `remove` is true, also remove containers (and compose resources) so
    /// they do not accumulate.
    ///
    /// # Errors
    ///
    /// Returns an error if stopping a service fails (see
    /// [`stop_service`](Self::stop_service)) or if state cannot be saved.
    pub async fn stop_all(&mut self, eph: &EphFile, remove: bool, skip_hooks: bool) -> Result<()> {
        // Snapshot the running services once, before any teardown, so every
        // pre-stop hook sees the full environment as it was when `down` began.
        let running = self.status().await?;
        for (name, service) in &eph.services {
            self.stop_service(name, service, remove, eph, &running, skip_hooks)
                .await?;
        }
        self.state.services.clear();
        self.state.processes.clear();
        self.state.save(&self.workspace).await?;
        Ok(())
    }

    /// Stop a single service after running its `pre-stop` hooks.
    ///
    /// When `remove` is true, also remove the underlying container after
    /// stopping it (compose uses `down`, which already removes containers;
    /// killing a `run` process already removes it). The process/compose teardown
    /// itself is best-effort and logged rather than propagated, so a stale or
    /// already-stopped service does not error.
    ///
    /// When `skip_hooks` is true, the `pre-stop` hooks are not run -- the escape
    /// hatch for a broken hook that would otherwise wedge teardown.
    ///
    /// # Errors
    ///
    /// Returns an error if a `pre-stop` hook fails (the service is left running
    /// so the hook can be retried), or if a Docker stop or remove call fails for
    /// an `image`/`dockerfile` service.
    pub async fn stop_service(
        &mut self,
        name: &str,
        service: &Service,
        remove: bool,
        eph: &EphFile,
        running: &HashMap<String, RunningService>,
        skip_hooks: bool,
    ) -> Result<()> {
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

        match &service.source {
            ServiceSource::Command(_) => {
                // Kill the process
                if let Some(&pid) = self.state.processes.get(name) {
                    info!("Stopping process {} (PID {})", name, pid);
                    // Send SIGTERM. Best-effort: the process may already have
                    // exited (stale PID), in which case `kill` fails and there is
                    // nothing left to stop, so the error is intentionally ignored.
                    let _ = TokioCommand::new("kill")
                        .arg(pid.to_string())
                        .output()
                        .await;
                    // Wait a bit then SIGKILL if it ignored SIGTERM. Same
                    // best-effort rationale: a failure means the process is
                    // already gone, so the result is intentionally ignored.
                    sleep(Duration::from_secs(2)).await;
                    let _ = TokioCommand::new("kill")
                        .args(["-9", &pid.to_string()])
                        .output()
                        .await;
                }
                self.state.processes.remove(name);
            }
            ServiceSource::Compose(path) => {
                let compose_file = self.workspace.path.join(path);
                let project_name = format!("eph-{}-{}", self.workspace.short_id, name);

                info!("Stopping docker-compose service {}", name);
                // Best-effort teardown: if the compose project is already down
                // (or was never brought up) `docker compose down` reports an
                // error we cannot act on here, so it is intentionally ignored.
                let _ = TokioCommand::new("docker")
                    .args([
                        "compose",
                        "-f",
                        &compose_file.to_string_lossy(),
                        "-p",
                        &project_name,
                        "down",
                    ])
                    .output()
                    .await;
            }
            _ => {
                let container_name = self.workspace.container_name(name);
                self.docker.stop_container(&container_name).await?;
                if remove {
                    self.docker.remove_container(&container_name).await?;
                }
            }
        }

        self.state.services.remove(name);
        Ok(())
    }

    /// Fully reset the workspace: stop and remove every service's container
    /// (or compose resources / process), remove every per-workspace named
    /// volume, clear in-memory state, and delete the persisted state file.
    ///
    /// Returns a [`CleanSummary`] describing what was removed.
    ///
    /// When `skip_hooks` is true, `pre-stop` hooks are not run, so a broken hook
    /// cannot block the reset.
    ///
    /// # Errors
    ///
    /// Returns an error if a `pre-stop` hook fails (unless `skip_hooks`), if
    /// stopping a service, removing a named volume, or deleting the state
    /// directory fails.
    pub async fn clean(&mut self, eph: &EphFile, skip_hooks: bool) -> Result<CleanSummary> {
        let mut summary = CleanSummary::default();

        // Snapshot running services once so pre-stop hooks see the full
        // environment as it was before teardown began.
        let running = self.status().await?;

        for (name, service) in &eph.services {
            // Stop and remove the underlying resource for this service.
            self.stop_service(name, service, true, eph, &running, skip_hooks)
                .await?;
            summary.services_removed += 1;

            // Remove per-workspace named volumes. A volume entry is a named
            // volume (not a bind mount) when its host part does not begin with
            // "." or "/". The real Docker volume name is derived via
            // Workspace::volume_name(service, base).
            for volume in &service.volumes {
                let base = volume.split_once(':').map(|(b, _)| b).unwrap_or(volume);
                if base.starts_with('.') || base.starts_with('/') {
                    continue; // bind mount, not a managed named volume
                }
                let volume_name = self.workspace.volume_name(name, base);
                self.docker.remove_volume(&volume_name).await?;
                summary.volumes_removed += 1;
            }
        }

        // Clear in-memory state.
        self.state.services.clear();
        self.state.processes.clear();

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
            // Compose services are not named `eph-<id>-<name>`; detect them by
            // their recorded `compose:<project>` id and check the compose
            // project's liveness by label instead of by container name. Without
            // this they would never appear in `status` and their ports could not
            // be interpolated into `eph env`.
            if let Some(project) = entry.container_id.strip_prefix("compose:") {
                if self.docker.compose_project_running(project).await? {
                    result.insert(
                        name.clone(),
                        RunningService {
                            name: name.clone(),
                            container_id: entry.container_id.clone(),
                            ports: entry.ports.clone(),
                        },
                    );
                }
                continue;
            }

            // run= services are tracked in state.processes and reported by the
            // process loop below; their id is `pid:<n>`, which never names a
            // real eph container, so skip the Docker lookup for them rather than
            // making a wasted call that could also match an unrelated container.
            if entry.container_id.starts_with("pid:") {
                continue;
            }

            let container_name = self.workspace.container_name(name);
            if let Some(info) = self.docker.get_container(&container_name).await?
                && info.is_running
            {
                // Use saved state's ports (which have proper names) instead of docker's
                result.insert(
                    name.clone(),
                    RunningService {
                        name: name.clone(),
                        container_id: info.id,
                        ports: entry.ports.clone(),
                    },
                );
            }
        }

        // Check shell command processes
        for (name, &pid) in &self.state.processes {
            // Check if process is still running
            let output = TokioCommand::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .await;

            if output.is_ok_and(|o| o.status.success()) {
                // Process is running - get ports from state
                if let Some(entry) = self.state.services.get(name) {
                    result.insert(
                        name.clone(),
                        RunningService {
                            name: name.clone(),
                            container_id: format!("pid:{}", pid),
                            ports: entry.ports.clone(),
                        },
                    );
                }
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

        // Dump the existing contents (last N lines when --tail is set), then
        // remember where the file ends so --follow only prints what is appended
        // afterwards. The offset is the file length, independent of how many
        // bytes we chose to print, so tail + follow line up.
        let initial = read_tail(&path, opts.tail)?;
        out.write_all(initial.as_bytes())
            .context("failed to write logs to stdout")?;
        out.flush().ok();

        if !opts.follow {
            return Ok(());
        }

        let mut offset = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat log file: {}", path.display()))?
            .len();

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
                let mut buf = Vec::new();
                let read = file
                    .read_to_end(&mut buf)
                    .context("failed to read log file")?;
                offset += read as u64;
                out.write_all(&buf)
                    .context("failed to write logs to stdout")?;
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

    /// The environment a non-service command (`eph run`) inherits from eph: the
    /// resolved top-level `.eph` variables plus the `EPH_*` metadata variables.
    ///
    /// This is the same connection environment `eph env` emits, augmented with
    /// metadata, so an arbitrary command can reach the running services exactly
    /// as a developer's shell would after `eval "$(eph env)"`.
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

    /// The environment overlaid on a lifecycle hook (`post-start` / `pre-stop`).
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
        env.extend(service.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }

    /// Run a hook command in the workspace directory with `env` overlaid on
    /// eph's own environment.
    ///
    /// The child inherits eph's process environment; the `env` pairs are set on
    /// top of it, so later entries (the owning service's `env.X`) win over the
    /// resolved top-level variables they may shadow.
    async fn run_hook(&self, cmd: &str, env: &[(String, String)]) -> Result<()> {
        let output = TokioCommand::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace.path)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .await
            .with_context(|| format!("failed to execute hook: {}", cmd))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("hook failed: {}\n{}", cmd, stderr);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: Option<&str>, container_port: u16) -> PortMapping {
        PortMapping {
            name: name.map(str::to_string),
            container_port,
        }
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

    #[test]
    fn read_tail_returns_whole_file_when_no_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();

        assert_eq!(read_tail(&path, None).unwrap(), "line1\nline2\nline3\n");
    }

    #[test]
    fn read_tail_trims_to_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        assert_eq!(read_tail(&path, Some(2)).unwrap(), "d\ne\n");
    }

    #[test]
    fn read_tail_more_lines_than_file_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "only\ntwo\n").unwrap();

        assert_eq!(read_tail(&path, Some(100)).unwrap(), "only\ntwo\n");
    }

    #[test]
    fn read_tail_empty_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "").unwrap();

        assert_eq!(read_tail(&path, None).unwrap(), "");
        assert_eq!(read_tail(&path, Some(5)).unwrap(), "");
    }

    #[test]
    fn apply_tail_keeps_last_n_or_all() {
        let lines = || vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(apply_tail(lines(), None), vec!["a", "b", "c"]);
        assert_eq!(apply_tail(lines(), Some(2)), vec!["b", "c"]);
        assert_eq!(apply_tail(lines(), Some(0)), Vec::<String>::new());
        // More than present returns all.
        assert_eq!(apply_tail(lines(), Some(10)), vec!["a", "b", "c"]);
    }

    #[test]
    fn read_tail_decodes_invalid_utf8_lossily() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        // An invalid byte (0xFF) must not abort the read.
        std::fs::write(&path, [b'o', b'k', 0xFF, b'\n']).unwrap();

        let out = read_tail(&path, None).unwrap();
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
}
