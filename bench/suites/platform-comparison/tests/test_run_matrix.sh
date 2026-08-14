#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

mkdir -p "$tmp/artifacts" "$tmp/bin"
: >"$tmp/linux-kernel"
: >"$tmp/mygo-kernel"
for mode in warm cold; do
    for boot in 0 1 2; do
        [ "$mode" = cold ] && [ "$boot" -gt 0 ] && continue
        for system in linux mygo-tomori mygo-native; do
            : >"$tmp/artifacts/$system-clock-read-$mode-boot-$boot.cpio"
        done
    done
done

cat >"$tmp/bin/fake-runner" <<'EOF'
#!/bin/sh
set -eu
system=
mode=
boot=
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --system) system=$2; shift 2 ;;
        --mode) mode=$2; shift 2 ;;
        --boot) boot=$2; shift 2 ;;
        --output) output=$2; shift 2 ;;
        *) shift ;;
    esac
done
printf '%s:%s:%s\n' "$boot" "$system" "$mode" >>"$MATRIX_CALL_LOG"
mkdir -p "$output"
cat >"$output/samples.tsv" <<SAMPLES
system	workload	mode	boot	round	sample_ticks	counter_hz	status	detail
$system	clock-read	$mode	$boot	0	1	10000000	ok	-
SAMPLES
printf 'READY\n' >"$output/status"
EOF
chmod +x "$tmp/bin/fake-runner"

cat >"$tmp/bin/fake-summarizer" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >"$MATRIX_SUMMARY_LOG"
EOF
chmod +x "$tmp/bin/fake-summarizer"

cat >"$tmp/bin/fake-comparer" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >"$MATRIX_COMPARE_LOG"
EOF
chmod +x "$tmp/bin/fake-comparer"

export MATRIX_CALL_LOG="$tmp/calls.log"
export MATRIX_SUMMARY_LOG="$tmp/summary.log"
export MATRIX_COMPARE_LOG="$tmp/compare.log"
RUNNER="$tmp/bin/fake-runner" SUMMARIZER="$tmp/bin/fake-summarizer" \
    COMPARER="$tmp/bin/fake-comparer" \
    "$root/run-matrix.sh" --workload clock-read --cycles 3 \
    --kernel-linux "$tmp/linux-kernel" --kernel-mygo "$tmp/mygo-kernel" \
    --artifacts "$tmp/artifacts" --matrix-output "$tmp/results"

cat >"$tmp/expected.log" <<'EOF'
0:linux:warm
0:mygo-tomori:warm
0:mygo-native:warm
1:mygo-tomori:warm
1:mygo-native:warm
1:linux:warm
2:mygo-native:warm
2:linux:warm
2:mygo-tomori:warm
EOF
cmp "$tmp/expected.log" "$tmp/calls.log"
test "$(wc -l <"$tmp/results/samples.tsv")" -eq 10
rg -- '--expected-boots 3' "$tmp/summary.log" >/dev/null
rg -- '--modes warm' "$tmp/summary.log" >/dev/null
rg -- '--pair tomori-linux=linux,mygo-tomori' "$tmp/compare.log" >/dev/null
rg -- '--pair native-tomori=mygo-tomori,mygo-native' "$tmp/compare.log" >/dev/null
rg -- '--pair native-linux=linux,mygo-native' "$tmp/compare.log" >/dev/null

: >"$MATRIX_CALL_LOG"
RUNNER="$tmp/bin/fake-runner" SUMMARIZER="$tmp/bin/fake-summarizer" \
    COMPARER="$tmp/bin/fake-comparer" \
    "$root/run-matrix.sh" --workload clock-read --mode cold --cycles 1 \
    --rounds 1 --samples-per-round 1 --kernel-linux "$tmp/linux-kernel" \
    --kernel-mygo "$tmp/mygo-kernel" --artifacts "$tmp/artifacts" \
    --matrix-output "$tmp/cold-results"
cat >"$tmp/cold-expected.log" <<'EOF'
0:linux:cold
0:mygo-tomori:cold
0:mygo-native:cold
EOF
cmp "$tmp/cold-expected.log" "$tmp/calls.log"
test "$(wc -l <"$tmp/cold-results/samples.tsv")" -eq 4
rg -- '--expected-samples-per-boot 1' "$tmp/summary.log" >/dev/null
rg -- '--modes cold' "$tmp/summary.log" >/dev/null

echo "matrix tests: PASS"
