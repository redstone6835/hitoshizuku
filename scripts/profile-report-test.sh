#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp)
bad=$(mktemp)
trap 'rm -f "$tmp" "$bad" "$bad.err"' EXIT INT TERM

hist=$(awk 'BEGIN { for (i = 0; i < 64; i++) printf "%s%s", (i == 0 ? "" : ","), (i == 3 ? 2 : 0); print "" }')
zero=$(awk 'BEGIN { for (i = 0; i < 64; i++) printf "%s0", (i == 0 ? "" : ","); print "" }')

{
    for phase in before after; do
        if [ "$phase" = before ]; then
            echo "~ # @@PROFILE_META_BEGIN phase=$phase case=smoke"
        else
            echo "@@PROFILE_META_BEGIN phase=$phase case=smoke"
        fi
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
    echo "@@PROFILE_WORKLOAD case=smoke pid=7"
    echo "@@PROFILE_STATS_BEGIN phase=before case=smoke"
    echo "state=frozen enabled=0 session=1 generation=1 active_writers=0 counter_hz=1000000000 event_mask=0x1ffffffff sampling=1 trace=1 timing_shift=8 effective_timing_shift=8 timing_sampler=hashed-bernoulli-v1"
    echo "cpu=0 event=net_protocol_turn calls=0 timed_samples=0 cycles=0 bytes=0 packets=0 sampled_wall_ns=0 sampled_on_cpu_ns=0 sampled_off_cpu_ns=0 sampled_max_latency_ns=0 migrations=0 hist=$zero"
    echo "cpu=mixed event=slab_cache_hit calls=0 timed_samples=0 cycles=0 bytes=0 packets=0 sampled_wall_ns=0 sampled_on_cpu_ns=0 sampled_off_cpu_ns=0 sampled_max_latency_ns=0 migrations=0 hist=$zero"
    echo "cpu=0 metric=ingress_ring_depth observations=0 sum=0 max=0 hist=$zero"
    echo "@@PROFILE_STATS_END phase=before case=smoke"
    echo "@@PROFILE_SAMPLES_BEGIN phase=before case=smoke"
    echo "state=frozen enabled=0 session=1 generation=1 sampling=1 slots_per_cpu=4096"
    echo "cpu=0 dropped_samples=0"
    echo "@@PROFILE_SAMPLES_END phase=before case=smoke"
    echo "@@PROFILE_TRACE_BEGIN phase=before case=smoke"
    echo "state=frozen enabled=0 session=1 generation=1 active_writers=0 trace=1 counter_hz=1000000000 slots_per_cpu=1024 record_bytes=80 format_version=2"
    echo "cpu=0 first_sequence=0 next_sequence=0 retained=0 overwritten=0"
    echo "@@PROFILE_TRACE_END phase=before case=smoke"
    echo "@@PROFILE_STATS_BEGIN phase=after case=smoke"
    echo "state=frozen enabled=0 session=1 generation=2 active_writers=0 counter_hz=1000000000 event_mask=0x1ffffffff sampling=1 trace=1 timing_shift=8 effective_timing_shift=8 timing_sampler=hashed-bernoulli-v1"
    echo "cpu=0 event=net_protocol_turn calls=8 timed_samples=2 cycles=20 bytes=64 packets=2 sampled_wall_ns=20 sampled_on_cpu_ns=12 sampled_off_cpu_ns=8 sampled_max_latency_ns=16 migrations=1 hist=$hist"
    echo "cpu=mixed event=slab_cache_hit calls=4 timed_samples=0 cycles=0 bytes=0 packets=0 sampled_wall_ns=0 sampled_on_cpu_ns=0 sampled_off_cpu_ns=0 sampled_max_latency_ns=0 migrations=0 hist=$zero"
    echo "cpu=0 metric=ingress_ring_depth observations=2 sum=10 max=8 hist=$hist"
    echo "@@PROFILE_STATS_END phase=after case=smoke"
    echo "@@PROFILE_SAMPLES_BEGIN phase=after case=smoke"
    echo "state=frozen enabled=0 session=1 generation=2 sampling=1 slots_per_cpu=4096"
    echo "cpu=0 dropped_samples=0"
    echo "cpu=0 task=7 mode=kernel pc=0x1000 samples=2"
    echo "@@PROFILE_SAMPLES_END phase=after case=smoke"
    echo "@@PROFILE_TRACE_BEGIN phase=after case=smoke"
    echo "state=frozen enabled=0 session=1 generation=2 active_writers=0 trace=1 counter_hz=1000000000 slots_per_cpu=1024 record_bytes=80 format_version=2"
    echo "cpu=0 first_sequence=0 next_sequence=0 retained=0 overwritten=0"
    echo "@@PROFILE_TRACE_END phase=after case=smoke"
} >"$tmp"

