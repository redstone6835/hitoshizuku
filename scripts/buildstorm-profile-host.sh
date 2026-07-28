#!/bin/sh
# Run one cold BuildStorm profiling window under the contest QEMU.
set -eu

extract_progress() {
    tr '\r' '\n' | awk '
    BEGIN { maximum = 0 }
    {
        line = $0
        while (match(line, /[0-9][0-9]*\/446/)) {
            value = substr(line, RSTART, RLENGTH)
            sub(/\/446$/, "", value)
            suffix = substr(line, RSTART + RLENGTH, 1)
            if (suffix !~ /[0-9]/ && value + 0 <= 446) {
                if (value + 0 > maximum) maximum = value + 0
                found = 1
            }
            line = substr(line, RSTART + RLENGTH)
        }
    }
    END { if (found) print maximum }
    '
}

if [ "${1:-}" = "--extract-progress" ]; then
    extract_progress
    exit 0
fi

if [ "${1:-}" = "--extract-progress-after" ]; then
    [ "$#" -eq 2 ] && [ -n "$2" ] || exit 2
    awk -v marker="$2" 'seen { print } index($0, marker) { seen = 1 }' | extract_progress
    exit 0
fi

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
duration_arg=${1:-0}
if [ -n "${PROFILE_DURATION_MS:-}" ]; then
    duration_ms=$PROFILE_DURATION_MS
else
    duration_ms=$(awk -v value="$duration_arg" \
        'BEGIN { if (value !~ /^[0-9]+([.][0-9]+)?$/ || value < 0) exit 1; printf "%.0f\n", value * 1000 }') || {
        echo "usage: $0 [non-negative-seconds]" >&2
        exit 2
    }
fi
case "$duration_ms" in
    ''|*[!0-9]*) echo "PROFILE_DURATION_MS must be a non-negative integer" >&2; exit 2 ;;
esac

warmup_ms=${PROFILE_WARMUP_MS:-0}
stage_timeout_ms=${PROFILE_STAGE_TIMEOUT_MS:-0}
boot_timeout_ms=${PROFILE_BOOT_TIMEOUT_MS:-0}
done_timeout_ms=${PROFILE_DONE_TIMEOUT_MS:-0}
capture_start_timeout_ms=${PROFILE_CAPTURE_START_TIMEOUT_MS:-0}
controller_timeout_ms=${PROFILE_CONTROLLER_TIMEOUT_MS:-0}
sample_ms=${PROFILE_HOST_SAMPLE_MS:-1000}
poll_ms=${PROFILE_POLL_MS:-50}
anchor=${PROFILE_STAGE_ANCHOR:-workload}
cpuset=${PROFILE_CPUSET:-}
for pair in \
    "PROFILE_WARMUP_MS:$warmup_ms" \
    "PROFILE_STAGE_TIMEOUT_MS:$stage_timeout_ms" \
    "PROFILE_BOOT_TIMEOUT_MS:$boot_timeout_ms" \
    "PROFILE_DONE_TIMEOUT_MS:$done_timeout_ms" \
    "PROFILE_CAPTURE_START_TIMEOUT_MS:$capture_start_timeout_ms" \
    "PROFILE_CONTROLLER_TIMEOUT_MS:$controller_timeout_ms" \
    "PROFILE_HOST_SAMPLE_MS:$sample_ms" \
    "PROFILE_POLL_MS:$poll_ms"
do
    name=${pair%%:*}
    value=${pair#*:}
    case "$value" in ''|*[!0-9]*) echo "$name must be a non-negative integer" >&2; exit 2 ;; esac
done
[ "$sample_ms" -gt 0 ] || { echo "PROFILE_HOST_SAMPLE_MS must be positive" >&2; exit 2; }
[ "$poll_ms" -gt 0 ] || { echo "PROFILE_POLL_MS must be positive" >&2; exit 2; }
case "$cpuset" in *[!0-9,-]*) echo "PROFILE_CPUSET has invalid syntax" >&2; exit 2 ;; esac
case "$anchor" in
    workload|aws-object) ;;
    cargo:*)
        anchor_progress=${anchor#cargo:}
        case "$anchor_progress" in ''|*[!0-9]*) echo "invalid cargo stage anchor: $anchor" >&2; exit 2 ;; esac
        [ "$anchor_progress" -le 446 ] || { echo "invalid cargo stage anchor: $anchor" >&2; exit 2; }
        ;;
    marker:*)
        anchor_marker=${anchor#marker:}
        [ -n "$anchor_marker" ] || { echo "empty marker stage anchor" >&2; exit 2; }
        printf '%s\n' "$anchor_marker" | LC_ALL=C awk '
            NR == 1 && length($0) <= 128 && $0 !~ /[[:cntrl:]]/ { ok = 1 }
            NR != 1 { bad = 1 }
            END { exit !(ok && !bad) }
        ' || { echo "marker stage anchor must be at most 128 printable bytes" >&2; exit 2; }
        ;;
    *) echo "PROFILE_STAGE_ANCHOR must be workload, aws-object, cargo:N, or marker:TEXT" >&2; exit 2 ;;
esac

boot_mode=${PROFILE_BOOT_MODE:-mygo}
case "$boot_mode" in
    mygo)
        default_kernel=$repo/kernel-la
        default_map=
        default_capture=1
        workload_device=/dev/vd0
        tools_device=/dev/vd1
        ;;
    linux)
        default_kernel=$repo/build/linux/vmlinux
        default_map=$repo/build/linux/System.map
        default_capture=0
        workload_device=/dev/vda
        tools_device=/dev/vdb
        ;;
    *) echo "PROFILE_BOOT_MODE must be mygo or linux" >&2; exit 2 ;;
esac
kernel=${PROFILE_KERNEL:-"$default_kernel"}
linux_initramfs=${PROFILE_LINUX_INITRAMFS:-"$repo/build/loongarch64/compat-initramfs.cpio"}
base=${PROFILE_BASE_IMAGE:-"$repo/../oskernel2026-mygo-network-cagent/build/sdcard-la-pub.img"}
container_image=${PROFILE_CONTAINER_IMAGE:-zhouzhouyi/os-contest:20260510}
label=${PROFILE_LABEL:-"${boot_mode}-host${duration_ms}ms"}
observer_enabled=${PROFILE_QEMU_OBSERVER:-0}
observer_system=${PROFILE_SYSTEM:-$boot_mode}
observer_plugin=${PROFILE_QEMU_PLUGIN:-"$repo/build/qemu-plugins/buildstorm_observer.so"}
observer_map=${PROFILE_SYMBOL_MAP:-"$default_map"}
observer_manifest=${PROFILE_SYMBOL_MANIFEST:-"$observer_map.manifest"}
observer_period=${PROFILE_PLUGIN_PERIOD_INSNS:-50000000}
observer_stack_bytes=${PROFILE_PLUGIN_STACK_BYTES:-1024}
observer_histogram=${PROFILE_HISTOGRAM:-1}
observer_proc_ms=${PROFILE_OBSERVER_PROC_MS:-1000}
observer_require_valid=${PROFILE_OBSERVER_REQUIRE_VALID:-1}
observer_require_manifest=${PROFILE_REQUIRE_SYMBOL_MANIFEST:-1}
sampling=${PROFILE_SAMPLING:-0}
trace_enabled=${PROFILE_TRACE_ENABLED:-0}
timing_shift=${PROFILE_TIMING_SHIFT:-8}
timing_sampler=${PROFILE_TIMING_SAMPLER:-hashed-bernoulli-v1}
capture=${PROFILE_CAPTURE:-$default_capture}
event_mask=${PROFILE_EVENT_MASK:-0xfef000000}
case "$sampling:$trace_enabled" in
    0:0|0:1|1:0|1:1) ;;
    *) echo "PROFILE_SAMPLING and PROFILE_TRACE_ENABLED must be 0 or 1" >&2; exit 2 ;;
