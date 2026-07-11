---
name: rust100k-architecture-docs
description: Maintain eph's Rust architecture documentation in the Matklad Rust100k style. Use when changing module boundaries, the CLI/library split, parsing, workspace identity, lifecycle or state ownership, process control, pruning, bundled skills, updates, docs/developer-guide/architecture.md, docs/developer-guide/internals.md, or when reviewing whether code structure and architecture docs agree.
---

# Rust100k Architecture Docs

Keep eph's architecture docs short, stable, and useful as a map. This skill comes from Matklad's `ARCHITECTURE.md` article in the Rust100k series.

## Workflow

1. Start with `docs/developer-guide/architecture.md` and `docs/developer-guide/internals.md`, then inspect the touched Rust modules.
2. Update architecture docs only for durable structure: problem overview, coarse boundaries, invariants, and cross-cutting concerns.
3. Name important modules, files, traits, and types, but avoid fragile Markdown links to local code paths.
4. Call out important absences and boundaries explicitly, such as "watch is binary-side" or "the daemonless process model has no persistent supervisor".
5. Keep implementation details in code comments, module docs, or narrower docs when they are likely to churn.
6. Run the usual cargo checks and review `docs/architecture.md` for durable boundaries.

## eph policy

- Preserve the first plain-language overview for humans who are new to the project.
- Keep the program shape, workspace identity, persisted state, service backends, lifecycle, process control, pruning, and bundled-skill boundaries current.
- If code moves, update the codemap without adding a migration note.
- If a rule is enforced by type construction or config validation, state the invariant where the boundary is described.
- If a topic becomes too detailed, split it into a focused doc and leave only a pointer-level summary in `docs/architecture.md`.

## Validation

Build the site to check the guide's frontmatter and links, then run the Rust checks:

```sh
(cd site && npm run build)
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```

The architecture review remains a human check because durable boundaries depend on the shape of the change.

Read `references/article-notes.md` for the source summary and source URL.
