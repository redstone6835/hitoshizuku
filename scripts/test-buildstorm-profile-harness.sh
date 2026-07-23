#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
host=$repo/scripts/buildstorm-profile-host.sh
guest=$repo/scripts/buildstorm-profile-guest.sh

sh -n "$host"
sh -n "$guest"

fixture=$(mktemp)
summary_python=$(mktemp)
summary_dir=$(mktemp -d)
child_pid_file=$(mktemp)
group_pid=
trap '[ -z "$group_pid" ] || kill -KILL "-$group_pid" 2>/dev/null || true; rm -f "$fixture" "$summary_python" "$child_pid_file" /tmp/buildstorm-profile-owner-fixture-mismatch /tmp/buildstorm-profile-owner-fixture-group; rm -rf "$summary_dir"' EXIT INT TERM
printf 'Compiling a\r    Building [====> ] 7/446\nnoise 63/446 then 64/446\r384/446\n440/446\r446/446\n' >"$fixture"
actual=$("$host" --extract-progress <"$fixture")
[ "$actual" = 446 ] || {
    echo "progress fixture: expected 446, got $actual" >&2
    exit 1
}

printf 'ordinary cargo output without a total\n' >"$fixture"
actual=$("$host" --extract-progress <"$fixture")
[ -z "$actual" ] || {
    echo "progress fixture: expected empty output, got $actual" >&2
    exit 1
}
printf '    Building [                           ] 0/446\n' >"$fixture"
actual=$("$host" --extract-progress <"$fixture")
[ "$actual" = 0 ] || {
    echo "progress fixture: expected zero, got $actual" >&2
    exit 1
}
printf 'false totals 64/4460 and impossible 999/446\n' >"$fixture"
actual=$("$host" --extract-progress <"$fixture")
[ -z "$actual" ] || {
    echo "progress fixture: accepted a false total: $actual" >&2
    exit 1
}

printf 'old run 446/446\n@@PROFILE_WORKLOAD case=fresh pid=1 start_ticks=1 token=fresh\nnew run 64/446\n' >"$fixture"
actual=$("$host" --extract-progress-after '@@PROFILE_WORKLOAD case=fresh pid=1 start_ticks=1 token=fresh' <"$fixture")
[ "$actual" = 64 ] || {
    echo "scoped progress fixture: old log polluted progress: $actual" >&2
    exit 1
}

if PROFILE_DURATION_MS=bad "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid millisecond duration was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_CPUSET='0;id' "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid cpuset was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_CAPTURE=bad "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid capture mode was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_TIMING_SHIFT=17 "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid timing shift was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_EVENT_MASK='0x1;id' "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid event mask was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_EVENT_MASK="0x1
0x2" "$host" >/dev/null 2>&1; then
    echo "validation fixture: multiline event mask was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_STAGE_ANCHOR="marker:ok
old-log-marker" "$host" >/dev/null 2>&1; then
    echo "validation fixture: multiline marker was accepted" >&2
    exit 1
fi
if PROFILE_CAPTURE=bad "$guest" run fixture-token >/dev/null 2>&1; then
    echo "validation fixture: guest accepted invalid capture mode" >&2
    exit 1
fi
if PROFILE_EVENT_MASK='0x1;id' "$guest" run fixture-token >/dev/null 2>&1; then
    echo "validation fixture: guest accepted invalid event mask" >&2
    exit 1
fi
stop_output=$("$guest" stop 999999 1 fixture-no-owner)
case "$stop_output" in
    'PROFILE_STOP_SKIPPED reason=missing-owner token=fixture-no-owner') ;;
    *) echo "stop fixture: unexpected output: $stop_output" >&2; exit 1 ;;
esac

# The leader and a TERM-ignoring descendant share one session/process group.
# stop must inspect and eliminate the complete PGID, not merely the leader.
setsid sh -c 'trap "" TERM; sleep 300 & echo $! >"$1"; wait' sh "$child_pid_file" &
group_pid=$!
attempts=0
while [ ! -s "$child_pid_file" ] && [ "$attempts" -lt 100 ]; do
    attempts=$((attempts + 1))
    sleep 0.01
