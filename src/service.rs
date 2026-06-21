//! Service management - starting, stopping, and managing Docker containers

use crate::parser::{EphFile, Service, ServiceSource};
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StartContainerOptions, StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command as TokioCommand;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

// ============================================================================
// Running Service Info
// ============================================================================

/// Runtime information about a running service
#[derive(Debug, Clone)]
pub struct RunningService {
    #[allow(dead_code)]
    pub name: String,
    pub container_id: String,
    /// Map of port name (or "default") to assigned host port
    pub ports: HashMap<String, u16>,
}

impl RunningService {
    /// Get the host for this service (always localhost for now)
    pub fn host(&self) -> &str {
        "localhost"
    }

    /// Get the primary port (first port or named "default")
    pub fn port(&self) -> Option<u16> {
        self.ports
            .get("default")
            .copied()
            .or_else(|| self.ports.values().next().copied())
    }

    /// Get a named port
    pub fn named_port(&self, name: &str) -> Option<u16> {
        self.ports.get(name).copied()
    }
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
pub struct ServiceState {
    /// Running services keyed by service name
    pub services: HashMap<String, ServiceStateEntry>,
    /// Process IDs for shell command services
    #[serde(default)]
    pub processes: HashMap<String, u32>,
}

/// State entry for a single service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStateEntry {
    pub container_id: String,
    pub ports: HashMap<String, u16>,
}

impl ServiceState {
    /// Load state from disk
    pub async fn load(workspace: &Workspace) -> Result<Self> {
        let path = state_file_path(workspace)?;

        if !path.exists() {
            return Ok(ServiceState::default());
        }

        let contents = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("Failed to read state file: {}", path.display()))?;

        let state: ServiceState = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse state file: {}", path.display()))?;

        Ok(state)
    }

    /// Save state to disk
    pub async fn save(&self, workspace: &Workspace) -> Result<()> {
        let path = state_file_path(workspace)?;

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create state directory: {}", parent.display())
            })?;
        }

        let contents = serde_json::to_string_pretty(self).context("Failed to serialize state")?;

        tokio::fs::write(&path, contents)
            .await
            .with_context(|| format!("Failed to write state file: {}", path.display()))?;

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
pub struct ContainerInfo {
    pub id: String,
    pub is_running: bool,
    pub ports: HashMap<String, u16>,
}

/// Docker client wrapper
pub struct DockerClient {
    client: Docker,
}

impl DockerClient {
    /// Connect to Docker daemon
    pub async fn connect() -> Result<Self> {
        let client = Docker::connect_with_local_defaults()
            .context("Failed to connect to Docker. Is Docker running?")?;

        // Verify connection
        client
            .ping()
            .await
            .context("Failed to ping Docker daemon")?;

        Ok(DockerClient { client })
    }

