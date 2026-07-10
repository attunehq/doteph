//! Cross-workspace pruning for state left behind by deleted workspaces.
//!
//! Normal lifecycle commands start from the current `.eph` file. Prune starts
//! from the global state root instead, so it can tear down resources for a
//! workspace path that no longer exists.

use crate::proc;
use crate::service::{Backend, ServiceState};
use crate::workspace::{WORKSPACE_METADATA_FILE, WorkspaceMetadata, state_root};
use anyhow::{Context, Result};
use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::models::{ContainerSummary, ContainerSummaryStateEnum};
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, ListNetworksOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder,
};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::sleep;

/// Options for [`prune`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneOptions {
    /// Print what would be removed without deleting Docker resources or state.
    pub dry_run: bool,
    /// Prune state directories written by eph v0.4.2 and earlier.
    pub compatibility_v042: bool,
    /// Remove a stale workspace's resources even when it still has running
    /// containers or a live `run=` process. Without this, a workspace that
    /// reads as stale only because it was moved or renamed (its recorded path
    /// no longer resolves) is reported and skipped instead of force-killed.
    pub force_live: bool,
}

/// Whether `eph system prune`'s confirmation prompt should be shown, skipped,
/// or refused for a real (non-dry-run) prune.
///
/// This is a plain function over booleans, not a method that reads
/// `std::io::stdin()` itself, so the CLI layer's terminal check and its
/// decision of what to do with that check are two different, independently
/// testable things: this one needs no real terminal or Docker daemon to
/// exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationOutcome {
    /// Nothing would be removed, or `--yes` was passed: proceed without
    /// asking.
    Proceed,
    /// Show the "Remove these resources? [y/N]" prompt on stdin.
    Prompt,
    /// stdin is not a terminal and `--yes` was not passed, so there is no way
    /// to ask and no consent to assume: refuse until the caller passes
    /// `--yes`.
    RequireYes,
}

/// Decide [`ConfirmationOutcome`] for a real prune.
///
/// `docker system prune` always confirms before deleting anything; this
/// mirrors that default while still letting scripts (`--yes`) and dry runs
/// (`would_remove == false` once nothing is left to remove) skip the prompt.
#[must_use]
pub fn confirmation_outcome(
    would_remove: bool,
    yes: bool,
    stdin_is_terminal: bool,
) -> ConfirmationOutcome {
    if !would_remove || yes {
        ConfirmationOutcome::Proceed
    } else if stdin_is_terminal {
        ConfirmationOutcome::Prompt
    } else {
        ConfirmationOutcome::RequireYes
    }
}

/// The reason a metadata-backed workspace is considered stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// The recorded workspace path no longer exists.
    Missing,
    /// The recorded workspace path exists but is now an empty directory.
    EmptyDirectory,
    /// The recorded workspace path exists but is no longer a directory.
    NotDirectory,
    /// The state directory was written before eph recorded workspace metadata.
    CompatibilityV042State,
}

impl StaleReason {
    fn label(self) -> &'static str {
        match self {
            StaleReason::Missing => "missing workspace",
            StaleReason::EmptyDirectory => "empty workspace directory",
            StaleReason::NotDirectory => "workspace path is not a directory",
            StaleReason::CompatibilityV042State => {
                "v0.4.2-and-earlier state without workspace metadata"
            }
        }
    }
}

impl std::fmt::Display for StaleReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Counts of resources removed, or that would be removed during a dry run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneCounts {
    /// Docker containers removed.
    pub containers: usize,
    /// Docker volumes removed.
    pub volumes: usize,
    /// Docker images removed.
    pub images: usize,
    /// Docker networks removed.
    pub networks: usize,
    /// Verified `run=` process trees terminated.
    pub processes: usize,
    /// State directories removed.
    pub state_dirs: usize,
}

impl PruneCounts {
    fn add(&mut self, other: &PruneCounts) {
        self.containers += other.containers;
        self.volumes += other.volumes;
        self.images += other.images;
        self.networks += other.networks;
        self.processes += other.processes;
        self.state_dirs += other.state_dirs;
    }

    /// Whether all counts are zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.containers == 0
            && self.volumes == 0
            && self.images == 0
            && self.networks == 0
            && self.processes == 0
            && self.state_dirs == 0
    }
}

