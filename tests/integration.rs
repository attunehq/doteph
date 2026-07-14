//! Integration tests for eph
//!
//! These tests exercise the full stack: parsing .eph files, starting services,
//! interpolating environment variables, and running lifecycle hooks.
//!
//! Tests assume Docker is running and available.
//!
//! The shared harness ([`TestWorkspace`] and friends) lives in
//! [`mod@common`]; heavyweight multi-service and concurrency stress tests live
//! in the separate `tests/stress.rs` binary.

use std::time::Duration;

use tokio::time::sleep;

mod common;
use common::{TestWorkspace, extract_port, parse_env_json};

#[cfg(unix)]
fn hook_touch_marker() -> &'static str {
    "touch post-start-ran"
}

#[cfg(windows)]
fn hook_touch_marker() -> &'static str {
    "type nul > post-start-ran"
}

#[cfg(unix)]
fn hook_write_env_lines() -> &'static str {
    r#"printf '%s\n%s\n%s\n' "$REDIS_URL" "$EPH_REDIS_PORT" "$EPH_WORKSPACE_ID" > hook-env"#
}

#[cfg(windows)]
fn hook_write_env_lines() -> &'static str {
    "(echo %REDIS_URL%& echo %EPH_REDIS_PORT%& echo %EPH_WORKSPACE_ID%) > hook-env"
}

#[cfg(unix)]
fn hook_write_redis_url(file: &str) -> String {
    format!(r#"printf '%s' "$REDIS_URL" > {file}"#)
}

#[cfg(windows)]
fn hook_write_redis_url(file: &str) -> String {
    format!(r#"echo %REDIS_URL%> {file}"#)
}

#[cfg(unix)]
fn hook_append_x() -> &'static str {
    "printf 'x' >> ran-count"
}

#[cfg(windows)]
fn hook_append_x() -> &'static str {
    "echo x>> ran-count"
}

#[cfg(unix)]
fn hook_success() -> &'static str {
    "true"
}

#[cfg(windows)]
fn hook_success() -> &'static str {
    "ver >NUL"
}

/// A hook that sleeps for roughly `secs` seconds, used to widen a lock-holding
/// window so a concurrency test's overlap is deterministic rather than a race
/// against process scheduling.
#[cfg(unix)]
fn hook_sleep(secs: u32) -> String {
    format!("sleep {secs}")
}

#[cfg(windows)]
fn hook_sleep(secs: u32) -> String {
    // `ping -n N` waits roughly N-1 seconds between its N echo requests, since
    // there is no bundled `sleep` on Windows.
    format!("ping -n {} 127.0.0.1 >NUL", secs + 1)
}

#[cfg(unix)]
fn hook_mark_and_wait(marker: &str, release: &str) -> String {
    format!("touch {marker}; while [ ! -f {release} ]; do sleep 0.05; done")
}

#[cfg(windows)]
fn hook_mark_and_wait(marker: &str, release: &str) -> String {
    format!(
        "type nul > {marker} & powershell -NoProfile -Command \
         \"while (-not (Test-Path '{release}')) {{ Start-Sleep -Milliseconds 50 }}\""
    )
}

/// A hook that writes the resolved value of env var `name` to `file`, so a test
/// can compare what a lifecycle hook saw against what `eph env` resolves.
#[cfg(unix)]
fn hook_write_var(name: &str, file: &str) -> String {
    format!(r#"printf '%s' "${name}" > {file}"#)
}

#[cfg(windows)]
fn hook_write_var(name: &str, file: &str) -> String {
    format!("echo %{name}%> {file}")
}

/// A hook that creates the named marker file (its existence is the signal).
#[cfg(unix)]
fn hook_touch(file: &str) -> String {
    format!("touch {file}")
}

#[cfg(windows)]
fn hook_touch(file: &str) -> String {
    format!("type nul > {file}")
}

/// A hook that appends `marker` to `file`, used to record the order in which the
/// lifecycle steps ran.
#[cfg(unix)]
fn hook_append(marker: &str, file: &str) -> String {
    format!("printf '{marker}' >> {file}")
}

#[cfg(windows)]
fn hook_append(marker: &str, file: &str) -> String {
    format!("echo {marker}>> {file}")
}

/// A `run=` command that records `marker` in `file` and then stays alive, so a
/// hook's write to the same file can be ordered against the service starting.
#[cfg(unix)]
fn run_append_and_wait(marker: &str, file: &str) -> String {
    format!("printf '{marker}' >> {file}; sleep 30")
}

#[cfg(windows)]
fn run_append_and_wait(marker: &str, file: &str) -> String {
    format!("echo {marker}>> {file}& ping -n 30 127.0.0.1 >NUL")
}

/// A `run=` command that just stays alive for roughly `secs` seconds: the
/// plain long-lived service fixture.
#[cfg(unix)]
fn run_sleep(secs: u32) -> String {
    format!("sleep {secs}")
}

#[cfg(windows)]
fn run_sleep(secs: u32) -> String {
    // ping waits ~1s between its probes; `>NUL` keeps the probe chatter out
    // of the captured log. There is no bundled `sleep` on Windows.
    format!("ping -n {} 127.0.0.1 >NUL", secs + 1)
}

/// A `run=` command that prints `marker` and then stays alive, so the marker
/// is readable from the captured log while the service still runs.
#[cfg(unix)]
fn run_echo_and_wait(marker: &str) -> String {
    format!("echo {marker} && sleep 300")
}

#[cfg(windows)]
fn run_echo_and_wait(marker: &str) -> String {
    format!("echo {marker}& ping -n 300 127.0.0.1 >NUL")
}

#[cfg(unix)]
fn foreground_wait_fixture(marker: &str, release: &str, exit: &str) -> (&'static str, String) {
    (
        "sh dev-wait.sh",
        format!(
            "touch {marker}\nwhile [ ! -f {release} ]; do sleep 0.05; done\n\
             while [ ! -f {exit} ]; do sleep 0.05; done\n"
        ),
    )
}

#[cfg(windows)]
fn foreground_wait_fixture(marker: &str, release: &str, exit: &str) -> (&'static str, String) {
    (
        "powershell -NoProfile -ExecutionPolicy Bypass -File dev-wait.ps1",
        format!(
            "$null = New-Item -ItemType File '{marker}'\n\
             while (-not (Test-Path '{release}')) {{ Start-Sleep -Milliseconds 50 }}\n\
             while (-not (Test-Path '{exit}')) {{ Start-Sleep -Milliseconds 50 }}\n"
        ),
    )
}

#[cfg(unix)]
fn healthcheck_file_exists(path: &str) -> String {
    format!("test -f {path}")
}

#[cfg(windows)]
fn healthcheck_file_exists(path: &str) -> String {
    format!("if exist {path} (exit /b 0) else exit /b 1")
}

async fn wait_for_file(path: &std::path::Path) -> bool {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !path.exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

async fn wait_for_file_text(path: &std::path::Path, needle: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if std::fs::read_to_string(path).is_ok_and(|contents| contents.contains(needle)) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

fn spawn_force_non_empty_prune(
    workspace: &std::path::Path,
    state_root: &str,
    stdout_path: &std::path::Path,
    stderr_path: &std::path::Path,
) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_eph"))
        .args(["system", "prune", "--force-non-empty", "--yes"])
        .current_dir(workspace)
        .env("EPH_STATE_ROOT", state_root)
        .stdout(std::process::Stdio::from(
            std::fs::File::create(stdout_path).expect("failed to create prune stdout capture"),
        ))
        .stderr(std::process::Stdio::from(
            std::fs::File::create(stderr_path).expect("failed to create prune stderr capture"),
        ))
        .kill_on_drop(true)
        .spawn()
        .expect("failed to run system prune")
}

/// A `run=` command that prints `marker` and then exits with a failure code.
#[cfg(unix)]
fn run_echo_then_fail(marker: &str) -> String {
    format!("echo {marker} && exit 1")
}

#[cfg(windows)]
fn run_echo_then_fail(marker: &str) -> String {
    format!("echo {marker}& exit 1")
}

/// A `run=` command that reports the injected `$PORT` env var as a
/// `BOUND_PORT=<n>` log line and stays alive.
#[cfg(unix)]
fn run_report_port() -> &'static str {
    r#"echo "BOUND_PORT=$PORT" && sleep 300"#
}

#[cfg(windows)]
fn run_report_port() -> &'static str {
    "echo BOUND_PORT=%PORT%& ping -n 300 127.0.0.1 >NUL"
}

/// A healthcheck that passes only when the injected `$PORT` env var equals
/// the eph-resolved `${web.port}`. The resolved port is never empty, so an
/// unset `$PORT` also fails the comparison.
#[cfg(unix)]
fn healthcheck_port_matches() -> &'static str {
    r#"test -n "$PORT" && test "${web.port}" = "$PORT""#
}

#[cfg(windows)]
fn healthcheck_port_matches() -> &'static str {
    // Bracket comparison instead of quotes: the command string reaches
    // `cmd /C` with std's backslash-escaped quoting, which cmd would read
    // literally.
    "if [%PORT%]==[${web.port}] (exit /b 0) else (exit /b 1)"
}

/// A `run=` command that appends the resolved `$DATABASE_URL` to `starts.log`
/// and stays alive; each (re)start leaves one line to observe.
#[cfg(unix)]
fn run_log_database_url() -> &'static str {
    r#"echo "$DATABASE_URL" >> starts.log; sleep 300"#
}

#[cfg(windows)]
fn run_log_database_url() -> &'static str {
    // Parenthesized so a value ending in a digit cannot turn `>>` into a
    // numbered-stream redirect (`...5>> file`).
    "(echo %DATABASE_URL%)>> starts.log& ping -n 300 127.0.0.1 >NUL"
}

/// A `run=` command that prints `tailline-1` through `tailline-5`, then stays
/// alive so the log can be tailed while the service runs.
#[cfg(unix)]
fn run_five_tail_lines() -> &'static str {
    "for i in 1 2 3 4 5; do echo tailline-$i; done; sleep 300"
}

#[cfg(windows)]
fn run_five_tail_lines() -> &'static str {
    "(for %i in (1 2 3 4 5) do @echo tailline-%i)& ping -n 300 127.0.0.1 >NUL"
}

/// A `run=` command that emits `{prefix}-N` lines on a steady cadence (~200ms
/// on Unix, ~1s on Windows) for long enough that a follow stream sees plenty.
#[cfg(unix)]
fn run_counter_lines(prefix: &str) -> String {
    format!("i=0; while [ $i -lt 100 ]; do echo {prefix}-$i; i=$((i+1)); sleep 0.2; done")
}

#[cfg(windows)]
fn run_counter_lines(prefix: &str) -> String {
    format!("for /L %i in (1,1,100) do @(echo {prefix}-%i& ping -n 2 127.0.0.1 >NUL)")
}

#[cfg(unix)]
fn print_env_command(name: &str) -> Vec<&str> {
    vec!["run", "printenv", name]
}

#[cfg(windows)]
fn print_env_command(name: &str) -> Vec<&'static str> {
    match name {
        "REDIS_URL" => vec!["run", "cmd", "/C", "echo %REDIS_URL%"],
        "EPH_REDIS_PORT" => vec!["run", "cmd", "/C", "echo %EPH_REDIS_PORT%"],
        _ => panic!("unsupported Windows test env var: {name}"),
    }
}

#[cfg(unix)]
fn exit_7_command() -> Vec<&'static str> {
    vec!["run", "sh", "-c", "exit 7"]
}

#[cfg(windows)]
fn exit_301_command() -> Vec<&'static str> {
    vec!["run", "cmd", "/C", "exit /B 301"]
}

#[cfg(unix)]
fn terminate_with_sigterm_command() -> Vec<&'static str> {
    vec!["run", "sh", "-c", "kill -TERM $$"]
}

#[cfg(windows)]
fn exit_7_command() -> Vec<&'static str> {
    vec!["run", "cmd", "/C", "exit /B 7"]
}

/// An `eph run` invocation whose command echoes back every argument it was
/// given, including several flag-shaped ones (`-v`, `-h`, `-V`, `--weird`).
/// Used to prove those tokens reached the command untouched rather than being
/// intercepted by eph's own flag parsing.
#[cfg(unix)]
fn echo_flags_command() -> Vec<&'static str> {
    vec![
        "run",
        "sh",
        "-c",
        "echo \"$@\"",
        "_",
        "-v",
        "-h",
        "-V",
        "--weird",
    ]
}

#[cfg(windows)]
fn echo_flags_command() -> Vec<&'static str> {
    vec!["run", "cmd", "/C", "echo", "-v", "-h", "-V", "--weird"]
}

// ============================================================================
// Check Functions
// ============================================================================

/// Check that parsing an .eph file succeeds and reports expected services
#[track_caller]
fn check_parse(eph_content: &str, expected_services: &[&str], expected_env_count: usize) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let ws = TestWorkspace::new(eph_content);
        let output = ws.eph_ok(&["check"]).await;

        for service in expected_services {
            assert!(
                output.contains(service),
                "Expected service '{}' not found in output:\n{}",
                service,
                output
            );
        }

        let env_line = format!("Environment variables: {}", expected_env_count);
        assert!(
            output.contains(&env_line),
            "Expected '{}' not found in output:\n{}",
            env_line,
            output
        );
    });
}

// ============================================================================
// Parser Tests
// ============================================================================

#[test]
fn parse_minimal() {
    check_parse(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
        &["redis"],
        0,
    );
}

#[test]
fn parse_with_env_vars() {
    check_parse(
        r#"
APP_NAME=test
DEBUG=true

[postgres]
image=postgres:16
port=5432
"#,
        &["postgres"],
        2,
    );
}

#[test]
fn parse_multiple_services() {
    check_parse(
        r#"
[postgres]
image=postgres:16
port=5432

[redis]
image=redis:7
port=6379

[minio]
image=minio/minio
port.api=9000
port.console=9001
"#,
        &["postgres", "redis", "minio"],
        0,
    );
}

