//! Self-update for the `eph` binary from GitHub Releases.
//!
//! `eph update` is a native updater with no dependency on a shell or `curl`: it
//! resolves the latest published release through the GitHub API, downloads the
//! release archive that matches this binary's build target, verifies it against
//! the release `checksums.txt`, extracts the `eph` binary, and atomically swaps
//! it over the running executable. It mirrors what `scripts/install.sh` and
//! `scripts/install.ps1` do, so a script-installed user and a self-updated user
//! converge on the same bits (down to the same SHA-256 check).
//!
//! The tedious, platform-specific pieces lean on three focused crates: [`ureq`]
//! for HTTPS (rustls, so no system TLS), `flate2` + `tar` for the archive, and
//! [`self_replace`] for the in-place binary swap (an atomic rename on Unix, the
//! move-aside dance on Windows where a running `.exe` cannot be overwritten). The
//! GitHub resolution, asset naming, and checksum verification live here so the
//! integrity guarantee the install scripts provide is preserved.
//!
//! This module also drives the passive out-of-date nag ([`warn_if_outdated`])
//! that every other command runs at startup: it reads a cached latest-release
//! lookup to decide whether to warn, and refreshes that cache in a detached
//! background process ([`run_check_worker`]), so the check never blocks or fails
//! the command the user actually ran.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The GitHub `owner/repo` releases are pulled from. `EPH_REPO` overrides it,
/// matching the `REPO` the install scripts honor.
pub const DEFAULT_REPO: &str = "attunehq/doteph";

/// The target triple this binary was built for, baked by `build.rs`. It is the
/// exact infix of the release asset name (`eph-<target>.tar.gz`), so the updater
/// downloads the same build variant, crucially the musl vs gnu Linux split that
/// runtime OS/arch detection cannot distinguish.
pub const TARGET: &str = env!("EPH_TARGET");

/// User-Agent sent on every request. The GitHub API rejects requests without
/// one, and a descriptive value makes the traffic identifiable in logs.
const USER_AGENT: &str = "eph-selfupdate";

/// Bound on metadata reads (the release JSON and `checksums.txt`). Both are tiny;
/// this only guards against a misbehaving endpoint streaming without end.
const MAX_METADATA: u64 = 4 << 20; // 4 MiB

/// Bound on the archive download and extraction, a defensive sanity cap well
/// above any real `eph` release.
const MAX_ARCHIVE: u64 = 512 << 20; // 512 MiB

/// Where the running version stands relative to the latest published release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The running version is a clean release at or ahead of the latest, so no
    /// update is needed (a local build one patch ahead is not told to downgrade).
    UpToDate,
    /// The running version is a clean release older than the latest.
    UpdateAvailable,
    /// The running version is not a comparable release: a `git describe` dev
    /// build (`v0.2.0-3-gabc1234`, `-dirty`), a prerelease, or an unparseable
    /// string. `eph update` reinstalls the latest release in this case, matching
    /// what re-running the install script would do.
    Development,
}

/// Compare the running `current` version against the `latest` release tag.
///
/// Either may carry a leading `v`. `current` counts as comparable only when it is
/// a clean `X.Y.Z` release (empty semver prerelease and build metadata); a dev
/// build or prerelease resolves to [`Status::Development`] so it is always offered
/// the update rather than being compared with a misleading result.
pub fn status(current: &str, latest: &str) -> Status {
    let (Some(cur), Some(lat)) = (parse_release(current), parse_release(latest)) else {
        return Status::Development;
    };
    if cur >= lat {
        Status::UpToDate
    } else {
        Status::UpdateAvailable
    }
}

/// Parse a `vX.Y.Z` release tag into a [`Version`], returning `None` for anything
/// that is not a clean release: a prerelease, build metadata, or a `git describe`
/// dev-build string that semver cannot parse. The leading `v` is optional.
fn parse_release(tag: &str) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

