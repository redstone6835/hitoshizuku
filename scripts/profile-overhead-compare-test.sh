#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
printf '%s\n' 'WORKLOAD_RESULT ok=true elapsed_s=100.00' >"$tmp/formal"
printf '%s\n' 'WORKLOAD_RESULT ok=true elapsed_s=104.00' >"$tmp/counter"
printf '%s\n' 'WORKLOAD_RESULT ok=true elapsed_s=109.00' >"$tmp/sample"
"$root/scripts/profile-overhead-compare.py" "$tmp/formal" "$tmp/counter" "$tmp/sample" \
    --output "$tmp/result.json" >/dev/null
grep -q '"counter_pass": true' "$tmp/result.json"
grep -q '"sample_pass": true' "$tmp/result.json"
sed 's/elapsed_s=109.00/elapsed_s=111.00/' "$tmp/sample" >"$tmp/slow"
if "$root/scripts/profile-overhead-compare.py" "$tmp/formal" "$tmp/counter" "$tmp/slow" \
    >/dev/null 2>&1; then
    echo "profile-overhead-compare fixture: excessive overhead was accepted" >&2
    exit 1
fi
echo "profile-overhead-compare fixture: ok"