#[test]
fn parse_with_interpolation() {
    check_parse(
        r#"
[postgres]
image=postgres:16
port=5432
env.POSTGRES_USER=dev
env.POSTGRES_DB=test

[env]
DATABASE_URL=postgres://dev:dev@localhost:${postgres.port}/test
"#,
        &["postgres"],
        1,
    );
}

// ============================================================================
// Roles Tests (no Docker: parsing, validation, and selection only)
// ============================================================================

/// `eph check` reports each service's role and the resulting bring-up order when
/// the file uses roles, so the dependency-vs-app split is visible without Docker.
#[tokio::test]
async fn check_reports_roles_and_bring_up_order() {
    let ws = TestWorkspace::new(
        r#"
roles_order=dep,app

[web]
run=serve
role=app

[postgres]
image=postgres:16
role=dep
"#,
    );

    let out = ws.eph_ok(&["check"]).await;
    assert!(out.contains("postgres [dep]"), "roles not shown:\n{out}");
    assert!(out.contains("web [app]"), "roles not shown:\n{out}");
    // dep comes up before app regardless of declaration order.
    assert!(
        out.contains("Bring-up order: postgres, web"),
        "bring-up order missing or wrong:\n{out}"
    );
}

/// `--role` on a file that does not define roles is a clear error, and it fails
/// during selection (before touching Docker), so it is safe to assert here.
#[tokio::test]
async fn up_role_without_roles_order_errors() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let out = ws.eph(&["up", "--role", "dep"]).await;
    assert!(!out.status.success(), "up --role should fail without roles");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not define roles"),
        "expected a no-roles error, got: {stderr}"
    );
}

/// A file that tags a role but omits `roles_order` is rejected by `eph check`,
/// enforcing the mutual-completeness invariant with a message naming roles_order.
#[tokio::test]
async fn check_rejects_role_without_roles_order() {
    let ws = TestWorkspace::new("[postgres]\nimage=postgres:16\nrole=dep\n");
    let out = ws.eph(&["check"]).await;
    assert!(
        !out.status.success(),
        "check should reject a role without roles_order"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("roles_order"),
        "expected roles_order in the error, got: {stderr}"
    );
}

// ============================================================================
// Service Lifecycle Tests
// ============================================================================

#[tokio::test]
async fn service_redis_starts_and_stops() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[env]
REDIS_URL=redis://localhost:${redis.port}
"#,
    );

    // Start
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Verify running
    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("redis") && status.contains("localhost:"));

    // Check env interpolation happened
    let env = ws.eph_ok(&["env", "-f", "json"]).await;
    let env_map = parse_env_json(&env);
    let redis_url = env_map.get("REDIS_URL").expect("REDIS_URL not found");
    assert!(
        !redis_url.contains("${"),
        "REDIS_URL not interpolated: {}",
        redis_url
    );
    assert!(redis_url.starts_with("redis://localhost:"));

    // Stop
    ws.eph_ok(&["down", "redis"]).await;

    // Verify stopped
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("stopped") || status.contains("No services running"),
        "Service should be stopped: {}",
        status
    );
}

#[tokio::test]
async fn service_postgres_with_env_vars() {
    let ws = TestWorkspace::new(
        r#"
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=testuser
env.POSTGRES_PASSWORD=testpass
env.POSTGRES_DB=testdb

[env]
DATABASE_URL=postgres://testuser:testpass@localhost:${postgres.port}/testdb
"#,
    );

    // Start
    ws.eph_ok(&["up", "postgres"]).await;
    sleep(Duration::from_secs(2)).await;

    // Verify running
    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("postgres"));

    // Check DATABASE_URL is properly interpolated
    let env = ws.eph_ok(&["env", "-f", "json"]).await;
    let env_map = parse_env_json(&env);
    let db_url = env_map.get("DATABASE_URL").expect("DATABASE_URL not found");

    assert!(db_url.starts_with("postgres://testuser:testpass@localhost:"));
    assert!(db_url.ends_with("/testdb"));
    assert!(!db_url.contains("${"));

    // Cleanup
    ws.eph_ok(&["down"]).await;
}

#[tokio::test]
async fn service_multiple_ports() {
    // Use redis with a custom exposed port to test multiple port mappings
    // We expose both 6379 (redis) and 6380 (fake second port, same container)
    let ws = TestWorkspace::new(
        r#"
[multi]
image=redis:7-alpine
port.primary=6379
port.secondary=6380

[env]
PRIMARY_URL=redis://localhost:${multi.port.primary}
SECONDARY_URL=redis://localhost:${multi.port.secondary}
"#,
    );

    // Start
    ws.eph_ok(&["up", "multi"]).await;
    sleep(Duration::from_secs(2)).await;

    // Check both ports are interpolated
    let env = ws.eph_ok(&["env", "-f", "json"]).await;
    let env_map = parse_env_json(&env);

    let primary = env_map.get("PRIMARY_URL").expect("PRIMARY_URL not found");
    let secondary = env_map
        .get("SECONDARY_URL")
        .expect("SECONDARY_URL not found");

    assert!(
        !primary.contains("${"),
        "PRIMARY_URL not interpolated: {}",
        primary
    );
    assert!(
        !secondary.contains("${"),
        "SECONDARY_URL not interpolated: {}",
        secondary
    );

    // Ports should be different (Docker assigns different host ports)
    let primary_port = extract_port(primary).expect("Could not extract primary port");
    let secondary_port = extract_port(secondary).expect("Could not extract secondary port");
    assert_ne!(
        primary_port, secondary_port,
        "Primary and secondary ports should be different"
    );

    // Cleanup
    ws.eph_ok(&["down"]).await;
}

#[tokio::test]
async fn workspace_isolation() {
    // Create two workspaces with the same service
    let ws1 = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    let ws2 = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    // Start redis in both
    ws1.eph_ok(&["up", "redis"]).await;
    ws2.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Both should be running (different containers)
    let status1 = ws1.eph_ok(&["status"]).await;
    let status2 = ws2.eph_ok(&["status"]).await;

    assert!(status1.contains("redis") && status1.contains("localhost:"));
    assert!(status2.contains("redis") && status2.contains("localhost:"));

    // Get their ports - should be different
    let info1 = ws1.eph_ok(&["info"]).await;
    let info2 = ws2.eph_ok(&["info"]).await;

    // Container prefixes should be different
    assert_ne!(
        info1, info2,
        "Workspace infos should be different (different IDs)"
    );

    // Cleanup
    ws1.eph_ok(&["down"]).await;
    ws2.eph_ok(&["down"]).await;
}

// ============================================================================
// Environment Variable Tests
// ============================================================================

#[tokio::test]
async fn env_format_export() {
    let ws = TestWorkspace::new(
        r#"
APP_NAME=testapp
DEBUG=true
"#,
    );

    let output = ws.eph_ok(&["env", "-f", "export"]).await;
    assert!(output.contains("export APP_NAME=\"testapp\""));
    assert!(output.contains("export DEBUG=\"true\""));
}

#[tokio::test]
async fn env_format_fish() {
    let ws = TestWorkspace::new(
        r#"
APP_NAME=testapp
DEBUG=true
"#,
    );

    let output = ws.eph_ok(&["env", "-f", "fish"]).await;
    assert!(output.contains("set -gx APP_NAME \"testapp\""));
    assert!(output.contains("set -gx DEBUG \"true\""));
}

#[tokio::test]
async fn env_format_json() {
    let ws = TestWorkspace::new(
        r#"
APP_NAME=testapp
DEBUG=true
"#,
    );

    let output = ws.eph_ok(&["env", "-f", "json"]).await;
    let env = parse_env_json(&output);
    assert_eq!(env.get("APP_NAME"), Some(&"testapp".to_string()));
    assert_eq!(env.get("DEBUG"), Some(&"true".to_string()));
}

// ============================================================================
// Lifecycle Hook Tests
// ============================================================================

/// A `pre-start` hook runs before its service is created.
#[tokio::test]
async fn pre_start_hook_runs() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-start={}
"#,
        hook_touch("pre-start-ran")
    ));

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    assert!(
        ws.path().join("pre-start-ran").exists(),
        "pre-start hook did not run"
    );

    ws.eph_ok(&["down"]).await;
}

/// Read-only eph commands invoked by a hook must not contend with the parent
/// lifecycle transaction. Hooks commonly inspect the environment they are
/// preparing, and waiting for the parent's lock would deadlock both processes.
#[tokio::test]
async fn pre_start_hook_can_run_eph_status() {
    let eph = env!("CARGO_BIN_EXE_eph");
    let ws = TestWorkspace::new(&format!(
        "[app]\nrun={}\npre-start={eph} status > hook-status\n",
        run_sleep(300)
    ));

    tokio::time::timeout(Duration::from_secs(30), ws.eph_ok(&["up"]))
        .await
        .expect("a read-only eph command in a hook must not wait on the lifecycle lock");

    let hook_status = std::fs::read_to_string(ws.path().join("hook-status"))
        .expect("the pre-start hook did not capture eph status");
    assert!(
        hook_status.contains("No services running"),
        "the hook should inspect the pre-start state: {hook_status}"
    );
    ws.eph_ok(&["down"]).await;
}

/// A `pre-start` hook runs *before* the service it precedes boots. The `run=`
/// app and the hook both append to one file, so the recorded order proves the
/// hook ran first.
#[tokio::test]
async fn pre_start_runs_before_service() {
    let ws = TestWorkspace::new(&format!(
        r#"
[app]
run={}
pre-start={}
"#,
        run_append_and_wait("app", "order"),
        hook_append("pre", "order")
    ));

    ws.eph_ok(&["up", "app"]).await;
    sleep(Duration::from_secs(1)).await;

    let order = std::fs::read_to_string(ws.path().join("order"))
        .expect("neither the hook nor the app wrote order");
    let pre = order.find("pre");
    let app = order.find("app");
    assert!(
        matches!((pre, app), (Some(p), Some(a)) if p < a),
        "pre-start should run before the service started, got: {order:?}"
    );

    ws.eph_ok(&["down"]).await;
}

/// A `pre-start` hook sees eph's resolved environment, the same way `post-start`
/// does. Because it runs before its own service, a backing service it references
/// (started earlier in start order) still resolves.
#[tokio::test]
async fn pre_start_hook_receives_resolved_env() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379

[app]
run={}
pre-start={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        run_append_and_wait("app", "order"),
        hook_write_redis_url("pre-start-env")
    ));

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let redis_url = ws
        .env_json()
        .await
        .get("REDIS_URL")
        .expect("REDIS_URL not found")
        .clone();

    let captured = std::fs::read_to_string(ws.path().join("pre-start-env"))
        .expect("pre-start hook did not write pre-start-env");
    let captured = captured.trim_end_matches(['\r', '\n']);
    assert_eq!(
        captured, redis_url,
        "pre-start hook saw a stale or unresolved REDIS_URL"
    );

    ws.eph_ok(&["down"]).await;
}

/// A `pre-start` hook sees its own `run=` service's port: the port is reserved
/// before hooks run, so a top-level variable derived from it (the
/// `APP_URL=http://localhost:${web.port}` pattern) resolves in the hook's
/// environment and matches the port the app actually binds. This exact shape
/// used to abort `eph up` with "could not resolve environment" even though the
/// hook never read the variable.
///
/// `#[cfg(unix)]`, like the other long-lived `run=` + env tests
/// (`run_service_auto_port_is_stable_across_restart` and friends): on Windows
/// the spawned service inherits copies of eph's own stdout/stderr handles
/// (`CreateProcess` with `bInheritHandles` leaks every inheritable handle, not
/// just the redirected stdio), so the harness's piped `eph up` does not see EOF
/// until the service dies and the test starves. The Docker-backed CI job that
/// runs this suite is Linux, where the assertion is meaningful.
#[cfg(unix)]
#[tokio::test]
async fn pre_start_hook_sees_own_service_auto_port() {
    // The app writes the PORT it was handed before parking, so the test can
    // pin the whole chain: reserved port -> hook environment -> injected
    // process environment.
    let ws = TestWorkspace::new(&format!(
        r#"
APP_URL=http://localhost:${{web.port}}

[web]
run=printf '%s' "$PORT" > bound-port; sleep 300
port=auto
env.PORT=${{web.port}}
pre-start={}
"#,
        hook_write_var("APP_URL", "pre-start-url")
    ));

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let app_url = ws
        .env_json()
        .await
        .get("APP_URL")
        .expect("APP_URL not found")
        .clone();
    assert!(
        !app_url.contains("${"),
        "APP_URL should be fully resolved once the app is up, got: {app_url}"
    );

    let captured = std::fs::read_to_string(ws.path().join("pre-start-url"))
        .expect("pre-start hook did not write pre-start-url");
    let captured = captured.trim_end_matches(['\r', '\n']);
    // The hook saw the same URL the live environment reports...
    assert_eq!(
        captured, app_url,
        "pre-start hook saw a different port than the app was started on"
    );

    // ...and the process itself was handed that same port, so the value the
    // hook captured is the one the app actually serves on.
    let bound = std::fs::read_to_string(ws.path().join("bound-port"))
        .expect("the app did not record its injected PORT");
    let hook_port = extract_port(captured).expect("pre-start-url should contain a port");
    assert_eq!(
        bound.trim().parse::<u16>().ok(),
        Some(hook_port),
        "the app's injected PORT should match the port the pre-start hook saw"
    );

    ws.eph_ok(&["down"]).await;
}

