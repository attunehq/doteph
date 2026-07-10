//! Workspace management for ephemeral services
//!
//! Each workspace (directory with a .eph file) gets a unique ID based on
//! its absolute path. This ensures multiple checkouts of the same project
//! don't conflict with each other.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const WORKSPACE_METADATA_FILE: &str = "workspace.json";
const WORKSPACE_METADATA_SCHEMA: u32 = 1;

/// Cross-workspace state that lets `eph system prune` decide whether a state
/// directory still points at a real workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceMetadata {
    pub(crate) schema: u32,
    pub(crate) workspace_id: String,
    pub(crate) short_id: String,
    pub(crate) workspace_path: PathBuf,
    pub(crate) container_prefix: String,
    pub(crate) last_seen_unix_secs: u64,
}

impl WorkspaceMetadata {
    pub(crate) fn for_workspace(workspace: &Workspace) -> Self {
        WorkspaceMetadata {
            schema: WORKSPACE_METADATA_SCHEMA,
            workspace_id: workspace.id.clone(),
            short_id: workspace.short_id.clone(),
            workspace_path: workspace.path.clone(),
            container_prefix: workspace.container_prefix(),
            last_seen_unix_secs: current_unix_secs(),
        }
    }

    pub(crate) async fn load_from_state_dir(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(WORKSPACE_METADATA_FILE);
        let contents = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read workspace metadata: {}", path.display()))?;
        serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse workspace metadata: {}", path.display()))
    }
}

/// A workspace: a directory containing a `.eph` file.
///
/// Each workspace gets a stable [`id`](Self::id) derived from the SHA-256 of its
/// canonical path, so multiple checkouts of the same project at different paths
/// never collide on container or volume names. Construct one with
/// [`Workspace::from_path`] (an exact directory) or [`Workspace::find_from_path`]
/// / [`Workspace::find_from_cwd`] (search upwards for a `.eph` file).
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Absolute, canonicalized path to the workspace directory.
    pub path: PathBuf,
    /// Unique identifier for this workspace (hex SHA-256 of [`path`](Self::path)).
    pub id: String,
    /// First 8 hex characters of [`id`](Self::id), used for display and naming.
    pub short_id: String,
}

impl Workspace {
    /// Create a workspace from a directory path.
    ///
    /// The path is canonicalized, so the resulting [`id`](Self::id) is stable
    /// regardless of how the directory was addressed (relative path, symlink,
    /// etc.). On Windows the canonical form omits the extended-length `\\?\`
    /// prefix (via `dunce`) whenever a plain `C:\...` path exists, so the stored
    /// path is one Docker and the display code can use directly.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` cannot be canonicalized, for example because
    /// the directory does not exist or is not accessible.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use eph::Workspace;
    ///
    /// let ws = Workspace::from_path(".")?;
    /// println!("workspace id: {}", ws.short_id);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        // dunce::canonicalize, not std's: on Windows std returns the
        // extended-length `\\?\C:\...` form, which Docker rejects as a bind-mount
        // source and which leaks into display output. dunce yields a plain
        // `C:\...` path whenever one exists. On Unix it is std::fs::canonicalize.
        let path = dunce::canonicalize(path.as_ref()).with_context(|| {
            format!(
                "failed to resolve workspace path: {}",
                path.as_ref().display()
            )
        })?;

        let id = compute_workspace_id(&path);
        let short_id = id[..8].to_string();

        Ok(Workspace { path, id, short_id })
    }

    /// Find the workspace by walking up from the current directory.
    ///
    /// Convenience wrapper over [`find_from_path`](Self::find_from_path) that
    /// starts at the process's current working directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the current directory cannot be determined, or if no
    /// `.eph` file is found in it or any parent directory (see
    /// [`find_from_path`](Self::find_from_path)).
    pub fn find_from_cwd() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to get current directory")?;
        Self::find_from_path(&cwd)
    }

    /// Find the workspace by walking up from a given directory.
    ///
    /// Starts at `start` and ascends through parent directories, returning the
    /// first one that contains a `.eph` file (via
    /// [`from_path`](Self::from_path)).
    ///
    /// # Errors
    ///
    /// Returns an error if no `.eph` file is found in `start` or any of its
    /// ancestors, or if the directory that does contain one cannot be
    /// canonicalized (see [`from_path`](Self::from_path)).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use eph::Workspace;
    /// use std::path::Path;
    ///
    /// let ws = Workspace::find_from_path(Path::new("."))?;
    /// println!("found .eph at {}", ws.eph_file_path().display());
    /// # Ok(())
    /// # }
    /// ```
    pub fn find_from_path(start: &Path) -> Result<Self> {
        let mut current = start.to_path_buf();

        loop {
            let eph_file = current.join(".eph");
            if eph_file.exists() {
                return Self::from_path(&current);
            }

            if !current.pop() {
                anyhow::bail!(
                    "no .eph file found in {} or any parent directory",
                    start.display()
                );
            }
        }
    }

    /// Get the path to the .eph file
    #[must_use]
    pub fn eph_file_path(&self) -> PathBuf {
        self.path.join(".eph")
    }

    /// Get a container name prefix for this workspace
    #[must_use]
    pub fn container_prefix(&self) -> String {
        format!("eph-{}", self.short_id)
    }

    /// Get a full container name for a service
    #[must_use]
    pub fn container_name(&self, service: &str) -> String {
        format!("{}-{}", self.container_prefix(), service)
    }

    /// Get a volume name for a service
    #[must_use]
    pub fn volume_name(&self, service: &str, volume_name: &str) -> String {
        format!("{}-{}-{}", self.container_prefix(), service, volume_name)
    }

    /// Get the state directory for this workspace.
    ///
    /// This is `<local data dir>/eph/<short_id>`, where the per-workspace
    /// [`short_id`](Self::short_id) keeps state for different checkouts apart.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform's local data directory cannot be
    /// determined.
    pub fn state_dir(&self) -> Result<PathBuf> {
        Ok(state_root()?.join(&self.short_id))
    }

    pub(crate) async fn save_metadata(&self) -> Result<()> {
        let state_dir = self.state_dir()?;
        tokio::fs::create_dir_all(&state_dir)
            .await
            .with_context(|| {
                format!("failed to create state directory: {}", state_dir.display())
            })?;
        let path = state_dir.join(WORKSPACE_METADATA_FILE);
        let contents = serde_json::to_string_pretty(&WorkspaceMetadata::for_workspace(self))
            .context("failed to serialize workspace metadata")?;
        tokio::fs::write(&path, contents)
            .await
            .with_context(|| format!("failed to write workspace metadata: {}", path.display()))
    }

    /// Get the directory holding captured `run=` service logs.
    ///
    /// This is `<state_dir>/logs`. Because it lives under
    /// [`state_dir`](Self::state_dir), `eph clean` removes it along with the
    /// rest of the workspace's persisted state -- no separate cleanup path is
    /// needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform's local data directory cannot be
    /// determined (see [`state_dir`](Self::state_dir)).
    pub fn logs_dir(&self) -> Result<PathBuf> {
        Ok(self.state_dir()?.join("logs"))
    }

    /// Get the path to a `run=` service's captured log file.
    ///
    /// This is `<state_dir>/logs/<service>.log`. Docker- and compose-backed
    /// services keep their logs in the daemon (read via `docker logs`); only
    /// `run=` services, which eph spawns directly, are captured here.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform's local data directory cannot be
    /// determined (see [`state_dir`](Self::state_dir)).
    pub fn log_file_path(&self, service: &str) -> Result<PathBuf> {
        Ok(self.logs_dir()?.join(format!("{service}.log")))
    }
}

