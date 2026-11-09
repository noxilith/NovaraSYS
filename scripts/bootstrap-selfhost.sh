#!/usr/bin/env bash
# Self-hosting: Novara rebuilds its whole base and toolchain with its own
# compiler, in sandboxes with no host tools, until the result stops changing.
#
#   BASE_REPO=repo/stage0-<stamp> STAGE1_REPO=repo/stage1-<stamp> \
#       scripts/bootstrap-selfhost.sh
#
# BASE_REPO comes from scripts/bootstrap-base.sh and STAGE1_REPO from
# scripts/bootstrap-toolchain.sh. Both hold packages built by the HOST's
# compiler (`*-stage0`, `*-stage1`); nothing built from here on uses the host.
#
# The six packages are linux-headers (from the kernel source), musl, binutils,
# busybox, make and gcc. The recipes in recipes/ (without a suffix) describe the
# real packages; this script derives each GENERATION from them by renaming, so
# that generations never clash by name:
#
#   stage2   built by the stage-1 compilers and the stage-0 tools
#   stage3   built by stage2
#   (plain)  built by stage3: the real packages, with the real names
#
# Why not stop at stage2: a static binary contains libc code. Stage2 links the
# stage-0 musl, which the HOST's gcc compiled; stage3 links the musl that
# Novara's own compiler built, so it is the first generation with no host
# compiler output in it. The fixed point therefore appears one generation later:
# stage3 and the plain generation must be identical, file by file.
#
# Takes about 35 minutes on 8 cores. Every run writes fresh directories and
# refuses to reuse existing ones. No downloads: all sources are already cached.
set -euo pipefail

cd "$(dirname "$0")/.."
FORGE=${FORGE:-target/debug/forge}
BASE_REPO=${BASE_REPO:-}
STAGE1_REPO=${STAGE1_REPO:-}
CACHE=${CACHE:-sources/cache}
JOBS=${JOBS:-6}
STAMP=${STAMP:-$(date +%Y%m%d-%H%M%S)}
OUT=${OUT:-packages/selfhost-$STAMP}
REPO=${REPO:-repo/selfhost-$STAMP}
WORK=${WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/forge/build}

# Build order: a package's run-time dependencies are always earlier in the list.
PKGS="linux-headers musl binutils busybox make gcc"

fail() { echo "FAIL: $*" >&2; exit 1; }

[ -x "$FORGE" ] || fail "build forge first (cargo build), or set FORGE=/path/to/forge"
. scripts/lib-keys.sh; ensure_dev_key || fail "could not set up the development signing key"
sign_existing_repo "$BASE_REPO"; sign_existing_repo "$STAGE1_REPO"; sign_existing_repo "$REPO"
for v in BASE_REPO STAGE1_REPO; do
    [ -n "${!v}" ] || fail "set $v (see the comments at the top of this script)"
    [ -f "${!v}/index.toml" ] || fail "${!v} has no index.toml"
done
for d in "$OUT" "$REPO"; do
    [ ! -e "$d" ] || fail "$d already exists; choose another STAMP (or OUT/REPO) instead of reusing it"
done
mkdir -p "$OUT/logs"

# ---- deriving a generation's recipes --------------------------------------

# Which package provides <pkg> as a TOOL for a build: "bootstrap" means the
# host-built stage-0/stage-1 packages, anything else is a suffix such as -stage2.
tool_name() {
    case $2 in
        bootstrap)
            case $1 in
                binutils|gcc) echo "$1-stage1" ;;
                *) echo "$1-stage0" ;;
            esac ;;
        *) echo "$1$2" ;;
    esac
}

# make_recipe <pkg> <dest-dir> <own-suffix> <tools>: copy recipes/<pkg> and
# rename it. The package gets <own-suffix>, its build dependencies become the
# tools of the previous generation, its run dependencies stay in its own
# generation.
make_recipe() {
    local pkg=$1 dest=$2 own=$3 tools=$4 p expr_build="" expr_run=""
    mkdir -p "$dest"
    cp -R "recipes/$pkg/." "$dest/"
    for p in $PKGS; do
        expr_build+="s/\"$p\"/\"$(tool_name "$p" "$tools")\"/g;"
        expr_run+="s/\"$p\"/\"$p$own\"/g;"
    done
    sed -i -e "/^build *= *\[/{${expr_build}}" -e "/^run *= *\[/{${expr_run}}" \
        -e "s/^name\\( *\\)= \"$pkg\"\$/name\\1= \"$pkg$own\"/" "$dest/recipe.toml"
}

