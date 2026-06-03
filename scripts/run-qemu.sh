#!/bin/sh
# 在 QEMU 下跑 kernel,附加 fat32.img / ext4.img 作为两个 virtio-blk-device,
# 打印 serial 到 /tmp/mygo-qemu.log 和 stdout。
#
# 必须带 timeout,否则跑完 bench 进入 spin_loop 会卡住。

set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="$ROOT/target/loongarch64-unknown-none/debug/kernel"
FAT_IMG="$ROOT/build/fat32.img"
EXT_IMG="$ROOT/build/ext4.img"
LOG="${MYGO_QEMU_LOG:-/tmp/mygo-qemu.log}"

if [ ! -f "$KERNEL" ]; then
    echo "kernel binary not found at $KERNEL; cargo build -p kernel first" >&2
    exit 2
fi
if [ ! -f "$FAT_IMG" ] || [ ! -f "$EXT_IMG" ]; then
    echo "disk images not found; run scripts/mkimg.sh first" >&2
    exit 2
fi

# UEFI + ACPI 引导
# 两个 virtio-blk-device,id=hd0 / hd1。
TIMEOUT="${MYGO_QEMU_TIMEOUT:-20}"

exec timeout "${TIMEOUT}" qemu-system-loongarch64 \
    -machine virt,acpi=on -cpu la464 -m 1G -nographic \
    -serial "file:${LOG}" \
    -bios /usr/share/qemu/edk2-loongarch64-code.fd \
    -kernel "$KERNEL" \
    -drive if=none,id=hd0,file="$FAT_IMG",format=raw \
    -device virtio-blk-device,drive=hd0 \
    -drive if=none,id=hd1,file="$EXT_IMG",format=raw \
    -device virtio-blk-device,drive=hd1
