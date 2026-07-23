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
    echo "       PROFILE_PRESET=io|syscall|filesystem|block|full" >&2
    exit 2
}

write_control() {
    printf '%s\n' "$1" >"$control"
}

preset_mask() {
    case "$1" in
        io) echo 0x1e3ff4000 ;;
        syscall) echo 0x1000000 ;;
        filesystem) echo 0x3000000 ;;
        block) echo 0x1e0000000 ;;
        full) echo 0x1ffffffff ;;
        *)
            echo "profile capture: unknown PROFILE_PRESET=$1" >&2
            exit 2
            ;;
    esac
}

snapshot() {
    phase=$1
    case_id=$2
    echo "@@PROFILE_STATS_BEGIN phase=$phase case=$case_id"
    cat "$stats"
    echo "@@PROFILE_STATS_END phase=$phase case=$case_id"
    if [ -r "$samples" ]; then
        echo "@@PROFILE_SAMPLES_BEGIN phase=$phase case=$case_id"
        cat "$samples"
        echo "@@PROFILE_SAMPLES_END phase=$phase case=$case_id"
    fi
    if [ -r "$trace" ]; then
        echo "@@PROFILE_TRACE_BEGIN phase=$phase case=$case_id"
        cat "$trace"
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
    echo "control=$(cat "$control")"
    echo "@@PROFILE_META_END phase=$phase case=$case_id"
}

start_capture() {
    case_id=$1
    if [ -n "${PROFILE_EVENT_MASK:-}" ] && [ -n "${PROFILE_PRESET:-}" ]; then
        echo "profile capture: PROFILE_EVENT_MASK and PROFILE_PRESET are mutually exclusive" >&2
        exit 2
    fi
    preset=
    if [ -n "${PROFILE_PRESET:-}" ]; then
        preset=$(preset_mask "$PROFILE_PRESET")
    fi
    write_control freeze
    write_control reset
    if [ -n "${PROFILE_EVENT_MASK:-}" ]; then
        write_control "events=$PROFILE_EVENT_MASK"
    elif [ -n "$preset" ]; then
        write_control "events=$preset"
    fi
    [ -z "${PROFILE_SAMPLING:-}" ] || \
        write_control "samples=$PROFILE_SAMPLING"
    [ -z "${PROFILE_TRACE_ENABLED:-}" ] || \
        write_control "trace=$PROFILE_TRACE_ENABLED"
    metadata before "$case_id"
    snapshot before "$case_id"
    write_control resume
}

stop_capture() {
    case_id=$1
    exit_status=${2:-unknown}
    # Interactive init shells may have printed a prompt while a background
    # workload was running; keep the after markers parseable on their own lines.
    printf '\n'
    write_control freeze
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
