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
min_timed_samples=${PROFILE_REPORT_MIN_TIMED_SAMPLES:-32}
case "$user_base" in
    0x[0-9a-fA-F]*|[0-9]*) ;;
    *)
        echo "profile report: invalid user load base: $user_base" >&2
        exit 2
        ;;
esac
case "$min_timed_samples" in
    ''|*[!0-9]*|0)
        echo "profile report: PROFILE_REPORT_MIN_TIMED_SAMPLES must be a positive integer" >&2
        exit 2
        ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
clean_log="$tmp/log"
tr -d '\r' <"$log" | sed 's/^~ # //' >"$clean_log"

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
function has(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return 1
    }
    return 0
}
function fail(message) {
    print "profile report: " message > "/dev/stderr"
    invalid = 1
}
function valid_marker(kind,    marker_phase, marker_case) {
    marker_phase = value("phase")
    marker_case = value("case")
    if (marker_phase != "before" && marker_phase != "after")
        fail("invalid " kind " phase=" marker_phase)
    if (marker_case == "") fail("missing " kind " case id")
    return marker_case SUBSEP marker_phase
}
function begin_section(kind, key) {
    if (section_active) fail("nested " kind " section inside " section_kind)
    section_active = 1
    section_kind = kind
    section_key = key
}
function end_section(kind, key) {
    if (!section_active) fail(kind " end without begin")
    else if (section_kind != kind) fail(kind " end while " section_kind " section is active")
    else if (section_key != key) fail("mismatched " kind " end marker")
    section_active = 0
    section_kind = ""
    section_key = ""
}
function is_uint(number) {
    return number ~ /^[0-9]+$/
}
function normalized(number) {
    sub(/^0+/, "", number)
    return number == "" ? "0" : number
}
function uint_ge(left, right,    lhs, rhs) {
    lhs = normalized(left)
    rhs = normalized(right)
    if (length(lhs) != length(rhs)) return length(lhs) > length(rhs)
    return lhs >= rhs
}
function require_uint(name, context,    number) {
    if (!has(name)) {
        fail("missing " context " field " name)
        return "0"
    }
    number = value(name)
    if (!is_uint(number)) fail("invalid " context " field " name "=" number)
    return number
}
function require_cpu(name, context,    cpu) {
    if (!has(name)) {
        fail("missing " context " field " name)
        return "0"
    }
    cpu = value(name)
    if (!is_uint(cpu) && cpu != "mixed")
        fail("invalid " context " field " name "=" cpu)
    return cpu
}
function parse_hist(contents, prefix, context,    count, bucket_index, buckets) {
    count = split(contents, buckets, ",")
    if (count != 64) {
        fail(context " hist must contain exactly 64 buckets")
        return
    }
    for (bucket_index = 1; bucket_index <= 64; bucket_index++) {
        if (!is_uint(buckets[bucket_index]))
            fail("invalid " context " histogram bucket=" buckets[bucket_index])
        snapshot_hist[prefix SUBSEP (bucket_index - 1)] = buckets[bucket_index]
    }
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    stats_key = valid_marker("stats")
    begin_section("stats", stats_key)
    if (stats_begin[stats_key]++) fail("duplicate stats section for case=" case_id " phase=" phase)
    stats_active = 1
    next
}
/^@@PROFILE_STATS_END / {
    end_key = valid_marker("stats")
    if (!stats_active) fail("stats end without begin")
    else if (end_key != stats_key) fail("mismatched stats end marker")
    stats_end[stats_key]++
    stats_active = 0
    end_section("stats", end_key)
    next
}
/^@@PROFILE_SAMPLES_BEGIN / {
    samples_phase = value("phase")
    samples_case = value("case")
    samples_key = valid_marker("samples")
    begin_section("samples", samples_key)
    if (samples_begin[samples_key]++) fail("duplicate samples section for case=" samples_case " phase=" samples_phase)
    samples_active = 1
    next
}
/^@@PROFILE_SAMPLES_END / {
    end_key = valid_marker("samples")
    if (!samples_active) fail("samples end without begin")
    else if (end_key != samples_key) fail("mismatched samples end marker")
    samples_end[samples_key]++
    samples_active = 0
    end_section("samples", end_key)
    next
}
/^@@PROFILE_TRACE_BEGIN / {
    trace_phase = value("phase")
    trace_case = value("case")
    trace_key = valid_marker("trace")
    begin_section("trace", trace_key)
    if (trace_begin[trace_key]++) fail("duplicate trace section for case=" trace_case " phase=" trace_phase)
    trace_active = 1
    next
}
/^@@PROFILE_TRACE_END / {
    end_key = valid_marker("trace")
    if (!trace_active) fail("trace end without begin")
    else if (end_key != trace_key) fail("mismatched trace end marker")
    trace_end[trace_key]++
    trace_active = 0
    end_section("trace", end_key)
    next
}
/^@@PROFILE_META_BEGIN / {
    meta_phase = value("phase")
    meta_case = value("case")
    meta_key = valid_marker("metadata")
    begin_section("metadata", meta_key)
    if (seen_meta[meta_key]++) fail("duplicate metadata for case=" meta_case " phase=" meta_phase)
    meta_active = 1
    next
}
/^@@PROFILE_META_END / {
    end_key = valid_marker("metadata")
    if (!meta_active) fail("metadata end without begin")
    else if (end_key != meta_key) fail("mismatched metadata end marker")
    meta_end[meta_key]++
    meta_active = 0
    end_section("metadata", end_key)
    next
}
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
    require_uint("session", "stats header")
    generations[key] = require_uint("generation", "stats header")
    config_count = split("counter_hz event_mask sampling trace timing_shift effective_timing_shift timing_sampler", config_names, " ")
    for (config_index = 1; config_index <= config_count; config_index++) {
        config_name = config_names[config_index]
        config_value = value(config_name)
        if (config_value == "") fail("missing stats config " config_name " for case=" case_id " phase=" phase)
        configs[key SUBSEP config_name] = config_value
    }
    if (has("timing_sampler")) {
        if (value("timing_sampler") == "") fail("empty stats config timing_sampler for case=" case_id " phase=" phase)
        configs[key SUBSEP "timing_sampler"] = value("timing_sampler")
        seen_timing_sampler[key] = 1
    }
    cases[case_id] = 1
    next
}
stats_active && /^cpu=/ && / event=/ {
    cpu = require_cpu("cpu", "event")
    event = value("event")
    if (event == "") fail("missing event field event")
    event_key = case_id SUBSEP phase SUBSEP cpu SUBSEP event
    if (seen_event[event_key]++)
        fail("duplicate event row for case=" case_id " phase=" phase " cpu=" cpu " event=" event)
    event_fields_count = split("calls timed_samples cycles bytes packets sampled_wall_ns sampled_on_cpu_ns sampled_off_cpu_ns sampled_max_latency_ns migrations", event_fields, " ")
    for (event_field_index = 1; event_field_index <= event_fields_count; event_field_index++) {
        event_field = event_fields[event_field_index]
        snapshot_value[event_key SUBSEP event_field] = require_uint(event_field, "event")
    }
    if (!uint_ge(snapshot_value[event_key SUBSEP "calls"], snapshot_value[event_key SUBSEP "timed_samples"]))
        fail("event timed_samples exceeds calls for case=" case_id " phase=" phase " cpu=" cpu " event=" event)
    if (!has("hist")) fail("missing event field hist")
    else parse_hist(value("hist"), event_key, "event")
    next
}
stats_active && /^cpu=/ && / metric=/ {
    cpu = require_cpu("cpu", "metric")
    metric = value("metric")
    if (metric == "") fail("missing metric field metric")
    metric_key = case_id SUBSEP phase SUBSEP cpu SUBSEP metric
    if (seen_metric[metric_key]++)
        fail("duplicate metric row for case=" case_id " phase=" phase " cpu=" cpu " metric=" metric)
    metric_fields_count = split("observations sum max", metric_fields, " ")
    for (metric_field_index = 1; metric_field_index <= metric_fields_count; metric_field_index++) {
        metric_field = metric_fields[metric_field_index]
        metric_value[metric_key SUBSEP metric_field] = require_uint(metric_field, "metric")
    }
    if (!has("hist")) fail("missing metric field hist")
    else parse_hist(value("hist"), metric_key, "metric")
    next
}
samples_active && /^state=/ {
    if (seen_samples_header[samples_key]++) fail("duplicate samples header for case=" samples_case " phase=" samples_phase)
    if (value("state") != "frozen" || value("enabled") != "0") fail("samples capture is not frozen for case=" samples_case " phase=" samples_phase)
    sample_sessions[samples_key] = value("session")
    require_uint("session", "samples header")
    sample_generations[samples_key] = require_uint("generation", "samples header")
    sample_header_sampling[samples_key] = require_uint("sampling", "samples header")
    require_uint("slots_per_cpu", "samples header")
    next
}
samples_active && /^cpu=/ && / dropped_samples=/ {
    cpu = require_uint("cpu", "samples")
    sample_cpu_key = samples_key SUBSEP cpu
    if (seen_sample_cpu[sample_cpu_key]++) fail("duplicate dropped_samples row for case=" samples_case " phase=" samples_phase " cpu=" cpu)
    sample_dropped[sample_cpu_key] = require_uint("dropped_samples", "samples")
    next
}
samples_active && /^cpu=/ && / mode=/ {
    cpu = require_uint("cpu", "PC sample")
    require_uint("task", "PC sample")
    if (value("mode") != "kernel" && value("mode") != "user") fail("invalid PC sample mode=" value("mode"))
    if (value("pc") !~ /^0x[0-9a-fA-F]+$/) fail("invalid PC sample pc=" value("pc"))
    require_uint("samples", "PC sample")
    next
}
trace_active && /^state=/ {
    if (seen_trace_header[trace_key]++) fail("duplicate trace header for case=" trace_case " phase=" trace_phase)
    if (value("state") != "frozen" || value("enabled") != "0") fail("trace capture is not frozen for case=" trace_case " phase=" trace_phase)
    trace_sessions[trace_key] = value("session")
    require_uint("session", "trace header")
    trace_generations[trace_key] = require_uint("generation", "trace header")
    trace_header_count = split("trace counter_hz slots_per_cpu record_bytes format_version", trace_header_fields, " ")
    for (trace_header_index = 1; trace_header_index <= trace_header_count; trace_header_index++) {
        trace_header_field = trace_header_fields[trace_header_index]
        trace_header_value[trace_key SUBSEP trace_header_field] = require_uint(trace_header_field, "trace header")
    }
    next
}
trace_active && /^cpu=/ && / first_sequence=/ {
    cpu = require_uint("cpu", "trace window")
    trace_cpu_key = trace_key SUBSEP cpu
    if (seen_trace_cpu[trace_cpu_key]++) fail("duplicate trace window for case=" trace_case " phase=" trace_phase " cpu=" cpu)
    trace_window_count = split("first_sequence next_sequence retained overwritten", trace_window_fields, " ")
    for (trace_window_index = 1; trace_window_index <= trace_window_count; trace_window_index++) {
        trace_window_field = trace_window_fields[trace_window_index]
        trace_window[trace_cpu_key SUBSEP trace_window_field] = require_uint(trace_window_field, "trace window")
    }
    next
}
END {
    if (stats_active) fail("unterminated stats section")
    if (samples_active) fail("unterminated samples section")
    if (trace_active) fail("unterminated trace section")
    if (meta_active) fail("unterminated metadata section")
    if (section_active) fail("unterminated " section_kind " section")
    required_count = split("arch cpu_online kernel_release kernel_features kernel_image_id rootfs_image_id workload workload_exit_status cmdline control", required, " ")
    stable_count = split("arch cpu_online kernel_release kernel_features kernel_image_id rootfs_image_id workload cmdline", stable, " ")
    for (case_id in cases) {
        before = case_id SUBSEP "before"
        after = case_id SUBSEP "after"
        if (!seen_stats[before] || !seen_stats[after])
            fail("missing stats header for case=" case_id)
        else if (sessions[before] == "" || sessions[before] != sessions[after])
            fail("session mismatch for case=" case_id)
        else if (!uint_ge(generations[after], generations[before]))
            fail("stats generation is not monotonic for case=" case_id)
        if (stats_begin[before] != 1 || stats_end[before] != 1 || stats_begin[after] != 1 || stats_end[after] != 1)
            fail("incomplete stats section for case=" case_id)
        config_count = split("counter_hz event_mask sampling trace timing_shift effective_timing_shift timing_sampler", config_names, " ")
        for (i = 1; i <= config_count; i++) {
            name = config_names[i]
            if (configs[before SUBSEP name] != configs[after SUBSEP name])
                fail("stats config mismatch for case=" case_id " field=" name)
        }
        if (seen_timing_sampler[before] || seen_timing_sampler[after]) {
            if (!seen_timing_sampler[before] || !seen_timing_sampler[after] || configs[before SUBSEP "timing_sampler"] != configs[after SUBSEP "timing_sampler"])
                fail("stats config mismatch for case=" case_id " field=timing_sampler")
        }
        if (samples_begin[before] || samples_begin[after]) {
            if (samples_begin[before] != 1 || samples_end[before] != 1 || samples_begin[after] != 1 || samples_end[after] != 1)
                fail("incomplete samples section for case=" case_id)
            if (!seen_samples_header[before] || !seen_samples_header[after] || sample_sessions[before] != sessions[before] || sample_sessions[after] != sessions[after])
                fail("samples session mismatch for case=" case_id)
            if (!uint_ge(sample_generations[after], sample_generations[before]))
                fail("samples generation is not monotonic for case=" case_id)
            if (sample_header_sampling[before] != configs[before SUBSEP "sampling"] || sample_header_sampling[after] != configs[after SUBSEP "sampling"])
                fail("samples header disagrees with stats config for case=" case_id)
        }
        if (trace_begin[before] || trace_begin[after]) {
            if (trace_begin[before] != 1 || trace_end[before] != 1 || trace_begin[after] != 1 || trace_end[after] != 1)
                fail("incomplete trace section for case=" case_id)
            if (!seen_trace_header[before] || !seen_trace_header[after] || trace_sessions[before] != sessions[before] || trace_sessions[after] != sessions[after])
                fail("trace session mismatch for case=" case_id)
            if (!uint_ge(trace_generations[after], trace_generations[before]))
                fail("trace generation is not monotonic for case=" case_id)
            if (trace_header_value[before SUBSEP "trace"] != configs[before SUBSEP "trace"] || trace_header_value[after SUBSEP "trace"] != configs[after SUBSEP "trace"] || \
                trace_header_value[before SUBSEP "counter_hz"] != configs[before SUBSEP "counter_hz"] || trace_header_value[after SUBSEP "counter_hz"] != configs[after SUBSEP "counter_hz"])
                fail("trace header disagrees with stats config for case=" case_id)
        }
        if (!seen_meta[before] || !seen_meta[after]) {
            fail("missing metadata for case=" case_id)
            continue
        }
        if (meta_end[before] != 1 || meta_end[after] != 1)
            fail("incomplete metadata section for case=" case_id)
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
        timing_sampler_before = before SUBSEP "timing_sampler"
        timing_sampler_after = after SUBSEP "timing_sampler"
        if (seen_meta_field[timing_sampler_before] || seen_meta_field[timing_sampler_after]) {
            if (!seen_meta_field[timing_sampler_before] || !seen_meta_field[timing_sampler_after] || metadata[timing_sampler_before] != metadata[timing_sampler_after])
                fail("metadata mismatch for case=" case_id " field=timing_sampler")
        }
    }
    event_fields_count = split("calls timed_samples cycles bytes packets sampled_wall_ns sampled_on_cpu_ns sampled_off_cpu_ns sampled_max_latency_ns migrations", event_fields, " ")
    for (event_key in seen_event) {
        split(event_key, event_parts, SUBSEP)
        if (event_parts[2] != "after") continue
        before_event = event_parts[1] SUBSEP "before" SUBSEP event_parts[3] SUBSEP event_parts[4]
        for (i = 1; i <= event_fields_count; i++) {
            name = event_fields[i]
            before_value = seen_event[before_event] ? snapshot_value[before_event SUBSEP name] : "0"
            if (!uint_ge(snapshot_value[event_key SUBSEP name], before_value))
                fail("event counter is not monotonic for case=" event_parts[1] " cpu=" event_parts[3] " event=" event_parts[4] " field=" name)
        }
        for (i = 0; i < 64; i++) {
            before_value = seen_event[before_event] ? snapshot_hist[before_event SUBSEP i] : "0"
            if (!uint_ge(snapshot_hist[event_key SUBSEP i], before_value))
                fail("event histogram is not monotonic for case=" event_parts[1] " cpu=" event_parts[3] " event=" event_parts[4] " bucket=" i)
        }
        before_calls = seen_event[before_event] ? snapshot_value[before_event SUBSEP "calls"] : "0"
        before_timed = seen_event[before_event] ? snapshot_value[before_event SUBSEP "timed_samples"] : "0"
        delta_calls = snapshot_value[event_key SUBSEP "calls"] - before_calls
        delta_timed = snapshot_value[event_key SUBSEP "timed_samples"] - before_timed
        if (delta_timed > delta_calls)
            fail("event interval timed_samples exceeds calls for case=" event_parts[1] " cpu=" event_parts[3] " event=" event_parts[4])
    }
    for (event_key in seen_event) {
        split(event_key, event_parts, SUBSEP)
        if (event_parts[2] == "before") {
            after_event = event_parts[1] SUBSEP "after" SUBSEP event_parts[3] SUBSEP event_parts[4]
            if (!seen_event[after_event]) fail("event row disappeared after capture for case=" event_parts[1] " cpu=" event_parts[3] " event=" event_parts[4])
        }
    }
    metric_fields_count = split("observations sum max", metric_fields, " ")
    for (metric_key in seen_metric) {
        split(metric_key, metric_parts, SUBSEP)
        if (metric_parts[2] != "after") continue
        before_metric = metric_parts[1] SUBSEP "before" SUBSEP metric_parts[3] SUBSEP metric_parts[4]
        for (i = 1; i <= metric_fields_count; i++) {
            name = metric_fields[i]
            before_value = seen_metric[before_metric] ? metric_value[before_metric SUBSEP name] : "0"
            if (!uint_ge(metric_value[metric_key SUBSEP name], before_value))
                fail("metric counter is not monotonic for case=" metric_parts[1] " cpu=" metric_parts[3] " metric=" metric_parts[4] " field=" name)
        }
        for (i = 0; i < 64; i++) {
            before_value = seen_metric[before_metric] ? snapshot_hist[before_metric SUBSEP i] : "0"
            if (!uint_ge(snapshot_hist[metric_key SUBSEP i], before_value))
                fail("metric histogram is not monotonic for case=" metric_parts[1] " cpu=" metric_parts[3] " metric=" metric_parts[4] " bucket=" i)
        }
    }
    for (metric_key in seen_metric) {
        split(metric_key, metric_parts, SUBSEP)
        if (metric_parts[2] == "before") {
            after_metric = metric_parts[1] SUBSEP "after" SUBSEP metric_parts[3] SUBSEP metric_parts[4]
            if (!seen_metric[after_metric]) fail("metric row disappeared after capture for case=" metric_parts[1] " cpu=" metric_parts[3] " metric=" metric_parts[4])
        }
    }
    for (sample_cpu_key in seen_sample_cpu) {
        split(sample_cpu_key, sample_parts, SUBSEP)
        if (sample_parts[2] != "after") continue
        before_sample = sample_parts[1] SUBSEP "before" SUBSEP sample_parts[3]
        before_value = seen_sample_cpu[before_sample] ? sample_dropped[before_sample] : "0"
        if (!uint_ge(sample_dropped[sample_cpu_key], before_value))
            fail("dropped_samples is not monotonic for case=" sample_parts[1] " cpu=" sample_parts[3])
    }
    for (sample_cpu_key in seen_sample_cpu) {
        split(sample_cpu_key, sample_parts, SUBSEP)
        if (sample_parts[2] == "before") {
            after_sample = sample_parts[1] SUBSEP "after" SUBSEP sample_parts[3]
            if (!seen_sample_cpu[after_sample]) fail("samples CPU row disappeared after capture for case=" sample_parts[1] " cpu=" sample_parts[3])
        }
    }
    for (trace_cpu_key in seen_trace_cpu) {
        split(trace_cpu_key, trace_parts, SUBSEP)
        if (trace_parts[2] != "after") continue
        before_trace = trace_parts[1] SUBSEP "before" SUBSEP trace_parts[3]
        trace_window_count = split("first_sequence next_sequence retained overwritten", trace_window_fields, " ")
        for (i = 1; i <= trace_window_count; i++) {
            name = trace_window_fields[i]
            before_value = seen_trace_cpu[before_trace] ? trace_window[before_trace SUBSEP name] : "0"
            if (!uint_ge(trace_window[trace_cpu_key SUBSEP name], before_value))
                fail("trace window is not monotonic for case=" trace_parts[1] " cpu=" trace_parts[3] " field=" name)
        }
    }
    for (trace_cpu_key in seen_trace_cpu) {
        split(trace_cpu_key, trace_parts, SUBSEP)
        if (trace_parts[2] == "before") {
            after_trace = trace_parts[1] SUBSEP "after" SUBSEP trace_parts[3]
            if (!seen_trace_cpu[after_trace]) fail("trace CPU window disappeared after capture for case=" trace_parts[1] " cpu=" trace_parts[3])
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
awk -v min_timed="$min_timed_samples" '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
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
    timed[key] += value("timed_samples")
    cycles[key] += value("cycles")
    bytes[key] += value("bytes")
    packets[key] += value("packets")
    sampled_wall = value("sampled_wall_ns")
    if (sampled_wall == "") sampled_wall = value("wall_ns")
    sampled_oncpu = value("sampled_on_cpu_ns")
    if (sampled_oncpu == "") sampled_oncpu = value("on_cpu_ns")
    sampled_offcpu = value("sampled_off_cpu_ns")
    if (sampled_offcpu == "") sampled_offcpu = value("off_cpu_ns")
    wall[key] += sampled_wall
    oncpu[key] += sampled_oncpu
    offcpu[key] += sampled_offcpu
    migrations[key] += value("migrations")
    split(value("hist"), buckets, ",")
    for (i = 1; i <= 64; i++) hist[key, i - 1] += buckets[i]
    observed[key] = 1
    next
}
END {
    print "case\tevent\tcalls\ttimed_samples\tsample_ratio\ttiming_status\tcycles\tbytes\tpackets\tsampled_wall_ns\testimated_wall_ns\tmean_ns\tsampled_on_cpu_ns\testimated_on_cpu_ns\tsampled_off_cpu_ns\testimated_off_cpu_ns\tmigrations\toff_cpu%\tp50_ns\tp95_ns\tp99_ns"
    for (key in observed) {
        split(key, parts, SUBSEP)
        if (parts[3] != "after") continue
        before = parts[1] SUBSEP parts[2] SUBSEP "before"
        dcalls = calls[key] - calls[before]
        dtimed = timed[key] - timed[before]
        dcycles = cycles[key] - cycles[before]
        dbytes = bytes[key] - bytes[before]
        dpackets = packets[key] - packets[before]
        dwall = wall[key] - wall[before]
        don = oncpu[key] - oncpu[before]
        doff = offcpu[key] - offcpu[before]
        dmigrations = migrations[key] - migrations[before]
        ratio = dcalls ? dtimed / dcalls : 0
        if (dcalls == 0) status = "no-calls"
        else if (dtimed == 0) status = "invalid-no-samples"
        else if (dtimed > dcalls) status = "invalid-sample-count"
        else if (dtimed == dcalls) status = "exact"
        else if (dtimed < min_timed) status = "low-confidence"
        else status = "sampled"
        trustworthy = status == "exact" || status == "sampled"
        estimated_wall = trustworthy ? sprintf("%.0f", dwall * dcalls / dtimed) : "NA"
        estimated_on = trustworthy ? sprintf("%.0f", don * dcalls / dtimed) : "NA"
        estimated_off = trustworthy ? sprintf("%.0f", doff * dcalls / dtimed) : "NA"
        mean = trustworthy ? sprintf("%.2f", dwall / dtimed) : "NA"
        offpct = trustworthy ? sprintf("%.1f", dwall ? doff * 100 / dwall : 0) : "NA"
        p50 = trustworthy ? sprintf("%.0f", percentile(key, before, 50)) : "NA"
        p95 = trustworthy ? sprintf("%.0f", percentile(key, before, 95)) : "NA"
        p99 = trustworthy ? sprintf("%.0f", percentile(key, before, 99)) : "NA"
        printf "%s\t%s\t%.0f\t%.0f\t%.6f\t%s\t%.0f\t%.0f\t%.0f\t%.0f\t%s\t%s\t%.0f\t%s\t%.0f\t%s\t%.0f\t%s\t%s\t%s\t%s\n", \
            parts[1], parts[2], dcalls, dtimed, ratio, status, dcycles, dbytes, \
            dpackets, dwall, estimated_wall, mean, don, estimated_on, doff, \
            estimated_off, dmigrations, offpct, p50, p95, p99
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
    trace_phase = value("phase")
    trace_active = 1
    next
}
/^@@PROFILE_TRACE_END / { trace_active = 0; next }
trace_active && /^cpu=/ && / overwritten=/ {
    overwritten[trace_case, trace_phase] += value("overwritten") + 0
    next
}
trace_active && /^cpu=/ && / sequence=/ && / kind=task_spawn / {
    if (trace_phase != "after") next
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
    print "case\tenabled\tsamples\tdropped\ttrace_overwritten\tstatus"
    for (case_id in cases) {
        sample_delta = samples[case_id, "after"] - samples[case_id, "before"]
        dropped_delta = dropped[case_id, "after"] - dropped[case_id, "before"]
        overwritten_delta = overwritten[case_id, "after"] - overwritten[case_id, "before"]
        is_enabled = enabled[case_id, "after"]
        status = !is_enabled ? "disabled" : \
            (dropped_delta > 0 || overwritten_delta > 0 ? "invalid" : \
            (sample_delta > 0 ? "ok" : "no_samples"))
        printf "%s\t%d\t%d\t%d\t%d\t%s\n", case_id, is_enabled, sample_delta, \
            dropped_delta, overwritten_delta, status
    }
}
' "$clean_log" >"$tmp/sampling-health"

echo
echo "SAMPLING HEALTH"
cat "$tmp/sampling-health"
awk -F '\t' 'NR > 1 && $6 == "invalid" { print $1 }' \
    "$tmp/sampling-health" >"$tmp/invalid-sampling-cases"
awk -F '\t' 'NR > 1 && $6 == "no_samples" {
    print "profile report: warning: sampling enabled but no PC samples for case=" $1 > "/dev/stderr"
}
NR > 1 && $6 == "invalid" && $4 > 0 {
    print "profile report: warning: sampling attribution invalid because PC samples were dropped for case=" $1 > "/dev/stderr"
}
NR > 1 && $6 == "invalid" && $5 > 0 {
    print "profile report: warning: sampling/trace attribution invalid because trace records were overwritten for case=" $1 > "/dev/stderr"
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
FILENAME == ARGV[1] { invalid_sampling[$1] = 1; next }
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
    if (invalid_sampling[case_id]) next
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
' "$tmp/invalid-sampling-cases" "$clean_log" >"$tmp/samples"

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