/// A stale workspace that prune removed or would remove.
#[derive(Debug, Clone)]
pub struct PrunedWorkspace {
    /// Workspace short ID, the namespace used in Docker resource names.
    pub short_id: String,
    /// Recorded workspace path, when metadata exists.
    pub workspace_path: Option<PathBuf>,
    /// Why the workspace was selected.
    pub reason: StaleReason,
    /// Resource counts removed for this workspace.
    pub counts: PruneCounts,
}

/// A state directory left alone by prune.
#[derive(Debug, Clone)]
pub struct SkippedWorkspace {
    /// Workspace short ID, when it could be read from the state directory name.
    pub short_id: String,
    /// Recorded workspace path, when metadata exists.
    pub workspace_path: Option<PathBuf>,
    /// Human-readable reason for skipping.
    pub reason: String,
}

/// Summary returned by [`prune`].
#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Stale workspaces removed, or that would be removed during a dry run.
    pub pruned: Vec<PrunedWorkspace>,
    /// Workspaces or state directories left untouched.
    pub skipped: Vec<SkippedWorkspace>,
    /// Non-fatal warnings, including unsafe `run=` process prune skips.
    pub warnings: Vec<String>,
    /// Total resource counts across [`pruned`](Self::pruned).
    pub totals: PruneCounts,
}

/// Remove resources for metadata-backed workspaces whose recorded path is gone
/// or empty.
///
/// # Errors
///
/// Returns an error if the state root cannot be read, Docker cannot be reached,
/// or a Docker/filesystem removal fails. Individual malformed state directories
/// are reported as warnings and skipped.
pub async fn prune(options: PruneOptions) -> Result<PruneReport> {
    let root = state_root()?;
    let mut prune_lock = open_prune_lock(&root)?;
    let _lock = prune_lock.try_write().map_err(|err| {
        let path = root.join("prune.lock");
        if err.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!(
                "failed to acquire prune lock at {}; another prune may be running",
                path.display()
            )
        } else {
            anyhow::Error::new(err).context(format!(
                "failed to acquire prune lock at {}",
                path.display()
            ))
        }
    })?;
    let docker = Docker::connect_with_local_defaults()
        .context("failed to connect to docker (is docker running?)")?;
    docker
        .ping()
        .await
        .context("failed to ping docker daemon")?;

    let mut report = PruneReport {
        dry_run: options.dry_run,
        ..PruneReport::default()
    };

    let state_dirs = state_dirs(&root).await?;
    for state_dir in state_dirs {
        inspect_state_dir(&docker, &state_dir, options, &mut report).await?;
    }

    Ok(report)
}

async fn inspect_state_dir(
    docker: &Docker,
    state_dir: &Path,
    options: PruneOptions,
    report: &mut PruneReport,
) -> Result<()> {
    let short_id = state_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string());
    if !is_workspace_short_id(&short_id) {
        report.skipped.push(SkippedWorkspace {
            short_id,
            workspace_path: None,
            reason: "state directory name is not an eph workspace short ID".to_string(),
        });
        return Ok(());
    }
    let metadata_path = state_dir.join(WORKSPACE_METADATA_FILE);

    if !metadata_path.exists() {
        if options.compatibility_v042 {
            if let Some(pruned) = prune_workspace(
                docker,
                state_dir,
                short_id,
                None,
                StaleReason::CompatibilityV042State,
                options,
                report,
            )
            .await?
            {
                report.totals.add(&pruned.counts);
                report.pruned.push(pruned);
            }
        } else {
            report.skipped.push(SkippedWorkspace {
                short_id,
                workspace_path: None,
                reason:
                    "v0.4.2-and-earlier state has no workspace metadata; pass --compatibility-v042 to prune it"
                        .to_string(),
            });
        }
        return Ok(());
    }

    let metadata = match WorkspaceMetadata::load_from_state_dir(state_dir).await {
        Ok(metadata) => metadata,
        Err(err) => {
            report.skipped.push(SkippedWorkspace {
                short_id,
                workspace_path: None,
                reason: format!("{err:#}"),
            });
            return Ok(());
        }
    };

    if metadata.short_id != short_id {
        report.skipped.push(SkippedWorkspace {
            short_id,
            workspace_path: Some(metadata.workspace_path),
            reason: format!(
                "metadata short ID does not match state directory name ({})",
                metadata.short_id
            ),
        });
        return Ok(());
    }

    let Some(reason) = classify_workspace_path(&metadata.workspace_path)? else {
        report.skipped.push(SkippedWorkspace {
            short_id: metadata.short_id,
            workspace_path: Some(metadata.workspace_path),
            reason: "workspace still exists and is not empty".to_string(),
        });
        return Ok(());
    };

    if !options.dry_run && !metadata_still_stale(state_dir, &metadata).await? {
        report.skipped.push(SkippedWorkspace {
            short_id: metadata.short_id,
            workspace_path: Some(metadata.workspace_path),
            reason: "workspace metadata changed during prune".to_string(),
        });
        return Ok(());
    }

    if let Some(pruned) = prune_workspace(
        docker,
        state_dir,
        metadata.short_id,
        Some(metadata.workspace_path),
        reason,
        options,
        report,
    )
    .await?
    {
        report.totals.add(&pruned.counts);
        report.pruned.push(pruned);
    }
    Ok(())
}

