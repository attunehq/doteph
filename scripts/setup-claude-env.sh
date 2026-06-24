#!/usr/bin/env bash
# Claude Code cloud environment setup for doteph.
#
# Runs as root on Ubuntu 24.04 before the session starts, per
# https://code.claude.com/docs/en/claude-code-on-the-web#setup-scripts
# Point an environment's Setup script at:  bash scripts/setup-claude-env.sh
#
# Design rules (from the docs):
#   - Never block session start: every step is non-fatal and the script exits 0.
#     ("If the script exits non-zero, the session fails to start.")
#   - Keep total runtime under ~5 minutes so the environment cache can build.
#   - Rust (rustc/cargo), git, gcc/clang/cmake and Docker are pre-installed, so
#     this only fetches crates and warms the build.
# Idempotent and cached; safe to re-run.

set -uo pipefail

log()  { printf '==> %s\n' "$1"; }
warn() { printf 'warn: %s\n' "$1" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Persist a PATH entry for later session commands (best-effort: $CLAUDE_ENV_FILE
# is set for SessionStart hooks and may be unset during the setup script).
persist_path() {
  [ -n "${CLAUDE_ENV_FILE:-}" ] || return 0
  printf 'export PATH="%s:$PATH"\n' "$1" >> "$CLAUDE_ENV_FILE"
}

if ! command -v cargo >/dev/null 2>&1; then
  log "cargo not found; installing Rust via rustup (rustup.rs is allowlisted under Trusted)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal || warn "rustup install failed"
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env" 2>/dev/null || true
  persist_path "$HOME/.cargo/bin"
fi

# Nightly rustfmt powers `make format`; optional, never required to build.
if command -v rustup >/dev/null 2>&1; then
  rustup toolchain install nightly --profile minimal --component rustfmt >/dev/null 2>&1 \
    || warn "nightly rustfmt unavailable; 'make format' may not work"
fi

log "Fetching crates"
cargo fetch --locked || cargo fetch || warn "cargo fetch failed (check the environment's network access level)"

log "Warming the build (best-effort)"
cargo build --all-targets \
  || warn "cargo build did not finish; crates are fetched and the session can build in-session"

# Docker is pre-installed but dockerd is not running and the cache does not keep
# running processes. The integration and stress tests need Docker; start it
# in-session (e.g. 'service docker start'). A plain build needs none.
command -v docker >/dev/null 2>&1 \
  || warn "docker not found; integration/stress tests need it. The build is unaffected."

log "doteph environment ready"
exit 0
