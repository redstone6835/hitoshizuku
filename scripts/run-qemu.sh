#!/bin/sh
# 在 QEMU 下运行 kernel。磁盘镜像由外部测试或 initramfs 工程提供。
# 串口日志默认写入 /tmp/hitoshizuku-qemu.log，同时输出到 stdout。
#
# 必须带 timeout,否则跑完 bench 进入 spin_loop 会卡住。

set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNEL="$ROOT/target/loongarch64-unknown-none/debug/kernel"
FAT_IMG="${HITOSHIZUKU_FAT_IMAGE:-$ROOT/build/fat32.img}"
EXT_IMG="${HITOSHIZUKU_EXT_IMAGE:-$ROOT/build/ext4.img}"
LOG="${HITOSHIZUKU_QEMU_LOG:-/tmp/hitoshizuku-qemu.log}"

if [ ! -f "$KERNEL" ]; then
    echo "kernel binary not found at $KERNEL; cargo build -p kernel first" >&2
    exit 2
fi
if [ ! -f "$FAT_IMG" ] || [ ! -f "$EXT_IMG" ]; then
    echo "disk images not found; set HITOSHIZUKU_FAT_IMAGE and HITOSHIZUKU_EXT_IMAGE" >&2
    exit 2
fi

# UEFI + ACPI 引导
# 两个 virtio-blk-device,id=hd0 / hd1。
TIMEOUT="${HITOSHIZUKU_QEMU_TIMEOUT:-20}"

exec timeout "${TIMEOUT}" qemu-system-loongarch64 \
    -machine virt,acpi=on -cpu la464 -m 1G -nographic \
    -serial "file:${LOG}" \
    -bios /usr/share/qemu/edk2-loongarch64-code.fd \
    -kernel "$KERNEL" \
    -drive if=none,id=hd0,file="$FAT_IMG",format=raw \
    -device virtio-blk-device,drive=hd0 \
    -drive if=none,id=hd1,file="$EXT_IMG",format=raw \
    -device virtio-blk-device,drive=hd1
