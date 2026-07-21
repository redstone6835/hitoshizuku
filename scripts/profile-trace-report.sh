#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <serial-log> [chrome-trace.json]" >&2
    exit 2
fi

log=$1
chrome_json=${2:-}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
clean_log="$tmp/log"
rows="$tmp/rows"
sorted="$tmp/sorted"
tr -d '\r' <"$log" >"$clean_log"

if ! grep -q '^@@PROFILE_TRACE_BEGIN phase=after ' "$clean_log"; then
    echo "profile trace report: missing after trace marker" >&2
    exit 1
fi

awk -F '[ =]' '
function value(name,    i) {
    for (i = 1; i < NF; i++) if ($i == name) return $(i + 1)
    return ""
}
function fail(message) {
    print "profile trace report: " message > "/dev/stderr"
    invalid = 1
}
/^@@PROFILE_TRACE_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    current_session = ""
    next
}
/^@@PROFILE_TRACE_END / { active = 0; next }
/^@@PROFILE_META_BEGIN / {
    meta_phase = value("phase")
    meta_case = value("case")
    meta_key = meta_case SUBSEP meta_phase
    if (seen_meta[meta_key]++) fail("duplicate metadata for case=" meta_case " phase=" meta_phase)
    meta_active = 1
    next
}
/^@@PROFILE_META_END / { meta_active = 0; next }
meta_active && /^[a-z_]+=/ {
    name = $0
    sub(/=.*/, "", name)
    contents = $0
    sub(/^[^=]*=/, "", contents)
    field_key = meta_key SUBSEP name
    if (seen_meta_field[field_key]++) fail("duplicate metadata field " name " for case=" meta_case " phase=" meta_phase)
    metadata[field_key] = contents
    next
}
/^@@PROFILE_WORKLOAD / {
    marker_case = value("case")
    marker_pid = value("pid")
    if (seen_workload[marker_case]++)
        fail("duplicate workload marker for case=" marker_case)
    if (marker_case == "" || marker_pid !~ /^[0-9]+$/ || marker_pid == 0)
        fail("invalid workload marker")
    next
}
active && /^state=/ {
    key = case_id SUBSEP phase
    if (seen_header[key]++) fail("duplicate trace header for case=" case_id " phase=" phase)
    if (value("state") != "frozen") fail("capture is not frozen for case=" case_id " phase=" phase)
    if (value("enabled") != "0") fail("capture is still enabled for case=" case_id " phase=" phase)
    if (value("active_writers") != "0") fail("active writers remain for case=" case_id " phase=" phase)
    if (value("format_version") != "2") fail("unsupported trace format for case=" case_id " phase=" phase)
    if (value("counter_hz") + 0 <= 0) fail("invalid counter frequency for case=" case_id " phase=" phase)
    if (value("slots_per_cpu") + 0 <= 0) fail("invalid trace capacity for case=" case_id " phase=" phase)
    if (value("record_bytes") != "80") fail("invalid trace record size for case=" case_id " phase=" phase)
    current_session = value("session")
    sessions[key] = current_session
    frequencies[key] = value("counter_hz")
    capacities[key] = value("slots_per_cpu")
    record_sizes[key] = value("record_bytes")
    cases[case_id] = 1
    next
}
active && /^cpu=/ && / first_sequence=/ {
    if (value("overwritten") + 0 != 0)
        fail("overwritten trace records for case=" case_id " phase=" phase " cpu=" value("cpu"))
    next
}
active && /^cpu=/ && / sequence=/ {
    if (current_session == "" || value("session") != current_session)
        fail("record session mismatch for case=" case_id " phase=" phase " cpu=" value("cpu") " sequence=" value("sequence"))
    if (value("span") == "")
        fail("trace record has no span id for case=" case_id " phase=" phase)
}
END {
    required_count = split("arch cpu_online kernel_release kernel_features kernel_image_id rootfs_image_id workload workload_exit_status cmdline control", required, " ")
    stable_count = split("arch cpu_online kernel_release kernel_features kernel_image_id rootfs_image_id workload cmdline", stable, " ")
    for (case_id in cases) {
        before = case_id SUBSEP "before"
        after = case_id SUBSEP "after"
        if (!seen_header[before] || !seen_header[after])
            fail("missing trace header for case=" case_id)
        else if (sessions[before] == "" || sessions[before] != sessions[after])
            fail("session mismatch for case=" case_id)
        else if (frequencies[before] != frequencies[after])
            fail("counter frequency mismatch for case=" case_id)
        else if (capacities[before] != capacities[after] || record_sizes[before] != record_sizes[after])
            fail("trace layout mismatch for case=" case_id)
        if (!seen_meta[before] || !seen_meta[after]) {
            fail("missing metadata for case=" case_id)
            continue
        }
        for (i = 1; i <= required_count; i++) {
            name = required[i]
            if (!seen_meta_field[before SUBSEP name] || \
                (name != "cmdline" && metadata[before SUBSEP name] == ""))
                fail("missing metadata field " name " for case=" case_id " phase=before")
            if (!seen_meta_field[after SUBSEP name] || \
                (name != "cmdline" && metadata[after SUBSEP name] == ""))
                fail("missing metadata field " name " for case=" case_id " phase=after")
        }
        for (i = 1; i <= stable_count; i++) {
            name = stable[i]
            if (metadata[before SUBSEP name] != metadata[after SUBSEP name])
                fail("metadata mismatch for case=" case_id " field=" name)
        }
    }
    if (invalid) exit 1
}
' "$clean_log"

