#!/usr/bin/env bash
# Run `forge strap` for real, against a loopback disk image.
#
# It installs a tiny fake root filesystem onto a temporary 1 GB image, then
# mounts the result and checks it. Safety:
#   - only the temporary image is ever formatted, never one of your disks;
#   - it passes --no-efi-entry, so your computer's UEFI boot menu is untouched;
#   - the source is a small fake tree, not your running system.
#
# It cannot check that the result actually *boots*; that needs a VM.
# Needs root (losetup, mount, mkfs) and: rsync, sfdisk, mkfs.btrfs, mkfs.vfat.
#
#   cargo build && sudo scripts/test-strap-loop.sh
set -euo pipefail

FORGE=$(realpath "${FORGE:-target/debug/forge}")
[ -x "$FORGE" ] || { echo "build forge first (cargo build), or set FORGE=/path/to/forge" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo $0" >&2; exit 1; }
for tool in rsync sfdisk mkfs.btrfs mkfs.vfat losetup blkid btrfs; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

work=$(mktemp -d)
loop=
cleanup() {
    for m in "$work/esp" "$work/top" "$work/target/boot" "$work/target/forge/store" \
             "$work/target/home" "$work/target/.snapshots" "$work/target" "$work/target-top"; do
        umount "$m" 2>/dev/null || true
    done
    [ -n "$loop" ] && losetup -d "$loop" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
has() { grep -qF -- "$2" "$1" || fail "$1 does not contain: $2"; }

# --- a tiny fake system to install -----------------------------------------
src=$work/src
mkdir -p "$src"/{etc,boot,lib,usr/bin,root,proc,dev,tmp,.snapshots}
echo 'NAME=FakeNovara' > "$src/etc/os-release"
echo hello > "$src/usr/bin/hello"
touch "$src/boot/vmlinuz-6.1.0" "$src/boot/initramfs-6.1.0.img" "$src/lib/ld-musl-x86_64.so.1"
touch "$src/proc/must-not-copy" "$src/dev/must-not-copy" "$src/.snapshots/must-not-copy" "$src/fastboot"
lim=$work/limine
mkdir "$lim"
echo fake-efi > "$lim/BOOTX64.EFI"
echo fake-bios > "$lim/limine-bios.sys"

# --- a loopback disk: 300 MB EFI System Partition + the rest as root --------
truncate -s 1G "$work/disk.img"
# The first partition starts as a plain Linux one (like cfdisk's default), so
# the check below proves that strap itself marks it as an EFI System Partition.
printf 'label: gpt\n,300M,L\n,,L\n' | sfdisk -q "$work/disk.img"
loop=$(losetup --find --show --partscan "$work/disk.img")
udevadm settle 2>/dev/null || true
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -b "${loop}p2" ] && break; sleep 0.5; done
[ -b "${loop}p2" ] || fail "partitions of $loop did not appear"
esp_dev=${loop}p1
root_dev=${loop}p2

strap() {
    "$FORGE" strap --uefi --no-users --no-efi-entry \
        --source "$src" --mount-point "$work/target" --limine-dir "$lim" \
        --hostname testbox "$@" "$root_dev" "$esp_dev"
}

echo "== refuses a mounted partition"
mkdir "$work/esp"
mkfs.vfat -F 32 "$esp_dev" >/dev/null
mount "$esp_dev" "$work/esp"
if strap --yes 2>/dev/null; then fail "strap ran on a mounted partition"; fi
umount "$work/esp"

echo "== a wrong confirmation changes nothing"
out=$(echo "not-the-device" | strap 2>&1 || true)
echo "$out" | grep -qF "aborted: nothing was changed" || fail "strap did not abort; it printed: $out"
# `blkid` alone also succeeds for a partition that merely has a PARTUUID, so
# look for an actual filesystem type, which only exists if it was formatted.
fstype=$(blkid -s TYPE -o value "$root_dev" 2>/dev/null || true)
[ -z "$fstype" ] || fail "root partition has a '$fstype' filesystem after a wrong confirmation"

echo "== install"
strap --yes

echo "== inspect the root volume"
mkdir "$work/top"
mount -o subvolid=5 "$root_dev" "$work/top"
subvols=$(btrfs subvolume list "$work/top" | awk '{print $NF}' | sort | tr '\n' ' ')
[ "$subvols" = "@ @home @snapshots @store " ] || fail "subvolumes are: $subvols"
root=$work/top/@
[ "$(cat "$root/usr/bin/hello")" = hello ] || fail "files were not copied"
has "$root/etc/os-release" FakeNovara
for p in proc/must-not-copy dev/must-not-copy .snapshots/must-not-copy fastboot; do
    [ ! -e "$root/$p" ] || fail "$p was copied but must be excluded"
done
for d in dev proc sys run tmp mnt; do [ -d "$root/$d" ] || fail "$d was not created"; done
[ "$(stat -c %a "$root/tmp")" = 1777 ] || fail "/tmp mode is $(stat -c %a "$root/tmp")"
has "$root/etc/fstab" "subvol=@snapshots,noatime"
has "$root/etc/fstab" "subvol=@store,noatime"
has "$root/etc/fstab" "/boot vfat"
has "$root/etc/forge/system.toml" 'hostname = "testbox"'
has "$root/etc/forge/system.toml" 'libc = "musl"'
root_uuid=$(blkid -s UUID -o value "$root_dev")
has "$root/etc/fstab" "UUID=$root_uuid /"

echo "== inspect the boot partition"
mount "$esp_dev" "$work/esp"
has "$work/esp/EFI/BOOT/BOOTX64.EFI" fake-efi
[ -f "$work/esp/vmlinuz-6.1.0" ] || fail "kernel was not copied to /boot"
has "$work/esp/limine.conf" "path: boot():/vmlinuz-6.1.0"
has "$work/esp/limine.conf" "root=UUID=$root_uuid"
has "$work/esp/limine.conf" "rootflags=subvol=@ rw"
[ "$(blkid -s TYPE -o value "$esp_dev")" = vfat ] || fail "boot partition is not FAT32"
esp_type=$(sfdisk --part-type "$loop" 1 | tr 'A-F' 'a-f')
[ "$esp_type" = c12a7328-f81f-11d2-ba4b-00a0c93ec93b ] || fail "boot partition type is '$esp_type', expected the EFI System Partition"

echo "== the installed layout works with 'forge gen'"
"$FORGE" gen --live "$root" --snapshots "$work/top/@snapshots" --top "$work/top" \
    --limine-conf "$work/esp/limine.conf" --boot-dir "$work/esp" \
    --kernel vmlinuz-6.1.0 --initramfs initramfs-6.1.0.img \
    --cmdline "root=UUID=$root_uuid rootflags=subvol=@ rw" create "first boot"
btrfs subvolume show "$work/top/@snapshots/gen-1" >/dev/null || fail "gen-1 is not a subvolume"
has "$work/esp/limine.conf" "Generation 1: first boot"
has "$work/esp/limine.conf" "path: boot():/vmlinuz-6.1.0"

echo "== nothing is left mounted by strap"
mounted=$(grep -c "$work/target" /proc/self/mounts || true)
[ "$mounted" = 0 ] || fail "$mounted target mounts left behind"

echo "PASS"
