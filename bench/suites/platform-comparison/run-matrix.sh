#!/bin/sh
set -eu

usage() {
    cat >&2 <<'EOF'
用法:
  run-matrix.sh --workload <name> --mode <warm|cold> --kernel-linux <path>
                --kernel-mygo <path> --artifacts <dir> --matrix-output <dir>
                [--cycles <count>] [--samples-per-round <count>]
                [--rounds <count>] [--counter-hz <hz>] [--timeout <seconds>]
                [--qemu <path>]
EOF
    exit 2
}

workload=
mode=warm
kernel_linux=
kernel_mygo=
artifacts=
matrix_output=
cycles=3
samples_per_round=1000
rounds=5
counter_hz=10000000
timeout_seconds=120
qemu=qemu-system-riscv64
while [ "$#" -gt 0 ]; do
    case "$1" in
        --workload) [ "$#" -ge 2 ] || usage; workload=$2; shift 2 ;;
        --mode) [ "$#" -ge 2 ] || usage; mode=$2; shift 2 ;;
        --kernel-linux) [ "$#" -ge 2 ] || usage; kernel_linux=$2; shift 2 ;;
        --kernel-mygo) [ "$#" -ge 2 ] || usage; kernel_mygo=$2; shift 2 ;;
        --artifacts) [ "$#" -ge 2 ] || usage; artifacts=$2; shift 2 ;;
        --matrix-output) [ "$#" -ge 2 ] || usage; matrix_output=$2; shift 2 ;;
        --cycles) [ "$#" -ge 2 ] || usage; cycles=$2; shift 2 ;;
        --samples-per-round) [ "$#" -ge 2 ] || usage; samples_per_round=$2; shift 2 ;;
        --rounds) [ "$#" -ge 2 ] || usage; rounds=$2; shift 2 ;;
        --counter-hz) [ "$#" -ge 2 ] || usage; counter_hz=$2; shift 2 ;;
        --timeout) [ "$#" -ge 2 ] || usage; timeout_seconds=$2; shift 2 ;;
        --qemu) [ "$#" -ge 2 ] || usage; qemu=$2; shift 2 ;;
        *) usage ;;
    esac
done
case "$workload" in
    clock-read|stream-write|stream-write-1|stream-write-64|stream-write-256|\
        heap-small|heap-batch|map-large|page-touch) ;;
    *) usage ;;
esac
case "$mode" in warm|cold) ;; *) usage ;; esac
[ -n "$kernel_linux" ] && [ -n "$kernel_mygo" ] && [ -n "$artifacts" ] && \
    [ -n "$matrix_output" ] || usage
for value in "$cycles" "$samples_per_round" "$rounds" "$counter_hz" "$timeout_seconds"; do
    case "$value" in ''|*[!0-9]*|0) usage ;; esac
done
if [ "$mode" = cold ] && { [ "$rounds" -ne 1 ] || [ "$samples_per_round" -ne 1 ]; }; then
    usage
fi

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
tools_dir=$(CDPATH= cd -- "$script_dir/../../tools" && pwd)
runner=${RUNNER:-"$script_dir/run.sh"}
mkdir -p "$matrix_output"
combined="$matrix_output/samples.tsv"
printf '%s\n' \
    'system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail' >"$combined"

run_one() {
    boot=$1
    system=$2
    case "$system" in
        linux) kernel=$kernel_linux ;;
        mygo-tomori|mygo-native) kernel=$kernel_mygo ;;
    esac
    initramfs="$artifacts/$system-$workload-$mode-boot-$boot.cpio"
    [ -f "$initramfs" ] || {
        echo "缺少矩阵 initramfs: $initramfs" >&2
        exit 3
    }
    output="$matrix_output/boot-$boot/$system"
    "$runner" --system "$system" --kernel "$kernel" --initramfs "$initramfs" \
        --workload "$workload" --mode "$mode" --boot "$boot" --counter-hz "$counter_hz" \
        --timeout "$timeout_seconds" --qemu "$qemu" --output "$output"
    [ "$(cat "$output/status")" = READY ] || {
        echo "矩阵运行未 READY: boot=$boot system=$system" >&2
        exit 1
    }
    tail -n +2 "$output/samples.tsv" >>"$combined"
}

boot=0
while [ "$boot" -lt "$cycles" ]; do
    case $((boot % 3)) in
        0) order='linux mygo-tomori mygo-native' ;;
        1) order='mygo-tomori mygo-native linux' ;;
        2) order='mygo-native linux mygo-tomori' ;;
    esac
    for system in $order; do
        run_one "$boot" "$system"
    done
    boot=$((boot + 1))
done

summary_output="$matrix_output/summary"
if [ -n "${SUMMARIZER:-}" ]; then
    "$SUMMARIZER" --input "$combined" --output-dir "$summary_output" \
        --systems linux,mygo-tomori,mygo-native --workloads "$workload" --modes "$mode" \
        --expected-boots "$cycles" --expected-rounds "$rounds" \
        --expected-samples-per-boot $((rounds * samples_per_round)) \
        --counter-hz "$counter_hz" --require-complete
else
    python3 "$tools_dir/summarize.py" --input "$combined" --output-dir "$summary_output" \
        --systems linux,mygo-tomori,mygo-native --workloads "$workload" --modes "$mode" \
        --expected-boots "$cycles" --expected-rounds "$rounds" \
        --expected-samples-per-boot $((rounds * samples_per_round)) \
        --counter-hz "$counter_hz" --require-complete
fi

if [ -n "${COMPARER:-}" ]; then
    "$COMPARER" --input "$summary_output/summary.tsv" \
        --output "$summary_output/comparisons.tsv" \
        --pair tomori-linux=linux,mygo-tomori \
        --pair native-tomori=mygo-tomori,mygo-native \
        --pair native-linux=linux,mygo-native
else
    python3 "$tools_dir/compare.py" --input "$summary_output/summary.tsv" \
        --output "$summary_output/comparisons.tsv" \
        --pair tomori-linux=linux,mygo-tomori \
        --pair native-tomori=mygo-tomori,mygo-native \
        --pair native-linux=linux,mygo-native
fi

printf 'READY workload=%s cycles=%s\n' "$workload" "$cycles"
