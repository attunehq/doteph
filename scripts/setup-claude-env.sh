#!/usr/bin/env bash
set -euo pipefail

# setup-claude-env.sh
#
# Provision a fresh Claude Code cloud environment for doteph.
# Targets a Debian/Ubuntu Linux container that starts with nothing installed.
# Idempotent: safe to re-run. Invoke as: ./scripts/setup-claude-env.sh
#
# What it does:
#   - installs build tooling (git, build-essential, pkg-config)
#   - installs the Rust stable toolchain (plus nightly rustfmt for `make format`)
#   - fetches dependencies and warms the build cache
#
# Note: the integration and stress tests need a running Docker daemon. This
# script does not install Docker; if the environment provides one, the tests
# pick it up automatically. A plain `cargo build` needs no Docker.

GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'
log()  { printf "${GREEN}==>${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}warn:${NC} %s\n" "$1" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

SUDO=""
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then SUDO="sudo"; fi

apt_install() {
  if ! command -v apt-get >/dev/null 2>&1; then
    warn "apt-get not found; please install manually: $*"
    return 0
  fi
  $SUDO apt-get update -y
  $SUDO DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@"
}

log "Installing system build dependencies"
apt_install git build-essential pkg-config ca-certificates curl

if ! command -v cargo >/dev/null 2>&1; then
  log "Installing Rust (stable)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"

log "Ensuring nightly rustfmt is available (used by 'make format')"
rustup toolchain install nightly --profile minimal --component rustfmt >/dev/null 2>&1 \
  || warn "could not install nightly rustfmt; 'make format' may be unavailable"

log "Fetching dependencies"
cargo fetch --locked || cargo fetch

log "Warming the build (cargo build --all-targets)"
cargo build --all-targets

if command -v docker >/dev/null 2>&1; then
  log "Docker detected; integration and stress tests can run"
else
  warn "Docker not found; 'cargo test' integration and stress cases will fail or skip. The build itself is unaffected."
fi

log "doteph environment ready"
