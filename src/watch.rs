//! Filesystem watching for `eph dev --watch`.
//!
//! `eph dev` normally foregrounds one `run=` service and stays attached until it
//! is stopped. With one or more `--watch <glob>` patterns it also watches the
//! workspace tree, and when a file the patterns select changes it restarts the
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
//! Patterns are ordered rules, as in `.gitignore`: a leading `!` turns a pattern
//! into an exclusion, and the last pattern that matches a path decides whether it
//! is watched. So `**/*.go` followed by `!**/*_templ.go` watches Go sources but
//! not generated Templ output, and a later positive pattern can re-include a path
//! an earlier `!` excluded. A leading `\!` matches a literal `!`.
//!
//! [`notify`] delivers raw OS events on its own thread; they are filtered against
//! the globs there and the matching relative paths are forwarded over a channel.
//! [`Watch::changed`] then debounces: one editor save tends to emit a burst
//! (a temp file, a rename, a metadata touch), and restarting the whole stack per
//! event would thrash, so it waits for a quiet gap before reporting a change.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
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
/// loop awaits. Only changes to files the configured patterns select are
/// reported; everything else (including git's own churn under `.git`) is dropped
/// before it reaches the caller.
pub struct Watch {
    /// The OS watcher. Kept alive purely for its `Drop`: dropping it tears down
    /// the underlying inotify / FSEvents / ReadDirectoryChangesW registration.
    _watcher: RecommendedWatcher,
    /// Debounced stream of workspace-relative paths that matched the rules.
    rx: mpsc::UnboundedReceiver<PathBuf>,
}

