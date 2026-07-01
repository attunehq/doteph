//! Workspace management for ephemeral services
//!
//! Each workspace (directory with a .eph file) gets a unique ID based on
//! its absolute path. This ensures multiple checkouts of the same project
//! don't conflict with each other.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

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
        let state_dir = dirs::data_local_dir()
            .context("failed to determine local data directory")?
            .join("eph")
            .join(&self.short_id);
        Ok(state_dir)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
