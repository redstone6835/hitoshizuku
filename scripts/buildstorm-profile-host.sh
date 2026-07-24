#!/bin/sh
# Run one cold BuildStorm profiling window under the contest QEMU.
set -eu

extract_progress() {
    tr '\r' '\n' | awk '
    BEGIN { maximum = 0 }
    {
        line = $0
        while (match(line, /[0-9][0-9]*\/446/)) {
            value = substr(line, RSTART, RLENGTH)
            sub(/\/446$/, "", value)
            suffix = substr(line, RSTART + RLENGTH, 1)
            if (suffix !~ /[0-9]/ && value + 0 <= 446) {
                if (value + 0 > maximum) maximum = value + 0
                found = 1
            }
            line = substr(line, RSTART + RLENGTH)
        }
    }
    END { if (found) print maximum }
    '
}

if [ "${1:-}" = "--extract-progress" ]; then
    extract_progress
    exit 0
fi

if [ "${1:-}" = "--extract-progress-after" ]; then
    [ "$#" -eq 2 ] && [ -n "$2" ] || exit 2
    awk -v marker="$2" 'seen { print } index($0, marker) { seen = 1 }' | extract_progress
    exit 0
fi

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
duration_arg=${1:-120}
if [ -n "${PROFILE_DURATION_MS:-}" ]; then
    duration_ms=$PROFILE_DURATION_MS
else
    duration_ms=$(awk -v value="$duration_arg" \
        'BEGIN { if (value !~ /^[0-9]+([.][0-9]+)?$/ || value <= 0) exit 1; printf "%.0f\n", value * 1000 }') || {
        echo "usage: $0 [positive-seconds]" >&2
        exit 2
    }
fi
case "$duration_ms" in
    ''|*[!0-9]*|0) echo "PROFILE_DURATION_MS must be a positive integer" >&2; exit 2 ;;
esac

warmup_ms=${PROFILE_WARMUP_MS:-0}
stage_timeout_ms=${PROFILE_STAGE_TIMEOUT_MS:-900000}
boot_timeout_ms=${PROFILE_BOOT_TIMEOUT_MS:-120000}
done_timeout_ms=${PROFILE_DONE_TIMEOUT_MS:-30000}
capture_start_timeout_ms=${PROFILE_CAPTURE_START_TIMEOUT_MS:-30000}
sample_ms=${PROFILE_HOST_SAMPLE_MS:-1000}
poll_ms=${PROFILE_POLL_MS:-50}
anchor=${PROFILE_STAGE_ANCHOR:-workload}
cpuset=${PROFILE_CPUSET:-}
for pair in \
    "PROFILE_WARMUP_MS:$warmup_ms" \
    "PROFILE_STAGE_TIMEOUT_MS:$stage_timeout_ms" \
    "PROFILE_BOOT_TIMEOUT_MS:$boot_timeout_ms" \
    "PROFILE_DONE_TIMEOUT_MS:$done_timeout_ms" \
    "PROFILE_CAPTURE_START_TIMEOUT_MS:$capture_start_timeout_ms" \
    "PROFILE_HOST_SAMPLE_MS:$sample_ms" \
    "PROFILE_POLL_MS:$poll_ms"
do
    name=${pair%%:*}
    value=${pair#*:}
    case "$value" in ''|*[!0-9]*) echo "$name must be a non-negative integer" >&2; exit 2 ;; esac
done
[ "$sample_ms" -gt 0 ] || { echo "PROFILE_HOST_SAMPLE_MS must be positive" >&2; exit 2; }
[ "$poll_ms" -gt 0 ] || { echo "PROFILE_POLL_MS must be positive" >&2; exit 2; }
case "$cpuset" in *[!0-9,-]*) echo "PROFILE_CPUSET has invalid syntax" >&2; exit 2 ;; esac
case "$anchor" in
    workload|aws-object) ;;
    cargo:*)
        anchor_progress=${anchor#cargo:}
        case "$anchor_progress" in ''|*[!0-9]*) echo "invalid cargo stage anchor: $anchor" >&2; exit 2 ;; esac
        [ "$anchor_progress" -le 446 ] || { echo "invalid cargo stage anchor: $anchor" >&2; exit 2; }
        ;;
    marker:*)
        anchor_marker=${anchor#marker:}
        [ -n "$anchor_marker" ] || { echo "empty marker stage anchor" >&2; exit 2; }
        printf '%s\n' "$anchor_marker" | LC_ALL=C awk '
            NR == 1 && length($0) <= 128 && $0 !~ /[[:cntrl:]]/ { ok = 1 }
            NR != 1 { bad = 1 }
            END { exit !(ok && !bad) }
        ' || { echo "marker stage anchor must be at most 128 printable bytes" >&2; exit 2; }
        ;;
    *) echo "PROFILE_STAGE_ANCHOR must be workload, aws-object, cargo:N, or marker:TEXT" >&2; exit 2 ;;