# ---- building ---------------------------------------------------------------

# build <recipe-dir> <label> <store> [repo...]: build with store tools only and
# publish the result to $REPO (which is also searched, once it exists).
build() {
    local dir=$1 label=$2 store=$3; shift 3
    local log=$OUT/logs/${label//\//-}.log repos=() r
    for r in "$@"; do repos+=(--repo "$r"); done
    if [ -f "$REPO/index.toml" ]; then repos+=(--repo "$REPO"); fi
    printf -- '---- %-26s ' "$label"
    if ! "$FORGE" build "${TRUST[@]}" "$dir" --libc musl --tools store --jobs "$JOBS" --store "$store" \
        "${repos[@]}" --out "$OUT" --source-cache "$CACHE" --work-dir "$WORK" \
        --publish "$REPO" "${SIGN[@]}" >"$log" 2>&1; then
        echo "FAILED"
        grep -v 'no version information available' "$log" | tail -n 40
        fail "building $label failed; the full log is $log"
    fi
    # No host tools were mounted, so nothing should be flagged as impure.
    case $(grep '^warning' "$log" || true) in *"not reproducible"*) fail "$label was built with host tools" ;; esac
    grep -m1 '^built ' "$log" | sed 's/^built //'
}

# generation <label> <own-suffix> <tools> [repo...]
generation() {
    local label=$1 own=$2 tools=$3 p; shift 3
    echo "==== $label: built by $([ "$tools" = bootstrap ] && echo "the stage-0/1 bootstrap packages" || echo "$tools")"
    for p in $PKGS; do
        make_recipe "$p" "$OUT/recipes/$label/$p" "$own" "$tools"
        build "$OUT/recipes/$label/$p" "$label/$p$own" "$OUT/store-$label" "$@"
    done
}

# install_one <package-name> <store>: install the one archive for that package.
# (`name-[0-9]*` cannot match `name-stage3-...`, so plain and suffixed names
# never get mixed up.)
install_one() {
    local archive
    archive=$(ls "$OUT"/"$1"-[0-9]*.tar.zst)
    [ "$(echo "$archive" | wc -l)" = 1 ] || fail "expected exactly one archive for $1 in $OUT"
    "$FORGE" store --store "$2" install "$archive" --unverified >/dev/null || fail "cannot install $archive"
}

# ---- checking ---------------------------------------------------------------

# Output is captured into variables, never `strings | grep -q`: under pipefail
# grep exiting at its first match can fail a correct check.
check_static_musl() {
    local f=$1 info glibc_marks musl_marks
    [ -e "$f" ] || fail "$f is missing"
    info=$(file -L "$f")
    case $info in *"statically linked"*) ;; *) fail "$(basename "$f") is not static: $info" ;; esac
    glibc_marks=$(strings "$f" | grep -c 'glibc\.malloc' || true)
    musl_marks=$(strings "$f" | grep -c 'No error information' || true)
    [ "$glibc_marks" = 0 ] || fail "$(basename "$f") contains glibc code"
    [ "$musl_marks" -ge 1 ] || fail "$(basename "$f") shows no musl markers"
}

verify_generation() {
    local label=$1 own=$2 store=$OUT/verify/$1 p b g bb mk answer version f
    for p in binutils gcc busybox make; do install_one "$p$own" "$store"; done
    b=$(ls -d "$store/binutils$own"-[0-9]*); g=$(ls -d "$store/gcc$own"-[0-9]*)
    bb=$(ls -d "$store/busybox$own"-[0-9]*); mk=$(ls -d "$store/make$own"-[0-9]*)
    local libexec=$g/usr/libexec/gcc/x86_64-linux-musl/14.2.0
    for f in "$b/usr/bin/ld" "$b/usr/bin/as" "$g/usr/bin/gcc" "$g/usr/bin/g++" \
             "$libexec/cc1" "$libexec/cc1plus" "$bb/bin/busybox" "$mk/usr/bin/make"; do
        check_static_musl "$f"
    done
    answer=$("$bb/bin/sh" -c 'echo $((6*7))')
    [ "$answer" = 42 ] || fail "the $label busybox shell does not work: got '$answer'"
    version=$("$mk/usr/bin/make" --version 2>&1 || true)
    case ${version%%$'\n'*} in "GNU Make"*) ;; *) fail "the $label make does not run: $version" ;; esac
    echo "ok: $label: binutils, gcc, busybox and make are static, linked against musl, and run"
}

