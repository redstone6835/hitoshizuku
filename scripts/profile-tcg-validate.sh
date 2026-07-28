#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <report> <riscv64|loongarch64> <smp>" >&2
    exit 2
}

[ "$#" -eq 3 ] || usage
report=$1
expected_target=$2
expected_vcpus=$3
case "$expected_target" in riscv64|loongarch64) ;; *) usage ;; esac
case "$expected_vcpus" in ''|*[!0-9]*|0) usage ;; esac
[ -r "$report" ] || {
    echo "profile TCG validate: report is unreadable: $report" >&2
    exit 1
}

awk -v expected_target="$expected_target" -v expected_vcpus="$expected_vcpus" '
function is_uint(value) {
    return value ~ /^[0-9]+$/
}
function fail(message) {
    print "profile TCG validate: " message > "/dev/stderr"
    invalid = 1
}
$1 == "MYGO_TCG_PROFILE" {
    headers++
    for (field_index = 2; field_index <= NF; field_index++) {
        pieces = split($field_index, pair, "=")
        if (pieces != 2 || pair[1] == "" || pair[2] == "") {
            fail("malformed header field=" $field_index)
            continue
        }
        if (pair[1] in values) fail("duplicate header field=" pair[1])
        values[pair[1]] = pair[2]
    }
    next
}
END {
    if (headers != 1) fail("header count=" headers)
    required["version"] = 1
    required["target"] = 1
    required["configured_vcpus"] = 1
    required["active_vcpus"] = 1
    required["table_bits"] = 1
    required["table_slots"] = 1
    required["table_probes"] = 1
    required["counter_bytes_per_vcpu"] = 1
    required["translated_blocks"] = 1
    required["occupied_slots"] = 1
    required["dropped"] = 1
    required["collision_probes"] = 1
    required["max_probe"] = 1
    required["total_blocks"] = 1
    required["total_instructions"] = 1
    required["reported_hotspots"] = 1
    for (name in required)
        if (!(name in values)) fail("missing header field=" name)
    if (values["version"] != "2") fail("version=" values["version"])
    if (values["target"] != expected_target) fail("target=" values["target"])
    for (name in required)
        if (name != "target" && !is_uint(values[name])) fail("non-numeric " name)
    if (values["configured_vcpus"] + 0 != expected_vcpus + 0)
        fail("configured_vcpus=" values["configured_vcpus"])
    bits = values["table_bits"] + 0
    slots = 1
    for (bit_index = 0; bit_index < bits; bit_index++) slots *= 2
    if (bits < 12 || bits > 23 || values["table_slots"] + 0 != slots)
        fail("invalid table geometry")
    if (values["table_probes"] + 0 < 1 || values["counter_bytes_per_vcpu"] + 0 <= 16)
        fail("invalid counter layout")
    if (values["active_vcpus"] + 0 < 1 || values["active_vcpus"] + 0 > expected_vcpus + 0)
        fail("invalid active_vcpus")
    if (values["translated_blocks"] + 0 < 1 || values["occupied_slots"] + 0 < 1 ||
        values["occupied_slots"] + 0 > values["table_slots"] + 0)
        fail("empty or overfull hot table")
    if (values["collision_probes"] + 0 < values["translated_blocks"] + 0 ||
        values["max_probe"] + 0 < 1 ||
        values["max_probe"] + 0 > values["table_probes"] + 0)
        fail("invalid probe accounting")
    if (values["total_blocks"] + 0 < 1 || values["total_instructions"] + 0 < 1 ||
        values["reported_hotspots"] + 0 < 1 || values["reported_hotspots"] + 0 > 4096)
        fail("empty execution counters")
    exit invalid
}
' "$report"
