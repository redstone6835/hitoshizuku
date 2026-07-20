#!/bin/sh
set -eu

control=${PROFILE_CONTROL:-/sys/kernel/profile_control}
stats=${PROFILE_STATS:-/sys/kernel/profile_stats}
samples=${PROFILE_SAMPLES:-/sys/kernel/profile_samples}
catalog=${PROFILE_CATALOG:-/sys/kernel/profile_catalog}

usage() {
    echo "usage: $0 <start|stop|status|catalog> [case-id]" >&2
    exit 2
}

write_control() {
    printf '%s\n' "$1" | dd of="$control" conv=notrunc 2>/dev/null
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
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
command=$1
case_id=${2:-default}

if [ ! -w "$control" ] || [ ! -r "$stats" ]; then
    echo "profile capture: profiling sysfs interface is unavailable" >&2
    exit 1
fi

case "$command" in
    start)
        write_control freeze
        write_control reset
        [ -z "${PROFILE_EVENT_MASK:-}" ] || \
            write_control "events=$PROFILE_EVENT_MASK"
        [ -z "${PROFILE_SAMPLING:-}" ] || \
            write_control "samples=$PROFILE_SAMPLING"
        snapshot before "$case_id"
        write_control resume
        ;;
    stop)
        write_control freeze
        snapshot after "$case_id"
        ;;
    status)
        cat "$control"
        ;;
    catalog)
        cat "$catalog"
        ;;
    *)
        usage
        ;;
esac