# ---- the generations --------------------------------------------------------

generation stage2 -stage2 bootstrap "$BASE_REPO" "$STAGE1_REPO"
verify_generation stage2 -stage2

generation stage3 -stage3 -stage2
verify_generation stage3 -stage3

generation release "" -stage3
verify_generation release ""

# ---- the real packages must stand on their own ------------------------------
# Compile C and C++ with ONLY the plain-named packages: the repository passed
# holds nothing else, so no bootstrap or host-built package can be used.
echo "==== the real packages on their own"
log=$OUT/logs/toolchain-check.log
if ! "$FORGE" build "${TRUST[@]}" scripts/fixtures/toolchain-check --libc musl --tools store \
    --store "$OUT/store-final" --repo "$REPO" --out "$OUT/check" \
    --source-cache "$CACHE" --work-dir "$WORK" >"$log" 2>&1; then
    grep -v 'no version information available' "$log" | tail -n 30
    fail "the real packages could not compile the check; the full log is $log"
fi
"$FORGE" store --store "$OUT/verify/check" install "$OUT"/check/toolchain-check-*.tar.zst --unverified >/dev/null \
    || fail "cannot install the check package"
report=$(cat "$OUT"/verify/check/toolchain-check-*/check/result.txt)
for want in "compiler: gcc (GCC) 14.2.0" "target: x86_64-linux-musl" "c dynamic: hello from C" \
            "c static: hello from C" "c++ dynamic: hello from C++ (caught)" \
            "c++ static: hello from C++ (caught)" "static has no interpreter: yes" \
            "host libc present: no" "host gcc present: no"; do
    case $report in *"$want"*) ;; *) fail "the check report lacks '$want'. It says:
$report" ;; esac
done
stored=$(ls "$OUT/store-final")
case $stored in *stage*) fail "a bootstrap package was in the check's store, so it proves nothing:
$stored" ;; esac
echo "ok: C and C++ compile and run with only the real packages ($(echo "$stored" | sed 's/-[0-9a-f]\{32\}//' | tr '\n' ' '))"

# ---- the fixed point --------------------------------------------------------
echo "==== comparing stage3 and the real packages (the fixed point)"
cmp3=$OUT/compare-stage3
for p in $PKGS; do install_one "$p-stage3" "$cmp3"; install_one "$p" "$cmp3"; done
# The exit status is checked directly: piping the comparison into anything would
# hide it.
for p in $PKGS; do
    a=$(ls -d "$cmp3/$p-stage3"-[0-9]*); b=$(ls -d "$cmp3/$p"-[0-9]*)
    if scripts/compare-trees.sh "$a" "$b" >"$OUT/logs/compare-$p.txt" 2>&1; then
        echo "ok: $p: stage3 and the real package are identical ($(tail -n 1 "$OUT/logs/compare-$p.txt"))"
    else
        cat "$OUT/logs/compare-$p.txt"
        fail "$p: stage3 and the real package differ, so the chain is not at a fixed point"
    fi
done

# For information only: stage2 still links the host-compiled stage-0 musl, so it
# is expected to differ from stage3 in the libc code inside static binaries.
echo "==== for information: stage2 vs stage3 (not required to match)"
cmp2=$OUT/compare-stage2
for p in $PKGS; do install_one "$p-stage2" "$cmp2"; install_one "$p-stage3" "$cmp2"; done
for p in $PKGS; do
    a=$(ls -d "$cmp2/$p-stage2"-[0-9]*); b=$(ls -d "$cmp2/$p-stage3"-[0-9]*)
    scripts/compare-trees.sh "$a" "$b" >"$OUT/logs/compare2-$p.txt" 2>&1 || true
    echo "  $p: $(tail -n 1 "$OUT/logs/compare2-$p.txt")"
done

echo
echo "repository: $REPO   (the plain-named packages are the real ones)"
echo "packages:   $OUT"
echo "PASS"
