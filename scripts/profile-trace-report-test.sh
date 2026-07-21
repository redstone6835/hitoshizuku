#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
log="$tmp/trace.log"
json="$tmp/trace.json"

{
    for phase in before after; do
        echo "@@PROFILE_META_BEGIN phase=$phase case=smoke"
        echo "arch=riscv64"
        echo "cpu_online=0-1"
        echo "kernel_release=mygo"
        echo "kernel_features=performance-profile"
        echo "kernel_image_id=kernel-sha256"
        echo "rootfs_image_id=rootfs-sha256"
        echo "workload=smoke-test"
        echo "workload_exit_status=$([ "$phase" = before ] && echo not-run || echo 0)"
        echo "cmdline=console=ttyS0"
        echo "control=state=frozen enabled=0"
        echo "@@PROFILE_META_END phase=$phase case=smoke"
    done
    echo "@@PROFILE_TRACE_BEGIN phase=before case=smoke"
    echo "state=frozen enabled=0 session=1 generation=2 active_writers=0 counter_hz=1000000 format_version=2"
    echo "cpu=0 first_sequence=0 next_sequence=0 retained=0 overwritten=0"
    echo "@@PROFILE_TRACE_END phase=before case=smoke"
    echo "@@PROFILE_TRACE_BEGIN phase=after case=smoke"
    echo "state=frozen enabled=0 session=1 generation=4 active_writers=0 counter_hz=1000000 format_version=2"
    echo "cpu=0 first_sequence=0 next_sequence=2 retained=2 overwritten=0"
    echo "cpu=0 sequence=0 session=1 generation=3 timestamp_cycles=100 duration_cycles=25 kind=scope event=vfs_read task=7 span=42 arg0=64 arg1=0"
    echo "cpu=0 sequence=1 session=1 generation=3 timestamp_cycles=130 duration_cycles=0 kind=sched_switch event=sched_switch task=7 span=42 arg0=7 arg1=8"
    echo "@@PROFILE_TRACE_END phase=after case=smoke"
} >"$log"

output=$($root/scripts/profile-trace-report.sh "$log" "$json")
printf '%s\n' "$output" | grep -q 'smoke.*100.*25.*vfs_read.*64'
printf '%s\n' "$output" | grep -q 'smoke.*130.*sched_switch.*7.*8'
grep -q '"name":"vfs_read"' "$json"
grep -q '"ph":"X"' "$json"
grep -q '"name":"sched_switch"' "$json"
grep -q '"ph":"i"' "$json"
sed 's/overwritten=0/overwritten=1/' "$log" >"$tmp/lost.log"
if $root/scripts/profile-trace-report.sh "$tmp/lost.log" >/dev/null 2>&1; then
    echo "profile-trace-report fixture: overwritten records were accepted" >&2
    exit 1
fi
sed '0,/rootfs_image_id=rootfs-sha256/s//rootfs_image_id=wrong-image/' "$log" >"$tmp/mismatch.log"
if $root/scripts/profile-trace-report.sh "$tmp/mismatch.log" >/dev/null 2>&1; then
    echo "profile-trace-report fixture: mismatched metadata was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_META_BEGIN phase=before /,/^@@PROFILE_META_END phase=before /d' "$log" >"$tmp/incomplete.log"
if $root/scripts/profile-trace-report.sh "$tmp/incomplete.log" >/dev/null 2>&1; then
    echo "profile-trace-report fixture: incomplete metadata was accepted" >&2
    exit 1
fi
sed '0,/session=1/s//session=9/' "$log" >"$tmp/session.log"
if $root/scripts/profile-trace-report.sh "$tmp/session.log" >/dev/null 2>&1; then
    echo "profile-trace-report fixture: mismatched session was accepted" >&2
    exit 1
fi
echo "profile-trace-report fixture: ok"
