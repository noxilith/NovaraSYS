#!/usr/bin/env bash
# Publish the package repository to the project's GitHub Release.
#
#   scripts/publish-release.sh            # assemble repo/release, show what would be uploaded, ask
#   scripts/publish-release.sh -y         # same, without asking
#
# What it does:
#   1. assembles ONE clean repository, repo/release, from the build outputs (the newest version of every
#      real package, no -stage bootstrap packages), signed with the release key;
#   2. uploads every archive to the GitHub Release `repo` of $GH_REPO (default noxilith/NovaraSYS), and the
#      index and its signature LAST, so a client never sees an index that names a file not there yet;
#   3. with PRUNE=1, deletes release assets the new index no longer lists.
#
# Systems read it as  https://github.com/noxilith/NovaraSYS/releases/download/repo  (DEFAULT_REPO_URL).
# Needs the GitHub CLI (`gh auth login` first). The release key is keys/release/novara-release.key:
# BACK IT UP and never commit it. Whoever holds it can publish packages every Novara system will trust.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

FORGE=${FORGE:-target/debug/forge}
GH_REPO=${GH_REPO:-noxilith/NovaraSYS}
TAG=${TAG:-repo}
RELEASE_KEY=${RELEASE_KEY:-keys/release/novara-release.key}
DEST=${DEST:-repo/release}
SOURCES=${SOURCES:-"repo/selfhost-r2 repo/tools repo/kernel-6.12"}
YES=
[ "${1:-}" = "-y" ] && YES=1

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
[ -f "$RELEASE_KEY" ] || fail "no release key at $RELEASE_KEY (forge key generate novara-release --dir keys/release)"
command -v gh >/dev/null || fail "the GitHub CLI (gh) is needed to upload; install it and run: gh auth login"

echo "==== assembling $DEST"
if [ -e "$DEST" ]; then
    echo "$DEST exists from an earlier run; moving it to $DEST.old"
    rm -rf "$DEST.old"
    mv "$DEST" "$DEST.old"
fi
from=()
for s in $SOURCES; do from+=(--from "$s"); done
"$FORGE" repo assemble "$DEST" "${from[@]}" --key "$RELEASE_KEY" 2>&1 | grep -v 'no version information available'

# Never publish something the project's own public key would not accept.
"$FORGE" repo verify "$DEST" --keys distro/keys 2>&1 | grep -v 'no version information available' \
    || fail "the assembled repository does not verify against distro/keys"

total=$(du -sh "$DEST" | cut -f1)
echo "==== about to upload to https://github.com/$GH_REPO/releases/tag/$TAG ($total, $(ls "$DEST" | wc -l) files)"
ls -S "$DEST" | head -5 | sed 's/^/     /'
if [ -z "$YES" ]; then
    read -r -p "Upload? This makes these packages public. [y/N] " ans
    [ "$ans" = "y" ] || [ "$ans" = "Y" ] || { echo "not uploaded (the repository is ready in $DEST)"; exit 0; }
fi

if ! gh release view "$TAG" --repo "$GH_REPO" >/dev/null 2>&1; then
    gh release create "$TAG" --repo "$GH_REPO" --title "Package repository" \
        --notes "The signed NovaraSYS package repository. Not a release of the distro: forge reads index.toml and the archives from here." \
        --prerelease
fi
# Archives first, then the index and its signature.
find "$DEST" -maxdepth 1 -type f ! -name 'index.toml' ! -name 'index.toml.sig' -print0 |
    xargs -0 -n 8 gh release upload "$TAG" --repo "$GH_REPO" --clobber
gh release upload "$TAG" --repo "$GH_REPO" --clobber "$DEST/index.toml" "$DEST/index.toml.sig"

if [ "${PRUNE:-}" = 1 ]; then
    keep=$(ls "$DEST")
    gh release view "$TAG" --repo "$GH_REPO" --json assets --jq '.assets[].name' | while read -r a; do
        grep -qxF "$a" <<<"$keep" || { echo "removing stale asset $a"; gh release delete-asset "$TAG" "$a" --repo "$GH_REPO" -y; }
    done
fi
echo "PASS: published to https://github.com/$GH_REPO/releases/tag/$TAG"