esac

kernel=${PROFILE_KERNEL:-"$repo/kernel-la"}
base=${PROFILE_BASE_IMAGE:-"$repo/../oskernel2026-mygo-network-cagent/build/sdcard-la-pub.img"}
container_image=${PROFILE_CONTAINER_IMAGE:-zhouzhouyi/os-contest:20260510}
label=${PROFILE_LABEL:-"host${duration_ms}ms"}
sampling=${PROFILE_SAMPLING:-0}
trace_enabled=${PROFILE_TRACE_ENABLED:-0}
timing_shift=${PROFILE_TIMING_SHIFT:-8}
timing_sampler=${PROFILE_TIMING_SAMPLER:-hashed-bernoulli-v1}
capture=${PROFILE_CAPTURE:-1}
event_mask=${PROFILE_EVENT_MASK:-0xfef000000}
case "$sampling:$trace_enabled" in
    0:0|0:1|1:0|1:1) ;;
    *) echo "PROFILE_SAMPLING and PROFILE_TRACE_ENABLED must be 0 or 1" >&2; exit 2 ;;
esac
case "$capture" in 0|1) ;; *) echo "PROFILE_CAPTURE must be 0 or 1" >&2; exit 2 ;; esac
case "$timing_shift" in
    ''|*[!0-9]*) echo "PROFILE_TIMING_SHIFT must be an integer from 0 to 16" >&2; exit 2 ;;
esac
[ "$timing_shift" -le 16 ] || { echo "PROFILE_TIMING_SHIFT must be an integer from 0 to 16" >&2; exit 2; }
case "$timing_sampler" in
    ''|*[!A-Za-z0-9_.-]*) echo "PROFILE_TIMING_SAMPLER has invalid syntax" >&2; exit 2 ;;
esac
printf '%s\n' "$event_mask" | LC_ALL=C awk '
    NR == 1 && /^0x[0-9A-Fa-f]+$/ && length($0) >= 3 && length($0) <= 18 { ok = 1 }
    NR != 1 { bad = 1 }
    END { exit !(ok && !bad) }
' || { echo "PROFILE_EVENT_MASK must be a 1-16 digit hexadecimal mask with a 0x prefix" >&2; exit 2; }

test -r "$kernel" || { echo "profile host: missing kernel: $kernel" >&2; exit 1; }
test -r "$base" || { echo "profile host: missing base image: $base" >&2; exit 1; }
for command in docker mkfs.ext4 socat timeout python3 sha256sum setsid sudo; do
    command -v "$command" >/dev/null 2>&1 || { echo "profile host: $command is required" >&2; exit 1; }
done

monotonic_ns() {
    python3 -c 'import time; print(time.monotonic_ns())'
}

sleep_ms() {
    python3 - "$1" <<'PY'
import sys, time
time.sleep(int(sys.argv[1]) / 1000)
PY
}

safe_label=$(printf '%s' "$label" | tr -c 'A-Za-z0-9_.-' '-')
run_dir=$(mktemp -d "${PROFILE_RUN_ROOT:-/tmp}/mygo-profile-${safe_label}.XXXXXX")
stage=$(mktemp -d "${PROFILE_RUN_ROOT:-/tmp}/mygo-profile-tools.XXXXXX")
container="mygo-profile-$$"
run_token="p$$_$(monotonic_ns)"
workload_log_line=
workload_log_offset=
logger_pid=
qemu_pid=
normal_exit=0

host_process_group_alive() {
    LC_ALL=C awk -v target="$1" '
        {
            line = $0
            sub(/^[0-9]+ \(.*\) /, "", line)
            split(line, field, " ")
            if (field[3] == target && field[1] != "Z") found = 1
        }
        END { exit !found }
    ' /proc/[0-9]*/stat 2>/dev/null
}

cleanup() {
    timeout 10 docker stop -t 1 "$container" >/dev/null 2>&1 || true
    timeout 10 docker rm -f "$container" >/dev/null 2>&1 || true
    if [ -n "$logger_pid" ]; then
        kill -TERM "-$logger_pid" >/dev/null 2>&1 || true
        cleanup_wait=0
        while host_process_group_alive "$logger_pid" && [ "$cleanup_wait" -lt 100 ]; do
            cleanup_wait=$((cleanup_wait + 1))
            sleep 0.01
        done
        if host_process_group_alive "$logger_pid"; then
            kill -KILL "-$logger_pid" >/dev/null 2>&1 || true
        fi
        wait "$logger_pid" 2>/dev/null || true
    fi
    rm -f "$stage/profile-capture.sh" "$stage/run.sh"
    rmdir "$stage" 2>/dev/null || true
    if [ "$normal_exit" -ne 1 ]; then
        echo "profile host: incomplete run retained at $run_dir" >&2
    fi
}
trap cleanup EXIT INT TERM

