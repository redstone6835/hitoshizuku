#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 4 ]; then
    echo "usage: $0 <serial-log> [kernel-elf] [user-elf] [user-load-base]" >&2
    exit 2
fi

log=$1
elf=${2:-}
user_elf=${3:-}
user_base=${4:-0}
case "$user_base" in
    0x[0-9a-fA-F]*|[0-9]*) ;;
    *)
        echo "profile report: invalid user load base: $user_base" >&2
        exit 2
        ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
clean_log="$tmp/log"
tr -d '\r' <"$log" >"$clean_log"

for marker in \
    '@@PROFILE_STATS_BEGIN phase=before ' \
    '@@PROFILE_STATS_BEGIN phase=after '
do
    if ! grep -q "^$marker" "$clean_log"; then
        echo "profile report: missing marker: $marker" >&2
        exit 1
    fi
done

awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
}
function fail(message) {
    print "profile report: " message > "/dev/stderr"
    invalid = 1
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    stats_active = 1
    next
}
/^@@PROFILE_STATS_END / { stats_active = 0; next }
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
stats_active && /^state=/ {
    key = case_id SUBSEP phase
    if (seen_stats[key]++) fail("duplicate stats header for case=" case_id " phase=" phase)
    if (value("state") != "frozen") fail("capture is not frozen for case=" case_id " phase=" phase)
    if (value("enabled") != "0") fail("capture is still enabled for case=" case_id " phase=" phase)
    if (value("active_writers") != "0") fail("active writers remain for case=" case_id " phase=" phase)
    sessions[key] = value("session")
    cases[case_id] = 1
    next
}
/^@@PROFILE_SAMPLES_BEGIN / {
    sample_case = value("case")
    sample_phase = value("phase")
    samples_active = 1
    next
}
/^@@PROFILE_SAMPLES_END / { samples_active = 0; next }
samples_active && / dropped_samples=/ {
    if (value("dropped_samples") + 0 != 0)
        fail("dropped PC samples for case=" sample_case " phase=" sample_phase " cpu=" value("cpu"))
}
END {
    required_count = split("arch cpu_online kernel_release kernel_features kernel_image_id rootfs_image_id workload workload_exit_status cmdline control", required, " ")
    stable_count = split("arch cpu_online kernel_release kernel_features kernel_image_id rootfs_image_id workload cmdline", stable, " ")
    for (case_id in cases) {
        before = case_id SUBSEP "before"
        after = case_id SUBSEP "after"
        if (!seen_stats[before] || !seen_stats[after])
            fail("missing stats header for case=" case_id)
        else if (sessions[before] == "" || sessions[before] != sessions[after])
            fail("session mismatch for case=" case_id)
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

echo "METADATA"
awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
}
/^@@PROFILE_META_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = phase == "after"
    next
}
/^@@PROFILE_META_END / { active = 0; next }
active && /^[a-z_]+=/ {
    name = $0
    sub(/=.*/, "", name)
    contents = $0
    sub(/^[^=]*=/, "", contents)
    print case_id "\t" name "\t" contents
}
' "$clean_log" | {
    printf 'case\tfield\tvalue\n'
    sort
}

echo

echo "CASE_EVENTS"
awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return 0
}
function percentile(key, before, pct,    i, total, target, seen, count) {
    total = 0
    for (i = 0; i < 64; i++) total += hist[key, i] - hist[before, i]
    if (total <= 0) return 0
    target = int((total * pct + 99) / 100)
    seen = 0
    for (i = 0; i < 64; i++) {
        count = hist[key, i] - hist[before, i]
        seen += count
        if (seen >= target) return i == 0 ? 0 : 2 ^ (i - 1)
    }
    return 2 ^ 62
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    next
}
/^@@PROFILE_STATS_END / { active = 0; next }
active && /^cpu=/ && / event=/ {
    event = value("event")
    key = case_id SUBSEP event SUBSEP phase
    calls[key] += value("calls")
    cycles[key] += value("cycles")
    bytes[key] += value("bytes")
    packets[key] += value("packets")
    wall[key] += value("wall_ns")
    oncpu[key] += value("on_cpu_ns")
    offcpu[key] += value("off_cpu_ns")
    migrations[key] += value("migrations")
    split(value("hist"), buckets, ",")
    for (i = 1; i <= 64; i++) hist[key, i - 1] += buckets[i]
    observed[key] = 1
    next
}
END {
    print "case\tevent\tcalls\tcycles\tbytes\tpackets\twall_ns\ton_cpu_ns\toff_cpu_ns\tmigrations\toff_cpu%\tp50_ns\tp95_ns\tp99_ns"
    for (key in observed) {
        split(key, parts, SUBSEP)
        if (parts[3] != "after") continue
        before = parts[1] SUBSEP parts[2] SUBSEP "before"
        dcalls = calls[key] - calls[before]
        dcycles = cycles[key] - cycles[before]
        dbytes = bytes[key] - bytes[before]
        dpackets = packets[key] - packets[before]
        dwall = wall[key] - wall[before]
        don = oncpu[key] - oncpu[before]
        doff = offcpu[key] - offcpu[before]
        dmigrations = migrations[key] - migrations[before]
        offpct = dwall ? doff * 100 / dwall : 0
        printf "%s\t%s\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.1f\t%.0f\t%.0f\t%.0f\n", \
            parts[1], parts[2], dcalls, dcycles, dbytes, dpackets, \
            dwall, don, doff, dmigrations, offpct, percentile(key, before, 50), \
            percentile(key, before, 95), percentile(key, before, 99)
    }
}
' "$clean_log" | {
    IFS= read -r header
    printf '%s\n' "$header"
    sort
}

