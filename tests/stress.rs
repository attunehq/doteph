//! Heavyweight integration / stress tests for `eph`.
//!
//! Where `tests/integration.rs` proves the happy path one service at a time,
//! this suite stresses the system the way a real user would: standing up whole
//! multi-service environments, running many isolated workspaces at once, and
//! talking to every backend over its real wire protocol to prove the published
//! ports actually work.
//!
//! What it covers:
//! - **Multi-service environment**: postgres + redis + minio in one workspace,
//!   with real SQL / RESP / S3-health connectivity over the mapped host ports.
//! - **Concurrent workspace isolation**: N independent workspaces each running
//!   postgres + redis, brought up in parallel, with assertions that no host
//!   ports or container names collide and that data written to one workspace's
//!   backend is invisible to the others.
//! - **Every service source type**: `image=`, `dockerfile=` (build), `run=`
//!   (shell command), and `compose=`, which the lighter suite never exercises.
//! - **Leak-free teardown**: after `eph clean`, no containers for the
//!   workspace remain.
//!
//! These tests are slow, need a Docker daemon, pull real images, and (for the
//! `run=` service) expect `python3` and a POSIX `sh` on PATH, so every test is
//! marked `#[ignore]`. They are skipped by a bare `cargo test` and run
//! explicitly:
//!
//! ```text
//! make test-stress
//! # or
//! cargo test --test stress -- --ignored --test-threads=1
//! ```
//!
//! CI runs them on every push via a dedicated step. The concurrency factor for
//! the isolation stress test can be raised locally with
//! `EPH_STRESS_WORKSPACES=8`.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

mod common;
use common::{
    TestWorkspace, docker_container_names, docker_remove_image, extract_port, prepull_images,
    retry_until,
};

// ============================================================================
// Service definitions
// ============================================================================

const POSTGRES_IMAGE: &str = "postgres:16-alpine";
const REDIS_IMAGE: &str = "redis:7-alpine";
// Pinned to an immutable RELEASE tag: `minio/minio` has no stable channel other
// than the moving `latest`, and its `server` CLI / health endpoints have changed
// across releases, so a float would let an unrelated MinIO release redden CI.
const MINIO_IMAGE: &str = "minio/minio:RELEASE.2025-09-07T16-13-09Z";

/// A full, realistic stack: a relational DB, a cache, and an object store, all
/// wired together through interpolated connection URLs.
const FULL_STACK_EPH: &str = r#"
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=test
env.POSTGRES_PASSWORD=test
env.POSTGRES_DB=test
healthcheck=pg_isready -U test
ready-timeout=60

[redis]
image=redis:7-alpine
port=6379

[minio]
image=minio/minio:RELEASE.2025-09-07T16-13-09Z
command=server /data --console-address :9001
port.api=9000
port.console=9001
env.MINIO_ROOT_USER=minioadmin
env.MINIO_ROOT_PASSWORD=minioadmin

[env]
DATABASE_URL=postgres://test:test@localhost:${postgres.port}/test
REDIS_URL=redis://localhost:${redis.port}
MINIO_ENDPOINT=http://localhost:${minio.port.api}
MINIO_CONSOLE=http://localhost:${minio.port.console}
"#;

/// A lighter two-service stack (DB + cache) used by the concurrency test, where
/// many copies run at once and a full object store per workspace would be
/// wasteful.
const DB_CACHE_EPH: &str = r#"
[postgres]
image=postgres:16-alpine
port=5432
env.POSTGRES_USER=test
env.POSTGRES_PASSWORD=test
env.POSTGRES_DB=test
healthcheck=pg_isready -U test
ready-timeout=60

[redis]
image=redis:7-alpine
port=6379

[env]
DATABASE_URL=postgres://test:test@localhost:${postgres.port}/test
REDIS_URL=redis://localhost:${redis.port}
"#;

// ============================================================================
// Connectivity: real wire-protocol clients over the mapped host ports
// ============================================================================

/// A parsed RESP reply, covering the reply types the stress suite can receive
/// (simple strings from PING/SET, bulk strings or nil from GET).
#[derive(Debug)]
enum RedisReply {
    Simple(String),
    Bulk(String),
    Nil,
}