send_line() {
    # The first byte sent immediately after entering the interactive shell can
    # be consumed by the console transition. Leading spaces make that loss inert.
    line="  $1"
    printf '%s\n' "$line" | LC_ALL=C awk 'NR == 1 && length($0) <= 256 { ok = 1 } NR != 1 { bad = 1 } END { exit !(ok && !bad) }' || {
        echo "profile host: refusing unsafe or oversized serial command" >&2
        return 2
    }
    timeout 2 sh -c 'printf "%s\n" "$1" >"$2"' sh "$line" "$run_dir/serial.in"
}

deadline_after_ms() {
    now=$(monotonic_ns)
    printf '%s\n' "$((now + $1 * 1000000))"
}

wait_for_fixed() {
    needle=$1
    timeout_ms=$2
    deadline=$(deadline_after_ms "$timeout_ms")
    while ! grep -Fq "$needle" "$run_dir/profile.serial.log" 2>/dev/null; do
        [ "$(monotonic_ns)" -lt "$deadline" ] || return 1
        sleep_ms 20
    done
}

workload_finished() {
    grep -q "@@PROFILE_WORKLOAD_EXIT .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null
}

current_progress() {
    [ -n "$workload_log_offset" ] || return 0
    tail -c "+$((workload_log_offset + 1))" "$run_dir/profile.serial.log" 2>/dev/null |
        tail -c 131072 | extract_progress || true
}

serial_after_workload_has() {
    [ -n "$workload_log_offset" ] || return 1
    tail -c "+$((workload_log_offset + 1))" "$run_dir/profile.serial.log" 2>/dev/null |
        grep -Fq "$1"
}

last_progress=-1
record_progress() {
    progress=$(current_progress)
    [ -n "$progress" ] || return 0
    [ "$progress" -gt "$last_progress" ] || return 0
    last_progress=$progress
    stamp=$(monotonic_ns)
    for milestone in 0 64 128 256 384 440 446; do
        if [ "$progress" -ge "$milestone" ] && [ ! -e "$run_dir/progress-$milestone" ]; then
            : >"$run_dir/progress-$milestone"
            printf '%s\t%s\n' "$milestone" "$stamp" >>"$run_dir/progress.tsv"
        fi
    done
}

psi_field() {
    file=$1
    kind=$2
    key=$3
    awk -v kind="$kind" -v key="$key" '$1 == kind { for (i=2;i<=NF;i++) { split($i,a,"="); if (a[1] == key) { print a[2]; exit } } }' "$file" 2>/dev/null || true
}

