#!/bin/sh
set -eu

usage() {
    cat >&2 <<'EOF'
用法:
  run.sh --system <linux|mygo-tomori|mygo-native> --kernel <path>
         --initramfs <path> --workload <name> --mode <warm|cold> --boot <n>
         --output <dir> [--counter-hz <hz>] [--disk <path>]
         [--memory <size>] [--smp <count>] [--timeout <seconds>] [--qemu <path>]
EOF
    exit 2
}

system=
kernel=
initramfs=
disk=
workload=
mode=warm
boot=
output=
counter_hz=10000000
memory=1G
smp=1
timeout_seconds=120
qemu=qemu-system-riscv64
while [ "$#" -gt 0 ]; do
    case "$1" in
        --system) [ "$#" -ge 2 ] || usage; system=$2; shift 2 ;;
        --kernel) [ "$#" -ge 2 ] || usage; kernel=$2; shift 2 ;;
        --initramfs) [ "$#" -ge 2 ] || usage; initramfs=$2; shift 2 ;;
        --disk) [ "$#" -ge 2 ] || usage; disk=$2; shift 2 ;;
        --workload) [ "$#" -ge 2 ] || usage; workload=$2; shift 2 ;;
        --mode) [ "$#" -ge 2 ] || usage; mode=$2; shift 2 ;;
        --boot) [ "$#" -ge 2 ] || usage; boot=$2; shift 2 ;;
        --output) [ "$#" -ge 2 ] || usage; output=$2; shift 2 ;;
        --counter-hz) [ "$#" -ge 2 ] || usage; counter_hz=$2; shift 2 ;;
        --memory) [ "$#" -ge 2 ] || usage; memory=$2; shift 2 ;;
        --smp) [ "$#" -ge 2 ] || usage; smp=$2; shift 2 ;;
        --timeout) [ "$#" -ge 2 ] || usage; timeout_seconds=$2; shift 2 ;;
        --qemu) [ "$#" -ge 2 ] || usage; qemu=$2; shift 2 ;;
        *) usage ;;
    esac
done

case "$system" in linux|mygo-tomori|mygo-native) ;; *) usage ;; esac
case "$workload" in
    clock-read|stream-write|stream-write-1|stream-write-64|stream-write-256|\
        heap-small|heap-batch|map-large|page-touch) ;;
    *) usage ;;
esac
case "$mode" in warm|cold) ;; *) usage ;; esac
[ -n "$kernel" ] && [ -n "$initramfs" ] && [ -n "$boot" ] && [ -n "$output" ] || usage
for value in "$boot" "$smp" "$timeout_seconds" "$counter_hz"; do
    case "$value" in ''|*[!0-9]*) usage ;; esac
done
[ "$smp" -gt 0 ] && [ "$timeout_seconds" -gt 0 ] && [ "$counter_hz" -gt 0 ] || usage

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
tools_dir=$(CDPATH= cd -- "$script_dir/../../tools" && pwd)
mkdir -p "$output"
status_file="$output/status"
serial="$output/serial.log"
samples="$output/samples.tsv"
meta="$output/run.meta"
rm -f "$status_file" "$serial" "$samples" "$meta"

write_unavailable() {
    reason=$1
    printf 'UNAVAILABLE reason=%s\n' "$reason" | tee "$status_file" >&2
    {
        printf 'format=benchmark-run-1\n'
        printf 'system=%s\nworkload=%s\nmode=%s\nboot=%s\n' "$system" "$workload" "$mode" "$boot"
        printf 'counter=rdtime\ncounter_hz=%s\nstate=UNAVAILABLE\nreason=%s\n' \
            "$counter_hz" "$reason"
    } >"$meta"
    exit 3
}

[ -f "$kernel" ] || write_unavailable "kernel_missing"
[ -f "$initramfs" ] || write_unavailable "initramfs_missing"
[ -z "$disk" ] || [ -f "$disk" ] || write_unavailable "disk_missing"
command -v "$qemu" >/dev/null 2>&1 || write_unavailable "qemu_missing"
command -v timeout >/dev/null 2>&1 || write_unavailable "timeout_missing"
command -v sha256sum >/dev/null 2>&1 || write_unavailable "sha256sum_missing"

kernel_sha=$(sha256sum "$kernel" | awk '{print $1}')
initramfs_sha=$(sha256sum "$initramfs" | awk '{print $1}')
disk_sha=
[ -z "$disk" ] || disk_sha=$(sha256sum "$disk" | awk '{print $1}')
qemu_version=$("$qemu" --version 2>&1 | head -n 1 || true)
{
    printf '%s\n' \
        'format=benchmark-run-1' \
        "system=$system" \
        "workload=$workload" \
        "mode=$mode" \
        "boot=$boot" \
        'counter=rdtime' \
        "counter_hz=$counter_hz" \
        "memory=$memory" \
        "smp=$smp" \
        "timeout_seconds=$timeout_seconds" \
        "kernel=$kernel" \
        "kernel_sha256=$kernel_sha" \
        "initramfs=$initramfs" \
        "initramfs_sha256=$initramfs_sha" \
        "disk=$disk" \
        "disk_sha256=$disk_sha" \
        "qemu=$qemu" \
        "qemu_version=$qemu_version" \
        "started_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} >"$meta"

set -- "$qemu" -m "$memory" -nographic -smp "$smp" -no-reboot -rtc base=utc
if [ "$system" = linux ]; then
    set -- "$@" -machine virt -bios default -kernel "$kernel" -initrd "$initramfs" \
        -append 'console=ttyS0 rdinit=/init'
else
    set -- "$@" -machine virt -bios default -global virtio-mmio.force-legacy=false \
        -kernel "$kernel" -initrd "$initramfs" -append 'console=ttyS0 rdinit=/init'
fi
if [ -n "$disk" ]; then
    set -- "$@" -drive "file=$disk,if=none,format=raw,id=x0" \
        -device virtio-blk-device,drive=x0
fi
printf 'qemu_command=' >>"$meta"
printf '%s ' "$@" >>"$meta"
printf '\n' >>"$meta"

if timeout --signal=TERM "${timeout_seconds}s" "$@" >"$serial" 2>&1; then
    qemu_status=0
else
    qemu_status=$?
fi
printf 'qemu_status=%s\n' "$qemu_status" >>"$meta"

panic_status=0
if grep -Eiq '\[panic\]|Kernel panic|panicked at|not syncing' "$serial"; then
    panic_status=1
fi
poweroff_status=1
if grep -Eq 'reboot: Power down|\[syscall\]\[reboot\] poweroff requested' "$serial"; then
    poweroff_status=0
fi
printf 'panic_status=%s\npoweroff_status=%s\n' "$panic_status" "$poweroff_status" >>"$meta"

set +e
"$tools_dir/collect-samples.sh" --system "$system" --workload "$workload" \
    --mode "$mode" --boot "$boot" --counter-hz "$counter_hz" \
    --serial "$serial" --output "$samples"
collect_status=$?
set -e
printf 'collect_status=%s\n' "$collect_status" >>"$meta"

if [ "$qemu_status" -ne 0 ] || [ "$panic_status" -ne 0 ] || \
    [ "$poweroff_status" -ne 0 ] || [ "$collect_status" -ne 0 ]; then
    printf 'FAILED qemu=%s panic=%s poweroff=%s collect=%s\n' \
        "$qemu_status" "$panic_status" "$poweroff_status" "$collect_status" \
        | tee "$status_file" >&2
    exit 1
fi
printf 'READY\n' | tee "$status_file"
