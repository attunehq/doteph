---
name: parse-dont-validate
description: Apply Alexis King's "Parse, don't validate" design rule in eph. Use when adding or reviewing Rust boundary code for .eph text, persisted JSON state, CLI arguments, environment variables, process identities, Compose output, release metadata, or any function that checks input validity before deeper processing.
---

# Parse, Don't Validate

Convert weak external inputs into proof-carrying Rust types at the boundary, then make downstream code require those types.

## Workflow

1. Locate the boundary where untrusted or weakly typed data enters: file load, serde decode, CLI/env parsing, generated data, user edits, or capture metadata.
2. Define the downstream representation you wish processing code could require. Prefer enums, non-empty collections, private-field newtypes, maps/sets, bounded numeric wrappers, and checked structs over `String`, `Vec`, `Option`, loose booleans, or comments.
3. Write a parser or refinement constructor that consumes the weaker input and returns the stronger type, such as `CommandOverride::parse(raw) -> Result<CommandOverride>`.
4. Push the stronger type down into function signatures. If a caller can skip the parse and still typecheck, the design is not done.
5. Keep invalid-input failure in the parse phase. Processing code should not rediscover basic shape errors after it has already acted on the input.
6. For build-time literals or checked-in assets, prefer compile-time or startup-time construction helpers over parse-and-`expect` spread through runtime paths.
7. Run `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and targeted tests.

## eph rules

- Do not add `validate* -> Result<()>` or `parse* -> Result<()>` APIs for boundary shape checks. Return the parsed/refined data.
- Treat `Result<()>` functions with suspicion. They are fine for commands or effects with no meaningful value, but not for preserving input knowledge.
- Raw serde structs may exist at the boundary. Name the checked form for what it proves, such as a record with a non-zero PID and verified process identity or a parsed command with non-empty argv.
- If a value is already created inside typed code and no invalid state is representable, do not parse it again. Make the receiving API require the precise type.
- Avoid denormalized representations unless one small module owns synchronization.

## Enforcement

Nudge blocks new Rust functions named `validate*` or `parse*` that return `Result<()>`. Clippy denies `unnecessary_wraps`, which catches functions that claim fallibility without needing it.

Read `references/article-notes.md` for source notes and examples.
