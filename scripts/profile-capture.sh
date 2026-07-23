#!/bin/sh
set -eu

control=${PROFILE_CONTROL:-/sys/kernel/profile_control}
stats=${PROFILE_STATS:-/sys/kernel/profile_stats}
samples=${PROFILE_SAMPLES:-/sys/kernel/profile_samples}
catalog=${PROFILE_CATALOG:-/sys/kernel/profile_catalog}
trace=${PROFILE_TRACE_FILE:-/sys/kernel/profile_trace}

usage() {
    echo "usage: $0 <start|stop|status|catalog> [case-id]" >&2
    echo "       $0 run <case-id> <command> [args...]" >&2
    echo "       PROFILE_PRESET=io|syscall|filesystem|block|full PROFILE_TIMING_SHIFT=0..16" >&2
    exit 2
}

write_control() {
    printf '%s\n' "$1" >"$control"
}

preset_mask() {
    case "$1" in
        io) echo 0x1efff4000 ;;
        syscall) echo 0x1000000 ;;
        filesystem) echo 0x6000000 ;;
        block) echo 0x1e0000000 ;;
        full) echo 0x1ffffffff ;;
        *)
            echo "profile capture: unknown PROFILE_PRESET=$1" >&2
            exit 2
            ;;
    esac
}

validate_case_id() {
    case "$1" in
        ''|*[!A-Za-z0-9_.-]*)
            echo "profile capture: case id may contain only letters, digits, '.', '_' and '-'" >&2
            exit 2
            ;;
    esac
}

snapshot() {
    phase=$1
    case_id=$2
    stats_snapshot=$(cat "$stats")
    echo "@@PROFILE_STATS_BEGIN phase=$phase case=$case_id"
    printf '%s\n' "$stats_snapshot"
    echo "@@PROFILE_STATS_END phase=$phase case=$case_id"
    if [ -r "$samples" ]; then
        samples_snapshot=$(cat "$samples")
        echo "@@PROFILE_SAMPLES_BEGIN phase=$phase case=$case_id"
        printf '%s\n' "$samples_snapshot"
        echo "@@PROFILE_SAMPLES_END phase=$phase case=$case_id"
    fi
    if [ -r "$trace" ]; then
        trace_snapshot=$(cat "$trace")
        echo "@@PROFILE_TRACE_BEGIN phase=$phase case=$case_id"
        printf '%s\n' "$trace_snapshot"
        echo "@@PROFILE_TRACE_END phase=$phase case=$case_id"
    fi
}

metadata() {
    phase=$1
    case_id=$2
    exit_status=${3:-not-run}
    arch=${PROFILE_ARCH:-$(uname -m 2>/dev/null || echo unknown)}
    cpu_online=${PROFILE_CPU_ONLINE:-$(cat /sys/devices/system/cpu/online 2>/dev/null || echo unknown)}
    kernel_release=${PROFILE_KERNEL_RELEASE:-$(uname -r 2>/dev/null || echo unknown)}
    cmdline=${PROFILE_CMDLINE:-$(cat /sys/kernel/cmdline 2>/dev/null || echo unknown)}
    echo "@@PROFILE_META_BEGIN phase=$phase case=$case_id"
    echo "arch=$arch"
    echo "cpu_online=$cpu_online"
    echo "kernel_release=$kernel_release"
    echo "kernel_features=${PROFILE_FEATURES:-performance-profile}"
    echo "kernel_image_id=${PROFILE_KERNEL_IMAGE_ID:-unknown}"
    echo "rootfs_image_id=${PROFILE_ROOTFS_IMAGE_ID:-unknown}"
    echo "workload=${PROFILE_WORKLOAD:-unknown}"
    echo "workload_exit_status=$exit_status"
    echo "cmdline=$cmdline"
    if [ -n "${PROFILE_TIMING_SAMPLER:-}" ]; then
        echo "timing_sampler=$PROFILE_TIMING_SAMPLER"
    fi
    echo "control=$(cat "$control")"
    echo "@@PROFILE_META_END phase=$phase case=$case_id"
}