esac
case "$capture" in 0|1) ;; *) echo "PROFILE_CAPTURE must be 0 or 1" >&2; exit 2 ;; esac
if [ "$boot_mode" = linux ] && [ "$capture" -ne 0 ]; then
    echo "PROFILE_CAPTURE must be 0 for a Linux guest" >&2
    exit 2
fi
case "$observer_enabled:$observer_require_valid" in
    0:0|0:1|1:0|1:1) ;;
    *) echo "PROFILE_QEMU_OBSERVER and PROFILE_OBSERVER_REQUIRE_VALID must be 0 or 1" >&2; exit 2 ;;
esac
case "$observer_require_manifest" in
    0|1) ;;
    *) echo "PROFILE_REQUIRE_SYMBOL_MANIFEST must be 0 or 1" >&2; exit 2 ;;
esac
case "$observer_system" in
    ''|*[!A-Za-z0-9_.-]*) echo "PROFILE_SYSTEM has invalid syntax" >&2; exit 2 ;;
esac
for pair in \
    "PROFILE_PLUGIN_PERIOD_INSNS:$observer_period" \
    "PROFILE_OBSERVER_PROC_MS:$observer_proc_ms" \
    "PROFILE_PLUGIN_STACK_BYTES:$observer_stack_bytes"
do
    name=${pair%%:*}
    value=${pair#*:}
    case "$value" in ''|*[!0-9]*) echo "$name must be a non-negative integer" >&2; exit 2 ;; esac
done
[ "$observer_period" -ge 1000 ] || { echo "PROFILE_PLUGIN_PERIOD_INSNS must be at least 1000" >&2; exit 2; }
[ "$observer_proc_ms" -gt 0 ] || { echo "PROFILE_OBSERVER_PROC_MS must be positive" >&2; exit 2; }
[ "$observer_stack_bytes" -le 4096 ] && [ $((observer_stack_bytes % 8)) -eq 0 ] || {
    echo "PROFILE_PLUGIN_STACK_BYTES must be a multiple of 8 up to 4096" >&2
    exit 2
}
case "$timing_shift" in
    ''|*[!0-9]*) echo "PROFILE_TIMING_SHIFT must be an integer from 0 to 16" >&2; exit 2 ;;
esac
[ "$timing_shift" -le 16 ] || { echo "PROFILE_TIMING_SHIFT must be an integer from 0 to 16" >&2; exit 2; }
case "$timing_sampler" in
    ''|*[!A-Za-z0-9_.-]*) echo "PROFILE_TIMING_SAMPLER has invalid syntax" >&2; exit 2 ;;
esac
printf '%s\n' "$event_mask" | LC_ALL=C awk '
    NR == 1 && /^0x[0-9A-Fa-f]+$/ && length($0) >= 3 && length($0) <= 18 { ok = 1 }
    NR != 1 { bad = 1 }
    END { exit !(ok && !bad) }
' || { echo "PROFILE_EVENT_MASK must be a 1-16 digit hexadecimal mask with a 0x prefix" >&2; exit 2; }

test -r "$kernel" || { echo "profile host: missing kernel: $kernel" >&2; exit 1; }
if [ "$boot_mode" = linux ] || [ "$observer_enabled" -eq 1 ]; then
    test -r "$linux_initramfs" || {
        echo "profile host: missing shared initramfs: $linux_initramfs" >&2
        exit 1
    }
fi
test -r "$base" || { echo "profile host: missing base image: $base" >&2; exit 1; }
if [ "$observer_enabled" -eq 1 ]; then
    test -r "$observer_plugin" || { echo "profile host: missing QEMU plugin: $observer_plugin" >&2; exit 1; }
    test -r "$observer_map" || { echo "profile host: missing symbol map: ${observer_map:-unset}" >&2; exit 1; }
    if [ "$observer_require_manifest" -eq 1 ]; then
        test -r "$observer_manifest" || {
            echo "profile host: missing kernel/map manifest: ${observer_manifest:-unset}" >&2
            exit 1
        }
    fi
fi
for command in docker id ln mkfs.ext4 socat timeout python3 sha256sum setsid sudo; do
    command -v "$command" >/dev/null 2>&1 || { echo "profile host: $command is required" >&2; exit 1; }
done

monotonic_ns() {
    python3 -c 'import time; print(time.monotonic_ns())'
}

sleep_ms() {
    python3 - "$1" <<'PY'
import sys, time
time.sleep(int(sys.argv[1]) / 1000)
PY
}

safe_label=$(printf '%s' "$label" | tr -c 'A-Za-z0-9_.-' '-')
[ -n "$safe_label" ] && [ "${#safe_label}" -le 64 ] || {
    echo "PROFILE_LABEL must yield 1-64 safe characters" >&2
    exit 2
}
run_dir=$(mktemp -d "${PROFILE_RUN_ROOT:-/tmp}/buildstorm-profile-${safe_label}.XXXXXX")
stage=$(mktemp -d "${PROFILE_RUN_ROOT:-/tmp}/buildstorm-profile-tools.XXXXXX")
run_dir=$(CDPATH= cd -- "$run_dir" && pwd)
stage=$(CDPATH= cd -- "$stage" && pwd)
container="mygo-profile-$$"
run_token="p$$_$(monotonic_ns)"
workload_log_line=
workload_log_offset=
logger_pid=
qemu_pid=
observer_pid=
runtime_socket_dir=
runtime_socket_root=$run_dir
observer_quality_valid=1
normal_exit=0

runtime_socket_dir=$(mktemp -d /tmp/mygo-qemu-runtime.XXXXXX)
ln -s "$run_dir" "$runtime_socket_dir/run"
runtime_socket_root=$runtime_socket_dir/run

host_process_group_alive() {
    LC_ALL=C awk -v target="$1" '
        {
            line = $0
            sub(/^[0-9]+ \(.*\) /, "", line)
            split(line, field, " ")
            if (field[3] == target && field[1] != "Z") found = 1
        }
        END { exit !found }
    ' /proc/[0-9]*/stat 2>/dev/null
}

cleanup() {
    if [ -n "$observer_pid" ]; then
        if [ -S "$runtime_socket_root/qemu-observer-control.sock" ]; then
            timeout 10 python3 "$repo/scripts/qemu_profile_daemon.py" ctl \
                --socket "$runtime_socket_root/qemu-observer-control.sock" shutdown \
                >/dev/null 2>&1 || true
        fi
        kill -TERM "$observer_pid" >/dev/null 2>&1 || true
        observer_wait=0
        while kill -0 "$observer_pid" >/dev/null 2>&1 && [ "$observer_wait" -lt 100 ]; do
            observer_wait=$((observer_wait + 1))
            sleep 0.01
        done
        kill -KILL "$observer_pid" >/dev/null 2>&1 || true
        wait "$observer_pid" 2>/dev/null || true
        observer_pid=
    fi
    timeout 10 docker stop -t 1 "$container" >/dev/null 2>&1 || true
    timeout 10 docker rm -f "$container" >/dev/null 2>&1 || true
    if [ -n "$logger_pid" ]; then
        kill -TERM "-$logger_pid" >/dev/null 2>&1 || true
        cleanup_wait=0
        while host_process_group_alive "$logger_pid" && [ "$cleanup_wait" -lt 100 ]; do
            cleanup_wait=$((cleanup_wait + 1))
            sleep 0.01
        done
        if host_process_group_alive "$logger_pid"; then
            kill -KILL "-$logger_pid" >/dev/null 2>&1 || true
        fi
        wait "$logger_pid" 2>/dev/null || true
    fi
    rm -f "$stage/profile-capture.sh" "$stage/run.sh" "$stage/config.env"
    rmdir "$stage" 2>/dev/null || true
    if [ -n "$runtime_socket_dir" ]; then
        rm -f "$runtime_socket_dir/run"
        rmdir "$runtime_socket_dir" 2>/dev/null || true
        runtime_socket_dir=
    fi
    if [ "$normal_exit" -ne 1 ]; then
        echo "profile host: incomplete run retained at $run_dir" >&2
    fi
}
trap cleanup EXIT INT TERM

