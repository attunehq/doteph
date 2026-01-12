//! Workspace management for ephemeral services
//!
//! Each workspace (directory with a .eph file) gets a unique ID based on
//! its absolute path. This ensures multiple checkouts of the same project
//! don't conflict with each other.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// A workspace represents a directory with a .eph file
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Absolute path to the workspace directory
    pub path: PathBuf,
    /// Unique identifier for this workspace (hash of path)
    pub id: String,
    /// Short ID for display and container naming
    pub short_id: String,
}

impl Workspace {
    /// Create a workspace from a directory path
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().canonicalize().with_context(|| {
            format!(
                "Failed to resolve workspace path: {}",
                path.as_ref().display()
            )
        })?;

        let id = compute_workspace_id(&path);
        let short_id = id[..8].to_string();

        Ok(Workspace { path, id, short_id })
    }

    /// Find workspace by walking up from current directory
    pub fn find_from_cwd() -> Result<Self> {
        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        Self::find_from_path(&cwd)
    }

    /// Find workspace by walking up from a given directory
    pub fn find_from_path(start: &Path) -> Result<Self> {
        let mut current = start.to_path_buf();

        loop {
            let eph_file = current.join(".eph");
            if eph_file.exists() {
                return Self::from_path(&current);
            }

            if !current.pop() {
                anyhow::bail!(
                    "No .eph file found in {} or any parent directory",
                    start.display()
                );
            }
        }
    }

    /// Get the path to the .eph file
    pub fn eph_file_path(&self) -> PathBuf {
        self.path.join(".eph")
    }

    /// Get a container name prefix for this workspace
    pub fn container_prefix(&self) -> String {
        format!("eph-{}", self.short_id)
    }

    /// Get a full container name for a service
    pub fn container_name(&self, service: &str) -> String {
        format!("{}-{}", self.container_prefix(), service)
    }

    /// Get a volume name for a service
    pub fn volume_name(&self, service: &str, volume_name: &str) -> String {
        format!("{}-{}-{}", self.container_prefix(), service, volume_name)
    }

    /// Get the state directory for this workspace
    pub fn state_dir(&self) -> Result<PathBuf> {
        let state_dir = dirs::data_local_dir()
            .context("Failed to determine local data directory")?
            .join("eph")
            .join(&self.short_id);
        Ok(state_dir)
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
