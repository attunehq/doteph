//! Read-only git inspection for `eph system prune` and `eph system ls`.
//!
//! A workspace path that still exists tells prune nothing about whether anyone
//! still needs it. When that path is a git checkout, the branch's relationship
//! to the repository's default branch does: a worktree whose commits are all
//! in `main` and whose tree is clean is finished work that the creating tool
//! (an agent harness, an IDE) never removed. This module answers exactly that
//! question, with the local `git` binary and no network access, so the answer
//! is only as fresh as the last fetch.

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// How far back along the default branch to look for a squash commit carrying
/// the worktree's changes. Bounds the `git log -p` cost on long-lived repos;
/// a branch merged further back than this reads as unmerged.
const SQUASH_SEARCH_COMMITS: &str = "1000";

/// How a workspace's checked-out branch relates to its repository's default
/// branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    /// Every commit on the branch is already in the default branch (as an
    /// ancestor, or because merging the branch into the default branch would
    /// change nothing, as after a squash or rebase merge), and the working
    /// tree is clean.
    Merged,
    /// The branch content is merged but the working tree has uncommitted or
    /// untracked changes, so someone may still be working there.
    MergedDirty,
    /// The branch has content the default branch does not, the checkout is
    /// the default branch itself, or the branch sits on the default branch's
    /// tip with no commits of its own.
    Unmerged,
    /// The path is not a git checkout, has no recognizable default branch, or
    /// `git` is unavailable.
    Unknown,
}

impl MergeStatus {
    /// Short label for tabular output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MergeStatus::Merged => "merged",
            MergeStatus::MergedDirty => "merged+dirty",
            MergeStatus::Unmerged => "unmerged",
            MergeStatus::Unknown => "-",
        }
    }
}

/// Run `git` in `cwd` and return trimmed stdout on success, `None` on any
/// failure (non-zero exit, missing binary, spawn error).
async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    git_with_stdin(cwd, args, None).await
}

/// [`git`] with optional bytes written to the child's stdin.
async fn git_with_stdin(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A closed pipe (git exited early) is reported by the exit status.
        let _ = pipe.write_all(input).await;
        drop(pipe);
    }
    let output = child.wait_with_output().await.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Find the ref to compare against: the remote's advertised HEAD when known,