send_line() {
    # The first byte sent immediately after entering the interactive shell can
    # be consumed by the console transition. Leading spaces make that loss inert.
    line="  $1"
    printf '%s\n' "$line" | LC_ALL=C awk 'NR == 1 && length($0) <= 256 { ok = 1 } NR != 1 { bad = 1 } END { exit !(ok && !bad) }' || {
        echo "profile host: refusing unsafe or oversized serial command" >&2
        return 2
    }
    timeout 2 sh -c 'printf "%s\n" "$1" >"$2"' sh "$line" "$run_dir/serial.in"
}

deadline_after_ms() {
    [ "$1" -eq 0 ] && return 0
    now=$(monotonic_ns)
    printf '%s\n' "$((now + $1 * 1000000))"
}

deadline_expired() {
    [ -n "$1" ] && [ "$(monotonic_ns)" -ge "$1" ]
}

wait_for_fixed() {
    needle=$1
    timeout_ms=$2
    deadline=$(deadline_after_ms "$timeout_ms")
    while ! grep -Fq "$needle" "$run_dir/profile.serial.log" 2>/dev/null; do
        deadline_expired "$deadline" && return 1
        sleep_ms 20
    done
}

report_controller_status() {
    send_line "/tmp/p/run.sh d $run_token" >/dev/null 2>&1 || return 0
}

workload_finished() {
    grep -q "@@PROFILE_WORKLOAD_EXIT .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null
}

current_progress() {
    [ -n "$workload_log_offset" ] || return 0
    tail -c "+$((workload_log_offset + 1))" "$run_dir/profile.serial.log" 2>/dev/null |
        tail -c 131072 | extract_progress || true
}

serial_after_workload_has() {
    [ -n "$workload_log_offset" ] || return 1
    tail -c "+$((workload_log_offset + 1))" "$run_dir/profile.serial.log" 2>/dev/null |
        grep -Fq "$1"
}

last_progress=-1
record_progress() {
    progress=$(current_progress)
    [ -n "$progress" ] || return 0
    [ "$progress" -gt "$last_progress" ] || return 0
    last_progress=$progress
    stamp=$(monotonic_ns)
    for milestone in 0 64 128 256 384 440 446; do
        if [ "$progress" -ge "$milestone" ] && [ ! -e "$run_dir/progress-$milestone" ]; then
            : >"$run_dir/progress-$milestone"
            printf '%s\t%s\n' "$milestone" "$stamp" >>"$run_dir/progress.tsv"
        fi
    done
}

psi_field() {
    file=$1
    kind=$2
    key=$3
    awk -v kind="$kind" -v key="$key" '$1 == kind { for (i=2;i<=NF;i++) { split($i,a,"="); if (a[1] == key) { print a[2]; exit } } }' "$file" 2>/dev/null || true
}

qemu_cpu_value() {
    qcpu="0 0"
    if [ -n "$qemu_pid" ] && [ -r "/proc/$qemu_pid/stat" ]; then
        qstat=$(cat "/proc/$qemu_pid/stat" 2>/dev/null || true)
        qrest=${qstat#*) }
        if [ "$qrest" != "$qstat" ]; then
            set -- $qrest
            [ "$#" -ge 13 ] && qcpu="${12} ${13}"
        fi
    fi
    printf '%s\n' "$qcpu"
}

sample_qemu_boundary() {
    phase=$1
    stamp=$2
    set -- $(qemu_cpu_value)
    printf '%s\t%s\t%s\t%s\n' "$stamp" "$phase" "$1" "$2" >>"$run_dir/qemu-cpu-boundaries.tsv"
}

sample_host() {
    phase=$1
    progress=$(current_progress)
    [ -n "$progress" ] || progress=-1
    stamp=$(monotonic_ns)
    set -- $(cat /proc/loadavg)
    load1=$1 load5=$2 load15=$3 runnable=$4 lastpid=$5
    set -- $(qemu_cpu_value)
    q_utime=$1 q_stime=$2
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s' \
        "$stamp" "$phase" "$progress" "$q_utime" "$q_stime" \
        "$load1" "$load5" "$load15" "$runnable" "$lastpid" >>"$run_dir/host-samples.tsv"
    for spec in cpu:some:avg10 cpu:some:total io:some:avg10 io:some:total io:full:avg10 io:full:total memory:some:avg10 memory:some:total memory:full:avg10 memory:full:total; do
        file=${spec%%:*}; tail=${spec#*:}; kind=${tail%%:*}; key=${tail#*:}
        value=$(psi_field "/proc/pressure/$file" "$kind" "$key")
        printf '\t%s' "${value:-NA}" >>"$run_dir/host-samples.tsv"
    done
    printf '\n' >>"$run_dir/host-samples.tsv"
}

printf 'milestone\tmonotonic_ns\n' >"$run_dir/progress.tsv"
printf 'monotonic_ns\tphase\tprogress\tqemu_utime_ticks\tqemu_stime_ticks\tload1\tload5\tload15\trunnable_total\tlast_pid\tcpu_some_avg10\tcpu_some_total\tio_some_avg10\tio_some_total\tio_full_avg10\tio_full_total\tmemory_some_avg10\tmemory_some_total\tmemory_full_avg10\tmemory_full_total\n' >"$run_dir/host-samples.tsv"
printf 'monotonic_ns\tphase\tqemu_utime_ticks\tqemu_stime_ticks\n' >"$run_dir/qemu-cpu-boundaries.tsv"

cp "$kernel" "$run_dir/kernel-la"
kernel_id=$(sha256sum "$run_dir/kernel-la" | awk '{print $1}')
if [ -r "$linux_initramfs" ]; then
    initramfs_id=$(sha256sum "$linux_initramfs" | awk '{print $1}')
else
    initramfs_id=unavailable
fi
if [ "$boot_mode" = linux ]; then
    cp "$linux_initramfs" "$run_dir/linux-initramfs.cpio"
    [ "$(sha256sum "$run_dir/linux-initramfs.cpio" | awk '{print $1}')" = "$initramfs_id" ] || {
        echo "profile host: Linux initramfs changed while it was copied" >&2
        exit 1
    }
fi
base_id=$(sha256sum "$base" | awk '{print $1}')
cp "$repo/scripts/profile-capture.sh" "$stage/profile-capture.sh"
cp "$repo/scripts/buildstorm-profile-guest.sh" "$stage/run.sh"
cat >"$run_dir/workload-plan.txt" <<'EOF'
schema=mygo.buildstorm-workload.v1
command=cargo build -p tg-xtask
cwd=/work/tgoskits
target=/work/tgoskits/target
target_setup=remove-and-mount-tmpfs:size=5G
network=offline
EOF
workload_plan_id=$(sha256sum "$run_dir/workload-plan.txt" | awk '{print $1}')
workload_script_id=$(sha256sum "$stage/run.sh" | awk '{print $1}')
if [ "$observer_enabled" -eq 1 ]; then
    cp "$observer_plugin" "$run_dir/qemu-observer-plugin.so"
    cp "$observer_map" "$run_dir/kernel.map"
    plugin_id=$(sha256sum "$run_dir/qemu-observer-plugin.so" | awk '{print $1}')
    if [ -r "$observer_manifest" ]; then
        cp "$observer_manifest" "$run_dir/kernel.map.manifest"
        manifest_target=$(PYTHONPATH="$repo" python3 - "$run_dir/kernel.map.manifest" <<'PY'
import pathlib
import sys

from scripts.qemu_profile_daemon import load_kernel_map_manifest

print(load_kernel_map_manifest(pathlib.Path(sys.argv[1])).target)
PY
        )
        manifest_id=$(sha256sum "$run_dir/kernel.map.manifest" | awk '{print $1}')
    else
        manifest_target=unverified
        manifest_id=unavailable
    fi
else
    plugin_id=unavailable
    manifest_target=unavailable
    manifest_id=unavailable
fi
{
    printf 'export PROFILE_BOOT_MODE=%s\n' "$boot_mode"
    printf 'export PROFILE_CAPTURE=%s\n' "$capture"
    printf 'export PROFILE_EVENT_MASK=%s\n' "$event_mask"
    printf 'export PROFILE_SAMPLING=%s\n' "$sampling"
    printf 'export PROFILE_TRACE_ENABLED=%s\n' "$trace_enabled"
    printf 'export PROFILE_TIMING_SHIFT=%s\n' "$timing_shift"
    printf 'export PROFILE_TIMING_SAMPLER=%s\n' "$timing_sampler"
    printf 'export PROFILE_KERNEL_IMAGE_ID=%s\n' "$kernel_id"
    printf 'export PROFILE_ROOTFS_IMAGE_ID=%s\n' "$base_id"
    printf 'export PROFILE_WORKLOAD=%s\n' "$safe_label"
    printf 'export PROFILE_RUN_TOKEN=%s\n' "$run_token"
    printf 'export PROFILE_TOOL_MOUNT=/tmp/p\n'
} >"$stage/config.env"
chmod 0755 "$stage/profile-capture.sh" "$stage/run.sh"
truncate -s 16M "$run_dir/tools.ext4"
mkfs.ext4 -q -d "$stage" "$run_dir/tools.ext4"

qemu_version=$(timeout 30 docker run --rm "$container_image" qemu-system-loongarch64 --version | head -n 1 | tr '\t\r\n' '   ')
container_image_id=$(timeout 10 docker image inspect --format '{{.Id}}' "$container_image")
host_uid=$(id -u)
host_gid=$(id -g)
case "$container_image_id" in
    sha256:*) container_digest=${container_image_id#sha256:} ;;
    *) echo "profile host: invalid container image identity: $container_image_id" >&2; exit 1 ;;
esac
case "$container_digest" in
    *[!0-9a-f]*|'') echo "profile host: invalid container image identity: $container_image_id" >&2; exit 1 ;;
