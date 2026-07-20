#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
log="$tmp/trace.log"
json="$tmp/trace.json"

{
    echo "@@PROFILE_TRACE_BEGIN phase=before case=smoke"
    echo "state=frozen counter_hz=1000000"
    echo "@@PROFILE_TRACE_END phase=before case=smoke"
    echo "@@PROFILE_TRACE_BEGIN phase=after case=smoke"
    echo "state=frozen counter_hz=1000000"
    echo "cpu=0 sequence=0 session=1 generation=3 timestamp_cycles=100 duration_cycles=25 kind=scope event=vfs_read task=7 arg0=64 arg1=0"
    echo "cpu=0 sequence=1 session=1 generation=3 timestamp_cycles=130 duration_cycles=0 kind=sched_switch event=sched_switch task=7 arg0=7 arg1=8"
    echo "@@PROFILE_TRACE_END phase=after case=smoke"
} >"$log"

output=$($root/scripts/profile-trace-report.sh "$log" "$json")
printf '%s\n' "$output" | grep -q 'smoke.*100.*25.*vfs_read.*64'
printf '%s\n' "$output" | grep -q 'smoke.*130.*sched_switch.*7.*8'
grep -q '"name":"vfs_read"' "$json"
grep -q '"ph":"X"' "$json"
grep -q '"name":"sched_switch"' "$json"
grep -q '"ph":"i"' "$json"
echo "profile-trace-report fixture: ok"
