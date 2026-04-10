# Fink compiler — standard repo targets
#
# Prerequisites: cargo, cargo-outdated (cargo install cargo-outdated)

.PHONY: deps-check deps-update deps-install clean build test test-full bless coverage release

deps-check:
	cargo outdated

deps-update:
	cargo update

deps-install:
	cargo fetch

clean:
	cargo clean

build:
	cargo build

test:
	cargo test

# BLESS must run single-threaded (-j1) — the proc macro rewrites the .fnk
# test file in place, so concurrent test threads race on the same file.
# Also touches the test-macros crate so cargo re-expands the proc macro and
# picks up any .fnk fixture changes (proc macros don't track those as inputs).
bless:
	@touch crates/test-macros/src/lib.rs
	BLESS=1 cargo test -j1

test-full: test
	cargo clippy -- -D warnings

coverage:
	cargo llvm-cov --lib --html
	@echo "Report: target/llvm-cov/html/index.html"

coverage-summary:
	cargo llvm-cov --lib --summary-only

release:
	cargo build --release
