#!/usr/bin/env bash
# Build the tools `forge strap` needs on the live system, with the self-hosted
# toolchain only (--tools store: no host compiler), and publish them, signed, to
# $REPO. Static linking throughout, so the live system needs no extra libraries.
#
#   BASE_REPO=repo/selfhost-r2 scripts/build-tools.sh [package...]
#
# With no arguments it builds everything, in dependency order.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

FORGE=${FORGE:-target/debug/forge}
BASE_REPO=${BASE_REPO:-}
REPO=${REPO:-repo/tools}
CACHE=${CACHE:-sources/cache}
WORK=${WORK:-$HOME/.cache/forge/build}
OUT=${OUT:-packages/tools}
JOBS=${JOBS:-$(nproc)}
ALL="zlib zstd lzo ncurses pkgconf util-linux rsync btrfs-progs shadow limine efivar efibootmgr libcap python ninja meson openrc m4 flex bison kmod bash bc libelf perl openssl sudo libnl wpa_supplicant wireless-regdb linux-firmware-wifi"

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
[ -n "$BASE_REPO" ] || fail "set BASE_REPO to the repository with the real busybox, musl, gcc... (scripts/bootstrap-selfhost.sh)"
. scripts/lib-keys.sh
ensure_dev_key || fail "could not set up the development signing key"
sign_existing_repo "$BASE_REPO"
mkdir -p "$OUT/logs"

for p in ${@:-$ALL}; do
    log=$OUT/logs/$p.log
    repos=(--repo "$BASE_REPO")
    if [ -f "$REPO/index.toml" ]; then repos+=(--repo "$REPO"); fi
    printf -- '---- %-14s ' "$p"
    if ! "$FORGE" build "${TRUST[@]}" "recipes/$p" --libc musl --tools store --jobs "$JOBS" \
        --store "$OUT/store" "${repos[@]}" --out "$OUT" --source-cache "$CACHE" \
        --work-dir "$WORK" --publish "$REPO" "${SIGN[@]}" >"$log" 2>&1; then
        echo "FAILED"
        grep -v 'no version information available' "$log" | tail -n 40
        fail "building $p failed; the full log is $log"
    fi
    grep '^built ' "$log" | head -1 | cut -d' ' -f2
done
echo "PASS: tools repository is $REPO"
