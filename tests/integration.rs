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
fn exit_7_command() -> Vec<&'static str> {
    vec!["run", "cmd", "/C", "exit /B 7"]
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

[never_started]
image=redis:7-alpine
port=6379
pre-stop=exit 1
"#,
    );

    // Bring up only redis; `never_started` stays down with its failing hook.
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

    let before = std::fs::read_to_string(&state_path).expect("state.json should exist after up");
    assert!(before.contains("redis") && before.contains("cache"));

    // Stop just one service.
    ws.eph_ok(&["down", "redis"]).await;

    let after = std::fs::read_to_string(&state_path).expect("state.json should still exist");
    assert!(
        !after.contains("redis"),
        "redis should be gone from state.json after a targeted down: {after}"
    );
    assert!(
        after.contains("cache"),
        "cache should remain in state.json: {after}"
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

[never_started]
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

DATABASE_URL=postgres://test:test@localhost:${postgres.port}/test
"#,
    );

    // Start - should wait for pg_isready
    let start = std::time::Instant::now();
    ws.eph_ok(&["up", "postgres"]).await;
    let elapsed = start.elapsed();

    // Should have taken at least a moment for postgres to be ready
    // but not too long (timeout is 30s)
    assert!(
        elapsed.as_secs() < 30,
        "Took too long to start: {:?}",
        elapsed
    );

    // Service should be running and healthy
    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("postgres"));

    // Cleanup
    ws.eph_ok(&["down"]).await;
}

// ============================================================================
// Logs Tests
// ============================================================================

// eph's run= support is cross-platform now, but this fixture's command uses
// POSIX shell syntax (`echo ... && sleep 300`), so gate the capture test to Unix
// to match the command string rather than because the feature is unavailable.
#[cfg(unix)]
#[tokio::test]
async fn logs_captures_run_service_output() {
    // The command prints a known marker, then sleeps so the process stays alive
    // long enough for `eph logs` to read its captured output.
    let ws = TestWorkspace::new(
        r#"
[worker]
run=echo hello-from-run-logs && sleep 300
"#,
    );

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
#[cfg(unix)]
#[tokio::test]
async fn logs_persist_after_run_service_exits() {
    let ws = TestWorkspace::new(
        r#"
[doomed]
run=echo about-to-die && exit 1
"#,
    );

    // The process exits immediately; `eph up` still returns (no healthcheck).
    ws.eph_ok(&["up"]).await;

    let logs = ws.eph_ok(&["logs", "doomed"]).await;
    assert!(
        logs.contains("about-to-die"),
        "expected the dead service's trace to survive, got:\n{}",
        logs
    );

    ws.eph_ok(&["down"]).await;
}

// `port=auto` on a run= service: eph allocates a free host port, injects it into
// the process environment, and resolves it for interpolation -- the core of
// first-party app port creation.
#[cfg(unix)]
#[tokio::test]
async fn run_service_auto_port_is_allocated_and_injected() {
    // The process echoes the PORT it was handed, then stays alive so its log can
    // be read. `env.PORT=${web.port}` is how the assigned port reaches it.
    let ws = TestWorkspace::new(
        r#"
[web]
run=echo "BOUND_PORT=$PORT" && sleep 300
port=auto
env.PORT=${web.port}

APP_URL=http://localhost:${web.port}
"#,
    );

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
#[cfg(unix)]
#[tokio::test]
async fn run_service_auto_port_healthcheck_sees_port() {
    // The check passes only if BOTH the eph-resolved `${web.port}` and the
    // injected `$PORT` env equal the allocated port. If eph ran the healthcheck
    // without the app's env, `$PORT` would be empty; if it didn't resolve the
    // string, `${web.port}` would be a literal -- either way `eph up` would fail.
    let ws = TestWorkspace::new(
        r#"
[web]
run=sleep 300
port=auto
env.PORT=${web.port}
healthcheck=test -n "$PORT" && test "${web.port}" = "$PORT"
ready-timeout=10
"#,
    );

    // `eph_ok` panics if `up` times out, so reaching `down` is the assertion.
    ws.eph_ok(&["up"]).await;
    ws.eph_ok(&["down"]).await;
}

// An eph-allocated auto port stays the same across `eph down` / `eph up`, so a
// managed app's URL is stable for bookmarks and OAuth callbacks.
#[cfg(unix)]
#[tokio::test]
async fn run_service_auto_port_is_stable_across_restart() {
    let ws = TestWorkspace::new(
        r#"
[web]
run=sleep 300
port=auto

APP_URL=http://localhost:${web.port}
"#,
    );

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
#[cfg(unix)]
#[tokio::test]
async fn logs_all_services_tags_each_line() {
    let ws = TestWorkspace::new(
        r#"
[alpha]
run=echo alpha-marker && sleep 300

[beta]
run=echo beta-marker && sleep 300
"#,
    );

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
#[cfg(unix)]
#[tokio::test]
async fn logs_follow_all_services_interleaves() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Two services that each emit a tagged line roughly every 200ms, so a
    // follow-all stream sees output from both within a couple of seconds.
    let ws = TestWorkspace::new(
        r#"
[alpha]
run=i=0; while [ $i -lt 100 ]; do echo alpha-$i; i=$((i+1)); sleep 0.2; done

[beta]
run=i=0; while [ $i -lt 100 ]; do echo beta-$i; i=$((i+1)); sleep 0.2; done
"#,
    );

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
#[cfg(unix)]
#[tokio::test]
async fn logs_tail_returns_last_n_lines() {
    let ws = TestWorkspace::new(
        r#"
[svc]
run=for i in 1 2 3 4 5; do echo tailline-$i; done; sleep 300
"#,
    );

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