    /// Get information about a container by name
    pub async fn get_container(&self, name: &str) -> Result<Option<ContainerInfo>> {
        let filters: HashMap<String, Vec<String>> =
            HashMap::from([("name".to_string(), vec![name.to_string()])]);

        let containers = self
            .client
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .context("Failed to list containers")?;

        // Find exact match (Docker's name filter is a prefix match)
        let container = containers.into_iter().find(|c| {
            c.names
                .as_ref()
                .is_some_and(|names| names.iter().any(|n| n == &format!("/{}", name)))
        });

        let Some(container) = container else {
            return Ok(None);
        };

        let is_running = container.state.as_ref().is_some_and(|s| s == "running");

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

    /// Start an existing container
    pub async fn start_container(&self, id: &str) -> Result<()> {
        self.client
            .start_container(id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start container")?;
        Ok(())
    }

    /// Stop a container
    pub async fn stop_container(&self, name: &str) -> Result<()> {
        if let Some(info) = self.get_container(name).await?
            && info.is_running
        {
            info!("Stopping container {}", name);
            self.client
                .stop_container(&info.id, Some(StopContainerOptions { t: 10 }))
                .await
                .context("Failed to stop container")?;
        }
        Ok(())
    }

    /// Remove a container
    pub async fn remove_container(&self, name: &str) -> Result<()> {
        if let Some(info) = self.get_container(name).await? {
            info!("Removing container {}", name);
            self.client
                .remove_container(
                    &info.id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
                .context("Failed to remove container")?;
        }
        Ok(())
    }

    /// Remove a named volume, ignoring "not found" errors
    pub async fn remove_volume(&self, name: &str) -> Result<()> {
        use bollard::errors::Error as BollardError;
        use bollard::volume::RemoveVolumeOptions;

        info!("Removing volume {}", name);
        match self
            .client
            .remove_volume(name, Some(RemoveVolumeOptions { force: true }))
            .await
        {
            Ok(()) => Ok(()),
            // Volume already gone (or never created) - treat as success.
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to remove volume {}", name)),
        }
    }

    /// Execute a command inside a running container
    pub async fn exec_in_container(&self, container_id: &str, cmd: &[&str]) -> Result<i64> {
        use bollard::exec::{CreateExecOptions, StartExecResults};

        let exec = self
            .client
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(cmd.to_vec()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("Failed to create exec")?;

        let start_result = self
            .client
            .start_exec(&exec.id, None)
            .await
            .context("Failed to start exec")?;

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
            .context("Failed to inspect exec")?;

        Ok(inspect.exit_code.unwrap_or(-1))
    }

    /// Pull an image and run it as a container
    pub async fn run_image(
        &self,
        container_name: &str,
        image: &str,
        service: &Service,
        workspace: &Workspace,
    ) -> Result<RunningService> {
        // Pull image if needed
        self.ensure_image(image).await?;

        // Build port bindings - let Docker assign host ports
        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings: HashMap<String, Option<Vec<bollard::service::PortBinding>>> =
            HashMap::new();

        for port_mapping in &service.ports {
            let container_port = format!("{}/tcp", port_mapping.container_port);
            exposed_ports.insert(container_port.clone(), HashMap::new());
            // Empty host port = random assignment
            port_bindings.insert(
                container_port,
                Some(vec![bollard::service::PortBinding {
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

        let host_config = bollard::service::HostConfig {
            port_bindings: Some(port_bindings),
            binds: Some(binds),
            ..Default::default()
        };

        // Handle command override
        let cmd = service
            .command_override
            .as_ref()
            .map(|c| shell_words::split(c).unwrap_or_else(|_| vec![c.clone()]));

        let config = Config {
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
                    name: container_name,
                    platform: None,
                }),
                config,
            )
            .await
            .context("Failed to create container")?;

        // Start container
        self.client
            .start_container(&response.id, None::<StartContainerOptions<String>>)
            .await
            .context("Failed to start container")?;

        // Get assigned ports
        let info = self
            .get_container(container_name)
            .await?
            .context("Container disappeared after creation")?;

        // Map port names
        let mut named_ports = HashMap::new();
        for port_mapping in &service.ports {
            let key = port_mapping.container_port.to_string();
            if let Some(&host_port) = info.ports.get(&key) {
                let name = port_mapping
                    .name
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                named_ports.insert(name, host_port);
            }
        }

        Ok(RunningService {
            name: service.name.clone(),
            container_id: response.id,
            ports: named_ports,
        })
    }

    /// Build from Dockerfile and run
    pub async fn build_and_run(
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
            .context("Failed to run docker build")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Docker build failed:\n{}", stderr);
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
            Some(CreateImageOptions {
                from_image: image,
                ..Default::default()
            }),
            None,
            None,
        );

        while let Some(result) = stream.next().await {
            result.context("Failed to pull image")?;
        }

        Ok(())
    }
}

// ============================================================================
// Service Manager
// ============================================================================

/// Manager for all services in a workspace
pub struct ServiceManager {
    workspace: Workspace,
    docker: DockerClient,
    state: ServiceState,
}

impl ServiceManager {
    /// Create a new service manager for a workspace
    pub async fn new(workspace: Workspace) -> Result<Self> {
        let docker = DockerClient::connect().await?;
        let state = ServiceState::load(&workspace).await?;
        Ok(ServiceManager {
            workspace,
            docker,
            state,
        })
    }

    /// Start all services defined in the .eph file
    pub async fn start_all(&mut self, eph: &EphFile) -> Result<HashMap<String, RunningService>> {
        let mut running = HashMap::new();

        for (name, service) in &eph.services {
            let result = self.start_service(name, service).await?;
            running.insert(name.clone(), result);
        }

        // Save state
        self.state.save(&self.workspace).await?;

        Ok(running)
    }

    /// Start a single service
    pub async fn start_service(&mut self, name: &str, service: &Service) -> Result<RunningService> {
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
                // Record in state even for already-running containers
                self.state.services.insert(
                    name.to_string(),
                    ServiceStateEntry {
                        container_id: existing.id.clone(),
                        ports: existing.ports.clone(),
                    },
                );
                return Ok(RunningService {
                    name: name.to_string(),
                    container_id: existing.id,
                    ports: existing.ports,
                });
            } else {
                // Container exists but not running, start it
                info!("Starting existing container for {}", name);
                self.docker.start_container(&existing.id).await?;
                let refreshed = self
                    .docker
                    .get_container(&container_name)
                    .await?
                    .context("Container disappeared after start")?;

                // Wait for health check
                self.wait_for_healthy(name, service, &refreshed.id).await?;

                // Record in state
                self.state.services.insert(
                    name.to_string(),
                    ServiceStateEntry {
                        container_id: refreshed.id.clone(),
                        ports: refreshed.ports.clone(),
                    },
                );

                return Ok(RunningService {
                    name: name.to_string(),
                    container_id: refreshed.id,
                    ports: refreshed.ports,
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
            ServiceSource::None => {
                bail!("Service {} has no source defined", name);
            }
        };

        // Record in state
        self.state.services.insert(
            name.to_string(),
            ServiceStateEntry {
                container_id: running.container_id.clone(),
                ports: running.ports.clone(),
            },
        );

        // Run post-start hooks
        if !service.post_start.is_empty() {
            info!("Running post-start hooks for {}", name);
            for cmd in &service.post_start {
                self.run_hook(cmd).await?;
            }
        }

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
                "Service {} failed to become healthy within {}s",
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

        // Start the process
        let child = TokioCommand::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace.path)
            .envs(&env_vars)
            .spawn()
            .with_context(|| format!("Failed to start command: {}", cmd))?;

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
            .context("Failed to run docker compose")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Docker compose failed:\n{}", stderr);
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

    /// Stop all services. When `remove` is true, also remove containers
    /// (and compose resources) so they do not accumulate.
    pub async fn stop_all(&mut self, eph: &EphFile, remove: bool) -> Result<()> {
        for (name, service) in &eph.services {
            self.stop_service(name, service, remove).await?;
        }
        self.state.services.clear();
        self.state.processes.clear();
        self.state.save(&self.workspace).await?;
        Ok(())
    }

    /// Stop a single service. When `remove` is true, also remove the underlying
    /// container after stopping it (compose uses `down`, which already removes
    /// containers; killing a shell command process already removes it).
    pub async fn stop_service(
        &mut self,
        name: &str,
        service: &Service,
        remove: bool,
    ) -> Result<()> {
        // Run pre-stop hooks
        if !service.pre_stop.is_empty() {
            info!("Running pre-stop hooks for {}", name);
            for cmd in &service.pre_stop {
                if let Err(e) = self.run_hook(cmd).await {
                    warn!("Pre-stop hook failed: {}", e);
                }
            }
        }

        match &service.source {
            ServiceSource::Command(_) => {
                // Kill the process
                if let Some(&pid) = self.state.processes.get(name) {
                    info!("Stopping process {} (PID {})", name, pid);
                    // Send SIGTERM
                    let _ = TokioCommand::new("kill")
                        .arg(pid.to_string())
                        .output()
                        .await;
                    // Wait a bit then SIGKILL if needed
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
    pub async fn clean(&mut self, eph: &EphFile) -> Result<CleanSummary> {
        let mut summary = CleanSummary::default();

        for (name, service) in &eph.services {
            // Stop and remove the underlying resource for this service.
            self.stop_service(name, service, true).await?;
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
                    format!("Failed to remove state directory: {}", state_dir.display())
                })?;
            summary.state_removed = true;
        }

        Ok(summary)
    }

    /// Save the current state to disk
    pub async fn save_state(&self) -> Result<()> {
        self.state.save(&self.workspace).await
    }

    /// Get currently running services
    pub async fn status(&self) -> Result<HashMap<String, RunningService>> {
        let mut result = HashMap::new();

        for (name, entry) in &self.state.services {
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

    /// Run a hook command in the workspace directory
    async fn run_hook(&self, cmd: &str) -> Result<()> {
        let output = TokioCommand::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace.path)
            .output()
            .await
            .with_context(|| format!("Failed to execute hook: {}", cmd))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Hook failed: {}\n{}", cmd, stderr);
        }

        Ok(())
    }
}
