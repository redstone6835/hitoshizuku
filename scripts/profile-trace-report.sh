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
    ts = value("timestamp_cycles") * 1000000 / hz
    dur = value("duration_cycles") * 1000000 / hz
    print case_id "\t" ts "\t" dur "\t" value("cpu") "\t" \
        value("task") "\t" value("kind") "\t" value("event") "\t" \
        value("arg0") "\t" value("arg1") "\t" value("session") "\t" \
        value("generation") "\t" value("sequence")
}
' "$clean_log" >"$rows"

if [ ! -s "$rows" ]; then
    echo "profile trace report: no trace records" >&2
    exit 1
fi

sort -t '	' -k1,1 -k2,2n -k4,4n -k12,12n "$rows" >"$sorted"
printf 'case\tts_us\tdur_us\tcpu\ttask\tkind\tevent\targ0\targ1\tsession\tgeneration\tsequence\n'
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
    name = $6 == "scope" ? $7 : $6
    printf "{\"name\":\"%s\",\"cat\":\"%s\",", escape(name), escape($6)
    if ($3 > 0) {
        printf "\"ph\":\"X\",\"ts\":%.3f,\"dur\":%.3f,", $2, $3
    } else {
        printf "\"ph\":\"i\",\"s\":\"t\",\"ts\":%.3f,", $2
    }
    printf "\"pid\":1,\"tid\":%s,", $5
    printf "\"args\":{\"case\":\"%s\",\"cpu\":%s,", escape($1), $4
    printf "\"event\":\"%s\",\"arg0\":\"%s\",\"arg1\":\"%s\",", \
        escape($7), escape($8), escape($9)
    printf "\"session\":\"%s\",\"generation\":\"%s\",\"sequence\":\"%s\"}}", \
        escape($10), escape($11), escape($12)
}
END { print "\n]}" }
' "$sorted" >"$chrome_json"
