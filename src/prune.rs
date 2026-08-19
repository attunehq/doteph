//! Cross-workspace pruning for state left behind by disposable workspaces.
//!
//! Normal lifecycle commands start from the current `.eph` file. Prune starts
//! from the global state root instead, so it can tear down resources for a
//! workspace path that no longer exists.

use crate::git::{self, MergeStatus};
use crate::hooks::{
    CleanupKind, HookWorkspace, TeardownHookService, TeardownHookSnapshot, run_hook,
};
use crate::parser;
use crate::proc;
use crate::service::{Backend, RunningService, ServiceState, WorkspaceLock};
use crate::workspace::{WORKSPACE_METADATA_FILE, WorkspaceMetadata, state_root};
use anyhow::{Context, Result};
use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::models::{ContainerSummary, ContainerSummaryStateEnum, ImageSummary, Network, Volume};
use bollard::query_parameters::{
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, ListNetworksOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder,
    StopContainerOptionsBuilder,
};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::time::sleep;
use tracing::{debug, info};

/// Options for [`prune`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneOptions {
    /// Print what would be removed without deleting Docker resources or state.
    pub dry_run: bool,
    /// Prune state directories written by eph v0.4.2 and earlier.
    pub compatibility_v042: bool,
    /// Treat recorded workspace paths that still contain files as prune
    /// candidates. Live resources remain protected unless [`Self::force_live`]
    /// is also set.
    pub force_non_empty: bool,
    /// Remove a stale workspace's resources even when it still has a running
    /// container, or (for a [`StaleReason::NonEmptyDirectory`] candidate) a
    /// live `run=` process. Without this, a workspace that reads as stale only
    /// because it was moved or renamed (its recorded path no longer resolves)
    /// is reported and skipped instead of force-killed.
    pub force_live: bool,
    /// Treat an existing workspace as stale when no lifecycle command has
    /// touched it for at least this long (its recorded `last_seen` age).
    pub idle: Option<Duration>,
    /// Treat an existing workspace as stale when its git branch is merged into
    /// the repository's default branch and the working tree is clean.
    pub merged: bool,
}

/// Whether the recorded workspace path still holds an eph workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathState {
    /// The path no longer exists.
    Missing,
    /// The path exists but is not a directory.
    NotDirectory,
    /// The path is an empty directory.
    Empty,
    /// The path is a non-empty directory with no `.eph` file, so no eph
    /// command can run there any more.
    NoEphFile,
    /// The path is a non-empty directory with a `.eph` file.
    Present,
}

impl PathState {
    /// Short label for tabular output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            PathState::Missing => "missing",
            PathState::NotDirectory => "not a directory",
            PathState::Empty => "empty",
            PathState::NoEphFile => "no .eph",
            PathState::Present => "present",
        }
    }
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
    /// The recorded workspace path is non-empty and was selected explicitly.
    NonEmptyDirectory,
    /// The recorded workspace path exists but is no longer a directory.
    NotDirectory,
    /// The recorded workspace path exists but no longer contains a `.eph` file.
    NoEphFile,
    /// No lifecycle command has touched the workspace within
    /// [`PruneOptions::idle`].
    Idle,
    /// The workspace's git branch is merged into its repository's default
    /// branch and its working tree is clean ([`PruneOptions::merged`]).
    MergedBranch,
    /// The state directory was written before eph recorded workspace metadata.
    CompatibilityV042State,
}

impl StaleReason {
    fn label(self) -> &'static str {
        match self {
            StaleReason::Missing => "missing workspace",
            StaleReason::EmptyDirectory => "empty workspace directory",
            StaleReason::NonEmptyDirectory => "non-empty workspace directory",
            StaleReason::NotDirectory => "workspace path is not a directory",
            StaleReason::NoEphFile => "workspace has no .eph file",
            StaleReason::Idle => "idle workspace",
            StaleReason::MergedBranch => "merged branch",
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

/// A non-fatal problem prune reports and then works around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneWarning {
    /// Workspace short ID the warning belongs to.
    pub short_id: String,
    /// What went wrong and what prune did instead.
    pub message: String,
}

impl PruneReport {
    fn warn(&mut self, short_id: &str, message: impl Into<String>) {
        self.warnings.push(PruneWarning {
            short_id: short_id.to_string(),
            message: message.into(),
        });
    }
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

/// What prune (or `eph system ls`) knows about one metadata-backed workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceSummary {
    /// Workspace short ID, the namespace used in Docker resource names.
    pub short_id: String,
    /// Recorded workspace path.
    pub workspace_path: PathBuf,
    /// Whether the recorded path still holds an eph workspace.
    pub path_state: PathState,
    /// Unix time of the last lifecycle, `env`, `run`, or `status` command.
    pub last_seen_unix_secs: u64,
    /// The checkout's relationship to its repository's default branch.
    pub merge: MergeStatus,
    /// `run=` processes still alive under the identity eph recorded.
    pub live_processes: usize,
    /// Containers in the workspace's namespace that are running.
    pub running_containers: usize,
    /// All containers in the workspace's namespace.
    pub containers: usize,
    /// Volumes in the workspace's namespace.
    pub volumes: usize,
}

impl WorkspaceSummary {
    /// Seconds since the workspace was last seen, relative to `now`.
    #[must_use]
    pub fn idle_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_seen_unix_secs)
    }
}

/// Summary returned by [`prune`].
#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Unix time the pass ran, the reference point for idle ages.
    pub now_unix_secs: u64,
    /// Stale workspaces removed, or that would be removed during a dry run.
    pub pruned: Vec<PrunedWorkspace>,
    /// Metadata-backed workspaces that still exist and were not selected,
    /// oldest `last_seen` first, with the signals a user needs to decide
    /// whether to select them (`--idle`, `--merged`, `--force-non-empty`).
    pub kept: Vec<WorkspaceSummary>,
    /// Workspaces or state directories left untouched.
    pub skipped: Vec<SkippedWorkspace>,
    /// Non-fatal warnings, including unsafe `run=` process prune skips.
    pub warnings: Vec<PruneWarning>,
    /// Total resource counts across [`pruned`](Self::pruned).
    pub totals: PruneCounts,
}

/// One daemon-wide resource listing shared by every workspace in a prune pass.
///
/// State roots can contain thousands of workspaces. Listing each Docker
/// resource type for every stale workspace makes prune time grow with both the
/// state count and Docker round-trip latency, while resource names already
/// carry enough information to partition one snapshot in memory.
struct DockerInventory {
    containers: Vec<ContainerSummary>,
    volumes: Vec<Volume>,
    networks: Vec<Network>,
    images: Vec<ImageSummary>,
}

