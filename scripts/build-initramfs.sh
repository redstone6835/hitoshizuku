#!/bin/sh
# 从 userland/rootfs-{la,rv}/ 构建 initramfs cpio 到 build/
#
# 用法: ./scripts/build-initramfs.sh la|rv

set -e

ARCH="${1:?Usage: $0 la|rv}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

case "$ARCH" in
    la)
        SRC="$ROOT/userland/rootfs-la"
        ;;
    rv)
        echo "riscv64 尚未实现" >&2
        exit 1
        ;;
    *)
        echo "用法: $0 la|rv" >&2
        exit 1
        ;;
esac

if [ ! -f "$SRC/bin/busybox" ]; then
    echo "busybox not found at $SRC/bin/busybox; run scripts/build-busybox.sh $ARCH first" >&2
    exit 2
fi

OUT_CPIO="$ROOT/build/initramfs.cpio"
mkdir -p "$(dirname "$OUT_CPIO")"
rm -f "$OUT_CPIO"
cd "$SRC" && find . -print0 | cpio --quiet -o -0 -H newc >| "$OUT_CPIO"

echo "created: $OUT_CPIO ($(stat -c%s "$OUT_CPIO") bytes)"
