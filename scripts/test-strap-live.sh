#!/usr/bin/env bash
# Run the packaged `forge strap`, with the packaged tools, on the real live root from the ISO,
# onto a loopback disk image, and check what it installed.
#
#   sudo scripts/test-strap-live.sh [novara-live.iso]
#
# What it does:
#   1. extracts the live system (live/rootfs.squashfs) from the ISO into a scratch directory;
#   2. makes a 2 GB image with an EFI partition and a root partition (like cfdisk would);
#   3. enters a private mount namespace, chroots into the live root and runs
#      `forge strap --uefi` there, exactly as the user would on the live system, so rsync,
#      mkfs.btrfs, limine, blkid, ... are the ones from the packages, not the host's;
#   4. mounts the result and checks it: subvolumes, fstab, limine.conf (root=PARTUUID=, no
#      initramfs), the kernel, the EFI loader, init, forge, and that the ISO was not copied.
#   5. if qemu-system-x86_64 and OVMF (OVMF=/path/to/OVMF_CODE.fd) are available, BOOTS the
#      installed disk and waits for a login prompt on the serial console. Otherwise it says so.
#
# Safety: only the temporary image is ever formatted, never one of your disks. It passes
# --no-efi-entry, so your computer's UEFI boot menu is untouched. Needs root (losetup, mount,
# chroot) and on the host: xorriso, unsquashfs, sfdisk, losetup, btrfs (to inspect the result).
# KEEP_IMAGE=/path/disk.img keeps the installed image.
set -euo pipefail

ISO=$(realpath "${1:-novara-live.iso}")
[ -f "$ISO" ] || { echo "no ISO at $ISO: run scripts/build-iso.sh first" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo $0" >&2; exit 1; }
for t in xorriso unsquashfs sfdisk losetup btrfs blkid unshare chroot; do
    command -v "$t" >/dev/null || { echo "missing host tool: $t" >&2; exit 1; }
done

work=$(mktemp -d /var/tmp/novara-strap-XXXXXX)
loop=
qpid=
cleanup() {
    [ -n "$qpid" ] && kill "$qpid" 2>/dev/null || true
    for m in "$work/esp" "$work/top"; do umount "$m" 2>/dev/null || true; done
    [ -n "$loop" ] && losetup -d "$loop" 2>/dev/null || true
    if [ -n "${KEEP_IMAGE:-}" ] && [ -f "$work/disk.img" ]; then mv "$work/disk.img" "$KEEP_IMAGE"; fi
    # Never delete through a mount that is somehow still alive (a bound /dev would be wiped).
    if grep -qF " $work" /proc/mounts; then
        echo "WARNING: something is still mounted under $work; NOT deleting it. Unmount it, then remove the directory yourself." >&2
        return 0
    fi
    rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "ok: $*"; }
has()  { grep -qF -- "$2" "$1" || fail "$1 does not contain: $2"; }
hasnt(){ ! grep -qF -- "$2" "$1" || fail "$1 must not contain: $2"; }

echo "---- extracting the live system"
xorriso -osirrox on -indev "$ISO" -extract /live/rootfs.squashfs "$work/rootfs.sqsh" >/dev/null 2>&1 \
    || fail "cannot read live/rootfs.squashfs from $ISO"
unsquashfs -q -d "$work/live" "$work/rootfs.sqsh" >/dev/null
for t in usr/bin/forge usr/bin/rsync usr/bin/mkfs.btrfs usr/bin/limine usr/sbin/sfdisk usr/sbin/blkid usr/sbin/useradd; do
    [ -x "$work/live/$t" ] || fail "the live system lacks /$t"
done
ok "the live system has forge and the strap tools"

echo "---- making the disk"
truncate -s 2G "$work/disk.img"
printf 'label: gpt\n,300M,L\n,,L\n' | sfdisk -q "$work/disk.img"
loop=$(losetup --find --show --partscan "$work/disk.img")
udevadm settle 2>/dev/null || true
for _ in $(seq 1 20); do [ -b "${loop}p2" ] && break; sleep 0.5; done
[ -b "${loop}p2" ] || fail "partitions of $loop did not appear"

echo "---- forge strap, inside the live root"
mkdir -p "$work/live/mnt/target"
unshare -m --propagation private sh -c '
    set -e
    live=$1; loop=$2
    mount --rbind /dev "$live/dev"
    mount -t proc proc "$live/proc"
    mount -t sysfs sysfs "$live/sys"
    exec chroot "$live" /usr/bin/env PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        forge strap --uefi --no-efi-entry --no-users --yes "${loop}p2" "${loop}p1"
' sh "$work/live" "$loop" 2>&1 | grep -v 'no version information' || fail "forge strap failed (output above)"
ok "forge strap finished"

echo "---- checking the result"
mkdir -p "$work/top" "$work/esp"
mount "${loop}p1" "$work/esp"
mount -o subvolid=5 "${loop}p2" "$work/top"
sv=$(btrfs subvolume list "$work/top" | awk '{print $NF}' | tr '\n' ' ')
for s in @ @snapshots @home @store; do
    case " $sv " in *" $s "*) ;; *) fail "missing subvolume $s (have: $sv)";; esac