struct PruneDocker<'a> {
    client: &'a Docker,
    inventory: &'a DockerInventory,
}

struct PruneCandidate {
    state_dir: PathBuf,
    short_id: String,
    workspace_path: Option<PathBuf>,
    reason: StaleReason,
    metadata: Option<WorkspaceMetadata>,
    /// Merge status at classification time, reused by the pre-removal
    /// re-check so git is not consulted twice.
    merge: MergeStatus,
}

/// A metadata-backed state directory after the filesystem checks, before the
/// stale decision (which may also need the merge status) is made.
struct InspectedWorkspace {
    state_dir: PathBuf,
    metadata: WorkspaceMetadata,
    path_state: PathState,
}

struct PruneHookContext<'a> {
    snapshot: &'a TeardownHookSnapshot,
    metadata: &'a WorkspaceMetadata,
    state: Option<&'a ServiceState>,
    running: &'a HashMap<String, RunningService>,
    live_services: &'a HashSet<String>,
    cwd: &'a Path,
    short_id: &'a str,
}

impl PruneHookContext<'_> {
    fn workspace(&self) -> HookWorkspace<'_> {
        HookWorkspace::new(
            &self.metadata.workspace_path,
            &self.metadata.workspace_id,
            &self.metadata.short_id,
        )
    }
}

impl DockerInventory {
    async fn load(docker: &Docker) -> Result<Self> {
        let (containers, volumes, networks, images) = tokio::try_join!(
            docker.list_containers(Some(ListContainersOptionsBuilder::new().all(true).build())),
            docker.list_volumes(None::<bollard::query_parameters::ListVolumesOptions>),
            docker.list_networks(Some(ListNetworksOptionsBuilder::default().build())),
            docker.list_images(Some(ListImagesOptionsBuilder::default().all(true).build())),
        )
        .context("failed to inventory Docker resources")?;

        Ok(Self {
            containers,
            volumes: volumes.volumes.unwrap_or_default(),
            networks,
            images,
        })
    }
}

/// Remove resources for metadata-backed workspaces whose recorded path is gone
/// or empty, plus non-empty paths selected by
/// [`PruneOptions::force_non_empty`].
///
/// # Errors
///
/// Returns an error if the state root cannot be read, Docker cannot be reached,
/// or a Docker/filesystem removal fails. Individual malformed state directories
/// are reported as warnings and skipped.
pub async fn prune(options: PruneOptions) -> Result<PruneReport> {
    let root = state_root()?;
    debug!("Acquiring system prune lock at {}", root.display());
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
    debug!("Connecting to Docker");
    docker
        .ping()
        .await
        .context("failed to ping docker daemon")?;
    let now = current_unix_secs();
    let mut report = PruneReport {
        dry_run: options.dry_run,
        now_unix_secs: now,
        ..PruneReport::default()
    };

    let state_dirs = state_dirs(&root).await?;
    let state_dir_count = state_dirs.len();
    debug!("Inspecting {state_dir_count} eph state directories");
    let mut candidates = Vec::new();
    let mut inspected = Vec::new();
    for (index, state_dir) in state_dirs.into_iter().enumerate() {
        if index > 0 && index % 100 == 0 {
            debug!("Inspected {index} of {state_dir_count} eph state directories");
        }
        debug!("Inspecting {}", state_dir.display());
        match classify_state_dir(state_dir, options, &mut report)? {
            Classified::Candidate(candidate) => candidates.push(candidate),
            Classified::Inspected(workspace) => inspected.push(workspace),
            Classified::Skipped => {}
        }
    }

    // Every existing checkout is asked about its branch once, concurrently:
    // the answer selects `--merged` candidates and is shown for kept
    // workspaces either way.
    let merges = merge_statuses(&inspected).await;
    let mut kept = Vec::new();
    for (workspace, merge) in inspected.into_iter().zip(merges) {
        let age = Duration::from_secs(now.saturating_sub(workspace.metadata.last_seen_unix_secs));
        match stale_reason(workspace.path_state, merge, age, options) {
            Some(reason) => {
                debug!(
                    "Found stale workspace {} at {} ({reason})",
                    workspace.metadata.short_id,
                    workspace.metadata.workspace_path.display()
                );
                candidates.push(PruneCandidate {
                    state_dir: workspace.state_dir,
                    short_id: workspace.metadata.short_id.clone(),
                    workspace_path: Some(workspace.metadata.workspace_path.clone()),
                    reason,
                    metadata: Some(workspace.metadata),
                    merge,
                });
            }
            None => kept.push((workspace, merge)),
        }
    }
    candidates.sort_by(|a, b| a.short_id.cmp(&b.short_id));

    debug!(
        "Found {} stale workspace candidates; inventorying Docker resources",
        candidates.len()
    );

    // An existing non-empty path can still run lifecycle commands. Lock every
    // destructive candidate before taking the shared Docker snapshot so an
    // `up` cannot create resources after the liveness check and before prune
    // removes the workspace's state. The daemon-wide prune lock gives every
    // prune the same lock order, so candidate locks cannot deadlock each other.
    let mut workspace_locks = if options.dry_run {
        Vec::new()
    } else {
        candidates
            .iter()
            .map(|candidate| WorkspaceLock::open_state_dir(&candidate.state_dir))
            .collect::<Result<Vec<_>>>()?
    };
    let _workspace_guards = workspace_locks
        .iter_mut()
        .map(WorkspaceLock::acquire)
        .collect::<Result<Vec<_>>>()?;

    let inventory = DockerInventory::load(&docker).await?;
    let prune_docker = PruneDocker {
        client: &docker,
        inventory: &inventory,
    };
    debug!(
        "Docker inventory contains {} containers, {} volumes, {} networks, and {} images",
        inventory.containers.len(),
        inventory.volumes.len(),
        inventory.networks.len(),
        inventory.images.len()
    );

    for candidate in candidates {
        prune_candidate(&prune_docker, candidate, options, &mut report).await?;
    }
    for (workspace, merge) in kept {
        report
            .kept
            .push(summarize(workspace, merge, &inventory).await);
    }
    report
        .kept
        .sort_by_key(|workspace| workspace.last_seen_unix_secs);
    debug!(
        "System prune pass found {} stale workspaces, kept {} existing workspaces, and skipped {} state directories",
        report.pruned.len(),
        report.kept.len(),
        report.skipped.len()
    );

    Ok(report)
}

