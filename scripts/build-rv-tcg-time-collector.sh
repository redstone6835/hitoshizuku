#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
output=${1:-"$root/build/tools/rv_tcg_time_collect"}
compiler=${CC:-cc}

case "$output" in
    /*) ;;
    *) output=$PWD/$output ;;
esac

mkdir -p "$(dirname "$output")"
"$compiler" -std=c11 -O2 -g -Wall -Wextra -Werror \
    "$root/tools/perf/rv_tcg_time_collect.c" -o "$output"
printf '%s\n' "$output"
