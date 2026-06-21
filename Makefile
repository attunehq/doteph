.PHONY: help format check check-fix cargo-sort precommit dev release test test-unit test-integration test-stress install install-dev clean

.DEFAULT_GOAL := help

help:
	@echo "Available commands:"
	@echo "  make format           - Format code with cargo +nightly fmt"
	@echo "  make check            - Run clippy linter"
	@echo "  make check-fix        - Run clippy with automatic fixes"
	@echo "  make cargo-sort       - Sort dependencies in Cargo.toml"
	@echo "  make precommit        - Run checks and fixes before committing"
	@echo "  make dev              - Build in debug mode"
	@echo "  make release          - Build in release mode"
	@echo "  make test             - Run all tests (excludes heavyweight stress tests)"
	@echo "  make test-unit        - Run unit tests only"
	@echo "  make test-integration - Run integration tests only (requires Docker)"
	@echo "  make test-stress      - Run heavyweight stress tests (requires Docker, slow)"
	@echo "  make install          - Install eph globally"
	@echo "  make install-dev      - Install as 'eph-dev' to avoid conflicts"
	@echo "  make clean            - Remove build artifacts"

format:
	cargo +nightly fmt

check:
	cargo clippy

check-fix:
	cargo clippy --fix --allow-dirty --allow-staged

cargo-sort:
	cargo sort

precommit: check-fix format
	@echo "Ready to commit!"

dev:
	cargo build

release:
	cargo build --release

test:
	cargo test

test-unit:
	cargo test --lib

test-integration:
	cargo test --test integration

# Heavyweight, Docker-backed stress tests. They are #[ignore]'d so a bare
# `cargo test` skips them; run them explicitly here. Single-threaded to avoid
# host port/resource contention between the many containers they spin up.
# Override EPH_STRESS_WORKSPACES to scale the concurrency stress test.
test-stress:
	cargo test --test stress -- --ignored --test-threads=1

install:
	@cargo install --path . --locked --force
	@CARGO_HOME=$${CARGO_HOME:-$$HOME/.cargo} && \
		VERSION=$$($$CARGO_HOME/bin/eph --version) && \
		echo "Installed '$$VERSION' to $$CARGO_HOME/bin/eph"

install-dev:
	@cargo install --path . --locked --force
	@CARGO_HOME=$${CARGO_HOME:-$$HOME/.cargo} && \
		mv "$$CARGO_HOME/bin/eph" "$$CARGO_HOME/bin/eph-dev" && \
		VERSION=$$($$CARGO_HOME/bin/eph-dev --version) && \
		echo "Installed '$$VERSION' to $$CARGO_HOME/bin/eph-dev"

clean:
	cargo clean
	rm -rf .scratch
