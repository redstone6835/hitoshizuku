#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <serial-log> [top-n]" >&2
    exit 2
fi

log=$1
top=${2:-20}
case "$top" in
    ''|*[!0-9]*)
        echo "profile analyze: top-n must be a non-negative integer" >&2
        exit 2
        ;;
esac

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
rows=$tmp/rows
summary=$tmp/summary
bottlenecks=$tmp/bottlenecks
io_bottlenecks=$tmp/io-bottlenecks
workload_summary=$tmp/workload-summary
workload_bottlenecks=$tmp/workload-bottlenecks
attribution=$tmp/attribution
sys_rows=$tmp/syscall-rows
io_summary=$tmp/io-summary
tab=$(printf '\t')

# Reuse the trace validator so analysis never runs on an incomplete window.
if ! "$root/scripts/profile-trace-report.sh" "$log" >"$rows"; then
    echo "profile analyze: trace validation failed" >&2
    exit 1
fi

awk -F '\t' -v summary="$summary" -v bottlenecks="$bottlenecks" \
    -v io_bottlenecks="$io_bottlenecks" -v workload_summary="$workload_summary" \
    -v workload_bottlenecks="$workload_bottlenecks" '
NR == 1 { next }
NF < 13 { next }
{
    case_id = $1
    timestamp = $2 + 0
    duration = $3 + 0
    task = $5
    span = $6
    kind = $7
    event = $8
    role = $14
    if (span == "" || span == "0") next
    key = case_id SUBSEP span
    effective_duration = duration
    interval_start = timestamp
    if (kind == "task_wake" && event ~ /^wait_/) {
        effective_duration = ($9 + 0) / 1000
        interval_start = timestamp - effective_duration
    }
    if (!(key in first)) {
        first[key] = interval_start
        first_task[key] = task
    }
    if (interval_start < first[key]) first[key] = interval_start
    if (timestamp + duration > last[key]) last[key] = timestamp + duration
    records[key]++
    if (role == "workload-root") workload_root[key] = 1
    event_duration[key, event] += effective_duration
    event_seen[key, event] = 1
    if (duration > 0) {
        record_count[key]++
        record_index = record_count[key]
        record_start[key, record_index] = timestamp
        record_end[key, record_index] = timestamp + duration
        record_event[key, record_index] = event
    } else if (effective_duration > 0) {
        event_exclusive[key, event] += effective_duration
    }
    if (event == "syscall_dispatch") {
        syscall[key] = $9
        syscall_duration[key] += duration
    }
}
END {
    print "case\tspan\tsyscall\ttask\trecords\twall_us\tsyscall_us\tvfs_us\tblock_submit_us\tblock_drain_us\tblock_complete_us\tblock_wait_us\twait_us\tdominant_event\tdominant_us\tdominant_share_pct" > summary
    print "case\tspan\tsyscall\ttask\trecords\twall_us\tsyscall_us\tvfs_us\tblock_submit_us\tblock_drain_us\tblock_complete_us\tblock_wait_us\twait_us\tdominant_event\tdominant_us\tdominant_share_pct" > workload_summary
    print "case\tspan\tsyscall\tevent\texclusive_us\tshare_pct" > bottlenecks
    print "case\tspan\tsyscall\tevent\texclusive_us\tshare_pct" > workload_bottlenecks
    print "case\tspan\tsyscall\tevent\texclusive_us\tshare_pct" > io_bottlenecks
    for (key in first) {
        split(key, parts, SUBSEP)
        case_id = parts[1]
        span = parts[2]
        wall = last[key] - first[key]
        is_io = event_duration[key, "vfs_read"] + event_duration[key, "vfs_write"] + \
            event_duration[key, "block_submit"] + event_duration[key, "block_drain"] + \
            event_duration[key, "block_complete"] + event_duration[key, "block_wait"] > 0
        for (i = 1; i <= record_count[key]; i++)
            record_exclusive[i] = record_end[key, i] - record_start[key, i]
        for (child = 1; child <= record_count[key]; child++) {
            parent = 0
            parent_duration = 1e300
            child_start = record_start[key, child]
            child_end = record_end[key, child]
            child_duration = child_end - child_start
            for (candidate = 1; candidate <= record_count[key]; candidate++) {
                if (candidate == child) continue
                candidate_start = record_start[key, candidate]
                candidate_end = record_end[key, candidate]
                candidate_duration = candidate_end - candidate_start
                if (candidate_start <= child_start && candidate_end >= child_end && \
                    (candidate_start < child_start || candidate_end > child_end) && \
                    candidate_duration < parent_duration) {
                    parent = candidate
                    parent_duration = candidate_duration
                }
            }
            if (parent != 0)
                record_exclusive[parent] -= child_duration
        }
        for (i = 1; i <= record_count[key]; i++) {
            exclusive = record_exclusive[i]
            if (exclusive < 0) exclusive = 0
            event_exclusive[key, record_event[key, i]] += exclusive
            delete record_exclusive[i]
        }
        dominant = ""
        dominant_duration = 0
        for (event_key in event_seen) {
            split(event_key, event_parts, SUBSEP)
            event_owner = event_parts[1] SUBSEP event_parts[2]
            if (event_owner != key) continue
            event = event_parts[3]
            duration = event_exclusive[key, event]
            if (duration > dominant_duration) {
                dominant = event
                dominant_duration = duration
            }
            if (duration > 0)
                printf "%s\t%s\t%s\t%s\t%.3f\t%.1f\n", case_id, span, syscall[key], event, duration, (wall > 0 ? duration * 100 / wall : 0) > bottlenecks
            if (workload_root[key] && duration > 0)
                printf "%s\t%s\t%s\t%s\t%.3f\t%.1f\n", case_id, span, syscall[key], event, duration, (wall > 0 ? duration * 100 / wall : 0) > workload_bottlenecks
            if (is_io && duration > 0)
                printf "%s\t%s\t%s\t%s\t%.3f\t%.1f\n", case_id, span, syscall[key], event, duration, (wall > 0 ? duration * 100 / wall : 0) > io_bottlenecks
        }
        line = sprintf("%s\t%s\t%s\t%s\t%d\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t%s\t%.3f\t%.1f", \
            case_id, span, syscall[key], first_task[key], records[key], wall, \
            event_duration[key, "syscall_dispatch"], event_duration[key, "vfs_read"] + event_duration[key, "vfs_write"], \
            event_duration[key, "block_submit"], event_duration[key, "block_drain"], \
            event_duration[key, "block_complete"], event_duration[key, "block_wait"], \
            event_duration[key, "wait_other"] + event_duration[key, "wait_futex"] + event_duration[key, "wait_mutex"] + event_duration[key, "wait_timer"] + event_duration[key, "wait_yield"], \
            dominant, dominant_duration, (wall > 0 ? dominant_duration * 100 / wall : 0))
        print line > summary
        if (workload_root[key]) print line > workload_summary
    }
}
' "$rows"

