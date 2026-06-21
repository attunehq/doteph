# eph Developer Guide

Documentation for working **on** `eph` itself - its architecture, how to build
and test it, and how the internals fit together. If you want to *use* `eph`, see
the [User Guide](../user-guide/README.md) instead.

## Contents

1. [Architecture](architecture.md) - the design decisions and the why behind
   them: workspace isolation, automatic ports, state, the file format, and the
   service lifecycle.
2. [Building and Testing](building-and-testing.md) - toolchain, build, the test
   suite (including Docker-backed integration tests), linting, and the
   pre-commit checks CI enforces.
3. [Internals](internals.md) - a module-by-module map of the source: the parser,
   workspace resolution, the Docker-backed service manager, state persistence,
   and env rendering. Start here to find where a change belongs.

See also [CONTRIBUTING.md](../../CONTRIBUTING.md) at the repo root for working
style, pull-request expectations, and the release process.
