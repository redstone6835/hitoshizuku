#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
output=${1:-"$root/build/qemu-plugins/riscv_syscall_model.so"}
image=${RISCV_SYSCALL_MODEL_CONTAINER:-zhouzhouyi/os-contest:20260510}

case "$output" in
    /*) ;;
    *) output=$PWD/$output ;;
esac
case "$output" in
    "$root"/*) ;;
    *) echo "plugin output must be inside the repository" >&2; exit 2 ;;
esac
relative=${output#"$root"/}
mkdir -p "$(dirname "$output")"

docker run --rm -v "$root":/work -w /work "$image" sh -c '
    set -eu
    cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fPIC -fvisibility=hidden \
        -shared -pthread -I/opt/qemu-bin-10.0.2/include \
        $(pkg-config --cflags glib-2.0) \
        tools/qemu-plugins/riscv-syscall-model.c \
        -o "$1" $(pkg-config --libs glib-2.0)
' sh "$relative"
printf '%s\n' "$output"
