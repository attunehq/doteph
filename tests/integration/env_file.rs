//! `env-file=`: importing a `.env` file's variables into the workspace.
//!
//! The precedence and grammar rules are unit-tested in `src/parser.rs`; these
//! tests prove that the real binary reads imported files relative to the
//! `.eph`, and that every consumer of the top-level environment sees them.
//! None of them needs Docker.

use std::process::Output;

use crate::common::{TestWorkspace, parse_env_json};

/// A `run=` command that prints `$FROM_DOTENV` and then stays alive.
#[cfg(unix)]
fn print_from_dotenv_and_wait() -> &'static str {
    "echo service-saw-$FROM_DOTENV && sleep 300"
}

#[cfg(windows)]
fn print_from_dotenv_and_wait() -> &'static str {
    "echo service-saw-%FROM_DOTENV%& ping -n 300 127.0.0.1 >NUL"
}

#[cfg(unix)]
fn print_from_dotenv() -> Vec<&'static str> {
    vec!["run", "printenv", "FROM_DOTENV"]
}

#[cfg(windows)]
fn print_from_dotenv() -> Vec<&'static str> {
    vec!["run", "cmd", "/C", "echo %FROM_DOTENV%"]
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The file names `eph info` lists under "Config layers", in order.
fn info_layers(info: &str) -> Vec<String> {
    info.lines()
        .skip_while(|line| !line.starts_with("Config layers"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| {
            let path = line.trim().split(" (").next().unwrap();
            std::path::Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Imported variables reach `env`, `run`, and `run=` services; each layer
/// overrides its own imports; `check` and `info` list the imported files in
/// precedence order.
#[tokio::test]
async fn env_files_reach_every_consumer_in_layer_order() {
    let ws = TestWorkspace::new(&format!(
        "env-file=.env\nLOG_LEVEL=info\n\n[web]\nrun={}\n",
        print_from_dotenv_and_wait()
    ));
    ws.write_file(
        ".env",
        "FROM_DOTENV=yes\nLOG_LEVEL=from-dotenv\nSECRET=placeholder\n",
    );
    ws.write_file(".eph.local", "env-file=.env.local\n");
    ws.write_file(".env.local", "SECRET=real\n");

    let env = parse_env_json(&ws.eph_ok(&["env", "-f", "json"]).await);
    assert_eq!(env.get("FROM_DOTENV").map(String::as_str), Some("yes"));
    assert_eq!(env.get("LOG_LEVEL").map(String::as_str), Some("info"));
    assert_eq!(env.get("SECRET").map(String::as_str), Some("real"));

    let run = ws.eph_ok(&print_from_dotenv()).await;
    assert_eq!(run.trim(), "yes");

    ws.eph_ok(&["up"]).await;
    let logs = ws.eph_ok(&["logs", "web"]).await;
    assert!(logs.contains("service-saw-yes"), "got:\n{logs}");
    ws.eph_ok(&["down"]).await;

    let check = ws.eph_ok(&["check"]).await;
    assert!(
        check.contains("Env file: ") && check.contains("(env-file= at line 1 of .eph)"),
        "got:\n{check}"
    );
    assert!(
        check.contains("(env-file= at line 1 of .eph.local)"),
        "got:\n{check}"
    );
    assert!(check.contains("FROM_DOTENV"), "got:\n{check}");

    let info = ws.eph_ok(&["info"]).await;
    assert_eq!(
        info_layers(&info),
        [".env", ".eph", ".env.local", ".eph.local"],
        "got:\n{info}"
    );
    ws.clean().await;
}

/// Paths resolve against the directory holding `.eph`, not the caller's
/// working directory.
#[tokio::test]
async fn env_file_paths_resolve_beside_the_eph_file() {
    let ws = TestWorkspace::new("env-file=.env\n");
    ws.write_file(".env", "FROM_DOTENV=root\n");
    let sub = ws.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join(".env"), "FROM_DOTENV=sub\n").unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_eph"))
        .args(["env", "-f", "json"])
        .current_dir(&sub)
        .output()
        .await
        .expect("failed to run eph");
    assert!(output.status.success(), "got:\n{}", stderr(&output));
    let env = parse_env_json(&stdout(&output));
    assert_eq!(env.get("FROM_DOTENV").map(String::as_str), Some("root"));
}

/// A missing env file fails every command that loads configuration, naming
/// the declaration and the resolved path; `info` still works.
#[tokio::test]
async fn a_missing_env_file_is_an_error() {
    let ws = TestWorkspace::new("A=1\nenv-file=.env\n");

    for args in [vec!["check"], vec!["env"]] {
        let output = ws.eph(&args).await;
        let err = stderr(&output);
        assert!(!output.status.success(), "eph {args:?} should fail");
        assert!(
            err.contains("line 2 of .eph: cannot read env file '.env'"),
            "eph {args:?} got:\n{err}"
        );
        let resolved = ws.path().join(".env");
        assert!(
            err.contains(&resolved.display().to_string()),
            "eph {args:?} should name {}, got:\n{err}",
            resolved.display()
        );
    }

    let info = ws.eph(&["info"]).await;
    assert!(info.status.success(), "got:\n{}", stderr(&info));
    assert!(
        stdout(&info).contains("imported env files not shown"),
        "got:\n{}",
        stdout(&info)
    );
}
