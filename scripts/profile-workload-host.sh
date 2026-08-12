#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <riscv64|loongarch64> <formal|counter|sample|profile> <case-id> [smp]" >&2
    exit 2
}

[ "$#" -ge 3 ] && [ "$#" -le 4 ] || usage
arch=$1
mode=$2
case_id=$3
smp=${4:-2}
case "$arch" in riscv64|loongarch64) ;; *) usage ;; esac
case "$mode" in formal|counter|sample|profile) ;; *) usage ;; esac
case "$case_id" in ''|*[!A-Za-z0-9._-]*) usage ;; esac
case "$smp" in ''|*[!0-9]*|0) usage ;; esac
tcg_table_bits=${PROFILE_TCG_TABLE_BITS:-23}
case "$tcg_table_bits" in ''|*[!0-9]*) usage ;; esac
[ "$tcg_table_bits" -ge 12 ] && [ "$tcg_table_bits" -le 23 ] || usage

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
memory=${PROFILE_MEMORY:-8G}
accel=${PROFILE_ACCEL:-tcg,thread=multi}
run_id=$(date -u +%Y%m%dT%H%M%SZ)-$case_id-$arch-$mode-smp$smp
run_root=${PROFILE_RUN_ROOT:-$root/build/profile-runs}
output=$run_root/$run_id
mkdir -p "$output"
qemu_pid=
perf_stat_pid=
perf_record_pid=
host_perf_available=0
perf_stat_status=not_run
perf_record_status=not_run
perf_report_status=not_run

cleanup_processes() {
    for pid in $qemu_pid $perf_stat_pid $perf_record_pid; do
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
        fi
    done
    for pid in $qemu_pid $perf_stat_pid $perf_record_pid; do
        remaining=200
        while kill -0 "$pid" 2>/dev/null && [ "$remaining" -gt 0 ]; do
            remaining=$((remaining - 1))
            sleep 0.01
        done
        if kill -0 "$pid" 2>/dev/null; then
            kill -KILL "$pid" 2>/dev/null || true
        fi
        wait "$pid" 2>/dev/null || true
    done
    qemu_pid=
    perf_stat_pid=
    perf_record_pid=
}
trap cleanup_processes EXIT
trap 'cleanup_processes; exit 130' INT
trap 'cleanup_processes; exit 143' TERM

case "$arch" in
    riscv64)
        source_kernel=${PROFILE_KERNEL:-$root/kernel-rv}
        source_disk=${PROFILE_DISK:-$root/build/sdcards-final2.1/sdcard-rv-pub.img}
        guest_arch=riscv64
        qemu=qemu-system-riscv64
        machine_args="-machine virt -global virtio-mmio.force-legacy=false"
        device_args="-device virtio-blk-device,drive=x0"
        ;;
    loongarch64)
        source_kernel=${PROFILE_KERNEL:-$root/kernel-la}
        source_disk=${PROFILE_DISK:-$root/build/sdcards-final2.1/sdcard-la-pub.img}
        guest_arch=loongarch64
        qemu=qemu-system-loongarch64
        machine_args=""
        device_args="-device virtio-blk-pci,drive=x0"
        ;;
esac

[ -f "$source_kernel" ] || { echo "profile host: kernel not found: $source_kernel" >&2; exit 1; }
[ -f "$source_disk" ] || { echo "profile host: disk not found: $source_disk" >&2; exit 1; }
command -v "$qemu" >/dev/null 2>&1 || {
    echo "profile host: $qemu is unavailable" >&2
    exit 1
}

disk=$output/testdisk.img
cp --reflink=auto --sparse=always "$source_disk" "$disk"
kernel=$output/kernel.elf
cp --reflink=auto "$source_kernel" "$kernel"
kernel_sha=$(sha256sum "$kernel" | awk '{print $1}')
disk_sha=$(sha256sum "$source_disk" | awk '{print $1}')
{
    echo "format=mygo-workload-host-v1"
    echo "case_id=$case_id"
    echo "arch=$arch"
    echo "mode=$mode"
    echo "smp=$smp"
    echo "memory=$memory"
    echo "accel=$accel"
    echo "tcg_table_bits=$tcg_table_bits"
    echo "source_kernel=$source_kernel"
    echo "kernel=$kernel"
    echo "kernel_sha256=$kernel_sha"
    echo "source_disk=$source_disk"
    echo "source_disk_sha256=$disk_sha"
    echo "working_disk=$disk"
} >"$output/run.meta"
echo "workload_watchdog=disabled" >>"$output/run.meta"

serial=$output/serial.log
plugin_args=
if [ "$mode" = profile ]; then
    plugin_source=$root/tools/qemu-plugins/mygo-tcg-profile.c
    plugin_binary=$output/mygo-tcg-profile.so
    plugin_report=$output/host-tcg-profile.txt
    command -v pkg-config >/dev/null 2>&1 || {
        echo "profile host: pkg-config is required to build the QEMU plugin" >&2
        exit 1
    }
    # shellcheck disable=SC2046
    cc -std=c11 -O2 -fPIC -shared $(pkg-config --cflags glib-2.0) \
        "$plugin_source" -o "$plugin_binary"
    plugin_args="-plugin file=$plugin_binary,output=$plugin_report,table_bits=$tcg_table_bits"
