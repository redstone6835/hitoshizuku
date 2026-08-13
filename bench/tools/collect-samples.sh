#!/bin/sh
set -eu

usage() {
    cat >&2 <<'EOF'
用法:
  collect-samples.sh --system <name> --workload <name>
                     --mode <warm|cold> --boot <n> --counter-hz <hz>
                     --serial <path> --output <path>
EOF
    exit 2
}

system=
workload=
mode=
boot=
counter_hz=
serial=
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --system) [ "$#" -ge 2 ] || usage; system=$2; shift 2 ;;
        --workload) [ "$#" -ge 2 ] || usage; workload=$2; shift 2 ;;
        --mode) [ "$#" -ge 2 ] || usage; mode=$2; shift 2 ;;
        --boot) [ "$#" -ge 2 ] || usage; boot=$2; shift 2 ;;
        --counter-hz) [ "$#" -ge 2 ] || usage; counter_hz=$2; shift 2 ;;
        --serial) [ "$#" -ge 2 ] || usage; serial=$2; shift 2 ;;
        --output) [ "$#" -ge 2 ] || usage; output=$2; shift 2 ;;
        *) usage ;;
    esac
done

case "$system" in ''|*[!A-Za-z0-9._-]*) usage ;; esac
case "$workload" in ''|*[!A-Za-z0-9._-]*) usage ;; esac
case "$mode" in warm|cold) ;; *) usage ;; esac
[ -n "$boot" ] && [ -n "$counter_hz" ] && [ -n "$serial" ] && [ -n "$output" ] || usage
case "$boot" in ''|*[!0-9]*) usage ;; esac
case "$counter_hz" in ''|*[!0-9]*|0) usage ;; esac
[ -f "$serial" ] || { echo "serial 日志不存在: $serial" >&2; exit 2; }

mkdir -p "$(dirname "$output")"
tmp="$output.tmp.$$"
trap 'rm -f "$tmp"' EXIT HUP INT TERM
printf '%s\n' \
    'system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail' >"$tmp"

awk -v expected_system="$system" -v expected_workload="$workload" \
    -v expected_mode="$mode" -v expected_boot="$boot" -v expected_hz="$counter_hz" '
{
    sub(/\r$/, "")
}
function fail(message) {
    print "采样协议错误: " message > "/dev/stderr"
    bad = 1
}
function clear_values(    key) {
    for (key in values) delete values[key]
}
function read_fields(line, allowed,    fields, count, i, pair, key, value) {
    count = split(line, fields, " ")
    clear_values()
    for (i = 2; i <= count; i++) {
        if (index(fields[i], "=") == 0) {
            fail("字段不是 key=value")
            continue
        }
        split(fields[i], pair, "=")
        key = pair[1]
        value = substr(fields[i], length(key) + 2)
        if (index(" " allowed " ", " " key " ") == 0) {
            fail("未知字段 " key)
        } else if (key in values) {
            fail("重复字段 " key)
        } else {
            values[key] = value
        }
    }
}
function validate_identity() {
    if (values["system"] != expected_system ||
        values["workload"] != expected_workload ||
        values["mode"] != expected_mode ||
        values["boot"] != expected_boot)
        fail("system/workload/mode/boot 与命令行不匹配")
}
BEGIN { meta_count = 0; done_count = 0; samples = 0; bad = 0 }
index($0, "BENCH_META ") > 0 {
    read_fields(substr($0, index($0, "BENCH_META ")), "system workload mode boot counter counter_hz")
    validate_identity()
    if (values["counter"] != "rdtime" || values["counter_hz"] != expected_hz)
        fail("counter 元数据不匹配")
    meta_count++
    if (meta_count != 1) fail("重复 META marker")
    next
}
index($0, "BENCH_SAMPLE ") > 0 {
    read_fields(substr($0, index($0, "BENCH_SAMPLE ")), "system workload mode boot round sample_ticks status detail")
    validate_identity()
    if (meta_count != 1) fail("样本出现在 META 之前")
    if (values["round"] !~ /^[0-9]+$/ ||
        values["status"] !~ /^(ok|error|unavailable)$/)
        fail("round 或 status 无效")
    if (values["status"] == "ok" && values["sample_ticks"] !~ /^[0-9]+$/)
        fail("ok 样本缺少非负 sample_ticks")
    if (values["status"] != "ok" && values["sample_ticks"] != "")
        fail("失败样本不得携带 sample_ticks")
    detail = values["detail"]
    if (detail == "") detail = "-"
    print values["system"] "\t" values["workload"] "\t" values["mode"] "\t" \
        values["boot"] "\t" values["round"] "\t" values["sample_ticks"] "\t" \
        expected_hz "\t" values["status"] "\t" detail
    samples++
    next
}
index($0, "BENCH_DONE ") > 0 {
    read_fields(substr($0, index($0, "BENCH_DONE ")), "system workload mode boot status detail")
    validate_identity()
    done_count++
    if (done_count != 1) fail("重复 DONE marker")
    if (values["status"] != "ok") fail("DONE marker 未成功")
    next
}
END {
    if (meta_count != 1) fail("缺少唯一 META marker")
    if (samples == 0) fail("没有 BENCH_SAMPLE 样本")
    if (done_count != 1) fail("缺少唯一成功 DONE marker")
    if (bad) exit 2
}
' "$serial" >>"$tmp" || exit $?

mv "$tmp" "$output"
trap - EXIT HUP INT TERM