esac
[ "${#container_digest}" -eq 64 ] || {
    echo "profile host: invalid container image identity: $container_image_id" >&2
    exit 1
}
if [ -n "$cpuset" ]; then cpuset_identity=$cpuset; else cpuset_identity=unrestricted; fi
clock_ticks=$(getconf CLK_TCK 2>/dev/null || echo 100)
{
    printf 'kernel_sha256=%s\nbase_sha256=%s\n' "$kernel_id" "$base_id"
    printf 'qemu_version=%s\ncontainer_image=%s\ncpuset=%s\n' "$qemu_version" "$container_image" "$cpuset"
    printf 'duration_ms=%s\nwarmup_ms=%s\nstage_anchor=%s\n' "$duration_ms" "$warmup_ms" "$anchor"
    printf 'capture_enabled=%s\n' "$capture"
    printf 'event_mask=%s\nsampling_enabled=%s\ntrace_enabled=%s\ntiming_shift=%s\ntiming_sampler=%s\n' \
        "$event_mask" "$sampling" "$trace_enabled" "$timing_shift" "$timing_sampler"
    printf 'poll_ms=%s\nhost_sample_ms=%s\n' "$poll_ms" "$sample_ms"
    printf 'host_clock_ticks_per_second=%s\n' "$clock_ticks"
    printf 'qemu_observer_enabled=%s\nobserver_system=%s\n' "$observer_enabled" "$observer_system"
    printf 'guest_boot_mode=%s\nguest_initramfs_sha256=%s\n' "$boot_mode" "$initramfs_id"
    printf 'guest_workload_device=%s\nguest_tools_device=%s\n' "$workload_device" "$tools_device"
    printf 'qemu_machine=virt\nqemu_cpu=la464\nqemu_accel=tcg,thread=multi\n'
    printf 'qemu_name=buildstorm-profile\nqemu_debug_threads=on\n'
    printf 'memory_bytes=8589934592\nsmp=8\ncpuset_identity=%s\n' "$cpuset_identity"
    printf 'target_tmpfs=size=5G\ncold_target=true\ntoolchain=nightly-2026-05-28\n'
    printf 'container_image_id=%s\nworkload_plan_sha256=%s\nworkload_script_sha256=%s\n' \
        "$container_image_id" "$workload_plan_id" "$workload_script_id"
    printf 'plugin_sha256=%s\nplugin_period_insns=%s\nplugin_stack_bytes=%s\nobserver_proc_ms=%s\n' \
        "$plugin_id" "$observer_period" "$observer_stack_bytes" "$observer_proc_ms"
    printf 'container_user=%s:%s\n' "$host_uid" "$host_gid"
    printf 'symbol_manifest_required=%s\nsymbol_manifest_target=%s\nsymbol_manifest_sha256=%s\n' \
        "$observer_require_manifest" "$manifest_target" "$manifest_id"
} >"$run_dir/metadata.env"

base_dir=$(dirname "$base")
base_name=$(basename "$base")
set -- docker run --rm --user "$host_uid:$host_gid"
[ -z "$cpuset" ] || set -- "$@" --cpuset-cpus "$cpuset"
set -- "$@" -v "$run_dir":/run -v "$base_dir":/base:ro "$container_image" \
    qemu-img create -f qcow2 -F raw -b "/base/$base_name" /run/run.qcow2
timeout 60 "$@" >/dev/null

mkfifo "$run_dir/serial.in"
set -- docker run -d --name "$container" --user "$host_uid:$host_gid"
[ -z "$cpuset" ] || set -- "$@" --cpuset-cpus "$cpuset"
set -- "$@" -v "$run_dir":/run -v "$base_dir":/base:ro "$container_image" \
    qemu-system-loongarch64 \
    -machine virt -cpu la464 -accel tcg,thread=multi -m 8G -smp 8 \
    -name guest=buildstorm-profile,debug-threads=on \
    -display none -monitor none -S -no-reboot -rtc base=utc \
    -serial unix:/run/serial.sock,server=on,wait=off \
    -qmp unix:/run/qmp.sock,server=on,wait=off \
    -kernel /run/kernel-la \
    -drive if=none,id=x0,file=/run/run.qcow2,format=qcow2 \
    -device virtio-blk-pci,drive=x0 \
    -drive if=none,id=x1,file=/run/tools.ext4,format=raw \
    -device virtio-blk-pci,drive=x1
if [ "$boot_mode" = linux ]; then
    set -- "$@" \
        -initrd /run/linux-initramfs.cpio \
        -append 'console=ttyS0 panic=-1 rdinit=/linuxrc'