/// Remove a stale workspace's resources, or report and skip it when it turns
/// out not to be as dead as its recorded path suggests.
///
/// Staleness is judged purely by the recorded workspace *path*
/// ([`classify_workspace_path`]); a workspace that was moved or renamed while
/// its services still run reads exactly the same as one that is truly gone.
/// So before removing anything, this checks the workspace's actual Docker
/// containers and `run=` processes for signs of life. Live resources block
/// the prune (reported via [`PruneReport::skipped`]) unless
/// `options.force_live` opts back into the old, unguarded behavior. This
/// applies during `--dry-run` too, so the preview shown before the
/// confirmation prompt matches what a real run would do.
///
/// Returns `Ok(None)` when the workspace was skipped for liveness rather than
/// pruned.
async fn prune_workspace(
    docker: &Docker,
    state_dir: &Path,
    short_id: String,
    workspace_path: Option<PathBuf>,
    reason: StaleReason,
    options: PruneOptions,
    report: &mut PruneReport,
) -> Result<Option<PrunedWorkspace>> {
    let prefix = format!("eph-{short_id}-");

    let state = load_state_or_warn(state_dir, &short_id, report).await;
    let live_processes = state.as_ref().map_or(0, count_live_processes);
    let containers = matching_containers(docker, &prefix).await?;
    let running_containers = count_running_containers(&containers);

    if blocks_prune(running_containers, live_processes, options.force_live) {
        report.skipped.push(SkippedWorkspace {
            short_id,
            workspace_path,
            reason: format!(
                "{reason} but has {}; stop them or re-run with --force-live",
                live_resource_summary(running_containers, live_processes)
            ),
        });
        return Ok(None);
    }

    let mut counts = PruneCounts::default();

    if let Some(state) = state {
        terminate_live_processes(state, &short_id, options.dry_run, report, &mut counts).await;
    }
    counts.containers = remove_containers(docker, containers, options.dry_run).await?;
    counts.volumes = remove_volumes(docker, &prefix, options.dry_run).await?;
    counts.networks = remove_networks(docker, &prefix, options.dry_run).await?;
    counts.images = remove_images(docker, &prefix, options.dry_run).await?;

    if state_dir.exists() {
        counts.state_dirs = 1;
        if !options.dry_run {
            tokio::fs::remove_dir_all(state_dir)
                .await
                .with_context(|| {
                    format!("failed to remove state directory: {}", state_dir.display())
                })?;
        }
    }

    Ok(Some(PrunedWorkspace {
        short_id,
        workspace_path,
        reason,
        counts,
    }))
}

/// Whether a stale-pathed workspace has live resources that block a default
/// prune: a running container, a live `run=` process, or both. `force_live`
/// overrides the guard entirely, restoring the old unconditional behavior.
///
/// Pulled out of [`prune_workspace`] as a plain function over counts (rather
/// than the Docker/process-table calls that produce them) so the decision
/// itself is exercised by a unit test with no Docker daemon involved.
fn blocks_prune(running_containers: usize, live_processes: usize, force_live: bool) -> bool {
    !force_live && (running_containers > 0 || live_processes > 0)
}

