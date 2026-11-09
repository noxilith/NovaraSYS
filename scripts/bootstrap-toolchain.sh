#!/usr/bin/env bash
# Stage 1: build binutils and gcc (C and C++) for musl, then prove that Novara
# can compile with them using store packages only.
#
#   BASE_REPO=repo/stage0-<stamp> scripts/bootstrap-toolchain.sh
#
# BASE_REPO is the repository made by scripts/bootstrap-base.sh (it supplies
# musl, linux-headers, busybox and make). This script adds binutils and gcc.
#
# Like stage 0 it is impure: it compiles with the HOST's gcc, because nothing in
# the store can compile yet. The compilers it produces are static, so they run
# in a sandbox with no libc around them. A later stage rebuilds them with
# themselves.
#
# The gcc build takes a long time (30 to 60 minutes on 8 cores) and a lot of
# memory: JOBS defaults to 6; lower it on a machine with little RAM.
# Every run writes fresh directories and refuses to reuse existing ones.
set -euo pipefail

cd "$(dirname "$0")/.."
FORGE=${FORGE:-target/debug/forge}
BASE_REPO=${BASE_REPO:-}
CACHE=${CACHE:-sources/cache}
JOBS=${JOBS:-6}
STAMP=${STAMP:-$(date +%Y%m%d-%H%M%S)}
OUT=${OUT:-packages/stage1-$STAMP}
REPO=${REPO:-repo/stage1-$STAMP}
WORK=${WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/forge/build}
KEEP_WORK=${KEEP_WORK:-}   # set to 1 to keep each build directory (several GB for gcc) for debugging

fail() { echo "FAIL: $*" >&2; exit 1; }

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
. scripts/lib-keys.sh; ensure_dev_key || fail "could not set up the development signing key"
sign_existing_repo "$BASE_REPO"; sign_existing_repo "$REPO"
[ -n "$BASE_REPO" ] || fail "set BASE_REPO to a stage-0 repository (see scripts/bootstrap-base.sh)"
[ -f "$BASE_REPO/index.toml" ] || fail "$BASE_REPO has no index.toml"
for d in "$OUT" "$REPO"; do
    [ ! -e "$d" ] || fail "$d already exists; choose another STAMP (or OUT/REPO) instead of reusing it"
done
for tool in gcc g++ make ld tar xz sed awk readelf file bison flex m4 perl; do
    command -v "$tool" >/dev/null || fail "stage 1 needs '$tool' on the host"
done
mkdir -p "$OUT/logs"

build() {
    local recipe=$1 log=$OUT/logs/$1.log
    # Our own repository only exists once the first package is published.
    local repos=(--repo "$BASE_REPO")
    if [ -f "$REPO/index.toml" ]; then repos+=(--repo "$REPO"); fi
    printf -- '---- %-10s ' "$recipe"
    if ! "$FORGE" build "${TRUST[@]}" "recipes/$recipe" --libc musl --tools host --jobs "$JOBS" \
        "${repos[@]}" --out "$OUT" --source-cache "$CACHE" \
        --work-dir "$WORK" --publish "$REPO" "${SIGN[@]}" ${KEEP_WORK:+--keep-work} >"$log" 2>&1; then
        echo "FAILED"
        grep -v 'no version information available' "$log" | tail -n 40
        fail "building $recipe failed; the full log is $log"
    fi
    grep -m1 '^built ' "$log" | sed 's/^built //'
}

echo "==== building stage 1 (musl)"
build binutils-stage1
build gcc-stage1

# ---- verify what was built ------------------------------------------------
# Output is captured into variables, never piped into `grep -q`: under
# `pipefail` grep exiting at its first match can fail a correct check.
echo "==== checking the results"
verify=$OUT/verify
for name in binutils-stage1 gcc-stage1; do
    archive=$(ls "$OUT/$name"-*.tar.zst)
    [ "$(echo "$archive" | wc -l)" = 1 ] || fail "expected exactly one $name archive in $OUT"
    "$FORGE" store --store "$verify" install "$archive" --unverified >/dev/null || fail "cannot install $archive"
done
binutils_dir=$(ls -d "$verify"/binutils-stage1-*)
gcc_dir=$(ls -d "$verify"/gcc-stage1-*)

for f in "$binutils_dir/usr/bin/x86_64-linux-musl-ld" "$binutils_dir/usr/bin/x86_64-linux-musl-as" \
         "$gcc_dir/usr/bin/x86_64-linux-musl-gcc" "$gcc_dir/usr/bin/x86_64-linux-musl-g++" \
         "$gcc_dir/usr/libexec/gcc/x86_64-linux-musl/14.2.0/cc1" \
         "$gcc_dir/usr/libexec/gcc/x86_64-linux-musl/14.2.0/cc1plus"; do
    [ -e "$f" ] || fail "$f is missing"
    interp=$(readelf -lW "$f" | grep -c INTERP || true)
    [ "$interp" = 0 ] || fail "$(basename "$f") is dynamically linked; it would not start in the sandbox"
done
echo "ok: the compilers and binutils are fully static"

# The point of stage 1: build C and C++ with store tools only. The fixture names
# its toolchain packages; point it at the stage-1 (bootstrap) ones.
echo "==== compiling C and C++ with store tools only"
cp -R scripts/fixtures/toolchain-check "$OUT/fixture"
sed -i '/^build/{s/"busybox"/"busybox-stage0"/;s/"make"/"make-stage0"/;s/"musl"/"musl-stage0"/;s/"linux-headers"/"linux-headers-stage0"/;s/"binutils"/"binutils-stage1"/;s/"gcc"/"gcc-stage1"/}' "$OUT/fixture/recipe.toml"
log=$OUT/logs/toolchain-check.log
if ! "$FORGE" build "${TRUST[@]}" "$OUT/fixture" --libc musl --tools store \
    --store "$OUT/tools-store" --repo "$BASE_REPO" --repo "$REPO" --out "$OUT/check" \
    --source-cache "$CACHE" --work-dir "$WORK" >"$log" 2>&1; then
    grep -v 'no version information available' "$log" | tail -n 40
    fail "the store-only compile failed; the full log is $log"
fi
"$FORGE" store --store "$verify/check" install "$OUT"/check/toolchain-check-*.tar.zst --unverified >/dev/null \
    || fail "cannot install the check package"
report=$(cat "$OUT"/verify/check/toolchain-check-*/check/result.txt)

expect() {
    case $report in *"$1"*) ;; *) fail "the check report lacks '$1'. It says:
$report" ;; esac
}
expect "compiler: gcc (GCC) 14.2.0"
expect "target: x86_64-linux-musl"
expect "c dynamic: hello from C"
expect "c static: hello from C"
expect "c++ dynamic: hello from C++ (caught)"
expect "c++ static: hello from C++ (caught)"
expect "/lib/ld-musl-x86_64.so.1"
expect "static has no interpreter: yes"
expect "host libc present: no"
expect "host gcc present: no"
case $(grep '^warning' "$log" || true) in *"not reproducible"*) fail "a store-tools build was flagged as not reproducible" ;; esac
echo "ok: C and C++ compiled and ran, using only store packages"

echo
echo "$report"
echo
echo "stage 1 repository: $REPO"
echo "packages:           $OUT"
echo "PASS"
