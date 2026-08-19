//! How `eph system prune` selects existing workspaces (`--idle`, `--merged`,
//! a missing `.eph`), how it treats orphaned `run=` processes, and what
//! `eph system ls` shows.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

struct SelectionWorkspace {
    workspace: Option<tempfile::TempDir>,
    workspace_path: PathBuf,
    state_root: tempfile::TempDir,
}

impl SelectionWorkspace {
    fn new(eph: &str) -> Self {
        let workspace = tempfile::tempdir().expect("failed to create test workspace");
        std::fs::write(workspace.path().join(".eph"), eph).expect("failed to write test .eph");
        let workspace_path = workspace.path().to_path_buf();
        Self {
            workspace: Some(workspace),
            workspace_path,
            state_root: tempfile::tempdir().expect("failed to create test state root"),
        }
    }

    fn remove_workspace(&mut self) {
        self.workspace
            .take()
            .expect("workspace already removed")
            .close()
            .expect("failed to remove test workspace");
    }

    async fn eph(&self, args: &[&str]) -> Output {
        let cwd = if self.workspace_path.is_dir() {
            &self.workspace_path
        } else {
            self.state_root.path()
        };
        tokio::process::Command::new(env!("CARGO_BIN_EXE_eph"))
            .args(args)
            .current_dir(cwd)
            .env("EPH_STATE_ROOT", self.state_root.path())
            .output()
            .await
            .expect("failed to run eph")
    }

    async fn short_id(&self) -> String {
        let output = self.eph(&["info"]).await;
        assert_success("eph info", &output);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("Short ID: "))
            .expect("eph info should print a short ID")
            .trim()
            .to_string()
    }

    async fn state_dir(&self) -> PathBuf {
        self.state_root.path().join(self.short_id().await)
    }

    /// Rewrite the recorded `last_seen` so the workspace reads as idle for
    /// `secs` without waiting that long.
    async fn age_metadata(&self, secs: u64) {
        let path = self.state_dir().await.join("workspace.json");
        let contents = std::fs::read_to_string(&path).expect("workspace metadata should exist");
        let mut metadata: serde_json::Value =
            serde_json::from_str(&contents).expect("workspace metadata should be JSON");
        let last_seen = metadata["last_seen_unix_secs"]
            .as_u64()
            .expect("last_seen_unix_secs should be recorded");
        metadata["last_seen_unix_secs"] = serde_json::json!(last_seen.saturating_sub(secs));
        std::fs::write(&path, serde_json::to_string_pretty(&metadata).unwrap()).unwrap();
    }

    async fn last_seen(&self) -> u64 {
        let path = self.state_dir().await.join("workspace.json");
        let contents = std::fs::read_to_string(&path).expect("workspace metadata should exist");
        let metadata: serde_json::Value = serde_json::from_str(&contents).unwrap();
        metadata["last_seen_unix_secs"].as_u64().unwrap()
    }

    fn recorded_pids(&self, state_dir: &Path) -> Vec<u32> {
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        state["services"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(|service| service["backend"]["process"]["pid"].as_u64())
            .map(|pid| pid as u32)
            .collect()
    }
}

impl Drop for SelectionWorkspace {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_eph"))
            .args(["system", "prune", "--force"])
            .current_dir(self.state_root.path())
            .env("EPH_STATE_ROOT", self.state_root.path())
            .output();
    }
}

fn assert_success(operation: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(unix)]
fn long_running_command() -> &'static str {
    "sleep 300"
}

#[cfg(windows)]
fn long_running_command() -> &'static str {
    "ping -n 301 127.0.0.1 >NUL"
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks for existence and permission.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

