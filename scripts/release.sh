#!/usr/bin/env bash
# Cut a release of the oxylite library (roadmap 3.1, #39): the CURRENT
# release is whatever the latest oxylite-v* tag on GitHub says (the tag
# is authoritative, not the local manifest); the script suggests
# patch/minor/major and asks 1/2/3 — or takes --patch/--minor/--major
# directly. Then the dance: bump the manifest, sync the lock, commit,
# push master, verify the package locally (a packaging failure must not
# cost a CI round-trip), then tag oxylite-v<version> and push it — the
# tag is what starts CI (the gate on the tag ref, then the publish).
#
#   scripts/release.sh              # interactive 1/2/3 (patch default)
#   scripts/release.sh --minor
set -euo pipefail
source "$(dirname "$0")/common.sh"

# ---- guards: the dance rewrites history on master; a dirty tree or a
# wrong branch must stop it before anything moves.
[ "$(git branch --show-current)" = "master" ] || {
    red "releases are cut from master"; exit 1;
}
[ -z "$(git status --porcelain --untracked-files=no)" ] || {
    red "tracked changes in the tree — commit or stash first"; exit 1;
}

# ---- mode: a flag, or the 1/2/3(+4) prompt
MODE=""
for arg in "$@"; do
    case "$arg" in
        --patch|--minor|--major|--retry)
            [ -z "$MODE" ] || { red "one release flag at a time"; exit 1; }
            MODE="${arg#--}" ;;
        *)
            red "usage: scripts/release.sh [--patch|--minor|--major|--retry]"
            exit 1 ;;
    esac
done

# ---- the authoritative current version: the remote's latest oxylite-v*
blue "fetching the release tags from GitHub…"
git fetch --tags --quiet origin
CURRENT="$(git ls-remote --tags origin 'refs/tags/oxylite-v*' \
    | sed -e 's|.*refs/tags/oxylite-v||' -e 's|\^{}$||' -e '/^$/d' \
    | sort -V | tail -1 || true)"
HAS_TAG=""
if [ -z "$CURRENT" ]; then
    CURRENT="0.0.0"
    blue "no oxylite-v* tags on the remote yet — suggesting from 0.0.0"
else
    HAS_TAG=1
fi
[[ "$CURRENT" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    red "unparsable current tag: $CURRENT"; exit 1;
}

# The manifest may drift from the remote (hand-bumped, or a release cut
# without the tag). The TAG is authoritative: feedback first, then the
# bump applies on the realigned base — a first release (manifest ahead
# of no tags) is the common drift case.
MANIFEST_V="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/oxylite/Cargo.toml | head -1)"
if [ "$MANIFEST_V" != "$CURRENT" ]; then
    blue "manifest is at v$MANIFEST_V — the remote's latest release is v$CURRENT"
    blue "the manifest realigns to the remote; the bump applies on top of it"
fi

MAJOR=${CURRENT%%.*}
REST=${CURRENT#*.}
MINOR=${REST%%.*}
PATCH=${REST#*.}
PATCH_V="$MAJOR.$MINOR.$((PATCH + 1))"
MINOR_V="$MAJOR.$((MINOR + 1)).0"
MAJOR_V="$((MAJOR + 1)).0.0"

echo "current release: $CURRENT (latest oxylite-v* on the remote)"
echo "  1) patch  → $PATCH_V"
echo "  2) minor  → $MINOR_V"
echo "  3) major  → $MAJOR_V"
[ -n "$HAS_TAG" ] && echo "  4) retry  → $CURRENT (same tag, re-tagged at this commit)"

V=""
RETRY=""
resolve() { # the choice → version (+retry marker); retry only with a tag
    case "$1" in
        1) V="$PATCH_V" ;;
        2) V="$MINOR_V" ;;
        3) V="$MAJOR_V" ;;
        4)
            [ -n "$HAS_TAG" ] || { red "nothing to retry — no tag on the remote"; exit 1; }
            V="$CURRENT"; RETRY=1 ;;
        *) red "pick 1, 2, 3 or 4"; exit 1 ;;
    esac
}
if [ -n "$MODE" ]; then
    case "$MODE" in
        patch) resolve 1 ;;
        minor) resolve 2 ;;
        major) resolve 3 ;;
        retry) resolve 4 ;;
    esac
else
    read -r -p "release type [1/2/3/4] (default 1): " CHOICE || CHOICE=""
    resolve "${CHOICE:-1}"
fi
green "cutting oxylite v$V"

# ---- the dance
blue "setting crates/oxylite/Cargo.toml to v$V…"
# Anchored: only the PACKAGE version line matches (dependency lines are
# indented or the `dep = { version = … }` form — they never start a line).
sed -i "s/^version = \".*\"/version = \"$V\"/" crates/oxylite/Cargo.toml
cargo check -q -p oxylite 2>/dev/null   # syncs Cargo.lock with the manifest

if git diff --quiet -- crates/oxylite/Cargo.toml; then
    # The chosen version equals what the manifest already carries (the
    # first-release drift case: remote at 0.0.0, manifest at 0.1.0,
    # minor bump lands on 0.1.0). No commit — release the manifest as-is.
    green "manifest already at v$V — no bump commit needed"
else
    git add crates/oxylite/Cargo.toml Cargo.lock
    git commit -m "oxylite: release v$V"
fi

blue "pushing master…"
git push origin master

blue "verifying the package locally (dry-run publish — no token, no tag spent)…"
cargo publish -p oxylite --dry-run

TAG="oxylite-v$V"
if [ -n "$RETRY" ]; then
    # The retry re-points the tag at THIS commit — the whole point is
    # the fix that landed since the failed run. Re-running the same
    # commit is legitimate too (infra failures, a missing token):
    # warn, don't block.
    if [ "$(git rev-parse HEAD)" = "$(git rev-parse "$TAG^{commit}" 2>/dev/null || echo none)" ]; then
        blue "HEAD is already the tagged commit — retrying the same code"
    else
        blue "re-pointing $TAG at this commit…"
    fi
    git tag -d "$TAG" 2>/dev/null || true
    if git ls-remote --tags origin "refs/tags/$TAG" | grep -q "$TAG"; then
        git push origin ":refs/tags/$TAG"
    fi
fi

blue "tagging $TAG (the tag is what starts CI)…"
git tag -a "$TAG" -m "oxylite v$V"
git push origin "$TAG"
green "released $TAG — the gate runs on the tag ref, then crates.io. Watch the Actions run."