fi
if [ "$observer_enabled" -eq 1 ]; then
    _histogram_plugin_arg=""
    if [ "$observer_histogram" -eq 1 ]; then
        _histogram_plugin_arg=",histogram=/run/histogram.json"
    fi
    set -- "$@" -plugin \
        "/run/qemu-observer-plugin.so,socket=/run/qemu-observer.sock,period=$observer_period,stack-bytes=$observer_stack_bytes,summary=/run/qemu-observer-plugin-summary.json${_histogram_plugin_arg}"
fi
timeout 30 "$@" >/dev/null

socket_deadline=$(deadline_after_ms "$controller_timeout_ms")
while [ ! -S "$run_dir/serial.sock" ] || [ ! -S "$run_dir/qmp.sock" ]; do
    deadline_expired "$socket_deadline" && {
        echo "profile host: QEMU sockets did not appear" >&2
        timeout 5 docker logs "$container" >&2 || true
        exit 1
    }
    sleep_ms 20
done

qemu_pid=$(timeout 5 docker top "$container" -eo pid,comm,args | awk 'NR > 1 && $2 != "tini" && /qemu-system-loongarch64/ { print $1; exit }')
case "$qemu_pid" in ''|*[!0-9]*) echo "profile host: unable to resolve QEMU host PID" >&2; exit 1 ;; esac

if [ "$observer_enabled" -eq 1 ]; then
    set -- python3 "$repo/scripts/qemu_profile_daemon.py" capture \
        --qemu-pid "$qemu_pid" \
        --plugin-socket "$runtime_socket_root/qemu-observer.sock" \
        --plugin-summary "$run_dir/qemu-observer-plugin-summary.json" \
        --serial-log "$run_dir/profile.serial.log" \
        --output "$run_dir/qemu-profile.jsonl" \
        --summary "$run_dir/qemu-profile-summary.json" \
        --control-socket "$runtime_socket_root/qemu-observer-control.sock" \
        --ready-file "$run_dir/qemu-observer.ready" \
        --system "$observer_system" \
        --workload buildstorm-tg-xtask \
        --vcpu-count 8 \
        --proc-interval-ms "$observer_proc_ms" \
        --stack-interval-ms 0 \
        --stack-timeout-ms 5000 \
        --max-frames 32 \
        --max-pause-ratio 0.05 \
        --symbol-map "$run_dir/kernel.map" \
        --plugin-period-insns "$observer_period" \
        --plugin-stack-bytes "$observer_stack_bytes"
    if [ -r "$run_dir/kernel.map.manifest" ]; then
        set -- "$@" \
            --kernel-image "$run_dir/kernel-la" \
            --symbol-manifest "$run_dir/kernel.map.manifest"
    fi
    set -- "$@" \
        --environment "container_image_id=$container_image_id" \
        --environment "container_user=$host_uid:$host_gid" \
        --environment "qemu_version=$qemu_version" \
        --environment qemu_machine=virt \
        --environment qemu_cpu=la464 \
        --environment qemu_accel=tcg,thread=multi \
        --environment qemu_name=buildstorm-profile \
        --environment qemu_debug_threads=on \
        --environment memory_bytes=8589934592 \
        --environment smp=8 \
        --environment "base_image_sha256=$base_id" \
        --environment "cpuset=$cpuset_identity" \
        --environment target_tmpfs=size=5G \
        --environment "workload_plan_sha256=$workload_plan_id" \
        --environment "workload_script_sha256=$workload_script_id" \
        --environment "guest_initramfs_sha256=$initramfs_id" \
        --environment cold_target=true \
        --environment toolchain=nightly-2026-05-28 \
        --environment "plugin_sha256=$plugin_id"
    "$@" >"$run_dir/qemu-observer.stdout" 2>"$run_dir/qemu-observer.stderr" &
    observer_pid=$!
    observer_deadline=$(deadline_after_ms "$controller_timeout_ms")
    while [ ! -s "$run_dir/qemu-observer.ready" ]; do
        kill -0 "$observer_pid" 2>/dev/null || {
            echo "profile host: QEMU observer exited during setup" >&2
            cat "$run_dir/qemu-observer.stderr" >&2 || true
            exit 1
        }
        deadline_expired "$observer_deadline" && {
            echo "profile host: QEMU observer setup timed out" >&2
            exit 1
        }
        sleep_ms 20
    done
fi

setsid sh -c '
    run_dir=$1
    runtime_socket_root=$2
    while :; do cat "$run_dir/serial.in"; done |
        sudo -n socat STDIO "UNIX-CONNECT:$runtime_socket_root/serial.sock" |
        tee "$run_dir/profile.serial.log" >/dev/null
' sh "$run_dir" "$runtime_socket_root" &
logger_pid=$!

{
    sleep_ms 100
    printf '%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"cont"}'
    sleep_ms 300
} | timeout 10 sudo -n socat - "UNIX-CONNECT:$runtime_socket_root/qmp.sock" >"$run_dir/qmp.log"

wait_for_fixed '[init] press Ctrl+C within 3 seconds' "$boot_timeout_ms" || {
    echo "profile host: init interrupt prompt timed out" >&2; exit 1;
}
sleep_ms 1500
timeout 2 sh -c 'printf "\003" >"$1"' sh "$run_dir/serial.in"
wait_for_fixed '[init] Ctrl+C detected, entering shell' "$controller_timeout_ms" || {
    echo "profile host: failed to enter the init shell" >&2; exit 1;
}
wait_for_fixed '~ # ' "$controller_timeout_ms" || { echo "profile host: init shell prompt timed out" >&2; exit 1; }
sleep_ms 500

serial_sync_attempts=0
while [ "$serial_sync_attempts" -lt 3 ]; do
    serial_sync_attempts=$((serial_sync_attempts + 1))
    send_line "echo @\"\"@PROFILE_CONSOLE_SYNC token=$run_token"
    if wait_for_fixed "@@PROFILE_CONSOLE_SYNC token=$run_token" "$controller_timeout_ms"; then
        break
    fi
done
[ "$serial_sync_attempts" -le 3 ] &&
    grep -Fq "@@PROFILE_CONSOLE_SYNC token=$run_token" "$run_dir/profile.serial.log" || {
    echo "profile host: interactive console synchronization failed" >&2
    exit 1
}

send_line 'grep -q " /mnt " /proc/mounts && mkdir -p /tmp/p && echo @""@PROFILE_SETUP_1'
wait_for_fixed "@@PROFILE_SETUP_1" "$controller_timeout_ms" || {
    echo "profile host: workload disk was not mounted before the shell prompt" >&2
    exit 1
}
send_line "{ grep -q ' /tmp/p ' /proc/mounts || mount -t ext4 $tools_device /tmp/p; } && echo @\"\"@PROFILE_SETUP_2"
wait_for_fixed "@@PROFILE_SETUP_2" "$controller_timeout_ms" || { echo "profile host: guest setup mount failed" >&2; exit 1; }
send_line '. /tmp/p/config.env && echo @""@PROFILE_SETUP_3'
wait_for_fixed "@@PROFILE_SETUP_3" "$controller_timeout_ms" || { echo "profile host: guest setup config failed" >&2; exit 1; }
send_line '/tmp/p/run.sh run "$PROFILE_RUN_TOKEN" &'

marker_deadline=$(deadline_after_ms "$controller_timeout_ms")
while ! grep -q "@@PROFILE_WORKLOAD .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
    if grep -Eq 'profile runner:|mount: .*failed|PROFILE_RUNNER_DONE' "$run_dir/profile.serial.log" 2>/dev/null; then
        echo "profile host: guest setup failed" >&2; exit 1
    fi
    deadline_expired "$marker_deadline" && { echo "profile host: workload marker timed out" >&2; exit 1; }
    sleep_ms 20