/// The binary's file name inside the release archive for this platform. The
/// running binary was built for [`TARGET`], so the host it runs on matches, and a
/// compile-time `cfg` is exactly right.
pub(crate) fn binary_name() -> &'static str {
    if cfg!(windows) { "eph.exe" } else { "eph" }
}

/// Resolves and downloads `eph` release assets from GitHub.
pub struct Updater {
    /// The `owner/repo` to download from.
    repo: String,
    /// Base URL for the GitHub API. Overridable for tests.
    api_base: String,
    /// When set (via `EPH_BASE_URL`), overrides the per-tag GitHub download URL
    /// for the archive and `checksums.txt`. It mirrors the install scripts'
    /// base-URL override and lets tests point at a local server.
    download_base: Option<String>,
    /// Shared HTTPS agent with a global timeout, reused across the handful of
    /// requests a single update makes.
    agent: ureq::Agent,
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

impl Updater {
    /// Build an updater honoring the `EPH_REPO` and `EPH_BASE_URL` overrides the
    /// install scripts also respect.
    pub fn new() -> Self {
        Self::with_endpoints(
            effective_repo(),
            "https://api.github.com".to_string(),
            env_nonempty("EPH_BASE_URL"),
        )
    }

    /// Build an updater with explicit endpoints. Tests use this to point at a
    /// local server; [`Updater::new`] wraps it with the GitHub defaults.
    fn with_endpoints(repo: String, api_base: String, download_base: Option<String>) -> Self {
        // A global timeout keeps a stalled network from wedging `eph update`: the
        // whole operation is a few small requests plus one archive download.
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        Self {
            repo,
            api_base,
            download_base,
            agent: config.into(),
        }
    }

    /// Resolve the tag of the latest published release, the same release the
    /// install scripts resolve through the GitHub API. GitHub's `releases/latest`
    /// excludes prereleases, so this is always a stable `vX.Y.Z` tag.
    pub fn latest_tag(&self) -> Result<String> {
        let url = format!("{}/repos/{}/releases/latest", self.api_base, self.repo);
        let body = self
            .get_bytes(&url, Some("application/vnd.github+json"))
            .with_context(|| format!("resolve the latest release for {}", self.repo))?;

        #[derive(serde::Deserialize)]
        struct Release {
            tag_name: String,
        }
        let release: Release =
            serde_json::from_slice(&body).context("parse the latest-release response")?;
        if release.tag_name.is_empty() {
            bail!("no published release found for {}", self.repo);
        }
        Ok(release.tag_name)
    }

    /// The release archive filename for this build target, matching the name the
    /// release workflow produces (`eph-<target>.tar.gz`).
    pub fn asset_name(&self) -> String {
        format!("eph-{TARGET}.tar.gz")
    }

    /// The base URL holding the archive and `checksums.txt` for `tag`.
    fn download_base(&self, tag: &str) -> String {
        match &self.download_base {
            Some(base) => base.trim_end_matches('/').to_string(),
            None => format!("https://github.com/{}/releases/download/{}", self.repo, tag),
        }
    }

    /// Download the release archive for `tag`, verify it against the release
    /// `checksums.txt`, and extract the `eph` binary to `dest` (0755 on Unix).
    ///
    /// The running binary is left untouched; the caller installs the extracted
    /// file with [`replace_running_exe`]. The download is streamed to disk and
    /// checksum-verified before a single byte is extracted, so a corrupted or
    /// tampered archive never reaches the swap.
    pub fn fetch(&self, tag: &str, dest: &Path) -> Result<()> {
        let asset = self.asset_name();
        let base = self.download_base(tag);

        let sums = self
            .get_bytes(&format!("{base}/checksums.txt"), None)
            .context("download checksums.txt")?;
        let want = checksum_for(&sums, &asset)?;

        // Stream the archive to a temp file (it can exceed ureq's in-memory read
        // cap), hashing as it lands, then verify before extracting anything.
        let mut archive =
            tempfile::NamedTempFile::new().context("create a temp file for the archive")?;
        let got = self
            .download_hashed(&format!("{base}/{asset}"), archive.as_file_mut())
            .with_context(|| format!("download {asset}"))?;
        if got != want {
            bail!("checksum mismatch for {asset} (expected {want}, got {got})");
        }

        // Rewind the file we just wrote and read it back for extraction, rather
        // than reopening it by path (which races self-deletion and Windows file
        // sharing).
        archive
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .context("rewind the downloaded archive")?;
        extract_binary(archive.as_file_mut(), dest)
    }

