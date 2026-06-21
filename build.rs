//! Build script for `eph` that derives the version string baked into the binary.
//!
//! Resolution order (first hit wins):
//!   1. The `EPH_VERSION` environment variable, if set and non-empty. CI injects
//!      the exact release tag here so the version is correct even inside `cross`
//!      Docker images that may not ship `git` or the `.git` directory.
//!   2. `git describe --always --tags --dirty=-dirty`. When the working tree has
//!      uncommitted changes, a short content hash of the changed files is
//!      appended so dev builds get a distinct version.
//!   3. `v{CARGO_PKG_VERSION}` as a last resort, so building from a source
//!      tarball with no `.git` (e.g. a crates.io install) still succeeds.
//!
//! The result is exported as `EPH_VERSION` for `env!("EPH_VERSION")` to read in
//! `src/main.rs`. No `rerun-if-*` directives are emitted on purpose: that makes
//! Cargo rerun this script whenever any file in the package changes, which keeps
//! the dirty-tree hash fresh during local development.

use std::fs;
use std::hash::{DefaultHasher, Hasher as _};
use std::iter;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

fn main() {
    // Declare EPH_VERSION as a build-script input. CI injects the release tag
    // here, and that value differs between a dry run (a `git describe` string)
    // and the tag build (the bare tag) of the SAME commit. The release matrix
    // shares a Rust cache keyed only on target, so without this directive Cargo
    // could restore the dry run's cached build-script output for the tag build,
    // never re-run this script, and bake the stale version into the released
    // binary. Declaring the env var forces a re-run whenever its value changes.
    println!("cargo:rerun-if-env-changed=EPH_VERSION");
    // Keep the dirty-tree version fresh in local dev by re-running when tracked
    // sources change. (Emitting any rerun-if line opts out of Cargo's implicit
    // "re-run on any package change", so list the inputs that feed the version.)
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=build.rs");

    let version = env_override()
        .or_else(|| compute_version().ok())
        .unwrap_or_else(fallback_version);

    println!("cargo:rustc-env=EPH_VERSION={version}");
}

/// Use `EPH_VERSION` verbatim when CI (or a developer) sets it to a non-empty
/// value.
fn env_override() -> Option<String> {
    std::env::var("EPH_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `v{CARGO_PKG_VERSION}`, used when git is unavailable. `CARGO_PKG_VERSION` is
/// always present in a build script's environment.
fn fallback_version() -> String {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    format!("v{pkg}")
}

fn compute_version() -> Result<String, String> {
    let base_version = git_describe()?;
    let changed_files = changed_files()?;

    if changed_files.is_empty() {
        return Ok(base_version);
    }

    let content_hash = content_hash(changed_files)?;
    let short_hash = &content_hash[..7.min(content_hash.len())];

    Ok(format!("{base_version}-{short_hash}"))
}

fn content_hash(mut files: Vec<StatusEntry>) -> Result<String, String> {
    files.sort();
    files.dedup();

    let repo_root = repo_root()?;

    let mut hashes = Vec::new();
    for file in files {
        let path = Path::new(&repo_root).join(file.path);
        let mut hasher = DefaultHasher::new();
        if let Ok(content) = fs::read(&path) {
            hasher.write(path.as_os_str().as_encoded_bytes());
            hasher.write(&content);
            let hash = hasher.finish();
            hashes.push(hash);
        }
    }
    hashes.sort();
    hashes.dedup();

    let mut hasher = DefaultHasher::new();
    for hash in hashes {
        hasher.write_u64(hash);
    }
    let final_hash = hasher.finish();

    Ok(format!("{final_hash:x}"))
}

fn run(prog: &str, argv: &[&str]) -> Result<String, String> {
    let invocation = iter::once(prog)
        .chain(argv.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");

    let output = Command::new(prog)
        .args(argv)
        .output()
        .map_err(|e| format!("failed to execute `{invocation}`: {e}"))?;
    if !output.status.success() {
        return Err(format!("`{invocation}` exited with non-zero status"));
    }

    let output = String::from_utf8(output.stdout)
        .map_err(|e| format!("could not parse output of `{invocation}` as UTF-8: {e}"))?;
    Ok(output.trim_end().to_string())
}

fn git_describe() -> Result<String, String> {
    run("git", &["describe", "--always", "--tags", "--dirty=-dirty"])
}

fn repo_root() -> Result<String, String> {
    run("git", &["rev-parse", "--show-toplevel"])
}

fn changed_files() -> Result<Vec<StatusEntry>, String> {
    let output = run("git", &["status", "--porcelain"])?;

    let mut files = Vec::new();
    for line in output.lines() {
        files.push(line.parse::<StatusEntry>()?);
    }

    Ok(files)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GitFileStatus {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
    Ignored,
}

impl GitFileStatus {
    fn parse(c: char) -> Option<Self> {
        match c {
            ' ' => Some(Self::Unmodified),
            'M' => Some(Self::Modified),
            'A' => Some(Self::Added),
            'D' => Some(Self::Deleted),
            'R' => Some(Self::Renamed),
            'C' => Some(Self::Copied),
            'U' => Some(Self::Unmerged),
            '?' => Some(Self::Untracked),
            '!' => Some(Self::Ignored),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StatusEntry {
    index: GitFileStatus,
    worktree: GitFileStatus,
    path: String,
    orig_path: Option<String>,
}

impl FromStr for StatusEntry {
    type Err = String;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        if line.len() < 4 {
            return Err("line too short".into());
        }

        let mut chars = line.chars();
        let index_char = chars
            .next()
            .expect("git status line should have index char");
        let worktree_char = chars
            .next()
            .expect("git status line should have worktree char");
        let space = chars
            .next()
            .expect("git status line should have space separator");

        if space != ' ' {
            return Err("expected space after status".into());
        }

        let index = GitFileStatus::parse(index_char)
            .ok_or_else(|| format!("invalid index status: {index_char}"))?;
        let worktree = GitFileStatus::parse(worktree_char)
            .ok_or_else(|| format!("invalid worktree status: {worktree_char}"))?;

        let rest = chars.collect::<String>();
        let (path, orig_path) = if matches!(index, GitFileStatus::Renamed | GitFileStatus::Copied) {
            if let Some((old, new)) = rest.split_once(" -> ") {
                (new.to_string(), Some(old.to_string()))
            } else {
                (rest, None)
            }
        } else {
            (rest, None)
        };

        Ok(StatusEntry {
            index,
            worktree,
            path,
            orig_path,
        })
    }
}