/// An earlier service's `pre-start` hook sees a *later* `run=` service's port:
/// every managed app's port is reserved before any hook runs, not just the
/// hook's own service's.
///
/// `#[cfg(unix)]` for the same Windows handle-inheritance reason as
/// `pre_start_hook_sees_own_service_auto_port`.
#[cfg(unix)]
#[tokio::test]
async fn pre_start_hook_sees_later_run_service_port() {
    let ws = TestWorkspace::new(&format!(
        r#"
APP_URL=http://localhost:${{web.port}}

[first]
run=sleep 300
pre-start={}

[web]
run=sleep 300
port=auto
"#,
        hook_write_var("APP_URL", "first-pre-start-url")
    ));

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let app_url = ws
        .env_json()
        .await
        .get("APP_URL")
        .expect("APP_URL not found")
        .clone();

    let captured = std::fs::read_to_string(ws.path().join("first-pre-start-url"))
        .expect("first's pre-start hook did not write first-pre-start-url");
    let captured = captured.trim_end_matches(['\r', '\n']);
    assert_eq!(
        captured, app_url,
        "an earlier service's pre-start hook saw a different port than web got"
    );

    ws.eph_ok(&["down"]).await;
}

/// A failing `pre-start` hook aborts `eph up` before the service it precedes is
/// created, so the service never comes up.
#[tokio::test]
async fn pre_start_failure_aborts_up() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-start=exit 1
"#,
    );

    let out = ws.eph(&["up", "redis"]).await;
    assert!(
        !out.status.success(),
        "up should fail when a pre-start hook fails"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pre-start hook failed"),
        "expected a pre-start failure message, got: {stderr}"
    );

    // The service must not have started: a failing pre-start aborts before create.
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        !status.contains("localhost:"),
        "service should not be running after a failed pre-start: {status}"
    );

    ws.eph_ok(&["down"]).await;
}

/// `eph up --skip-hooks` brings services up without running their `pre-start`
/// hooks.
#[tokio::test]
async fn up_skip_hooks_does_not_run_pre_start() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-start={}
"#,
        hook_touch("pre-start-ran")
    ));

    ws.eph_ok(&["up", "--skip-hooks", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("redis") && status.contains("localhost:"));
    assert!(
        !ws.path().join("pre-start-ran").exists(),
        "pre-start should be skipped with --skip-hooks"
    );

    ws.eph_ok(&["down", "redis"]).await;
}

#[tokio::test]
async fn post_start_hook_runs() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        hook_touch_marker()
    ));

    // Start service
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Check marker file exists
    let marker = ws.path().join("post-start-ran");
    assert!(marker.exists(), "post-start hook did not run");

    // Cleanup
    ws.eph_ok(&["down"]).await;
}

/// A post-start hook should see eph's own resolved environment: the top-level
/// `.eph` variables (with `${service.port}` filled in) and the `EPH_*` metadata
/// variables.
#[tokio::test]
async fn post_start_hook_receives_resolved_env() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        hook_write_env_lines()
    ));

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let captured = std::fs::read_to_string(ws.path().join("hook-env"))
        .expect("post-start hook did not write hook-env");
    let lines: Vec<&str> = captured.lines().collect();

    // The hook's REDIS_URL must match what `eph env` resolves, fully expanded.
    let env_map = ws.env_json().await;
    let redis_url = env_map.get("REDIS_URL").expect("REDIS_URL not found");
    assert!(!redis_url.contains("${"), "REDIS_URL not resolved in env");
    assert_eq!(lines[0], redis_url, "hook saw a different REDIS_URL");

    // EPH_REDIS_PORT must equal the assigned host port inside REDIS_URL.
    let port = extract_port(redis_url).expect("no port in REDIS_URL");
    assert_eq!(lines[1], port.to_string(), "EPH_REDIS_PORT mismatch");

    // EPH_WORKSPACE_ID is always populated.
    assert!(!lines[2].is_empty(), "EPH_WORKSPACE_ID was empty");

    ws.eph_ok(&["down"]).await;
}

/// Because post-start hooks run only after every service is healthy (not at the
/// moment each service is created), a hook can reference a sibling service whose
/// port is interpolated into a top-level variable -- regardless of the order in
/// which the two services happened to start.
#[tokio::test]
async fn post_start_resolves_cross_service_refs() {
    // `worker`'s post-start reads REDIS_URL, which interpolates `redis`'s port.
    // Service iteration order is not deterministic, so under per-service hook
    // timing this would intermittently see an unresolved `${redis.port}`.
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379

[worker]
image=redis:7-alpine
port=6379
post-start={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        hook_write_redis_url("worker-saw")
    ));

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let captured = std::fs::read_to_string(ws.path().join("worker-saw"))
        .expect("worker post-start did not run");
    let captured = captured.trim_end_matches(['\r', '\n']);
    assert!(
        !captured.contains("${"),
        "cross-service ref not resolved in post-start: {captured}"
    );

    let env_map = ws.env_json().await;
    let redis_url = env_map.get("REDIS_URL").expect("REDIS_URL not found");
    assert_eq!(
        captured, redis_url,
        "worker hook saw a stale or unresolved REDIS_URL"
    );

    ws.eph_ok(&["down"]).await;
}

/// post-start hooks run on every `eph up`, including when a stopped container is
/// restarted -- not only on fresh creation.
#[tokio::test]
async fn post_start_reruns_on_restart() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start={}
"#,
        hook_append_x()
    ));

    // Fresh create -> post-start runs once.
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Stop but keep the container, then bring it back up (the restart path).
    ws.eph_ok(&["down"]).await;
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let count = std::fs::read_to_string(ws.path().join("ran-count")).unwrap_or_default();
    assert_eq!(
        count.matches('x').count(),
        2,
        "post-start should run on both create and restart, got {count:?}"
    );

    ws.eph_ok(&["down"]).await;
}

/// `eph up --skip-hooks` brings services up healthy but does not run their
/// post-start hooks.
#[tokio::test]
async fn up_skip_hooks_does_not_run_post_start() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start={}
"#,
        hook_touch_marker()
    ));

    ws.eph_ok(&["up", "--skip-hooks", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Service is up...
    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("redis") && status.contains("localhost:"));

    // ...but the hook did not run.
    assert!(
        !ws.path().join("post-start-ran").exists(),
        "post-start should be skipped with --skip-hooks"
    );

    ws.eph_ok(&["down", "redis"]).await;
}

/// `--skip-hooks` lets teardown bypass a failing pre-stop hook.
#[tokio::test]
async fn down_skip_hooks_bypasses_failing_pre_stop() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-stop=exit 1
"#,
    );

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // A plain down fails on the hook...
    let failed = ws.eph(&["down"]).await;
    assert!(!failed.status.success(), "down should fail on the bad hook");

    // ...but --skip-hooks tears it down anyway.
    ws.eph_ok(&["down", "--skip-hooks"]).await;
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("stopped") || status.contains("No services running"),
        "service should be stopped after --skip-hooks down: {status}"
    );
}

/// A failing pre-stop hook aborts `eph down` and leaves the service running so
/// the hook can be retried.
#[tokio::test]
async fn pre_stop_failure_aborts_down() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-stop=exit 1
"#,
    );

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // down must fail because the pre-stop hook fails.
    let out = ws.eph(&["down"]).await;
    assert!(
        !out.status.success(),
        "down should fail when a pre-stop hook fails"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pre-stop hook failed"),
        "expected a pre-stop failure message, got: {stderr}"
    );

    // The service is left running so the operator can fix and retry.
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("redis") && status.contains("localhost:"),
        "service should still be running after a failed down: {status}"
    );

    // Fix the hook, then tear down cleanly (also lets Drop's `eph down` succeed
    // rather than leaking the container).
    ws.write_file(
        ".eph",
        &format!(
            r#"
[redis]
image=redis:7-alpine
port=6379
pre-stop={}
"#,
            hook_success()
        ),
    );
    ws.eph_ok(&["down"]).await;
}

/// A failing `pre-stop` hook on a service that is **not running** must not break
/// `eph down`. `stop_all` iterates every service in the `.eph` file, so the hook
/// of a never-started service must be skipped rather than run (and fail).
#[tokio::test]
async fn pre_stop_skipped_for_non_running_service() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[never-started]
image=redis:7-alpine
port=6379
pre-stop=exit 1
"#,
    );

    // Bring up only redis; `never-started` stays down with its failing hook.
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // A full `eph down` iterates every service but must not run the stopped
    // service's pre-stop hook, so it succeeds.
    let out = ws.eph(&["down"]).await;
    assert!(
        out.status.success(),
        "down should not run pre-stop for a non-running service: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A targeted `eph down <service>` persists state, so the stopped service is
/// dropped from `state.json` immediately rather than lingering until the next
/// `eph status` reconciles it.
#[tokio::test]
async fn targeted_down_persists_state() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[cache]
image=redis:7-alpine
port=6379
"#,
    );

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    // Locate state.json via `eph info`.
    let info = ws.eph_ok(&["info"]).await;
    let state_dir = info
        .lines()
        .find_map(|l| l.strip_prefix("State directory: "))
        .expect("info should print the state directory");
    let state_path = std::path::Path::new(state_dir.trim()).join("state.json");

    let read_running_services = || {
        let contents = std::fs::read_to_string(&state_path).expect("state.json should exist");
        let state: serde_json::Value = serde_json::from_str(&contents).expect("valid state.json");
        state["services"]
            .as_object()
            .expect("state services should be an object")
            .clone()
    };
    let before = read_running_services();
    assert!(before.contains_key("redis") && before.contains_key("cache"));

    // Stop just one service.
    ws.eph_ok(&["down", "redis"]).await;

    let after = read_running_services();
    assert!(
        !after.contains_key("redis"),
        "redis should be gone from running services after a targeted down: {after:?}"
    );
    assert!(
        after.contains_key("cache"),
        "cache should remain in running services: {after:?}"
    );

    ws.eph_ok(&["down"]).await;
}

/// A pre-stop hook should receive the same resolved environment as post-start.
#[tokio::test]
async fn pre_stop_hook_receives_resolved_env() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-stop={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        hook_write_redis_url("pre-stop-env")
    ));

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let redis_url = ws
        .env_json()
        .await
        .get("REDIS_URL")
        .expect("REDIS_URL not found")
        .clone();

    // Stopping triggers the pre-stop hook.
    ws.eph_ok(&["down"]).await;

    let captured = std::fs::read_to_string(ws.path().join("pre-stop-env"))
        .expect("pre-stop hook did not write pre-stop-env");
    let captured = captured.trim_end_matches(['\r', '\n']);
    assert_eq!(
        captured, redis_url,
        "pre-stop hook saw a different REDIS_URL"
    );
}

/// A `post-stop` hook runs after its service is stopped, during `eph down`.
#[tokio::test]
async fn post_stop_hook_runs() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-stop={}
"#,
        hook_touch("post-stop-ran")
    ));

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // The hook must not run at startup, only on teardown.
    assert!(
        !ws.path().join("post-stop-ran").exists(),
        "post-stop should not run during up"
    );

    ws.eph_ok(&["down"]).await;
    assert!(
        ws.path().join("post-stop-ran").exists(),
        "post-stop hook did not run on down"
    );
}

/// A `post-stop` hook sees the same resolved environment as `pre-stop`: the
/// pre-teardown snapshot, so the now-stopped service's port still resolves.
#[tokio::test]
async fn post_stop_hook_receives_resolved_env() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-stop={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        hook_write_redis_url("post-stop-env")
    ));

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let redis_url = ws
        .env_json()
        .await
        .get("REDIS_URL")
        .expect("REDIS_URL not found")
        .clone();

    ws.eph_ok(&["down"]).await;

    let captured = std::fs::read_to_string(ws.path().join("post-stop-env"))
        .expect("post-stop hook did not write post-stop-env");
    let captured = captured.trim_end_matches(['\r', '\n']);
    assert_eq!(
        captured, redis_url,
        "post-stop hook saw a different REDIS_URL"
    );
}

/// A failing `post-stop` hook aborts the teardown with a clear message. The
/// service is already stopped by the time the hook runs.
#[tokio::test]
async fn post_stop_failure_aborts_down() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-stop=exit 1
"#,
    );

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let out = ws.eph(&["down"]).await;
    assert!(
        !out.status.success(),
        "down should fail when a post-stop hook fails"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("post-stop hook failed"),
        "expected a post-stop failure message, got: {stderr}"
    );

    // The service was still stopped even though the hook failed afterward.
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("stopped") || status.contains("No services running"),
        "service should be stopped after a post-stop failure: {status}"
    );
}

/// `--skip-hooks` lets teardown bypass a failing `post-stop` hook.
#[tokio::test]
async fn down_skip_hooks_bypasses_failing_post_stop() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-stop=exit 1
"#,
    );

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let failed = ws.eph(&["down"]).await;
    assert!(!failed.status.success(), "down should fail on the bad hook");

    ws.eph_ok(&["down", "--skip-hooks"]).await;
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("stopped") || status.contains("No services running"),
        "service should be stopped after --skip-hooks down: {status}"
    );
}

