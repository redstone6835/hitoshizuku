#!/bin/sh
# 生成 fat32.img + ext4.img(无日志),作为 QEMU virtio-blk 附加镜像。
#
# 两个镜像各 32 MiB,根目录预植若干文件供 bench 读:
#   - HELLO.TXT (短文本)
#   - README.TXT (短文本)
#   - DATA.BIN (1 MiB 随机数据,供顺序/随机读 bench)

set -e
DIR="$(dirname "$0")/../build"
mkdir -p "$DIR"

FAT_IMG="$DIR/fat32.img"
EXT_IMG="$DIR/ext4.img"
STAGE="$DIR/stage"
mkdir -p "$STAGE"

# 预置内容
echo "Hello from fatfs / extfs bench" > "$STAGE/HELLO.TXT"
echo "This is the README for the disk image" > "$STAGE/README.TXT"
# 1 MiB 的可预测数据(用零填充便于校验;要随机数据可改 /dev/urandom)
dd if=/dev/zero bs=1024 count=1024 of="$STAGE/DATA.BIN" status=none
# 注意:fatfs 的 ls 中 SFN 需要大写,生成时统一大写文件名

# ── fat32 ──────────────────────────────────────────────────────────────
dd if=/dev/zero bs=1M count=64 of="$FAT_IMG" status=none
/usr/bin/mkfs.fat -F 32 -n MYFAT "$FAT_IMG" >/dev/null
# 宿主 mtools 不一定可用,用 losetup/mount 需要 root;改用 mtools 风格
# 的 mcopy。若无 mtools,fallback 到临时挂载(需要 sudo)。
if command -v mcopy >/dev/null; then
    MTOOLS_SKIP_CHECK=1 mcopy -i "$FAT_IMG" "$STAGE/HELLO.TXT" ::/HELLO.TXT
    MTOOLS_SKIP_CHECK=1 mcopy -i "$FAT_IMG" "$STAGE/README.TXT" ::/README.TXT
    MTOOLS_SKIP_CHECK=1 mcopy -i "$FAT_IMG" "$STAGE/DATA.BIN" ::/DATA.BIN
else
    echo "WARNING: mtools(mcopy) not found; fat32.img will be EMPTY" >&2
fi

# ── ext4 (无 journal,mount 才不被拒) ───────────────────────────────────
dd if=/dev/zero bs=1M count=64 of="$EXT_IMG" status=none
# -O ^has_journal: 关闭日志(我们的驱动不支持回放)
# -O ^metadata_csum: 暂关 csum,避免 inode csum 不对(很多老 tune2fs 路径会留 0)
# -O ^64bit: 保持 32bit group desc,简化测试
# -b 4096: 块大小固定 4K
/usr/bin/mke2fs -t ext4 -F -b 4096 \
    -O ^has_journal,^metadata_csum,^64bit,^huge_file,^flex_bg \
    -L MYEXT4 \
    "$EXT_IMG" >/dev/null 2>&1

# debugfs 写入文件
if command -v debugfs >/dev/null; then
    debugfs -w "$EXT_IMG" <<EOF >/dev/null 2>&1
write $STAGE/HELLO.TXT HELLO.TXT
write $STAGE/README.TXT README.TXT
write $STAGE/DATA.BIN DATA.BIN
EOF
else
    echo "WARNING: debugfs not found; ext4.img will be EMPTY" >&2
fi

echo "built: $FAT_IMG $EXT_IMG"
ls -lh "$FAT_IMG" "$EXT_IMG"