/// Send one command and read exactly one complete RESP reply.
///
/// RESP is stream-framed, so a single `read()` can split even a tiny reply.
/// This reads the status line, then for a bulk string reads exactly the
/// declared byte count plus the trailing CRLF, so a fragmented read can never
/// silently truncate a value into a false test failure.
async fn redis_command(port: u16, args: &[&str]) -> Result<RedisReply> {
    // Cap the whole exchange: a peer that accepts the connection but never sends
    // a complete reply must not hang the test forever, because retry_until can
    // only recover if the operation actually returns.
    tokio::time::timeout(Duration::from_secs(5), async {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .with_context(|| format!("connecting to redis on 127.0.0.1:{port}"))?;
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);

        let mut cmd = format!("*{}\r\n", args.len());
        for arg in args {
            write!(cmd, "${}\r\n{}\r\n", arg.len(), arg).expect("writing to String never fails");
        }
        wr.write_all(cmd.as_bytes()).await?;

        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        ensure!(n > 0, "redis closed the connection without replying");
        let line = line.trim_end_matches(['\r', '\n']);
        ensure!(!line.is_empty(), "empty RESP reply line");
        // The first byte of a RESP reply is an ASCII control char (+ - $ : *),
        // so slicing at byte index 1 is a valid char boundary in the branches
        // that take it; the catch-all branch never slices.
        match line.as_bytes()[0] {
            b'+' => Ok(RedisReply::Simple(line[1..].to_string())),
            b'-' => bail!("redis error reply: {}", &line[1..]),
            b'$' => {
                let len: i64 = line[1..].parse().context("invalid bulk-string length")?;
                if len < 0 {
                    return Ok(RedisReply::Nil);
                }
                let mut buf = vec![0u8; len as usize + 2]; // value bytes + trailing CRLF
                reader
                    .read_exact(&mut buf)
                    .await
                    .context("short bulk-string read")?;
                ensure!(buf.ends_with(b"\r\n"), "bulk string not CRLF-terminated");
                buf.truncate(len as usize);
                Ok(RedisReply::Bulk(
                    String::from_utf8(buf).context("non-utf8 bulk string")?,
                ))
            }
            other => bail!("unexpected RESP reply type {:?}: {line:?}", other as char),
        }
    })
    .await
    .context("redis command timed out")?
}

/// `PING` redis and assert it pongs.
async fn redis_ping(port: u16) -> Result<()> {
    match redis_command(port, &["PING"]).await? {
        RedisReply::Simple(s) if s == "PONG" => Ok(()),
        other => bail!("unexpected PING reply: {other:?}"),
    }
}

/// `SET key value` on redis.
async fn redis_set(port: u16, key: &str, value: &str) -> Result<()> {
    match redis_command(port, &["SET", key, value]).await? {
        RedisReply::Simple(s) if s == "OK" => Ok(()),
        other => bail!("unexpected SET reply: {other:?}"),
    }
}

/// `GET key` on redis, returning the value or `None` for a missing key.
async fn redis_get(port: u16, key: &str) -> Result<Option<String>> {
    match redis_command(port, &["GET", key]).await? {
        RedisReply::Bulk(v) => Ok(Some(v)),
        RedisReply::Nil => Ok(None),
        other => bail!("unexpected GET reply: {other:?}"),
    }
}

/// Issue a minimal HTTP/1.1 GET over a raw socket and return the status code.
async fn http_status(port: u16, path: &str) -> Result<u16> {
    // Bounded so a server that accepts but never closes the connection (we send
    // `Connection: close`, but be defensive) cannot hang the test.
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .with_context(|| format!("connecting to http on 127.0.0.1:{port}"))?;

        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await?;

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        let text = String::from_utf8_lossy(&buf);
        let status_line = text.lines().next().context("empty HTTP response")?;
        // e.g. "HTTP/1.1 200 OK"
        let code = status_line
            .split_whitespace()
            .nth(1)
            .context("malformed HTTP status line")?;
        code.parse().context("non-numeric HTTP status code")
    })
    .await
    .context("http request timed out")?
}

/// Probe MinIO's liveness endpoint and assert it reports healthy.
async fn minio_healthy(port: u16) -> Result<()> {
    let code = http_status(port, "/minio/health/live").await?;
    ensure!(code == 200, "minio health endpoint returned {code}");
    Ok(())
}

/// Connect to postgres over the mapped host port. The background connection
/// task drives the protocol for the returned client.
async fn pg_connect(port: u16) -> Result<tokio_postgres::Client> {
    let conn =
        format!("host=127.0.0.1 port={port} user=test password=test dbname=test connect_timeout=5");
    let (client, connection) = tokio_postgres::connect(&conn, tokio_postgres::NoTls)
        .await
        .with_context(|| format!("connecting to postgres on 127.0.0.1:{port}"))?;
    tokio::spawn(async move {
        // The driver future resolves when the client is dropped; ignore the
        // result since teardown closing the socket is expected.
        let _ = connection.await;
    });
    Ok(client)
}

