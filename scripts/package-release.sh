#!/usr/bin/env bash
# Package a release tarball for a given host target.
#
# Bundles the host-arch `fink` and `finkrt` binaries, plus `finkrt` for every
# supported target under targets/<triple>/finkrt — so `fink compile --target`
# works fully offline against any supported target.
#
# Required env vars:
#   VERSION      — version string without leading v, e.g. 0.9.0
#   HOST_TARGET  — the target triple this tarball is for (one of scripts/targets.txt)
#   DIST_IN      — directory containing pre-built binaries laid out as:
#                    $DIST_IN/<triple>/fink
#                    $DIST_IN/<triple>/finkrt
#   DIST_OUT     — directory to write the tarball and sha256 into
#
# Output:
#   $DIST_OUT/fink-$VERSION-$HOST_TARGET.tar.gz
#   $DIST_OUT/fink-$VERSION-$HOST_TARGET.tar.gz.sha256

set -euo pipefail

: "${VERSION:?VERSION must be set}"
: "${HOST_TARGET:?HOST_TARGET must be set}"
: "${DIST_IN:?DIST_IN must be set}"
: "${DIST_OUT:?DIST_OUT must be set}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGETS_FILE="$SCRIPT_DIR/targets.txt"

# Read supported targets, skipping comments and blank lines.
# (Avoid `readarray` — not available in macOS system bash 3.2.)
ALL_TARGETS=()
while IFS= read -r line; do
  ALL_TARGETS+=("$line")
done < <(grep -vE '^\s*(#|$)' "$TARGETS_FILE")

STAGE_NAME="fink-$VERSION-$HOST_TARGET"
STAGE_DIR="$DIST_OUT/$STAGE_NAME"

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/targets"

# Host-arch fink + finkrt live at the top of the tree.
cp "$DIST_IN/$HOST_TARGET/fink"   "$STAGE_DIR/fink"
cp "$DIST_IN/$HOST_TARGET/finkrt" "$STAGE_DIR/finkrt"
chmod +x "$STAGE_DIR/fink" "$STAGE_DIR/finkrt"

# All supported finkrts under targets/<triple>/finkrt — bundled so
# `fink compile --target=<any>` works without downloading anything.
for t in "${ALL_TARGETS[@]}"; do
  mkdir -p "$STAGE_DIR/targets/$t"
  cp "$DIST_IN/$t/finkrt" "$STAGE_DIR/targets/$t/finkrt"
  chmod +x "$STAGE_DIR/targets/$t/finkrt"
done

# Create tarball. Use -C so the tarball contains the versioned directory only.
TARBALL="$DIST_OUT/$STAGE_NAME.tar.gz"
tar -czf "$TARBALL" -C "$DIST_OUT" "$STAGE_NAME"

# SHA256 — use shasum if present (macOS default), else sha256sum (Linux).
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$DIST_OUT" && sha256sum "$STAGE_NAME.tar.gz" > "$STAGE_NAME.tar.gz.sha256")
else
  (cd "$DIST_OUT" && shasum -a 256 "$STAGE_NAME.tar.gz" > "$STAGE_NAME.tar.gz.sha256")
fi

# Clean up the staging dir — we only want the tarball in DIST_OUT.
rm -rf "$STAGE_DIR"

echo "Packaged: $TARBALL"
