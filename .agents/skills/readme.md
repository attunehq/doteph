# Repo-Local Rust Skills

This directory vendors the Rust guidance used when writing, reviewing, or
refactoring `eph` (the `doteph` repo). It was copied from the Homeport repo's
`.agents/skills/` set.

- `rust-skills/`: installed from `leonardomso/rust-skills` at commit
  `89910e8585331dabbecd400ae132b4070ecf24af` (179 rules, MIT licensed).
- `rust100k-*`: skills derived from Matklad's Rust100k article index at
  <https://matklad.github.io/2021/09/05/Rust100k.html>.
- `parse-dont-validate/` and `names-are-not-type-safety/`: skills derived from
  Alexis King's articles at
  <https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/> and
  <https://lexi-lambda.github.io/blog/2020/11/01/names-are-not-type-safety/>.

When the sources disagree, the table below records which one wins for this repo.

## Conflict Decisions

| Topic | Matklad | rust-skills | doteph decision |
|---|---|---|---|
| Cargo integration tests | Internal crates should avoid integration crates; public libraries should use at most one modular integration crate. | Put integration tests under `tests/`, with examples using multiple files. | Mixed. `eph` is a small CLI, so prefer unit tests in the library for pure logic (parsing, interpolation, workspace IDs, env formatting). Keep exactly **one** modular integration crate, `tests/integration.rs`, because the Docker daemon is a genuine external boundary that must be exercised end to end. Do not add more root `tests/*.rs` binaries. |
| Test module shape | For larger test bodies, use `#[cfg(test)] mod tests;` and a sibling `tests.rs` so test-only edits avoid recompilation. | Use inline `#[cfg(test)] mod tests { ... }`. | Inline `#[cfg(test)] mod tests { ... }` is fine at this size. Migrate a module to a sibling `tests.rs` only if its test body grows large. |
| Doctests | Disable doctests for internal libraries in large projects when link cost dominates. | Keep examples executable as doctests. | Prefer `rust-skills`. `eph` is small; executable doc examples are valuable. Revisit only if doctest build time becomes a measured problem. |
| Mocking | Favor boundary/data-driven tests and observability over mocks. | Use trait mocks and `mockall` for isolation. | No mocks. Exercise real behavior: pure functions for logic, the real Docker daemon via `tests/integration.rs`, and `tempfile` workspaces for filesystem state. No `mockall`, no first-party `Mock*` types. |
| Generic and `dyn` boundaries | In large systems avoid generics across crate boundaries; use thin wrappers over concrete/`dyn` internals. | Prefer `impl Trait`/generics over type erasure. | Prefer `rust-skills`. `eph` is small enough that runtime clarity wins; do not over-abstract (`anti-over-abstraction`). |

## Enforcement

The repo-level checks are:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test            # unit + doctests + the Docker integration crate (needs Docker)
```

`make precommit` runs the format and lint checks; `make test` runs the full
suite. CI (`.github/workflows/ci.yml`) runs all three, including the Docker
integration tests, on every push and pull request.

Review rules carried over from the source skills (not yet machine-enforced):

- No new Cargo integration test crates beyond `tests/integration.rs`.
- Doctests stay enabled.
- No `mockall`, `#[automock]`, or first-party `Mock*` identifiers.
- Boundary code should **parse, not validate**: new `parse*`/`validate*` APIs
  must return refined, proof-carrying types rather than `Result<()>`.
- Prefer real readiness signals (`eph`'s own healthchecks) over sleep-based
  synchronization in tests.
</content>
</invoke>