async fn git(cwd: &Path, args: &[&str]) {
    let status = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("git should spawn");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

/// A `run=` process whose workspace directory is deleted is an orphan: prune
/// reaps it and removes the workspace without `--force-live`.
#[tokio::test]
async fn missing_workspace_with_live_run_process_is_pruned_without_force_live() {
    let mut workspace =
        SelectionWorkspace::new(&format!("[app]\nrun={}\n", long_running_command()));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    let state_dir = workspace.state_dir().await;
    let pids = workspace.recorded_pids(&state_dir);
    assert!(!pids.is_empty(), "up should record the run= PID");
    assert!(pids.iter().all(|&pid| pid_is_alive(pid)));
    workspace.remove_workspace();

    let prune = workspace.eph(&["system", "prune", "--yes"]).await;
    assert_success("eph system prune --yes", &prune);
    let out = stdout(&prune);
    assert!(
        out.contains("(missing workspace)") && out.contains("Verified run= processes: 1"),
        "prune should reap the orphaned run= process: {out}"
    );
    assert!(
        !out.contains("Skipped:"),
        "nothing should be skipped: {out}"
    );
    assert!(!state_dir.exists());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        pids.iter().all(|&pid| !pid_is_alive(pid)),
        "orphaned run= processes should be terminated"
    );
}

/// `--idle` selects a present workspace whose `last_seen` is old enough, and
/// reaps its `run=` processes; without the flag it stays in the Kept table.
#[tokio::test]
async fn idle_selects_old_workspaces_and_reaps_their_processes() {
    let workspace = SelectionWorkspace::new(&format!("[app]\nrun={}\n", long_running_command()));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    let short_id = workspace.short_id().await;
    let state_dir = workspace.state_dir().await;
    let pids = workspace.recorded_pids(&state_dir);
    workspace.age_metadata(3 * 86_400).await;

    let kept = workspace.eph(&["system", "prune", "--dry-run"]).await;
    assert_success("eph system prune --dry-run", &kept);
    let kept_out = stdout(&kept);
    assert!(
        kept_out.contains("Kept (workspace still exists")
            && kept_out.contains(&short_id)
            && kept_out.contains("3d"),
        "an idle workspace should be listed as kept with its age: {kept_out}"
    );
    assert!(state_dir.exists());

    let too_young = workspace
        .eph(&["system", "prune", "--dry-run", "--idle", "7d"])
        .await;
    assert_success("eph system prune --idle 7d", &too_young);
    assert!(
        !stdout(&too_young).contains("(idle workspace)"),
        "a 3d-old workspace is not idle for 7d: {}",
        stdout(&too_young)
    );

    let prune = workspace
        .eph(&["system", "prune", "--idle", "2d", "--yes"])
        .await;
    assert_success("eph system prune --idle 2d --yes", &prune);
    let out = stdout(&prune);
    assert!(
        out.contains("(idle workspace)") && out.contains("Verified run= processes: 1"),
        "idle prune should select the workspace and reap its process: {out}"
    );
    assert!(!state_dir.exists());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(pids.iter().all(|&pid| !pid_is_alive(pid)));
}

/// `eph env` refreshes `last_seen`, so a workspace that is only read from
/// never reads as idle.
#[tokio::test]
async fn env_refreshes_last_seen() {
    let workspace = SelectionWorkspace::new(&format!("[app]\nrun={}\n", long_running_command()));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    workspace.age_metadata(86_400).await;
    let aged = workspace.last_seen().await;

    assert_success("eph env", &workspace.eph(&["env"]).await);

    assert!(
        workspace.last_seen().await >= aged + 86_400 - 60,
        "env should refresh last_seen"
    );
}

/// A workspace directory that lost its `.eph` file is stale by default.
#[tokio::test]
async fn workspace_without_eph_file_is_pruned_by_default() {
    let workspace = SelectionWorkspace::new(&format!("[app]\nrun={}\n", long_running_command()));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    let state_dir = workspace.state_dir().await;
    std::fs::remove_file(workspace.workspace_path.join(".eph")).unwrap();
    std::fs::write(workspace.workspace_path.join("README.md"), "left behind").unwrap();

    let prune = workspace.eph(&["system", "prune", "--yes"]).await;
    assert_success("eph system prune --yes", &prune);
    assert!(
        stdout(&prune).contains("(workspace has no .eph file)"),
        "{}",
        stdout(&prune)
    );
    assert!(!state_dir.exists());
}