/// A failing `post-stop` hook on a service that never started must not break
/// `eph down`: like `pre-stop`, it is gated on the pre-teardown snapshot.
#[tokio::test]
async fn post_stop_skipped_for_non_running_service() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[never-started]
image=redis:7-alpine
port=6379
post-stop=exit 1
"#,
    );

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let out = ws.eph(&["down"]).await;
    assert!(
        out.status.success(),
        "down should not run post-stop for a non-running service: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Clean-specific hooks run only for `eph clean`, even when the service was
/// already stopped, and bracket the ordinary teardown hooks when it is live.
#[tokio::test]
async fn clean_hooks_run_only_for_clean_and_bracket_teardown() {
    let ws = TestWorkspace::new(&format!(
        r#"
[app]
run={}
pre-clean={}
pre-stop={}
post-stop={}
post-clean={}
"#,
        run_sleep(300),
        hook_append("preclean", "hook-order"),
        hook_append("prestop", "hook-order"),
        hook_append("poststop", "hook-order"),
        hook_append("postclean", "hook-order")
    ));

    ws.eph_ok(&["up"]).await;
    ws.eph_ok(&["down"]).await;

    let down_order = std::fs::read_to_string(ws.path().join("hook-order"))
        .expect("the stop hooks should record their order");
    assert!(down_order.contains("prestop"));
    assert!(down_order.contains("poststop"));
    assert!(!down_order.contains("preclean"));
    assert!(!down_order.contains("postclean"));

    std::fs::write(ws.path().join("hook-order"), "").unwrap();
    ws.eph_ok(&["clean"]).await;
    let stopped_clean_order = std::fs::read_to_string(ws.path().join("hook-order"))
        .expect("clean hooks should run for a stopped service");
    assert!(stopped_clean_order.contains("preclean"));
    assert!(stopped_clean_order.contains("postclean"));
    assert!(!stopped_clean_order.contains("prestop"));
    assert!(!stopped_clean_order.contains("poststop"));

    std::fs::write(ws.path().join("hook-order"), "").unwrap();
    ws.eph_ok(&["up"]).await;
    ws.eph_ok(&["clean"]).await;
    let clean_order = std::fs::read_to_string(ws.path().join("hook-order"))
        .expect("clean and stop hooks should record their order");
    let pre_clean = clean_order.find("preclean").unwrap();
    let pre_stop = clean_order.find("prestop").unwrap();
    let post_stop = clean_order.find("poststop").unwrap();
    let post_clean = clean_order.find("postclean").unwrap();
    assert!(
        pre_clean < pre_stop && pre_stop < post_stop && post_stop < post_clean,
        "expected pre-clean, pre-stop, post-stop, post-clean; got {clean_order:?}"
    );
}

/// Clean hooks retain the last resolved environment after `down`, including a
/// container's dynamically assigned host port, instead of failing before the
/// hook launches because the service is no longer live.
#[tokio::test]
async fn clean_hooks_resolve_last_ports_after_down() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-clean={}
post-clean={}

[env]
REDIS_URL=redis://localhost:${{redis.port}}
"#,
        hook_write_var("REDIS_URL", "pre-clean-url"),
        hook_write_var("REDIS_URL", "post-clean-url")
    ));

    ws.eph_ok(&["up"]).await;
    let expected = ws
        .env_json()
        .await
        .get("REDIS_URL")
        .expect("REDIS_URL should resolve while redis is running")
        .clone();
    ws.eph_ok(&["down"]).await;
    ws.eph_ok(&["clean"]).await;

    for file in ["pre-clean-url", "post-clean-url"] {
        let actual = std::fs::read_to_string(ws.path().join(file))
            .unwrap_or_else(|error| panic!("failed to read {file}: {error}"));
        assert_eq!(actual.trim(), expected);
    }
}

/// A failed pre-clean preserves the service, while a failed post-clean is
/// reported after its resources are gone. `--skip-hooks` remains the escape
/// hatch for either failure.
#[tokio::test]
async fn clean_hook_failures_preserve_phase_semantics() {
    let ws = TestWorkspace::new(&format!(
        "[app]\nrun={}\npre-clean=exit 1\n",
        run_sleep(300)
    ));
    ws.eph_ok(&["up"]).await;

    let failed_pre = ws.eph(&["clean"]).await;
    assert!(!failed_pre.status.success());
    let stderr = String::from_utf8_lossy(&failed_pre.stderr);
    assert!(
        stderr.contains("pre-clean hook failed"),
        "expected a pre-clean failure message, got: {stderr}"
    );
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("app"),
        "pre-clean failure should leave the service running: {status}"
    );

    ws.write_file(
        ".eph",
        &format!("[app]\nrun={}\npost-clean=exit 1\n", run_sleep(300)),
    );
    let failed_post = ws.eph(&["clean"]).await;
    assert!(!failed_post.status.success());
    let stderr = String::from_utf8_lossy(&failed_post.stderr);
    assert!(
        stderr.contains("post-clean hook failed"),
        "expected a post-clean failure message, got: {stderr}"
    );
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("No services running"),
        "post-clean failure should happen after teardown: {status}"
    );

    ws.eph_ok(&["clean", "--skip-hooks"]).await;
}

// ============================================================================
// eph run
// ============================================================================

/// `eph run` runs a command with the resolved environment overlaid, so a
/// top-level variable and `EPH_*` metadata are visible to the child.
#[tokio::test]
async fn eph_run_injects_resolved_env() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[env]
REDIS_URL=redis://localhost:${redis.port}
"#,
    );

    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let redis_url = ws
        .env_json()
        .await
        .get("REDIS_URL")
        .expect("REDIS_URL not found")
        .clone();

    // The command is executed directly (no shell), so use the platform's
    // environment-printing command rather than relying on eph to expand a shell
    // expression.
    let out = ws.eph_ok(&print_env_command("REDIS_URL")).await;
    assert_eq!(out.trim(), redis_url, "eph run did not inject REDIS_URL");

    let port_out = ws.eph_ok(&print_env_command("EPH_REDIS_PORT")).await;
    let port = extract_port(&redis_url).expect("no port in REDIS_URL");
    assert_eq!(port_out.trim(), port.to_string(), "EPH_REDIS_PORT mismatch");

    ws.eph_ok(&["down"]).await;
}

/// `eph run` propagates the child command's exit code.
#[tokio::test]
async fn eph_run_propagates_exit_code() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    // No `up` needed: `eph run` resolves whatever is running (nothing here) and
    // still execs the command. The child's exit code must surface unchanged.
    let output = ws.eph(&exit_7_command()).await;
    assert_eq!(
        output.status.code(),
        Some(7),
        "eph run did not propagate the child exit code"
    );
}

/// Windows process statuses are 32-bit values. `eph run` must not narrow them
/// through Rust's eight-bit `ExitCode` wrapper.
#[cfg(windows)]
#[tokio::test]
async fn eph_run_preserves_windows_exit_codes_above_255() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");

    let output = ws.eph(&exit_301_command()).await;

    assert_eq!(output.status.code(), Some(301));
}

/// A signal-terminated child maps to the conventional shell status instead of
/// the unrelated generic failure code 1.
#[cfg(unix)]
#[tokio::test]
async fn eph_run_maps_unix_signals_to_shell_status() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");

    let output = ws.eph(&terminate_with_sigterm_command()).await;

    assert_eq!(output.status.code(), Some(143));
}

/// A command is never launched with an unresolved top-level interpolation.
#[tokio::test]
async fn eph_run_refuses_an_unresolved_environment() {
    let ws = TestWorkspace::new(
        "[db]\nimage=postgres:16\nport=5432\n[env]\nDATABASE_URL=postgres://localhost:${db.port}/app\n",
    );

    let output = ws.eph(&exit_7_command()).await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DATABASE_URL") && stderr.contains("${db.port}"),
        "got: {stderr}"
    );
}

/// Regression for #15: a malformed `command=` override must fail closed even
/// when the service's container already exists (the reuse fast path), not only
/// on first create. The error is reported at `up` time, not silently smuggled
/// through as a single argv element.
#[tokio::test]
async fn malformed_command_override_fails_closed_on_reuse() {
    let ws = TestWorkspace::new(
        r#"
[box]
image=redis:7-alpine
command=sleep 3600
"#,
    );

    // First up creates the container and leaves it running.
    ws.eph_ok(&["up", "box"]).await;

    // Edit the file so `command=` now has an unbalanced quote.
    ws.write_file(
        ".eph",
        r#"
[box]
image=redis:7-alpine
command=sleep "3600
"#,
    );

    // The container already exists, so this goes through the reuse path. It must
    // still fail, with a clear message, rather than reusing the stale config.
    let output = ws.eph(&["up", "box"]).await;
    assert!(
        !output.status.success(),
        "malformed command= should fail even when the container already exists"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid command override for service 'box'"),
        "expected a command-override parse error, got: {stderr}"
    );

    // Restore a parseable file so normal lifecycle cleanup can identify the
    // already-running container.
    ws.write_file(
        ".eph",
        r#"
[box]
image=redis:7-alpine
command=sleep 3600
"#,
    );
    ws.eph_ok(&["down"]).await;
}

// ============================================================================
// Health Check Tests
// ============================================================================

#[tokio::test]
async fn healthcheck_waits_for_ready() {
    let ws = TestWorkspace::new(
        r#"
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=test
env.POSTGRES_PASSWORD=test
env.POSTGRES_DB=test
healthcheck=pg_isready -U test
ready-timeout=30

[env]
DATABASE_URL=postgres://test:test@localhost:${postgres.port}/test
"#,
    );

    // A successful `up` proves pg_isready passed: readiness failures and the
    // configured timeout are surfaced as command errors.
    ws.eph_ok(&["up", "postgres"]).await;

    // Service should be running and healthy
    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("postgres"));

    // Cleanup
    ws.eph_ok(&["down"]).await;
}

// ============================================================================
// Lifecycle Correctness Tests
// ============================================================================

/// Read the `Container prefix:` and `State directory:` lines out of `eph info`,
/// used throughout this section to locate a workspace's containers and
/// `state.json` without hardcoding eph's platform-specific data directory.
async fn container_prefix_and_state_dir(ws: &TestWorkspace) -> (String, std::path::PathBuf) {
    let info = ws.eph_ok(&["info"]).await;
    let prefix = info
        .lines()
        .find_map(|l| l.strip_prefix("Container prefix: "))
        .expect("info should print the container prefix")
        .trim()
        .to_string();
    let state_dir = info
        .lines()
        .find_map(|l| l.strip_prefix("State directory: "))
        .expect("info should print the state directory")
        .trim();
    (prefix, std::path::PathBuf::from(state_dir))
}

async fn docker_container_id(name: &str) -> String {
    let output = tokio::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", name])
        .output()
        .await
        .expect("failed to run docker inspect");
    assert!(
        output.status.success(),
        "docker inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test]
async fn changed_container_config_recreates_running_and_stopped_containers() {
    let ws = TestWorkspace::new("[box]\nimage=alpine:3.21\ncommand=sleep 300\nenv.MARKER=one\n");
    ws.eph_ok(&["up"]).await;
    let (prefix, _) = container_prefix_and_state_dir(&ws).await;
    let container = format!("{prefix}-box");
    let first = docker_container_id(&container).await;

    ws.write_file(
        ".eph",
        "[box]\nimage=alpine:3.21\ncommand=sleep 300\nenv.MARKER=two\n",
    );
    ws.eph_ok(&["up"]).await;
    let second = docker_container_id(&container).await;
    assert_ne!(first, second, "env drift should recreate a live container");

    ws.eph_ok(&["down"]).await;
    ws.write_file(
        ".eph",
        "[box]\nimage=alpine:3.21\ncommand=sleep 301\nenv.MARKER=two\n",
    );
    ws.eph_ok(&["up"]).await;
    let third = docker_container_id(&container).await;
    assert_ne!(
        second, third,
        "command drift should recreate a stopped container"
    );

    ws.clean().await;
}

#[tokio::test]
async fn failed_container_healthcheck_removes_the_container_before_retry() {
    let ws = TestWorkspace::new(
        "[box]\nimage=alpine:3.21\ncommand=sleep 300\nhealthcheck=false\nready-timeout=0\n",
    );
    let (prefix, _) = container_prefix_and_state_dir(&ws).await;
    let container = format!("{prefix}-box");

    for attempt in 1..=2 {
        let up = ws.eph(&["up"]).await;
        assert!(
            !up.status.success(),
            "unhealthy startup attempt {attempt} should fail"
        );
        assert!(
            common::docker_container_names(&container).await.is_empty(),
            "failed attempt {attempt} left a container that a retry could adopt"
        );
    }
}

#[tokio::test]
async fn dockerfile_context_change_rebuilds_and_recreates_the_container() {
    let ws = TestWorkspace::new("[box]\ndockerfile=Dockerfile\ncontext=.\ncommand=sleep 300\n");
    ws.write_file(
        "Dockerfile",
        "FROM alpine:3.21\nCOPY marker /marker\nCMD [\"sleep\", \"300\"]\n",
    );
    ws.write_file("marker", "one\n");
    ws.eph_ok(&["up"]).await;
    let (prefix, _) = container_prefix_and_state_dir(&ws).await;
    let container = format!("{prefix}-box");
    let first = docker_container_id(&container).await;

    ws.write_file("marker", "two\n");
    ws.eph_ok(&["up"]).await;
    let second = docker_container_id(&container).await;

    assert_ne!(
        first, second,
        "a changed Docker build context should replace the container"
    );
    ws.clean().await;
    common::docker_remove_image(&container).await;
}

#[tokio::test]
async fn dependency_port_change_restarts_a_run_service_with_resolved_env() {
    let first_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let second_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let first_port = first_listener.local_addr().unwrap().port();
    let second_port = second_listener.local_addr().unwrap().port();
    drop((first_listener, second_listener));
    let config = |port| {
        format!(
            "[db]\nrun={}\nport={port}\n\n[app]\nrun={}\nenv.DATABASE_URL=tcp://localhost:${{db.port}}\n",
            run_sleep(300),
            run_log_database_url()
        )
    };
    let ws = TestWorkspace::new(&config(first_port));
    ws.eph_ok(&["up"]).await;

    ws.write_file(".eph", &config(second_port));
    ws.eph_ok(&["up"]).await;

    let starts = std::fs::read_to_string(ws.path().join("starts.log")).unwrap();
    assert!(
        starts.contains(&format!("tcp://localhost:{first_port}")),
        "first={first_port}, second={second_port}, starts={starts:?}"
    );
    assert!(
        starts.contains(&format!("tcp://localhost:{second_port}")),
        "first={first_port}, second={second_port}, starts={starts:?}"
    );
    assert_eq!(
        starts.lines().count(),
        2,
        "the dependent should restart exactly once after its resolved env changes"
    );
    ws.eph_ok(&["down"]).await;
}

#[cfg(unix)]
#[tokio::test]
async fn source_type_change_stops_the_recorded_backend_before_replacement() {
    let ws = TestWorkspace::new("[worker]\nrun=echo $$ > worker.pid; sleep 300\n");
    ws.eph_ok(&["up"]).await;
    let pid: libc::pid_t = std::fs::read_to_string(ws.path().join("worker.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    ws.write_file(".eph", "[worker]\nimage=alpine:3.21\ncommand=sleep 300\n");
    ws.eph_ok(&["up"]).await;

    // SAFETY: signal 0 only probes whether this numeric PID exists.
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    assert!(
        !alive,
        "the previous process backend must be stopped before the container starts"
    );
    let (prefix, _) = container_prefix_and_state_dir(&ws).await;
    assert_eq!(
        common::docker_container_names(&format!("{prefix}-worker"))
            .await
            .len(),
        1
    );
    ws.clean().await;
}

/// Renaming (or deleting) a running service's section leaves its container
/// recorded in state under the old name. `eph down` must still find and stop
/// it via `stop_orphan`, and drop it from `state.json`, rather than leaking it
/// forever because it no longer appears in the `.eph` file.
#[tokio::test]
async fn down_stops_container_orphaned_by_a_service_rename() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let (prefix, state_dir) = container_prefix_and_state_dir(&ws).await;
    let state_path = state_dir.join("state.json");
    let old_container = format!("{prefix}-redis");

    assert_eq!(
        common::docker_container_names(&old_container).await,
        vec![old_container.clone()],
        "redis container should exist right after up"
    );
    let before = std::fs::read_to_string(&state_path).expect("state.json should exist after up");
    assert!(
        before.contains("redis"),
        "state.json should record redis: {before}"
    );

    // Rename the section: "redis" no longer appears in the .eph file, but its
    // container is still recorded under the old name.
    ws.write_file(
        ".eph",
        r#"
[cache]
image=redis:7-alpine
port=6379
"#,
    );

    // `--rm` so the orphan is fully removed, not merely stopped: `stop_orphan`
    // is never reached again for this name (it is gone from the .eph file), so
    // a stopped-but-present container here would be permanently unmanaged.
    ws.eph_ok(&["down", "--rm"]).await;

    assert!(
        common::docker_container_names(&old_container)
            .await
            .is_empty(),
        "the renamed-away container should have been stopped and removed by down"
    );
    let after = std::fs::read_to_string(&state_path).expect("state.json should still exist");
    assert!(
        !after.contains("redis"),
        "state.json should no longer record the orphaned 'redis' entry: {after}"
    );
}

/// `eph clean` on a workspace whose services were never started must report
/// zero resources removed, not a count derived from the services the `.eph`
/// file declares.
#[tokio::test]
async fn clean_reports_zero_for_never_started_services() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[cache]
image=redis:7-alpine
port=6379
"#,
    );

    let output = ws.eph_ok(&["clean"]).await;
    assert!(
        output.contains("Services stopped and removed: 0"),
        "clean should report zero removed services for a workspace that never \
         started anything: {output}"
    );
    assert!(
        output.contains("Named volumes removed: 0"),
        "clean should report zero removed volumes for a workspace that never \
         started anything: {output}"
    );
}

/// After a real `eph up` then `eph clean`, the reported counts reflect what was
/// actually stopped and removed.
#[tokio::test]
async fn clean_reports_actual_counts_after_up() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[cache]
image=redis:7-alpine
port=6379
"#,
    );

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let output = ws.eph_ok(&["clean"]).await;
    assert!(
        output.contains("Services stopped and removed: 2"),
        "clean should count both started services as removed: {output}"
    );
    assert!(
        output.contains("Named volumes removed: 0"),
        "no named volumes were declared: {output}"
    );
    assert!(
        output.contains("Persisted state: removed"),
        "the state directory should have been removed: {output}"
    );
}