awk -F '[ =]' '
function value(name,    i) {
    for (i = 1; i < NF; i++) if ($i == name) return $(i + 1)
    return ""
}
function role_for(case_id, task,    root, current, parent, depth) {
    root = workload_pid[case_id]
    if (root == "") return "unclassified"
    if (task == root) return "workload-root"
    current = task
    for (depth = 0; depth < 128; depth++) {
        parent = task_parent[case_id SUBSEP current]
        if (parent == "" || parent == 0 || parent == current) break
        if (parent == root) return "workload-child"
        current = parent
    }
    return "other"
}
/^@@PROFILE_WORKLOAD / {
    workload_pid[value("case")] = value("pid")
    next
}
/^@@PROFILE_TRACE_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = phase == "after"
    hz = 0
    next
}
/^@@PROFILE_TRACE_END / { active = 0; next }
active && /^state=/ {
    hz = value("counter_hz") + 0
    next
}
active && /^cpu=/ && / sequence=/ {
    if (hz <= 0) next
    rows++
    row_case[rows] = case_id
    row_ts[rows] = value("timestamp_cycles") * 1000000 / hz
    row_dur[rows] = value("duration_cycles") * 1000000 / hz
    row_cpu[rows] = value("cpu")
    row_task[rows] = value("task")
    row_span[rows] = value("span")
    row_kind[rows] = value("kind")
    row_event[rows] = value("event")
    row_arg0[rows] = value("arg0")
    row_arg1[rows] = value("arg1")
    row_session[rows] = value("session")
    row_generation[rows] = value("generation")
    row_sequence[rows] = value("sequence")
    if (row_kind[rows] == "task_spawn")
        task_parent[case_id SUBSEP row_task[rows]] = row_arg0[rows]
}
END {
    for (i = 1; i <= rows; i++)
        printf "%s\t%.6f\t%.6f\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", \
            row_case[i], row_ts[i], row_dur[i], row_cpu[i], row_task[i], \
            row_span[i], row_kind[i], row_event[i], row_arg0[i], row_arg1[i], \
            row_session[i], row_generation[i], row_sequence[i], \
            role_for(row_case[i], row_task[i])
}
' "$clean_log" >"$rows"

if [ ! -s "$rows" ]; then
    echo "profile trace report: no trace records" >&2
    exit 1
fi

sort -t '	' -k1,1 -k2,2n -k4,4n -k13,13n "$rows" >"$sorted"
printf 'case\tts_us\tdur_us\tcpu\ttask\tspan\tkind\tevent\targ0\targ1\tsession\tgeneration\tsequence\trole\n'
cat "$sorted"

if [ -z "$chrome_json" ]; then
    exit 0
fi

awk -F '\t' '
function escape(value) {
    gsub(/\\/, "\\\\", value)
    gsub(/"/, "\\\"", value)
    gsub(/\t/, "\\t", value)
    gsub(/\n/, "\\n", value)
    return value
}
BEGIN { print "{\"traceEvents\":["; first = 1 }
{
    if (!first) print ","
    first = 0
    name = $7 == "scope" ? $8 : $7
    printf "{\"name\":\"%s\",\"cat\":\"%s\",", escape(name), escape($7)
    if ($3 > 0) {
        printf "\"ph\":\"X\",\"ts\":%.3f,\"dur\":%.3f,", $2, $3
    } else {
        printf "\"ph\":\"i\",\"s\":\"t\",\"ts\":%.3f,", $2
    }
    printf "\"pid\":1,\"tid\":%s,", $5
    printf "\"args\":{\"case\":\"%s\",\"cpu\":%s,", escape($1), $4
    printf "\"span\":\"%s\",\"event\":\"%s\",\"arg0\":\"%s\",\"arg1\":\"%s\",", \
        escape($6), escape($8), escape($9), escape($10)
    printf "\"session\":\"%s\",\"generation\":\"%s\",\"sequence\":\"%s\",\"role\":\"%s\"}}", \
        escape($11), escape($12), escape($13), escape($14)
}
END { print "\n]}" }
' "$sorted" >"$chrome_json"