impl Watch {
    /// Start watching `root` for changes to files selected by `patterns`.
    ///
    /// The root is canonicalized so the relative-path matching below lines up
    /// with the absolute paths [`notify`] reports (notably on macOS, where
    /// FSEvents resolves symlinks like `/var` to `/private/var`). The whole tree
    /// is watched recursively; correctness comes from the glob filter, not from
    /// narrowing the watched set.
    ///
    /// # Errors
    ///
    /// Returns an error if the patterns are invalid (see [`Rules::compile`]), the
    /// root cannot be canonicalized, or the OS watch cannot be established.
    pub fn new(root: &Path, patterns: &[String]) -> Result<Self> {
        let rules = Rules::compile(patterns)?;

        // Match against the canonical root so `strip_prefix` succeeds regardless
        // of how the caller spelled the path or how the OS reports events.
        // dunce::canonicalize matches Workspace::from_path: on Windows it avoids
        // the extended-length `\\?\` prefix, so the root here lines up with the
        // stored workspace path and with the paths notify reports.
        let root = dunce::canonicalize(root)
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
                if let Some(rel) = match_relative(&match_root, &path, &rules) {
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

/// The compiled `--watch` patterns: ordered include and exclude rules.
///
/// Every pattern's glob lives in one [`GlobSet`], and `excludes[i]` records
/// whether pattern `i` had a leading `!`. [`GlobSet::matches`] reports the index
/// of every glob that matches a path, so the highest index is the last matching
/// pattern, and its effect (include or exclude) decides the path.
#[derive(Debug)]
struct Rules {
    /// One glob per pattern, in command-line order.
    globs: GlobSet,
    /// Whether the pattern at the same index is an exclusion.
    excludes: Vec<bool>,
}

impl Rules {
    /// Compile `patterns` into ordered rules with gitignore-style separators.
    ///
    /// `literal_separator(true)` is what makes `*` stop at a `/` and `**` span it,
    /// so the patterns behave the way a developer expects from `.gitignore` rather
    /// than the shell's looser default where `*` also crosses directories.
    ///
    /// # Errors
    ///
    /// Rejects an empty pattern, a bare `!`, a malformed glob, and a non-empty
    /// list made only of exclusions: with nothing included there is nothing to
    /// exclude from, so the watch could never fire, which is surely a mistake.
    fn compile(patterns: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut excludes = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let (exclude, glob) = split_negation(pattern)?;
            let glob: Glob = GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid --watch glob: {pattern}"))?;
            builder.add(glob);
            excludes.push(exclude);
        }
        if !excludes.is_empty() && excludes.iter().all(|&exclude| exclude) {
            bail!(
                "every --watch pattern is an exclusion (starts with `!`), so nothing would be \
                 watched; add a pattern that includes files, e.g. --watch '**/*' before the \
                 exclusions"
            );
        }
        let globs = builder.build().context("compiling --watch globs")?;
        Ok(Self { globs, excludes })
    }

    /// Whether a workspace-relative path is watched: the last pattern that
    /// matches it must be an include. A path no pattern matches is not watched.
    fn is_match(&self, rel: &Path) -> bool {
        self.globs
            .matches(rel)
            .into_iter()
            .max()
            .is_some_and(|last| !self.excludes[last])
    }
}

/// Split a `--watch` pattern into its effect and its glob.
///
/// A leading `!` marks an exclusion; a leading `\!` is an escape for a glob that
/// really starts with `!` (as in `.gitignore`). Returns `(is_exclusion, glob)`.
///
/// # Errors
///
/// Rejects an empty pattern and a bare `!`, which have no glob to match.
fn split_negation(pattern: &str) -> Result<(bool, &str)> {
    if pattern.is_empty() {
        bail!("invalid --watch pattern: it is empty");
    }
    if let Some(literal) = pattern
        .strip_prefix('\\')
        .filter(|rest| rest.starts_with('!'))
    {
        return Ok((false, literal));
    }
    match pattern.strip_prefix('!') {
        Some("") => {
            bail!("invalid --watch pattern `!`: it needs a glob to exclude, e.g. '!**/*_test.go'")
        }
        Some(glob) => Ok((true, glob)),
        None => Ok((false, pattern)),
    }
}

/// Match an absolute event `path` against `rules` as a workspace-relative path.
///
/// Returns the relative path when it is watched, or `None` when it is outside
/// the root, lives under `.git` (git's index churn must never trigger a restart),
/// or the rules do not select it. Matching relative to the root is what lets
/// `*.toml` mean "at the repo root" and `**/*.rs` mean "anywhere in the repo".
fn match_relative(root: &Path, path: &Path, rules: &Rules) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return None;
    }
    rules.is_match(rel).then(|| rel.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gitignore-style separator rules the feature promises: `*` stays within
    /// a directory, `**` crosses it, and matching is relative to the root.
    #[test]
    fn globs_match_relative_to_root_with_literal_separators() {
        let rules = Rules::compile(&["**/*.rs".to_string(), "*.toml".to_string()]).unwrap();
        let root = Path::new("/repo");

        // `**/*.rs` matches a `.rs` file at any depth.
        assert!(match_relative(root, Path::new("/repo/src/main.rs"), &rules).is_some());
        assert!(match_relative(root, Path::new("/repo/deep/nested/a.rs"), &rules).is_some());

        // `*.toml` matches only at the root, because `*` does not cross `/`.
        assert!(match_relative(root, Path::new("/repo/Cargo.toml"), &rules).is_some());
        assert!(match_relative(root, Path::new("/repo/crates/x/Cargo.toml"), &rules).is_none());

        // An unmatched extension is dropped.
        assert!(match_relative(root, Path::new("/repo/src/readme.md"), &rules).is_none());
    }

    /// The returned path is workspace-relative, so log lines read from the repo
    /// root rather than dumping an absolute path.
    #[test]
    fn match_returns_the_relative_path() {
        let rules = Rules::compile(&["**/*.rs".to_string()]).unwrap();
        let matched = match_relative(Path::new("/repo"), Path::new("/repo/src/main.rs"), &rules);
        assert_eq!(matched.as_deref(), Some(Path::new("src/main.rs")));
    }

    /// Changes under `.git` must never restart the stack, even if a glob would
    /// otherwise match them (e.g. watching `**/*` while git rewrites its index).
    #[test]
    fn git_internal_changes_are_ignored() {
        let rules = Rules::compile(&["**/*".to_string()]).unwrap();
        let root = Path::new("/repo");
        assert!(match_relative(root, Path::new("/repo/.git/index"), &rules).is_none());
        assert!(match_relative(root, Path::new("/repo/.git/refs/heads/main"), &rules).is_none());
        // A tracked file next to `.git` still matches.
        assert!(match_relative(root, Path::new("/repo/src/main.rs"), &rules).is_some());
    }

    /// A path outside the watched root is ignored rather than mis-matched.
    #[test]
    fn paths_outside_the_root_are_ignored() {
        let rules = Rules::compile(&["**/*".to_string()]).unwrap();
        assert!(match_relative(Path::new("/repo"), Path::new("/other/x.rs"), &rules).is_none());
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
        let err = Rules::compile(&["src/[".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--watch glob"), "got: {err}");
        assert!(err.contains("src/["), "got: {err}");

        // The same holds behind a `!`, and the error names the pattern as written.
        let err = compile_err(&["**/*", "!src/["]);
        assert!(err.contains("--watch glob: !src/["), "got: {err}");
    }

    /// Build rules from string literals.
    fn parse(patterns: &[&str]) -> Rules {
        let patterns: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        Rules::compile(&patterns).unwrap()
    }

    /// Compile string literals and return the error message, failing if they compile.
    fn compile_err(patterns: &[&str]) -> String {
        let patterns: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        match Rules::compile(&patterns) {
            Ok(_) => panic!("expected {patterns:?} to be rejected"),
            Err(err) => format!("{err:#}"),
        }
    }

    /// Regression for #97: a `!` pattern excludes paths an earlier broad pattern
    /// includes, so generated files do not restart the stack.
    #[test]
    fn negated_pattern_excludes_a_match() {
        let rules = parse(&["**/*.go", "**/*.templ", "!**/*_templ.go"]);
        assert!(rules.is_match(Path::new("cmd/main.go")));
        assert!(rules.is_match(Path::new("views/page.templ")));
        assert!(!rules.is_match(Path::new("views/page_templ.go")));
        assert!(!rules.is_match(Path::new("page_templ.go")));
    }

    /// The last matching pattern wins, so a later positive pattern re-includes a
    /// path an earlier `!` excluded, and order is what decides it.
    #[test]
    fn later_positive_pattern_re_includes() {
        let rules = parse(&["**/*.go", "!gen/**", "gen/keep.go"]);
        assert!(rules.is_match(Path::new("main.go")));
        assert!(!rules.is_match(Path::new("gen/api.go")));
        assert!(rules.is_match(Path::new("gen/keep.go")));

        // A leading exclusion has nothing to subtract from yet: the later broad
        // include is the last match, so the path is watched.
        let rules = parse(&["!gen/**", "**/*.go"]);
        assert!(rules.is_match(Path::new("gen/api.go")));
    }

    /// `\!` escapes a glob that really starts with `!`; it is an include.
    #[test]
    fn escaped_bang_is_a_literal_include() {
        let rules = parse(&[r"\!important.txt"]);
        assert!(rules.is_match(Path::new("!important.txt")));
        assert!(!rules.is_match(Path::new("important.txt")));
    }

    /// A list made only of exclusions could never fire, so it is rejected.
    #[test]
    fn all_negative_patterns_are_rejected() {
        let err = compile_err(&["!**/*_templ.go"]);
        assert!(
            err.contains("every --watch pattern is an exclusion"),
            "got: {err}"
        );
        let err = compile_err(&["!a", "!b"]);
        assert!(
            err.contains("every --watch pattern is an exclusion"),
            "got: {err}"
        );
    }

    /// An empty pattern and a bare `!` have no glob, so they are rejected.
    #[test]
    fn empty_and_bare_bang_patterns_are_rejected() {
        let err = compile_err(&["**/*.rs", ""]);
        assert!(err.contains("it is empty"), "got: {err}");
        let err = compile_err(&["**/*.rs", "!"]);
        assert!(err.contains("invalid --watch pattern `!`"), "got: {err}");
    }

    /// End-to-end for #97: a write to an excluded file is filtered out before it
    /// reaches the debounce, so the first reported change is the included file.
    #[tokio::test]
    async fn excluded_change_does_not_wake_the_watch() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("views")).unwrap();

        let patterns = ["**/*.go".to_string(), "!**/*_templ.go".to_string()];
        let mut watch = Watch::new(root, &patterns).unwrap();

        // The generated file is written first, then given time to arrive. Were it
        // not excluded, it would be the change reported below.
        fs::write(root.join("views").join("page_templ.go"), "package views").unwrap();
        let early = tokio::time::timeout(Duration::from_secs(1), watch.changed()).await;
        assert!(
            early.is_err(),
            "an excluded write woke the watch: {early:?}"
        );

        fs::write(root.join("views").join("page.go"), "package views").unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(10), watch.changed())
            .await
            .expect("a matching change should be reported within the timeout")
            .expect("the watcher should still be live");
        assert!(
            changed.ends_with("page.go"),
            "expected the included write to be reported, got: {}",
            changed.display()
        );
    }
}