/// Describe a positive count of running containers and/or live `run=`
/// processes for a [`SkippedWorkspace`] reason. Only called once at least one
/// of the two counts is non-zero.
fn live_resource_summary(running_containers: usize, live_processes: usize) -> String {
    let mut parts = Vec::new();
    if running_containers > 0 {
        parts.push(format!(
            "{running_containers} running container{}",
            if running_containers == 1 { "" } else { "s" }
        ));
    }
    if live_processes > 0 {
        parts.push(format!(
            "{live_processes} live run= process{}",
            if live_processes == 1 { "" } else { "es" }
        ));
    }
    parts.join(" and ")
}

/// Load `state_dir`'s `state.json`, warning and returning `None` if it cannot
/// be read or parsed. A missing file (a workspace with no `run=` services)
/// also returns `None`, silently: that is the common case, not a problem.
async fn load_state_or_warn(
    state_dir: &Path,
    short_id: &str,
    report: &mut PruneReport,
) -> Option<ServiceState> {
    match load_state(state_dir).await {
        Ok(state) => state,
        Err(err) => {
            report.warnings.push(format!(
                "{short_id}: could not read state.json, so run= process prune was skipped: {err:#}"
            ));
            None
        }
    }
}

/// Count `state`'s `run=` services whose recorded PID still names the exact
/// process eph launched (see [`proc::identity_matches`]). This is the pure
/// half of the liveness check: given an already-loaded [`ServiceState`], no
/// process table is touched here beyond what `identity_matches` itself
/// queries by PID, and no Docker or filesystem I/O happens at all.
fn count_live_processes(state: &ServiceState) -> usize {
    state
        .services
        .values()
        .filter(|entry| {
            let Backend::Process {
                pid,
                identity: Some(identity),
            } = &entry.backend
            else {
                return false;
            };
            proc::identity_matches(*pid, identity)
        })
        .count()
}

/// Terminate every `run=` service in `state` whose recorded PID still matches
/// the identity eph captured at launch. A PID with no recorded identity, or
/// one whose live process no longer matches it, is left alone (with a
/// warning if it is still alive): the state predates identity tracking, or
/// the PID was reused by an unrelated process, and either way killing it
/// would be wrong.
async fn terminate_live_processes(
    state: ServiceState,
    short_id: &str,
    dry_run: bool,
    report: &mut PruneReport,
    counts: &mut PruneCounts,
) {
    for (name, entry) in state.services {
        let Backend::Process { pid, identity } = entry.backend else {
            continue;
        };

        let Some(identity) = identity else {
            if proc::is_alive(pid) {
                report.warnings.push(format!(
                    "{short_id}/{name}: skipped run= PID {pid}; state has no process identity"
                ));
            }
            continue;
        };

        if proc::identity_matches(pid, &identity) {
            counts.processes += 1;
            if !dry_run {
                proc::terminate(pid);
                sleep(Duration::from_secs(2)).await;
                proc::force_kill(pid);
            }
        } else if proc::is_alive(pid) {
            report.warnings.push(format!(
                "{short_id}/{name}: skipped run= PID {pid}; the live process does not match recorded identity"
            ));
        }
    }
}

async fn load_state(state_dir: &Path) -> Result<Option<ServiceState>> {
    let path = state_dir.join("state.json");
    if !path.exists() {
        return Ok(None);
    }
    let contents = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read state file: {}", path.display()))?;
    let state = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse state file: {}", path.display()))?;
    Ok(Some(state))
}

/// List containers whose name carries `prefix`, eph's `eph-<short_id>-`
/// namespace. Fetched once per workspace and shared by the liveness check
/// ([`count_running_containers`]) and the actual removal
/// ([`remove_containers`]), so the two agree on exactly which containers
/// exist rather than risking a container starting or stopping between two
/// separate `docker ps` calls.
async fn matching_containers(docker: &Docker, prefix: &str) -> Result<Vec<ContainerSummary>> {
    let containers = docker
        .list_containers(Some(ListContainersOptionsBuilder::new().all(true).build()))
        .await
        .context("failed to list containers")?;
    Ok(containers
        .into_iter()
        .filter(|container| {
            container.names.as_ref().is_some_and(|names| {
                names
                    .iter()
                    .any(|name| docker_name_has_prefix(name, prefix))
            })
        })
        .collect())
}

