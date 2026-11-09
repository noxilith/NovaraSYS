#!/usr/bin/env bash
# Boot the live ISO in QEMU and check that it reaches a login prompt and that root/root
# works. Needs qemu-system-x86_64; for UEFI also OVMF (set OVMF=/path/to/OVMF_CODE.fd).
#
#   scripts/test-iso-qemu.sh novara-live.iso            # BIOS
#   OVMF=/usr/share/edk2/x64/OVMF_CODE.4m.fd scripts/test-iso-qemu.sh novara-live.iso   # UEFI
#
# Drives the serial console (the ISO's command line puts a getty on ttyS0). Writes
# nothing but a log, and never touches a real disk.
set -uo pipefail
ISO=${1:?usage: test-iso-qemu.sh <iso>}
LOG=${LOG:-$(mktemp /tmp/novara-qemu-XXXXXX.log)}
TIMEOUT=${TIMEOUT:-120}
command -v qemu-system-x86_64 >/dev/null || { echo "FAIL: qemu-system-x86_64 not found" >&2; exit 1; }

fw=()
[ -n "${OVMF:-}" ] && fw=(-drive "if=pflash,format=raw,readonly=on,file=$OVMF")
kvm=()
[ -w /dev/kvm ] && kvm=(-enable-kvm -cpu host)

fifo=$(mktemp -u /tmp/novara-in-XXXXXX); mkfifo "$fifo"
cleanup() { kill "${qpid:-0}" 2>/dev/null; exec 3>&- 2>/dev/null; rm -f "$fifo"; }
trap cleanup EXIT

qemu-system-x86_64 -m 1024 "${kvm[@]}" "${fw[@]}" -cdrom "$ISO" -boot d \
    -display none -serial stdio -no-reboot <"$fifo" >"$LOG" 2>&1 &
qpid=$!
exec 3>"$fifo"

wait_for() { # pattern seconds
    for _ in $(seq 1 "$2"); do grep -q "$1" "$LOG" && return 0; sleep 1; done
    return 1
}
step() { echo "---- $1"; }

step "waiting for the login prompt (up to ${TIMEOUT}s)"
wait_for "login:" "$TIMEOUT" || { echo "FAIL: no login prompt; log: $LOG"; tail -n 30 "$LOG"; exit 1; }
echo "ok: reached a login prompt"
printf 'root\n' >&3; sleep 2; printf 'root\n' >&3; sleep 3
printf 'echo NOVARA-OK-$(uname -s); cat /etc/os-release | head -2; ls /medium/live\n' >&3
wait_for "NOVARA-OK-Linux" 20 || { echo "FAIL: logging in as root/root did not work; log: $LOG"; tail -n 20 "$LOG"; exit 1; }
echo "ok: root/root logs in and a command runs"
grep -q "rootfs.squashfs" "$LOG" && echo "ok: the medium is mounted under /medium"
echo "PASS (log: $LOG)"