    /// GET `url` and return the response body, bounded by [`MAX_METADATA`]. Used
    /// for the small metadata reads (release JSON, `checksums.txt`).
    fn get_bytes(&self, url: &str, accept: Option<&str>) -> Result<Vec<u8>> {
        let mut request = self.agent.get(url).header("User-Agent", USER_AGENT);
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }
        let mut response = request.call().map_err(|e| anyhow!("GET {url}: {e}"))?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_METADATA)
            .read_to_vec()
            .with_context(|| format!("read the response from {url}"))
    }

    /// Stream `url` to `out`, returning the hex-encoded SHA-256 of everything
    /// written, so the caller can verify it against the published checksum.
    fn download_hashed(&self, url: &str, out: &mut File) -> Result<String> {
        let response = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| anyhow!("GET {url}: {e}"))?;
        let mut reader = response.into_body().into_reader().take(MAX_ARCHIVE);

        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).context("read the download stream")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .context("write the archive to disk")?;
        }
        out.flush().context("flush the downloaded archive")?;
        Ok(hex::encode(hasher.finalize()))
    }
}

/// Install `new_binary` over the currently running executable.
///
/// Delegates to [`self_replace`], which owns the platform split: on Unix an
/// atomic rename swaps the file while the live process keeps its already-open
/// image mapped; on Windows, where a running `.exe` cannot be overwritten, it
/// moves the image aside and cleans it up once the process exits. `new_binary`
/// may live in any directory; `self_replace` stages its own adjacent temp for the
/// swap, so a cross-filesystem source is fine.
pub fn replace_running_exe(new_binary: &Path) -> Result<()> {
    self_replace::self_replace(new_binary).with_context(|| {
        format!(
            "replace the running executable with {}",
            new_binary.display()
        )
    })
}

/// The hex SHA-256 listed for `asset` in a `checksums.txt` body.
///
/// The file has one `<hex>  <name>` line per asset. The release workflow runs
/// `sha256sum ./*.tar.gz`, which writes each name with a leading `./`, so match
/// on the trailing file name rather than the raw field.
fn checksum_for(sums: &[u8], asset: &str) -> Result<String> {
    let text = std::str::from_utf8(sums).context("checksums.txt was not valid UTF-8")?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(hex), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name.trim_start_matches("./") == asset {
            return Ok(hex.to_string());
        }
    }
    bail!("no checksum for {asset} in checksums.txt")
}

/// Extract the `eph` binary from a release `tar.gz` (read from `archive`) to
/// `dest`.
///
/// The archive holds an `eph-<target>/` directory with the binary (`eph` or
/// `eph.exe`) alongside README/LICENSE/NOTICE, so match by base name rather than
/// a fixed path, keeping extraction robust to a layout change.
fn extract_binary(archive: impl Read, dest: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let wanted = binary_name();
    for entry in tar.entries().context("read the archive")? {
        let mut entry = entry.context("read an archive entry")?;
        let path = entry.path().context("read an archive entry path")?;
        if path.file_name().and_then(|n| n.to_str()) == Some(wanted) {
            return write_binary(&mut entry, dest)
                .with_context(|| format!("extract {wanted} to {}", dest.display()));
        }
    }
    bail!("the archive did not contain {wanted}")
}

/// Write `reader` to `dest` as an executable, replacing any existing file. `dest`
/// is a throwaway staging path; [`replace_running_exe`] installs it over the live
/// binary.
fn write_binary(reader: &mut impl Read, dest: &Path) -> Result<()> {
    let mut out = File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    io::copy(&mut reader.take(MAX_ARCHIVE), &mut out).context("write the extracted binary")?;
    out.flush().context("flush the extracted binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .context("mark the extracted binary executable")?;
    }
    Ok(())
}

