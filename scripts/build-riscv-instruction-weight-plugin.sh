#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
output=${1:-"$root/build/qemu-plugins/riscv_instruction_weight.so"}
image=${RISCV_WEIGHT_CONTAINER:-zhouzhouyi/os-contest:20260510}
container_runtime=${RISCV_WEIGHT_CONTAINER_RUNTIME:-docker}
container_mount_suffix=${RISCV_WEIGHT_CONTAINER_MOUNT_SUFFIX:-}
container_run_arguments=${RISCV_WEIGHT_CONTAINER_RUN_ARGUMENTS:-}

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

case "$container_mount_suffix" in ''|:z|:Z) ;; *) exit 2 ;; esac
# shellcheck disable=SC2086
"$container_runtime" run $container_run_arguments --rm \
    -v "$root:/work$container_mount_suffix" -w /work "$image" sh -c '
    set -eu
    cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fPIC -fvisibility=hidden \
        -shared -pthread -I/opt/qemu-bin-10.0.2/include \
        $(pkg-config --cflags glib-2.0) \
        tools/qemu-plugins/riscv_instruction_weight.c \
        -o "$1" $(pkg-config --libs glib-2.0)
' sh "$relative"
printf '%s\n' "$output"
