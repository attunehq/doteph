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
    let _lock = PruneLock::acquire(&root)?;
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
            let pruned = prune_workspace(
                docker,
                state_dir,
                short_id,
                None,
                StaleReason::CompatibilityV042State,
                options.dry_run,
                report,
            )
            .await?;
            report.totals.add(&pruned.counts);
            report.pruned.push(pruned);
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

    let pruned = prune_workspace(
        docker,
        state_dir,
        metadata.short_id,
        Some(metadata.workspace_path),
        reason,
        options.dry_run,
        report,
    )
    .await?;
    report.totals.add(&pruned.counts);
    report.pruned.push(pruned);
    Ok(())
}

async fn prune_workspace(
    docker: &Docker,
    state_dir: &Path,
    short_id: String,
    workspace_path: Option<PathBuf>,
    reason: StaleReason,
    dry_run: bool,
    report: &mut PruneReport,
) -> Result<PrunedWorkspace> {
    let mut counts = PruneCounts::default();
    let prefix = format!("eph-{short_id}-");

    prune_processes(state_dir, &short_id, dry_run, report, &mut counts).await;
    counts.containers = remove_containers(docker, &prefix, dry_run).await?;
    counts.volumes = remove_volumes(docker, &prefix, dry_run).await?;
    counts.networks = remove_networks(docker, &prefix, dry_run).await?;
    counts.images = remove_images(docker, &prefix, dry_run).await?;

    if state_dir.exists() {
        counts.state_dirs = 1;
        if !dry_run {
            tokio::fs::remove_dir_all(state_dir)
                .await
                .with_context(|| {
                    format!("failed to remove state directory: {}", state_dir.display())
                })?;
        }
    }

    Ok(PrunedWorkspace {
        short_id,
        workspace_path,
        reason,
        counts,
    })
}

async fn prune_processes(
    state_dir: &Path,
    short_id: &str,
    dry_run: bool,
    report: &mut PruneReport,
    counts: &mut PruneCounts,
) {
    let state = match load_state(state_dir).await {
        Ok(Some(state)) => state,
        Ok(None) => return,
        Err(err) => {
            report.warnings.push(format!(
                "{short_id}: could not read state.json, so run= process prune was skipped: {err:#}"
            ));
            return;
        }
    };

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

async fn remove_containers(docker: &Docker, prefix: &str, dry_run: bool) -> Result<usize> {
    let containers = docker
        .list_containers(Some(ListContainersOptionsBuilder::new().all(true).build()))
        .await
        .context("failed to list containers")?;
    let mut removed = 0;

    for container in containers {
        let matches = container.names.as_ref().is_some_and(|names| {
            names
                .iter()
                .any(|name| docker_name_has_prefix(name, prefix))
        });
        if !matches {
            continue;
        }
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
    value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ignore_not_found<T: Default>(err: BollardError) -> std::result::Result<T, BollardError> {
    match err {
        BollardError::DockerResponseServerError {
            status_code: 404, ..
        } => Ok(T::default()),
        other => Err(other),
    }
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

struct PruneLock {
    path: PathBuf,
    _file: File,
}

impl PruneLock {
    fn acquire(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create state root: {}", root.display()))?;
        let path = root.join("prune.lock");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "failed to acquire prune lock at {}; another prune may be running",
                    path.display()
                )
            })?;
        Ok(PruneLock { path, _file: file })
    }
}

impl Drop for PruneLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
    fn workspace_short_id_is_eight_hex_digits() {
        assert!(is_workspace_short_id("a1b2c3d4"));
        assert!(is_workspace_short_id("ABCDEF12"));
        assert!(!is_workspace_short_id("not-a-workspace"));
        assert!(!is_workspace_short_id("a1b2c3d"));
    }

    #[test]
    fn prune_counts_reports_empty() {
        let mut counts = PruneCounts::default();
        assert!(counts.is_empty());
        counts.state_dirs = 1;
        assert!(!counts.is_empty());
    }
}
