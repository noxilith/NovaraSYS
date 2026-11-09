#!/usr/bin/env bash
# Build the stage-0 base (linux-headers, musl, busybox, make) with the host's
# compiler into a local repository, then check what came out.
#
#   scripts/bootstrap-base.sh                        # musl packages
#   LIBCS="musl glibc" scripts/bootstrap-base.sh     # both
#
# Stage 0 is impure by design: it needs a compiler on the host (gcc, make, ld,
# binutils, tar, sed, awk, strings) because nothing in the store can compile
# yet. Its job is to produce the first tools, a static shell and `make`, so
# later stages can build with `--tools store`.
#
# Every run writes to a fresh directory (packages/stage0-<stamp>/ and
# repo/stage0-<stamp>/) and refuses to reuse an existing one, so a stale or
# half-built repository is never mixed with a new one. Build logs are kept in
# packages/stage0-<stamp>/logs/.
#
# Sources are downloaded by `forge build` (using curl) into $CACHE and checked
# against the SHA-256 pinned in each recipe.
set -euo pipefail

cd "$(dirname "$0")/.."
FORGE=${FORGE:-target/debug/forge}
LIBCS=${LIBCS:-musl}
CACHE=${CACHE:-sources/cache}
STAMP=${STAMP:-$(date +%Y%m%d-%H%M%S)}
OUT=${OUT:-packages/stage0-$STAMP}
REPO=${REPO:-repo/stage0-$STAMP}
WORK=${WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/forge/build}

fail() { echo "FAIL: $*" >&2; exit 1; }

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
. scripts/lib-keys.sh; ensure_dev_key || fail "could not set up the development signing key"
sign_existing_repo "$REPO"
for d in "$OUT" "$REPO"; do
    [ ! -e "$d" ] || fail "$d already exists; choose another STAMP (or OUT/REPO) instead of reusing it"
done
for tool in gcc make ld tar sed awk strings file; do
    command -v "$tool" >/dev/null || fail "stage 0 needs '$tool' on the host"
done
mkdir -p "$OUT/logs"

# musl itself only exists for musl; the other packages are built per libc.
recipes_for() {
    if [ "$1" = musl ]; then echo "linux-headers-stage0 musl-stage0 busybox-stage0 make-stage0"; else echo "linux-headers-stage0 busybox-stage0 make-stage0"; fi
}

for libc in $LIBCS; do
    echo "==== building stage 0 for $libc"
    for r in $(recipes_for "$libc"); do
        log=$OUT/logs/$libc-$r.log
        printf -- '---- %-14s' "$r"
        if ! "$FORGE" build "${TRUST[@]}" "recipes/$r" --libc "$libc" --tools host --out "$OUT/$libc" \
            --source-cache "$CACHE" --work-dir "$WORK" --publish "$REPO" "${SIGN[@]}" >"$log" 2>&1; then
            echo "FAILED"
            grep -v 'no version information available' "$log" | tail -n 40
            fail "building $r for $libc failed; the full log is $log"
        fi
        grep -m1 '^built ' "$log" | sed 's/^built //'
    done
done