done

marker=$(grep "@@PROFILE_WORKLOAD .* token=$run_token" "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
workload_log_line=$(grep -n "@@PROFILE_WORKLOAD .* token=$run_token" "$run_dir/profile.serial.log" | tail -n 1 | cut -d: -f1)
case "$workload_log_line" in ''|*[!0-9]*) echo "profile host: malformed workload marker position" >&2; exit 1 ;; esac
workload_log_offset=$(wc -c <"$run_dir/profile.serial.log" | tr -d ' ')
case "$workload_log_offset" in ''|*[!0-9]*) echo "profile host: malformed workload marker offset" >&2; exit 1 ;; esac
workload_pid=$(printf '%s\n' "$marker" | sed -n 's/.* pid=\([0-9][0-9]*\).*/\1/p')
workload_start=$(printf '%s\n' "$marker" | sed -n 's/.* start_ticks=\([0-9][0-9]*\).*/\1/p')
case "$workload_pid:$workload_start" in *[!0-9:]*|:|:*|*:) echo "profile host: malformed workload identity" >&2; exit 1 ;; esac
wait_for_fixed "@@PROFILE_CONTROLLER_READY pid=$workload_pid start_ticks=$workload_start token=$run_token" "$controller_timeout_ms" || {
    echo "profile host: guest window controller did not become ready" >&2
    exit 1
}
wait_for_fixed "@@PROFILE_GATE_READY token=$run_token" "$controller_timeout_ms" || {
    echo "profile host: workload gate did not become ready" >&2
    exit 1
}

# Arm guest filesystem stage watchers before opening Cargo's start gate. This
# makes the first observed output artifact a reliable lower boundary instead
# of racing host-side serial setup against an already-running workload.
if [ "$anchor" = aws-object ]; then
    send_line "/tmp/p/run.sh w $run_token aws-first-object &"
    wait_for_fixed "@@PROFILE_STAGE_WATCH_READY name=aws-first-object token=$run_token" "$controller_timeout_ms" || {
        echo "profile host: aws object stage watcher did not become ready" >&2; exit 1;
    }
fi

# Cargo is born behind a guest-side gate. Open it before searching for an
# anchor that requires workload output; workload/zero-warmup remains gated
# until the measured window is fully prepared.
case "$anchor:$warmup_ms" in
    workload:0) ;;
    *)
        send_line "/tmp/p/run.sh g $run_token"
        wait_for_fixed "@@PROFILE_GATE_OPENED token=$run_token" "$controller_timeout_ms" || {
            echo "profile host: workload start gate timed out" >&2; exit 1;
        }
        ;;
esac

anchor_deadline=$(deadline_after_ms "$stage_timeout_ms")
anchor_ns=
while [ -z "$anchor_ns" ]; do
    record_progress
    case "$anchor" in
        workload) anchor_ns=$(monotonic_ns) ;;
        aws-object)
            serial_after_workload_has "@@PROFILE_STAGE name=aws-first-object token=$run_token" &&
                anchor_ns=$(monotonic_ns)
            ;;
        cargo:*) [ -e "$run_dir/progress-$anchor_progress" ] && anchor_ns=$(monotonic_ns) ;;
        marker:*) serial_after_workload_has "$anchor_marker" && anchor_ns=$(monotonic_ns) ;;
    esac
    if workload_finished; then break; fi
    deadline_expired "$anchor_deadline" && { echo "profile host: stage anchor timed out" >&2; exit 1; }
    [ -n "$anchor_ns" ] || sleep_ms "$poll_ms"
done
printf '%s\n' "${anchor_ns:-0}" >"$run_dir/anchor-monotonic-ns"

workload_ended=0
if [ -z "$anchor_ns" ]; then
    workload_ended=1
else
    warmup_deadline=$((anchor_ns + warmup_ms * 1000000))
    while [ "$(monotonic_ns)" -lt "$warmup_deadline" ]; do
        record_progress
        if workload_finished; then workload_ended=1; break; fi
        sleep_ms "$poll_ms"
    done
fi