qemu_cpu_value() {
    qcpu="0 0"
    if [ -n "$qemu_pid" ] && [ -r "/proc/$qemu_pid/stat" ]; then
        qstat=$(cat "/proc/$qemu_pid/stat" 2>/dev/null || true)
        qrest=${qstat#*) }
        if [ "$qrest" != "$qstat" ]; then
            set -- $qrest
            [ "$#" -ge 13 ] && qcpu="${12} ${13}"
        fi
    fi
    printf '%s\n' "$qcpu"
}

sample_qemu_boundary() {
    phase=$1
    stamp=$2
    set -- $(qemu_cpu_value)
    printf '%s\t%s\t%s\t%s\n' "$stamp" "$phase" "$1" "$2" >>"$run_dir/qemu-cpu-boundaries.tsv"
}

sample_host() {
    phase=$1
    progress=$(current_progress)
    [ -n "$progress" ] || progress=-1
    stamp=$(monotonic_ns)
    set -- $(cat /proc/loadavg)
    load1=$1 load5=$2 load15=$3 runnable=$4 lastpid=$5
    set -- $(qemu_cpu_value)
    q_utime=$1 q_stime=$2
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s' \
        "$stamp" "$phase" "$progress" "$q_utime" "$q_stime" \
        "$load1" "$load5" "$load15" "$runnable" "$lastpid" >>"$run_dir/host-samples.tsv"
    for spec in cpu:some:avg10 cpu:some:total io:some:avg10 io:some:total io:full:avg10 io:full:total memory:some:avg10 memory:some:total memory:full:avg10 memory:full:total; do
        file=${spec%%:*}; tail=${spec#*:}; kind=${tail%%:*}; key=${tail#*:}
        value=$(psi_field "/proc/pressure/$file" "$kind" "$key")
        printf '\t%s' "${value:-NA}" >>"$run_dir/host-samples.tsv"
    done
    printf '\n' >>"$run_dir/host-samples.tsv"
}

printf 'milestone\tmonotonic_ns\n' >"$run_dir/progress.tsv"
printf 'monotonic_ns\tphase\tprogress\tqemu_utime_ticks\tqemu_stime_ticks\tload1\tload5\tload15\trunnable_total\tlast_pid\tcpu_some_avg10\tcpu_some_total\tio_some_avg10\tio_some_total\tio_full_avg10\tio_full_total\tmemory_some_avg10\tmemory_some_total\tmemory_full_avg10\tmemory_full_total\n' >"$run_dir/host-samples.tsv"
printf 'monotonic_ns\tphase\tqemu_utime_ticks\tqemu_stime_ticks\n' >"$run_dir/qemu-cpu-boundaries.tsv"

cp "$kernel" "$run_dir/kernel-la"
kernel_id=$(sha256sum "$run_dir/kernel-la" | awk '{print $1}')
base_id=$(sha256sum "$base" | awk '{print $1}')
controller_timeout_ms=$((stage_timeout_ms + warmup_ms + duration_ms + done_timeout_ms + 60000))
cp "$repo/scripts/profile-capture.sh" "$stage/profile-capture.sh"
cp "$repo/scripts/buildstorm-profile-guest.sh" "$stage/run.sh"
{
    printf 'export PROFILE_CONTROLLER_TIMEOUT_MS=%s\n' "$controller_timeout_ms"
    printf 'export PROFILE_CAPTURE=%s\n' "$capture"
    printf 'export PROFILE_EVENT_MASK=%s\n' "$event_mask"
    printf 'export PROFILE_SAMPLING=%s\n' "$sampling"
    printf 'export PROFILE_TRACE_ENABLED=%s\n' "$trace_enabled"
    printf 'export PROFILE_TIMING_SHIFT=%s\n' "$timing_shift"
    printf 'export PROFILE_TIMING_SAMPLER=%s\n' "$timing_sampler"
    printf 'export PROFILE_KERNEL_IMAGE_ID=%s\n' "$kernel_id"
    printf 'export PROFILE_ROOTFS_IMAGE_ID=%s\n' "$base_id"
    printf 'export PROFILE_WORKLOAD=%s\n' "$safe_label"
    printf 'export PROFILE_RUN_TOKEN=%s\n' "$run_token"
    printf 'export PROFILE_TOOL_MOUNT=/tmp/p\n'
} >"$stage/config.env"
chmod 0755 "$stage/profile-capture.sh" "$stage/run.sh"
truncate -s 16M "$run_dir/tools.ext4"
mkfs.ext4 -q -d "$stage" "$run_dir/tools.ext4"

qemu_version=$(timeout 30 docker run --rm "$container_image" qemu-system-loongarch64 --version | head -n 1 | tr '\t\r\n' '   ')
clock_ticks=$(getconf CLK_TCK 2>/dev/null || echo 100)
{
    printf 'kernel_sha256=%s\nbase_sha256=%s\n' "$kernel_id" "$base_id"
    printf 'qemu_version=%s\ncontainer_image=%s\ncpuset=%s\n' "$qemu_version" "$container_image" "$cpuset"
    printf 'duration_ms=%s\nwarmup_ms=%s\nstage_anchor=%s\n' "$duration_ms" "$warmup_ms" "$anchor"
    printf 'capture_enabled=%s\n' "$capture"
    printf 'event_mask=%s\nsampling_enabled=%s\ntrace_enabled=%s\ntiming_shift=%s\ntiming_sampler=%s\n' \
        "$event_mask" "$sampling" "$trace_enabled" "$timing_shift" "$timing_sampler"
    printf 'poll_ms=%s\nhost_sample_ms=%s\n' "$poll_ms" "$sample_ms"
    printf 'host_clock_ticks_per_second=%s\n' "$clock_ticks"
} >"$run_dir/metadata.env"

base_dir=$(dirname "$base")
base_name=$(basename "$base")
set -- docker run --rm
[ -z "$cpuset" ] || set -- "$@" --cpuset-cpus "$cpuset"
set -- "$@" -v "$run_dir":/run -v "$base_dir":/base:ro "$container_image" \
    qemu-img create -f qcow2 -F raw -b "/base/$base_name" /run/run.qcow2
timeout 60 "$@" >/dev/null

mkfifo "$run_dir/serial.in"
set -- docker run -d --name "$container"
[ -z "$cpuset" ] || set -- "$@" --cpuset-cpus "$cpuset"
set -- "$@" -v "$run_dir":/run -v "$base_dir":/base:ro "$container_image" \
    qemu-system-loongarch64 \
    -machine virt -cpu la464 -accel tcg,thread=multi -m 8G -smp 8 \
    -display none -monitor none -S -no-reboot -rtc base=utc \
    -serial unix:/run/serial.sock,server=on,wait=off \
    -qmp unix:/run/qmp.sock,server=on,wait=off \
    -kernel /run/kernel-la \
    -drive if=none,id=x0,file=/run/run.qcow2,format=qcow2 \
    -device virtio-blk-pci,drive=x0 \
    -drive if=none,id=x1,file=/run/tools.ext4,format=raw \
    -device virtio-blk-pci,drive=x1
timeout 30 "$@" >/dev/null

socket_deadline=$(deadline_after_ms 10000)
while [ ! -S "$run_dir/serial.sock" ] || [ ! -S "$run_dir/qmp.sock" ]; do
    [ "$(monotonic_ns)" -lt "$socket_deadline" ] || {
        echo "profile host: QEMU sockets did not appear" >&2
        timeout 5 docker logs "$container" >&2 || true
        exit 1
    }
    sleep_ms 20
done

container_pid=$(timeout 5 docker inspect -f '{{.State.Pid}}' "$container")
qemu_pid=$(timeout 5 docker top "$container" -eo pid,comm,args | awk 'NR > 1 && $2 != "tini" && /qemu-system-loongarch64/ { print $1; exit }')
[ -n "$qemu_pid" ] || qemu_pid=$container_pid
case "$qemu_pid" in ''|*[!0-9]*) echo "profile host: unable to resolve QEMU host PID" >&2; exit 1 ;; esac

setsid sh -c '
    run_dir=$1
    while :; do cat "$run_dir/serial.in"; done |
        sudo -n socat STDIO "UNIX-CONNECT:$run_dir/serial.sock" |
        tee "$run_dir/profile.serial.log" >/dev/null
' sh "$run_dir" &
logger_pid=$!

{
    sleep_ms 100
    printf '%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"cont"}'
    sleep_ms 300
} | timeout 10 sudo -n socat - "UNIX-CONNECT:$run_dir/qmp.sock" >"$run_dir/qmp.log"

wait_for_fixed '[init] press Ctrl+C within 3 seconds' "$boot_timeout_ms" || {
    echo "profile host: init interrupt prompt timed out" >&2; exit 1;
}
sleep_ms 1500
timeout 2 sh -c 'printf "\003" >"$1"' sh "$run_dir/serial.in"
wait_for_fixed '[init] Ctrl+C detected, entering shell' 10000 || {
    echo "profile host: failed to enter the init shell" >&2; exit 1;
}
wait_for_fixed '~ # ' 10000 || { echo "profile host: init shell prompt timed out" >&2; exit 1; }
sleep_ms 500

send_line 'mkdir -p /tmp/p && echo @""@PROFILE_SETUP_1'
wait_for_fixed "@@PROFILE_SETUP_1" 10000 || { echo "profile host: guest setup mkdir failed" >&2; exit 1; }
send_line 'mount /dev/vd1 /tmp/p && echo @""@PROFILE_SETUP_2'
wait_for_fixed "@@PROFILE_SETUP_2" 10000 || { echo "profile host: guest setup mount failed" >&2; exit 1; }
send_line '. /tmp/p/config.env && echo @""@PROFILE_SETUP_3'
wait_for_fixed "@@PROFILE_SETUP_3" 10000 || { echo "profile host: guest setup config failed" >&2; exit 1; }
send_line '/tmp/p/run.sh run "$PROFILE_RUN_TOKEN" &'

marker_deadline=$(deadline_after_ms 60000)
while ! grep -q "@@PROFILE_WORKLOAD .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
    if grep -Eq 'profile runner:|mount: .*failed|PROFILE_RUNNER_DONE' "$run_dir/profile.serial.log" 2>/dev/null; then
        echo "profile host: guest setup failed" >&2; exit 1
    fi
    [ "$(monotonic_ns)" -lt "$marker_deadline" ] || { echo "profile host: workload marker timed out" >&2; exit 1; }
    sleep_ms 20
done

marker=$(grep "@@PROFILE_WORKLOAD .* token=$run_token" "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
workload_log_line=$(grep -n "@@PROFILE_WORKLOAD .* token=$run_token" "$run_dir/profile.serial.log" | tail -n 1 | cut -d: -f1)
case "$workload_log_line" in ''|*[!0-9]*) echo "profile host: malformed workload marker position" >&2; exit 1 ;; esac
workload_log_offset=$(wc -c <"$run_dir/profile.serial.log" | tr -d ' ')
case "$workload_log_offset" in ''|*[!0-9]*) echo "profile host: malformed workload marker offset" >&2; exit 1 ;; esac
workload_pid=$(printf '%s\n' "$marker" | sed -n 's/.* pid=\([0-9][0-9]*\).*/\1/p')
workload_start=$(printf '%s\n' "$marker" | sed -n 's/.* start_ticks=\([0-9][0-9]*\).*/\1/p')
case "$workload_pid:$workload_start" in *[!0-9:]*|:|:*|*:) echo "profile host: malformed workload identity" >&2; exit 1 ;; esac