/// Inspect every metadata-backed workspace in the state root without removing
/// anything: the data behind `eph system ls`.
///
/// # Errors
///
/// Returns an error if the state root cannot be read or Docker cannot be
/// reached.
pub async fn list_workspaces() -> Result<Vec<WorkspaceSummary>> {
    let root = state_root()?;
    let docker = Docker::connect_with_local_defaults()
        .context("failed to connect to docker (is docker running?)")?;
    docker
        .ping()
        .await
        .context("failed to ping docker daemon")?;

    let mut report = PruneReport::default();
    let mut inspected = Vec::new();
    for state_dir in state_dirs(&root).await? {
        if let Classified::Inspected(workspace) =
            classify_state_dir(state_dir, PruneOptions::default(), &mut report)?
        {
            inspected.push(workspace);
        }
    }
    let merges = merge_statuses(&inspected).await;
    let inventory = DockerInventory::load(&docker).await?;
    let mut summaries = Vec::with_capacity(inspected.len());
    for (workspace, merge) in inspected.into_iter().zip(merges) {
        summaries.push(summarize(workspace, merge, &inventory).await);
    }
    summaries.sort_by_key(|workspace| workspace.last_seen_unix_secs);
    Ok(summaries)
}

/// Current Unix time in seconds; a clock before 1970 reads as zero.
#[must_use]
pub fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Ask git about every inspected workspace whose path still holds a checkout,
/// concurrently. Paths that are gone get [`MergeStatus::Unknown`] without a
/// git invocation.
async fn merge_statuses(inspected: &[InspectedWorkspace]) -> Vec<MergeStatus> {
    futures_util::future::join_all(inspected.iter().map(|workspace| async {
        match workspace.path_state {
            PathState::Present | PathState::NoEphFile => {
                git::merge_status(&workspace.metadata.workspace_path).await
            }
            PathState::Missing | PathState::NotDirectory | PathState::Empty => MergeStatus::Unknown,
        }
    }))
    .await
}

/// Combine an inspected workspace with its Docker and process footprint.
async fn summarize(
    workspace: InspectedWorkspace,
    merge: MergeStatus,
    inventory: &DockerInventory,
) -> WorkspaceSummary {
    let InspectedWorkspace {
        state_dir,
        metadata,
        path_state,
    } = workspace;
    let prefix = format!("eph-{}-", metadata.short_id);
    let live_processes = load_state(&state_dir)
        .await
        .ok()
        .flatten()
        .as_ref()
        .map_or(0, count_live_processes);
    let containers = matching_containers(&inventory.containers, &prefix);
    WorkspaceSummary {
        short_id: metadata.short_id,
        workspace_path: metadata.workspace_path,
        path_state,
        last_seen_unix_secs: metadata.last_seen_unix_secs,
        merge,
        live_processes,
        running_containers: count_running_containers(&containers),
        containers: containers.len(),
        volumes: inventory
            .volumes
            .iter()
            .filter(|volume| volume.name.starts_with(&prefix))
            .count(),
    }
}

/// Outcome of looking at one state directory.
enum Classified {
    /// A legacy directory selected by `--compatibility-v042`.
    Candidate(PruneCandidate),
    /// A metadata-backed workspace whose stale decision still needs the merge
    /// status and idle age.
    Inspected(InspectedWorkspace),
    /// Already reported under `skipped`.
    Skipped,
}

fn classify_state_dir(
    state_dir: PathBuf,
    options: PruneOptions,
    report: &mut PruneReport,
) -> Result<Classified> {
    let short_id = state_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string());
    let Some(short_id_format) = workspace_short_id_format(&short_id) else {
        report.skipped.push(SkippedWorkspace {
            short_id,
            workspace_path: None,
            reason: "state directory name is not an eph workspace short ID".to_string(),
        });
        return Ok(Classified::Skipped);
    };
    let metadata_path = state_dir.join(WORKSPACE_METADATA_FILE);

    if !metadata_path.exists() {
        match (short_id_format, options.compatibility_v042) {
            (WorkspaceShortIdFormat::LegacyV042, true) => {
                return Ok(Classified::Candidate(PruneCandidate {
                    state_dir,
                    short_id,
                    workspace_path: None,
                    reason: StaleReason::CompatibilityV042State,
                    metadata: None,
                    merge: MergeStatus::Unknown,
                }));
            }
            (WorkspaceShortIdFormat::LegacyV042, false) => {
                report.skipped.push(SkippedWorkspace {
                    short_id,
                    workspace_path: None,
                    reason:
                        "v0.4.2-and-earlier state has no workspace metadata; pass --compatibility-v042 to prune it"
                            .to_string(),
                });
            }
            (WorkspaceShortIdFormat::Current, _) => {
                report.skipped.push(SkippedWorkspace {
                    short_id,
                    workspace_path: None,
                    reason:
                        "current-format state has no workspace metadata and cannot be pruned safely"
                            .to_string(),
                });
            }
        }
        return Ok(Classified::Skipped);
    }

    let metadata = match WorkspaceMetadata::load_from_state_dir_sync(&state_dir) {
        Ok(metadata) => metadata,
        Err(err) => {
            report.skipped.push(SkippedWorkspace {
                short_id,
                workspace_path: None,
                reason: format!("{err:#}"),
            });
            return Ok(Classified::Skipped);
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
        return Ok(Classified::Skipped);
    }

    let path_state = path_state(&metadata.workspace_path)?;
    Ok(Classified::Inspected(InspectedWorkspace {
        state_dir,
        metadata,
        path_state,
    }))
}

async fn prune_candidate(
    docker: &PruneDocker<'_>,
    candidate: PruneCandidate,
    options: PruneOptions,
    report: &mut PruneReport,
) -> Result<()> {
    if candidate.metadata.is_none() && gained_workspace_metadata(&candidate.state_dir) {
        report.skipped.push(SkippedWorkspace {
            short_id: candidate.short_id,
            workspace_path: candidate.workspace_path,
            reason: "workspace metadata appeared during prune".to_string(),
        });
        return Ok(());
    }

    if !options.dry_run
        && let Some(metadata) = candidate.metadata.as_ref()
        && !metadata_still_prunable(&candidate.state_dir, metadata, candidate.merge, options)?
    {
        report.skipped.push(SkippedWorkspace {
            short_id: candidate.short_id,
            workspace_path: candidate.workspace_path,
            reason: "workspace metadata changed during prune".to_string(),
        });
        return Ok(());
    }

    if let Some(pruned) = prune_workspace(docker, candidate, options, report).await? {
        report.totals.add(&pruned.counts);
        report.pruned.push(pruned);
    }
    Ok(())
}