/// A corrupt `state.json` (a crash mid-write, a bad hand edit) must not break
/// eph: the broken file is quarantined to `state.json.corrupt` rather than
/// failing the command, and a subsequent `eph up` still finds the container
/// Docker already has by name.
#[tokio::test]
async fn corrupt_state_file_is_quarantined_and_up_recovers() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let (_prefix, state_dir) = container_prefix_and_state_dir(&ws).await;
    let state_path = state_dir.join("state.json");
    let corrupt_path = state_dir.join("state.json.corrupt");

    // Simulate corruption: a crash mid-write or a bad hand edit.
    std::fs::write(&state_path, "{not json").expect("failed to corrupt state.json");

    let status = ws.eph(&["status"]).await;
    assert!(
        status.status.success(),
        "status should tolerate a corrupt state file: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        corrupt_path.exists(),
        "the corrupt state file should be quarantined to state.json.corrupt"
    );
    assert_eq!(
        std::fs::read_to_string(&corrupt_path).unwrap(),
        "{not json",
        "the quarantined file should keep the original corrupt contents"
    );
    assert!(
        !state_path.exists(),
        "state.json should have been moved aside, not left in place"
    );

    // The container itself was never touched: eph up still finds it in Docker
    // by name, even though its state entry was just wiped.
    ws.eph_ok(&["up"]).await;
    let running = ws.eph_ok(&["status"]).await;
    assert!(
        running.contains("redis") && running.contains("localhost:"),
        "up should recover the still-running container after quarantine: {running}"
    );

    ws.eph_ok(&["down"]).await;
}

/// When the first of two services starts fine and the second fails, `eph up`
/// must still persist the first service's state before returning the error, so
/// a subsequent `eph down` can find and stop it rather than leaking it.
#[tokio::test]
async fn up_persists_earlier_services_when_a_later_one_fails() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[broken]
image=does-not-exist-eph-test:latest
port=1234
"#,
    );

    let (prefix, state_dir) = container_prefix_and_state_dir(&ws).await;
    let state_path = state_dir.join("state.json");
    let redis_container = format!("{prefix}-redis");

    let output = ws.eph(&["up"]).await;
    assert!(
        !output.status.success(),
        "up should fail when the second service's image cannot be found"
    );

    assert_eq!(
        common::docker_container_names(&redis_container).await,
        vec![redis_container.clone()],
        "redis should have started before broken failed"
    );
    let state_contents =
        std::fs::read_to_string(&state_path).expect("state.json should exist after the failure");
    assert!(
        state_contents.contains("redis"),
        "redis should be persisted despite the later failure: {state_contents}"
    );
    assert!(
        !state_contents.contains("broken"),
        "broken never started, so it should not be recorded: {state_contents}"
    );

    // A subsequent down must find and stop the service that started before the
    // failure, even though the last `up` never returned successfully. `--rm` so
    // the container is fully gone, not merely stopped.
    ws.eph_ok(&["down", "--rm"]).await;
    assert!(
        common::docker_container_names(&redis_container)
            .await
            .is_empty(),
        "down should stop the service recorded before the failed up"
    );
}

/// A container service's `env.KEY=${other.port}` is resolved to the other
/// service's actual host port both inside the container's own environment and
/// wherever a lifecycle hook sees the resolved environment.
#[tokio::test]
async fn container_env_resolves_sibling_service_port() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379

[app]
image=redis:7-alpine
port=6379
env.OTHER_PORT=${{redis.port}}
post-start={}

[env]
REDIS_PORT=${{redis.port}}
"#,
        hook_write_var("OTHER_PORT", "hook-other-port")
    ));

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let (prefix, _state_dir) = container_prefix_and_state_dir(&ws).await;
    let redis_port = ws
        .env_json()
        .await
        .get("REDIS_PORT")
        .expect("REDIS_PORT not found")
        .clone();

    // The container's own environment resolved ${redis.port} at creation time.
    let app_container = format!("{prefix}-app");
    let exec = tokio::process::Command::new("docker")
        .args(["exec", &app_container, "sh", "-c", "echo $OTHER_PORT"])
        .output()
        .await
        .expect("failed to run docker exec");
    assert!(
        exec.status.success(),
        "docker exec into {app_container} failed: {}",
        String::from_utf8_lossy(&exec.stderr)
    );
    let seen_in_container = String::from_utf8_lossy(&exec.stdout).trim().to_string();
    assert_eq!(
        seen_in_container, redis_port,
        "the container's own OTHER_PORT should equal redis's resolved host port"
    );

    // The service's own post-start hook sees the same resolved value.
    let seen_by_hook = std::fs::read_to_string(ws.path().join("hook-other-port"))
        .expect("post-start hook did not write hook-other-port");
    let seen_by_hook = seen_by_hook.trim_end_matches(['\r', '\n']);
    assert_eq!(
        seen_by_hook, redis_port,
        "the post-start hook should see the same resolved sibling port"
    );

    ws.eph_ok(&["down"]).await;
}

/// Two `eph up` runs against the same workspace at once must not race: the
/// per-workspace lock serializes them, so the service set is started exactly
/// once and `state.json` stays valid JSON. A `pre-start` hook that sleeps for a
/// few seconds widens the window the first process holds the lock, so the
/// second is guaranteed to still be waiting on it rather than merely finishing
/// first by luck.
#[tokio::test]
async fn concurrent_up_serializes_on_the_workspace_lock() {
    let ws = TestWorkspace::new(&format!(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-start={}
"#,
        hook_sleep(3)
    ));

    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let run_up = || {
        tokio::process::Command::new(eph_binary)
            .arg("up")
            .current_dir(ws.path())
            .output()
    };
    let (first, second) = tokio::join!(run_up(), run_up());
    let first = first.expect("failed to spawn the first eph up");
    let second = second.expect("failed to spawn the second eph up");

    assert!(
        first.status.success(),
        "the first concurrent up should succeed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "the second concurrent up should succeed (it should wait on the lock, \
         not fail): {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let (prefix, state_dir) = container_prefix_and_state_dir(&ws).await;
    let redis_container = format!("{prefix}-redis");
    assert_eq!(
        common::docker_container_names(&redis_container).await,
        vec![redis_container.clone()],
        "exactly one redis container should exist; the second up should have \
         reused it rather than creating a duplicate"
    );

    let state_contents = std::fs::read_to_string(state_dir.join("state.json"))
        .expect("state.json should exist after both ups");
    let parsed: serde_json::Value = serde_json::from_str(&state_contents)
        .expect("state.json should be valid JSON, not interleaved by the race");
    assert!(
        parsed["services"]["redis"].is_object(),
        "state.json should record redis: {state_contents}"
    );

    ws.eph_ok(&["down"]).await;
}

/// The foreground liveness check belongs inside the same transaction as its
/// spawn. An `up` that wins the workspace lock must make a queued `dev` reject
/// the now-running app instead of spawning a duplicate and orphaning one PID.
#[tokio::test]
async fn concurrent_up_prevents_dev_from_duplicating_the_foreground() {
    let held = "up-holds-lock-before-dev";
    let release = "release-up-before-dev";
    let ws = TestWorkspace::new(&format!(
        "[web]\nrun={}\npre-start={}\n",
        run_sleep(300),
        hook_mark_and_wait(held, release)
    ));
    let eph_binary = env!("CARGO_BIN_EXE_eph");

    let mut up = tokio::process::Command::new(eph_binary)
        .arg("up")
        .current_dir(ws.path())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn eph up");
    assert!(
        wait_for_file(&ws.path().join(held)).await,
        "eph up never reached its locked pre-start hook"
    );

    let dev_stderr_path = ws.path().join("dev-lock-wait.stderr");
    let mut dev = tokio::process::Command::new(eph_binary)
        .arg("dev")
        .current_dir(ws.path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&dev_stderr_path).expect("failed to capture eph dev stderr"),
        ))
        .spawn()
        .expect("failed to spawn eph dev");
    assert!(
        wait_for_file_text(
            &dev_stderr_path,
            "another eph command is running in this workspace; waiting for it",
        )
        .await,
        "eph dev never contended on eph up's workspace lock"
    );
    std::fs::write(ws.path().join(release), "release").unwrap();

    let up_status = up.wait().await.expect("failed to wait for eph up");
    assert!(up_status.success(), "the winning eph up should succeed");
    let dev_status = tokio::time::timeout(Duration::from_secs(20), dev.wait())
        .await
        .expect("eph dev spawned a duplicate foreground process")
        .expect("failed to wait for eph dev");
    let dev_stderr = std::fs::read_to_string(&dev_stderr_path).unwrap_or_default();
    assert!(!dev_status.success(), "eph dev should reject the live app");
    assert!(
        dev_stderr.contains("already running"),
        "eph dev should explain the locked liveness rejection: {dev_stderr}"
    );

    ws.eph_ok(&["down"]).await;
}

/// A destructive force-non-empty prune must share the lifecycle lock with
/// `up`. The marker proves `up` is inside its lock before prune starts; prune
/// must wait, refresh Docker inventory, and then preserve the running service
/// unless `--force-live` was also supplied.
#[tokio::test]
async fn force_non_empty_prune_serializes_with_up_before_inventory() {
    let state_root = tempfile::tempdir().unwrap();
    let state_root_str = state_root.path().to_string_lossy().into_owned();
    let held = "up-holds-workspace-lock";
    let release = "release-up-workspace-lock";
    let ws = TestWorkspace::new(&format!(
        "[redis]\nimage=redis:7-alpine\nport=6379\npre-start={}\n",
        hook_mark_and_wait(held, release)
    ));
    let eph_binary = env!("CARGO_BIN_EXE_eph");

    let mut up = tokio::process::Command::new(eph_binary)
        .arg("up")
        .current_dir(ws.path())
        .env("EPH_STATE_ROOT", &state_root_str)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn eph up");

    let held_observed = wait_for_file(&ws.path().join(held)).await;
    let prune_stdout_path = ws.path().join("prune.stdout");
    let prune_stderr_path = ws.path().join("prune.stderr");
    let mut prune = spawn_force_non_empty_prune(
        &ws.path(),
        &state_root_str,
        &prune_stdout_path,
        &prune_stderr_path,
    );
    let prune_waited = wait_for_file_text(
        &prune_stderr_path,
        "another eph command is running in this workspace; waiting for it",
    )
    .await;
    std::fs::write(ws.path().join(release), "release").unwrap();

    let up_status = up.wait().await.expect("failed to wait for eph up");
    let prune_status = prune.wait().await.expect("failed to wait for system prune");
    let prune_stdout = std::fs::read_to_string(&prune_stdout_path).unwrap_or_default();
    let prune_stderr = std::fs::read_to_string(&prune_stderr_path).unwrap_or_default();

    let cleanup = ws
        .eph_with_envs(
            &["clean", "--skip-hooks"],
            &[("EPH_STATE_ROOT", &state_root_str)],
        )
        .await;
    let cleanup_stderr = String::from_utf8_lossy(&cleanup.stderr).into_owned();

    assert!(
        held_observed,
        "pre-start marker should appear while up holds the workspace lock"
    );
    assert!(
        prune_waited,
        "prune should report waiting for up's workspace lock: {prune_stderr}"
    );
    assert!(up_status.success(), "eph up should succeed");
    assert!(
        prune_status.success(),
        "system prune should succeed: {prune_stderr}"
    );
    assert!(
        prune_stdout.contains("Skipped:") && prune_stdout.contains("running container"),
        "prune should preserve the service started before inventory: {prune_stdout}"
    );
    assert!(
        cleanup.status.success(),
        "cleanup should succeed: {cleanup_stderr}"
    );
}