/// otherwise the conventional names, remote first so a stale local `main`
/// does not hide an upstream merge.
async fn default_branch_ref(cwd: &Path) -> Option<String> {
    if let Some(head) = git(cwd, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"]).await
        && !head.is_empty()
    {
        return Some(head);
    }
    for candidate in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ] {
        if git(cwd, &["rev-parse", "-q", "--verify", candidate])
            .await
            .is_some()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Classify the checkout at `path` against its repository's default branch.
///
/// Never fails: any error reads as [`MergeStatus::Unknown`], because prune
/// must keep working on machines without git and on paths that are not
/// repositories.
pub async fn merge_status(path: &Path) -> MergeStatus {
    if git(path, &["rev-parse", "--is-inside-work-tree"])
        .await
        .as_deref()
        != Some("true")
    {
        return MergeStatus::Unknown;
    }
    let Some(target) = default_branch_ref(path).await else {
        return MergeStatus::Unknown;
    };
    // The default branch's own checkout is the integration target, not
    // finished work, however its tip relates to the remote.
    if let Some(branch) = git(path, &["symbolic-ref", "-q", "--short", "HEAD"]).await
        && target.rsplit('/').next() == Some(branch.as_str())
    {
        return MergeStatus::Unmerged;
    }
    // A checkout sitting exactly on the default branch's tip is a worktree
    // that was just created and has no commits yet, far more often than a
    // fast-forward merge the default branch has not moved past. Nothing has
    // been merged from it, so it is not finished work.
    if git(path, &["rev-parse", "HEAD"]).await
        == git(path, &["rev-parse", &format!("{target}^{{commit}}")]).await
    {
        return MergeStatus::Unmerged;
    }

    // A worktree is often behind its own pushed branch: the PR got a merge
    // from main or a review fix-up before it landed, and nobody pulled that
    // back into the checkout. When the local HEAD is an ancestor of the
    // upstream branch, judge the upstream tip, since that is what got merged.
    let tip = match git(path, &["rev-parse", "-q", "--verify", "@{upstream}"]).await {
        Some(upstream)
            if git(path, &["merge-base", "--is-ancestor", "HEAD", &upstream])
                .await
                .is_some() =>
        {
            upstream
        }
        _ => "HEAD".to_string(),
    };

    let merged = git(path, &["merge-base", "--is-ancestor", &tip, &target])
        .await
        .is_some()
        || rebase_merged(path, &target, &tip).await
        || merge_would_be_noop(path, &target, &tip).await
        || squash_merged(path, &target, &tip).await;
    if !merged {
        return MergeStatus::Unmerged;
    }

    match git(path, &["status", "--porcelain"]).await {
        Some(status) if status.is_empty() => MergeStatus::Merged,
        Some(_) => MergeStatus::MergedDirty,
        None => MergeStatus::Unknown,
    }
}

/// Whether every commit on `tip` has a patch-equivalent commit in `target`:
/// the shape of a rebase merge, or of commits cherry-picked one by one.
/// `git cherry` marks such commits with `-`; any `+` is unmerged work.
async fn rebase_merged(path: &Path, target: &str, tip: &str) -> bool {
    git(path, &["cherry", target, tip])
        .await
        .is_some_and(|cherry| {
            !cherry.is_empty() && cherry.lines().all(|line| line.starts_with('-'))
        })
}

/// Whether the whole branch, squashed into one patch, matches a commit on
/// `target` since the branches diverged: the shape of a squash merge. Unlike
/// [`merge_would_be_noop`], this still works after `target` has gone on to
/// change the same files again, which is the common state of a worktree left
/// behind for a few days.
async fn squash_merged(path: &Path, target: &str, tip: &str) -> bool {
    let Some(base) = git(path, &["merge-base", target, tip]).await else {
        return false;
    };
    let Some(branch_diff) = git(path, &["diff", "--no-color", &base, tip]).await else {
        return false;
    };
    if branch_diff.is_empty() {
        return false;
    }
    let Some(branch_id) = patch_ids(path, branch_diff.as_bytes())
        .await
        .into_iter()
        .next()
    else {
        return false;
    };
    let range = format!("{base}..{target}");
    let Some(target_log) = git(
        path,
        &[
            "log",
            "-p",
            "--no-color",
            "--no-merges",
            "--max-count",
            SQUASH_SEARCH_COMMITS,
            &range,
        ],
    )
    .await
    else {
        return false;
    };
    patch_ids(path, target_log.as_bytes())
        .await
        .contains(&branch_id)
}

/// Stable patch IDs for every patch in `patches` (`git log -p` or `git diff`
/// output), in order.
async fn patch_ids(path: &Path, patches: &[u8]) -> Vec<String> {
    git_with_stdin(path, &["patch-id", "--stable"], Some(patches))
        .await
        .map(|output| {
            output
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether merging `tip` into `target` would leave `target`'s tree unchanged.
/// This catches a squash or rebase merge whose patch was edited on the way in
/// (so neither [`rebase_merged`] nor [`squash_merged`] can match it), as long
/// as `target` has not since touched the same lines. `git merge-tree
/// --write-tree` (git 2.38+) does the three-way merge in memory; a conflict,
/// or an older git, reads as "not a no-op".
async fn merge_would_be_noop(path: &Path, target: &str, tip: &str) -> bool {
    let Some(output) = git(path, &["merge-tree", "--write-tree", target, tip]).await else {
        return false;
    };
    let Some(merged_tree) = output.lines().next() else {
        return false;
    };
    git(path, &["rev-parse", &format!("{target}^{{tree}}")])
        .await
        .is_some_and(|target_tree| target_tree == merged_tree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn run(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "Grace Hopper")
            .env("GIT_AUTHOR_EMAIL", "grace@example.com")
            .env("GIT_COMMITTER_NAME", "Grace Hopper")
            .env("GIT_COMMITTER_EMAIL", "grace@example.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .expect("git should spawn");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    async fn commit_file(repo: &Path, name: &str, contents: &str) {
        std::fs::write(repo.join(name), contents).unwrap();
        run(repo, &["add", name]).await;
        run(repo, &["commit", "-q", "-m", name]).await;
    }

    /// A repo on `main` with one commit, plus a worktree on `feature` at
    /// `<tmp>/feature`.
    async fn repo_with_feature_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]).await;
        commit_file(&repo, "base.txt", "base").await;
        let feature = tmp.path().join("feature");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                feature.to_str().unwrap(),
            ],
        )
        .await;
        (tmp, repo, feature)
    }

    #[tokio::test]
    async fn non_repository_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(merge_status(tmp.path()).await, MergeStatus::Unknown);
    }

    #[tokio::test]
    async fn default_branch_checkout_is_unmerged() {
        let (_tmp, repo, _feature) = repo_with_feature_worktree().await;
        assert_eq!(merge_status(&repo).await, MergeStatus::Unmerged);
    }

    #[tokio::test]
    async fn fresh_branch_on_the_default_tip_is_unmerged() {
        let (_tmp, _repo, feature) = repo_with_feature_worktree().await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Unmerged);
    }

    #[tokio::test]
    async fn branch_with_unmerged_commit_is_unmerged() {
        let (_tmp, _repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "feature.txt", "work").await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Unmerged);
    }

    #[tokio::test]
    async fn fast_forward_merged_branch_is_merged_once_main_moves_on() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "feature.txt", "work").await;
        run(&repo, &["merge", "-q", "--ff-only", "feature"]).await;
        // Until main moves, this is indistinguishable from a fresh worktree.
        assert_eq!(merge_status(&feature).await, MergeStatus::Unmerged);
        commit_file(&repo, "later.txt", "later").await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Merged);
    }

    #[tokio::test]
    async fn squash_merged_branch_is_merged() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "feature.txt", "work").await;
        // Squash onto main, then move main on so the feature tip is not an
        // ancestor and only the tree comparison can see the merge.
        run(&repo, &["merge", "-q", "--squash", "feature"]).await;
        run(&repo, &["commit", "-q", "-m", "squash feature"]).await;
        commit_file(&repo, "later.txt", "later").await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Merged);
    }

    #[tokio::test]
    async fn squash_merged_branch_is_merged_after_main_edits_the_same_file() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "base.txt", "feature edit").await;
        commit_file(&feature, "feature.txt", "work").await;
        run(&repo, &["merge", "-q", "--squash", "feature"]).await;
        run(&repo, &["commit", "-q", "-m", "squash feature"]).await;
        // main rewrites the line the branch touched: merge-tree now conflicts,
        // so only the patch-id comparison can recognize the merge.
        commit_file(&repo, "base.txt", "main edit after merge").await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Merged);
    }

    #[tokio::test]
    async fn rebase_merged_branch_is_merged() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "one.txt", "one").await;
        commit_file(&feature, "two.txt", "two").await;
        // Cherry-pick both commits onto main (new hashes, same patches), then
        // move main on with a conflicting edit.
        run(&repo, &["cherry-pick", "feature~1", "feature"]).await;
        commit_file(&repo, "one.txt", "main rewrites one").await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Merged);
    }

    #[tokio::test]
    async fn partially_merged_branch_is_unmerged() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "one.txt", "one").await;
        commit_file(&feature, "two.txt", "two").await;
        run(&repo, &["cherry-pick", "feature~1"]).await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Unmerged);
    }

    /// The worktree stays at its last local commit while the pushed branch
    /// gains a merge from main and is then squash-merged: only the upstream
    /// tip's patch matches the squash commit.
    #[tokio::test]
    async fn worktree_behind_its_merged_upstream_is_merged() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "feature.txt", "work").await;
        // Stand in for the remote: a bare clone of the repo that receives the
        // feature branch, then a merge from main on top of it.
        let remote = _tmp.path().join("remote.git");
        run(
            &repo,
            &["clone", "-q", "--bare", ".", remote.to_str().unwrap()],
        )
        .await;
        run(
            &feature,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .await;
        run(&feature, &["push", "-q", "-u", "origin", "feature"]).await;
        commit_file(&repo, "main.txt", "main moves").await;
        run(&repo, &["push", "-q", remote.to_str().unwrap(), "main"]).await;
        let review = _tmp.path().join("review");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                review.to_str().unwrap(),
                "feature",
            ],
        )
        .await;
        run(&review, &["merge", "-q", "--no-edit", "main"]).await;
        run(
            &review,
            &["push", "-q", remote.to_str().unwrap(), "HEAD:feature"],
        )
        .await;
        // Squash-merge the upstream state of feature into main.
        run(
            &repo,
            &[
                "fetch",
                "-q",
                remote.to_str().unwrap(),
                "feature:refs/remotes/origin/feature",
            ],
        )
        .await;
        run(
            &repo,
            &["merge", "-q", "--squash", "refs/remotes/origin/feature"],
        )
        .await;
        run(&repo, &["commit", "-q", "-m", "squash feature (#1)"]).await;
        // The feature worktree learns about both updates without moving HEAD.
        run(&feature, &["fetch", "-q", "origin"]).await;
        run(
            &feature,
            &[
                "fetch",
                "-q",
                repo.to_str().unwrap(),
                "main:refs/remotes/origin/main",
            ],
        )
        .await;
        assert_eq!(merge_status(&feature).await, MergeStatus::Merged);
    }

    #[tokio::test]
    async fn merged_branch_with_uncommitted_changes_is_dirty() {
        let (_tmp, repo, feature) = repo_with_feature_worktree().await;
        commit_file(&feature, "feature.txt", "work").await;
        run(&repo, &["merge", "-q", "--ff-only", "feature"]).await;
        commit_file(&repo, "later.txt", "later").await;
        std::fs::write(feature.join("scratch.txt"), "wip").unwrap();
        assert_eq!(merge_status(&feature).await, MergeStatus::MergedDirty);
    }
}
