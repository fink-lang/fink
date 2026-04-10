#!/usr/bin/env bash
# Build + package a full release for every supported target.
#
# Requires the host to be able to cross-compile to every target in
# scripts/targets.txt. On CI this script is NOT used — each target is built
# in a separate matrix job and the artifacts are assembled by the package job.
#
# Use this locally to reproduce a full release end-to-end.
#
# Required env vars:
#   VERSION  — version string without leading v, e.g. 0.9.0
#
# Optional:
#   DIST_OUT — where to write tarballs (default: ./dist)

set -euo pipefail

: "${VERSION:?VERSION must be set}"
DIST_OUT="${DIST_OUT:-./dist}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGETS_FILE="$SCRIPT_DIR/targets.txt"

ALL_TARGETS=()
while IFS= read -r line; do
  ALL_TARGETS+=("$line")
done < <(grep -vE '^\s*(#|$)' "$TARGETS_FILE")

# Stamp version into Cargo.toml before building.
make -C "$REPO_DIR" stamp-version VERSION="$VERSION"

# Build each target and stage the binaries where package-release.sh expects them.
DIST_IN="$(mktemp -d)"
trap 'rm -rf "$DIST_IN"' EXIT

for t in "${ALL_TARGETS[@]}"; do
  echo ">>> Building $t"
  make -C "$REPO_DIR" build-target TARGET="$t"
  mkdir -p "$DIST_IN/$t"
  cp "$REPO_DIR/target/$t/release/fink"   "$DIST_IN/$t/fink"
  cp "$REPO_DIR/target/$t/release/finkrt" "$DIST_IN/$t/finkrt"
done

# Package one tarball per supported host target.
mkdir -p "$DIST_OUT"
for t in "${ALL_TARGETS[@]}"; do
  echo ">>> Packaging $t"
  make -C "$REPO_DIR" package-release \
    VERSION="$VERSION" \
    HOST_TARGET="$t" \
    DIST_IN="$DIST_IN" \
    DIST_OUT="$DIST_OUT"
done

echo
echo "Release artifacts written to $DIST_OUT:"
ls -1 "$DIST_OUT"