capture_started=0
sample_host prestart
if [ "$workload_ended" -eq 0 ]; then
    send_line "/tmp/p/run.sh a $run_token"
    capture_deadline=$(deadline_after_ms "$capture_start_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_READY token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        record_progress
        if workload_finished; then
            workload_ended=1
            break
        fi
        deadline_expired "$capture_deadline" && {
            report_controller_status
            echo "profile host: capture start timed out" >&2
            exit 1
        }
        sleep_ms "$poll_ms"
    done
    [ "$workload_ended" -eq 1 ] || capture_started=$capture
fi

# Anchor discovery may have observed Cargo output before the measured window.
# Reset exported milestones at the START gate so summary.json cannot mix the
# warmup/stage interval with the profiler and QEMU CPU interval.
window_progress=$(current_progress)
[ -n "$window_progress" ] || window_progress=-1
last_progress=$window_progress
for milestone in 0 64 128 256 384 440 446; do
    rm -f "$run_dir/progress-$milestone"
done
printf 'milestone\tmonotonic_ns\n' >"$run_dir/progress.tsv"

if [ "$observer_enabled" -eq 1 ]; then
    timeout 10 python3 "$repo/scripts/qemu_profile_daemon.py" ctl \
        --socket "$runtime_socket_root/qemu-observer-control.sock" start --label "$safe_label" \
        >>"$run_dir/qemu-observer-control.log"
fi
start_ns=$(monotonic_ns)
printf '%s\n' "$start_ns" >"$run_dir/host-window-start-ns"
sample_qemu_boundary start "$start_ns"
start_observed_ns=$start_ns
if [ "$workload_ended" -eq 0 ]; then
    send_line "/tmp/p/run.sh c $run_token"
    wait_for_fixed "@@PROFILE_WINDOW_STARTED token=$run_token" "$controller_timeout_ms" || {
        echo "profile host: workload window resume timed out" >&2; exit 1;
    }
    wait_for_fixed "@@PROFILE_CARGO_EXEC token=$run_token" "$controller_timeout_ms" || {
        echo "profile host: cargo did not cross the start gate" >&2; exit 1;
    }
    start_observed_ns=$(monotonic_ns)
fi
deadline_ns=$(deadline_after_ms "$duration_ms")
next_sample_ns=$((start_ns + sample_ms * 1000000))
while [ "$workload_ended" -eq 0 ]; do
    now_ns=$(monotonic_ns)
    if workload_finished; then
        workload_ended=1
        break
    fi
    deadline_expired "$deadline_ns" && break
    record_progress
    if [ "$now_ns" -ge "$next_sample_ns" ]; then
        sample_host interval
        next_sample_ns=$((now_ns + sample_ms * 1000000))
    fi
    sleep_ms "$poll_ms"
done

stop_sent=0
frozen_observed=0
frozen_ended=1
frozen_quiescence_verified=1
frozen_quiescence_method=workload-ended
measurement_stop_ns=0
observer_capture_stopped=0
stop_progress=
stop_request_ns=$(monotonic_ns)
if [ "$workload_ended" -eq 0 ]; then
    stop_sent=1
    # The fixed host deadline is the profiler/QEMU CPU boundary. Guest
    # SIGSTOP propagation and quiescence proof happen afterwards and must not
    # contribute procfs/controller work to either kernel's hotspot counts.
    measurement_stop_ns=$stop_request_ns
    stop_progress=$(current_progress)
    [ -n "$stop_progress" ] || stop_progress=-1
    sample_qemu_boundary stop "$measurement_stop_ns"
    if [ "$observer_enabled" -eq 1 ]; then
        timeout 30 python3 "$repo/scripts/qemu_profile_daemon.py" ctl \
            --socket "$runtime_socket_root/qemu-observer-control.sock" stop \
            >>"$run_dir/qemu-observer-control.log"
        observer_capture_stopped=1
    fi
    send_line "/tmp/p/run.sh z $run_token"
    stop_deadline=$(deadline_after_ms "$done_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_FROZEN .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        deadline_expired "$stop_deadline" && {
            report_controller_status
            echo "profile host: window freeze timed out" >&2
            exit 1
        }
        sleep_ms "$poll_ms"
    done
else
    stop_deadline=$(deadline_after_ms "$done_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_FROZEN .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        deadline_expired "$stop_deadline" && break
        sleep_ms "$poll_ms"
    done
fi
if grep -q "@@PROFILE_WINDOW_FROZEN .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; then
    frozen_observed=1
    frozen_ended=$(sed -n "s/.*@@PROFILE_WINDOW_FROZEN ended=\([01]\) token=$run_token.*/\1/p" \
        "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
    frozen_quiescence_verified=$(sed -n "s/.*@@PROFILE_WINDOW_FROZEN .* token=$run_token quiescence_verified=\([01]\).*/\1/p" \
        "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
    frozen_quiescence_method=$(sed -n "s/.*@@PROFILE_WINDOW_FROZEN .* token=$run_token .*quiescence_method=\([A-Za-z0-9-][A-Za-z0-9-]*\).*/\1/p" \
        "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
    case "$frozen_ended" in
        0|1) ;;
        *) echo "profile host: malformed frozen-window state" >&2; exit 1 ;;
    esac
    [ "$frozen_quiescence_verified" = 1 ] || {
        echo "profile host: workload group was not quiescence-verified" >&2
        exit 1
    }
    case "$frozen_quiescence_method:$frozen_ended:$boot_mode" in
        task-snapshot:*:mygo|linux-proc-stat-double:*:linux|workload-ended:1:*) ;;
        *)
            echo "profile host: invalid quiescence method for $boot_mode: ${frozen_quiescence_method:-missing}" >&2
            exit 1
            ;;
    esac
fi
stop_ns=$(monotonic_ns)
if [ "$measurement_stop_ns" -eq 0 ]; then
    measurement_stop_ns=$stop_ns
    sample_qemu_boundary stop "$measurement_stop_ns"
fi
if [ "$observer_enabled" -eq 1 ] && [ "$observer_capture_stopped" -eq 0 ]; then
    timeout 30 python3 "$repo/scripts/qemu_profile_daemon.py" ctl \
        --socket "$runtime_socket_root/qemu-observer-control.sock" stop \
        >>"$run_dir/qemu-observer-control.log"
    observer_capture_stopped=1
fi
if [ -z "$stop_progress" ]; then
    stop_progress=$(current_progress)
    [ -n "$stop_progress" ] || stop_progress=-1
fi
capture_stop_observed_ns=0
if [ "$frozen_observed" -eq 1 ]; then
    send_line "/tmp/p/run.sh k $run_token"
    snapshot_deadline=$(deadline_after_ms "$done_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_STOPPED .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        deadline_expired "$snapshot_deadline" && {
            report_controller_status
            echo "profile host: window snapshot timed out" >&2
            exit 1
        }
        sleep_ms "$poll_ms"
    done
    stopped_ended=$(sed -n "s/.*@@PROFILE_WINDOW_STOPPED ended=\([01]\) token=$run_token.*/\1/p" \
        "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
    [ "$stopped_ended" = "$frozen_ended" ] || {
        echo "profile host: frozen/stopped workload state mismatch" >&2
        exit 1
    }
    capture_stop_observed_ns=$(monotonic_ns)
elif [ "$stop_sent" -eq 1 ]; then
    echo "profile host: missing frozen-window state after stop request" >&2
    exit 1
fi
actual_stop_sent=0
if [ "$stop_sent" -eq 1 ] && [ "$frozen_ended" -eq 0 ]; then
    actual_stop_sent=1
fi
stop_command_sent_ns=$(monotonic_ns)
sample_host poststop
printf '%s\n' "$stop_request_ns" >"$run_dir/host-stop-request-ns"
printf '%s\n' "$measurement_stop_ns" >"$run_dir/host-stop-sent-ns"
printf '%s\n' "$stop_ns" >"$run_dir/host-freeze-observed-ns"
printf '%s\n' "$stop_command_sent_ns" >"$run_dir/host-stop-command-complete-ns"

runner_status=null
runner_status_observed=0
termination_mode=host-qemu-teardown
if [ "$actual_stop_sent" -eq 0 ]; then
    # A naturally completed workload remains valid only when the guest runner
    # reports its final status. Deadline runs already have a verified stopped
    # snapshot, so their teardown must not depend on guest task reaping.
    termination_mode=guest-runner-complete
    done_deadline=$(deadline_after_ms "$done_timeout_ms")
    while ! grep -q "PROFILE_RUNNER_DONE .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        deadline_expired "$done_deadline" && {
            echo "profile host: natural runner completion timed out" >&2
            exit 1
        }
        sleep_ms "$poll_ms"
    done
    runner_status=$(sed -n "s/.*PROFILE_RUNNER_DONE status=\([0-9][0-9]*\) token=$run_token.*/\1/p" \
        "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
    case "$runner_status" in
        ''|*[!0-9]*) echo "profile host: malformed runner status" >&2; exit 1 ;;
    esac
    runner_status_observed=1
    if [ "$runner_status" -ne 0 ]; then
        echo "profile host: naturally completed workload failed with status $runner_status" >&2
        exit 1
    fi
fi
done_ns=$(monotonic_ns)
sample_host final
if [ "$capture_started" -eq 1 ]; then
    "$repo/scripts/profile-report.sh" "$run_dir/profile.serial.log" >"$run_dir/profile.report" 2>"$run_dir/profile-report.err"
    profile_report_status=available
else
    printf 'unavailable\n' >"$run_dir/profile.report"
    : >"$run_dir/profile-report.err"
    profile_report_status=unavailable
fi

# End every successful measurement through QMP. This is the authoritative
# deadline teardown and also gives the optional observer a reconciled QEMU exit.
qmp_shutdown_status=0
set +e
{
    sleep_ms 100
    printf '%s\n' '{"execute":"qmp_capabilities"}'
    sleep_ms 100
    printf '%s\n' '{"execute":"quit"}'
} | timeout 10 sudo -n socat - "UNIX-CONNECT:$runtime_socket_root/qmp.sock" \
    >"$run_dir/qmp-shutdown.log"
qmp_shutdown_status=$?
set -e
python3 - "$run_dir/qmp-shutdown.log" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
seen_return = False
seen_shutdown = False
for raw_line in path.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
        message = json.loads(raw_line)
    except json.JSONDecodeError:
        continue
    if isinstance(message, dict) and message.get("return") == {}:
        seen_return = True
    if isinstance(message, dict) and message.get("event") == "SHUTDOWN":
        seen_shutdown = True
if not (seen_return and seen_shutdown):
    raise SystemExit("profile host: QMP quit did not produce return/SHUTDOWN evidence")
PY
if [ "$qmp_shutdown_status" -ne 0 ]; then
    echo "profile host: QMP peer closed with status $qmp_shutdown_status after SHUTDOWN" >&2
fi
timeout 30 docker wait "$container" >"$run_dir/qemu-exit-status"
qemu_pid=
if [ "$observer_enabled" -eq 1 ]; then
    timeout 10 python3 "$repo/scripts/qemu_profile_daemon.py" ctl \
        --socket "$runtime_socket_root/qemu-observer-control.sock" shutdown \
        >>"$run_dir/qemu-observer-control.log"
    wait "$observer_pid"
    observer_pid=
    observer_quality_valid=$(python3 - "$run_dir/qemu-profile-summary.json" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
summary = json.loads(path.read_text())
if summary.get("schema") != "mygo.qemu-profile.v1":
    raise SystemExit("profile host: QEMU observer returned an unsupported schema")
quality = summary.get("quality", {})
print(
    1
    if quality.get("valid") is True
    and quality.get("plugin_exit_reconciled") is True
    else 0
)
PY
    )
fi

python3 - "$run_dir" "$anchor" "$anchor_ns" "$start_ns" "$stop_ns" "$done_ns" "$actual_stop_sent" "$runner_status" "$profile_report_status" "$capture_started" "$window_progress" "$stop_progress" "$start_observed_ns" "$stop_request_ns" "$stop_command_sent_ns" "$capture_stop_observed_ns" "$stop_sent" "$frozen_ended" "$frozen_quiescence_verified" "$runner_status_observed" "$termination_mode" "$frozen_quiescence_method" "$measurement_stop_ns" "$observer_histogram" <<'PY'
import csv, json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])
metadata = {}
for line in (run_dir / "metadata.env").read_text().splitlines():
    key, value = line.split("=", 1)
    metadata[key] = value
