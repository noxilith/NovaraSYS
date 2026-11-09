#!/usr/bin/env bash
# Exercise `forge gen` against a REAL btrfs filesystem on a loopback image.
#
# The unit tests use a directory-copy backend, so this is the check that the
# actual `btrfs subvolume snapshot/delete` calls behave. Needs root (mount,
# losetup). It only touches a temporary 512 MB image, never your disks:
#
#   cargo build && sudo scripts/test-gen-btrfs.sh
set -euo pipefail

FORGE=${FORGE:-target/debug/forge}
[ -x "$FORGE" ] || { echo "build forge first (cargo build), or set FORGE=/path/to/forge" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo $0" >&2; exit 1; }

work=$(mktemp -d)
mnt=$work/mnt
loop=
cleanup() {
    umount "$mnt" 2>/dev/null || true
    [ -n "$loop" ] && losetup -d "$loop" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
check() { [ "$(cat "$1")" = "$2" ] || fail "$1 is '$(cat "$1")', expected '$2'"; }

truncate -s 512M "$work/disk.img"
mkfs.btrfs -q "$work/disk.img"
loop=$(losetup --find --show "$work/disk.img")
mkdir "$mnt" "$work/boot"
mount "$loop" "$mnt"

btrfs -q subvolume create "$mnt/@"
btrfs -q subvolume create "$mnt/@snapshots"
mkdir -p "$mnt/@/etc"
echo v1 > "$mnt/@/etc/version"
touch "$work/boot/vmlinuz-test" "$work/boot/initramfs-test.img"

gen() {
    local cmdline=$1
    shift
    "$FORGE" gen --live "$mnt/@" --snapshots "$mnt/@snapshots" --top "$mnt" \
        --limine-conf "$work/boot/limine.conf" --boot-dir "$work/boot" \
        --kernel vmlinuz-test --initramfs initramfs-test.img \
        --cmdline "$cmdline" "$@"
}
ON_AT="root=/dev/loop rootflags=subvol=@ rw"
ON_NEXT="root=/dev/loop rootflags=subvol=@.next rw"

echo "== create two generations"
gen "$ON_AT" create first
echo v2 > "$mnt/@/etc/version"
gen "$ON_AT" create second
gen "$ON_AT" list

echo "== snapshots are real, read-only subvolumes"
btrfs subvolume show "$mnt/@snapshots/gen-1" >/dev/null || fail "gen-1 is not a subvolume"
if touch "$mnt/@snapshots/gen-1/x" 2>/dev/null; then fail "gen-1 is writable"; fi
check "$mnt/@snapshots/gen-1/etc/version" v1
check "$mnt/@snapshots/gen-2/etc/version" v2

echo "== rollback to generation 1 (staged, then promoted)"
echo broken > "$mnt/@/etc/version"
gen "$ON_AT" rollback 1
btrfs subvolume show "$mnt/@.next" >/dev/null || fail "@.next is not a subvolume"
check "$mnt/@.next/etc/version" v1
grep -q "subvol=@.next" "$work/boot/limine.conf" || fail "menu does not boot @.next"

gen "$ON_NEXT" rewrite-limine
check "$mnt/@/etc/version" v1
backup=$(echo "$mnt"/@.old-*)
check "$backup/etc/version" broken
grep -q "subvol=@ rw" "$work/boot/limine.conf" || fail "menu does not boot @ again"

gen "$ON_AT" cleanup-next
[ ! -e "$mnt/@.next" ] || fail "@.next still exists"

echo "== remove"
# After the rollback, generation 1 is the current one: it must be protected,
# and generation 2 is free to remove.
if gen "$ON_AT" remove 1 2>/dev/null; then fail "removed the current generation"; fi
[ -e "$mnt/@snapshots/gen-1" ] || fail "gen-1 vanished despite being refused"
gen "$ON_AT" remove 2
[ ! -e "$mnt/@snapshots/gen-2" ] || fail "gen-2 still exists"
[ ! -e "$mnt/@snapshots/gen-2.toml" ] || fail "gen-2 metadata still exists"

echo "PASS"
