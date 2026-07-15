use std::path::{Path, PathBuf};
use std::process::Output;

struct PruneHookWorkspace {
    workspace: Option<tempfile::TempDir>,
    workspace_path: PathBuf,
    state_root: tempfile::TempDir,
    markers: tempfile::TempDir,
}

impl PruneHookWorkspace {
    fn new(eph: &str) -> Self {
        let workspace = tempfile::tempdir().expect("failed to create test workspace");
        std::fs::write(workspace.path().join(".eph"), eph).expect("failed to write test .eph");
        Self {
            workspace_path: workspace.path().to_path_buf(),
            workspace: Some(workspace),
            state_root: tempfile::tempdir().expect("failed to create test state root"),
            markers: tempfile::tempdir().expect("failed to create marker directory"),
        }
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.markers.path().join(name)
    }

    fn rewrite_eph(&self, eph: &str) {
        std::fs::write(self.workspace_path.join(".eph"), eph).expect("failed to rewrite test .eph");
    }

    fn remove_workspace(&mut self) {
        self.workspace
            .take()
            .expect("workspace already removed")
            .close()
            .expect("failed to remove test workspace");
    }

    async fn eph(&self, args: &[&str]) -> Output {
        self.command(args, &self.workspace_path).await
    }

    async fn prune(&self, args: &[&str]) -> Output {
        let cwd = if self.workspace_path.is_dir() {
            &self.workspace_path
        } else {
            self.state_root.path()
        };
        self.command(args, cwd).await
    }

    async fn command(&self, args: &[&str], cwd: &Path) -> Output {
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
}

impl Drop for PruneHookWorkspace {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_eph"))
            .args(["system", "prune", "--force"])
            .current_dir(self.state_root.path())
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

#[cfg(unix)]
fn append_marker(path: &Path, value: &str) -> String {
    format!("printf '%s\\n' '{value}' >> '{}'", path.display())
}

#[cfg(windows)]
fn append_marker(path: &Path, value: &str) -> String {
    format!("echo {value}>>{}", path.display())
}

#[cfg(unix)]
fn write_variable(path: &Path, name: &str) -> String {
    format!("printf '%s' \"${name}\" > '{}'", path.display())
}

#[cfg(windows)]
fn write_variable(path: &Path, name: &str) -> String {
    format!("echo %{name}%>{}", path.display())
}

#[cfg(unix)]
fn write_cwd(path: &Path) -> String {
    format!("pwd > '{}'", path.display())
}

#[cfg(windows)]
fn write_cwd(path: &Path) -> String {
    format!("cd >{}", path.display())
}

#[cfg(unix)]
fn path_program_marker(path: &Path) -> String {
    format!("git --version > '{}'", path.display())
}

#[cfg(windows)]
fn path_program_marker(path: &Path) -> String {
    format!("git --version > {}", path.display())
}

#[cfg(unix)]
fn failing_hook() -> &'static str {
    "printf 'hook-stdout'; printf 'hook-stderr' >&2; exit 23"
}

#[cfg(windows)]
fn failing_hook() -> &'static str {
    "echo hook-stdout& echo hook-stderr 1>&2& exit /b 23"
}