progress = {}
with (run_dir / "progress.tsv").open() as f:
    for row in csv.DictReader(f, delimiter="\t"):
        progress[row["milestone"]] = int(row["monotonic_ns"])
for milestone in ("0", "64", "128", "256", "384", "440", "446"):
    progress.setdefault(milestone, None)
with (run_dir / "host-samples.tsv").open() as f:
    samples = list(csv.DictReader(f, delimiter="\t"))
with (run_dir / "qemu-cpu-boundaries.tsv").open() as f:
    boundaries = {row["phase"]: row for row in csv.DictReader(f, delimiter="\t")}
first_sample = samples[0]
last_sample = samples[-1]
start_cpu = boundaries["start"]
stop_cpu = boundaries["stop"]
ticks_per_second = int(metadata["host_clock_ticks_per_second"])
qemu_cpu_ticks = (
    int(stop_cpu["qemu_utime_ticks"]) + int(stop_cpu["qemu_stime_ticks"])
    - int(start_cpu["qemu_utime_ticks"]) - int(start_cpu["qemu_stime_ticks"])
)
observer_enabled = metadata.get("qemu_observer_enabled") == "1"
observer_summary = None
if observer_enabled:
    observer_summary = json.loads((run_dir / "qemu-profile-summary.json").read_text())
runner_status = None if sys.argv[8] == "null" else int(sys.argv[8])
runner_status_observed = bool(int(sys.argv[20]))
termination_mode = sys.argv[21]
if termination_mode == "host-qemu-teardown":
    if runner_status_observed or runner_status is not None:
        raise SystemExit("host QEMU teardown cannot claim a guest runner status")
elif termination_mode == "guest-runner-complete":
    if not runner_status_observed or runner_status is None:
        raise SystemExit("guest runner completion requires an observed status")
else:
    raise SystemExit(f"unsupported termination mode: {termination_mode}")
summary = {
    "schema": "mygo.buildstorm-profile",
    "schema_version": 2,
    "run_dir": str(run_dir),
    "metadata": metadata,
    "timing": {
        "stage_anchor": sys.argv[2],
        "anchor_monotonic_ns": int(sys.argv[3] or 0),
        "window_start_monotonic_ns": int(sys.argv[4]),
        "window_start_progress": int(sys.argv[11]),
        "window_stop_progress": int(sys.argv[12]),
        "window_start_observed_monotonic_ns": int(sys.argv[13]),
        "start_observation_latency_ms": (int(sys.argv[13]) - int(sys.argv[4])) / 1_000_000,
        "stop_request_monotonic_ns": int(sys.argv[14]),
        "measurement_stop_monotonic_ns": int(sys.argv[23]),
        "stop_monotonic_ns": int(sys.argv[5]),
        "stop_observation_latency_ms": (int(sys.argv[5]) - int(sys.argv[14])) / 1_000_000,
        "quiescence_observation_latency_ms": (int(sys.argv[5]) - int(sys.argv[23])) / 1_000_000,
        "done_monotonic_ns": int(sys.argv[6]),
        "stop_command_complete_monotonic_ns": int(sys.argv[15]),
        "capture_stop_observed_monotonic_ns": int(sys.argv[16]),
        "termination_command_latency_ms": (int(sys.argv[15]) - int(sys.argv[5])) / 1_000_000,
        "elapsed_ms": (int(sys.argv[23]) - int(sys.argv[4])) / 1_000_000,
        "cargo_progress_monotonic_ns": progress,
    },
    "result": {
        "deadline_stop_sent": bool(int(sys.argv[7])),
        "workload_ended_early": not bool(int(sys.argv[7])),
        "runner_status": runner_status,
        "runner_status_observed": runner_status_observed,
        "termination_mode": termination_mode,
        "stop_requested": bool(int(sys.argv[17])),
        "window_ended_before_stop": bool(int(sys.argv[18])),
        "quiescence_verified": bool(int(sys.argv[19])),
        "quiescence_method": sys.argv[22],
    },
    "profiling": {
        "capture_started": bool(int(sys.argv[10])),
        "mode": (
            "off" if metadata["capture_enabled"] == "0" else
            "trace" if metadata["trace_enabled"] == "1" else
            "sampled" if metadata["sampling_enabled"] == "1" else
            "counts-only"
        ),
        "observation_poll_upper_bound_ms": int(metadata["poll_ms"]),
        "report_status": sys.argv[9],
        "report": "profile.report",
    },
    "host": {
        "sample_count": len(samples),
        "first_sample": first_sample,
        "last_sample": last_sample,
        "qemu_cpu_start": start_cpu,
        "qemu_cpu_stop": stop_cpu,
        "qemu_cpu_ticks": qemu_cpu_ticks,
        "qemu_cpu_seconds": qemu_cpu_ticks / ticks_per_second,
    },
    "host_samples_tsv": "host-samples.tsv",
    "profile_report": "profile.report",
    "qemu_observer": {
        "enabled": observer_enabled,
        "summary": "qemu-profile-summary.json" if observer_enabled else None,
        "events": "qemu-profile.jsonl" if observer_enabled else None,
        "quality": observer_summary["quality"] if observer_summary else None,
        "guest_instructions": observer_summary["guest_instructions"] if observer_summary else None,
        "histogram": "histogram.json" if (observer_enabled and bool(int(sys.argv[24]))) else None,
    },
}
(run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY

elapsed_ns=$((measurement_stop_ns - start_ns))
normal_exit=1
printf 'PROFILE_HOST_DONE run_dir=%s elapsed_ms=%d.%06d status=%s status_observed=%s termination=%s stopped=%s observer_valid=%s\n' \
    "$run_dir" "$((elapsed_ns / 1000000))" "$((elapsed_ns % 1000000))" \
    "$runner_status" "$runner_status_observed" "$termination_mode" "$actual_stop_sent" "$observer_quality_valid"
if [ "$observer_enabled" -eq 1 ] && [ "$observer_require_valid" -eq 1 ] && \
    [ "$observer_quality_valid" -ne 1 ]; then
    echo "profile host: QEMU observer quality gate failed" >&2
    exit 1
fi