# ---- verify what was built --------------------------------------------------
# Install the packages into per-libc stores inside the run directory and inspect
# the binaries: they must be static, linked against the right libc, and work.
echo "==== checking the results"
verify_dir=$OUT/verify
for libc in $LIBCS; do
    store=$verify_dir/$libc
    for r in busybox-stage0 make-stage0; do
        archive=$(ls "$OUT/$libc/$r"-*.tar.zst)
        [ "$(echo "$archive" | wc -l)" = 1 ] || fail "expected exactly one $r archive for $libc in $OUT/$libc"
        "$FORGE" store --store "$store" install "$archive" --unverified >/dev/null \
            || fail "cannot install $archive"
    done
    busybox_dir=$(ls -d "$store"/busybox-stage0-*)
    make_dir=$(ls -d "$store"/make-stage0-*)

    # Note: output is captured into variables instead of piped into `grep -q`:
    # with `pipefail`, grep exiting at its first match kills the producer with
    # SIGPIPE and the pipeline would report failure for a correct binary.
    for bin in "$busybox_dir/bin/busybox" "$make_dir/usr/bin/make"; do
        name=$(basename "$bin")
        [ -x "$bin" ] || fail "$bin is missing"
        info=$(file "$bin")
        case $info in *"statically linked"*) ;; *) fail "$name ($libc) is not statically linked: $info" ;; esac
        glibc_marks=$(strings "$bin" | grep -c 'glibc\.malloc' || true)
        musl_marks=$(strings "$bin" | grep -c 'No error information' || true)
        if [ "$libc" = musl ]; then
            [ "$glibc_marks" = 0 ] || fail "$name ($libc) contains glibc code, so it was not linked against musl"
            [ "$musl_marks" -ge 1 ] || fail "$name ($libc) shows no musl markers"
            echo "ok: $name ($libc) is static and linked against musl"
        else
            [ "$glibc_marks" -ge 1 ] || fail "$name ($libc) shows no glibc markers"
            [ "$musl_marks" = 0 ] || fail "$name ($libc) shows musl markers"
            echo "ok: $name ($libc) is static and linked against glibc"
        fi
    done

    answer=$("$busybox_dir/bin/sh" -c 'echo $((6*7))')
    [ "$answer" = 42 ] || fail "the busybox shell does not work ($libc): got '$answer'"
    version=$("$make_dir/usr/bin/make" --version 2>&1 || true)
    case ${version%%$'\n'*} in "GNU Make"*) ;; *) fail "make does not run ($libc): $version" ;; esac
    echo "ok: the shell and make run ($libc)"
done

# ---- the point of stage 0 ---------------------------------------------------
# Build something using ONLY store packages as tools (BusyBox and make), with
# none of the host's tools mounted. If this works, later stages no longer need
# `--tools host`.
echo "==== building with store tools only"
for libc in $LIBCS; do
    log=$OUT/logs/$libc-store-tools-check.log
    fixture=$OUT/fixture-$libc
    mkdir -p "$fixture" && cp -R scripts/fixtures/store-tools/. "$fixture/"
    sed -i 's/"busybox", "make"/"busybox-stage0", "make-stage0"/' "$fixture/recipe.toml"
    if ! "$FORGE" build "${TRUST[@]}" "$fixture" --libc "$libc" --tools store \
        --store "$OUT/tools-store-$libc" --repo "$REPO" --out "$OUT/check-$libc" \
        --source-cache "$CACHE" --work-dir "$WORK" >"$log" 2>&1; then
        grep -v 'no version information available' "$log" | tail -n 30
        fail "the store-tools build failed for $libc; the full log is $log"
    fi
    "$FORGE" store --store "$OUT/verify/check-$libc" install "$OUT"/check-"$libc"/store-tools-check-*.tar.zst \
        --unverified >/dev/null || fail "cannot install the check package ($libc)"
    report=$(cat "$OUT"/verify/check-"$libc"/store-tools-check-*/check/result.txt)
    case $report in *"make: 4.4.1"*) ;; *) fail "the check did not run GNU make from the store ($libc): $report" ;; esac
    case $report in *"host libc present: no"*) ;; *) fail "the host's libc was visible in the sandbox ($libc)" ;; esac
    case $report in *"host gcc present: no"*) ;; *) fail "the host's gcc was visible in the sandbox ($libc)" ;; esac
    case $(grep '^warning' "$log" || true) in *"not reproducible"*) fail "a store-tools build was flagged as not reproducible ($libc)" ;; esac
    echo "ok: built with only BusyBox and make from the store ($libc)"
done

echo
echo "stage 0 repository: $REPO"
echo "packages:           $OUT"
echo "PASS"
