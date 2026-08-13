#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tools=$(CDPATH= cd -- "$root/../../tools" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cat >"$tmp/samples.tsv" <<'EOF'
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
linux	clock-read	warm	0	0	10	10000000	ok	-
linux	clock-read	warm	0	1	20	10000000	ok	-
linux	clock-read	warm	1	0	30	10000000	ok	-
linux	clock-read	warm	1	1	40	10000000	ok	-
mygo-tomori	clock-read	warm	0	0	15	10000000	ok	-
mygo-tomori	clock-read	warm	0	1	25	10000000	ok	-
mygo-tomori	clock-read	warm	1	0	35	10000000	ok	-
mygo-tomori	clock-read	warm	1	1	45	10000000	ok	-
mygo-native	clock-read	warm	0	0	5	10000000	ok	-
mygo-native	clock-read	warm	0	1	10	10000000	ok	-
mygo-native	clock-read	warm	1	0	15	10000000	ok	-
mygo-native	clock-read	warm	1	1	20	10000000	ok	-
EOF
python3 "$tools/summarize.py" --input "$tmp/samples.tsv" --output-dir "$tmp/out" \
    --systems linux,mygo-tomori,mygo-native --workloads clock-read --modes warm \
    --expected-boots 2 --expected-rounds 2 --expected-samples-per-boot 2 \
    --counter-hz 10000000 --require-complete

awk -F '\t' '$1 == "linux" && $2 == "clock-read" && $3 == "warm" {
    exit !($4 == "READY" && $5 == 1 && $6 == 2 && $7 == 4 && $10 == 10 &&
        $11 == 20 && $12 == 40 && $13 == 40 && $16 == 2000)
}' "$tmp/out/summary.tsv"
awk -F '\t' '$1 == "mygo-native" && $2 == "clock-read" && $3 == "warm" && $4 == "READY" {
    exit !($11 == 10 && $16 == 1000)
}' "$tmp/out/summary.tsv"
awk -F '\t' '$1 == "linux" && $2 == "clock-read" && $3 == "warm" && $4 == 0 {
    exit !($5 == "READY" && $6 == 1 && $7 == 2 && $9 == 2 && $12 == 10 && $13 == 20)
}' "$tmp/out/boot-summary.tsv"
test ! -e "$tmp/out/comparisons.tsv"

cat >"$tmp/incomplete.tsv" <<'EOF'
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
linux	stream-write	warm	0	0	100	10000000	ok	-
EOF
if python3 "$tools/summarize.py" --input "$tmp/incomplete.tsv" \
    --output-dir "$tmp/incomplete-out" --systems linux --workloads stream-write \
    --modes warm \
    --expected-boots 2 --expected-rounds 1 --expected-samples-per-boot 1 \
    --counter-hz 10000000 --require-complete; then
    echo "缺少 boot 的矩阵不应通过" >&2
    exit 1
fi

cat >"$tmp/legacy.tsv" <<'EOF'
system	workload	round	sample_ns	status	detail
linux	clock-read	0	1	ok	-
EOF
if python3 "$tools/summarize.py" --input "$tmp/legacy.tsv" --output-dir "$tmp/legacy-out" \
    --systems linux --workloads clock-read --modes warm; then
    echo "旧 sample_ns 协议应被拒绝" >&2
    exit 1
fi

cat >"$tmp/bad-hz.tsv" <<'EOF'
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
linux	clock-read	warm	0	0	1	999	ok	-
EOF
if python3 "$tools/summarize.py" --input "$tmp/bad-hz.tsv" --output-dir "$tmp/bad-hz-out" \
    --systems linux --workloads clock-read --modes warm --counter-hz 10000000; then
    echo "不一致的 counter_hz 应被拒绝" >&2
    exit 1
fi

cat >"$tmp/bad-mode.tsv" <<'EOF'
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
linux	clock-read	tepid	0	0	1	10000000	ok	-
EOF
if python3 "$tools/summarize.py" --input "$tmp/bad-mode.tsv" --output-dir "$tmp/bad-mode-out" \
    --systems linux --workloads clock-read --modes warm --expected-boots 1 \
    --expected-rounds 1 --expected-samples-per-boot 1 --counter-hz 10000000; then
    echo "未知 mode 应被拒绝" >&2
    exit 1
fi

cat >"$tmp/cold-too-many.tsv" <<'EOF'
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
linux	heap-small	cold	0	0	100	10000000	ok	-
linux	heap-small	cold	0	0	101	10000000	ok	-
EOF
if python3 "$tools/summarize.py" --input "$tmp/cold-too-many.tsv" \
    --output-dir "$tmp/cold-too-many-out" --systems linux --workloads heap-small \
    --modes cold --expected-boots 1 --expected-rounds 1 --expected-samples-per-boot 1 \
    --counter-hz 10000000 --require-complete; then
    echo "cold 每 boot 多于一个样本不应通过" >&2
    exit 1
