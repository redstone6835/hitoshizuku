#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
log=$tmp/trace.log

{
    for phase in before after; do
        echo "@@PROFILE_META_BEGIN phase=$phase case=io"
        echo "arch=riscv64"
        echo "cpu_online=0"
        echo "kernel_release=mygo"
        echo "kernel_features=performance-profile"
        echo "kernel_image_id=kernel-sha256"
        echo "rootfs_image_id=rootfs-sha256"
        echo "workload=read-test"
        echo "workload_exit_status=$([ "$phase" = before ] && echo not-run || echo 0)"
        echo "cmdline=console=ttyS0"
        echo "control=state=frozen enabled=0"
        echo "@@PROFILE_META_END phase=$phase case=io"
    done
    echo "@@PROFILE_WORKLOAD case=io pid=7"
    echo "@@PROFILE_TRACE_BEGIN phase=before case=io"
    echo "state=frozen enabled=0 session=1 generation=2 active_writers=0 counter_hz=1000000 slots_per_cpu=1024 record_bytes=80 format_version=2"
    echo "cpu=0 first_sequence=0 next_sequence=0 retained=0 overwritten=0"
    echo "@@PROFILE_TRACE_END phase=before case=io"
    echo "@@PROFILE_TRACE_BEGIN phase=after case=io"
    echo "state=frozen enabled=0 session=1 generation=4 active_writers=0 counter_hz=1000000 slots_per_cpu=1024 record_bytes=80 format_version=2"
    echo "cpu=0 first_sequence=0 next_sequence=10 retained=10 overwritten=0"
    echo "cpu=0 sequence=0 session=1 generation=3 timestamp_cycles=100 duration_cycles=100 kind=scope event=syscall_dispatch task=7 span=10 arg0=63 arg1=0"
    echo "cpu=0 sequence=1 session=1 generation=3 timestamp_cycles=110 duration_cycles=70 kind=scope event=vfs_read task=7 span=10 arg0=4096 arg1=0"
    echo "cpu=0 sequence=2 session=1 generation=3 timestamp_cycles=120 duration_cycles=10 kind=scope event=block_submit task=7 span=10 arg0=8 arg1=1"
    echo "cpu=0 sequence=3 session=1 generation=3 timestamp_cycles=130 duration_cycles=40 kind=scope event=block_wait task=7 span=10 arg0=8 arg1=1"
    echo "cpu=0 sequence=4 session=1 generation=3 timestamp_cycles=140 duration_cycles=10 kind=scope event=block_drain task=7 span=10 arg0=8 arg1=1"
    echo "cpu=0 sequence=5 session=1 generation=3 timestamp_cycles=160 duration_cycles=0 kind=task_wake event=wait_other task=7 span=10 arg0=30000 arg1=0"
    echo "cpu=0 sequence=6 session=1 generation=3 timestamp_cycles=171 duration_cycles=0 kind=scope event=block_complete task=7 span=10 arg0=8 arg1=1"
    echo "cpu=0 sequence=7 session=1 generation=3 timestamp_cycles=300 duration_cycles=200 kind=scope event=syscall_dispatch task=7 span=20 arg0=63 arg1=0"
    echo "cpu=0 sequence=8 session=1 generation=3 timestamp_cycles=320 duration_cycles=20 kind=scope event=vfs_read task=7 span=20 arg0=4096 arg1=0"
    echo "cpu=0 sequence=9 session=1 generation=3 timestamp_cycles=500 duration_cycles=0 kind=sched_switch event=sched_switch task=7 span=20 arg0=7 arg1=8"
    echo "@@PROFILE_TRACE_END phase=after case=io"
} >"$log"

output=$($root/scripts/profile-analyze.sh "$log" 20)
printf '%s\n' "$output" | grep -q '^PROFILE_ANALYSIS version=1 top=20$'
printf '%s\n' "$output" | grep -q '^TRACE_CAPACITY max_retained=10 slots_per_cpu=1024 utilization_pct=1.0 warning=none$'
printf '%s\n' "$output" | grep -q '^WORKLOAD_ATTRIBUTION$'
printf '%s\n' "$output" | grep -q '^io[[:space:]]10[[:space:]]10[[:space:]]0[[:space:]]0[[:space:]]100.0$'
printf '%s\n' "$output" | grep -q '^WORKLOAD_ROOT_SPANS$'
printf '%s\n' "$output" | grep -q '^WORKLOAD_ROOT_BOTTLENECKS$'
printf '%s\n' "$output" | grep -q '^IO_CRITICAL_PATHS$'
printf '%s\n' "$output" | awk -F '\t' '
$1 == "io" && $2 == "10" && NF == 16 {
    found = $3 == "63" && $6 == "100.000" && $7 == "100.000" && \
        $8 == "70.000" && $12 == "40.000" && $13 == "30.000"
}
END { exit !found }
'
printf '%s\n' "$output" | grep -q '^io[[:space:]]63[[:space:]]2[[:space:]]300.000[[:space:]]150.000[[:space:]]100.000[[:space:]]200.000[[:space:]]200.000[[:space:]]40.000$'
printf '%s\n' "$output" | grep -q '^io[[:space:]]10[[:space:]]63[[:space:]]block_wait[[:space:]]30.000[[:space:]]30.0$'

echo "profile-analyze fixture: ok"
