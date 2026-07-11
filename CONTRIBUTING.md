# Contributing

Thanks for your interest in `eph`. `eph` is a small Rust CLI for managing
ephemeral, per-workspace development services. Contributions that improve
correctness, the `.eph` format, cross-platform behavior, documentation, or
maintainability are all welcome.

For the design rationale, the build/test workflow in depth, and a tour of the
source, see the [Developer Guide](docs/developer-guide/README.md). This file is
the short version: setup, working style, and the PR/release process.

## Local Setup

Install Rust (stable), Git, and Docker. Docker must be running for the
integration tests.

```sh
git clone https://github.com/attunehq/doteph.git
cd doteph
make dev        # debug build
make test       # unit + doctests + standard Docker integration tests
```

Optional: enable the repo's pre-commit hook so formatting and lints run before
each commit:

```sh
git config core.hooksPath .githooks
```

## Working Style

- Fix root causes when they are in scope; mention unrelated issues rather than
  changing them without discussion.
- Keep secrets out of commits. A `.eph` file can contain credentials, so never
  commit one with real secrets (dev-only throwaway values are fine).
- Update the docs (`README.md`, `docs/`) when behavior or user-visible commands
  change.
- The Rust coding conventions this project follows are vendored under
  [`.agents/skills/`](.agents/skills/); skim them before larger changes.
- Use plain ASCII quotes in docs, comments, and generated text.
- Run the checks below before opening a PR.

## Checks

These are the same checks CI enforces:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1   # needs a running Docker daemon
```

CI runs these on every push and pull request, and additionally runs the
heavyweight stress suite (`cargo test --test stress -- --ignored
--test-threads=1`). The integration tests start real containers, so they are run
single-threaded to avoid host-port contention. `make precommit` applies lint
fixes, formats the code, and refreshes checked-in bundled skill copies;
`make test` runs the standard suite. See the
[Developer Guide](docs/developer-guide/building-and-testing.md) for the full
breakdown.

## AI-Assisted Contributions

AI-assisted PRs are welcome. The human submitter is responsible for the change:
understand the code, review the generated output, test it, and explain the
intent clearly. Do not submit a raw dump of generated code you cannot defend or
maintain. Maintainers may ask for simplification, tests, or clearer rationale
before review.

## Pull Requests

Please explain:

- Why the change exists.
- What behavior changed.
- Any user-facing, compatibility, or security impact.
- The verification you performed (the exact commands you ran).

## Releases

Releases are published via GitHub Releases. Pushing a tag `vX.Y.Z` triggers the
release workflow ([`.github/workflows/release.yml`](.github/workflows/release.yml)),
which:

1. Builds binaries for seven targets: macOS (x86_64, arm64), Linux glibc
   (x86_64, arm64), Linux musl (x86_64, arm64), and Windows (x86_64). The
   non-native targets cross-compile with [`cargo-cross`](https://github.com/cross-rs/cross);
   `eph` is pure Rust with no system dependencies, so this needs no extra setup.
2. Signs and notarizes the macOS binaries (requires the Apple signing secrets
   below).
3. Packages each target as a `tar.gz` (binary plus `README.md`, `LICENSE`,
   `NOTICE`), generates a `checksums.txt`, and publishes a GitHub Release with
   auto-generated notes.

The binary's `--version` comes from the tag: [`build.rs`](build.rs) derives it
from `git describe` (or the `EPH_VERSION` the workflow injects), so `eph
--version` on a `vX.Y.Z` build prints `vX.Y.Z`.

The auto-generated release notes are the changelog; there is no separate
changelog file.

Every pull request and push to `main` runs the same build matrix as a dry run
(no release is created), so cross-compilation breakage is caught before tagging.

### macOS signing secrets

Tag releases sign and notarize the macOS binaries using these repository
secrets. Until they are configured, tag releases fail at the signing step
(unsigned PR/main dry runs still pass):

| Secret | Purpose |
| --- | --- |
| `APPLE_CERTIFICATE_BASE64` | Developer ID Application certificate, base64-encoded `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | Password for the `.p12` |
| `APPLE_TEAM_ID` | Apple Developer Team ID (signing identity) |
| `APPLE_ID` | Apple ID email used for notarization |
| `APPLE_APP_SPECIFIC_PASSWORD` | App-specific password for that Apple ID |

### Installers

[`scripts/install.sh`](scripts/install.sh) and
[`scripts/install.ps1`](scripts/install.ps1) download the released archive for
the host platform, verify it against `checksums.txt`, and install the binary.
[`.github/workflows/installers.yml`](.github/workflows/installers.yml) smoke-tests
them daily against the published releases.
