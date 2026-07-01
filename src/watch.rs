//! Filesystem watching for `eph dev --watch`.
//!
//! `eph dev` normally foregrounds one `run=` service and stays attached until it
//! is stopped. With one or more `--watch <glob>` patterns it also watches the
//! workspace tree, and when a file matching any glob changes it restarts the
//! whole dev stack (see [`crate`]'s `cmd_dev`). This module owns just the watch
//! half of that: compile the globs, watch the workspace root, and hand the caller
//! a single debounced "something changed" signal it can drive from `tokio::select!`.
//!
//! The globs use gitignore-style semantics via [`globset`] with
//! `literal_separator` on, so `*` stops at a path separator and `**` spans them:
//! `*.toml` matches a top-level `Cargo.toml` but not `crates/x/Cargo.toml`, while
//! `**/*.rs` matches a `.rs` file at any depth. Patterns are matched against the
//! path relative to the workspace root, so they read the way a developer writes
//! them from the repo root.
//!
//! [`notify`] delivers raw OS events on its own thread; they are filtered against
//! the globs there and the matching relative paths are forwarded over a channel.
//! [`Watch::changed`] then debounces: one editor save tends to emit a burst
//! (a temp file, a rename, a metadata touch), and restarting the whole stack per
//! event would thrash, so it waits for a quiet gap before reporting a change.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// How long the tree must be quiet after a change before it is reported.
///
/// One save often lands as several OS events (write, rename, chmod); waiting for
/// a short gap collapses that burst into a single restart instead of several.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// A live watch over the workspace tree for `eph dev --watch`.
///
/// Holds the OS watcher open for its whole lifetime (dropping it ends the watch)
/// and exposes [`changed`](Self::changed) as the single, debounced signal the dev
/// loop awaits. Only changes to files matching one of the configured globs are
/// reported; everything else (including git's own churn under `.git`) is dropped
/// before it reaches the caller.
pub struct Watch {
    /// The OS watcher. Kept alive purely for its `Drop`: dropping it tears down
    /// the underlying inotify / FSEvents / ReadDirectoryChangesW registration.
    _watcher: RecommendedWatcher,
    /// Debounced stream of workspace-relative paths that matched a glob.
    rx: mpsc::UnboundedReceiver<PathBuf>,
}

impl Watch {
    /// Start watching `root` for changes to files matching any of `patterns`.
    ///
    /// The root is canonicalized so the relative-path matching below lines up
    /// with the absolute paths [`notify`] reports (notably on macOS, where
    /// FSEvents resolves symlinks like `/var` to `/private/var`). The whole tree
    /// is watched recursively; correctness comes from the glob filter, not from
    /// narrowing the watched set.
    ///
    /// # Errors
    ///
    /// Returns an error if a glob is malformed, the root cannot be canonicalized,
    /// or the OS watch cannot be established.
    pub fn new(root: &Path, patterns: &[String]) -> Result<Self> {
        let globs = compile(patterns)?;

        // Match against the canonical root so `strip_prefix` succeeds regardless
        // of how the caller spelled the path or how the OS reports events.
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving watch root {}", root.display()))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let match_root = root.clone();

        // The closure runs on notify's own thread. It does the glob filtering
        // there so only relevant paths ever cross the channel, and forwards them
        // best-effort: a closed receiver just means the dev loop has moved on.
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            // Ignore pure access/open events; only creation, content or metadata
            // changes, renames, and removals should trigger a restart.
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for path in event.paths {
                if let Some(rel) = match_relative(&match_root, &path, &globs) {
                    let _ = tx.send(rel);
                }
            }
        })
        .context("initializing the filesystem watcher")?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Resolve when a matching file has changed and the tree has gone quiet.
    ///
    /// Blocks until the first matching change, then keeps draining events until
    /// [`DEBOUNCE`] passes with no further change, so one save maps to one
    /// restart. Returns the first path that matched (for a human-readable log
    /// line), or `None` if the watcher has shut down and no more changes will
    /// ever arrive.
    pub async fn changed(&mut self) -> Option<PathBuf> {
        let first = self.rx.recv().await?;
        // Collapse the trailing burst: keep swallowing events until the tree is
        // quiet for a full debounce window.
        loop {
            match tokio::time::timeout(DEBOUNCE, self.rx.recv()).await {
                // More churn within the window: reset and keep waiting.
                Ok(Some(_)) => continue,
                // Quiet window elapsed, or the sender is gone: report the change.
                Err(_) | Ok(None) => return Some(first),
            }
        }
    }

    /// Like [`changed`](Self::changed), but parks forever instead of resolving
    /// when the watcher has shut down. Handy in a `tokio::select!` arm that must
    /// only ever fire on a real change, never on a closed channel: a dead watcher
    /// stays pending rather than resolving and spuriously restarting the stack.
    pub async fn changed_or_pending(&mut self) -> PathBuf {
        match self.changed().await {
            Some(path) => path,
            None => std::future::pending().await,
        }
    }
}

