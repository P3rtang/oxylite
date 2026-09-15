#!/usr/bin/env bash
# Re-vendor the PGlite artifact set into crates/oxylite/assets/pglite
# (or any destination — $2, repo-root-relative or absolute; the
# examples/ crates vendor their own copy the same way). Downloads the
# official npm tarball and extracts what the browser needs.
# (No npm project required — this just fetches and unpacks files.)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

VERSION="${1:-0.5.8}"
DEST="${2:-$ROOT/crates/oxylite/assets/pglite}"
[[ "$DEST" = /* ]] || DEST="$ROOT/$DEST"
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

# The ESM files carry `//# sourceMappingURL=` annotations — devtools
# follows them on every boot, and a missing map is a 404 line per module
# in the serving server's log. The maps ship in the same package; vendored
# alongside (loaded only when devtools is open).
cp "$TMP/package/dist/index.js.map" "$DEST/"
cp "$TMP"/package/dist/chunk-*.js.map "$DEST/"

# Apache-2.0 requires the license text to accompany redistribution (§4a).
cp "$TMP/package/LICENSE" "$DEST/LICENSE"

green "vendored into $DEST:"
ls -lh "$DEST"