done
ok "btrfs subvolumes: $sv"
root=$work/top/@
[ -x "$root/sbin/init" ] || [ -L "$root/sbin/init" ] || fail "no /sbin/init"
[ -x "$root/usr/bin/forge" ] || fail "forge was not copied"
[ -f "$root/etc/fstab" ] || fail "no /etc/fstab"
has "$root/etc/fstab" "subvol=@,"
[ -f "$root/etc/forge/system.toml" ] || fail "no /etc/forge/system.toml"
[ ! -e "$root/medium/live" ] || fail "the ISO under /medium was copied to the installed system"
ok "the installed root has init, forge, fstab and system.toml, and not the ISO"

conf=$work/esp/limine.conf
[ -f "$conf" ] || fail "no limine.conf on the boot partition"
has "$conf" "path: boot():/vmlinuz-"
hasnt "$conf" "module_path"
want=$(blkid -s PARTUUID -o value "${loop}p2")
has "$conf" "root=PARTUUID=$want"
ls "$work/esp"/vmlinuz-* >/dev/null 2>&1 || fail "the kernel is not on the boot partition"
[ -f "$work/esp/EFI/BOOT/BOOTX64.EFI" ] || fail "no EFI loader at EFI/BOOT/BOOTX64.EFI"
ok "limine.conf boots the kernel with root=PARTUUID=$want and no initramfs"
# (`blkid -s PART_ENTRY_TYPE` only answers with low-level probing, so read the table instead.)
ptype=$(sfdisk --part-type "$work/disk.img" 1 | tr 'a-f' 'A-F')
[ "$ptype" = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" ] \
    || fail "the boot partition is not marked as an EFI System Partition (type is $ptype)"
ok "the boot partition is an EFI System Partition"

echo "---- booting the installed disk"
umount "$work/esp" "$work/top"
if command -v qemu-system-x86_64 >/dev/null && [ -n "${OVMF:-}" ]; then
    log=$work/qemu.log
    kvm=(); [ -w /dev/kvm ] && kvm=(-enable-kvm -cpu host)
    qemu-system-x86_64 -m 1024 "${kvm[@]}" \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF" \
        -drive "file=$work/disk.img,format=raw,if=virtio" \
        -display none -serial "file:$log" -no-reboot </dev/null >/dev/null 2>&1 &
    qpid=$!
    for _ in $(seq 1 ${TIMEOUT:-120}); do grep -q "login:" "$log" 2>/dev/null && break; sleep 1; done
    grep -q "login:" "$log" || { cp "$log" /var/tmp/novara-strap-boot.log 2>/dev/null; tail -n 30 "$log" 2>/dev/null; fail "the installed system did not reach a login prompt (log kept at /var/tmp/novara-strap-boot.log)"; }
    ok "the installed system booted to a login prompt"
else
    echo "skipped: needs qemu-system-x86_64 and OVMF=/path/to/OVMF_CODE.fd (the install itself passed)"
fi
echo "PASS"