echo
echo "METRICS"
awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return 0
}
function percentile(key, before, pct,    i, total, target, seen, count) {
    total = 0
    for (i = 0; i < 64; i++) total += hist[key, i] - hist[before, i]
    if (total <= 0) return 0
    target = int((total * pct + 99) / 100)
    seen = 0
    for (i = 0; i < 64; i++) {
        count = hist[key, i] - hist[before, i]
        seen += count
        if (seen >= target) return i == 0 ? 0 : 2 ^ (i - 1)
    }
    return 2 ^ 62
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    next
}
/^@@PROFILE_STATS_END / { active = 0; next }
active && /^cpu=/ && / metric=/ {
    metric = value("metric")
    key = case_id SUBSEP metric SUBSEP phase
    observations[key] += value("observations")
    sum[key] += value("sum")
    if (value("max") > max[key]) max[key] = value("max")
    split(value("hist"), buckets, ",")
    for (i = 1; i <= 64; i++) hist[key, i - 1] += buckets[i]
    observed[key] = 1
}
END {
    print "case\tmetric\tobservations\tsum\tmean\tmax\tmax_exact\tp50\tp95\tp99"
    for (key in observed) {
        split(key, parts, SUBSEP)
        if (parts[3] != "after") continue
        before = parts[1] SUBSEP parts[2] SUBSEP "before"
        count = observations[key] - observations[before]
        total = sum[key] - sum[before]
        mean = count ? total / count : 0
        max_exact = observations[before] == 0 ? 1 : 0
        interval_max = max_exact ? max[key] : 0
        printf "%s\t%s\t%.0f\t%.0f\t%.2f\t%.0f\t%d\t%.0f\t%.0f\t%.0f\n", \
            parts[1], parts[2], count, total, mean, interval_max, max_exact, \
            percentile(key, before, 50), percentile(key, before, 95), \
            percentile(key, before, 99)
    }
}
' "$clean_log" | {
    IFS= read -r header
    printf '%s\n' "$header"
    sort
}

awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
}
function role_for(case_id, task,    root, current, parent, depth) {
    root = workload_pid[case_id]
    if (root == "" || task == 0) return "unclassified"
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
    trace_case = value("case")
    trace_active = value("phase") == "after"
    next
}
/^@@PROFILE_TRACE_END / { trace_active = 0; next }
trace_active && /^cpu=/ && / sequence=/ && / kind=task_spawn / {
    task_parent[trace_case SUBSEP value("task")] = value("arg0")
    next
}
/^@@PROFILE_SAMPLES_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    cases[case_id] = 1
    next
}
/^@@PROFILE_SAMPLES_END / { active = 0; next }
active && /^state=/ && / sampling=/ {
    enabled[case_id, phase] = value("sampling") + 0
    next
}
active && /^cpu=/ && / dropped_samples=/ {
    dropped[case_id, phase] += value("dropped_samples") + 0
    next
}
active && /^cpu=/ && / mode=/ {
    samples[case_id, phase] += value("samples") + 0
}
END {
    print "case\tenabled\tsamples\tdropped\tstatus"
    for (case_id in cases) {
        sample_delta = samples[case_id, "after"] - samples[case_id, "before"]
        dropped_delta = dropped[case_id, "after"] - dropped[case_id, "before"]
        is_enabled = enabled[case_id, "after"]
        status = !is_enabled ? "disabled" : \
            (dropped_delta > 0 ? "dropped" : (sample_delta > 0 ? "ok" : "no_samples"))
        printf "%s\t%d\t%d\t%d\t%s\n", case_id, is_enabled, sample_delta, dropped_delta, status
    }
}
' "$clean_log" >"$tmp/sampling-health"

echo
echo "SAMPLING HEALTH"
cat "$tmp/sampling-health"
awk -F '\t' 'NR > 1 && $5 == "no_samples" {
    print "profile report: warning: sampling enabled but no PC samples for case=" $1 > "/dev/stderr"
}' "$tmp/sampling-health"

awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
}
function role_for(case_id, task,    root, current, parent, depth) {
    root = workload_pid[case_id]
    if (root == "" || task == 0) return "unclassified"
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
    trace_case = value("case")
    trace_active = value("phase") == "after"
    next
}
/^@@PROFILE_TRACE_END / { trace_active = 0; next }
trace_active && /^cpu=/ && / sequence=/ && / kind=task_spawn / {
    task_parent[trace_case SUBSEP value("task")] = value("arg0")
    next
}
/^@@PROFILE_SAMPLES_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    next
}
/^@@PROFILE_SAMPLES_END / { active = 0; next }
active && /^cpu=/ && / mode=/ {
    task = value("task")
    if (task == "") task = 0
    key = case_id SUBSEP value("mode") SUBSEP value("pc") SUBSEP task SUBSEP phase
    samples[key] += value("samples")
    if (phase == "after") observed[key] = 1
}
END {
    for (key in observed) {
        split(key, parts, SUBSEP)
        before = parts[1] SUBSEP parts[2] SUBSEP parts[3] SUBSEP parts[4] SUBSEP "before"
        delta = samples[key] - samples[before]
        if (delta > 0) print parts[1] "\t" parts[2] "\t" parts[3] "\t" \
            parts[4] "\t" role_for(parts[1], parts[4]) "\t" delta
    }
}
' "$clean_log" >"$tmp/samples"

if [ ! -s "$tmp/samples" ]; then
    exit 0
fi

echo
echo "PC SAMPLES"

addr2line=
for candidate in llvm-addr2line rust-llvm-addr2line addr2line; do
    if command -v "$candidate" >/dev/null 2>&1; then
        addr2line=$candidate
        break
    fi
done

if [ -n "$addr2line" ] && [ -n "$elf" ] && [ -r "$elf" ]; then
    awk '$2 == "kernel" { print $3 }' "$tmp/samples" | sort -u >"$tmp/pcs"
    if [ -s "$tmp/pcs" ]; then
        "$addr2line" -e "$elf" -f -C -p <"$tmp/pcs" >"$tmp/symbols"
        paste "$tmp/pcs" "$tmp/symbols" >"$tmp/map"
    else
        : >"$tmp/map"
    fi
else
    : >"$tmp/map"
fi

: >"$tmp/user-map"
if [ -n "$addr2line" ] && [ -n "$user_elf" ] && [ -r "$user_elf" ]; then
    awk '$2 == "user" { print $3 }' "$tmp/samples" | sort -u | while IFS= read -r pc; do
        [ -n "$pc" ] || continue
        printf '%s\t0x%x\n' "$pc" "$((pc - user_base))"
    done >"$tmp/user-pcs"
    if [ -s "$tmp/user-pcs" ]; then
        cut -f2 "$tmp/user-pcs" | "$addr2line" -e "$user_elf" -f -C -p >"$tmp/user-symbols"
        paste "$tmp/user-pcs" "$tmp/user-symbols" | cut -f1,3 >"$tmp/user-map"
    fi
fi

awk -F '\t' '
    FILENAME == ARGV[1] { kernel_symbols[$1] = $2; next }
    FILENAME == ARGV[2] { user_symbols[$1] = $2; next }
    {
        if ($2 == "kernel") {
            symbol = kernel_symbols[$3]
            if (symbol == "") symbol = "[raw; pass kernel ELF as argument 2]"
        } else {
            symbol = user_symbols[$3]
            if (symbol == "") symbol = "[user ELF not supplied]"
        }
        print $0 "\t" symbol
    }
' "$tmp/map" "$tmp/user-map" "$tmp/samples" >"$tmp/resolved"

printf 'case\tmode\tpc\ttask\trole\tsamples\tshare%%\tsymbol\n'
awk -F '\t' '
    NR == FNR { total[$1] += $6; next }
    {
        share = total[$1] ? $6 * 100 / total[$1] : 0
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%.2f\t%s\n", \
            $1, $2, $3, $4, $5, $6, share, $7
    }
' "$tmp/resolved" "$tmp/resolved" | sort -t '	' -k1,1 -k6,6nr

echo
echo "TOP FUNCTIONS"
printf 'case\tsamples\tshare%%\tfunction\n'
awk -F '\t' '
    {
        function_name = $7
        sub(/ at .*/, "", function_name)
        key = $1 SUBSEP function_name
        samples[key] += $6
        total[$1] += $6
    }
    END {
        for (key in samples) {
            split(key, parts, SUBSEP)
            share = total[parts[1]] ? samples[key] * 100 / total[parts[1]] : 0
            printf "%s\t%.0f\t%.2f\t%s\n", parts[1], samples[key], share, parts[2]
        }
    }
' "$tmp/resolved" | sort -t '	' -k1,1 -k2,2nr
