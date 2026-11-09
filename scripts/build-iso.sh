#!/usr/bin/env bash
# Build the live ISO.
#
#   BASE_REPO=repo/selfhost-r2 scripts/build-iso.sh
#
# BASE_REPO holds the real busybox and musl (see scripts/bootstrap-selfhost.sh); TOOLS_REPO
# (default repo/tools) the strap tools and forge (scripts/build-tools.sh, scripts/package-forge.sh).
# The kernel is built here from recipes/linux (about 10 minutes the first time; it
# is IMPURE: it uses the host's gcc, flex, bison, bc, perl, libelf and OpenSSL headers)
# and published, signed, into its own repository. Needs on the host: xorriso,
# mksquashfs, openssl and limine (the `limine` tool and /usr/share/limine files).
#
# Writes novara-live.iso (override with ISO=...). Try it with scripts/test-iso-qemu.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

FORGE=${FORGE:-target/debug/forge}
BASE_REPO=${BASE_REPO:-}
KERNEL_REPO=${KERNEL_REPO:-repo/kernel-6.12}
TOOLS_REPO=${TOOLS_REPO:-repo/tools}
CACHE=${CACHE:-sources/cache}
WORK=${WORK:-$HOME/.cache/forge/build}
OUT=${OUT:-packages/iso}
ISO=${ISO:-novara-live.iso}
JOBS=${JOBS:-$(nproc)}

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
[ -n "$BASE_REPO" ] || fail "set BASE_REPO to a repository with busybox and musl"
for t in xorriso mksquashfs openssl limine; do
    command -v "$t" >/dev/null || fail "$t is needed on the host"
done
. scripts/lib-keys.sh
ensure_dev_key || fail "could not set up the development signing key"
sign_existing_repo "$BASE_REPO"
[ -f "$TOOLS_REPO/index.toml" ] || fail "no $TOOLS_REPO: run scripts/build-tools.sh and scripts/package-forge.sh first"

mkdir -p "$OUT"
if [ ! -f "$KERNEL_REPO/index.toml" ]; then
    echo "==== building the kernel"
    "$FORGE" build "${TRUST[@]}" recipes/linux --libc musl --tools host --jobs "$JOBS" \
        --out "$OUT" --source-cache "$CACHE" --work-dir "$WORK" \
        --publish "$KERNEL_REPO" "${SIGN[@]}" >"$OUT/kernel.log" 2>&1 \
        || { grep -v 'no version information available' "$OUT/kernel.log" | tail -n 40; fail "the kernel build failed; the full log is $OUT/kernel.log"; }
fi

echo "==== assembling the ISO"
[ -e "$ISO" ] && fail "$ISO exists; remove it first"
ISO_WORK=${ISO_WORK:-$OUT/iso-work}
[ -e "$ISO_WORK" ] && fail "$ISO_WORK exists from an earlier run; remove it first"
"$FORGE" iso "${TRUST[@]}" --repo "$BASE_REPO" --repo "$KERNEL_REPO" --repo "$TOOLS_REPO" \
    --trusted-keys distro/keys --out "$ISO" --work-dir "$ISO_WORK" 2>&1 | grep -v 'no version information available'
xorriso -indev "$ISO" -find / -maxdepth 3 2>/dev/null | grep -E "vmlinuz|initramfs|rootfs.squashfs|limine|BOOTX64" || true
echo "PASS: $ISO"