/// Count `containers` currently in Docker's `running` state, the liveness
/// signal for a workspace whose recorded path no longer resolves.
fn count_running_containers(containers: &[ContainerSummary]) -> usize {
    containers
        .iter()
        .filter(|container| matches!(container.state, Some(ContainerSummaryStateEnum::RUNNING)))
        .count()
}

async fn remove_containers(
    docker: &Docker,
    containers: Vec<ContainerSummary>,
    dry_run: bool,
) -> Result<usize> {
    let mut removed = 0;

    for container in containers {
        removed += 1;
        if dry_run {
            continue;
        }
        let Some(id) = container.id else {
            continue;
        };
        docker
            .remove_container(
                &id,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await
            .or_else(ignore_not_found)
            .context("failed to remove container")?;
    }

    Ok(removed)
}

async fn remove_volumes(docker: &Docker, prefix: &str, dry_run: bool) -> Result<usize> {
    let volumes = docker
        .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>)
        .await
        .context("failed to list volumes")?;
    let mut removed = 0;

    for volume in volumes.volumes.unwrap_or_default() {
        if !volume.name.starts_with(prefix) {
            continue;
        }
        removed += 1;
        if dry_run {
            continue;
        }
        docker
            .remove_volume(
                &volume.name,
                Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
            )
            .await
            .or_else(ignore_not_found)
            .with_context(|| format!("failed to remove volume {}", volume.name))?;
    }

    Ok(removed)
}

async fn remove_networks(docker: &Docker, prefix: &str, dry_run: bool) -> Result<usize> {
    let networks = docker
        .list_networks(Some(ListNetworksOptionsBuilder::default().build()))
        .await
        .context("failed to list networks")?;
    let mut removed = 0;

    for network in networks {
        let Some(name) = network.name else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        removed += 1;
        if dry_run {
            continue;
        }
        docker
            .remove_network(&name)
            .await
            .or_else(ignore_not_found)
            .with_context(|| format!("failed to remove network {name}"))?;
    }

    Ok(removed)
}

async fn remove_images(docker: &Docker, prefix: &str, dry_run: bool) -> Result<usize> {
    let images = docker
        .list_images(Some(ListImagesOptionsBuilder::default().all(true).build()))
        .await
        .context("failed to list images")?;
    let mut removed = 0;

    for image in images {
        let Some(tag) = image
            .repo_tags
            .iter()
            .find(|tag| {
                tag.strip_suffix(":latest")
                    .unwrap_or(tag)
                    .starts_with(prefix)
            })
            .cloned()
        else {
            continue;
        };

        removed += 1;
        if dry_run {
            continue;
        }
        docker
            .remove_image(
                &tag,
                Some(RemoveImageOptionsBuilder::default().force(true).build()),
                None,
            )
            .await
            .or_else(ignore_not_found)
            .with_context(|| format!("failed to remove image {tag}"))?;
    }

    Ok(removed)
}

fn docker_name_has_prefix(name: &str, prefix: &str) -> bool {
    name.strip_prefix('/').unwrap_or(name).starts_with(prefix)
}

