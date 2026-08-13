#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
output=${1:-"$root/build/qemu-plugins/riscv_instruction_mix.so"}
image=zhouzhouyi/os-contest:20260510

case "$output" in
    /*) ;;
    *) output=$PWD/$output ;;
esac
mkdir -p "$(dirname "$output")"
relative_output=$(python3 - "$root" "$output" <<'PY'
import os
import sys

root, output = map(os.path.realpath, sys.argv[1:])
relative = os.path.relpath(output, root)
if relative == ".." or relative.startswith("../"):
    raise SystemExit("plugin output must be inside the repository")
print(relative)
PY
)

docker run --rm -v "$root":/work -w /work "$image" sh -c '
    set -eu
    cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fPIC -fvisibility=hidden \
        -shared -pthread -I/opt/qemu-bin-10.0.2/include \
        $(pkg-config --cflags glib-2.0) \
        tools/qemu-plugins/riscv_instruction_mix.c \
        -o "$1" $(pkg-config --libs glib-2.0)
' sh "$relative_output"
printf '%s\n' "$output"
