# Fink compiler — standard repo targets
#
# Prerequisites: cargo, cargo-outdated (cargo install cargo-outdated)

.PHONY: deps-check deps-update deps-install clean build test test-full bless release

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
bless:
	BLESS=1 cargo test -j1

test-full: test
	cargo clippy -- -D warnings

release:
	cargo build --release