/// Read an environment variable, returning `None` when it is unset or empty (an
/// empty override should not shadow the default).
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The `owner/repo` eph's network calls target, honoring `EPH_REPO`.
///
/// Shared by [`Updater::new`], the passive update-check nag, and the
/// background refresh worker, so all three agree on which repo's releases
/// they mean, and therefore, via [`cache_path`], on which cache file to
/// read and write. Before this was centralized, `eph update` and the passive
/// nag each independently read `EPH_REPO` in different modules; a single
/// shared source keeps that from drifting apart.
fn effective_repo() -> String {
    env_nonempty("EPH_REPO").unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// How long a cached latest-release lookup stays fresh before the startup check
/// refreshes it in the background. A day keeps the nag current without checking
/// on every invocation.
const CHECK_TTL_SECS: u64 = 24 * 60 * 60;

/// The latest release the last background refresh observed, cached so the startup
/// check can decide whether to nag without touching the network on the hot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCheck {
    /// The latest release tag seen by the most recent successful refresh.
    latest: String,
    /// Unix seconds when that refresh ran, for TTL-based staleness.
    checked_at: u64,
}

/// Warn on stderr when a newer release is available, and refresh the cached
/// latest version in a detached background process for next time.
///
/// This is the passive counterpart to `eph update`: every other command calls it
/// at startup so a user on an old build is nudged to upgrade. It is deliberately
/// cheap and non-blocking: the decision to warn reads only a small on-disk cache,
/// and the network refresh runs in a spawned-and-forgotten process (see
/// [`spawn_background_refresh`]), so it never adds latency or a failure mode to
/// the command the user actually ran.
///
/// It stays silent unless all of these hold, so it never interferes with scripts,
/// pipes, CI, or `eval "$(eph env)"`: the running binary is a tagged release (a
/// `git describe` dev build has no release to compare against), stderr is a
/// terminal, and `EPH_NO_UPDATE_CHECK` is unset.
pub fn warn_if_outdated(current: &str) {
    // A source build has no clean release to compare against, so skip the check
    // and its background refresh entirely: developers building from a checkout
    // should never be nagged or have a worker spawned on their behalf.
    if parse_release(current).is_none() {
        return;
    }
    if env_nonempty("EPH_NO_UPDATE_CHECK").is_some() {
        return;
    }
    // The nag is for interactive use. Staying silent when stderr is redirected
    // keeps automation output clean and, just as importantly, avoids spawning a
    // background worker on every scripted invocation.
    if !io::stderr().is_terminal() {
        return;
    }

    let repo = effective_repo();
    let cache = read_cache(&repo);

    // Warn from the last known latest release, before kicking off the refresh
    // that updates it for next time.
    if let Some(cache) = &cache
        && let Some(message) = outdated_warning(current, &cache.latest)
    {
        eprintln!("{message}");
    }

    // Refresh when the cache is missing or past its TTL. Record the attempt first
    // (a reserve write) so a burst of commands, or a run of offline invocations
    // whose worker never succeeds, backs off for a full TTL instead of respawning
    // a worker every time.
    let now = now_unix();
    let stale = cache.as_ref().is_none_or(|c| is_stale(c.checked_at, now));
    if stale {
        let latest = cache.map_or_else(|| current.to_string(), |c| c.latest);
        let _ = write_cache(
            &repo,
            &CachedCheck {
                latest,
                checked_at: now,
            },
        );
        spawn_background_refresh();
    }
}

/// The nag to print when `current` is a released build behind `latest`, or `None`
/// when it is up to date, ahead, or not a comparable release.
fn outdated_warning(current: &str, latest: &str) -> Option<String> {
    match status(current, latest) {
        Status::UpdateAvailable => Some(format!(
            "A new eph release is available: {latest} (you have {current}).\n\
             Run `eph update` to upgrade, or set EPH_NO_UPDATE_CHECK=1 to silence this."
        )),
        Status::UpToDate | Status::Development => None,
    }
}