# Arm guest filesystem stage watchers before opening Cargo's start gate. This
# makes the first observed output artifact a reliable lower boundary instead
# of racing host-side serial setup against an already-running workload.
if [ "$anchor" = aws-object ]; then
    send_line "/tmp/p/run.sh w $run_token aws-first-object &"
    wait_for_fixed "@@PROFILE_STAGE_WATCH_READY name=aws-first-object token=$run_token" 10000 || {
        echo "profile host: aws object stage watcher did not become ready" >&2; exit 1;
    }
fi

# Cargo is born behind a guest-side gate. Open it before searching for an
# anchor that requires workload output; workload/zero-warmup remains gated
# until the measured window is fully prepared.
case "$anchor:$warmup_ms" in
    workload:0) ;;
    *)
        send_line "/tmp/p/run.sh g $run_token"
        wait_for_fixed "@@PROFILE_GATE_OPENED token=$run_token" 10000 || {
            echo "profile host: workload start gate timed out" >&2; exit 1;
        }
        ;;
esac

anchor_deadline=$(deadline_after_ms "$stage_timeout_ms")
anchor_ns=
while [ -z "$anchor_ns" ]; do
    record_progress
    case "$anchor" in
        workload) anchor_ns=$(monotonic_ns) ;;
        aws-object)
            serial_after_workload_has "@@PROFILE_STAGE name=aws-first-object token=$run_token" &&
                anchor_ns=$(monotonic_ns)
            ;;
        cargo:*) [ -e "$run_dir/progress-$anchor_progress" ] && anchor_ns=$(monotonic_ns) ;;
        marker:*) serial_after_workload_has "$anchor_marker" && anchor_ns=$(monotonic_ns) ;;
    esac
    if workload_finished; then break; fi
    [ "$(monotonic_ns)" -lt "$anchor_deadline" ] || { echo "profile host: stage anchor timed out" >&2; exit 1; }
    [ -n "$anchor_ns" ] || sleep_ms "$poll_ms"
