#!/bin/sh
set -eu

suite=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tools=$(CDPATH= cd -- "$suite/../../tools" && pwd)
tmp=$(mktemp -d)
trap 'find "$tmp" -type f -delete; find "$tmp" -type d -depth -empty -delete' EXIT HUP INT TERM

cat >"$tmp/summary.tsv" <<'EOF'
system	workload	mode	state	complete	valid_samples	failures	median_ns	p95_ns	p99_ns
linux	clock-read	warm	READY	1	10	0	100	110	120
mygo-tomori	clock-read	warm	READY	1	10	0	120	130	140
mygo-native	clock-read	warm	READY	1	10	0	119	129	139
EOF

python3 "$tools/compare.py" --input "$tmp/summary.tsv" \
    --output "$tmp/comparisons.tsv" \
    --pair tomori-linux=linux,mygo-tomori \
    --pair native-tomori=mygo-tomori,mygo-native \
    --pair native-linux=linux,mygo-native

test "$(wc -l <"$tmp/comparisons.tsv")" -eq 4
awk -F '\t' '$3 == "native-tomori" {
    exit !($4 == "mygo-tomori" && $5 == "mygo-native" && $6 == "READY" &&
        $7 == 120 && $8 == 119 && $9 == -1 && $10 == "-0.833")
}' "$tmp/comparisons.tsv"
test "$(find "$tmp" -maxdepth 1 -name '*.md' | wc -l)" -eq 0

if python3 "$tools/compare.py" --input "$tmp/summary.tsv" \
    --output "$tmp/missing.tsv" --pair missing=linux,other 2>/dev/null; then
    echo "缺少比较对象时应拒绝输出" >&2
    exit 1
fi

sed 's/READY/BROKEN/' "$tmp/summary.tsv" >"$tmp/incomplete.tsv"
if python3 "$tools/compare.py" --input "$tmp/incomplete.tsv" \
    --output "$tmp/incomplete-output.tsv" --pair systems=linux,mygo-native 2>/dev/null; then
    echo "未完成汇总应被拒绝" >&2
    exit 1
fi

echo "comparison tests: PASS"
