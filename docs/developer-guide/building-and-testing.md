# Building and Testing

## Toolchain

- **Rust** (stable) for building. The crate targets edition 2024 and a
  minimum Rust version of 1.88 (see `Cargo.toml`).
- **Rust nightly** for formatting only (`make format` runs
  `cargo +nightly fmt`).
- **Git**.
- **Docker**, running. The integration and stress tests start real
  containers, so the daemon must be reachable.

## Clone and build

```sh
git clone https://github.com/attunehq/doteph.git
cd doteph
make dev          # debug build (cargo build)
make release      # optimized release build (fat LTO, stripped)
```

Install the binary:

```sh
make install      # installs `eph` to $CARGO_HOME/bin
make install-dev  # installs as `eph-dev` to avoid clobbering a stable eph
```

## Make targets

The `Makefile` wraps the common Cargo invocations (`make help` lists them):

| Target | What it does |
|--------|--------------|
| `make dev` | Debug build (`cargo build`). |
| `make release` | Release build. |
| `make format` | Format with `cargo +nightly fmt`. |
| `make check` | `cargo clippy`. |
| `make check-fix` | `cargo clippy --fix` (allows dirty/staged). |
| `make cargo-sort` | Sort dependencies in `Cargo.toml` (`cargo sort`). |
| `make skills` | Regenerate `.claude/skills` and `.agents/skills` from `skills/`. |
| `make skills-check` | Verify generated skill copies match the bundled source. |
| `make precommit` | `check-fix`, `format`, then `skills`: the pre-commit gate. |
| `make test` | `cargo test` (unit + doctests + integration; stress is skipped). |
| `make test-unit` | `cargo test --lib` (pure, no Docker). |
| `make test-integration` | `cargo test --test integration` (needs Docker). |
| `make test-stress` | The heavyweight stress suite (needs Docker, slow). |
| `make install` / `make install-dev` | Install the binary. |
| `make clean` | `cargo clean` and remove `.scratch`. |

## Test suites

Three layers, plus a heavyweight fourth:

- **Unit tests** (`src/**`, inline `#[cfg(test)]`): parsing, env rendering,
  and workspace IDs. Pure and fast, no Docker. The parser (`src/parser.rs`)
  and env rendering (`src/env.rs`) carry the densest coverage: the `[env]`
  section and top-level variable placement, duplicate sections/properties/
  names, service and port name validation, quote stripping, interpolation
  (including the `$${` escape and unknown-service validation), and shell
  escaping. Run with `make test-unit` (`cargo test --lib`).
- **Doctests**: the public-API examples in `///` docs are compiled and run,
  so they cannot drift from the code. Included in `cargo test`.
- **Integration tests** (`tests/integration.rs`, with helpers in
  `tests/common/mod.rs`): CLI and live lifecycle behavior. They cover Docker
  services, host processes, roles, hooks, health checks, environment formats,
  reconciliation, state recovery, logs, skill installation, and `eph dev`.
  Needs Docker for the container cases. Run with `make test-integration`.
- **Stress tests** (`tests/stress.rs`): the end-to-end suite. It stands up a
  full multi-service environment (postgres + redis + minio), talks to each
  backend over its real wire protocol on the mapped host ports, runs many
  independent workspaces concurrently and asserts no port or name collisions
  and full data isolation, and exercises every service source
  (`image`/`dockerfile`/`run`/`compose`). These are `#[ignore]`'d so a bare
  `cargo test` skips them; run them with `make test-stress`. Scale the
  concurrency with `EPH_STRESS_WORKSPACES=8 make test-stress`.

Run the standard suite (everything except stress):

```sh
cargo test           # unit + doctests + integration (needs Docker)
```

If Docker is unavailable, run the pure layer in isolation with
`make test-unit`.

## Linting and formatting

CI enforces these checks; run them before opening a PR:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`make precommit` applies lint fixes, formats the code, and regenerates bundled
skills; `make test` runs the standard suite. CI runs on every push **and**
pull request: `cargo fmt --all --check`, then
`cargo clippy --all-targets -- -D warnings`, then
`cargo run --quiet -- skills check`, then
`cargo test --verbose -- --test-threads=1` (single-threaded so the
Docker-backed integration tests do not contend for host ports), and finally
the stress suite. The crate opts into stricter clippy groups at the crate
root (`correctness` denied; `suspicious`, `style`, `complexity`, `perf`
warned) and `#![warn(missing_docs)]` on the library, so new public items need
docs.

### Pre-commit hook

Optionally enable the repo's hook so the checks run on every commit:

```sh
git config core.hooksPath .githooks
```

## Coding conventions

The Rust conventions this project follows are vendored under
[`.agents/skills/`](../../.agents/skills/) (notably the `rust-skills` pack).
Skim them before larger changes. Use plain ASCII quotes in docs, comments,
and generated text.