done
[ -s "$child_pid_file" ] || { echo "process-group fixture: child did not start" >&2; exit 1; }
stat=$(cat "/proc/$group_pid/stat")
rest=${stat#*) }
set -- $rest
start_ticks=${20}
printf '%s %s %s\n' "$group_pid" "$start_ticks" fixture-group >/tmp/buildstorm-profile-owner-fixture-group
"$guest" stop "$group_pid" "$start_ticks" fixture-group >/dev/null
wait "$group_pid" 2>/dev/null || true
for stat_file in /proc/[0-9]*/stat; do
    stat=$(cat "$stat_file" 2>/dev/null) || continue
    rest=${stat#*) }
    set -- $rest
    [ "$#" -ge 3 ] || continue
    if [ "$3" = "$group_pid" ] && [ "$1" != Z ]; then
        echo "process-group fixture: descendant survived stop (pid=${stat_file#/proc/})" >&2
        exit 1
    fi
done
group_pid=
printf '%s %s %s\n' "$$" 1 fixture-mismatch >/tmp/buildstorm-profile-owner-fixture-mismatch
stop_output=$("$guest" stop-token fixture-mismatch)
case "$stop_output" in
    'PROFILE_STOP_SKIPPED reason=identity-mismatch token=fixture-mismatch') ;;
    *) echo "stop identity fixture: unexpected output: $stop_output" >&2; exit 1 ;;
esac

# Compile and execute the embedded summary generator against a tiny fixture so
# schema changes and argument-index mistakes fail without booting QEMU.
awk '/^python3 - "\$run_dir"/{copy=1; next} copy && /^PY$/{exit} copy{print}' \
    "$host" >"$summary_python"
python3 -m py_compile "$summary_python"
printf '%s\n' \
    'kernel_sha256=k' 'base_sha256=b' 'qemu_version=q' \
    'container_image=i' 'cpuset=0-1' 'duration_ms=10' 'warmup_ms=0' \
    'stage_anchor=workload' 'capture_enabled=0' 'poll_ms=5' \
    'event_mask=0x1' 'sampling_enabled=0' 'trace_enabled=0' 'timing_shift=8' \
    'timing_sampler=hashed-bernoulli-v1' \
    'host_sample_ms=1000' 'host_clock_ticks_per_second=100' \
    >"$summary_dir/metadata.env"
printf 'milestone\tmonotonic_ns\n0\t100\n64\t200\n' >"$summary_dir/progress.tsv"
printf '%b\n' \
    'monotonic_ns\tphase\tprogress\tqemu_utime_ticks\tqemu_stime_ticks\tload1\tload5\tload15\trunnable_total\tlast_pid\tcpu_some_avg10\tcpu_some_total\tio_some_avg10\tio_some_total\tio_full_avg10\tio_full_total\tmemory_some_avg10\tmemory_some_total\tmemory_full_avg10\tmemory_full_total' \
    '100\tstart\t0\t10\t5\t1\t1\t1\t1/10\t10\t0\t1\t0\t1\t0\t1\t0\t1\t0\t1' \
    '110\tstop\t64\t30\t15\t1\t1\t1\t1/10\t10\t0\t1\t0\t1\t0\t1\t0\t1\t0\t1' \
    '120\tfinal\t64\t31\t16\t1\t1\t1\t1/10\t10\t0\t1\t0\t1\t0\t1\t0\t1\t0\t1' \
    >"$summary_dir/host-samples.tsv"
printf '%b\n' \
    'monotonic_ns\tphase\tqemu_utime_ticks\tqemu_stime_ticks' \
    '100\tstart\t10\t5' '110\tstop\t30\t15' \
    >"$summary_dir/qemu-cpu-boundaries.tsv"
python3 "$summary_python" "$summary_dir" workload 90 100 110 120 0 0 unavailable 0 0 64 101 109 111 110
python3 - "$summary_dir/summary.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["schema"] == "mygo.buildstorm-profile.v2"
assert data["timing"]["window_start_progress"] == 0
assert data["timing"]["window_stop_progress"] == 64
assert data["timing"]["start_observation_latency_ms"] == 0.000001
assert data["timing"]["stop_observation_latency_ms"] == 0.000001
assert data["timing"]["cargo_progress_monotonic_ns"]["128"] is None
assert data["profiling"]["report_status"] == "unavailable"
assert data["profiling"]["mode"] == "off"
assert data["host"]["qemu_cpu_ticks"] == 30
PY

echo "buildstorm profile harness fixtures: ok"
