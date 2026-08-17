#!/bin/sh
# 用法: build-probe-kernel.sh <la|rv>
set -eu
ARCH=$1
case "$ARCH" in
  la) TARGET=loongarch64; PROBE=userland/mm-probe/mm_probe_la ;;
  rv) TARGET=riscv64; PROBE=userland/mm-probe/mm_probe_rv ;;
  *) echo "bad arch"; exit 2 ;;
esac
ROOT=build/$TARGET/probe-rootfs
rm -rf "$ROOT"
cp -a build/$TARGET/busybox-rootfs "$ROOT"
cp "$PROBE" "$ROOT/mm_probe"
chmod +x "$ROOT/mm_probe"
cp userland/mm-probe/rcS.probe "$ROOT/etc/init.d/rcS"
chmod +x "$ROOT/etc/init.d/rcS"
rm -rf "$ROOT/etc/init.d/test.sh" "$ROOT/etc/init.d/judge.sh"
mkdir -p build/$TARGET
(cd "$ROOT" && find . -print0 | cpio --quiet -o -0 -H newc > "$OLDPWD/build/$TARGET/probe-initramfs.cpio")
echo "initramfs: build/$TARGET/probe-initramfs.cpio ($(stat -c%s build/$TARGET/probe-initramfs.cpio) bytes)"
