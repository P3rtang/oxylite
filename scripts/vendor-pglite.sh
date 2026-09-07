#!/usr/bin/env bash
# Re-vendor the PGlite artifact set into crates/client/assets/pglite.
# Downloads the official npm tarball and extracts what the browser needs.
# (No npm project required — this just fetches and unpacks files.)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

VERSION="${1:-0.5.8}"
DEST="$ROOT/crates/sync/assets/pglite"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

blue "downloading @electric-sql/pglite@$VERSION …"
curl -sL "https://registry.npmjs.org/@electric-sql/pglite/-/pglite-$VERSION.tgz" \
    -o "$TMP/pglite.tgz"
tar -xzf "$TMP/pglite.tgz" -C "$TMP"

mkdir -p "$DEST"
# The PGlite class lives in index.js + chunks; pglite.wasm/.data are resolved
# relative to the importing module URL.
blue "extracting module set…"
cp "$TMP/package/dist/index.js" "$DEST/"
cp "$TMP"/package/dist/chunk-*.js "$DEST/"
cp "$TMP/package/dist/pglite.wasm" "$TMP/package/dist/pglite.data" "$TMP/package/dist/initdb.wasm" "$DEST/"

# Apache-2.0 requires the license text to accompany redistribution (§4a).
cp "$TMP/package/LICENSE" "$DEST/LICENSE"

green "vendored into $DEST:"
ls -lh "$DEST"
