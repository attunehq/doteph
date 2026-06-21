//! Shared test harness for the `eph` integration and stress suites.
//!
//! Both `tests/integration.rs` and `tests/stress.rs` drive the real compiled
//! `eph` binary against a temporary workspace and a live Docker daemon. This
//! module owns the pieces they have in common: the [`TestWorkspace`] harness,
//! JSON env parsing, port extraction, and a small async retry helper used to
//! poll for service readiness instead of sleeping a fixed amount.
//!
//! Each integration test file includes this module separately (`mod common;`),
//! so an item used by only one of them looks unused to the other. The
//! crate-level `allow(dead_code)` keeps `-D warnings` happy without sprinkling
//! per-item attributes.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;

// ============================================================================
// Test Harness
// ============================================================================

/// A test workspace: a temporary directory containing a `.eph` file.
///
/// Dropping the workspace makes a best-effort `eph down` so containers do not
/// outlive a panicking test. Tests that create volumes, built images, or
/// compose projects should additionally call [`TestWorkspace::clean`] (and tidy
/// any images) on the happy path, since `down` only stops containers.
pub struct TestWorkspace {
    dir: TempDir,
}

impl TestWorkspace {
    /// Create a new test workspace with the given `.eph` content.
    pub fn new(eph_content: &str) -> Self {
        let dir = TempDir::new().expect("Failed to create temp dir");
        std::fs::write(dir.path().join(".eph"), eph_content).expect("Failed to write .eph");
        TestWorkspace { dir }
    }

    /// Write an additional file into the workspace (e.g. a `Dockerfile` or
    /// `docker-compose.yml` referenced by the `.eph`).
    pub fn write_file(&self, name: &str, contents: &str) {
        std::fs::write(self.dir.path().join(name), contents)
            .unwrap_or_else(|e| panic!("Failed to write {name}: {e}"));
    }

    /// Get the workspace path.
    pub fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Run an `eph` subcommand in this workspace, returning the raw output.
    pub async fn eph(&self, args: &[&str]) -> Output {
        let eph_binary = env!("CARGO_BIN_EXE_eph");
        Command::new(eph_binary)
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .await
            .expect("Failed to run eph")
    }

    /// Run an `eph` subcommand and assert it succeeded, returning stdout.
    pub async fn eph_ok(&self, args: &[&str]) -> String {
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

    /// Convenience: run `eph env -f json` and parse the result.
    pub async fn env_json(&self) -> HashMap<String, String> {
        let output = self.eph_ok(&["env", "-f", "json"]).await;
        parse_env_json(&output)
    }

    /// Convenience: fully reset the workspace (`eph clean`).
    pub async fn clean(&self) {
        self.eph_ok(&["clean"]).await;
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        // Best effort cleanup: stop any services that might still be running.
        let eph_binary = env!("CARGO_BIN_EXE_eph");
        let _ = std::process::Command::new(eph_binary)
            .args(["down"])
            .current_dir(self.dir.path())
            .output();
    }
}

// ============================================================================
// Parsing Helpers
// ============================================================================

/// Parse the JSON object emitted by `eph env -f json`.
pub fn parse_env_json(output: &str) -> HashMap<String, String> {
    serde_json::from_str(output).expect("Failed to parse env JSON")
}

/// Extract a port number from a string: either a bare port, or the host port in
/// a `scheme://host:port/...` style URL.
pub fn extract_port(s: &str) -> Option<u16> {
    // Try direct parse.
    if let Ok(p) = s.parse::<u16>() {
        return Some(p);
    }

    // Try extracting from a URL like "postgres://...localhost:12345/...".
    if let Some(idx) = s.rfind("localhost:") {
        let after = &s[idx + 10..];
        let port_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = port_str.parse::<u16>() {
            return Some(p);
        }
    }

    // Try extracting from a "host:port" suffix.
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
// Async Retry
// ============================================================================

/// Poll an async operation until it succeeds or `timeout` elapses.
///
/// Services started by `eph` without a server-side healthcheck (redis, minio)
/// may take a moment to accept host connections after `eph up` returns. Rather
/// than sleeping a fixed, flaky amount, callers retry the real operation (a
/// protocol ping, an HTTP probe, a SQL connect) until it works. The most recent
/// error is returned on timeout so failures are diagnosable.
pub async fn retry_until<F, Fut, T, E>(timeout: Duration, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let start = Instant::now();
    let interval = Duration::from_millis(250);
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if start.elapsed() >= timeout {
                    return Err(err);
                }
                sleep(interval).await;
            }
        }
    }
}

/// List the names of Docker containers (running or stopped) whose name contains
/// `name_filter`. Used to assert that a workspace's containers exist while up
/// and are gone after `eph clean`.
pub async fn docker_container_names(name_filter: &str) -> Vec<String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--format",
            "{{.Names}}",
            "--filter",
            &format!("name={name_filter}"),
        ])
        .output()
        .await
        .expect("failed to run docker ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Best-effort `docker pull` of each image, ignoring failures. Pulling the
/// heavyweight images once up front keeps concurrency tests focused on `eph`'s
/// orchestration rather than racing several simultaneous first-time pulls.
pub async fn prepull_images(images: &[&str]) {
    for image in images {
        let _ = Command::new("docker").args(["pull", image]).output().await;
    }
}

/// Best-effort `docker rmi` of an image, ignoring failures. Used to tidy up
/// images produced by `dockerfile=` builds, which `eph clean` does not remove.
pub async fn docker_remove_image(image: &str) {
    let _ = Command::new("docker")
        .args(["rmi", "-f", image])
        .output()
        .await;
}