output=$($root/scripts/profile-report.sh "$tmp")
printf '%s\n' "$output" | awk -F '\t' '$1 == "smoke" && $2 == "net_protocol_turn" {
    if ($3 != 8 || $4 != 2 || $5 != "0.250000" || $6 != "low-confidence" || \
        $7 != 20 || $8 != 64 || $9 != 2 || $10 != 20 || $11 != "NA" || \
        $12 != "NA" || $13 != 12 || $14 != "NA" || $15 != 8 || $16 != "NA" || \
        $17 != 1 || $18 != "NA" || $19 != "NA" || $20 != "NA" || $21 != "NA") exit 1
    found = 1
} END { if (!found) exit 1 }'
printf '%s\n' "$output" | awk -F '\t' '$1 == "smoke" && $2 == "slab_cache_hit" {
    if ($3 != 4 || $4 != 0 || $6 != "invalid-no-samples") exit 1
    found = 1
} END { if (!found) exit 1 }'
output=$(PROFILE_REPORT_MIN_TIMED_SAMPLES=2 $root/scripts/profile-report.sh "$tmp")
printf '%s\n' "$output" | awk -F '\t' '$1 == "smoke" && $2 == "net_protocol_turn" {
    if ($6 != "sampled" || $11 != 80 || $12 != "10.00" || $14 != 48 || \
        $16 != 32 || $18 != "40.0" || $19 != 4 || $20 != 4 || $21 != 4) exit 1
    found = 1
} END { if (!found) exit 1 }'
printf '%s\n' "$output" | grep -q 'smoke.*ingress_ring_depth.*2.*5.00.*8.*1'
printf '%s\n' "$output" | grep -q 'smoke.*kernel.*0x1000.*2'
printf '%s\n' "$output" | grep -q 'smoke.*workload-root.*2'
printf '%s\n' "$output" | grep -q 'smoke.*1.*2.*0.*0.*ok'
printf '%s\n' "$output" | grep -q 'smoke.*kernel_image_id.*kernel-sha256'
sed '/^@@PROFILE_SAMPLES_BEGIN phase=after /,/^@@PROFILE_SAMPLES_END phase=after / s/dropped_samples=0/dropped_samples=1/' "$tmp" >"$bad"
output=$($root/scripts/profile-report.sh "$bad" 2>"$bad.err")
printf '%s\n' "$output" | grep -q 'smoke.*net_protocol_turn.*8.*2'
printf '%s\n' "$output" | grep -q 'smoke.*1.*2.*1.*0.*invalid'
grep -q 'sampling attribution invalid because PC samples were dropped for case=smoke' "$bad.err"
if printf '%s\n' "$output" | grep -q '^PC SAMPLES$'; then
    echo "profile-report fixture: invalid sampling still produced PC rankings" >&2
    exit 1
fi
sed '/^@@PROFILE_TRACE_BEGIN phase=after /,/^@@PROFILE_TRACE_END phase=after / s/overwritten=0/overwritten=1/' "$tmp" >"$bad"
output=$($root/scripts/profile-report.sh "$bad" 2>"$bad.err")
printf '%s\n' "$output" | grep -q 'smoke.*net_protocol_turn.*8.*2.*low-confidence'
printf '%s\n' "$output" | grep -q 'smoke.*1.*2.*0.*1.*invalid'
grep -q 'sampling/trace attribution invalid because trace records were overwritten for case=smoke' "$bad.err"
if printf '%s\n' "$output" | grep -q '^PC SAMPLES$'; then
    echo "profile-report fixture: overwritten trace still produced attributed PC rankings" >&2
    exit 1
