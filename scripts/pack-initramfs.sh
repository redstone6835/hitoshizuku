#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <rootfs> <output.cpio>" >&2
    exit 2
fi

rootfs=$1
output=$2

if [ ! -d "$rootfs" ]; then
    echo "initramfs root does not exist: $rootfs" >&2
    exit 1
fi

mkdir -p "$(dirname "$output")"
output=$(cd "$(dirname "$output")" && pwd)/$(basename "$output")
(cd "$rootfs" && find . -print0 | cpio --quiet -o -0 -H newc > "$output")