/// Foreground `eph dev` startup writes process state, so it must hold the same
/// lifecycle lock as prune until the process identity is persisted. The
/// healthcheck waits on an explicit release file to keep that transition open
/// without relying on elapsed time.
#[tokio::test]
async fn force_non_empty_prune_serializes_with_foreground_dev_startup() {
    let state_root = tempfile::tempdir().unwrap();
    let state_root_str = state_root.path().to_string_lossy().into_owned();
    let held = "dev-holds-workspace-lock";
    let release = "release-dev-workspace-lock";
    let exit = "exit-dev-foreground-process";
    let (run_command, wait_script) = foreground_wait_fixture(held, release, exit);
    let ws = TestWorkspace::new(&format!(
        "[web]\nrun={}\nhealthcheck={}\nready-timeout=20\n",
        run_command,
        healthcheck_file_exists(release)
    ));
    #[cfg(unix)]
    ws.write_file("dev-wait.sh", &wait_script);
    #[cfg(windows)]
    ws.write_file("dev-wait.ps1", &wait_script);
    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let dev_stdout_path = ws.path().join("dev.stdout");
    let dev_stderr_path = ws.path().join("dev.stderr");

    let mut dev = tokio::process::Command::new(eph_binary)
        .args(["dev", "--skip-hooks"])
        .current_dir(ws.path())
        .env("EPH_STATE_ROOT", &state_root_str)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            std::fs::File::create(&dev_stdout_path).unwrap(),
        ))
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&dev_stderr_path).unwrap(),
        ))
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn eph dev");

    let held_observed = wait_for_file(&ws.path().join(held)).await;
    let prune_stdout_path = ws.path().join("dev-prune.stdout");
    let prune_stderr_path = ws.path().join("dev-prune.stderr");
    let mut prune = spawn_force_non_empty_prune(
        &ws.path(),
        &state_root_str,
        &prune_stdout_path,
        &prune_stderr_path,
    );
    let prune_waited = wait_for_file_text(
        &prune_stderr_path,
        "another eph command is running in this workspace; waiting for it",
    )
    .await;
    std::fs::write(ws.path().join(release), "release").unwrap();

    let prune_status = prune.wait().await.expect("failed to wait for system prune");
    let prune_stdout = std::fs::read_to_string(&prune_stdout_path).unwrap_or_default();
    let prune_stderr = std::fs::read_to_string(&prune_stderr_path).unwrap_or_default();
    std::fs::write(ws.path().join(exit), "exit").unwrap();
    let dev_exited = matches!(
        tokio::time::timeout(Duration::from_secs(20), dev.wait()).await,
        Ok(Ok(_))
    );
    if !dev_exited {
        dev.kill().await.ok();
        dev.wait().await.ok();
    }

    let cleanup = ws
        .eph_with_envs(
            &["clean", "--skip-hooks"],
            &[("EPH_STATE_ROOT", &state_root_str)],
        )
        .await;
    let cleanup_stderr = String::from_utf8_lossy(&cleanup.stderr).into_owned();
    let dev_stderr = std::fs::read_to_string(&dev_stderr_path).unwrap_or_default();

    assert!(
        held_observed,
        "foreground process should start while dev holds the workspace lock: {dev_stderr}"
    );
    assert!(
        prune_waited,
        "prune should report waiting for dev's workspace lock: {prune_stderr}"
    );
    assert!(
        prune_status.success(),
        "system prune should succeed: {prune_stderr}"
    );
    assert!(
        dev_exited,
        "foreground process should exit after its explicit test release"
    );
    assert!(
        prune_stdout.contains("Skipped:") && prune_stdout.contains("live run= process"),
        "prune should preserve the foreground process recorded before inventory: {prune_stdout}"
    );
    assert!(
        cleanup.status.success(),
        "cleanup should succeed: {cleanup_stderr}"
    );
}

// ============================================================================
// Logs Tests
// ============================================================================

#[tokio::test]
async fn logs_captures_run_service_output() {
    // The command prints a known marker, then sleeps so the process stays alive
    // long enough for `eph logs` to read its captured output.
    let ws = TestWorkspace::new(&format!(
        "\n[worker]\nrun={}\n",
        run_echo_and_wait("hello-from-run-logs")
    ));

    ws.eph_ok(&["up"]).await;

    // The captured stdout should be visible via `eph logs`.
    let logs = ws.eph_ok(&["logs", "worker"]).await;
    assert!(
        logs.contains("hello-from-run-logs"),
        "expected captured run= output in logs, got:\n{}",
        logs
    );

    // --tail 1 should still include the single emitted line.
    let tailed = ws.eph_ok(&["logs", "-n", "1", "worker"]).await;
    assert!(
        tailed.contains("hello-from-run-logs"),
        "expected tailed run= output, got:\n{}",
        tailed
    );

    ws.eph_ok(&["down"]).await;
}

// Even after a `run=` service dies, its captured log should remain readable --
// the core motivation for capturing it (a service that dies on startup must
// leave a trace).
#[tokio::test]
async fn logs_persist_after_run_service_exits() {
    let ws = TestWorkspace::new(&format!(
        "\n[doomed]\nrun={}\n",
        run_echo_then_fail("about-to-die")
    ));

    // Startup must fail, while preserving the output that explains why.
    let up = ws.eph(&["up"]).await;
    assert!(
        !up.status.success(),
        "an immediately exiting run= service must fail startup"
    );
    assert!(
        String::from_utf8_lossy(&up.stderr).contains("exited during startup"),
        "startup should classify the early exit: {}",
        String::from_utf8_lossy(&up.stderr)
    );

    let logs = ws.eph_ok(&["logs", "doomed"]).await;
    assert!(
        logs.contains("about-to-die"),
        "expected the dead service's trace to survive, got:\n{}",
        logs
    );

    ws.eph_ok(&["down"]).await;
}

#[tokio::test]
async fn fixed_port_run_service_that_exits_during_startup_fails_up() {
    // `exit N` is spelled the same in both `sh` and `cmd`.
    let ws = TestWorkspace::new("[doomed]\nrun=exit 7\nport=43127\n");

    let up = ws.eph(&["up"]).await;

    assert!(!up.status.success(), "fixed-port early exit must fail up");
    assert!(
        String::from_utf8_lossy(&up.stderr).contains("exited during startup"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&up.stderr)
    );
}

/// Regression test for the Windows handle-inheritance hang: `eph up`'s output
/// is captured through pipes here (the harness uses `.output()`), and before
/// the fix in `proc::spawn_captured` the long-lived service tree inherited
/// eph's stdout/stderr pipe handles, so this capture did not return until the
/// service died (~5 minutes) even though eph exited immediately. The explicit
/// timeout turns a regression into a fast failure instead of a stuck suite.
#[tokio::test]
async fn up_output_capture_unblocks_while_run_service_lives() {
    let ws = TestWorkspace::new(&format!("[web]\nrun={}\n", run_sleep(300)));

    tokio::time::timeout(Duration::from_secs(60), ws.eph_ok(&["up"]))
        .await
        .expect("`eph up` output capture must not wait for the service to die");

    // Unblocking the capture must not have killed the service: it still has
    // to be running after `up` returned.
    let status = ws.eph_ok(&["status"]).await;
    assert!(
        status.contains("Running services") && !status.contains("web (stopped)"),
        "service should be running after up: {status}"
    );

    ws.eph_ok(&["down"]).await;
}

// `port=auto` on a run= service: eph allocates a free host port, injects it into
// the process environment, and resolves it for interpolation -- the core of
// first-party app port creation.
#[tokio::test]
async fn run_service_auto_port_is_allocated_and_injected() {
    // The process echoes the PORT it was handed, then stays alive so its log can
    // be read. `env.PORT=${web.port}` is how the assigned port reaches it.
    let ws = TestWorkspace::new(&format!(
        "\n[web]\nrun={}\nport=auto\nenv.PORT=${{web.port}}\n\n[env]\nAPP_URL=http://localhost:${{web.port}}\n",
        run_report_port()
    ));

    ws.eph_ok(&["up"]).await;

    // The allocated port is resolved into the top-level variable...
    let env = ws.env_json().await;
    let app_url = env.get("APP_URL").expect("APP_URL should be set");
    let port = extract_port(app_url).expect("APP_URL should contain a real port");
    assert!(port > 1024, "expected an ephemeral host port, got {port}");

    // ...and the same port was injected into the process as PORT.
    let logs = ws.eph_ok(&["logs", "web"]).await;
    assert!(
        logs.contains(&format!("BOUND_PORT={port}")),
        "expected the process to receive PORT={port}, got:\n{logs}"
    );

    ws.eph_ok(&["down"]).await;
}

// An auto-port readiness check must run with the same environment the app gets,
// and have its ${...} resolved, or `curl -sf http://localhost:$PORT` style
// checks would never see the allocated port and `eph up` would time out.
#[tokio::test]
async fn run_service_auto_port_healthcheck_sees_port() {
    // The check passes only if BOTH the eph-resolved `${web.port}` and the
    // injected `$PORT` env equal the allocated port. If eph ran the healthcheck
    // without the app's env, `$PORT` would be empty; if it didn't resolve the
    // string, `${web.port}` would be a literal -- either way `eph up` would fail.
    let ws = TestWorkspace::new(&format!(
        "\n[web]\nrun={}\nport=auto\nenv.PORT=${{web.port}}\nhealthcheck={}\nready-timeout=10\n",
        run_sleep(300),
        healthcheck_port_matches()
    ));

    // `eph_ok` panics if `up` times out, so reaching `down` is the assertion.
    ws.eph_ok(&["up"]).await;
    ws.eph_ok(&["down"]).await;
}

// An eph-allocated auto port stays the same across `eph down` / `eph up`, so a
// managed app's URL is stable for bookmarks and OAuth callbacks.
#[tokio::test]
async fn run_service_auto_port_is_stable_across_restart() {
    let ws = TestWorkspace::new(&format!(
        "\n[web]\nrun={}\nport=auto\n\n[env]\nAPP_URL=http://localhost:${{web.port}}\n",
        run_sleep(300)
    ));

    ws.eph_ok(&["up"]).await;
    let first = extract_port(ws.env_json().await.get("APP_URL").unwrap())
        .expect("APP_URL should contain a port");

    ws.eph_ok(&["down"]).await;
    ws.eph_ok(&["up"]).await;
    let second = extract_port(ws.env_json().await.get("APP_URL").unwrap())
        .expect("APP_URL should contain a port");

    assert_eq!(
        first, second,
        "auto port should be reused across down/up for a stable URL"
    );

    ws.eph_ok(&["down"]).await;
}

// A compound `run=` command makes the shell fork children; `eph down` must kill
// that whole tree, not just the wrapper, or the children are orphaned and survive
// teardown. The command spawns a long-lived *grandchild* (a `sleep` two levels
// below the wrapper shell) that records its own PID into the workspace, then the
// test asserts that PID is gone after `eph down`.
//
// `#[cfg(unix)]`: the command string is POSIX `sh`, liveness is probed with
// `kill -0`, and this exercises the Unix process-group teardown. The Docker-backed
// CI job that runs this suite is Linux, where the assertion is meaningful.
#[cfg(unix)]
#[tokio::test]
async fn run_service_compound_command_kills_child_tree_on_down() {
    // Both shell levels are compound (`... & wait`) so neither `sh` exec-optimizes
    // itself away: the outer shell stays as the wrapper eph records (and groups),
    // and the inner shell forks the real `sleep` grandchild rather than becoming
    // it.
    let ws = TestWorkspace::new(
        "[web]\nrun=sh -c 'sleep 300 & echo $! > grandchild.pid; wait' & wait\n",
    );

    ws.eph_ok(&["up"]).await;

    // The grandchild writes its PID into the workspace; wait for the file to land
    // and parse a PID out of it.
    let pid_path = ws.path().join("grandchild.pid");
    let pid: u32 = common::retry_until(Duration::from_secs(10), || async {
        let raw = tokio::fs::read_to_string(&pid_path)
            .await
            .map_err(|e| e.to_string())?;
        raw.trim().parse::<u32>().map_err(|e| e.to_string())
    })
    .await
    .expect("the grandchild should have recorded its PID");

    assert!(
        process_alive(pid),
        "the grandchild (PID {pid}) should be alive while the service is up"
    );

    ws.eph_ok(&["down"]).await;

    // After teardown the grandchild must be gone. Poll: SIGKILL reaping is
    // asynchronous, so a just-killed process can linger for a moment.
    let gone = common::retry_until(Duration::from_secs(10), || async {
        if process_alive(pid) { Err(()) } else { Ok(()) }
    })
    .await
    .is_ok();
    assert!(
        gone,
        "the orphaned grandchild (PID {pid}) survived `eph down`"
    );
}

