# Article Notes

Sources:

- https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html
- https://matklad.github.io/2021/05/31/how-to-test.html

Matklad's durable points:

- Cargo compiles each root `tests/*.rs` file as a separate test binary, so many integration test crates cost compile time and runtime parallelism.
- For internal crates, prefer unit tests in `src/`; for public libraries, use one modular integration crate when an external-public-API test is valuable.
- Prefer `#[cfg(test)] mod tests;` in a sibling test file for larger test bodies so test-only edits do not force normal library recompilation.
- Use data-driven `check(...)` helpers so refactors touch one adapter rather than many test cases.
- Test features and boundaries rather than implementation details.
- Keep IO out of core tests; use explicit data in and data out.
- Use expectation/externalized tests when outputs are large, but keep a direct smoke test for debugging.
- Avoid sleep-based concurrency tests. Preserve causality with join handles, receivers, or observable side channels.
- Use tests as automation for project invariants.

eph decisions:

- Prefer unit tests for pure internal behavior. eph does not use mocks.
- Keep executable doctests.
- `tests/integration.rs` covers CLI and live lifecycle behavior with temporary workspaces and the real Docker daemon.
- `tests/stress.rs` holds ignored heavyweight concurrency, isolation, and all-backend scenarios, including real protocol checks.
