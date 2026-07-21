#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

write_log() {
    path=$1
    duration=$2
    image=${3:-kernel-sha256}
    {
        for phase in before after; do
            echo "@@PROFILE_META_BEGIN phase=$phase case=smoke"
            echo "arch=riscv64"
            echo "cpu_online=0"
            echo "kernel_release=mygo"
            echo "kernel_features=performance-profile"
            echo "kernel_image_id=$image"
            echo "rootfs_image_id=rootfs-sha256"
            echo "workload=getpid-test"
            echo "workload_exit_status=$([ "$phase" = before ] && echo not-run || echo 0)"
            echo "cmdline=console=ttyS0"
            echo "control=state=frozen enabled=0"
            echo "@@PROFILE_META_END phase=$phase case=smoke"
        done
        echo "@@PROFILE_WORKLOAD case=smoke pid=7"
        echo "@@PROFILE_TRACE_BEGIN phase=before case=smoke"
        echo "state=frozen enabled=0 session=1 generation=2 active_writers=0 counter_hz=1000000 slots_per_cpu=1024 record_bytes=80 format_version=2"
        echo "cpu=0 first_sequence=0 next_sequence=0 retained=0 overwritten=0"
        echo "@@PROFILE_TRACE_END phase=before case=smoke"
        echo "@@PROFILE_TRACE_BEGIN phase=after case=smoke"
        echo "state=frozen enabled=0 session=1 generation=4 active_writers=0 counter_hz=1000000 slots_per_cpu=1024 record_bytes=80 format_version=2"
        echo "cpu=0 first_sequence=0 next_sequence=1 retained=1 overwritten=0"
        echo "cpu=0 sequence=0 session=1 generation=3 timestamp_cycles=100 duration_cycles=$duration kind=scope event=syscall_dispatch task=7 span=10 arg0=172 arg1=0"
        echo "@@PROFILE_TRACE_END phase=after case=smoke"
    } >"$path"
}

write_log "$tmp/one.log" 100
write_log "$tmp/two.log" 300
output=$($root/scripts/profile-campaign.sh "$tmp/one.log" "$tmp/two.log")
printf '%s\n' "$output" | grep -q '^PROFILE_CAMPAIGN version=1 logs=2$'
printf '%s\n' "$output" | grep -q '^smoke[[:space:]]172[[:space:]]2[[:space:]]2[[:space:]]200.000[[:space:]]200.000[[:space:]]300.000[[:space:]]141.421[[:space:]]70.7[[:space:]]100.000[[:space:]]300.000$'

write_log "$tmp/bad.log" 200 other-kernel
if $root/scripts/profile-campaign.sh "$tmp/one.log" "$tmp/bad.log" >/dev/null 2>&1; then
    echo "profile-campaign fixture: mismatched environment was accepted" >&2
    exit 1
fi

echo "profile-campaign fixture: ok"
