---
name: rust100k-testing-discipline
description: Apply Matklad Rust100k testing discipline to eph. Use when adding, reviewing, or reorganizing Rust tests, choosing unit versus Docker integration or stress coverage, designing fixtures, rejecting mocks, adding doctests, or changing Makefile and CI test coverage.
---

# Rust100k Testing Discipline

Design tests around product behavior and data, keep them real, and avoid mocks.
This skill combines Matklad's `Delete Cargo Integration Tests` and `How to Test`
guidance with the eph conflict decisions in `.agents/skills/readme.md`.

## Workflow

1. Define the feature boundary first. For eph, good boundaries are `.eph` parsing, workspace identity, environment rendering, lifecycle state transitions, process control, CLI behavior, and live Docker resources.
2. Prefer a small `check(...)` helper with data inputs and expected data outputs over many tests that call internal APIs directly.
3. Keep core tests sans IO: build values in memory and let the function under test compute.
4. Use externalized fixture files when they make cases easy to add, but keep at least one small smoke test that can be run/debugged directly from the IDE.
5. Put pure logic in unit tests beside the module. Use `tests/integration.rs` for CLI and live lifecycle behavior, and `tests/stress.rs` for ignored heavyweight concurrency and all-backend scenarios. Do not create another root integration crate without a distinct build or execution boundary.
6. Do not use mocks. Use real pure functions, temporary workspaces, native child processes, the real Docker daemon, and real wire protocols.
7. Keep executable doctests.
8. Run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`.

## eph policy

- eph does not use mocks, test doubles, `mockall`, or fake boundaries for eph-owned behavior.
- Keep unit tests for parser, renderer, state, selection, watcher, updater, and process helpers in `src/`.
- Keep Docker-backed CLI and lifecycle cases in `tests/integration.rs`; keep resource-intensive, concurrent, and all-source cases in ignored `tests/stress.rs` tests.
- Integration workspaces use `tempfile`; network assertions speak to the real service on eph's assigned host port.
- Avoid sleep-based synchronization in tests. If concurrency is involved, expose a join, receiver, or observable side channel.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
cargo test --test stress -- --ignored --test-threads=1
```

No mocks, no `mockall`, executable doctests, the two deliberate root test
crates, and causality-preserving concurrency tests remain review rules.

Read `references/article-notes.md` for source notes and conflict context.
