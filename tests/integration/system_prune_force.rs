use std::process::Output;

struct ForcePruneWorkspace {
    workspace: tempfile::TempDir,
    state_root: tempfile::TempDir,
}

impl ForcePruneWorkspace {
    fn new(eph: &str) -> Self {
        let workspace = tempfile::tempdir().expect("failed to create test workspace");
        std::fs::write(workspace.path().join(".eph"), eph).expect("failed to write test .eph");
        Self {
            workspace,
            state_root: tempfile::tempdir().expect("failed to create test state root"),
        }
    }

    async fn eph(&self, args: &[&str]) -> Output {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_eph"))
            .args(args)
            .current_dir(self.workspace.path())
            .env("EPH_STATE_ROOT", self.state_root.path())
            .output()
            .await
            .expect("failed to run eph")
    }
}

impl Drop for ForcePruneWorkspace {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_eph"))
            .arg("down")
            .current_dir(self.workspace.path())
            .env("EPH_STATE_ROOT", self.state_root.path())
            .output();
    }
}

#[cfg(unix)]
fn long_running_command() -> &'static str {
    "sleep 300"
}

#[cfg(windows)]
fn long_running_command() -> &'static str {
    "ping -n 301 127.0.0.1 >NUL"
}

async fn workspace_short_id(workspace: &ForcePruneWorkspace) -> String {
    let output = workspace.eph(&["info"]).await;
    assert!(
        output.status.success(),
        "eph info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Short ID: "))
        .expect("eph info should print the workspace short ID")
        .trim()
        .to_string()
}

/// The aggregate flag must cross the real nested clap boundary, select every
/// protected scope, remain non-destructive with `--dry-run`, and supply the
/// confirmation required by a non-interactive invocation.
#[tokio::test]
async fn force_aggregates_prune_overrides_end_to_end() {
    let workspace = ForcePruneWorkspace::new(&format!("[app]\nrun={}\n", long_running_command()));
    let up = workspace.eph(&["up"]).await;
    assert!(
        up.status.success(),
        "eph up failed: {}",
        String::from_utf8_lossy(&up.stderr)
    );

    let workspace_id = workspace_short_id(&workspace).await;
    let workspace_state = workspace.state_root.path().join(&workspace_id);
    let legacy_state = workspace.state_root.path().join("deadbeef");
    let incomplete_current_state = workspace.state_root.path().join("cafebabecafebabe");
    std::fs::create_dir(&legacy_state).unwrap();
    std::fs::create_dir(&incomplete_current_state).unwrap();

    let granular = workspace
        .eph(&["system", "prune", "--force-non-empty", "--dry-run"])
        .await;
    assert!(granular.status.success());
    let granular_stdout = String::from_utf8_lossy(&granular.stdout);
    assert!(
        granular_stdout.contains(&workspace_id)
            && granular_stdout.contains("live run= process")
            && granular_stdout.contains("pass --compatibility-v042"),
        "specific overrides should remain independent: {granular_stdout}"
    );

    let preview = workspace
        .eph(&["system", "prune", "--force", "--dry-run"])
        .await;
    assert!(preview.status.success());
    let preview_stdout = String::from_utf8_lossy(&preview.stdout);
    assert!(
        preview_stdout.contains(&workspace_id)
            && preview_stdout.contains("deadbeef")
            && preview_stdout.contains("cafebabecafebabe")
            && preview_stdout.contains("cannot be pruned safely"),
        "force preview should expose its complete safe scope: {preview_stdout}"
    );
    assert!(workspace_state.exists());
    assert!(legacy_state.exists());
    assert!(incomplete_current_state.exists());

    let prune = workspace.eph(&["system", "prune", "--force"]).await;
    assert!(
        prune.status.success(),
        "non-interactive forced prune failed: {}",
        String::from_utf8_lossy(&prune.stderr)
    );
    assert!(!workspace_state.exists());
    assert!(!legacy_state.exists());
    assert!(incomplete_current_state.exists());
}
