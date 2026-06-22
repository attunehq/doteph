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

#[tokio::test]
async fn post_start_hook_runs() {
    // The hook writes its marker with a path relative to the workspace. eph
    // runs hooks with the working directory set to the workspace, so the marker
    // lands in `ws.path()`. Using a relative path keeps the `sh -c` command free
    // of host-absolute paths, which on Windows would contain backslashes that
    // the POSIX shell would interpret as escapes.
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start=touch post-start-ran

REDIS_URL=redis://localhost:${redis.port}
"#,
    );

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
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start=printf '%s\n%s\n%s\n' "$REDIS_URL" "$EPH_REDIS_PORT" "$EPH_WORKSPACE_ID" > hook-env

REDIS_URL=redis://localhost:${redis.port}
"#,
    );

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
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379

[worker]
image=redis:7-alpine
port=6379
post-start=printf '%s' "$REDIS_URL" > worker-saw

REDIS_URL=redis://localhost:${redis.port}
"#,
    );

    ws.eph_ok(&["up"]).await;
    sleep(Duration::from_secs(1)).await;

    let captured = std::fs::read_to_string(ws.path().join("worker-saw"))
        .expect("worker post-start did not run");
    assert!(
        !captured.contains("${"),
        "cross-service ref not resolved in post-start: {captured}"
    );

    let env_map = ws.env_json().await;
    let redis_url = env_map.get("REDIS_URL").expect("REDIS_URL not found");
    assert_eq!(
        &captured, redis_url,
        "worker hook saw a stale or unresolved REDIS_URL"
    );

    ws.eph_ok(&["down"]).await;
}

/// post-start hooks run on every `eph up`, including when a stopped container is
/// restarted -- not only on fresh creation.
#[tokio::test]
async fn post_start_reruns_on_restart() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start=printf 'x' >> ran-count
"#,
    );

    // Fresh create -> post-start runs once.
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Stop but keep the container, then bring it back up (the restart path).
    ws.eph_ok(&["down"]).await;
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    let count = std::fs::read_to_string(ws.path().join("ran-count")).unwrap_or_default();
    assert_eq!(
        count.len(),
        2,
        "post-start should run on both create and restart, got {count:?}"
    );

    ws.eph_ok(&["down"]).await;
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
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-stop=true
"#,
    );
    ws.eph_ok(&["down"]).await;
}

/// A pre-stop hook should receive the same resolved environment as post-start.
#[tokio::test]
async fn pre_stop_hook_receives_resolved_env() {
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
pre-stop=printf '%s' "$REDIS_URL" > pre-stop-env

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

    // Stopping triggers the pre-stop hook.
    ws.eph_ok(&["down"]).await;

    let captured = std::fs::read_to_string(ws.path().join("pre-stop-env"))
        .expect("pre-stop hook did not write pre-stop-env");
    assert_eq!(captured, redis_url, "pre-stop hook saw a different REDIS_URL");
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

    // The command is executed directly (no shell), so use `printenv` to read a
    // single variable rather than relying on shell expansion.
    let out = ws.eph_ok(&["run", "printenv", "REDIS_URL"]).await;
    assert_eq!(out.trim(), redis_url, "eph run did not inject REDIS_URL");

    let port_out = ws.eph_ok(&["run", "printenv", "EPH_REDIS_PORT"]).await;
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
    // still execs the command. `sh -c 'exit 7'` must surface as exit code 7.
    let output = ws.eph(&["run", "sh", "-c", "exit 7"]).await;
    assert_eq!(
        output.status.code(),
        Some(7),
        "eph run did not propagate the child exit code"
    );
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