fi
sed '0,/kernel_image_id=kernel-sha256/s//kernel_image_id=wrong-image/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: mismatched metadata was accepted" >&2
    exit 1
fi
sed '/@@PROFILE_META_BEGIN phase=before /,/@@PROFILE_META_END phase=before /d' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: incomplete metadata was accepted" >&2
    exit 1
fi
sed '0,/session=1/s//session=9/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: mismatched session was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_END phase=after /d' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: truncated stats section was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=after /,/^@@PROFILE_STATS_END phase=after / s/timing_shift=8/timing_shift=7/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: changed timing configuration was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=after /,/^@@PROFILE_STATS_END phase=after / s/ timing_sampler=hashed-bernoulli-v1//' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: missing timing sampler was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=after /,/^@@PROFILE_STATS_END phase=after / s/ bytes=64//' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: event with a missing required field was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=after /,/^@@PROFILE_STATS_END phase=after / s/,0$//' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: event histogram with 63 buckets was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=after /,/^@@PROFILE_STATS_END phase=after / s/timed_samples=2/timed_samples=9/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: timed_samples greater than calls was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=before /,/^@@PROFILE_STATS_END phase=before / s/calls=0/calls=9/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: decreasing event counters were accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_STATS_BEGIN phase=before /,/^@@PROFILE_STATS_END phase=before / s/calls=0/calls=7/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: interval timed_samples greater than calls was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_SAMPLES_BEGIN phase=after /,/^@@PROFILE_SAMPLES_END phase=after / s/session=1/session=9/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: mismatched samples session was accepted" >&2
    exit 1
fi
sed '0,/^@@PROFILE_META_END phase=before case=smoke$/s//@@PROFILE_META_END phase=after case=smoke/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: mismatched metadata end marker was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_META_END phase=before case=smoke$/i @@PROFILE_META_BEGIN phase=before case=nested' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: nested metadata section was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_META_END phase=before case=smoke$/d' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: unterminated metadata section was accepted" >&2
    exit 1
fi
sed '/^@@PROFILE_TRACE_BEGIN phase=before /,/^@@PROFILE_TRACE_END phase=before / s/next_sequence=0/next_sequence=1/' "$tmp" >"$bad"
if $root/scripts/profile-report.sh "$bad" >/dev/null 2>&1; then
    echo "profile-report fixture: decreasing trace sequence was accepted" >&2
    exit 1
fi
sed 's/^cmdline=console=ttyS0$/cmdline=/' "$tmp" >"$bad"
$root/scripts/profile-report.sh "$bad" >/dev/null
sed 's/mode=kernel/mode=user/' "$tmp" >"$bad"
output=$($root/scripts/profile-report.sh "$bad" /bin/sh)
printf '%s\n' "$output" | grep -q 'smoke.*user.*0x1000.*2.*user ELF not supplied'
output=$($root/scripts/profile-report.sh "$bad" /bin/sh /bin/sh 0)
if printf '%s\n' "$output" | grep -q 'user ELF not supplied'; then
    echo "profile-report fixture: supplied user ELF was ignored" >&2
    exit 1
fi
sed '/mode=kernel/d' "$tmp" >"$bad"
output=$($root/scripts/profile-report.sh "$bad" 2>"$bad.err")
printf '%s\n' "$output" | grep -q 'smoke.*1.*0.*0.*0.*no_samples'
grep -q 'sampling enabled but no PC samples for case=smoke' "$bad.err"
if PROFILE_REPORT_MIN_TIMED_SAMPLES=0 $root/scripts/profile-report.sh "$tmp" >/dev/null 2>&1; then
    echo "profile-report fixture: invalid minimum timing sample threshold was accepted" >&2
    exit 1
fi
echo "profile-report fixture: ok"
