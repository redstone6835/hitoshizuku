#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
image=${QEMU_PLUGIN_CONTAINER_IMAGE:-zhouzhouyi/os-contest:20260510}

docker run --rm -v "$root":/work:ro -w /work "$image" sh -c '
    set -eu
    scratch=$(mktemp -d)
    trap '\''rm -rf "$scratch"'\'' EXIT INT TERM
    plugin=$scratch/buildstorm-observer.so

    cc -std=c11 -O2 -g0 -Wall -Wextra -Werror -fPIC -shared \
        -I/opt/qemu-bin-10.0.2/include \
        $(pkg-config --cflags glib-2.0) \
        tools/qemu-plugins/buildstorm_observer.c \
        -o "$plugin" $(pkg-config --libs glib-2.0)

    run_target() {
        target=$1
        qemu=qemu-system-$target
        summary=$scratch/$target-summary.json
        histogram=$scratch/$target-histogram.json
        log=$scratch/$target.log
        bios_args=
        [ "$target" != riscv64 ] || bios_args="-bios default"

        set +e
        timeout -s TERM 1 "$qemu" -machine virt -m 128M -smp 2 \
            -display none -monitor none -serial none $bios_args \
            -plugin "$plugin,socket=$scratch/missing.sock,period=1000,stack-bytes=0,summary=$summary,histogram=$histogram" \
            >"$log" 2>&1
        status=$?
        set -e
        case "$status" in 0|124|143) ;; *) cat "$log" >&2; exit 1 ;; esac
        test -s "$summary" || { cat "$log" >&2; exit 1; }
        python3 - "$summary" <<'\''PY'\''
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert data["schema"] == "mygo.qemu-observer-plugin.v1"
assert data["counter_granularity"] == "translation-block"
assert len(data["vcpus"]) == 2
assert all(row["total"] == row["user"] + row["kernel"] for row in data["vcpus"])
assert sum(row["total"] for row in data["vcpus"]) > 0
PY
    }

    run_target riscv64
    run_target loongarch64
'

echo "buildstorm observer smoke: ok"