/// Compute a unique ID for a workspace based on its path
fn compute_workspace_id(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())
}

/// The root directory holding every workspace's persisted state: one
/// `<short_id>` subdirectory per workspace, each with its `state.json` and
/// `workspace.json`.
///
/// Honors `EPH_STATE_ROOT` as an absolute-path override, checked first; unset
/// or empty, this is `<local data dir>/eph`. The override is what lets an
/// integration test point a whole `eph` invocation at a throwaway directory
/// instead of the real per-user data directory, and lets a user relocate
/// eph's state entirely (a non-default disk, a synced folder, and so on).
///
/// # Errors
///
/// Returns an error if `EPH_STATE_ROOT` is unset (or empty) and the
/// platform's local data directory cannot be determined.
pub fn state_root() -> Result<PathBuf> {
    if let Some(root) = env_nonempty("EPH_STATE_ROOT") {
        return Ok(PathBuf::from(root));
    }
    Ok(dirs::data_local_dir()
        .context("failed to determine local data directory")?
        .join("eph"))
}

/// Read an environment variable, treating unset or all-whitespace as absent so
/// an empty override never shadows the default.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the `EPH_STATE_ROOT` tests below: `std::env::set_var` mutates
    /// process-wide state, and `cargo test` runs tests concurrently, so two of
    /// these could otherwise race each other's view of the environment.
    static ENV_STATE_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn state_root_honors_the_eph_state_root_override() {
        let _guard = ENV_STATE_ROOT_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by ENV_STATE_ROOT_LOCK against the other test in
        // this module that touches EPH_STATE_ROOT; no other test reads it.
        unsafe {
            std::env::set_var("EPH_STATE_ROOT", dir.path());
        }
        let root = state_root();
        unsafe {
            std::env::remove_var("EPH_STATE_ROOT");
        }
        assert_eq!(root.unwrap(), dir.path());
    }

    #[test]
    fn state_root_ignores_an_empty_override() {
        let _guard = ENV_STATE_ROOT_LOCK.lock().unwrap();
        // SAFETY: see state_root_honors_the_eph_state_root_override.
        unsafe {
            std::env::set_var("EPH_STATE_ROOT", "   ");
        }
        let root = state_root();
        unsafe {
            std::env::remove_var("EPH_STATE_ROOT");
        }
        // An all-whitespace override must fall back to the real default rather
        // than resolving to a bogus path built from blank text.
        assert!(root.is_ok());
        assert_ne!(root.unwrap(), PathBuf::from("   "));
    }

    #[test]
    fn test_workspace_id_is_deterministic() {
        let path = Path::new("/home/user/projects/myapp");
        let id1 = compute_workspace_id(path);
        let id2 = compute_workspace_id(path);
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_different_paths_different_ids() {
        let id1 = compute_workspace_id(Path::new("/home/user/projects/app1"));
        let id2 = compute_workspace_id(Path::new("/home/user/projects/app2"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn workspace_metadata_records_prune_lookup_fields() {
        let workspace = Workspace {
            path: PathBuf::from("/home/user/projects/app"),
            id: "abcdef0123456789".to_string(),
            short_id: "abcdef01".to_string(),
        };

        let metadata = WorkspaceMetadata::for_workspace(&workspace);

        assert_eq!(metadata.schema, WORKSPACE_METADATA_SCHEMA);
        assert_eq!(metadata.workspace_id, workspace.id);
        assert_eq!(metadata.short_id, workspace.short_id);
        assert_eq!(metadata.workspace_path, workspace.path);
        assert_eq!(metadata.container_prefix, "eph-abcdef01");
    }
}
