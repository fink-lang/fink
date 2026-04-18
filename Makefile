# Fink compiler — standard repo targets
#
# Prerequisites: cargo, cargo-outdated (cargo install cargo-outdated)

.PHONY: deps-check deps-update deps-install clean build test test-full bless coverage release \
	stamp-version build-target package-release release-all

deps-check:
	@expected=$$(grep '^channel' rust-toolchain.toml | cut -d'"' -f2); \
	 actual=$$(rustc --version | awk '{print $$2}'); \
	 if [ "$$expected" != "$$actual" ]; then \
	   echo "Rust toolchain mismatch: expected $$expected, found $$actual"; \
	   echo "Run: rustup update"; \
	   exit 1; \
	 fi
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

# Rewrite the placeholder version in Cargo.toml to $(VERSION). No-op if VERSION
# unset. Called by CI before release builds so `fink --version` reports the
# real version. Local devs don't need this — leave Cargo.toml at 0.0.0.
stamp-version:
	@if [ -n "$(VERSION)" ]; then \
	  sed -i.bak 's/^version = "0.0.0"/version = "$(VERSION)"/' Cargo.toml && rm Cargo.toml.bak; \
	  echo "Stamped version $(VERSION) into Cargo.toml"; \
	fi

# Build fink (default features) and finkrt (runtime-only) for a specific
# target triple, release profile.
# Usage: make build-target TARGET=aarch64-apple-darwin
build-target:
	@test -n "$(TARGET)" || (echo "TARGET must be set" && exit 1)
	rustup target add $(TARGET) 2>/dev/null || true
	cargo build --release --target $(TARGET) --bin fink
	cargo build --release --target $(TARGET) --no-default-features --features runtime --bin finkrt

# Assemble a release tarball for HOST_TARGET, bundling finkrt runtimes for
# every supported target (so `fink compile --target=<any>` works offline).
# See scripts/package-release.sh for env var details.
package-release:
	@scripts/package-release.sh

# Build + package a full release for every supported target on the host.
# Slow — requires the host to cross-compile to every target in scripts/targets.txt.
# Usage: make release-all VERSION=0.9.0
release-all:
	@scripts/release-all.sh