fn is_workspace_short_id(value: &str) -> bool {
    matches!(value.len(), 8 | 16) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ignore_not_found<T: Default>(err: BollardError) -> std::result::Result<T, BollardError> {
    match err {
        BollardError::DockerResponseServerError {
            status_code: 404, ..
        } => Ok(T::default()),
        other => Err(other),
    }
}

/// Count metadata-backed workspaces under `root` whose recorded path no longer
/// resolves to a real, non-empty directory, not counting `exclude_short_id`.
///
/// This is the passive nudge `eph up` prints toward `eph system prune`: a
/// filesystem-only scan that mirrors the classification `prune` itself does
/// (reusing [`state_dirs`], [`WorkspaceMetadata::load_from_state_dir`], and
/// [`classify_workspace_path`]) but never touches Docker, so it is cheap
/// enough to run on every `up`. A directory whose name is not an eph workspace
/// short ID, that carries no metadata, or whose metadata cannot be read is
/// skipped silently, exactly as `prune` skips it. `exclude_short_id` is the
/// current workspace's own short ID, so `up` never nudges about itself.
///
/// Never errors: a stale-workspace count must never turn a successful `up`
/// into a failure, so an unreadable state root reads as zero rather than
/// propagating.
pub async fn count_stale_workspaces(root: &Path, exclude_short_id: &str) -> usize {
    let Ok(dirs) = state_dirs(root).await else {
        return 0;
    };

    let mut count = 0;
    for dir in dirs {
        let Some(short_id) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if !is_workspace_short_id(&short_id) || short_id == exclude_short_id {
            continue;
        }
        let Ok(metadata) = WorkspaceMetadata::load_from_state_dir(&dir).await else {
            continue;
        };
        let is_stale = classify_workspace_path(&metadata.workspace_path)
            .ok()
            .flatten()
            .is_some();
        if is_stale {
            count += 1;
        }
    }
    count
}

async fn state_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut dirs = Vec::new();
    let mut entries = tokio::fs::read_dir(root)
        .await
        .with_context(|| format!("failed to read state root: {}", root.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn classify_workspace_path(path: &Path) -> Result<Option<StaleReason>> {
    if !path.exists() {
        return Ok(Some(StaleReason::Missing));
    }
    if !path.is_dir() {
        return Ok(Some(StaleReason::NotDirectory));
    }
    if path
        .read_dir()
        .with_context(|| format!("failed to read workspace directory: {}", path.display()))?
        .next()
        .is_none()
    {
        return Ok(Some(StaleReason::EmptyDirectory));
    }
    Ok(None)
}

async fn metadata_still_stale(state_dir: &Path, original: &WorkspaceMetadata) -> Result<bool> {
    let current = WorkspaceMetadata::load_from_state_dir(state_dir).await?;
    if &current != original {
        return Ok(false);
    }
    Ok(classify_workspace_path(&current.workspace_path)?.is_some())
}

/// Open the lock file that makes `eph system prune` invocations mutually
/// exclusive, so two prunes never remove resources out from under each other.
///
/// This used to be a `create_new` file plus a `Drop` impl that deleted it:
/// whichever process created the file first held the lock, and finishing
/// (or a signal) cleaned it up. But a crash skips `Drop`, so the file, and
/// the lock, outlived the process that made it, and every later prune,
/// including `--dry-run`, failed until someone deleted it by hand.
///
/// [`fd_lock::RwLock`] is an OS advisory lock (`flock` on Unix, `LockFileEx`
/// on Windows) instead: the kernel releases it the instant the holding
/// process exits, crash or not, so a dead process can never wedge the next
/// prune. The lock file itself is still left on disk (fd-lock needs a real
/// file to hold the lock on), but that is harmless now: it is never load-
/// bearing on its own, only the OS-level lock on it is.
///
/// The caller keeps the returned lock and its `try_write` guard as two locals
/// in [`prune`], so the OS lock releases when `prune` returns. That matters
/// because `eph system prune` calls [`prune`] twice, a dry-run preview and
/// then the real pass, and the second call must be able to take the lock the
/// first one held.
fn open_prune_lock(root: &Path) -> Result<fd_lock::RwLock<File>> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("failed to create state root: {}", root.display()))?;
    let path = root.join("prune.lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open prune lock file: {}", path.display()))?;
    Ok(fd_lock::RwLock::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_missing_workspace_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            classify_workspace_path(&missing).unwrap(),
            Some(StaleReason::Missing)
        );
    }

    #[test]
    fn classifies_empty_workspace_as_stale() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            classify_workspace_path(dir.path()).unwrap(),
            Some(StaleReason::EmptyDirectory)
        );
    }

    #[test]
    fn keeps_non_empty_workspace_active() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".eph"), "[db]\nimage=postgres:16\n").unwrap();

        assert_eq!(classify_workspace_path(dir.path()).unwrap(), None);
    }

    #[test]
    fn docker_name_prefix_ignores_leading_slash() {
        assert!(docker_name_has_prefix("/eph-abcd1234-web", "eph-abcd1234-"));
        assert!(docker_name_has_prefix("eph-abcd1234-web", "eph-abcd1234-"));
        assert!(!docker_name_has_prefix(
            "/not-eph-abcd1234-web",
            "eph-abcd1234-"
        ));
    }

    #[test]
    fn workspace_short_id_accepts_current_and_legacy_hex_lengths() {
        assert!(is_workspace_short_id("a1b2c3d4"));
        assert!(is_workspace_short_id("ABCDEF12"));
        assert!(is_workspace_short_id("a1b2c3d4e5f60718"));
        assert!(!is_workspace_short_id("not-a-workspace"));
        assert!(!is_workspace_short_id("a1b2c3d"));
        assert!(!is_workspace_short_id("a1b2c3d4e5f607182"));
    }

    #[test]
    fn prune_counts_reports_empty() {
        let mut counts = PruneCounts::default();
        assert!(counts.is_empty());
        counts.state_dirs = 1;
        assert!(!counts.is_empty());
    }

    #[test]
    fn confirmation_proceeds_when_nothing_would_be_removed() {
        assert_eq!(
            confirmation_outcome(false, false, false),
            ConfirmationOutcome::Proceed
        );
        assert_eq!(
            confirmation_outcome(false, false, true),
            ConfirmationOutcome::Proceed
        );
    }

    #[test]
    fn confirmation_proceeds_with_yes_regardless_of_the_terminal() {
        assert_eq!(
            confirmation_outcome(true, true, false),
            ConfirmationOutcome::Proceed
        );
        assert_eq!(
            confirmation_outcome(true, true, true),
            ConfirmationOutcome::Proceed
        );
    }

    #[test]
    fn confirmation_prompts_on_an_interactive_terminal() {
        assert_eq!(
            confirmation_outcome(true, false, true),
            ConfirmationOutcome::Prompt
        );
    }

    #[test]
    fn confirmation_requires_yes_off_a_terminal() {
        assert_eq!(
            confirmation_outcome(true, false, false),
            ConfirmationOutcome::RequireYes
        );
    }

    #[test]
    fn blocks_prune_on_a_running_container() {
        assert!(blocks_prune(1, 0, false));
    }

    #[test]
    fn blocks_prune_on_a_live_process() {
        assert!(blocks_prune(0, 1, false));
    }

    #[test]
    fn blocks_prune_allows_a_fully_dead_workspace() {
        assert!(!blocks_prune(0, 0, false));
    }

    #[test]
    fn force_live_overrides_the_liveness_guard() {
        assert!(!blocks_prune(3, 2, true));
    }

    #[test]
    fn live_resource_summary_pluralizes_each_kind_independently() {
        assert_eq!(live_resource_summary(1, 0), "1 running container");
        assert_eq!(live_resource_summary(2, 0), "2 running containers");
        assert_eq!(live_resource_summary(0, 1), "1 live run= process");
        assert_eq!(live_resource_summary(0, 2), "2 live run= processes");
        assert_eq!(
            live_resource_summary(1, 1),
            "1 running container and 1 live run= process"
        );
    }

    fn container_with_state(state: Option<ContainerSummaryStateEnum>) -> ContainerSummary {
        ContainerSummary {
            state,
            ..ContainerSummary::default()
        }
    }

    #[test]
    fn count_running_containers_counts_only_the_running_state() {
        let containers = vec![
            container_with_state(Some(ContainerSummaryStateEnum::RUNNING)),
            container_with_state(Some(ContainerSummaryStateEnum::EXITED)),
            container_with_state(None),
        ];
        assert_eq!(count_running_containers(&containers), 1);
    }

    #[test]
    fn count_running_containers_is_zero_for_an_empty_list() {
        assert_eq!(count_running_containers(&[]), 0);
    }

    /// Fabricate a workspace's on-disk metadata directly under a temp state
    /// root, the same shape `Workspace::save_metadata` writes, so
    /// `count_stale_workspaces` can be exercised without a real workspace or
    /// Docker.
    fn write_workspace_metadata(root: &Path, short_id: &str, workspace_path: &str) {
        let dir = root.join(short_id);
        std::fs::create_dir_all(&dir).unwrap();
        let metadata = WorkspaceMetadata {
            schema: 1,
            workspace_id: short_id.to_string(),
            short_id: short_id.to_string(),
            workspace_path: PathBuf::from(workspace_path),
            container_prefix: format!("eph-{short_id}"),
            last_seen_unix_secs: 0,
        };
        std::fs::write(
            dir.join(WORKSPACE_METADATA_FILE),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn count_stale_workspaces_counts_gone_paths_excluding_the_current_one() {
        let root = tempfile::tempdir().unwrap();

        // Stale: recorded path no longer exists.
        write_workspace_metadata(
            root.path(),
            "aaaaaaaaaaaaaaaa",
            "/does/not/exist-eph-test-aaaaaaaa",
        );
        // Live: recorded path is a real, non-empty directory.
        let live = tempfile::tempdir().unwrap();
        std::fs::write(live.path().join(".eph"), "[db]\nimage=postgres:16\n").unwrap();
        write_workspace_metadata(
            root.path(),
            "bbbbbbbbbbbbbbbb",
            live.path().to_str().expect("temp path should be UTF-8"),
        );
        // Also stale by path, but this is the "current" workspace: excluded.
        write_workspace_metadata(
            root.path(),
            "cccccccccccccccc",
            "/also/gone-eph-test-cccccccc",
        );

        assert_eq!(
            count_stale_workspaces(root.path(), "cccccccccccccccc").await,
            1,
            "only the non-excluded stale workspace should be counted"
        );
    }

    #[tokio::test]
    async fn count_stale_workspaces_is_zero_when_nothing_is_stale() {
        let root = tempfile::tempdir().unwrap();
        let live = tempfile::tempdir().unwrap();
        std::fs::write(live.path().join(".eph"), "[db]\nimage=postgres:16\n").unwrap();
        write_workspace_metadata(
            root.path(),
            "dddddddd",
            live.path().to_str().expect("temp path should be UTF-8"),
        );

        assert_eq!(count_stale_workspaces(root.path(), "").await, 0);
    }

    #[tokio::test]
    async fn count_stale_workspaces_skips_non_short_id_and_metadata_less_dirs_silently() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("not-a-short-id")).unwrap();
        std::fs::create_dir_all(root.path().join("eeeeeeee")).unwrap(); // no workspace.json

        assert_eq!(count_stale_workspaces(root.path(), "").await, 0);
    }

    #[tokio::test]
    async fn count_stale_workspaces_is_zero_for_a_missing_root() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");
        assert_eq!(count_stale_workspaces(&missing, "").await, 0);
    }

    fn process_entry(
        pid: std::num::NonZeroU32,
        identity: Option<crate::proc::ProcessIdentity>,
    ) -> crate::service::ServiceStateEntry {
        crate::service::ServiceStateEntry {
            backend: Backend::Process { pid, identity },
            ports: std::collections::HashMap::new(),
        }
    }

    fn state_with(name: &str, entry: crate::service::ServiceStateEntry) -> ServiceState {
        let mut services = std::collections::HashMap::new();
        services.insert(name.to_string(), entry);
        ServiceState {
            services,
            auto_ports: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn count_live_processes_counts_a_matching_identity() {
        let pid = std::num::NonZeroU32::new(std::process::id())
            .expect("the test process has a nonzero pid");
        let identity = proc::identity(pid).expect("the test process should expose an identity");

        let state = state_with("web", process_entry(pid, Some(identity)));

        assert_eq!(count_live_processes(&state), 1);
    }

    #[test]
    fn count_live_processes_ignores_a_mismatched_identity() {
        let pid = std::num::NonZeroU32::new(std::process::id())
            .expect("the test process has a nonzero pid");
        let mut stale_identity =
            proc::identity(pid).expect("the test process should expose an identity");
        // Diverge the recorded command line from the real one, standing in for
        // a PID that got reused by an unrelated process.
        stale_identity
            .cmd
            .push("not-actually-this-test".to_string());

        let state = state_with("web", process_entry(pid, Some(stale_identity)));

        assert_eq!(count_live_processes(&state), 0);
    }

    #[test]
    fn count_live_processes_ignores_a_backend_with_no_recorded_identity() {
        let pid = std::num::NonZeroU32::new(std::process::id())
            .expect("the test process has a nonzero pid");

        let state = state_with("web", process_entry(pid, None));

        // Legacy state without an identity is a liveness warning, not a
        // liveness *count*: `terminate_live_processes` handles that case, but
        // it must not silently block a prune the way a matched identity does.
        assert_eq!(count_live_processes(&state), 0);
    }

    #[test]
    fn count_live_processes_ignores_a_non_process_backend() {
        let state = state_with(
            "db",
            crate::service::ServiceStateEntry {
                backend: Backend::Container {
                    id: "abc123".to_string(),
                },
                ports: std::collections::HashMap::new(),
            },
        );

        assert_eq!(count_live_processes(&state), 0);
    }
}
