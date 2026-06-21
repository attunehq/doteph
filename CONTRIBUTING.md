# Contributing

Thanks for your interest in `eph`. `eph` is a small Rust CLI for managing
ephemeral, per-workspace development services. Contributions that improve
correctness, the `.eph` format, cross-platform behavior, documentation, or
maintainability are all welcome.

## Local Setup

Install Rust (stable), Git, and Docker. Docker must be running for the
integration tests.

```sh
git clone https://github.com/attunehq/doteph.git
cd doteph
make dev        # debug build
make test       # unit + doctests + Docker integration tests
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
cargo test                       # needs a running Docker daemon
```

`make precommit` runs the format and lint steps; `make test` runs the full
suite.

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

Releases are published via GitHub Releases. Tagging a commit `vX.Y.Z` triggers
the release workflow, which builds binaries for Linux, macOS, and Windows and
attaches them to the release.
