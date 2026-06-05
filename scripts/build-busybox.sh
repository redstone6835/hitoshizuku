#!/bin/sh
# 交叉编译 busybox PIE 静态二进制，输出到 userland/rootfs-{la,rv}/bin/
#
# 用法: ./scripts/build-busybox.sh la|rv

set -e

ARCH="${1:?Usage: $0 la|rv}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/third/busybox-1.36.1"

case "$ARCH" in
    la)
        CROSS_PREFIX="loongarch64-linux-gnu-"
        DEST="$ROOT/userland/rootfs-la"
        ;;
    rv)
        CROSS_PREFIX="riscv64-linux-musl-"
        DEST="$ROOT/userland/rootfs-rv"
        ;;
    *)
        echo "用法: $0 la|rv" >&2
        exit 1
        ;;
esac

if [ ! -d "$SRC" ]; then
    echo "busybox 源码目录不存在: $SRC" >&2
    exit 2
fi
if ! command -v "${CROSS_PREFIX}gcc" >/dev/null 2>&1; then
    echo "交叉编译器不可用: ${CROSS_PREFIX}gcc" >&2
    exit 2
fi

# 从 defconfig 生成 .config
if [ ! -f "$SRC/.config" ]; then
    make -C "$SRC" CROSS_COMPILE="$CROSS_PREFIX" defconfig
fi

# 必要配置: 静态链接 + PIE（内核只支持 ET_DYN 加载）
sed -i 's/.*CONFIG_STATIC.*/CONFIG_STATIC=y/' "$SRC/.config"
sed -i 's/.*CONFIG_PIE.*/CONFIG_PIE=y/' "$SRC/.config"
sed -i 's/^CONFIG_TC=.*/# CONFIG_TC is not set/' "$SRC/.config"

# 非交互式更新配置（对新选项全部选默认值）
yes '' | make -C "$SRC" CROSS_COMPILE="$CROSS_PREFIX" oldconfig

# 编译
make -C "$SRC" CROSS_COMPILE="$CROSS_PREFIX" -j"$(nproc)"

# 安装到 userland
mkdir -p "$DEST"
make -C "$SRC" CROSS_COMPILE="$CROSS_PREFIX" CONFIG_PREFIX="$DEST" install
"${CROSS_PREFIX}strip" "$DEST/bin/busybox" 2>/dev/null || true

echo "busybox ($ARCH) → $DEST/bin/busybox ($(stat -c%s "$DEST/bin/busybox") bytes)"

# 清理构建产物
make -C "$SRC" distclean
