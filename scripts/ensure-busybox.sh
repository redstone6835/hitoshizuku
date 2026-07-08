#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
src_dir="$repo_root/third/busybox-1.36.1"
archive="$repo_root/third/busybox-1.36.1.tar.gz"

if [ -f "$src_dir/Makefile" ] && [ -f "$src_dir/Config.in" ]; then
    exit 0
fi

if [ ! -f "$archive" ]; then
    echo "busybox source is missing and $archive was not found" >&2
    echo "run: git submodule update --init third/busybox-1.36.1" >&2
    exit 1
fi

if [ -e "$src_dir/.git" ]; then
    status=$(git -C "$src_dir" status --porcelain 2>/dev/null || :)
    if [ -n "$status" ]; then
        echo "$src_dir is an incomplete dirty submodule checkout; refusing to overwrite it" >&2
        echo "clean it or move it aside, then rerun make" >&2
        exit 1
    fi
fi

echo "expanding busybox source from local archive"
rm -rf "$src_dir"
mkdir -p "$src_dir"
tar -xzf "$archive" -C "$src_dir"