/// Detect a legacy candidate that another eph invocation adopted during prune.
///
/// Compatibility candidates have no metadata to compare, so the appearance of
/// the metadata file is the proof that the original classification is stale.
fn gained_workspace_metadata(state_dir: &Path) -> bool {
    state_dir.join(WORKSPACE_METADATA_FILE).exists()
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
    docker: &PruneDocker<'_>,
    candidate: PruneCandidate,
    options: PruneOptions,
    report: &mut PruneReport,
) -> Result<Option<PrunedWorkspace>> {
    let PruneCandidate {
        state_dir,
        short_id,
        workspace_path,
        reason,
        metadata,
        merge: _,
    } = candidate;
    let prefix = format!("eph-{short_id}-");

    let state = load_state_or_warn(&state_dir, &short_id, report).await;
    let live_processes = state.as_ref().map_or(0, count_live_processes);
    let containers = matching_containers(&docker.inventory.containers, &prefix);
    let running_containers = count_running_containers(&containers);

    if blocks_prune(
        reason,
        running_containers,
        live_processes,
        options.force_live,
    ) {
        debug!(
            "Skipping workspace {short_id}: {} still live",
            live_resource_summary(running_containers, live_processes)
        );
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
    let hook_snapshot =
        select_teardown_hooks(state.as_ref(), metadata.as_ref(), &short_id, report).await;
    let hook_services = state.as_ref().map_or_else(HashMap::new, hook_services);
    let live_hook_services = hook_snapshot
        .as_ref()
        .zip(metadata.as_ref())
        .map(|(snapshot, metadata)| {
            snapshot
                .services_rev()
                .filter(|service| {
                    service_is_live(
                        service,
                        state.as_ref(),
                        &docker.inventory.containers,
                        &metadata.short_id,
                    )
                })
                .map(|service| service.name.clone())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let hook_cwd = workspace_path
        .as_deref()
        .filter(|path| path.is_dir())
        .unwrap_or(&state_dir);

    if options.dry_run {
        debug!("Planning removal for workspace {short_id}");
    } else {
        info!("Removing resources for workspace {short_id}");
        if let (Some(snapshot), Some(metadata)) = (&hook_snapshot, &metadata) {
            let context = PruneHookContext {
                snapshot,
                metadata,
                state: state.as_ref(),
                running: &hook_services,
                live_services: &live_hook_services,
                cwd: hook_cwd,
                short_id: &short_id,
            };
            run_pre_prune_hooks(docker, &context, report, &mut counts).await?;
        }
    }

    if let Some(state) = &state {
        terminate_live_processes(state, &short_id, options.dry_run, report, &mut counts).await;
    }
    counts.containers = remove_containers(docker.client, containers, options.dry_run).await?;
    counts.volumes = remove_volumes(
        docker.client,
        &docker.inventory.volumes,
        &prefix,
        options.dry_run,
    )
    .await?;
    counts.networks = remove_networks(
        docker.client,
        &docker.inventory.networks,
        &prefix,
        options.dry_run,
    )
    .await?;
    counts.images = remove_images(
        docker.client,
        &docker.inventory.images,
        &prefix,
        options.dry_run,
    )
    .await?;

    if !options.dry_run
        && let (Some(snapshot), Some(metadata)) = (&hook_snapshot, &metadata)
    {
        let context = PruneHookContext {
            snapshot,
            metadata,
            state: state.as_ref(),
            running: &hook_services,
            live_services: &live_hook_services,
            cwd: hook_cwd,
            short_id: &short_id,
        };
        run_post_clean_hooks(&context, report).await;
    }

    if state_dir.exists() {
        counts.state_dirs = 1;
        if !options.dry_run {
            info!("Removing state directory {}", state_dir.display());
            tokio::fs::remove_dir_all(&state_dir)
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

async fn select_teardown_hooks(
    state: Option<&ServiceState>,
    metadata: Option<&WorkspaceMetadata>,
    short_id: &str,
    report: &mut PruneReport,
) -> Option<TeardownHookSnapshot> {
    let Some(metadata) = metadata else {
        report.warn(
            short_id,
            "teardown hooks are unavailable: workspace metadata is missing",
        );
        return None;
    };

    let eph_path = metadata.workspace_path.join(".eph");
    let mut current_error = None;
    if metadata.workspace_path.is_dir() {
        match tokio::fs::read_to_string(&eph_path).await {
            Ok(contents) => match parser::parse(&contents) {
                Ok(eph) => return Some(TeardownHookSnapshot::capture(&eph)),
                Err(error) => {
                    current_error = Some(format!(
                        "could not parse current {} for prune hooks: {error:#}",
                        eph_path.display()
                    ));
                }
            },
            Err(error) => {
                current_error = Some(format!(
                    "could not read current {} for prune hooks: {error}",
                    eph_path.display()
                ));
            }
        }
    }

    match state.and_then(|state| state.teardown_hooks.clone()) {
        Some(snapshot) => {
            if let Some(error) = current_error {
                report.warn(
                    short_id,
                    format!("{error}; using the saved teardown snapshot"),
                );
            }
            Some(snapshot)
        }
        None => {
            // Every `up` since hook snapshots were introduced saves one (empty
            // when the file has no teardown hooks), so a missing snapshot
            // means the state predates them.
            let detail = match current_error {
                Some(error) => format!("{error}; "),
                None => String::new(),
            };
            report.warn(
                short_id,
                format!(
                    "teardown hooks are unavailable: {detail}state.json has no saved hook snapshot (written by an older eph)"
                ),
            );
            None
        }
    }
}

fn hook_services(state: &ServiceState) -> HashMap<String, RunningService> {
    let mut services = state
        .last_ports
        .iter()
        .map(|(name, ports)| {
            (
                name.clone(),
                RunningService {
                    name: name.clone(),
                    ports: ports.clone(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    for (name, entry) in &state.services {
        services.insert(
            name.clone(),
            RunningService {
                name: name.clone(),
                ports: entry.ports.clone(),
            },
        );
    }
    services
}

async fn run_pre_prune_hooks(
    docker: &PruneDocker<'_>,
    context: &PruneHookContext<'_>,
    report: &mut PruneReport,
    counts: &mut PruneCounts,
) -> Result<()> {
    for service in context.snapshot.services_rev() {
        run_hook_phase(context, service, "pre-clean", &service.pre_clean, report).await;

        if !context.live_services.contains(&service.name) {
            continue;
        }

        run_hook_phase(context, service, "pre-stop", &service.pre_stop, report).await;
        stop_hook_service(
            docker.client,
            service,
            context.state,
            &docker.inventory.containers,
            &context.metadata.short_id,
            counts,
        )
        .await?;
        run_hook_phase(context, service, "post-stop", &service.post_stop, report).await;
    }
    Ok(())
}

async fn run_post_clean_hooks(context: &PruneHookContext<'_>, report: &mut PruneReport) {
    for service in context.snapshot.services_rev() {
        run_hook_phase(context, service, "post-clean", &service.post_clean, report).await;
    }
}

async fn run_hook_phase(
    context: &PruneHookContext<'_>,
    service: &TeardownHookService,
    phase: &str,
    commands: &[String],
    report: &mut PruneReport,
) {
    if commands.is_empty() {
        return;
    }

    let env = match context
        .snapshot
        .environment(context.workspace(), context.running, service)
    {
        Ok(env) => env,
        Err(error) => {
            for command in commands {
                report.warn(
                    context.short_id,
                    format!(
                        "{} {phase} hook `{command}` was skipped: {error:#}",
                        service.name
                    ),
                );
            }
            return;
        }
    };

    for command in commands {
        if let Err(error) = run_hook(command, context.cwd, &env).await {
            report.warn(
                context.short_id,
                format!("{} {phase} hook failed: {error}", service.name),
            );
        }
    }
}

fn service_is_live(
    service: &TeardownHookService,
    state: Option<&ServiceState>,
    containers: &[ContainerSummary],
    short_id: &str,
) -> bool {
    match state.and_then(|state| state.services.get(&service.name)) {
        Some(entry) => backend_is_live(&entry.backend, &service.name, containers, short_id),
        None => match service.cleanup_kind {
            CleanupKind::DirectContainer => direct_containers(containers, short_id, &service.name)
                .iter()
                .any(|container| container_is_running(container)),
            CleanupKind::Compose => {
                compose_containers(containers, &format!("eph-{short_id}-{}", service.name))
                    .iter()
                    .any(|container| container_is_running(container))
            }
            CleanupKind::Process => false,
        },
    }
}

fn backend_is_live(
    backend: &Backend,
    service_name: &str,
    containers: &[ContainerSummary],
    short_id: &str,
) -> bool {
    match backend {
        Backend::Process {
            pid,
            identity: Some(identity),
        } => proc::identity_matches(*pid, identity),
        Backend::Process { identity: None, .. } => false,
        Backend::Container { id } => containers.iter().any(|container| {
            (container.id.as_deref() == Some(id)
                || container_has_exact_name(container, &format!("eph-{short_id}-{service_name}")))
                && container_is_running(container)
        }),
        Backend::Compose { project } => compose_containers(containers, project)
            .iter()
            .any(|container| container_is_running(container)),
    }
}

async fn stop_hook_service(
    docker: &Docker,
    service: &TeardownHookService,
    state: Option<&ServiceState>,
    containers: &[ContainerSummary],
    short_id: &str,
    counts: &mut PruneCounts,
) -> Result<()> {
    if let Some(entry) = state.and_then(|state| state.services.get(&service.name)) {
        match &entry.backend {
            Backend::Process {
                pid,
                identity: Some(identity),
            } if proc::identity_matches(*pid, identity) => {
                proc::terminate(*pid);
                sleep(Duration::from_secs(2)).await;
                proc::force_kill(*pid);
                counts.processes += 1;
            }
            Backend::Process { .. } => {}
            Backend::Container { id } => {
                let matched = containers.iter().filter(|container| {
                    container.id.as_deref() == Some(id)
                        || container_has_exact_name(
                            container,
                            &format!("eph-{short_id}-{}", service.name),
                        )
                });
                stop_containers(docker, matched).await?;
            }
            Backend::Compose { project } => {
                stop_containers(docker, compose_containers(containers, project)).await?;
            }
        }
        return Ok(());
    }

    match service.cleanup_kind {
        CleanupKind::DirectContainer => {
            stop_containers(
                docker,
                direct_containers(containers, short_id, &service.name),
            )
            .await?;
        }
        CleanupKind::Compose => {
            let project = format!("eph-{short_id}-{}", service.name);
            stop_containers(docker, compose_containers(containers, &project)).await?;
        }
        CleanupKind::Process => {}
    }
    Ok(())
}

async fn stop_containers<'a>(
    docker: &Docker,
    containers: impl IntoIterator<Item = &'a ContainerSummary>,
) -> Result<()> {
    for container in containers {
        if !container_is_running(container) {
            continue;
        }
        let Some(id) = container.id.as_deref() else {
            continue;
        };
        info!("Stopping container {id} before prune hooks continue");
        docker
            .stop_container(id, Some(StopContainerOptionsBuilder::new().t(10).build()))
            .await
            .or_else(ignore_stopped_or_missing)
            .context("failed to stop container")?;
    }
    Ok(())
}

fn direct_containers<'a>(
    containers: &'a [ContainerSummary],
    short_id: &str,
    service_name: &str,
) -> Vec<&'a ContainerSummary> {
    let name = format!("eph-{short_id}-{service_name}");
    containers
        .iter()
        .filter(|container| container_has_exact_name(container, &name))
        .collect()
}

fn compose_containers<'a>(
    containers: &'a [ContainerSummary],
    project: &str,
) -> Vec<&'a ContainerSummary> {
    containers
        .iter()
        .filter(|container| {
            container
                .labels
                .as_ref()
                .and_then(|labels| labels.get("com.docker.compose.project").map(String::as_str))
                == Some(project)
        })
        .collect()
}

fn container_has_exact_name(container: &ContainerSummary, expected: &str) -> bool {
    container.names.as_ref().is_some_and(|names| {
        names
            .iter()
            .any(|name| name.strip_prefix('/').unwrap_or(name) == expected)
    })
}

fn container_is_running(container: &ContainerSummary) -> bool {
    matches!(container.state, Some(ContainerSummaryStateEnum::RUNNING))
}

fn ignore_stopped_or_missing<T: Default>(
    error: BollardError,
) -> std::result::Result<T, BollardError> {
    match error {
        BollardError::DockerResponseServerError {
            status_code: 304 | 404,
            ..
        } => Ok(T::default()),
        other => Err(other),
    }
}

/// Whether a stale-pathed workspace has live resources that block a default
/// prune: a running container, a live `run=` process, or both. `force_live`
/// overrides the guard entirely, restoring the old unconditional behavior.
///
/// Pulled out of [`prune_workspace`] as a plain function over counts (rather
/// than the Docker/process-table calls that produce them) so the decision
/// itself is exercised by a unit test with no Docker daemon involved.
/// Whether live resources keep a stale candidate out of this pass.
///
/// A running container always blocks without `--force-live`: a workspace that
/// was moved or renamed keeps its containers, so a running one can mean the
/// workspace is still in use under a path prune cannot see.
///
/// A live `run=` process is different. Its recorded identity includes the
/// working directory eph launched it in, so a process that still matches while
/// its workspace path is gone, emptied, or has lost its `.eph` file is an
/// orphan by construction: a moved workspace would report the new path and
/// stop matching. Idle and merged candidates were selected by a signal that
/// already says nobody is using them. Only the blunt `--force-non-empty`
/// selection, which carries no such signal, lets a live process block.
fn blocks_prune(
    reason: StaleReason,
    running_containers: usize,
    live_processes: usize,
    force_live: bool,
) -> bool {
    if force_live {
        return false;
    }
    running_containers > 0 || (reason == StaleReason::NonEmptyDirectory && live_processes > 0)
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
            report.warn(
                short_id,
                format!("could not read state.json, so run= process prune was skipped: {err:#}"),
            );
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
    state: &ServiceState,
    short_id: &str,
    dry_run: bool,
    report: &mut PruneReport,
    counts: &mut PruneCounts,
) {
    for (name, entry) in &state.services {
        let Backend::Process { pid, identity } = &entry.backend else {
            continue;
        };

        let Some(identity) = identity else {
            if proc::is_alive(*pid) {
                report.warn(
                    short_id,
                    format!("{name}: skipped run= PID {pid}; state has no process identity"),
                );
            }
            continue;
        };

        if proc::identity_matches(*pid, identity) {
            counts.processes += 1;
            if !dry_run {
                proc::terminate(*pid);
                sleep(Duration::from_secs(2)).await;
                proc::force_kill(*pid);
            }
        } else if proc::is_alive(*pid) {
            report.warn(
                short_id,
                format!(
                    "{name}: skipped run= PID {pid}; the live process does not match recorded identity"
                ),
            );
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

/// Select containers whose name carries `prefix` from the pass-wide snapshot.
fn matching_containers(containers: &[ContainerSummary], prefix: &str) -> Vec<ContainerSummary> {
    containers
        .iter()
        .filter(|container| {
            container.names.as_ref().is_some_and(|names| {
                names
                    .iter()
                    .any(|name| docker_name_has_prefix(name, prefix))
            })
        })
        .cloned()
        .collect()
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
        info!("Removing container {id}");
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

async fn remove_volumes(
    docker: &Docker,
    volumes: &[Volume],
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    let mut removed = 0;

    for volume in volumes {
        if !volume.name.starts_with(prefix) {
            continue;
        }
        removed += 1;
        if dry_run {
            continue;
        }
        info!("Removing volume {}", volume.name);
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

async fn remove_networks(
    docker: &Docker,
    networks: &[Network],
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
    let mut removed = 0;

    for network in networks {
        let Some(name) = network.name.as_ref() else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        removed += 1;
        if dry_run {
            continue;
        }
        info!("Removing network {name}");
        docker
            .remove_network(name)
            .await
            .or_else(ignore_not_found)
            .with_context(|| format!("failed to remove network {name}"))?;
    }

    Ok(removed)
}

async fn remove_images(
    docker: &Docker,
    images: &[ImageSummary],
    prefix: &str,
    dry_run: bool,
) -> Result<usize> {
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
        info!("Removing image {tag}");
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceShortIdFormat {
    LegacyV042,
    Current,
}

fn workspace_short_id_format(value: &str) -> Option<WorkspaceShortIdFormat> {
    if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    match value.len() {
        8 => Some(WorkspaceShortIdFormat::LegacyV042),
        16 => Some(WorkspaceShortIdFormat::Current),
        _ => None,
    }
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
        if workspace_short_id_format(&short_id).is_none() || short_id == exclude_short_id {
            continue;
        }
        let Ok(metadata) = WorkspaceMetadata::load_from_state_dir(&dir).await else {
            continue;
        };
        let is_stale = path_state(&metadata.workspace_path).is_ok_and(|state| {
            stale_reason(
                state,
                MergeStatus::Unknown,
                Duration::ZERO,
                PruneOptions::default(),
            )
            .is_some()
        });
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

/// Look at the recorded workspace path on disk.
fn path_state(path: &Path) -> Result<PathState> {
    if !path.exists() {
        return Ok(PathState::Missing);
    }
    if !path.is_dir() {
        return Ok(PathState::NotDirectory);
    }
    if path
        .read_dir()
        .with_context(|| format!("failed to read workspace directory: {}", path.display()))?
        .next()
        .is_none()
    {
        return Ok(PathState::Empty);
    }
    if !path.join(".eph").is_file() {
        return Ok(PathState::NoEphFile);
    }
    Ok(PathState::Present)
}

/// Decide whether a metadata-backed workspace is stale under `options`.
///
/// A path that no longer holds an eph workspace is always stale. A present one
/// is stale only on a signal the caller opted into: its branch is merged
/// (`--merged`), nothing has touched it for `--idle`, or the blunt
/// `--force-non-empty`. The first matching reason wins, most specific first,
/// so the report names the strongest evidence.
fn stale_reason(
    path_state: PathState,
    merge: MergeStatus,
    idle_for: Duration,
    options: PruneOptions,
) -> Option<StaleReason> {
    match path_state {
        PathState::Missing => return Some(StaleReason::Missing),
        PathState::NotDirectory => return Some(StaleReason::NotDirectory),
        PathState::Empty => return Some(StaleReason::EmptyDirectory),
        PathState::NoEphFile => return Some(StaleReason::NoEphFile),
        PathState::Present => {}
    }
    if options.merged && merge == MergeStatus::Merged {
        return Some(StaleReason::MergedBranch);
    }
    if options.idle.is_some_and(|threshold| idle_for >= threshold) {
        return Some(StaleReason::Idle);
    }
    options
        .force_non_empty
        .then_some(StaleReason::NonEmptyDirectory)
}

/// Re-check a metadata-backed candidate immediately before a destructive pass.
///
/// The same `options` (and the merge status observed at classification) must
/// drive both the preview classification and this check. Otherwise a
/// workspace could appear in the confirmed preview and then be rejected only
/// when removal begins. A lifecycle command that ran in between rewrites the
/// metadata (new `last_seen`), which the equality check catches.
fn metadata_still_prunable(
    state_dir: &Path,
    original: &WorkspaceMetadata,
    merge: MergeStatus,
    options: PruneOptions,
) -> Result<bool> {
    if !state_dir.join(WORKSPACE_METADATA_FILE).exists() {
        return Ok(false);
    }
    let current = WorkspaceMetadata::load_from_state_dir_sync(state_dir)?;
    if &current != original {
        return Ok(false);
    }
    let age = Duration::from_secs(current_unix_secs().saturating_sub(current.last_seen_unix_secs));
    Ok(stale_reason(path_state(&current.workspace_path)?, merge, age, options).is_some())
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

    fn present_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".eph"), "[db]\nimage=postgres:16\n").unwrap();
        dir
    }

    fn reason_for(path: &Path, options: PruneOptions) -> Option<StaleReason> {
        stale_reason(
            path_state(path).unwrap(),
            MergeStatus::Unknown,
            Duration::ZERO,
            options,
        )
    }

    #[test]
    fn classifies_missing_workspace_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert_eq!(
            reason_for(&missing, PruneOptions::default()),
            Some(StaleReason::Missing)
        );
    }

    #[test]
    fn classifies_workspace_without_eph_file_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "left behind").unwrap();

        assert_eq!(
            reason_for(dir.path(), PruneOptions::default()),
            Some(StaleReason::NoEphFile)
        );
    }

    #[test]
    fn idle_threshold_selects_only_workspaces_at_least_that_old() {
        let options = PruneOptions {
            idle: Some(Duration::from_secs(3600)),
            ..PruneOptions::default()
        };
        let idle = |age| {
            stale_reason(
                PathState::Present,
                MergeStatus::Unknown,
                Duration::from_secs(age),
                options,
            )
        };

        assert_eq!(idle(3599), None);
        assert_eq!(idle(3600), Some(StaleReason::Idle));
        assert_eq!(idle(86_400), Some(StaleReason::Idle));
    }

    #[test]
    fn merged_selects_clean_merged_branches_only_when_asked() {
        let asked = PruneOptions {
            merged: true,
            ..PruneOptions::default()
        };
        let reason =
            |merge, options| stale_reason(PathState::Present, merge, Duration::ZERO, options);

        assert_eq!(
            reason(MergeStatus::Merged, asked),
            Some(StaleReason::MergedBranch)
        );
        assert_eq!(reason(MergeStatus::MergedDirty, asked), None);
        assert_eq!(reason(MergeStatus::Unmerged, asked), None);
        assert_eq!(reason(MergeStatus::Unknown, asked), None);
        assert_eq!(reason(MergeStatus::Merged, PruneOptions::default()), None);
    }

    #[test]
    fn most_specific_reason_wins_for_a_present_workspace() {
        let everything = PruneOptions {
            force_non_empty: true,
            idle: Some(Duration::ZERO),
            merged: true,
            ..PruneOptions::default()
        };

        assert_eq!(
            stale_reason(
                PathState::Present,
                MergeStatus::Merged,
                Duration::ZERO,
                everything
            ),
            Some(StaleReason::MergedBranch)
        );
        assert_eq!(
            stale_reason(
                PathState::Present,
                MergeStatus::Unmerged,
                Duration::ZERO,
                everything
            ),
            Some(StaleReason::Idle)
        );
        assert_eq!(
            stale_reason(
                PathState::Missing,
                MergeStatus::Merged,
                Duration::ZERO,
                everything
            ),
            Some(StaleReason::Missing)
        );
    }

    #[test]
    fn detects_metadata_added_after_legacy_candidate_discovery() {
        let state_dir = tempfile::tempdir().unwrap();
        assert!(!gained_workspace_metadata(state_dir.path()));

        std::fs::write(state_dir.path().join(WORKSPACE_METADATA_FILE), "{}").unwrap();

        assert!(gained_workspace_metadata(state_dir.path()));
    }

    #[test]
    fn classifies_empty_workspace_as_stale() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            reason_for(dir.path(), PruneOptions::default()),
            Some(StaleReason::EmptyDirectory)
        );
    }

    #[test]
    fn keeps_non_empty_workspace_active() {
        let dir = present_workspace();

        assert_eq!(reason_for(dir.path(), PruneOptions::default()), None);
    }

    #[test]
    fn force_non_empty_classifies_non_empty_workspace_as_stale() {
        let dir = present_workspace();

        assert_eq!(
            reason_for(
                dir.path(),
                PruneOptions {
                    force_non_empty: true,
                    ..PruneOptions::default()
                }
            ),
            Some(StaleReason::NonEmptyDirectory)
        );
    }

    #[test]
    fn metadata_backed_workspace_is_inspected_not_skipped() {
        let root = tempfile::tempdir().unwrap();
        let workspace = present_workspace();
        let short_id = write_workspace_metadata(root.path(), workspace.path());
        let mut report = PruneReport::default();

        let classified = classify_state_dir(
            root.path().join(&short_id),
            PruneOptions::default(),
            &mut report,
        )
        .unwrap();

        let Classified::Inspected(inspected) = classified else {
            panic!("metadata-backed workspace should be inspected");
        };
        assert_eq!(inspected.path_state, PathState::Present);
        assert_eq!(inspected.metadata.short_id, short_id);
        assert!(report.skipped.is_empty());
    }

    #[tokio::test]
    async fn removed_metadata_is_no_longer_prunable_after_lock_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().join("aaaaaaaaaaaaaaaa");
        let metadata = WorkspaceMetadata {
            schema: 1,
            workspace_id: "a".repeat(64),
            short_id: "aaaaaaaaaaaaaaaa".to_string(),
            workspace_path: root.path().join("workspace"),
            container_prefix: "eph-aaaaaaaaaaaaaaaa".to_string(),
            last_seen_unix_secs: 0,
        };

        assert!(
            !metadata_still_prunable(
                &state_dir,
                &metadata,
                MergeStatus::Unknown,
                PruneOptions {
                    force_non_empty: true,
                    ..PruneOptions::default()
                },
            )
            .unwrap()
        );
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
    fn workspace_short_id_format_distinguishes_legacy_and_current_ids() {
        assert_eq!(
            workspace_short_id_format("a1b2c3d4"),
            Some(WorkspaceShortIdFormat::LegacyV042)
        );
        assert_eq!(
            workspace_short_id_format("ABCDEF12"),
            Some(WorkspaceShortIdFormat::LegacyV042)
        );
        assert_eq!(
            workspace_short_id_format("a1b2c3d4e5f60718"),
            Some(WorkspaceShortIdFormat::Current)
        );
        assert_eq!(workspace_short_id_format("not-a-workspace"), None);
        assert_eq!(workspace_short_id_format("a1b2c3d"), None);
        assert_eq!(workspace_short_id_format("a1b2c3d4e5f607182"), None);
    }

    #[tokio::test]
    async fn compatibility_never_selects_current_state_without_metadata() {
        let root = tempfile::tempdir().unwrap();
        let short_id = "aaaaaaaaaaaaaaaa";
        let state_dir = root.path().join(short_id);
        std::fs::create_dir(&state_dir).unwrap();
        let mut report = PruneReport::default();

        let classified = classify_state_dir(
            state_dir,
            PruneOptions {
                compatibility_v042: true,
                ..PruneOptions::default()
            },
            &mut report,
        )
        .unwrap();

        assert!(matches!(classified, Classified::Skipped));
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("cannot be pruned safely"));
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
    fn blocks_prune_on_a_running_container_for_every_reason() {
        for reason in [
            StaleReason::Missing,
            StaleReason::EmptyDirectory,
            StaleReason::NotDirectory,
            StaleReason::NoEphFile,
            StaleReason::Idle,
            StaleReason::MergedBranch,
            StaleReason::NonEmptyDirectory,
            StaleReason::CompatibilityV042State,
        ] {
            assert!(blocks_prune(reason, 1, 0, false), "{reason}");
        }
    }

    #[test]
    fn live_process_blocks_only_the_blunt_non_empty_selection() {
        assert!(blocks_prune(StaleReason::NonEmptyDirectory, 0, 1, false));
        for reason in [
            StaleReason::Missing,
            StaleReason::EmptyDirectory,
            StaleReason::NotDirectory,
            StaleReason::NoEphFile,
            StaleReason::Idle,
            StaleReason::MergedBranch,
            StaleReason::CompatibilityV042State,
        ] {
            assert!(!blocks_prune(reason, 0, 1, false), "{reason}");
        }
    }

    #[test]
    fn blocks_prune_allows_a_fully_dead_workspace() {
        assert!(!blocks_prune(StaleReason::NonEmptyDirectory, 0, 0, false));
    }

    #[test]
    fn force_live_overrides_the_liveness_guard() {
        assert!(!blocks_prune(StaleReason::NonEmptyDirectory, 3, 2, true));
        assert!(!blocks_prune(StaleReason::Missing, 3, 2, true));
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

    #[test]
    fn direct_service_recovery_requires_an_exact_container_name() {
        let containers = vec![ContainerSummary {
            names: Some(vec!["/eph-0123456789abcdef-web-api".to_string()]),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            ..ContainerSummary::default()
        }];

        assert!(direct_containers(&containers, "0123456789abcdef", "web").is_empty());
        assert_eq!(
            direct_containers(&containers, "0123456789abcdef", "web-api").len(),
            1
        );
    }

    #[test]
    fn compose_backend_liveness_uses_the_recorded_project_label() {
        let project = "eph-0123456789abcdef-stack";
        let containers = vec![ContainerSummary {
            labels: Some(HashMap::from([(
                "com.docker.compose.project".to_string(),
                project.to_string(),
            )])),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            ..ContainerSummary::default()
        }];
        let backend = Backend::Compose {
            project: project.to_string(),
        };

        assert!(backend_is_live(
            &backend,
            "renamed-source-service",
            &containers,
            "ffffffffffffffff"
        ));
    }

    #[tokio::test]
    async fn invalid_current_hooks_without_a_snapshot_report_one_truthful_warning() {
        let workspace_dir = tempfile::tempdir().unwrap();
        std::fs::write(workspace_dir.path().join(".eph"), "[app]\nrun=\n").unwrap();
        let workspace = crate::Workspace::from_path(workspace_dir.path()).unwrap();
        let metadata = WorkspaceMetadata::for_workspace(&workspace);
        let state = ServiceState::default();
        let mut report = PruneReport::default();

        let snapshot = select_teardown_hooks(
            Some(&state),
            Some(&metadata),
            &metadata.short_id,
            &mut report,
        )
        .await;

        assert!(snapshot.is_none());
        assert_eq!(report.warnings.len(), 1);
        let warning = &report.warnings[0];
        assert_eq!(warning.short_id, metadata.short_id);
        assert!(warning.message.contains("could not parse current"));
        assert!(warning.message.contains("no saved hook snapshot"));
        assert!(
            !warning
                .message
                .contains("using the saved teardown snapshot")
        );
    }

    /// Build metadata through the same path-derived identity contract as a
    /// real workspace so stale-state tests cannot accidentally exercise an
    /// impossible identity.
    fn write_workspace_metadata(root: &Path, workspace_path: &Path) -> String {
        let workspace_id = crate::workspace::compute_workspace_id(workspace_path);
        let short_id = workspace_id[..16].to_string();
        let dir = root.join(&short_id);
        std::fs::create_dir_all(&dir).unwrap();
        let metadata = WorkspaceMetadata {
            schema: 1,
            workspace_id,
            short_id: short_id.clone(),
            workspace_path: workspace_path.to_path_buf(),
            container_prefix: format!("eph-{short_id}"),
            last_seen_unix_secs: 0,
        };
        std::fs::write(
            dir.join(WORKSPACE_METADATA_FILE),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();
        short_id
    }

    #[tokio::test]
    async fn count_stale_workspaces_counts_gone_paths_excluding_the_current_one() {
        let root = tempfile::tempdir().unwrap();

        // Stale: recorded path no longer exists.
        let missing = root.path().join("does-not-exist-eph-test-aaaaaaaa");
        write_workspace_metadata(root.path(), &missing);
        // Live: recorded path is a real, non-empty directory.
        let live = tempfile::tempdir().unwrap();
        std::fs::write(live.path().join(".eph"), "[db]\nimage=postgres:16\n").unwrap();
        write_workspace_metadata(root.path(), live.path());
        // Also stale by path, but this is the "current" workspace: excluded.
        let excluded_path = root.path().join("also-gone-eph-test-cccccccc");
        let excluded = write_workspace_metadata(root.path(), &excluded_path);

        assert_eq!(
            count_stale_workspaces(root.path(), &excluded).await,
            1,
            "only the non-excluded stale workspace should be counted"
        );
    }

    #[tokio::test]
    async fn count_stale_workspaces_is_zero_when_nothing_is_stale() {
        let root = tempfile::tempdir().unwrap();
        let live = tempfile::tempdir().unwrap();
        std::fs::write(live.path().join(".eph"), "[db]\nimage=postgres:16\n").unwrap();
        write_workspace_metadata(root.path(), live.path());

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
        let mut state = ServiceState::default();
        state.services.insert(name.to_string(), entry);
        state
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