/// Whether `pid` is a live process, via a POSIX `kill -0` probe (no signal sent,
/// just an existence/permission check). Used to assert a `run=` service's child
/// tree is torn down. Returns `false` once the process is gone.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// `eph logs` for an image-backed service proxies `docker logs`.
#[tokio::test]
async fn logs_proxies_docker_for_image_service() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    ws.eph_ok(&["up"]).await;

    let logs = ws.eph_ok(&["logs", "redis"]).await;
    // redis announces its readiness on startup; any non-empty proxied output
    // confirms the docker-logs path works without coupling to an exact string.
    assert!(
        !logs.trim().is_empty(),
        "expected docker logs output for redis, got empty"
    );

    ws.eph_ok(&["down"]).await;
}

// `eph logs` with no SERVICE prefixes every line with a `[name]` tag.
#[tokio::test]
async fn logs_all_services_tags_each_line() {
    let ws = TestWorkspace::new(&format!(
        "\n[alpha]\nrun={}\n\n[beta]\nrun={}\n",
        run_echo_and_wait("alpha-marker"),
        run_echo_and_wait("beta-marker")
    ));

    ws.eph_ok(&["up"]).await;

    // Output is captured (not a TTY), so tags are uncolored. Each service's
    // output line is prefixed in place by its `[name]` tag. `[alpha]` is the
    // widest tag, so its lines carry no left padding; `[beta]` is right-aligned
    // under it, so the substring "[beta] beta-marker" still appears verbatim.
    let logs = ws.eph_ok(&["logs"]).await;
    assert!(
        logs.contains("[alpha] alpha-marker"),
        "alpha line not tagged in place:\n{logs}"
    );
    assert!(
        logs.contains("[beta] beta-marker"),
        "beta line not tagged in place:\n{logs}"
    );

    ws.eph_ok(&["down"]).await;
}

// `eph logs` with no SERVICE must exit non-zero when a Docker-backed source
// fails (here: an image service whose container does not exist because it was
// never started), matching the single-service path. Regression test for the
// all-services path silently swallowing per-task `docker logs` failures.
#[tokio::test]
async fn logs_all_services_fails_when_a_docker_source_fails() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    // Note: no `eph up`, so no container exists -- `docker logs <container>`
    // exits non-zero, and that failure must surface as a non-zero exit.
    let single = ws.eph(&["logs", "redis"]).await;
    assert!(
        !single.status.success(),
        "single-service logs should fail for a missing container"
    );

    let all = ws.eph(&["logs"]).await;
    assert!(
        !all.status.success(),
        "all-services logs should fail when a docker source fails:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&all.stdout),
        String::from_utf8_lossy(&all.stderr),
    );
}

// `eph logs -f` with no SERVICE follows every service at once, interleaving
// their tagged lines as they arrive.
#[tokio::test]
async fn logs_follow_all_services_interleaves() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Two services that each emit a tagged line on a steady cadence (see
    // `run_counter_lines`), so a follow-all stream sees output from both well
    // within the timeout below.
    let ws = TestWorkspace::new(&format!(
        "\n[alpha]\nrun={}\n\n[beta]\nrun={}\n",
        run_counter_lines("alpha"),
        run_counter_lines("beta")
    ));

    ws.eph_ok(&["up"]).await;

    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let mut child = tokio::process::Command::new(eph_binary)
        .args(["logs", "-f"])
        .current_dir(ws.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn `eph logs -f`");

    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout).lines();
    let mut saw_alpha = false;
    let mut saw_beta = false;

    // Read until both services' tagged lines have appeared, or time out.
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        while let Ok(Some(line)) = reader.next_line().await {
            // Output is piped (not a TTY), so tags are uncolored plain text.
            if line.contains("[alpha] alpha-") {
                saw_alpha = true;
            }
            if line.contains("[beta] beta-") {
                saw_beta = true;
            }
            if saw_alpha && saw_beta {
                return;
            }
        }
    })
    .await;

    let _ = child.kill().await;
    ws.eph_ok(&["down"]).await;

    assert!(
        result.is_ok(),
        "timed out before seeing both services (alpha={saw_alpha}, beta={saw_beta})"
    );
}

// `eph logs -n N` returns exactly the last N lines of a `run=` service's log.
#[tokio::test]
async fn logs_tail_returns_last_n_lines() {
    let ws = TestWorkspace::new(&format!("\n[svc]\nrun={}\n", run_five_tail_lines()));

    ws.eph_ok(&["up"]).await;

    let tail = ws.eph_ok(&["logs", "-n", "2", "svc"]).await;
    let lines: Vec<&str> = tail.lines().collect();
    assert_eq!(
        lines,
        vec!["tailline-4", "tailline-5"],
        "expected only the last 2 lines, got:\n{tail}"
    );

    ws.eph_ok(&["down"]).await;
}

// Captured `run=` logs can contain secrets, so the log file and its directory
// are created owner-only (0600/0700).
#[cfg(unix)]
#[tokio::test]
async fn logs_run_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let ws = TestWorkspace::new("[svc]\nrun=echo hi && sleep 300\n");
    ws.eph_ok(&["up"]).await;

    let info = ws.eph_ok(&["info"]).await;
    let state_dir = info
        .lines()
        .find_map(|l| l.strip_prefix("State directory: "))
        .expect("`eph info` should report the state directory")
        .trim();
    let logs_dir = std::path::Path::new(state_dir).join("logs");
    let log_file = logs_dir.join("svc.log");

    let file_mode = std::fs::metadata(&log_file).unwrap().permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(&logs_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "log file should be owner read/write only");
    assert_eq!(dir_mode, 0o700, "logs dir should be owner-only");

    ws.eph_ok(&["down"]).await;
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
async fn error_unknown_service() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
"#,
    );

    let output = ws.eph(&["up", "nonexistent"]).await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown service") || stderr.contains("nonexistent"),
        "Expected error about unknown service: {}",
        stderr
    );
}

#[tokio::test]
async fn error_invalid_eph_syntax() {
    let ws = TestWorkspace::new(
        r#"
this is not valid syntax
"#,
    );

    let output = ws.eph(&["check"]).await;
    assert!(!output.status.success());
}

// ============================================================================
// Skills Tests
// ============================================================================

