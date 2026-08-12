#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <case-id> <workload-script> [phase-rules]" >&2
    exit 2
}

[ "$#" -ge 2 ] && [ "$#" -le 3 ] || usage
case_id=$1
source_script=$2
phase_rules=${3:-}
case "$case_id" in ''|*[!A-Za-z0-9._-]*) usage ;; esac

control=${PROFILE_CONTROL:-/sys/kernel/profile_control}
snapshot=${PROFILE_SNAPSHOT:-/sys/kernel/profile_snapshot}
health=${PROFILE_HEALTH:-/sys/kernel/profile_health}
output_root=${PROFILE_OUTPUT_ROOT:-/work/mygo-profile}
sample_hz=${PROFILE_SAMPLE_HZ:-250}
profile_mode=${PROFILE_MODE:-$(cat /etc/mygo-profile-mode 2>/dev/null || echo sample)}
profile_preset=${PROFILE_PRESET:-all}

fail() {
    echo "[profile][$case_id] $*" >&2
    exit 1
}

write_control() {
    # BusyBox printf 可能把格式串和参数拆成多次 write，sysfs 控制命令必须单次提交。
    echo "$1" >"$control"
}

online_cpu_count() {
    if [ -r /sys/devices/system/cpu/online ]; then
        count=$(awk -F, '
        {
            count = 0
            for (i = 1; i <= NF; i++) {
                parts = split($i, range, "-")
                if (range[1] !~ /^[0-9]+$/ ||
                    (parts == 2 && range[2] !~ /^[0-9]+$/) || parts > 2)
                    exit 1
                count += parts == 1 ? 1 : range[2] - range[1] + 1
            }
            if (count < 1) exit 1
            print count
        }
        ' /sys/devices/system/cpu/online 2>/dev/null) || count=
        [ -n "$count" ] && { echo "$count"; return; }
    fi
    nproc 2>/dev/null || echo unknown
}

[ -w "$control" ] || fail "profile control is unavailable"
[ -r "$snapshot" ] || fail "profile snapshot is unavailable"
[ -r "$health" ] || fail "profile health is unavailable"
[ -r "$source_script" ] || fail "workload script is unavailable: $source_script"

case "$sample_hz" in ''|*[!0-9]*) fail "invalid sample frequency: $sample_hz" ;; esac
[ "$sample_hz" -ge 50 ] && [ "$sample_hz" -le 1000 ] || \
    fail "sample frequency must be between 50 and 1000 Hz"
case "$profile_mode" in counter|sample) ;; *) fail "invalid profile mode: $profile_mode" ;; esac
case "$profile_preset" in
    io|syscall|filesystem|memory|scheduler|block|network|build|all|full) ;;
    *) fail "invalid profile preset: $profile_preset" ;;
esac

work=/tmp/mygo-workload-profile.$$
instrumented=$work/workload.sh
fifo=$work/output.fifo
gate=$work/release
mkdir -p "$work" "$output_root"
mkfifo "$fifo"
trap 'rm -rf "$work"' EXIT INT TERM

if [ -n "$phase_rules" ] && [ -s "$phase_rules" ]; then
    awk -F '\t' '
    FNR == NR {
        if (NF != 3 || $1 !~ /^[0-9]+$/ || $2 !~ /^[A-Za-z0-9._-]+$/ || $3 == "")
            exit 41
        count++
        phase_id[count] = $1
        phase_name[count] = $2
        pattern[count] = $3
        next
    }
    {
        for (rule = 1; rule <= count; rule++) {
            if (!seen[rule] && $0 ~ pattern[rule]) {
                printf "echo @@MYGO_PROFILE_PHASE=%s:%s\n", phase_id[rule], phase_name[rule]
                seen[rule] = 1
            }
        }
        print
    }
    END {
        for (rule = 1; rule <= count; rule++)
            if (!seen[rule]) exit 42
    }
    ' "$phase_rules" "$source_script" >"$instrumented" || \
        fail "phase rules are invalid or do not match the workload"
else
    cp "$source_script" "$instrumented"
fi
chmod 0755 "$instrumented"

(
    while [ ! -e "$gate" ]; do sleep 0.01; done
    exec /bin/sh "$instrumented"
) >"$fifo" 2>&1 &
workload_pid=$!

write_control freeze
write_control reset
write_control "preset=$profile_preset"
write_control phase=0
write_control trace=0
if [ "$profile_mode" = sample ]; then
    write_control samples=1
else
    write_control samples=0
fi
write_control "sample_hz=$sample_hz"
write_control "root=$workload_pid"
write_control resume
echo "@@PROFILE_WORKLOAD case=$case_id pid=$workload_pid"
echo "@@PROFILE_PHASE id=0 name=initial"
: >"$gate"

while IFS= read -r line; do
    case "$line" in
        @@MYGO_PROFILE_PHASE=*:*)
            phase=${line#@@MYGO_PROFILE_PHASE=}
            phase_id=${phase%%:*}
            phase_name=${phase#*:}
            case "$phase_id" in ''|*[!0-9]*) fail "invalid phase marker: $line" ;; esac
            case "$phase_name" in ''|*[!A-Za-z0-9._-]*) fail "invalid phase marker: $line" ;; esac
            write_control "phase=$phase_id"
            echo "@@PROFILE_PHASE id=$phase_id name=$phase_name"
            ;;
        *) printf '%s\n' "$line" ;;
    esac
done <"$fifo"

if wait "$workload_pid"; then workload_status=0; else workload_status=$?; fi
write_control freeze
arch=$(uname -m 2>/dev/null || echo unknown)
cpus=$(online_cpu_count)
stem=$output_root/$case_id-$arch
cat "$health" >"$stem.health"
cat "$snapshot" >"$stem.bin"
{
    echo "format=mygo-workload-profile-v1"
    echo "case_id=$case_id"
    echo "arch=$arch"
    echo "cpus=$cpus"
    echo "sample_hz=$sample_hz"
    echo "profile_mode=$profile_mode"
    echo "profile_preset=$profile_preset"
    echo "workload_status=$workload_status"
    echo "snapshot=$stem.bin"
    echo "health=$stem.health"
    echo "control=$(cat "$control")"
} >"$stem.meta"
sync

if ! grep -q '^valid=1 ' "$stem.health"; then
    echo "[profile][$case_id] invalid snapshot: $(cat "$stem.health")" >&2
    exit 1
fi
if ! grep -q ' complete=1 ' "$stem.health"; then
    echo "[profile][$case_id] incomplete sections: $(cat "$stem.health")" >&2
fi
echo "@@PROFILE_ARTIFACT snapshot=$stem.bin health=$stem.health meta=$stem.meta"
exit "$workload_status"
