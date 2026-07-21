#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp)
bad=$(mktemp)
trap 'rm -f "$tmp" "$bad"' EXIT INT TERM

hist=$(awk 'BEGIN { for (i = 0; i < 64; i++) printf "%s%s", (i == 0 ? "" : ","), (i == 3 ? 2 : 0); print "" }')
zero=$(awk 'BEGIN { for (i = 0; i < 64; i++) printf "%s0", (i == 0 ? "" : ","); print "" }')

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
    echo "@@PROFILE_STATS_BEGIN phase=before case=smoke"
    echo "state=frozen enabled=0 session=1 generation=1 active_writers=0"
    echo "cpu=0 event=net_protocol_turn calls=0 cycles=0 bytes=0 packets=0 wall_ns=0 on_cpu_ns=0 off_cpu_ns=0 max_latency_ns=0 migrations=0 hist=$zero"
    echo "cpu=0 metric=ingress_ring_depth observations=0 sum=0 max=0 hist=$zero"
    echo "@@PROFILE_STATS_END phase=before case=smoke"
    echo "@@PROFILE_SAMPLES_BEGIN phase=before case=smoke"
    echo "cpu=0 dropped_samples=0"
    echo "@@PROFILE_SAMPLES_END phase=before case=smoke"
    echo "@@PROFILE_STATS_BEGIN phase=after case=smoke"
    echo "state=frozen enabled=0 session=1 generation=2 active_writers=0"
    echo "cpu=0 event=net_protocol_turn calls=2 cycles=20 bytes=64 packets=2 wall_ns=20 on_cpu_ns=12 off_cpu_ns=8 max_latency_ns=16 migrations=1 hist=$hist"
    echo "cpu=0 metric=ingress_ring_depth observations=2 sum=10 max=8 hist=$hist"
    echo "@@PROFILE_STATS_END phase=after case=smoke"
    echo "@@PROFILE_SAMPLES_BEGIN phase=after case=smoke"
    echo "cpu=0 dropped_samples=0"
    echo "cpu=0 mode=kernel pc=0x1000 samples=2"
    echo "@@PROFILE_SAMPLES_END phase=after case=smoke"
} >"$tmp"

output=$($root/scripts/profile-report.sh "$tmp")
printf '%s\n' "$output" | grep -q 'smoke.*net_protocol_turn.*2.*1.*40.0'
printf '%s\n' "$output" | grep -q 'smoke.*ingress_ring_depth.*2.*5.00.*8.*1'
printf '%s\n' "$output" | grep -q 'smoke.*kernel.*0x1000.*2'
printf '%s\n' "$output" | grep -q 'smoke.*kernel_image_id.*kernel-sha256'
sed '0,/dropped_samples=0/s//dropped_samples=1/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: dropped samples were accepted" >&2
    exit 1
fi
sed '0,/kernel_image_id=kernel-sha256/s//kernel_image_id=wrong-image/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: mismatched metadata was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_META_BEGIN phase=before /,/^@@PROFILE_META_END phase=before /d' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: incomplete metadata was accepted" >&2
    exit 1
fi
sed '0,/session=1/s//session=9/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: mismatched session was accepted" >&2
    exit 1
fi
echo "profile-report fixture: ok"
