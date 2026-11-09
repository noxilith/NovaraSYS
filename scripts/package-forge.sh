#!/usr/bin/env bash
# Package forge itself for the live system: a statically linked build with the host's Rust
# toolchain (static glibc: the binary needs no libc on the target, so it runs on musl).
# IMPURE, like stage 0: the toolchain and static glibc come from the build host. Published,
# signed, into $REPO.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

FORGE=${FORGE:-target/debug/forge}
REPO=${REPO:-repo/tools}
OUT=${OUT:-packages/tools}
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
. scripts/lib-keys.sh
ensure_dev_key || fail "could not set up the development signing key"

echo "==== building a static forge"
RUSTFLAGS='-C target-feature=+crt-static' cargo build --release -p forge \
    --target x86_64-unknown-linux-gnu --target-dir target/static 2>&1 | tail -n 3
bin=target/static/x86_64-unknown-linux-gnu/release/forge
readelf -lW "$bin" | grep -q INTERP && fail "$bin is dynamic"

tree=$(mktemp -d "${TMPDIR:-/tmp}/forge-pkg-XXXXXX")
trap 'chmod -R u+w "$tree" 2>/dev/null; rm -rf "$tree"' EXIT
mkdir -p "$tree/usr/bin"
install -m 755 "$bin" "$tree/usr/bin/forge"
strip "$tree/usr/bin/forge"

mkdir -p "$OUT"
archive=$OUT/forge-$VERSION.tar.zst
"$FORGE" store pack "$tree" --name forge --version "$VERSION" --libc musl --out "$archive"
"$FORGE" store publish "$archive" --repo "$REPO" "${SIGN[@]}"
echo "PASS: forge $VERSION is in $REPO"
