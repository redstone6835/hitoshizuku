#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT INT TERM

hist=$(awk 'BEGIN { for (i = 0; i < 64; i++) printf "%s%s", (i == 0 ? "" : ","), (i == 3 ? 2 : 0); print "" }')
zero=$(awk 'BEGIN { for (i = 0; i < 64; i++) printf "%s0", (i == 0 ? "" : ","); print "" }')

{
    echo "@@PROFILE_STATS_BEGIN phase=before case=smoke"
    echo "state=frozen session=1 generation=1"
    echo "cpu=0 event=net_protocol_turn calls=0 cycles=0 bytes=0 packets=0 wall_ns=0 on_cpu_ns=0 off_cpu_ns=0 max_latency_ns=0 migrations=0 hist=$zero"
    echo "cpu=0 metric=ingress_ring_depth observations=0 sum=0 max=0 hist=$zero"
    echo "@@PROFILE_STATS_END phase=before case=smoke"
    echo "@@PROFILE_SAMPLES_BEGIN phase=before case=smoke"
    echo "cpu=0 dropped_samples=0"
    echo "@@PROFILE_SAMPLES_END phase=before case=smoke"
    echo "@@PROFILE_STATS_BEGIN phase=after case=smoke"
    echo "state=frozen session=1 generation=2"
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
echo "profile-report fixture: ok"
