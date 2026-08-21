#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
output=${1:-"$root/build/qemu-plugins/profile_observer.so"}
image=${QEMU_PLUGIN_CONTAINER_IMAGE:-}
[ -n "$image" ] || {
    echo "QEMU_PLUGIN_CONTAINER_IMAGE must name a build image" >&2
    exit 2
}

case "$output" in
    /*) ;;
    *) output=$PWD/$output ;;
esac
mkdir -p "$(dirname "$output")"
relative_output=$(python3 - "$root" "$output" <<'PY'
import os, sys
root, output = map(os.path.realpath, sys.argv[1:])
relative = os.path.relpath(output, root)
if relative == ".." or relative.startswith("../"):
    raise SystemExit("plugin output must be inside the repository")
print(relative)
PY
)

docker run --rm -v "$root":/work -w /work "$image" sh -c '
    set -eu
    cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fPIC -shared \
        -I/opt/qemu-bin-10.0.2/include \
        $(pkg-config --cflags glib-2.0) \
        tools/qemu-plugins/profile_observer.c \
        -o "$1" $(pkg-config --libs glib-2.0)
' sh "$relative_output"
printf '%s\n' "$output"