/// `skills install` writes the bundled skill into both default roots; `skills
/// check` then passes, fails closed after a hand edit, and passes again once
/// `install --force` restores the file. End to end through the real binary. No
/// Docker is involved: skills are pure filesystem work.
#[tokio::test]
async fn skills_install_and_check_round_trip() {
    // The workspace's temp dir is not a git repo, so eph falls back to installing
    // relative to it; a `.eph` is not required for skills but the harness writes
    // one anyway, which is harmless here.
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");

    // Install lands a SKILL.md under each default root.
    let install = ws.eph(&["skills", "install"]).await;
    assert!(
        install.status.success(),
        "install failed; stderr:\n{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let claude = ws.path().join(".claude/skills/using-eph/SKILL.md");
    let agents = ws.path().join(".agents/skills/using-eph/SKILL.md");
    assert!(claude.exists(), "expected {}", claude.display());
    assert!(agents.exists(), "expected {}", agents.display());

    // The written file is a real Claude Code skill: front matter first, named,
    // and carrying the generated-by provenance stamp.
    let body = std::fs::read_to_string(&claude).unwrap();
    assert!(body.starts_with("---\n"), "body:\n{body}");
    assert!(body.contains("name: using-eph"), "body:\n{body}");
    assert!(
        body.contains("Generated by `eph skills install`"),
        "the provenance stamp should be present; body:\n{body}"
    );

    // Right after install, check is green.
    assert!(ws.eph(&["skills", "check"]).await.status.success());

    // A hand edit makes check fail closed (non-zero exit) and report drift.
    std::fs::write(&claude, "tampered\n").unwrap();
    let drifted = ws.eph(&["skills", "check"]).await;
    assert_ne!(drifted.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&drifted.stdout).contains("drifted"),
        "stdout:\n{}",
        String::from_utf8_lossy(&drifted.stdout)
    );

    // Without --force, install refuses to clobber the edited file.
    let no_force = ws.eph(&["skills", "install"]).await;
    assert!(no_force.status.success());
    assert!(
        String::from_utf8_lossy(&no_force.stdout).contains("skipped"),
        "stdout:\n{}",
        String::from_utf8_lossy(&no_force.stdout)
    );
    assert_eq!(std::fs::read_to_string(&claude).unwrap(), "tampered\n");

    // --force restores it, and check is green again.
    assert!(
        ws.eph(&["skills", "install", "--force"])
            .await
            .status
            .success()
    );
    assert!(ws.eph(&["skills", "check"]).await.status.success());

    // `skills list` names the bundled skill.
    let listed = ws.eph(&["skills", "list"]).await;
    assert!(listed.status.success());
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("using-eph"),
        "stdout:\n{}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// `eph dev` brings the stack up, foregrounds a `run=` service, opens the host
/// port a preview server injects as `$PORT` as a readiness gate to it, and on a
/// stop signal tears the stack down and exits zero.
///
/// Unix-only: it delivers `SIGTERM`, the signal a Claude Desktop preview server
/// uses to stop the dev command. Windows has no equivalent a test harness can
/// deliver, and that gap (a hard kill skips teardown) is documented behavior.
#[cfg(unix)]
#[tokio::test]
async fn dev_signal_during_foreground_readiness_kills_the_process_tree() {
    use std::process::Stdio;
    use tokio::process::Command;

    let ws = TestWorkspace::new(
        "[web]\nrun=printf '%s' $$ > foreground-pid; sleep 1; exec sh -c 'echo exec > foreground-exec; sleep 600'\nhealthcheck=exit 1\nready-timeout=60\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_eph"))
        .arg("dev")
        .current_dir(ws.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn eph dev");

    let pid_path = ws.path().join("foreground-pid");
    assert!(
        wait_for_file(&pid_path).await,
        "the foreground process never entered readiness"
    );
    assert!(
        wait_for_file(&ws.path().join("foreground-exec")).await,
        "the foreground process never exec'd during readiness"
    );
    let foreground_pid: libc::pid_t = std::fs::read_to_string(&pid_path)
        .expect("failed to read the foreground PID")
        .trim()
        .parse()
        .expect("foreground PID was not numeric");

    let dev_pid = child.id().expect("dev child has a pid") as libc::pid_t;
    // SAFETY: kill takes plain integers and has no memory-safety preconditions.
    unsafe {
        libc::kill(dev_pid, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("eph dev should stop promptly during readiness")
        .expect("failed to wait for eph dev");
    assert!(status.success(), "a signalled eph dev should exit cleanly");

    let process_gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // SAFETY: signal 0 only probes whether the PID still exists.
            if unsafe { libc::kill(foreground_pid, 0) } == -1 {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        process_gone,
        "foreground PID {foreground_pid} survived cancellation during readiness"
    );
    let after = ws.eph_ok(&["status"]).await;
    assert!(
        after.contains("No services running"),
        "cancelled foreground startup should leave no live service: {after}"
    );
}

/// The cancellation guard captures the shell wrapper immediately, while the
/// durable backend identity is captured after readiness so a later `exec` of
/// the real app remains visible to status and teardown.
#[cfg(unix)]
#[tokio::test]
async fn dev_tracks_foreground_identity_after_shell_exec() {
    use std::process::Stdio;
    use tokio::process::Command;

    let ws = TestWorkspace::new(
        "[web]\nrun=sleep 1; exec sleep 600\nhealthcheck=sleep 2; exit 0\nready-timeout=10\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_eph"))
        .arg("dev")
        .current_dir(ws.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn eph dev");

    let tracked = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status = ws.eph_ok(&["status"]).await;
            if status.contains("web") && !status.contains("web (stopped)") {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        tracked,
        "status lost the foreground process after shell exec"
    );

    let dev_pid = child.id().expect("dev child has a pid") as libc::pid_t;
    // SAFETY: kill takes plain integers and has no memory-safety preconditions.
    unsafe {
        libc::kill(dev_pid, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("eph dev should stop promptly")
        .expect("failed to wait for eph dev");
    assert!(status.success(), "a signalled eph dev should exit cleanly");
    let after = ws.eph_ok(&["status"]).await;
    assert!(
        after.contains("No services running"),
        "teardown should stop the exec'd foreground process: {after}"
    );
}

/// An auto-port foreground app that loses its first port race is relaunched and
/// remains attached to `eph dev` until shutdown.
#[cfg(unix)]
#[tokio::test]
async fn dev_retries_an_auto_port_app_that_exits_during_startup() {
    use std::process::Stdio;
    use tokio::process::Command;

    let ws = TestWorkspace::new(
        "[web]\nrun=if [ -f attempted ]; then sleep 600; else touch attempted; exit 1; fi\nport=auto\n",
    );
    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let mut child = Command::new(eph_binary)
        .arg("dev")
        .current_dir(ws.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn eph dev");

    // Match the running-services line, not just the name: `eph status` also
    // lists defined-but-stopped services, so a bare "web" matches while the
    // app is still down and the relaunch assertion passes spuriously.
    let mut running = false;
    for _ in 0..50 {
        sleep(Duration::from_millis(200)).await;
        if child.try_wait().expect("poll eph dev").is_some() {
            break;
        }
        let status = ws.eph(&["status"]).await;
        if String::from_utf8_lossy(&status.stdout).contains("web -> localhost:") {
            running = true;
            break;
        }
    }
    assert!(
        running,
        "eph dev should relaunch the app after its first auto-port startup exit"
    );
    assert!(ws.path().join("attempted").exists());

    let pid = child.id().expect("dev child has a pid") as libc::pid_t;
    // SAFETY: kill takes plain integers and has no memory-safety preconditions.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("eph dev should stop promptly")
        .expect("wait for eph dev");
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn dev_gates_injected_port_and_tears_down_on_signal() {
    use std::process::Stdio;
    use tokio::process::Command;

    // A no-Docker run= service that just stays alive. port=auto means the app gets
    // its own internal host port; `eph dev` opens the injected $PORT as a gate that
    // forwards to it.
    let ws = TestWorkspace::new("[web]\nrun=sleep 600\nport=auto\n");

    // The host port a preview server would have chosen and passed as $PORT.
    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };

    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let mut child = Command::new(eph_binary)
        .arg("dev")
        .current_dir(ws.path())
        .env("PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn eph dev");

    // The gate is opened only after setup and post-start finish, so once $PORT
    // accepts a connection the whole readiness sequence has run. A random,
    // ungated port would never accept here, which is what proves the $PORT gate
    // took effect (rather than the app binding some other port and $PORT staying
    // closed).
    let mut gated = false;
    for _ in 0..100 {
        let connected = tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
        if connected {
            gated = true;
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(
        gated,
        "`eph dev` should open the injected preview port {port} once ready"
    );

    // The app itself is up on its own internal port, reported by status.
    let status = String::from_utf8_lossy(&ws.eph(&["status"]).await.stdout).into_owned();
    assert!(
        status.contains("web"),
        "`eph dev` should report the foreground app 'web' running; got:\n{status}"
    );

    // Stop it the way a preview server does: a termination signal.
    let pid = child.id().expect("dev child has a pid") as libc::pid_t;
    // SAFETY: kill takes plain integers and has no memory-safety preconditions;
    // SIGTERM to a live child is well-defined.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("eph dev should exit promptly after SIGTERM")
        .expect("waiting on eph dev failed");
    assert!(
        status.success(),
        "a signalled `eph dev` should tear down and exit zero, got {status:?}"
    );

    // The foregrounded service must be gone after the graceful teardown.
    let after = ws.eph_ok(&["status"]).await;
    assert!(
        after.contains("No services running"),
        "services should be torn down after `eph dev` is signalled; got:\n{after}"
    );
}

/// `eph dev` reserves the foreground app's port before running its `pre-start`
/// hook, matching `eph up`: a top-level variable derived from the app's own
/// port resolves in the hook's environment and matches the port the app is
/// then started on.
#[cfg(unix)]
#[tokio::test]
async fn dev_pre_start_hook_sees_foreground_port() {
    use std::process::Stdio;
    use tokio::process::Command;

    let ws = TestWorkspace::new(concat!(
        "APP_URL=http://localhost:${web.port}\n",
        "[web]\n",
        "run=sleep 600\n",
        "port=auto\n",
        "pre-start=printf '%s' \"$APP_URL\" > pre-start-url\n",
    ));

    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let mut child = Command::new(eph_binary)
        .arg("dev")
        .current_dir(ws.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn eph dev");

    // Wait until the app is up so `eph env` resolves APP_URL to the live port.
    // Match the running-services line, not just the name: `eph status` also
    // lists defined-but-stopped services, so a bare "web" matches before the
    // app has started.
    let mut running = false;
    for _ in 0..100 {
        sleep(Duration::from_millis(200)).await;
        if child.try_wait().expect("poll eph dev").is_some() {
            break;
        }
        let status = ws.eph(&["status"]).await;
        if String::from_utf8_lossy(&status.stdout).contains("web -> localhost:") {
            running = true;
            break;
        }
    }
    assert!(running, "eph dev should bring the app up");

    let app_url = ws
        .env_json()
        .await
        .get("APP_URL")
        .expect("APP_URL not found")
        .clone();
    let captured = std::fs::read_to_string(ws.path().join("pre-start-url"))
        .expect("pre-start hook did not write pre-start-url");
    assert_eq!(
        captured.trim_end_matches(['\r', '\n']),
        app_url,
        "the dev pre-start hook saw a different port than the app was started on"
    );

    let pid = child.id().expect("dev child has a pid") as libc::pid_t;
    // SAFETY: kill takes plain integers and has no memory-safety preconditions.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("eph dev should stop promptly")
        .expect("wait for eph dev");
    assert!(status.success());
}

/// `eph dev` forwards eph's stdin, stdout, and stderr to the foreground app, so
/// it is fully interactive rather than reading from a captured log file.
///
/// Unix-only: it drives the child's stdio over pipes. `cat` copies stdin to
/// stdout and exits at EOF, so a line written to `eph dev`'s stdin must come back
/// on `eph dev`'s stdout, which proves both streams are inherited by the app.
#[cfg(unix)]
#[tokio::test]
async fn dev_forwards_stdio_to_the_foreground_app() {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::Command;

    let ws = TestWorkspace::new("[web]\nrun=cat\nport=auto\n");

    let eph_binary = env!("CARGO_BIN_EXE_eph");
    let mut child = Command::new(eph_binary)
        .arg("dev")
        .current_dir(ws.path())
        // eph's own chrome goes to stderr, so stdout carries only the app's bytes.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn eph dev");

    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin
        .write_all(b"marker-line\n")
        .await
        .expect("write stdin");
    drop(stdin); // EOF, so `cat` flushes and exits, which ends `eph dev`.

    // `eph dev` holds stdout open until it exits (just after `cat` does), so this
    // reads to EOF once the whole thing has wound down.
    let mut out = String::new();
    let mut stdout = child.stdout.take().expect("piped stdout");
    tokio::time::timeout(Duration::from_secs(20), stdout.read_to_string(&mut out))
        .await
        .expect("reading eph dev stdout should not hang")
        .expect("read stdout");
    let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;

    assert!(
        out.contains("marker-line"),
        "a line written to `eph dev` stdin should reach the app and stream back on \
         stdout; got: {out:?}"
    );
}

// ============================================================================
// eph env: unresolved references, powershell format, and json ordering
// ============================================================================

/// `eph env` clears a variable whose value is unresolved, makes the rendered
/// shell program fail, and exits nonzero itself. Once the referenced service
/// starts, the same variable resolves normally.
#[tokio::test]
async fn env_unsets_unresolved_reference_and_fails_closed() {
    let ws = TestWorkspace::new(
        r#"
[db]
image=redis:7-alpine
port=6379

[env]
DATABASE_URL=postgres://user:pass@localhost:${db.port}/app
"#,
    );

    // `db` is never started, so DATABASE_URL cannot resolve.
    let out = ws.eph(&["env"]).await;
    assert!(!out.status.success(), "unresolved env must exit nonzero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("unset DATABASE_URL") && stdout.ends_with("false\n"),
        "stdout must clear stale state and make eval fail, got:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("DATABASE_URL") && stderr.contains("${db.port}"),
        "stderr should name the variable and unresolved reference, got:\n{stderr}"
    );

    // Once `db` is up, the same variable resolves and appears.
    ws.eph_ok(&["up", "db"]).await;
    sleep(Duration::from_secs(1)).await;
    let resolved = ws.eph_ok(&["env"]).await;
    assert!(
        resolved.contains("DATABASE_URL") && !resolved.contains("${"),
        "DATABASE_URL should resolve once db is running, got:\n{resolved}"
    );

    ws.eph_ok(&["down"]).await;
}

#[tokio::test]
async fn env_unresolved_output_is_safe_in_every_format() {
    let ws = TestWorkspace::new(
        "[db]\nimage=postgres:16\nport=5432\n[env]\nDATABASE_URL=postgres://localhost:${db.port}/app\n",
    );

    for (format, expected) in [
        ("export", "unset DATABASE_URL\nfalse\n"),
        ("fish", "set -e DATABASE_URL\nfalse\n"),
        (
            "powershell",
            "Remove-Item Env:DATABASE_URL -ErrorAction SilentlyContinue\nthrow 'eph env: unresolved variables'\n",
        ),
    ] {
        let output = ws.eph(&["env", "--format", format]).await;
        assert!(!output.status.success(), "{format} must exit nonzero");
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }

    let json = ws.eph(&["env", "--format", "json"]).await;
    assert!(!json.status.success(), "json must exit nonzero");
    assert_eq!(String::from_utf8_lossy(&json.stdout), "{}\n");
}

#[tokio::test]
async fn relative_eph_state_root_is_rejected() {
    let ws = TestWorkspace::new("[db]\nimage=postgres:16\n");

    let output = ws
        .eph_with_envs(&["check"], &[("EPH_STATE_ROOT", "relative/state")])
        .await;

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be an absolute path"));
}

/// `eph env --format powershell` emits `$env:NAME = 'value'` lines, doubling an
/// embedded single quote per PowerShell's own single-quoted-string rule.
#[tokio::test]
async fn env_format_powershell() {
    let ws = TestWorkspace::new("APP_NAME=test's app\nDEBUG=true\n");

    let output = ws.eph_ok(&["env", "--format", "powershell"]).await;
    assert!(
        output.contains("$env:APP_NAME = 'test''s app'"),
        "got: {output}"
    );
    assert!(output.contains("$env:DEBUG = 'true'"), "got: {output}");
}

/// `eph env --format json` keys appear in the `.eph` file's declaration order,
/// not an arbitrary hash-map order.
#[tokio::test]
async fn env_format_json_preserves_declaration_order() {
    let ws = TestWorkspace::new("ZEBRA=z\nAPPLE=a\nMANGO=m\n");

    let output = ws.eph_ok(&["env", "--format", "json"]).await;
    let zebra = output.find("ZEBRA").expect("ZEBRA missing");
    let apple = output.find("APPLE").expect("APPLE missing");
    let mango = output.find("MANGO").expect("MANGO missing");
    assert!(
        zebra < apple && apple < mango,
        "json keys should preserve declaration order, got:\n{output}"
    );
}

// ============================================================================
// eph run: flags belong to the command, not to eph
// ============================================================================

/// Every token after `run` belongs to the command, even one that looks like
/// one of eph's own flags (`-v`, `-h`, `-V`) or an arbitrary long flag: none of
/// them are stolen by eph's global flag parsing.
#[tokio::test]
async fn run_passes_through_flag_shaped_arguments() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let out = ws.eph_ok(&echo_flags_command()).await;
    assert!(
        out.contains("-v") && out.contains("-h") && out.contains("-V") && out.contains("--weird"),
        "expected every flag-shaped token to reach the command untouched, got: {out}"
    );
}

/// `eph run -h` is not eph's help: there is no program named `-h`, so it fails
/// with eph's own "failed to run command" message naming it, and eph's usage
/// text never appears.
#[tokio::test]
async fn run_bare_dash_h_is_the_command_not_help() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let out = ws.eph(&["run", "-h"]).await;
    assert!(!out.status.success(), "there is no program named -h");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to run command: -h"),
        "expected eph's own exec-failure message, got: {stderr}"
    );
    assert!(
        !stderr.contains("Usage:"),
        "eph's own help text must not appear for `eph run -h`, got: {stderr}"
    );
}

/// `eph run -v` is likewise the command, not eph's verbose flag: there is no
/// program named `-v`, so it fails the same way `-h` does.
#[tokio::test]
async fn run_bare_dash_v_is_the_command_not_verbose() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let out = ws.eph(&["run", "-v"]).await;
    assert!(!out.status.success(), "there is no program named -v");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to run command: -v"),
        "expected eph's own exec-failure message, got: {stderr}"
    );
}

/// A flag placed *before* `run` is still eph's own global flag: `eph -v run
/// ...` keeps enabling verbose logging and must not leak `-v` into the
/// command's own arguments, the opposite of a flag placed after `run`.
#[cfg(unix)]
#[tokio::test]
async fn verbose_before_run_is_ephs_flag_not_the_commands() {
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let out = ws
        .eph(&["-v", "run", "sh", "-c", "echo \"$1\"", "_", "marker"])
        .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "marker",
        "the command's own args should be unaffected by a -v placed before run"
    );
}

// ============================================================================
// eph up: stale-workspace nudge
// ============================================================================

/// `eph up` nudges toward `eph system prune` when other workspaces' state
/// points at deleted checkouts. `EPH_STATE_ROOT` points the whole invocation at
/// a throwaway directory so the test never touches (or is confused by) the
/// real per-user state directory.
#[tokio::test]
async fn up_nudges_about_stale_workspaces() {
    let state_root = tempfile::tempdir().unwrap();
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let root_str = state_root.path().to_string_lossy().into_owned();

    // A fabricated stale workspace: a 16-hex-digit state dir with metadata
    // (the exact shape `Workspace::save_metadata` writes) pointing at a path
    // that does not exist.
    let stale_dir = state_root.path().join("deadbeefdeadbeef");
    std::fs::create_dir_all(&stale_dir).unwrap();
    let metadata = serde_json::json!({
        "schema": 1,
        "workspace_id": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "short_id": "deadbeefdeadbeef",
        "workspace_path": "/this/path/does/not/exist-eph-integration-test",
        "container_prefix": "eph-deadbeefdeadbeef",
        "last_seen_unix_secs": 0
    });
    std::fs::write(
        stale_dir.join("workspace.json"),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();

    let out = ws
        .eph_with_envs(&["up"], &[("EPH_STATE_ROOT", &root_str)])
        .await;
    assert!(
        out.status.success(),
        "up should still succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("1 stale eph workspace") && stderr.contains("eph system prune"),
        "expected a stale-workspace note, got: {stderr}"
    );

    ws.eph_with_envs(&["down"], &[("EPH_STATE_ROOT", &root_str)])
        .await;
}

/// With no stale workspace state recorded, `eph up` prints no nudge at all.
#[tokio::test]
async fn up_prints_no_nudge_when_nothing_is_stale() {
    let state_root = tempfile::tempdir().unwrap();
    let ws = TestWorkspace::new("[redis]\nimage=redis:7-alpine\nport=6379\n");
    let root_str = state_root.path().to_string_lossy().into_owned();

    let out = ws
        .eph_with_envs(&["up"], &[("EPH_STATE_ROOT", &root_str)])
        .await;
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("stale eph workspace"),
        "should not nudge with no stale workspaces: {stderr}"
    );

    ws.eph_with_envs(&["down"], &[("EPH_STATE_ROOT", &root_str)])
        .await;
}
