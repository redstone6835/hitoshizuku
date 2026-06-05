#!/bin/sh
# 交叉编译 lua 静态 PIE 二进制，输出到 userland/rootfs-{la,rv}/bin/
#
# 用法: ./scripts/build-lua.sh la|rv

set -e

ARCH="${1:?Usage: $0 la|rv}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/third/lua"

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
    echo "lua 源码目录不存在: $SRC" >&2
    exit 2
fi
CC="${CROSS_PREFIX}gcc"
if ! command -v "$CC" >/dev/null 2>&1; then
    echo "交叉编译器不可用: $CC" >&2
    exit 2
fi

# 静态编译；LUA_USE_POSIX 避免 -ldl 依赖
make -C "$SRC" all CC="$CC" \
    MYCFLAGS="-std=c99 -static -fPIE -DLUA_USE_POSIX" \
    MYLDFLAGS="-static" \
    MYLIBS="-lm" \
    -j"$(nproc)"

mkdir -p "$DEST/bin"
cp "$SRC/lua" "$DEST/bin/lua"
"${CROSS_PREFIX}strip" "$DEST/bin/lua" 2>/dev/null || true

echo "lua ($ARCH) → $DEST/bin/lua ($(stat -c%s "$DEST/bin/lua") bytes)"

# 清理
make -C "$SRC" clean 2>/dev/null || true