fn assert_success(operation: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{operation} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn lifecycle_config(marker: &Path) -> String {
    format!(
        "[app]\nrun={}\npre-clean={}\npre-stop={}\npost-stop={}\npost-clean={}\n",
        long_running_command(),
        append_marker(marker, "pre-clean"),
        append_marker(marker, "pre-stop"),
        append_marker(marker, "post-stop"),
        append_marker(marker, "post-clean"),
    )
}

#[tokio::test]
async fn force_prune_runs_the_full_lifecycle_for_a_live_service() {
    let workspace = PruneHookWorkspace::new("");
    let marker = workspace.marker("order");
    workspace.rewrite_eph(&lifecycle_config(&marker));
    let up = workspace.eph(&["up"]).await;
    assert_success("eph up", &up);

    let prune = workspace.prune(&["system", "prune", "--force"]).await;
    assert_success("eph system prune --force", &prune);

    assert_eq!(
        std::fs::read_to_string(marker).unwrap().replace('\r', ""),
        "pre-clean\npre-stop\npost-stop\npost-clean\n"
    );
}

#[tokio::test]
async fn recorded_backend_controls_liveness_after_the_source_kind_changes() {
    let workspace = PruneHookWorkspace::new("[app]\nimage=alpine:3.21\ncommand=sleep 300\n");
    assert_success("eph up", &workspace.eph(&["up"]).await);

    let marker = workspace.marker("recorded-backend");
    workspace.rewrite_eph(&lifecycle_config(&marker));

    let prune = workspace.prune(&["system", "prune", "--force"]).await;
    assert_success("eph system prune --force", &prune);

    assert_eq!(
        std::fs::read_to_string(marker).unwrap().replace('\r', ""),
        "pre-clean\npre-stop\npost-stop\npost-clean\n"
    );
}

#[tokio::test]
async fn prune_runs_only_clean_hooks_for_an_already_stopped_service() {
    let workspace = PruneHookWorkspace::new("");
    let marker = workspace.marker("stopped-order");
    workspace.rewrite_eph(&lifecycle_config(&marker));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    assert_success(
        "eph down --skip-hooks",
        &workspace.eph(&["down", "--skip-hooks"]).await,
    );

    let prune = workspace.prune(&["system", "prune", "--force"]).await;
    assert_success("eph system prune --force", &prune);

    assert_eq!(
        std::fs::read_to_string(marker).unwrap().replace('\r', ""),
        "pre-clean\npost-clean\n"
    );
}

#[tokio::test]
async fn missing_worktree_uses_the_saved_snapshot_and_state_directory_cwd() {
    let mut workspace = PruneHookWorkspace::new("");
    let marker = workspace.marker("missing-worktree");
    let cwd_marker = workspace.marker("hook-cwd");
    let path_marker = workspace.marker("path-program");
    let config = format!(
        "[box]\nimage=alpine:3.21\ncommand=sleep 300\npre-clean={}\npre-stop={}\npost-stop={}\npost-clean={}\n",
        path_program_marker(&path_marker),
        append_marker(&marker, "pre-stop"),
        append_marker(&marker, "post-stop"),
        write_cwd(&cwd_marker),
    );
    workspace.rewrite_eph(&config);
    assert_success("eph up", &workspace.eph(&["up"]).await);
    let state_dir = workspace.state_dir().await;
    workspace.remove_workspace();

    let prune = workspace.prune(&["system", "prune", "--force"]).await;
    assert_success("eph system prune --force", &prune);

    assert_eq!(
        std::fs::read_to_string(marker).unwrap().replace('\r', ""),
        "pre-stop\npost-stop\n"
    );
    assert!(
        std::fs::read_to_string(path_marker)
            .unwrap()
            .starts_with("git version")
    );
    assert_eq!(
        PathBuf::from(std::fs::read_to_string(cwd_marker).unwrap().trim()),
        state_dir
    );
    assert!(!state_dir.exists());
}

#[tokio::test]
async fn saved_ports_resolve_top_level_hook_environment_without_the_worktree() {
    let mut workspace = PruneHookWorkspace::new("");
    let marker = workspace.marker("saved-url");
    let config = format!(
        "APP_URL=http://localhost:${{box.port}}\n\n[box]\nimage=alpine:3.21\ncommand=sleep 300\nport=8080\npre-clean={}\n",
        write_variable(&marker, "APP_URL")
    );
    workspace.rewrite_eph(&config);
    assert_success("eph up", &workspace.eph(&["up"]).await);
    workspace.remove_workspace();

    let prune = workspace.prune(&["system", "prune", "--force"]).await;
    assert_success("eph system prune --force", &prune);

    let value = std::fs::read_to_string(marker).unwrap();
    assert!(
        value.trim().starts_with("http://localhost:")
            && value
                .trim()
                .strip_prefix("http://localhost:")
                .is_some_and(|port| port.parse::<u16>().is_ok()),
        "saved interpolation should resolve to the assigned port: {value:?}"
    );
}

#[tokio::test]
async fn every_hook_failure_warns_and_prune_still_removes_the_workspace() {
    let workspace = PruneHookWorkspace::new(&format!(
        "[app]\nrun={}\npre-clean={}\npre-stop={}\npost-stop={}\npost-clean={}\n",
        long_running_command(),
        failing_hook(),
        failing_hook(),
        failing_hook(),
        failing_hook(),
    ));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    let state_dir = workspace.state_dir().await;

    let prune = workspace.prune(&["system", "prune", "--force"]).await;
    assert_success("eph system prune --force", &prune);
    let stdout = String::from_utf8_lossy(&prune.stdout);

    for phase in ["pre-clean", "pre-stop", "post-stop", "post-clean"] {
        assert!(stdout.contains(phase), "missing {phase} warning: {stdout}");
    }
    assert!(
        stdout.matches("stdout:\nhook-stdout").count() == 4
            && stdout.matches("stderr:\nhook-stderr").count() == 4,
        "each warning should retain both output streams: {stdout}"
    );
    assert!(!state_dir.exists());
}

#[tokio::test]
async fn dry_run_never_executes_hooks() {
    let workspace = PruneHookWorkspace::new("");
    let marker = workspace.marker("dry-run");
    workspace.rewrite_eph(&lifecycle_config(&marker));
    assert_success("eph up", &workspace.eph(&["up"]).await);

    let preview = workspace
        .prune(&["system", "prune", "--force", "--dry-run"])
        .await;
    assert_success("eph system prune --force --dry-run", &preview);

    assert!(!marker.exists());
    assert!(workspace.state_dir().await.exists());
}

#[tokio::test]
async fn current_valid_eph_wins_and_invalid_eph_falls_back_to_saved_hooks() {
    let current = PruneHookWorkspace::new("");
    let old_marker = current.marker("old");
    let new_marker = current.marker("new");
    current.rewrite_eph(&format!(
        "[app]\nrun={}\npre-clean={}\n",
        long_running_command(),
        append_marker(&old_marker, "old")
    ));
    assert_success("eph up", &current.eph(&["up"]).await);
    assert_success(
        "eph down --skip-hooks",
        &current.eph(&["down", "--skip-hooks"]).await,
    );
    current.rewrite_eph(&format!(
        "[app]\nrun={}\npre-clean={}\n",
        long_running_command(),
        append_marker(&new_marker, "new")
    ));

    let current_prune = current
        .prune(&["system", "prune", "--force-non-empty", "--yes"])
        .await;
    assert_success("current-source prune", &current_prune);
    assert!(!old_marker.exists());
    assert_eq!(std::fs::read_to_string(new_marker).unwrap().trim(), "new");

    let fallback = PruneHookWorkspace::new("");
    let fallback_marker = fallback.marker("fallback");
    fallback.rewrite_eph(&format!(
        "[app]\nrun={}\npre-clean={}\n",
        long_running_command(),
        append_marker(&fallback_marker, "saved")
    ));
    assert_success("eph up", &fallback.eph(&["up"]).await);
    assert_success(
        "eph down --skip-hooks",
        &fallback.eph(&["down", "--skip-hooks"]).await,
    );
    fallback.rewrite_eph("[app]\nrun=\n");

    let fallback_prune = fallback
        .prune(&["system", "prune", "--force-non-empty", "--yes"])
        .await;
    assert_success("fallback-source prune", &fallback_prune);
    let stdout = String::from_utf8_lossy(&fallback_prune.stdout);
    assert!(
        stdout.contains("could not parse current") && stdout.contains("saved teardown snapshot"),
        "fallback should be visible in warnings: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(fallback_marker).unwrap().trim(),
        "saved"
    );
}

#[tokio::test]
async fn old_state_without_a_hook_snapshot_still_prunes_with_a_warning() {
    let workspace = PruneHookWorkspace::new("");
    let marker = workspace.marker("old-state");
    workspace.rewrite_eph(&format!(
        "[app]\nrun={}\npre-clean={}\n",
        long_running_command(),
        append_marker(&marker, "should-not-run")
    ));
    assert_success("eph up", &workspace.eph(&["up"]).await);
    assert_success(
        "eph down --skip-hooks",
        &workspace.eph(&["down", "--skip-hooks"]).await,
    );
    let state_dir = workspace.state_dir().await;
    let state_path = state_dir.join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state
        .as_object_mut()
        .expect("state should be an object")
        .remove("teardown_hooks");
    std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
    workspace.rewrite_eph("[app]\nrun=\n");

    let prune = workspace
        .prune(&["system", "prune", "--force-non-empty", "--yes"])
        .await;
    assert_success("old-state prune", &prune);
    let stdout = String::from_utf8_lossy(&prune.stdout);
    assert!(
        stdout.contains("teardown hooks are unavailable")
            && stdout.contains("no saved hook snapshot")
            && stdout.contains("could not parse current"),
        "missing compatibility warning: {stdout}"
    );
    assert!(!stdout.contains("using the saved teardown snapshot"));
    assert!(!marker.exists());
    assert!(!state_dir.exists());
}
