# Repo-Local Rust Skills

This directory contains the Rust guidance used when writing, reviewing, or
refactoring `eph`.

> **`using-eph/` is different.** That skill is generated: it is
> bundled into the `eph` binary and written here by `eph skills install`, and it
> teaches an agent to *use* `eph`, not to work *on* it. Do not hand-edit it; edit
> `skills/using-eph/SKILL.md` at the repo root and re-run `eph skills install`.
> CI (`eph skills check`) fails if the checked-in copy drifts from the source.
> Everything else in this directory is the repo-local Rust guidance described
> below.

- `rust-skills/`: installed from `leonardomso/rust-skills` at commit
  `89910e8585331dabbecd400ae132b4070ecf24af` (179 rules, MIT licensed).
- `rust100k-*`: skills derived from Matklad's Rust100k article index at
  <https://matklad.github.io/2021/09/05/Rust100k.html>.
- `parse-dont-validate/` and `names-are-not-type-safety/`: skills derived from
  Alexis King's articles at
  <https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/> and
  <https://lexi-lambda.github.io/blog/2020/11/01/names-are-not-type-safety/>.

When the sources disagree, the table below records the repository policy.

## Conflict Decisions

| Topic | Matklad | rust-skills | doteph decision |
|---|---|---|---|
| Cargo integration tests | Internal crates should avoid integration crates; public libraries should use at most one modular integration crate. | Put integration tests under `tests/`, with examples using multiple files. | Keep pure logic in module unit tests. `tests/integration.rs` owns CLI and live lifecycle behavior. `tests/stress.rs` is a separate ignored binary because its heavyweight concurrency and all-backend scenarios have a different execution contract. Add another root test binary only for a similarly distinct contract. |
| Test module shape | For larger test bodies, use `#[cfg(test)] mod tests;` and a sibling `tests.rs` so test-only edits avoid recompilation. | Use inline `#[cfg(test)] mod tests { ... }`. | Inline `#[cfg(test)] mod tests { ... }` is fine at this size. Migrate a module to a sibling `tests.rs` only if its test body grows large. |
| Doctests | Disable doctests for internal libraries in large projects when link cost dominates. | Keep examples executable as doctests. | Prefer `rust-skills`. `eph` is small; executable doc examples are valuable. Revisit only if doctest build time becomes a measured problem. |
| Mocking | Favor boundary/data-driven tests and observability over mocks. | Use trait mocks and `mockall` for isolation. | No mocks. Exercise real behavior: pure functions for logic, the real Docker daemon via `tests/integration.rs`, and `tempfile` workspaces for filesystem state. No `mockall`, no first-party `Mock*` types. |
| Generic and `dyn` boundaries | In large systems avoid generics across crate boundaries; use thin wrappers over concrete/`dyn` internals. | Prefer `impl Trait`/generics over type erasure. | `eph` is a single crate with a thin binary and reusable library. Prefer the clearest concrete or generic API and avoid abstractions for hypothetical scale. Measure build cost before changing the boundary. |

## Enforcement

The repo-level checks are:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
cargo test --test stress -- --ignored --test-threads=1
cargo run --quiet -- skills check
```

`make precommit` applies lint fixes, formats the code, and regenerates bundled
skill copies. `make test` runs unit tests, doctests, and the standard Docker
integration suite. `make test-stress` runs the ignored stress suite. CI also
checks that generated skills match `skills/using-eph/SKILL.md`.

Review rules carried over from the source skills (not yet machine-enforced):

- Keep the root integration binaries limited to `tests/integration.rs` and
  `tests/stress.rs` unless a distinct execution contract justifies another.
- Doctests stay enabled.
- No `mockall`, `#[automock]`, or first-party `Mock*` identifiers.
- Boundary code should **parse, not validate**: new `parse*`/`validate*` APIs
  must return refined, proof-carrying types rather than `Result<()>`.
- Prefer real readiness signals (`eph`'s own healthchecks) over sleep-based
  synchronization in tests.
