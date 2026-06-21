//! Integration tests for eph
//!
//! These tests exercise the full stack: parsing .eph files, starting services,
//! interpolating environment variables, and running lifecycle hooks.
//!
//! Tests assume Docker is running and available.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;

// ============================================================================
// Test Harness
// ============================================================================

/// A test workspace - a temporary directory with a .eph file
struct TestWorkspace {
    dir: TempDir,
}

impl TestWorkspace {
    /// Create a new test workspace with the given .eph content
    fn new(eph_content: &str) -> Self {
        let dir = TempDir::new().expect("Failed to create temp dir");
        std::fs::write(dir.path().join(".eph"), eph_content).expect("Failed to write .eph");
        TestWorkspace { dir }
    }

    /// Create a file in the workspace
    fn write_file(&self, path: &str, content: &str) {
        let full_path = self.dir.path().join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("Failed to create parent dirs");
        }
        std::fs::write(full_path, content).expect("Failed to write file");
    }

    /// Get the workspace path
    fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Run eph command in this workspace
    async fn eph(&self, args: &[&str]) -> Output {
        let eph_binary = env!("CARGO_BIN_EXE_eph");
        Command::new(eph_binary)
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .await
            .expect("Failed to run eph")
    }

    /// Run eph and assert success
    async fn eph_ok(&self, args: &[&str]) -> String {
        let output = self.eph(args).await;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            panic!(
                "eph {:?} failed:\nstdout: {}\nstderr: {}",
                args, stdout, stderr
            );
        }
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        // Best effort cleanup: stop any services that might be running
        let eph_binary = env!("CARGO_BIN_EXE_eph");
        let _ = std::process::Command::new(eph_binary)
            .args(["down"])
            .current_dir(self.dir.path())
            .output();
    }
}

/// Parse JSON output from `eph env -f json`
fn parse_env_json(output: &str) -> HashMap<String, String> {
    serde_json::from_str(output).expect("Failed to parse env JSON")
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

/// Extract port number from a string (URL or direct port)
fn extract_port(s: &str) -> Option<u16> {
    // Try direct parse
    if let Ok(p) = s.parse::<u16>() {
        return Some(p);
    }

    // Try extracting from URL like "postgres://...localhost:12345/..."
    if let Some(idx) = s.rfind("localhost:") {
        let after = &s[idx + 10..];
        let port_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = port_str.parse::<u16>() {
            return Some(p);
        }
    }

    // Try extracting from "host:port" format
    if let Some(idx) = s.rfind(':') {
        let port_str: String = s[idx + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(p) = port_str.parse::<u16>() {
            return Some(p);
        }
    }

    None
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
    let ws = TestWorkspace::new(
        r#"
[redis]
image=redis:7-alpine
port=6379
post-start=touch /tmp/eph-test-marker

REDIS_URL=redis://localhost:${redis.port}
"#,
    );

    // Create the marker script that creates a file in the workspace
    ws.write_file(
        "marker.sh",
        &format!("#!/bin/sh\ntouch {}/post-start-ran", ws.path().display()),
    );

    // Update .eph to use our script
    std::fs::write(
        ws.path().join(".eph"),
        format!(
            r#"
[redis]
image=redis:7-alpine
port=6379
post-start=touch {}/post-start-ran

REDIS_URL=redis://localhost:${{redis.port}}
"#,
            ws.path().display()
        ),
    )
    .unwrap();

    // Start service
    ws.eph_ok(&["up", "redis"]).await;
    sleep(Duration::from_secs(1)).await;

    // Check marker file exists
    let marker = ws.path().join("post-start-ran");
    assert!(marker.exists(), "post-start hook did not run");

    // Cleanup
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