fi
# 参数拆分有意依赖 shell；这里的值全部由上方固定表或受控数值产生。
# shellcheck disable=SC2086
"$qemu" $machine_args -accel "$accel" -kernel "$kernel" -m "$memory" -nographic -smp "$smp" \
    -drive "file=$disk,if=none,format=raw,id=x0" $device_args $plugin_args \
    -no-reboot -rtc base=utc \
    >"$serial" 2>&1 &
qemu_pid=$!
echo "$qemu_pid" >"$output/qemu.pid"

if [ "$mode" = profile ] && command -v perf >/dev/null 2>&1; then
    host_perf_available=1
    perf stat -x, -o "$output/host-perf.csv" \
        -e task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions,branches,branch-misses,cache-references,cache-misses \
        -p "$qemu_pid" &
    perf_stat_pid=$!
    perf record -q -F 99 -g -o "$output/host-qemu.data" -p "$qemu_pid" &
    perf_record_pid=$!
fi

printf 'timestamp_ns\trss_kb\tvmsize_kb\tread_bytes\twrite_bytes\tvoluntary_ctxt\tinvoluntary_ctxt\n' \
    >"$output/qemu-proc.tsv"
printf 'timestamp_ns\ttid\tcomm\tutime_ticks\tstime_ticks\tvoluntary_ctxt\tinvoluntary_ctxt\n' \
    >"$output/qemu-threads.tsv"
while kill -0 "$qemu_pid" 2>/dev/null; do
    now=$(date +%s%N)
    rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$qemu_pid/status" 2>/dev/null || true)
    vmsize=$(awk '/^VmSize:/ {print $2}' "/proc/$qemu_pid/status" 2>/dev/null || true)
    read_bytes=$(awk '/^read_bytes:/ {print $2}' "/proc/$qemu_pid/io" 2>/dev/null || true)
    write_bytes=$(awk '/^write_bytes:/ {print $2}' "/proc/$qemu_pid/io" 2>/dev/null || true)
    voluntary=$(awk '/^voluntary_ctxt_switches:/ {print $2}' "/proc/$qemu_pid/status" 2>/dev/null || true)
    involuntary=$(awk '/^nonvoluntary_ctxt_switches:/ {print $2}' "/proc/$qemu_pid/status" 2>/dev/null || true)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$now" "${rss:-0}" "${vmsize:-0}" \
        "${read_bytes:-0}" "${write_bytes:-0}" "${voluntary:-0}" "${involuntary:-0}" \
        >>"$output/qemu-proc.tsv"
    for task_dir in /proc/$qemu_pid/task/*; do
        [ -r "$task_dir/stat" ] || continue
        tid=${task_dir##*/}
        comm=$(sed -n 's/^Name:[[:space:]]*//p' "$task_dir/status" 2>/dev/null || true)
        cpu_ticks=$(awk '{ line=$0; sub(/^[0-9]+ \(.*\) /, "", line); split(line, fields, " "); print fields[12], fields[13] }' \
            "$task_dir/stat" 2>/dev/null || true)
        utime=${cpu_ticks%% *}
        stime=${cpu_ticks#* }
        task_voluntary=$(awk '/^voluntary_ctxt_switches:/ {print $2}' "$task_dir/status" 2>/dev/null || true)
        task_involuntary=$(awk '/^nonvoluntary_ctxt_switches:/ {print $2}' "$task_dir/status" 2>/dev/null || true)
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$now" "$tid" "${comm:-unknown}" \
            "${utime:-0}" "${stime:-0}" "${task_voluntary:-0}" "${task_involuntary:-0}" \
            >>"$output/qemu-threads.tsv"
    done
    sleep 1
done

if wait "$qemu_pid"; then qemu_status=0; else qemu_status=$?; fi
qemu_pid=
if [ -n "$perf_stat_pid" ]; then
    if wait "$perf_stat_pid" 2>/dev/null; then perf_stat_status=0; else perf_stat_status=$?; fi
fi
if [ -n "$perf_record_pid" ]; then
    if wait "$perf_record_pid" 2>/dev/null; then perf_record_status=0; else perf_record_status=$?; fi
fi
perf_stat_pid=
perf_record_pid=
echo "qemu_status=$qemu_status" >>"$output/run.meta"
echo "timed_out=0" >>"$output/run.meta"
echo "host_perf_available=$host_perf_available" >>"$output/run.meta"
echo "perf_stat_status=$perf_stat_status" >>"$output/run.meta"
echo "perf_record_status=$perf_record_status" >>"$output/run.meta"

if [ -s "$output/host-qemu.data" ]; then
    if env DEBUGINFOD_URLS= perf report --stdio --stdio-color never --no-children \
        --sort=dso,symbol -i "$output/host-qemu.data" \
        >"$output/host-qemu-report.txt" 2>"$output/host-qemu-report.err"; then
        perf_report_status=0
    else
        perf_report_status=$?
    fi
fi
echo "perf_report_status=$perf_report_status" >>"$output/run.meta"

validate_tcg_profile() {
    "$root/scripts/profile-tcg-validate.sh" "$1" "$guest_arch" "$smp"
}

host_profile_valid=1
tcg_profile_valid=1
tcg_profile_complete=1
host_perf_valid=0
if [ "$mode" = profile ]; then
    [ -s "$output/host-tcg-profile.txt" ] || host_profile_valid=0
    validate_tcg_profile "$output/host-tcg-profile.txt" || {
        tcg_profile_valid=0
        host_profile_valid=0
    }
    tcg_dropped=$(sed -n '1s/.* dropped=\([0-9][0-9]*\) .*/\1/p' \
        "$output/host-tcg-profile.txt")
    case "$tcg_dropped" in
        ''|*[!0-9]*) tcg_profile_valid=0; host_profile_valid=0 ;;
        0) ;;
        *) tcg_profile_complete=0 ;;
    esac
    [ -s "$output/qemu-proc.tsv" ] || host_profile_valid=0
    [ -s "$output/qemu-threads.tsv" ] || host_profile_valid=0
    if [ "$host_perf_available" = 1 ]; then
        if [ "$perf_stat_status" = 0 ] && [ "$perf_record_status" = 0 ] && \
            [ "$perf_report_status" = 0 ] && [ -s "$output/host-perf.csv" ] && \
            [ -s "$output/host-qemu.data" ] && [ -s "$output/host-qemu-report.txt" ]; then
            host_perf_valid=1
        else
            host_profile_valid=0
        fi
    fi