start_capture() {
    case_id=$1
    sampling=${PROFILE_SAMPLING:-0}
    trace_enabled=${PROFILE_TRACE_ENABLED:-0}
    timing_shift=${PROFILE_TIMING_SHIFT:-8}
    leave_frozen=${PROFILE_LEAVE_FROZEN:-0}
    validate_case_id "$case_id"
    if [ -n "${PROFILE_EVENT_MASK:-}" ] && [ -n "${PROFILE_PRESET:-}" ]; then
        echo "profile capture: PROFILE_EVENT_MASK and PROFILE_PRESET are mutually exclusive" >&2
        exit 2
    fi
    case "$sampling" in
        0|1) ;;
        *)
            echo "profile capture: PROFILE_SAMPLING must be 0 or 1" >&2
            exit 2
            ;;
    esac
    case "$trace_enabled" in
        0|1) ;;
        *)
            echo "profile capture: PROFILE_TRACE_ENABLED must be 0 or 1" >&2
            exit 2
            ;;
    esac
    case "$timing_shift" in
        ''|*[!0-9]*)
            echo "profile capture: PROFILE_TIMING_SHIFT must be an integer from 0 to 16" >&2
            exit 2
            ;;
    esac
    if [ "$timing_shift" -gt 16 ]; then
        echo "profile capture: PROFILE_TIMING_SHIFT must be an integer from 0 to 16" >&2
        exit 2
    fi
    case "$leave_frozen" in
        0|1) ;;
        *)
            echo "profile capture: PROFILE_LEAVE_FROZEN must be 0 or 1" >&2
            exit 2
            ;;
    esac
    preset=
    if [ -n "${PROFILE_PRESET:-}" ]; then
        preset=$(preset_mask "$PROFILE_PRESET")
    fi
    event_mask=${PROFILE_EVENT_MASK:-${preset:-0x1ffffffff}}
    write_control freeze
    write_control reset
    write_control "events=$event_mask"
    write_control "samples=$sampling"
    write_control "trace=$trace_enabled"
    write_control "timing_shift=$timing_shift"
    metadata before "$case_id"
    snapshot before "$case_id"
    [ "$leave_frozen" -eq 1 ] || write_control resume
}

stop_capture() {
    case_id=$1
    exit_status=${2:-unknown}
    already_frozen=${PROFILE_ALREADY_FROZEN:-0}
    validate_case_id "$case_id"
    case "$already_frozen" in
        0|1) ;;
        *)
            echo "profile capture: PROFILE_ALREADY_FROZEN must be 0 or 1" >&2
            exit 2
            ;;
    esac
    # Interactive init shells may have printed a prompt while a background
    # workload was running; keep the after markers parseable on their own lines.
    printf '\n'
    [ "$already_frozen" -eq 1 ] || write_control freeze
    metadata after "$case_id" "$exit_status"
    snapshot after "$case_id"
}

[ "$#" -ge 1 ] || usage
command=$1
case_id=${2:-default}

if [ ! -w "$control" ] || [ ! -r "$stats" ]; then
    echo "profile capture: profiling sysfs interface is unavailable" >&2
    exit 1
fi

case "$command" in
    start)
        [ "$#" -le 2 ] || usage
        start_capture "$case_id"
        ;;
    stop)
        [ "$#" -le 2 ] || usage
        stop_capture "$case_id"
        ;;
    status)
        [ "$#" -eq 1 ] || usage
        cat "$control"
        ;;
    catalog)
        [ "$#" -eq 1 ] || usage
        cat "$catalog"
        ;;
    run)
        [ "$#" -ge 3 ] || usage
        shift 2
        if [ -z "${PROFILE_WORKLOAD:-}" ]; then
            PROFILE_WORKLOAD=$*
            export PROFILE_WORKLOAD
        fi
        start_capture "$case_id"
        "$@" <&0 &
        workload_pid=$!
        echo "@@PROFILE_WORKLOAD case=$case_id pid=$workload_pid"
        if wait "$workload_pid"; then
            workload_status=0
        else
            workload_status=$?
        fi
        stop_capture "$case_id" "$workload_status"
        exit "$workload_status"
        ;;
    *)
        usage
        ;;
esac