/// The refresh body run by the detached `eph __update-check` worker: resolve the
/// latest release and rewrite the cache. Silent and best-effort, so a failure
/// (offline, rate-limited) just leaves the previous cache to be retried after the
/// TTL.
pub fn run_check_worker() {
    let Ok(latest) = Updater::new().latest_tag() else {
        return;
    };
    let _ = write_cache(
        &effective_repo(),
        &CachedCheck {
            latest,
            checked_at: now_unix(),
        },
    );
}

/// Spawn a detached `eph __update-check` process that refreshes the cache and
/// exits.
///
/// Detaching (rather than a background thread) is what lets the refresh finish
/// even when the current command exits immediately: a fast command like `eph env`
/// would otherwise tear the thread down mid-request and never update the cache.
/// The worker reports back only by writing the cache the next run reads, so its
/// stdio is discarded and its handle dropped without waiting.
fn spawn_background_refresh() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .arg("__update-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS drops the worker's tie to this command's console (so it
        // outlives it), and CREATE_NO_WINDOW keeps it from flashing a window.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    // Best effort: never wait on it (fully detached), and ignore a spawn failure.
    let _ = command.spawn();
}

/// The path of the cross-workspace update-check cache, under the user's cache
/// directory. `None` when no cache directory can be resolved (a headless or
/// misconfigured environment), which simply disables the passive check.
///
/// Namespaced by `repo` (see [`sanitize_repo_for_filename`]) so a fork's
/// release cache can never poison the default repo's nag, or vice versa: a
/// user who points `EPH_REPO` at their own fork sees that fork's releases
/// cached separately from `attunehq/doteph`'s.
fn cache_path(repo: &str) -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("eph").join(format!(
        "update-check-{}.json",
        sanitize_repo_for_filename(repo)
    )))
}

