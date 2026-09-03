#!/bin/sh
# Boot the Alpine image produced by build-alpine-x86_64.sh.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
kernel_iso=${ALPINE_ISO:-$repo_root/build/x86_64/alpine-boot.iso}
rootfs_image=${ALPINE_IMAGE:-$repo_root/build/alpine/alpine-x86_64.img}
qemu=${QEMU_SYSTEM_X86_64:-qemu-system-x86_64}

if [ ! -f "$kernel_iso" ] || [ ! -f "$rootfs_image" ]; then
    echo "alpine run: missing image(s); run scripts/build-alpine-x86_64.sh first" >&2
    exit 1
fi
command -v "$qemu" >/dev/null 2>&1 || {
    echo "alpine run: missing QEMU executable: $qemu" >&2
    exit 1
}
python3=${PYTHON3:-python3}
command -v "$python3" >/dev/null 2>&1 || {
    echo "alpine run: missing Python 3 serial proxy: $python3" >&2
    exit 1
}
serial_proxy=$repo_root/scripts/qemu-serial-proxy.py
if [ ! -f "$serial_proxy" ]; then
    echo "alpine run: missing serial proxy: $serial_proxy" >&2
    exit 1
fi

memory=${ALPINE_QEMU_MEMORY:-2G}
smp=${ALPINE_QEMU_SMP:-2}

# GRUB loads the Multiboot2 ELF from the ISO; the GPT/ext4 image is exposed
# through VirtIO PCI and is selected by root=/dev/vd0p1 in grub.cfg. Keep the
# serial endpoint on a private socket so the proxy can support an explicitly
# selected ash shell; its raw stdin forwarding preserves host Ctrl+C as guest
# byte 0x03.  The default Bash shell does not require the cursor query.
serial_base=${HITOSHIZUKU_SERIAL_TMPDIR:-${TMPDIR:-/tmp}}
if [ ! -d "$serial_base" ]; then
    echo "alpine run: serial proxy directory does not exist: $serial_base" >&2
    exit 1
fi
serial_tmp=$(mktemp -d "$serial_base/hitoshizuku-qemu.XXXXXX")
serial_socket=$serial_tmp/serial.sock
qemu_pid=
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ -n "$qemu_pid" ]; then
        kill "$qemu_pid" 2>/dev/null || true
        wait "$qemu_pid" 2>/dev/null || true
    fi
    rm -rf "$serial_tmp"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

"$qemu" \
    -machine "${ALPINE_QEMU_MACHINE:-q35}" \
    -cpu "${ALPINE_QEMU_CPU:-max}" \
    -m "$memory" \
    -smp "$smp" \
    -nographic \
    -no-reboot \
    -monitor none \
    -chardev "socket,id=serial0,path=$serial_socket,server=on,wait=on" \
    -serial chardev:serial0 \
    -boot order=d \
    -cdrom "$kernel_iso" \
    -drive "if=none,id=hitoshizuku_root,format=raw,file=$rootfs_image,readonly=off" \
    -device virtio-blk-pci,drive=hitoshizuku_root,disable-legacy=on \
    -netdev user,id=hitoshizuku_net \
    -device virtio-net-pci,netdev=hitoshizuku_net \
    ${ALPINE_QEMU_EXTRA_ARGS:-} \
    >/dev/null 2>"$serial_tmp/qemu.stderr" &
qemu_pid=$!

"$python3" "$serial_proxy" "$serial_socket"
