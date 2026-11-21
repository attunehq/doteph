//! `.eph.local`: the optional per-checkout override merged over `.eph`.
//!
//! The merge rules themselves are unit-tested in `src/parser.rs`; these tests
//! prove that every command reads the same merged configuration through the
//! real binary. None of them needs Docker: overrides are exercised with
//! `run=` services and parse-only commands.

use std::process::Output;

use crate::common::{TestWorkspace, parse_env_json};

/// A `run=` command that prints `marker` and then stays alive.
#[cfg(unix)]
fn echo_and_wait(marker: &str) -> String {
    format!("echo {marker} && sleep 300")
}

#[cfg(windows)]
fn echo_and_wait(marker: &str) -> String {
    format!("echo {marker}& ping -n 300 127.0.0.1 >NUL")
}

#[cfg(unix)]
fn print_log_level() -> Vec<&'static str> {
    vec!["run", "printenv", "LOG_LEVEL"]
}

#[cfg(windows)]
fn print_log_level() -> Vec<&'static str> {
    vec!["run", "cmd", "/C", "echo %LOG_LEVEL%"]
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Values from `.eph.local` reach `check`, `env`, `run`, and `info`, and
/// `info` lists the layers it loaded, base first.
#[tokio::test]
async fn local_override_reaches_check_env_run_and_info() {
    let ws = TestWorkspace::new("LOG_LEVEL=info\nKEEP=base\n");

    let info = ws.eph_ok(&["info"]).await;
    assert!(info.contains("Config layers"), "got:\n{info}");
    assert!(!info.contains(".eph.local"), "got:\n{info}");

    ws.write_file(
        ".eph.local",
        "[env]\nLOG_LEVEL=debug\nLOCAL_ONLY=yes\n\n[mailpit]\nimage=axllent/mailpit:latest\nport=8025\n",
    );

    let env = parse_env_json(&ws.eph_ok(&["env", "-f", "json"]).await);
    assert_eq!(env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
    assert_eq!(env.get("KEEP").map(String::as_str), Some("base"));
    assert_eq!(env.get("LOCAL_ONLY").map(String::as_str), Some("yes"));

    let run = ws.eph_ok(&print_log_level()).await;
    assert_eq!(run.trim(), "debug");

    let check = ws.eph_ok(&["check"]).await;
    assert!(check.contains("Override:"), "got:\n{check}");
    assert!(check.contains(".eph.local"), "got:\n{check}");
    assert!(check.contains("mailpit"), "got:\n{check}");

    let info = ws.eph_ok(&["info"]).await;
    let layers: Vec<&str> = info
        .lines()
        .skip_while(|line| !line.starts_with("Config layers"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(str::trim)
        .collect();
    let names: Vec<_> = layers
        .iter()
        .map(|layer| std::path::Path::new(layer).file_name().unwrap())
        .collect();
    assert_eq!(names, [".eph", ".eph.local"], "got:\n{info}");
}

/// `up`, `status`, `logs`, `down`, and `clean` act on the merged services: a
/// replaced `run=` command and a service only the override declares.
#[tokio::test]
async fn local_override_reaches_the_service_lifecycle() {
    let ws = TestWorkspace::new(&format!("[web]\nrun={}\n", echo_and_wait("base-web")));
    ws.write_file(
        ".eph.local",
        &format!(
            "[web]\nrun={}\n\n[worker]\nrun={}\n",
            echo_and_wait("local-web"),
            echo_and_wait("local-worker")
        ),
    );

    ws.eph_ok(&["up"]).await;

    let status = ws.eph_ok(&["status"]).await;
    assert!(status.contains("web"), "got:\n{status}");
    assert!(status.contains("worker"), "got:\n{status}");

    let web_logs = ws.eph_ok(&["logs", "web"]).await;
    assert!(web_logs.contains("local-web"), "got:\n{web_logs}");
    assert!(!web_logs.contains("base-web"), "got:\n{web_logs}");
    let worker_logs = ws.eph_ok(&["logs", "worker"]).await;
    assert!(worker_logs.contains("local-worker"), "got:\n{worker_logs}");

    ws.eph_ok(&["down"]).await;
    let after = ws.eph_ok(&["status"]).await;
    assert!(after.contains("No services running"), "got:\n{after}");
    ws.clean().await;
}

/// An override that makes the merged result invalid breaks every command that
/// loads configuration, each with a diagnostic naming both layers. `info`
/// still works, since it only lists the layers.
#[tokio::test]
async fn every_config_command_validates_the_merged_result() {
    let ws = TestWorkspace::new(&format!("[web]\nrun={}\n", echo_and_wait("web")));
    ws.write_file(".eph.local", "[web]\nimage=nginx:alpine\n");

    let commands: Vec<Vec<&str>> = vec![
        vec!["check"],
        vec!["env"],
        vec!["up"],
        vec!["down"],
        vec!["status"],
        vec!["logs"],
        vec!["clean"],
        vec!["dev"],
        print_log_level(),
    ];
    for args in &commands {
        let output = ws.eph(args).await;
        let err = stderr(&output);
        assert!(!output.status.success(), "eph {args:?} should fail");
        assert!(
            err.contains("more than one source")
                && err.contains("line 2 of .eph and")
                && err.contains("line 2 of .eph.local"),
            "eph {args:?} should report both layers, got:\n{err}"
        );
    }

    let info = ws.eph(&["info"]).await;
    assert!(info.status.success(), "got:\n{}", stderr(&info));
    assert!(stdout(&info).contains(".eph.local"));
}

/// `.eph.local` is only read beside the discovered `.eph`: alone it does not
/// make a workspace, and one in a subdirectory is ignored.
#[tokio::test]
async fn local_override_is_only_read_beside_the_base_file() {
    let ws = TestWorkspace::new("LOG_LEVEL=info\n");
    let sub = ws.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join(".eph.local"), "LOG_LEVEL=sub\n").unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_eph"))
        .args(["env", "-f", "json"])
        .current_dir(&sub)
        .output()
        .await
        .expect("failed to run eph");
    assert!(output.status.success(), "got:\n{}", stderr(&output));
    let env = parse_env_json(&stdout(&output));
    assert_eq!(env.get("LOG_LEVEL").map(String::as_str), Some("info"));

    // Without the base file, the override alone is not a workspace.
    std::fs::remove_file(ws.path().join(".eph")).unwrap();
    ws.write_file(".eph.local", "LOG_LEVEL=local\n");
    let output = ws.eph(&["check"]).await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no .eph file found"),
        "got:\n{}",
        stderr(&output)
    );
}