done
printf '%s\n' "${anchor_ns:-0}" >"$run_dir/anchor-monotonic-ns"

workload_ended=0
if [ -z "$anchor_ns" ]; then
    workload_ended=1
else
    warmup_deadline=$((anchor_ns + warmup_ms * 1000000))
    while [ "$(monotonic_ns)" -lt "$warmup_deadline" ]; do
        record_progress
        if workload_finished; then workload_ended=1; break; fi
        sleep_ms "$poll_ms"
    done
fi

capture_started=0
sample_host prestart
if [ "$workload_ended" -eq 0 ]; then
    send_line "/tmp/p/run.sh a $run_token"
    capture_deadline=$(deadline_after_ms "$capture_start_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_READY token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        record_progress
        if workload_finished; then
            workload_ended=1
            break
        fi
        [ "$(monotonic_ns)" -lt "$capture_deadline" ] || { echo "profile host: capture start timed out" >&2; exit 1; }
        sleep_ms "$poll_ms"
    done
    [ "$workload_ended" -eq 1 ] || capture_started=$capture
fi

# Anchor discovery may have observed Cargo output before the measured window.
# Reset exported milestones at the START gate so summary.json cannot mix the
# warmup/stage interval with the profiler and QEMU CPU interval.
window_progress=$(current_progress)
[ -n "$window_progress" ] || window_progress=-1
last_progress=$window_progress
for milestone in 0 64 128 256 384 440 446; do
    rm -f "$run_dir/progress-$milestone"
done
printf 'milestone\tmonotonic_ns\n' >"$run_dir/progress.tsv"

start_ns=$(monotonic_ns)
printf '%s\n' "$start_ns" >"$run_dir/host-window-start-ns"
sample_qemu_boundary start "$start_ns"
start_observed_ns=$start_ns
if [ "$workload_ended" -eq 0 ]; then
    send_line "/tmp/p/run.sh c $run_token"
    wait_for_fixed "@@PROFILE_WINDOW_STARTED token=$run_token" 10000 || {
        echo "profile host: workload window resume timed out" >&2; exit 1;
    }
    wait_for_fixed "@@PROFILE_CARGO_EXEC token=$run_token" 10000 || {
        echo "profile host: cargo did not cross the start gate" >&2; exit 1;
    }
    start_observed_ns=$(monotonic_ns)
fi
deadline_ns=$((start_ns + duration_ms * 1000000))
next_sample_ns=$((start_ns + sample_ms * 1000000))
while [ "$workload_ended" -eq 0 ]; do
    now_ns=$(monotonic_ns)
    if workload_finished; then
        workload_ended=1
        break
    fi
    [ "$now_ns" -lt "$deadline_ns" ] || break
    record_progress
    if [ "$now_ns" -ge "$next_sample_ns" ]; then
        sample_host interval
        next_sample_ns=$((now_ns + sample_ms * 1000000))
    fi
    sleep_ms "$poll_ms"
done

stop_sent=0
stop_request_ns=$(monotonic_ns)
if [ "$workload_ended" -eq 0 ]; then
    stop_sent=1
    send_line "/tmp/p/run.sh z $run_token"
    stop_deadline=$(deadline_after_ms "$done_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_FROZEN .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        [ "$(monotonic_ns)" -lt "$stop_deadline" ] || { echo "profile host: window freeze timed out" >&2; exit 1; }
        sleep_ms "$poll_ms"
    done
else
    stop_deadline=$(deadline_after_ms "$done_timeout_ms")
    while ! grep -q "@@PROFILE_WINDOW_FROZEN .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
        [ "$(monotonic_ns)" -lt "$stop_deadline" ] || break
        sleep_ms "$poll_ms"
    done
fi
stop_ns=$(monotonic_ns)
sample_qemu_boundary stop "$stop_ns"
stop_progress=$(current_progress)
[ -n "$stop_progress" ] || stop_progress=-1
snapshot_deadline=$(deadline_after_ms "$done_timeout_ms")
while ! grep -q "@@PROFILE_WINDOW_STOPPED .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
    [ "$(monotonic_ns)" -lt "$snapshot_deadline" ] || { echo "profile host: window snapshot timed out" >&2; exit 1; }
    sleep_ms "$poll_ms"
done
if [ "$stop_sent" -eq 1 ]; then
    wait_for_fixed "PROFILE_STOP_SENT token=$run_token" "$done_timeout_ms" || {
        echo "profile host: workload termination timed out" >&2; exit 1;
    }
fi
stop_command_sent_ns=$(monotonic_ns)
sample_host poststop
printf '%s\n' "$stop_request_ns" >"$run_dir/host-stop-request-ns"
printf '%s\n' "$stop_ns" >"$run_dir/host-stop-sent-ns"
printf '%s\n' "$stop_command_sent_ns" >"$run_dir/host-stop-command-complete-ns"

done_deadline=$(deadline_after_ms "$done_timeout_ms")
capture_stop_observed_ns=0
while ! grep -q "PROFILE_RUNNER_DONE .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; do
    if [ "$capture_stop_observed_ns" -eq 0 ] && grep -q "@@PROFILE_WINDOW_STOPPED .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; then
        capture_stop_observed_ns=$(monotonic_ns)
    fi
    [ "$(monotonic_ns)" -lt "$done_deadline" ] || { echo "profile host: after-snapshot timed out" >&2; exit 1; }
    sleep_ms "$poll_ms"
done
done_ns=$(monotonic_ns)
sample_host final
if [ "$capture_stop_observed_ns" -eq 0 ] && grep -q "@@PROFILE_WINDOW_STOPPED .* token=$run_token" "$run_dir/profile.serial.log" 2>/dev/null; then
    capture_stop_observed_ns=$done_ns
fi

runner_status=$(sed -n "s/.*PROFILE_RUNNER_DONE status=\([0-9][0-9]*\) token=$run_token.*/\1/p" "$run_dir/profile.serial.log" | tail -n 1 | tr -d '\r')
case "$runner_status" in ''|*[!0-9]*) echo "profile host: malformed runner status" >&2; exit 1 ;; esac
if [ "$capture_started" -eq 1 ]; then
    "$repo/scripts/profile-report.sh" "$run_dir/profile.serial.log" >"$run_dir/profile.report" 2>"$run_dir/profile-report.err"
    profile_report_status=available
else
    printf 'unavailable\n' >"$run_dir/profile.report"
    : >"$run_dir/profile-report.err"
    profile_report_status=unavailable
fi

python3 - "$run_dir" "$anchor" "$anchor_ns" "$start_ns" "$stop_ns" "$done_ns" "$stop_sent" "$runner_status" "$profile_report_status" "$capture_started" "$window_progress" "$stop_progress" "$start_observed_ns" "$stop_request_ns" "$stop_command_sent_ns" "$capture_stop_observed_ns" <<'PY'
import csv, json, pathlib, sys
run_dir = pathlib.Path(sys.argv[1])
metadata = {}
for line in (run_dir / "metadata.env").read_text().splitlines():
    key, value = line.split("=", 1)
    metadata[key] = value
progress = {}
with (run_dir / "progress.tsv").open() as f:
    for row in csv.DictReader(f, delimiter="\t"):
        progress[row["milestone"]] = int(row["monotonic_ns"])
for milestone in ("0", "64", "128", "256", "384", "440", "446"):
    progress.setdefault(milestone, None)
with (run_dir / "host-samples.tsv").open() as f:
    samples = list(csv.DictReader(f, delimiter="\t"))
with (run_dir / "qemu-cpu-boundaries.tsv").open() as f:
    boundaries = {row["phase"]: row for row in csv.DictReader(f, delimiter="\t")}
first_sample = samples[0]
last_sample = samples[-1]
start_cpu = boundaries["start"]
stop_cpu = boundaries["stop"]
ticks_per_second = int(metadata["host_clock_ticks_per_second"])
qemu_cpu_ticks = (
    int(stop_cpu["qemu_utime_ticks"]) + int(stop_cpu["qemu_stime_ticks"])
    - int(start_cpu["qemu_utime_ticks"]) - int(start_cpu["qemu_stime_ticks"])
)
summary = {
    "schema": "mygo.buildstorm-profile.v2",
    "run_dir": str(run_dir),
    "metadata": metadata,
    "timing": {
        "stage_anchor": sys.argv[2],
        "anchor_monotonic_ns": int(sys.argv[3] or 0),
        "window_start_monotonic_ns": int(sys.argv[4]),
        "window_start_progress": int(sys.argv[11]),
        "window_stop_progress": int(sys.argv[12]),
        "window_start_observed_monotonic_ns": int(sys.argv[13]),
        "start_observation_latency_ms": (int(sys.argv[13]) - int(sys.argv[4])) / 1_000_000,
        "stop_request_monotonic_ns": int(sys.argv[14]),
        "stop_monotonic_ns": int(sys.argv[5]),
        "stop_observation_latency_ms": (int(sys.argv[5]) - int(sys.argv[14])) / 1_000_000,
        "done_monotonic_ns": int(sys.argv[6]),
        "stop_command_complete_monotonic_ns": int(sys.argv[15]),
        "capture_stop_observed_monotonic_ns": int(sys.argv[16]),
        "termination_command_latency_ms": (int(sys.argv[15]) - int(sys.argv[5])) / 1_000_000,
        "elapsed_ms": (int(sys.argv[5]) - int(sys.argv[4])) / 1_000_000,
        "cargo_progress_monotonic_ns": progress,
    },
    "result": {
        "deadline_stop_sent": bool(int(sys.argv[7])),
        "workload_ended_early": not bool(int(sys.argv[7])),
        "runner_status": int(sys.argv[8]),
    },
    "profiling": {
        "capture_started": bool(int(sys.argv[10])),
        "mode": (
            "off" if metadata["capture_enabled"] == "0" else
            "trace" if metadata["trace_enabled"] == "1" else
            "sampled" if metadata["sampling_enabled"] == "1" else
            "counts-only"
        ),
        "observation_poll_upper_bound_ms": int(metadata["poll_ms"]),
        "report_status": sys.argv[9],
        "report": "profile.report",
    },
    "host": {
        "sample_count": len(samples),
        "first_sample": first_sample,
        "last_sample": last_sample,
        "qemu_cpu_start": start_cpu,
        "qemu_cpu_stop": stop_cpu,
        "qemu_cpu_ticks": qemu_cpu_ticks,
        "qemu_cpu_seconds": qemu_cpu_ticks / ticks_per_second,
    },
    "host_samples_tsv": "host-samples.tsv",
    "profile_report": "profile.report",
}
(run_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY

elapsed_ns=$((stop_ns - start_ns))
normal_exit=1
printf 'PROFILE_HOST_DONE run_dir=%s elapsed_ms=%d.%06d status=%s stopped=%s\n' \
    "$run_dir" "$((elapsed_ns / 1000000))" "$((elapsed_ns % 1000000))" "$runner_status" "$stop_sent"
