#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cc=${CC:-cc}
command -v "$cc" >/dev/null 2>&1 || {
    echo "workload tests: SKIP (missing C compiler)"
    exit 0
}

compile_run() {
    workload=$1
    expected=$2
    mode=${3:-warm}
    mode_id=1
    [ "$mode" = cold ] && mode_id=2
    binary="$tmp/$workload-$mode"
    "$cc" -std=c11 -Wall -Wextra -Werror \
        -DBENCH_WORKLOAD="$workload" -DBENCH_MODE="$mode_id" \
        -DBENCH_WARMUP=0 -DBENCH_ROUNDS=1 -DBENCH_SAMPLES=1 \
        -I"$root/guest" "$root/guest/bench-core.c" \
        "$root/tests/workload-host.c" -o "$binary"
    output=$("$binary" linux 0 "$mode")
    printf '%s\n' "$output" | grep -F "HOST $expected" >/dev/null || {
        echo "workload $workload 未调用预期 adapter: $expected" >&2
        printf '%s\n' "$output" >&2
        exit 1
    }
    printf '%s\n' "$output" | grep -F "BENCH_META system=linux" >/dev/null
    printf '%s\n' "$output" | grep -F "mode=$mode" >/dev/null
    printf '%s\n' "$output" | grep -F "BENCH_DONE system=linux" >/dev/null
}

compile_run 1 "clock" warm
compile_run 2 "stream length=64" warm
compile_run 3 "stream length=1" warm
compile_run 4 "stream length=64" warm
compile_run 5 "stream length=256" warm
compile_run 6 "heap size=32 count=1" warm
compile_run 7 "heap size=32 count=64" warm
compile_run 8 "map size=65536 touch=0" warm
compile_run 9 "map size=1048576 touch=1" cold

echo "workload tests: PASS"