fi
echo "tcg_profile_valid=$tcg_profile_valid" >>"$output/run.meta"
echo "tcg_profile_complete=$tcg_profile_complete" >>"$output/run.meta"
echo "host_perf_valid=$host_perf_valid" >>"$output/run.meta"
echo "host_profile_valid=$host_profile_valid" >>"$output/run.meta"

if [ "$mode" != formal ]; then
    artifact_dir=/work/mygo-profile
    stem=$case_id-$guest_arch
    for suffix in bin health meta; do
        debugfs -R "dump -p $artifact_dir/$stem.$suffix $output/guest.$suffix" "$disk" \
            >"$output/debugfs-$suffix.log" 2>&1 || {
                echo "profile host: missing guest profile artifact: $stem.$suffix" >&2
                exit 1
            }
        if [ ! -s "$output/guest.$suffix" ]; then
            echo "profile host: missing guest profile artifact: $stem.$suffix" >&2
            exit 1
        fi
    done
    expected_profile_mode=sample
    [ "$mode" = counter ] && expected_profile_mode=counter
    grep -q "^profile_mode=$expected_profile_mode$" "$output/guest.meta" || {
        echo "profile host: guest profile mode does not match host mode" >&2
        exit 1
    }
    grep -q "^case_id=$case_id$" "$output/guest.meta" || {
        echo "profile host: guest case id does not match host case" >&2
        exit 1
    }
    grep -q "^arch=$guest_arch$" "$output/guest.meta" || {
        echo "profile host: guest architecture does not match host architecture" >&2
        exit 1
    }
    guest_active_cpus=$(sed -n 's/^cpus=//p' "$output/guest.meta")
    case "$guest_active_cpus" in ''|*[!0-9]*|0)
        echo "profile host: guest online CPU count is invalid" >&2
        exit 1
        ;;
    esac
    echo "guest_active_cpus=$guest_active_cpus" >>"$output/run.meta"
    image_root=${PROFILE_IMAGE_ROOT:-$root/build/$arch/compat-rootfs}
    if [ -n "${PROFILE_PHASE_MAP:-}" ]; then
        cp "$PROFILE_PHASE_MAP" "$output/phase-map.tsv"
    fi
    set -- "$root/scripts/profile-snapshot-analyze.py" "$output/guest.bin" \
        --output "$output/analysis" --kernel-elf "$kernel" \
        --host-perf "$output/host-perf.csv" --tcg-profile "$output/host-tcg-profile.txt" \
        --health "$output/guest.health"
    [ -d "$image_root" ] && set -- "$@" --image-root "$image_root"
    command -v guestfish >/dev/null 2>&1 && set -- "$@" --disk-image "$disk"
    [ -n "${PROFILE_PHASE_MAP:-}" ] && set -- "$@" --phase-map "$output/phase-map.tsv"
    "$@"
fi

if [ "$mode" = profile ] && [ "$host_profile_valid" != 1 ]; then
    echo "profile host: host QEMU profiling is incomplete" >&2
    exit 1
fi

echo "profile host: run artifacts: $output"
exit "$qemu_status"