/// Sanitize an `owner/repo` string into a filesystem-safe filename component.
///
/// Anything outside `[A-Za-z0-9._-]` (crucially the `/` between owner and
/// repo) becomes `-`, so `attunehq/doteph` becomes `attunehq-doteph` and the
/// result is always a single valid path component on every platform eph runs
/// on, never a directory separator or a reserved character.
fn sanitize_repo_for_filename(repo: &str) -> String {
    repo.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Read the cached latest-release lookup for `repo`, or `None` when it is
/// absent or unreadable (a corrupt or older-format cache is treated as
/// missing and refreshed).
fn read_cache(repo: &str) -> Option<CachedCheck> {
    let bytes = std::fs::read(cache_path(repo)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write `repo`'s cache atomically (temp file then rename) so a concurrent
/// reader never sees a half-written file. A missing cache directory or write
/// error is returned for the caller to ignore: the passive check is
/// best-effort.
fn write_cache(repo: &str, cache: &CachedCheck) -> io::Result<()> {
    let Some(path) = cache_path(repo) else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(cache).map_err(io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

/// Whether a cache entry stamped at `checked_at` is older than the refresh TTL as
/// of `now` (both Unix seconds). Saturating so a clock that moved backward reads
/// as fresh rather than panicking.
fn is_stale(checked_at: u64, now: u64) -> bool {
    now.saturating_sub(checked_at) >= CHECK_TTL_SECS
}

/// The current time in Unix seconds, or 0 if the clock is before the epoch (which
/// only makes a cache entry read as stale, the safe direction).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    #[test]
    fn status_reports_up_to_date_when_current_is_the_latest() {
        assert_eq!(status("v0.2.0", "v0.2.0"), Status::UpToDate);
    }

    #[test]
    fn status_reports_up_to_date_when_current_is_ahead() {
        // A local build one patch ahead of the latest release must not be told to
        // downgrade.
        assert_eq!(status("v0.3.0", "v0.2.0"), Status::UpToDate);
    }

    #[test]
    fn status_reports_update_available_when_current_is_behind() {
        assert_eq!(status("v0.1.0", "v0.2.0"), Status::UpdateAvailable);
    }

    #[test]
    fn status_treats_a_git_describe_build_as_development() {
        // The build.rs dev version format: a tag, commits since, and a short SHA.
        assert_eq!(status("v0.2.0-3-gabc1234", "v0.2.0"), Status::Development);
        assert_eq!(
            status("v0.2.0-3-gabc1234-dirty", "v0.2.0"),
            Status::Development
        );
    }

    #[test]
    fn status_treats_a_prerelease_as_development() {
        assert_eq!(status("v0.2.0-rc.1", "v0.2.0"), Status::Development);
    }

    #[test]
    fn status_treats_a_bare_hash_as_development() {
        // `git describe --always` with no tags yields a bare commit hash.
        assert_eq!(status("abc1234", "v0.2.0"), Status::Development);
    }

    #[test]
    fn outdated_warning_fires_only_for_a_release_behind_the_latest() {
        assert!(outdated_warning("v0.4.0", "v0.5.0").is_some());
        assert!(outdated_warning("v0.5.0", "v0.5.0").is_none());
        assert!(outdated_warning("v0.6.0", "v0.5.0").is_none());
        // A development build is never nagged: it has no clean release to compare.
        assert!(outdated_warning("v0.4.0-3-gabc1234", "v0.5.0").is_none());
    }

    #[test]
    fn is_stale_respects_the_ttl() {
        assert!(!is_stale(1000, 1000));
        assert!(!is_stale(1000, 1000 + CHECK_TTL_SECS - 1));
        assert!(is_stale(1000, 1000 + CHECK_TTL_SECS));
        // A backward clock jump reads as fresh, not a panic.
        assert!(!is_stale(1000, 500));
    }

    #[test]
    fn sanitize_repo_for_filename_replaces_the_slash_and_anything_unsafe() {
        assert_eq!(
            sanitize_repo_for_filename("attunehq/doteph"),
            "attunehq-doteph"
        );
        assert_eq!(
            sanitize_repo_for_filename("some-org/weird repo!"),
            "some-org-weird-repo-"
        );
        // Already-safe characters (letters, digits, `.`, `_`, `-`) pass through.
        assert_eq!(sanitize_repo_for_filename("a.b_c-9/D0"), "a.b_c-9-D0");
    }

    #[test]
    fn different_repos_get_different_cache_paths() {
        let default_path = cache_path("attunehq/doteph").expect("cache dir resolvable");
        let fork_path = cache_path("someone/fork").expect("cache dir resolvable");
        assert_ne!(
            default_path, fork_path,
            "distinct repos must never share a cache file"
        );
        assert!(
            default_path
                .to_string_lossy()
                .contains("update-check-attunehq-doteph"),
            "got: {}",
            default_path.display()
        );
        assert!(
            fork_path
                .to_string_lossy()
                .contains("update-check-someone-fork"),
            "got: {}",
            fork_path.display()
        );
    }

    #[test]
    fn cached_check_round_trips_through_json() {
        let cache = CachedCheck {
            latest: "v1.2.3".to_string(),
            checked_at: 42,
        };
        let bytes = serde_json::to_vec(&cache).unwrap();
        let back: CachedCheck = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.latest, "v1.2.3");
        assert_eq!(back.checked_at, 42);
    }

    #[test]
    fn checksum_for_matches_the_dot_slash_prefixed_name() {
        // The release workflow's `sha256sum ./*.tar.gz` writes a leading `./`.
        let sums = b"deadbeef  ./eph-x86_64-unknown-linux-gnu.tar.gz\ncafef00d  ./eph-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "eph-aarch64-apple-darwin.tar.gz").unwrap(),
            "cafef00d"
        );
    }

    #[test]
    fn checksum_for_errors_on_a_missing_asset() {
        let sums = b"deadbeef  ./eph-x86_64-unknown-linux-gnu.tar.gz\n";
        let err = checksum_for(sums, "eph-aarch64-apple-darwin.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no checksum"), "got: {err}");
    }

    /// Build a gzip tar archive matching the release layout: an `eph-<target>/`
    /// directory containing the platform binary with the given `contents`.
    fn build_release_archive(contents: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);

        let entry_path = format!("eph-{TARGET}/{}", binary_name());
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_path, contents)
            .unwrap();

        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extract_binary_pulls_the_binary_out_of_the_release_layout() {
        let archive = build_release_archive(b"#!/fake eph binary\n");
        let dest = tempfile::NamedTempFile::new().unwrap();

        extract_binary(Cursor::new(archive), dest.path()).unwrap();

        let mut got = Vec::new();
        File::open(dest.path())
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"#!/fake eph binary\n");
    }

    #[test]
    fn extract_binary_errors_when_the_binary_is_absent() {
        // A gzip tar with an unrelated entry: extraction must not silently succeed.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        let body = b"read me";
        header.set_size(body.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "eph-somewhere/README.md", &body[..])
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();

        let dest = tempfile::NamedTempFile::new().unwrap();
        let err = extract_binary(Cursor::new(archive), dest.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not contain"), "got: {err}");
    }

    /// A minimal single-shot HTTP server: it serves each canned `(path, body)`
    /// route once, on its own connection, then the accept loop moves on. It
    /// exercises the real ureq client without a network or extra dependency.
    fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for (path, body) in routes {
                let (mut stream, _) = listener.accept().unwrap();

                // Read the request head so the client's write side is drained
                // before we reply (GET has no body, so headers end the request).
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read(&mut byte).unwrap_or(0) == 1 {
                    req.push(byte[0]);
                    if req.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }

                let request_line = String::from_utf8_lossy(&req);
                let served = request_line.lines().next().unwrap_or("");
                assert!(
                    served.contains(&path),
                    "expected a request for {path}, got: {served}"
                );

                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
            }
        });
        base
    }

    #[test]
    fn latest_tag_parses_the_github_response() {
        let base = serve(vec![(
            "/repos/attunehq/doteph/releases/latest".to_string(),
            br#"{"tag_name":"v9.9.9","name":"v9.9.9"}"#.to_vec(),
        )]);
        let updater = Updater::with_endpoints("attunehq/doteph".to_string(), base, None);
        assert_eq!(updater.latest_tag().unwrap(), "v9.9.9");
    }

    #[test]
    fn fetch_verifies_the_checksum_and_extracts_the_binary() {
        let archive = build_release_archive(b"the new eph\n");
        let digest = hex::encode(Sha256::digest(&archive));
        let asset = format!("eph-{TARGET}.tar.gz");
        let checksums = format!("{digest}  ./{asset}\n");

        // The download base serves checksums.txt first, then the archive, in the
        // order fetch() requests them.
        let base = serve(vec![
            ("/dl/checksums.txt".to_string(), checksums.into_bytes()),
            (format!("/dl/{asset}"), archive),
        ]);
        let updater = Updater::with_endpoints(
            "attunehq/doteph".to_string(),
            "http://unused".to_string(),
            Some(format!("{base}/dl")),
        );

        let dest = tempfile::NamedTempFile::new().unwrap();
        updater.fetch("v9.9.9", dest.path()).unwrap();

        let mut got = Vec::new();
        File::open(dest.path())
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"the new eph\n");
    }

    #[test]
    fn fetch_rejects_a_checksum_mismatch() {
        let archive = build_release_archive(b"tampered\n");
        let asset = format!("eph-{TARGET}.tar.gz");
        // A checksum that does not match the served archive.
        let checksums = format!("{}  ./{asset}\n", "0".repeat(64));

        let base = serve(vec![
            ("/dl/checksums.txt".to_string(), checksums.into_bytes()),
            (format!("/dl/{asset}"), archive),
        ]);
        let updater = Updater::with_endpoints(
            "attunehq/doteph".to_string(),
            "http://unused".to_string(),
            Some(format!("{base}/dl")),
        );

        let dest = tempfile::NamedTempFile::new().unwrap();
        let err = updater
            .fetch("v9.9.9", dest.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }
}