/// Compile `patterns` into a [`GlobSet`] with gitignore-style separator handling.
///
/// `literal_separator(true)` is what makes `*` stop at a `/` and `**` span it, so
/// the patterns behave the way a developer expects from `.gitignore` rather than
/// the shell's looser default where `*` also crosses directories.
fn compile(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob: Glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid --watch glob: {pattern}"))?;
        builder.add(glob);
    }
    builder.build().context("compiling --watch globs")
}

/// Match an absolute event `path` against `globs` as a workspace-relative path.
///
/// Returns the relative path when it matches, or `None` when it is outside the
/// root, lives under `.git` (git's index churn must never trigger a restart), or
/// simply matches no glob. Matching relative to the root is what lets `*.toml`
/// mean "at the repo root" and `**/*.rs` mean "anywhere in the repo".
fn match_relative(root: &Path, path: &Path, globs: &GlobSet) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return None;
    }
    globs.is_match(rel).then(|| rel.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gitignore-style separator rules the feature promises: `*` stays within
    /// a directory, `**` crosses it, and matching is relative to the root.
    #[test]
    fn globs_match_relative_to_root_with_literal_separators() {
        let globs = compile(&["**/*.rs".to_string(), "*.toml".to_string()]).unwrap();
        let root = Path::new("/repo");

        // `**/*.rs` matches a `.rs` file at any depth.
        assert!(match_relative(root, Path::new("/repo/src/main.rs"), &globs).is_some());
        assert!(match_relative(root, Path::new("/repo/deep/nested/a.rs"), &globs).is_some());

        // `*.toml` matches only at the root, because `*` does not cross `/`.
        assert!(match_relative(root, Path::new("/repo/Cargo.toml"), &globs).is_some());
        assert!(match_relative(root, Path::new("/repo/crates/x/Cargo.toml"), &globs).is_none());

        // An unmatched extension is dropped.
        assert!(match_relative(root, Path::new("/repo/src/readme.md"), &globs).is_none());
    }

    /// The returned path is workspace-relative, so log lines read from the repo
    /// root rather than dumping an absolute path.
    #[test]
    fn match_returns_the_relative_path() {
        let globs = compile(&["**/*.rs".to_string()]).unwrap();
        let matched = match_relative(Path::new("/repo"), Path::new("/repo/src/main.rs"), &globs);
        assert_eq!(matched.as_deref(), Some(Path::new("src/main.rs")));
    }

    /// Changes under `.git` must never restart the stack, even if a glob would
    /// otherwise match them (e.g. watching `**/*` while git rewrites its index).
    #[test]
    fn git_internal_changes_are_ignored() {
        let globs = compile(&["**/*".to_string()]).unwrap();
        let root = Path::new("/repo");
        assert!(match_relative(root, Path::new("/repo/.git/index"), &globs).is_none());
        assert!(match_relative(root, Path::new("/repo/.git/refs/heads/main"), &globs).is_none());
        // A tracked file next to `.git` still matches.
        assert!(match_relative(root, Path::new("/repo/src/main.rs"), &globs).is_some());
    }

    /// A path outside the watched root is ignored rather than mis-matched.
    #[test]
    fn paths_outside_the_root_are_ignored() {
        let globs = compile(&["**/*".to_string()]).unwrap();
        assert!(match_relative(Path::new("/repo"), Path::new("/other/x.rs"), &globs).is_none());
    }

    /// End-to-end: a real write under a real watched tree drives the OS watcher,
    /// the glob filter, the channel, and the debounce, and surfaces as one change.
    /// A non-matching sibling write must not be what wakes it.
    ///
    /// The nested `src/` directory is created *before* the watch starts on
    /// purpose: Linux's inotify does not automatically cover a subdirectory
    /// created after the recursive watch is established (the backend has to catch
    /// the subdir's own creation event and add a watch, which races a quick write
    /// into it). Watching a pre-existing subdir exercises recursive matching
    /// without that race, which is a test artifact rather than a real-usage one:
    /// `eph dev` runs for a long time, so any new subdir is watched well before a
    /// later edit lands in it.
    #[tokio::test]
    async fn reports_a_matching_change_end_to_end() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();

        let mut watch = Watch::new(root, &["**/*.rs".to_string()]).unwrap();

        // A non-matching file must not be what wakes the watcher; a matching one,
        // nested a directory down to exercise recursive matching, must.
        fs::write(root.join("notes.md"), "hi").unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();

        let changed = tokio::time::timeout(Duration::from_secs(10), watch.changed())
            .await
            .expect("a matching change should be reported within the timeout")
            .expect("the watcher should still be live");
        assert!(
            changed.ends_with("main.rs"),
            "expected the .rs write to be reported, got: {}",
            changed.display()
        );
    }

    /// A malformed glob is a startup error, surfaced with the offending pattern.
    #[test]
    fn invalid_glob_is_rejected() {
        // An unclosed character class is a genuine glob syntax error.
        let err = compile(&["src/[".to_string()]).unwrap_err().to_string();
        assert!(err.contains("--watch glob"), "got: {err}");
        assert!(err.contains("src/["), "got: {err}");
    }
}