// ============================================================================
// Workspace helpers
// ============================================================================

/// The `eph-<short_id>` container prefix for a workspace, read from `eph info`.
async fn container_prefix(ws: &TestWorkspace) -> String {
    let info = ws.eph_ok(&["info"]).await;
    info.lines()
        .find_map(|l| l.strip_prefix("Container prefix: "))
        .map(|s| s.trim().to_string())
        .expect("`eph info` did not report a container prefix")
}

/// Extract the postgres and redis host ports from a stack workspace's resolved
/// environment (`DATABASE_URL` / `REDIS_URL`).
async fn db_cache_ports(ws: &TestWorkspace) -> (u16, u16) {
    let env = ws.env_json().await;
    let db = env.get("DATABASE_URL").expect("DATABASE_URL not set");
    let redis = env.get("REDIS_URL").expect("REDIS_URL not set");
    let pg_port = extract_port(db).expect("could not extract postgres port");
    let redis_port = extract_port(redis).expect("could not extract redis port");
    (pg_port, redis_port)
}

/// Assert that no containers for `prefix` remain (leak-free teardown).
async fn assert_no_containers(prefix: &str) {
    let leftover = docker_container_names(prefix).await;
    assert!(
        leftover.is_empty(),
        "containers leaked after clean for {prefix}: {leftover:?}"
    );
}

// ============================================================================
// Test 1: full multi-service environment + real connectivity
// ============================================================================

#[tokio::test]
#[ignore = "stress: heavyweight, requires Docker; run via `make test-stress`"]
async fn full_stack_environment_with_real_connectivity() {
    prepull_images(&[POSTGRES_IMAGE, REDIS_IMAGE, MINIO_IMAGE]).await;

    let ws = TestWorkspace::new(FULL_STACK_EPH);
    let prefix = container_prefix(&ws).await;

    // Bring the whole stack up. Postgres has a server-side healthcheck, so `up`
    // blocks until it is accepting connections.
    ws.eph_ok(&["up"]).await;

    // All three containers should exist under this workspace's prefix.
    let running = docker_container_names(&prefix).await;
    assert_eq!(
        running.len(),
        3,
        "expected postgres+redis+minio containers, found: {running:?}"
    );

    // Every connection URL must be fully interpolated (no leftover `${...}`).
    let env = ws.env_json().await;
    for key in [
        "DATABASE_URL",
        "REDIS_URL",
        "MINIO_ENDPOINT",
        "MINIO_CONSOLE",
    ] {
        let value = env.get(key).unwrap_or_else(|| panic!("{key} not set"));
        assert!(!value.contains("${"), "{key} not interpolated: {value}");
    }

    let pg_port = extract_port(&env["DATABASE_URL"]).expect("postgres port");
    let redis_port = extract_port(&env["REDIS_URL"]).expect("redis port");
    let minio_port = extract_port(&env["MINIO_ENDPOINT"]).expect("minio port");
    assert_ne!(pg_port, redis_port);
    assert_ne!(pg_port, minio_port);
    assert_ne!(redis_port, minio_port);

    // --- Postgres: a real SQL round-trip over the mapped port. ---
    let client = retry_until(Duration::from_secs(60), || pg_connect(pg_port))
        .await
        .expect("postgres never accepted a connection");
    client
        .batch_execute(
            "CREATE TABLE widgets (id INT PRIMARY KEY, name TEXT);
             INSERT INTO widgets (id, name) VALUES (1, 'gadget');",
        )
        .await
        .expect("postgres DDL/DML failed");
    let row = client
        .query_one("SELECT name FROM widgets WHERE id = $1", &[&1i32])
        .await
        .expect("postgres SELECT failed");
    let name: String = row.get(0);
    assert_eq!(name, "gadget");

    // --- Redis: a real SET/GET round-trip over the mapped port. ---
    retry_until(Duration::from_secs(30), || redis_ping(redis_port))
        .await
        .expect("redis never answered PING");
    redis_set(redis_port, "greeting", "hello")
        .await
        .expect("redis SET failed");
    let got = redis_get(redis_port, "greeting")
        .await
        .expect("redis GET failed");
    assert_eq!(got.as_deref(), Some("hello"));

    // --- MinIO: real S3 health probe over the mapped API port. ---
    retry_until(Duration::from_secs(30), || minio_healthy(minio_port))
        .await
        .expect("minio never became healthy");

    // Tear the whole environment down and prove nothing leaked.
    ws.clean().await;
    assert_no_containers(&prefix).await;
}

// ============================================================================
// Test 2: concurrent independent environments, isolation under stress
// ============================================================================