awk -F '\t' '
NR == 1 { next }
{
    total[$1]++
    if ($14 == "workload-root") root[$1]++
    else if ($14 == "other") other[$1]++
    else unclassified[$1]++
}
END {
    print "case\ttotal_records\tworkload_root_records\tother_records\tunclassified_records\tworkload_root_pct"
    for (case_id in total)
        printf "%s\t%d\t%d\t%d\t%d\t%.1f\n", case_id, total[case_id], root[case_id], \
            other[case_id], unclassified[case_id], total[case_id] ? root[case_id] * 100 / total[case_id] : 0
}
' "$rows" >"$attribution"

awk -F '\t' 'NR > 1 && $3 != "" { print $1 "\t" $3 "\t" $6 "\t" $12 }' "$summary" >"$sys_rows"
awk -F '\t' 'NR == 1 || ($8 + $9 + $10 + $11 + $12) > 0' "$summary" >"$io_summary"

capacity=$(tr -d '\r' <"$log" | awk -F '[ =]' '
function value(name,    i) {
    for (i = 1; i < NF; i++) if ($i == name) return $(i + 1)
    return 0
}
/^@@PROFILE_TRACE_BEGIN / { active = value("phase") == "after"; next }
/^@@PROFILE_TRACE_END / { active = 0; next }
active && /^state=/ { slots = value("slots_per_cpu") + 0; next }
active && /^cpu=/ && / retained=/ {
    retained = value("retained") + 0
    if (retained > max_retained) max_retained = retained
}
END {
    utilization = slots > 0 ? max_retained * 100 / slots : 0
    printf "TRACE_CAPACITY max_retained=%d slots_per_cpu=%d utilization_pct=%.1f warning=%s", \
        max_retained, slots, utilization, utilization >= 80 ? "near_capacity" : "none"
}
')

echo "PROFILE_ANALYSIS version=1 top=$top"
echo "$capacity"
echo "WORKLOAD_ATTRIBUTION"
cat "$attribution"
echo
echo "WORKLOAD_ROOT_SPANS"
head -n 1 "$workload_summary"
tail -n +2 "$workload_summary" | sort -t "$tab" -k6,6nr | head -n "$top"
echo
echo "WORKLOAD_ROOT_BOTTLENECKS"
head -n 1 "$workload_bottlenecks"
tail -n +2 "$workload_bottlenecks" | sort -t "$tab" -k5,5nr | head -n "$top"
echo
echo "IO_CRITICAL_PATHS"
head -n 1 "$io_summary"
tail -n +2 "$io_summary" | sort -t "$tab" -k6,6nr | head -n "$top"

echo
echo "SPAN_SUMMARY"
head -n 1 "$summary"
tail -n +2 "$summary" | sort -t "$tab" -k6,6nr | head -n "$top"

echo
echo "SYSCALL_SUMMARY"
printf 'case\tsyscall\tspans\twall_total_us\tmean_wall_us\tp50_wall_us\tp95_wall_us\tp99_wall_us\tblock_wait_total_us\n'
cut -f1,2 "$sys_rows" | sort -u | while IFS="$tab" read -r case_id syscall; do
    values=$tmp/values
    awk -F '\t' -v case_id="$case_id" -v syscall="$syscall" '$1 == case_id && $2 == syscall { print $3 }' "$sys_rows" | sort -n >"$values"
    awk -v case_id="$case_id" -v syscall="$syscall" '
    {
        value[NR] = $1
        total += $1
    }
    END {
        if (NR > 0) {
            p50 = value[int((NR * 50 + 99) / 100)]
            p95 = value[int((NR * 95 + 99) / 100)]
            p99 = value[int((NR * 99 + 99) / 100)]
            printf "%s\t%s\t%d\t%.3f\t%.3f\t%.3f\t%.3f\t%.3f\t", case_id, syscall, NR, total, total / NR, p50, p95, p99
        }
    }
    ' "$values"
    awk -F '\t' -v case_id="$case_id" -v syscall="$syscall" '$1 == case_id && $2 == syscall { total += $4 } END { printf "%.3f\n", total }' "$sys_rows"
done

echo
echo "TOP_BOTTLENECKS"
head -n 1 "$bottlenecks"
tail -n +2 "$bottlenecks" | sort -t "$tab" -k5,5nr | head -n "$top"

echo
echo "TOP_IO_BOTTLENECKS"
head -n 1 "$io_bottlenecks"
tail -n +2 "$io_bottlenecks" | sort -t "$tab" -k5,5nr | head -n "$top"
