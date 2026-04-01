# Fink compiler — standard repo targets
#
# Prerequisites: cargo, cargo-outdated (cargo install cargo-outdated)

.PHONY: deps-check deps-update deps-install clean build test test-full coverage release

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

test-full: test
	cargo clippy -- -D warnings

coverage:
	cargo llvm-cov --lib --html
	@echo "Report: target/llvm-cov/html/index.html"

coverage-summary:
	cargo llvm-cov --lib --summary-only

release:
	cargo build --release