// A multi-thread runtime so the N workspaces are brought up with genuine
// parallelism. This is orthogonal to libtest's `--test-threads=1`, which
// serializes whole test functions (so this heavy test never overlaps the
// full-stack one) but does not touch this test's own tokio worker threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress: heavyweight, requires Docker; run via `make test-stress`"]
async fn concurrent_workspaces_are_isolated() {
    let n: usize = std::env::var("EPH_STRESS_WORKSPACES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    assert!(n >= 2, "isolation test needs at least 2 workspaces");

    // Pull the shared images once so the concurrent burst stresses eph's
    // orchestration, not Docker's first-time image pulls.
    prepull_images(&[POSTGRES_IMAGE, REDIS_IMAGE]).await;

    let workspaces: Vec<TestWorkspace> = (0..n).map(|_| TestWorkspace::new(DB_CACHE_EPH)).collect();

    let prefixes: Vec<String> = {
        let mut v = Vec::with_capacity(n);
        for ws in &workspaces {
            v.push(container_prefix(ws).await);
        }
        v
    };

    // Every workspace gets a distinct id, hence a distinct container prefix.
    let unique_prefixes: HashSet<&String> = prefixes.iter().collect();
    assert_eq!(
        unique_prefixes.len(),
        n,
        "workspace prefixes collided: {prefixes:?}"
    );

    // Bring all N stacks up concurrently.
    let ups = workspaces.iter().map(|ws| ws.eph_ok(&["up"]));
    futures_util::future::join_all(ups).await;

    // Collect every host port across every workspace; none may collide.
    let mut all_ports: Vec<u16> = Vec::with_capacity(n * 2);
    let mut ports_per_ws: Vec<(u16, u16)> = Vec::with_capacity(n);
    for ws in &workspaces {
        let (pg, redis) = db_cache_ports(ws).await;
        all_ports.push(pg);
        all_ports.push(redis);
        ports_per_ws.push((pg, redis));
    }
    let unique_ports: HashSet<u16> = all_ports.iter().copied().collect();
    assert_eq!(
        unique_ports.len(),
        all_ports.len(),
        "host ports collided across workspaces: {all_ports:?}"
    );

    // Write a workspace-specific marker into each backend. Every workspace uses
    // the SAME key/table, so if any two shared a backend the later writer would
    // clobber the earlier one and the read-back below would fail.
    for (i, (pg_port, redis_port)) in ports_per_ws.iter().enumerate() {
        let marker = format!("ws{i}");
        retry_until(Duration::from_secs(30), || redis_ping(*redis_port))
            .await
            .expect("redis never answered PING");
        redis_set(*redis_port, "owner", &marker)
            .await
            .expect("redis SET failed");

        let client = retry_until(Duration::from_secs(60), || pg_connect(*pg_port))
            .await
            .expect("postgres never accepted a connection");
        client
            .batch_execute("CREATE TABLE IF NOT EXISTS owner (id INT)")
            .await
            .expect("postgres DDL failed");
        client
            .execute("INSERT INTO owner (id) VALUES ($1)", &[&(i as i32)])
            .await
            .expect("postgres INSERT failed");
    }

    // Read every marker back and assert each backend holds only its own data.
    for (i, (pg_port, redis_port)) in ports_per_ws.iter().enumerate() {
        let owner = redis_get(*redis_port, "owner")
            .await
            .expect("redis GET failed");
        assert_eq!(
            owner.as_deref(),
            Some(format!("ws{i}").as_str()),
            "redis isolation breach: workspace {i} saw {owner:?}"
        );

        let client = pg_connect(*pg_port)
            .await
            .expect("postgres reconnect failed");
        let rows = client
            .query("SELECT id FROM owner", &[])
            .await
            .expect("postgres SELECT failed");
        assert_eq!(
            rows.len(),
            1,
            "postgres isolation breach: workspace {i} saw {} rows",
            rows.len()
        );
        let id: i32 = rows[0].get(0);
        assert_eq!(id, i as i32, "postgres isolation breach in workspace {i}");
    }

    // Tear all environments down concurrently and prove none leaked.
    let cleans = workspaces.iter().map(|ws| ws.eph_ok(&["clean"]));
    futures_util::future::join_all(cleans).await;
    for prefix in &prefixes {
        assert_no_containers(prefix).await;
    }
}

// ============================================================================
// Test 3: `run=` shell-command service
// ============================================================================

// eph's run= support is cross-platform (the shell is `sh -c` on Unix, `cmd /C`
// on Windows, with native PID liveness/teardown), but this fixture is Unix
// shaped: the command (`python3 -m http.server`) is a POSIX invocation, and on
// native Windows the spawned server's stdout pipe handle is leaked into the
// grandchild via bInheritHandles, which would hang the harness. Gate to Unix.
#[cfg(unix)]
#[tokio::test]
#[ignore = "stress: requires Docker host plus `python3`/`sh` on PATH"]
async fn source_type_run_shell_command() {
    // A non-Docker service: eph spawns the process and tracks its PID. `run=`
    // services bind their declared port directly on the host (no Docker
    // remapping), so reserve a currently-free port instead of hard-coding one
    // that might already be taken on a CI runner.
    let port = {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("could not reserve a local port");
        let p = listener.local_addr().unwrap().port();
        drop(listener);
        p
    };
    let eph = format!(
        "[web]\nrun=python3 -m http.server {port}\nport={port}\n\n[env]\nWEB_URL=http://localhost:${{web.port}}\n"
    );
    let ws = TestWorkspace::new(&eph);

    ws.eph_ok(&["up"]).await;

    // The interpolated URL should carry the declared port.
    let env = ws.env_json().await;
    assert_eq!(
        env.get("WEB_URL").map(String::as_str),
        Some(format!("http://localhost:{port}").as_str())
    );

    // The HTTP server should answer a real request.
    retry_until(Duration::from_secs(20), || async {
        let code = http_status(port, "/").await?;
        ensure!(code == 200, "http.server returned {code}");
        Ok::<(), anyhow::Error>(())
    })
    .await
    .expect("python http.server never served a request");

    ws.clean().await;
}

// ============================================================================
// Test 4: `dockerfile=` build-from-source service
// ============================================================================

#[tokio::test]
#[ignore = "stress: heavyweight, builds an image; requires Docker"]
async fn source_type_dockerfile_build() {
    prepull_images(&[REDIS_IMAGE]).await;

    let ws = TestWorkspace::new("[cache]\ndockerfile=Dockerfile\nport=6379\n");
    // A Dockerfile with a real build step, so this exercises the build path
    // rather than just re-running a base image.
    ws.write_file(
        "Dockerfile",
        "FROM redis:7-alpine\nRUN echo built > /built\n",
    );

    let prefix = container_prefix(&ws).await;
    // build_and_run tags the image `eph-<short_id>-<service>`, i.e. `<prefix>-<service>`.
    let built_image = format!("{prefix}-cache");

    ws.eph_ok(&["up"]).await;

    // Find the mapped port from status and prove the built image runs redis.
    let env_port = {
        let status = ws.eph_ok(&["status"]).await;
        // status prints "  cache -> localhost:<port>"
        status
            .lines()
            .find_map(|l| l.split("localhost:").nth(1))
            .and_then(|p| p.trim().parse::<u16>().ok())
            .expect("could not find mapped port in status output")
    };
    retry_until(Duration::from_secs(30), || redis_ping(env_port))
        .await
        .expect("redis (from built image) never answered PING");

    ws.clean().await;
    assert_no_containers(&prefix).await;

    // `eph clean` removes containers/volumes/state but not the built image.
    docker_remove_image(&built_image).await;
}

// ============================================================================
// Test 5: `compose=` docker-compose service
// ============================================================================

#[tokio::test]
#[ignore = "stress: heavyweight, requires Docker + compose v2"]
async fn source_type_compose() {
    prepull_images(&[REDIS_IMAGE]).await;

    let ws = TestWorkspace::new(
        // The alias (`cache`) is the interpolation name. The value explicitly
        // targets the Compose service (`redis`) and its container port.
        "[cache]\ncompose=docker-compose.yml\nexpose.cache=redis:6379\n\n[env]\nREDIS_URL=redis://localhost:${cache.port.cache}\n",
    );
    ws.write_file(
        "docker-compose.yml",
        "services:\n  redis:\n    image: redis:7-alpine\n    ports:\n      - \"6379\"\n",
    );
    let prefix = container_prefix(&ws).await;

    ws.eph_ok(&["up"]).await;

    let env = ws.env_json().await;
    let url = env.get("REDIS_URL").expect("REDIS_URL not set");
    assert!(!url.contains("${"), "REDIS_URL not interpolated: {url}");
    let port = extract_port(url).expect("could not extract compose redis port");

    retry_until(Duration::from_secs(30), || redis_ping(port))
        .await
        .expect("compose redis never answered PING");

    // `eph clean` runs `docker compose down`; prove the project's containers are
    // gone. docker compose names them `<project>-<service>-N`, which still
    // starts with this workspace's prefix.
    ws.clean().await;
    assert_no_containers(&prefix).await;
}