/// `--merged` selects a worktree whose branch is merged into the default
/// branch; the default branch checkout and an unmerged branch stay kept.
#[tokio::test]
async fn merged_selects_only_merged_clean_worktrees() {
    let repo = tempfile::tempdir().unwrap();
    let state_root = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]).await;
    std::fs::write(
        repo.path().join(".eph"),
        format!("[app]\nrun={}\n", long_running_command()),
    )
    .unwrap();
    git(repo.path(), &["add", ".eph"]).await;
    git(repo.path(), &["commit", "-q", "-m", "add eph"]).await;
    let run = |cwd: PathBuf, args: Vec<&'static str>| {
        let state_root = state_root.path().to_path_buf();
        async move {
            tokio::process::Command::new(env!("CARGO_BIN_EXE_eph"))
                .args(args)
                .current_dir(cwd)
                .env("EPH_STATE_ROOT", state_root)
                .output()
                .await
                .expect("failed to run eph")
        }
    };

    let merged = repo.path().join("merged");
    let open = repo.path().join("open");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "done",
            merged.to_str().unwrap(),
        ],
    )
    .await;
    git(
        repo.path(),
        &["worktree", "add", "-q", "-b", "wip", open.to_str().unwrap()],
    )
    .await;
    std::fs::write(merged.join("done.txt"), "done").unwrap();
    git(&merged, &["add", "done.txt"]).await;
    git(&merged, &["commit", "-q", "-m", "done"]).await;
    std::fs::write(open.join("wip.txt"), "wip").unwrap();
    git(&open, &["add", "wip.txt"]).await;
    git(&open, &["commit", "-q", "-m", "wip"]).await;
    // Squash-merge `done` and move main on.
    git(repo.path(), &["merge", "-q", "--squash", "done"]).await;
    git(repo.path(), &["commit", "-q", "-m", "done (#1)"]).await;
    std::fs::write(repo.path().join("later.txt"), "later").unwrap();
    git(repo.path(), &["add", "later.txt"]).await;
    git(repo.path(), &["commit", "-q", "-m", "later"]).await;

    for cwd in [repo.path().to_path_buf(), merged.clone(), open.clone()] {
        assert_success("eph up", &run(cwd, vec!["up"]).await);
    }

    let ls = run(repo.path().to_path_buf(), vec!["system", "ls"]).await;
    assert_success("eph system ls", &ls);
    let ls_out = stdout(&ls);
    let branch_for = |path: &Path| {
        ls_out
            .lines()
            .find(|line| line.ends_with(path.to_str().unwrap()))
            .map(|line| line.split_whitespace().rev().nth(1).unwrap().to_string())
            .unwrap_or_else(|| panic!("{} should be listed: {ls_out}", path.display()))
    };
    assert_eq!(branch_for(&merged), "merged");
    assert_eq!(branch_for(&open), "unmerged");
    assert_eq!(branch_for(repo.path()), "unmerged");

    let prune = run(
        repo.path().to_path_buf(),
        vec!["system", "prune", "--merged", "--yes"],
    )
    .await;
    assert_success("eph system prune --merged --yes", &prune);
    let out = stdout(&prune);
    assert!(
        out.contains("(merged branch)") && out.contains(merged.to_str().unwrap()),
        "{out}"
    );
    let kept_section = out.split("Kept (").nth(1).unwrap_or_default();
    assert!(kept_section.contains(open.to_str().unwrap()), "{out}");
    assert!(
        !out.contains(&format!("(merged branch) - {}", repo.path().display())),
        "{out}"
    );

    let _ = run(
        repo.path().to_path_buf(),
        vec!["system", "prune", "--force"],
    )
    .await;
}