fi

cat >"$tmp/modes.tsv" <<'EOF'
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
linux	heap-small	cold	0	0	300	10000000	ok	-
linux	heap-small	warm	0	0	30	10000000	ok	-
EOF
python3 "$tools/summarize.py" --input "$tmp/modes.tsv" --output-dir "$tmp/modes-out" \
    --systems linux --workloads heap-small --modes cold,warm --expected-boots 1 \
    --expected-rounds 1 --expected-samples-per-boot 1 --counter-hz 10000000 \
    --require-complete
awk -F '\t' '$1 == "linux" && $2 == "heap-small" && $3 == "cold" {
    cold = ($4 == "READY" && $11 == 300)
}
$1 == "linux" && $2 == "heap-small" && $3 == "warm" {
    warm = ($4 == "READY" && $11 == 30)
}
END { exit !(cold && warm) }' "$tmp/modes-out/summary.tsv"

echo "summarize tests: PASS"

printf '%s\r\n' \
    'boot noise' \
    'BENCH_META system=mygo-tomori workload=stream-write mode=warm boot=2 counter=rdtime counter_hz=10000000' \
    'payloadBENCH_SAMPLE system=mygo-tomori workload=stream-write mode=warm boot=2 round=0 sample_ticks=123 status=ok' \
    'payloadpayloadBENCH_SAMPLE system=mygo-tomori workload=stream-write mode=warm boot=2 round=1 status=error detail=broken_pipe' \
    'payloadBENCH_DONE system=mygo-tomori workload=stream-write mode=warm boot=2 status=ok' >"$tmp/serial.log"
"$tools/collect-samples.sh" --system mygo-tomori --workload stream-write --mode warm --boot 2 \
    --counter-hz 10000000 --serial "$tmp/serial.log" --output "$tmp/collected.tsv"
awk -F '\t' 'NR == 2 {
    exit !($1 == "mygo-tomori" && $2 == "stream-write" && $3 == "warm" && $4 == 2 &&
        $5 == 0 && $6 == 123 && $7 == 10000000 && $8 == "ok")
}
NR == 3 { exit !($5 == 1 && $6 == "" && $8 == "error" && $9 == "broken_pipe") }' \
    "$tmp/collected.tsv"

cat >"$tmp/no-done.log" <<'EOF'
BENCH_META system=linux workload=clock-read mode=warm boot=0 counter=rdtime counter_hz=10000000
BENCH_SAMPLE system=linux workload=clock-read mode=warm boot=0 round=0 sample_ticks=1 status=ok
EOF
if "$tools/collect-samples.sh" --system linux --workload clock-read --mode warm --boot 0 \
    --counter-hz 10000000 --serial "$tmp/no-done.log" --output "$tmp/no-done.tsv"; then
    echo "缺少完成 marker 应被拒绝" >&2
    exit 1
fi

cat >"$tmp/legacy-record.log" <<'EOF'
BENCH_META system=linux workload=clock-read mode=warm boot=0 counter=rdtime counter_hz=10000000
BENCH_SAMPLE system=linux workload=clock-read mode=warm boot=0 round=0 sample_ns=1 status=ok
BENCH_DONE system=linux workload=clock-read mode=warm boot=0 status=ok
EOF
if "$tools/collect-samples.sh" --system linux --workload clock-read --mode warm --boot 0 \
    --counter-hz 10000000 --serial "$tmp/legacy-record.log" \
    --output "$tmp/legacy-record.tsv"; then
    echo "旧 sample_ns marker 应被拒绝" >&2
    exit 1
fi

cat >"$tmp/custom.log" <<'EOF'
BENCH_META system=prototype workload=custom-call mode=warm boot=0 counter=rdtime counter_hz=10000000
BENCH_SAMPLE system=prototype workload=custom-call mode=warm boot=0 round=0 sample_ticks=7 status=ok
BENCH_DONE system=prototype workload=custom-call mode=warm boot=0 status=ok
EOF
"$tools/collect-samples.sh" --system prototype --workload custom-call --mode warm --boot 0 \
    --counter-hz 10000000 --serial "$tmp/custom.log" --output "$tmp/custom.tsv"
awk -F '\t' 'NR == 2 { exit !($1 == "prototype" && $2 == "custom-call" && $6 == 7) }' \
    "$tmp/custom.tsv"

echo "collect tests: PASS"
