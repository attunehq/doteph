//! Platform overrides: `.eph.<os>`, `.eph.<os>.<arch>`, and
//! `.eph.<os>.<arch>.<variant>` merged over `.eph` for the host running eph.
//!
//! The platform mapping and the merge rules are unit-tested in
//! `src/platform.rs`, `src/workspace.rs`, and `src/parser.rs`; these tests prove
//! the real binary loads the host's layers, and only those, on whichever
//! machine runs them. None of them needs Docker.

use eph::platform::HostPlatform;

use crate::common::{TestWorkspace, parse_env_json};

/// The host's OS and architecture layers reach `check`, `env`, and `info`, in
/// precedence order under `.eph.local`; a file for another OS is ignored.
#[tokio::test]
async fn host_platform_layers_merge_in_precedence_order() {
    let host = HostPlatform::current();
    let files = host.override_files();
    let (os_file, arch_file) = (&files[0], &files[1]);
    let other_os = if host.os == "windows" {
        ".eph.linux"
    } else {
        ".eph.windows"
    };

    let ws = TestWorkspace::new("A=base\nB=base\nC=base\nD=base\n");
    ws.write_file(os_file, "B=os\nC=os\nD=os\n");
    ws.write_file(arch_file, "C=arch\nD=arch\n");
    ws.write_file(".eph.local", "D=local\n");
    ws.write_file(other_os, "A=other\nB=other\nC=other\nD=other\n");

    let env = parse_env_json(&ws.eph_ok(&["env", "-f", "json"]).await);
    let get = |key: &str| env.get(key).map(String::as_str);
    assert_eq!(
        [get("A"), get("B"), get("C"), get("D")],
        [Some("base"), Some("os"), Some("arch"), Some("local")]
    );

    let check = ws.eph_ok(&["check"]).await;
    assert!(check.contains("Override: "), "got:\n{check}");
    assert!(check.contains(os_file.as_str()), "got:\n{check}");
    assert!(!check.contains(other_os), "got:\n{check}");

    let info = ws.eph_ok(&["info"]).await;
    assert!(
        info.contains(&format!("Host platform: {host}")),
        "got:\n{info}"
    );
    let layers: Vec<String> = info
        .lines()
        .skip_while(|line| !line.starts_with("Config layers"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| {
            std::path::Path::new(line.trim())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        layers,
        [".eph", os_file.as_str(), arch_file.as_str(), ".eph.local"],
        "got:\n{info}"
    );
}

/// A platform layer may complete a service the base leaves unfinished, since
/// committed layers are validated after merging; `.eph.local` still cannot
/// hide an error in the committed stack.
#[tokio::test]
async fn platform_layers_merge_before_validation() {
    let host = HostPlatform::current();
    let os_file = &host.override_files()[0];

    let ws = TestWorkspace::new("[api]\nport=8080\n");
    let output = ws.eph(&["check"]).await;
    assert!(!output.status.success(), "the base alone has no source");

    ws.write_file(os_file, "[api]\nrun=serve\n");
    let check = ws.eph_ok(&["check"]).await;
    assert!(check.contains("api (command: serve)"), "got:\n{check}");

    ws.write_file(os_file, "[api]\nrun=serve\nrun=again\n");
    ws.write_file(".eph.local", "[api]\nrun=local\n");
    let output = ws.eph(&["check"]).await;
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        err.contains(&format!(
            ".eph merged with {os_file} must be valid on its own"
        )),
        "got:\n{err}"
    );
}
