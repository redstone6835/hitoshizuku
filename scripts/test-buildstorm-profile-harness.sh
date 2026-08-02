#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
host=$repo/scripts/buildstorm-profile-host.sh
linux_host=$repo/scripts/buildstorm-profile-linux.sh
guest=$repo/scripts/buildstorm-profile-guest.sh

sh -n "$host"
sh -n "$linux_host"
sh -n "$guest"
[ -x "$linux_host" ] || {
    echo "linux fixture: thin runner is not executable" >&2
    exit 1
}
[ "$(wc -l <"$linux_host")" -le 40 ] || {
    echo "linux fixture: runner duplicated the common host implementation" >&2
    exit 1
}

grep -Fq -- '--kernel-image "$run_dir/kernel-la"' "$host" || {
    echo "observer fixture: host does not bind the daemon to its kernel image" >&2
    exit 1
}
grep -Fq -- '--symbol-manifest "$run_dir/kernel.map.manifest"' "$host" || {
    echo "observer fixture: host does not pass the symbol manifest to the daemon" >&2
    exit 1
}
grep -Fq -- '--plugin-socket "$runtime_socket_root/qemu-observer.sock"' "$host" || {
    echo "observer fixture: host does not use a bounded Unix socket path" >&2
    exit 1
}
grep -Fq -- '--plugin-summary "$run_dir/qemu-observer-plugin-summary.json"' "$host" || {
    echo "observer fixture: host does not request plugin exit reconciliation" >&2
    exit 1
}
grep -Fq 'smp=${PROFILE_SMP:-12}' "$host" || {
    echo "SMP fixture: host does not default to the LoongArch evaluation CPU count" >&2
    exit 1
}
grep -Fq -- '-smp "$smp"' "$host" || {
    echo "SMP fixture: QEMU does not use the configured CPU count" >&2
    exit 1
}
grep -Fq -- '--vcpu-count "$smp"' "$host" || {
    echo "SMP fixture: observer does not use the configured CPU count" >&2
    exit 1
}
grep -Fq -- '--environment "smp=$smp"' "$host" || {
    echo "SMP fixture: observer metadata does not use the configured CPU count" >&2
    exit 1
}
if grep -Eq '(^|[^A-Za-z_])(smp=8|-smp 8|vcpu-count 8)([^0-9]|$)' "$host"; then
    echo "SMP fixture: host still hard-codes an eight-CPU profile" >&2
    exit 1
fi
grep -Fq 'rm -f "$stage/profile-capture.sh" "$stage/run.sh" "$stage/config.env"' "$host" || {
    echo "cleanup fixture: host leaves generated tool configuration behind" >&2
    exit 1
}
grep -Fq 'serial_sync_attempts" -lt 3' "$host" || {
    echo "serial fixture: interactive shell synchronization is not retried" >&2
    exit 1
}
grep -Fq '@\"\"@PROFILE_CONSOLE_SYNC token=$run_token' "$host" || {
    echo "serial fixture: synchronization marker can be matched from command echo" >&2
    exit 1
}
grep -Fq 'send_line "/tmp/p/run.sh k $run_token"' "$host" || {
    echo "boundary fixture: host does not acknowledge the frozen window" >&2
    exit 1
}
grep -Fq 'wait_for_fixed "@@PROFILE_CONTROLLER_READY pid=$workload_pid start_ticks=$workload_start token=$run_token"' "$host" || {
    echo "gate fixture: host does not wait for the bound window controller" >&2
    exit 1
}
grep -Fq 'wait_for_fixed "@@PROFILE_GATE_READY token=$run_token"' "$host" || {
    echo "gate fixture: host can release the FIFO before its reader is ready" >&2
    exit 1
}
runner_result_body=$(sed -n '/^runner_status=null$/,/^done_ns=/p' "$host")
printf '%s\n' "$runner_result_body" | grep -Fq 'termination_mode=host-qemu-teardown' || {
    echo "teardown fixture: deadline result does not identify host QEMU teardown" >&2
    exit 1
}
printf '%s\n' "$runner_result_body" | grep -Fq 'if [ "$actual_stop_sent" -eq 0 ]; then' || {
    echo "teardown fixture: runner completion wait is not limited to natural completion" >&2
    exit 1
}
printf '%s\n' "$runner_result_body" | grep -Fq 'termination_mode=guest-runner-complete' || {
    echo "teardown fixture: natural completion mode is not explicit" >&2
    exit 1
}
printf '%s\n' "$runner_result_body" | grep -Fq 'while ! grep -q "PROFILE_RUNNER_DONE' || {
    echo "teardown fixture: natural completion no longer observes the guest runner" >&2
    exit 1
}
if grep -Fq 'wait_for_fixed "PROFILE_STOP_SENT' "$host" || grep -Fq 'after-snapshot timed out' "$host"; then
    echo "teardown fixture: deadline still waits for guest teardown after STOPPED" >&2
    exit 1
fi
deadline_stop_body=$(sed -n '/^stop_request_ns=$(monotonic_ns)$/,/^stop_ns=$(monotonic_ns)$/p' "$host")
deadline_observer_line=$(printf '%s\n' "$deadline_stop_body" | grep -n -F 'qemu_profile_daemon.py" ctl' | head -n 1 | cut -d: -f1)
deadline_guest_stop_line=$(printf '%s\n' "$deadline_stop_body" | grep -n -F 'send_line "/tmp/p/run.sh z $run_token"' | cut -d: -f1)
deadline_boundary_line=$(printf '%s\n' "$deadline_stop_body" | grep -n -F 'sample_qemu_boundary stop "$measurement_stop_ns"' | cut -d: -f1)
case "$deadline_observer_line:$deadline_guest_stop_line:$deadline_boundary_line" in
    *[!0-9:]*) echo "boundary fixture: malformed deadline stop ordering" >&2; exit 1 ;;
esac
if [ "$deadline_boundary_line" -ge "$deadline_observer_line" ] || \
    [ "$deadline_observer_line" -ge "$deadline_guest_stop_line" ]; then
    echo "boundary fixture: guest quiescence work contaminates the QEMU observer window" >&2
    exit 1
fi
grep -Fq 'PROFILE_WINDOW_FROZEN ended=$ended token=$token' "$guest" || {
    echo "boundary fixture: guest does not publish the frozen boundary" >&2
    exit 1
}
grep -Fq 'quiescence_verified=$quiescence_verified' "$guest" || {
    echo "boundary fixture: guest does not report quiescence verification" >&2
    exit 1
}
grep -Fq 'quiescence_method=$quiescence_method' "$guest" || {
    echo "boundary fixture: guest does not report its quiescence method" >&2
    exit 1
}
grep -Fq "printf 'export PROFILE_BOOT_MODE=%s\\n' \"\$boot_mode\"" "$host" || {
    echo "linux fixture: host does not pass the explicit boot mode to the guest" >&2
    exit 1
}
grep -Fq 'linux-proc-stat-double:*:linux' "$host" || {
    echo "linux fixture: host does not bind proc-stat proof to Linux boot mode" >&2
    exit 1
}
grep -Fq 'expect_controller_state snapshot' "$guest" || {
    echo "boundary fixture: guest does not wait for the host boundary acknowledgment" >&2
    exit 1
}

grep -Fq 'mount -t tmpfs -o size=5G tmpfs "$target_mount"' "$guest" || {
    echo "tmpfs fixture: profiling workload does not mount the official target tmpfs" >&2
    exit 1
}
grep -Fq '@@PROFILE_TARGET_FS type=tmpfs path=/work/tgoskits/target limit=5G' "$guest" || {
    echo "tmpfs fixture: profiling workload does not report its target filesystem" >&2
    exit 1
}
grep -Fq 'mkfifo "/mnt$gate"' "$guest" || {
    echo "gate fixture: cooperative gate is not a FIFO" >&2
    exit 1
}
grep -Fq '[ -p "/mnt$gate" ]' "$guest" || {
    echo "gate fixture: FIFO creation is not verified" >&2
    exit 1
}
grep -Fq 'exec 9<>"$gate" || exit 1' "$guest" || {
    echo "gate fixture: workload does not hold a read/write FIFO endpoint" >&2
    exit 1
}
grep -Fq 'IFS= read -r gate_word <&9' "$guest" || {
    echo "gate fixture: workload gate does not use a blocking shell builtin" >&2
    exit 1
}
grep -Fq 'mkfifo "$control"' "$guest" || {
    echo "controller fixture: control channel is not a FIFO" >&2
    exit 1
}
grep -Fq 'exec 7<>"$control"' "$guest" || {
    echo "controller fixture: parent runner does not hold the control FIFO" >&2
    exit 1
}
grep -Fq 'printf '\''abort\n'\'' >&7' "$guest" || {
    echo "controller fixture: natural workload exit cannot wake the controller" >&2
    exit 1
}
if grep -Fq 'while [ ! -e "$gate" ]; do sleep 0.01; done' "$guest"; then
    echo "gate fixture: closed gate still creates polling processes" >&2
    exit 1
fi
proc_identity_body=$(sed -n '/^proc_identity()/,/^}/p' "$guest")
if printf '%s\n' "$proc_identity_body" | grep -Fq 'task_snapshot_available'; then
    echo "task snapshot fixture: identity lookup renders the snapshot twice" >&2
    exit 1
fi
wait_group_body=$(sed -n '/^wait_group_stopped()/,/^}/p' "$guest")
printf '%s\n' "$wait_group_body" | grep -Fq 'kill -STOP "-$pgrp"' || {
    echo "process-group fixture: stop wait does not catch late-forked members" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq 'window-start) group_stop_limit=1' || {
    echo "process-group fixture: start gate sends redundant stop signals" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq 'sleep 0.25' || {
    echo "process-group fixture: start gate lacks stop propagation time" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq '*) group_stop_limit=25' || {
    echo "process-group fixture: stop sweep is not bounded" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq 'process_group_snapshot_records' || {
    echo "process-group fixture: stop boundary does not use a selected snapshot backend" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq 'expected_start="$group_stop_expected_start"' || {
    echo "process-group fixture: stopped snapshot is not bound to the original leader" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq '$6 != expected_start' || {
    echo "process-group fixture: stopped snapshot omits leader start-time validation" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq 'report_group_stop_timeout "$pgrp" "$group_stop_phase"' || {
    echo "process-group fixture: stop timeout diagnostics are unreachable" >&2
    exit 1
}
if printf '%s\n' "$wait_group_body" | grep -Eq '/proc/\[0-9\]\*/stat|process_group_(state|alive|stopped)'; then
    echo "process-group fixture: stop boundary uses the heavyweight task stat path" >&2
    exit 1
fi
if printf '%s\n' "$wait_group_body" | grep -Fq 'task_snapshot_available'; then
    echo "process-group fixture: stop boundary renders task state before sending SIGSTOP" >&2
    exit 1
fi
linux_snapshot_body=$(sed -n '/^linux_proc_task_snapshot_records()/,/^)/p' "$guest")
printf '%s\n' "$linux_snapshot_body" | grep -Fq '/proc/[0-9]*/task/[0-9]*/stat' || {
    echo "linux fixture: proc fallback does not inspect every thread" >&2
    exit 1
}
printf '%s\n' "$linux_snapshot_body" | grep -Fq 'LC_ALL=C awk -v target="$target"' || {
    echo "linux fixture: proc fallback does not batch task parsing in one process" >&2
    exit 1
}
printf '%s\n' "$linux_snapshot_body" | grep -Fq 'sub(/^[0-9]+ \(.*\) /, "", line)' || {
    echo "linux fixture: proc fallback misparses command names containing a right parenthesis" >&2
    exit 1
}
printf '%s\n' "$linux_snapshot_body" | grep -Fq 'END { if (bad) exit 1 }' || {
    echo "linux fixture: proc fallback accepts a partial or malformed snapshot" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq 'method == "linux-proc-stat-double" && $5 !~ /^[TtZX]$/' || {
    echo "linux fixture: proc fallback accepts a runnable or sleeping thread" >&2
    exit 1
}
printf '%s\n' "$wait_group_body" | grep -Fq '[ "$group_first_snapshot" = "$group_second_snapshot" ]' || {
    echo "linux fixture: proc fallback does not require two stable snapshots" >&2
    exit 1
}
diagnostic_body=$(sed -n '/^report_group_stop_timeout()/,/^}/p' "$guest")
printf '%s\n' "$diagnostic_body" | grep -Fq 'pid=$group_diag_pid ppid=$group_diag_ppid pgid=$group_diag_member_pgrp state=$group_diag_state wchan=$group_diag_wchan comm=$group_diag_comm' || {
    echo "process-group fixture: timeout diagnostics omit member state" >&2
    exit 1
}
printf '%s\n' "$diagnostic_body" | grep -Fq 'elapsed_ms=$group_diag_elapsed_ms' || {
    echo "process-group fixture: timeout diagnostics omit controller delay" >&2
    exit 1
}
printf '%s\n' "$diagnostic_body" | grep -Fq 'd_state_members=$group_diag_d_state late_fork_candidates=$group_diag_late_forks' || {
    echo "process-group fixture: timeout diagnostics omit failure classification" >&2
    exit 1
}
grep -Fq 'wait_group_stopped "$workload_pid" window-start' "$guest" || {
    echo "process-group fixture: window-start diagnostics lack phase context" >&2
    exit 1
}
grep -Fq 'wait_group_stopped "$workload_pid" window-end' "$guest" || {
    echo "process-group fixture: window-end diagnostics lack phase context" >&2
    exit 1
}
capture_controller_body=$(sed -n '/^capture_controller()/,/^}/p' "$guest")
printf '%s\n' "$capture_controller_body" | grep -Fq 'exec 8<>"$control"' || {
    echo "controller fixture: controller does not hold a blocking FIFO endpoint" >&2
    exit 1
}
printf '%s\n' "$capture_controller_body" | grep -Fq 'expect_controller_state start' || {
    echo "controller fixture: controller does not consume ordered start state" >&2
    exit 1
}
printf '%s\n' "$capture_controller_body" | grep -Fq 'expect_controller_state snapshot' || {
    echo "controller fixture: controller does not consume ordered snapshot state" >&2
    exit 1
}
if printf '%s\n' "$capture_controller_body" | grep -Fq 'cat "$control"' || \
    printf '%s\n' "$capture_controller_body" | grep -Fq 'sleep 0.05'; then
    echo "controller fixture: controller still polls state with child processes" >&2
    exit 1
fi
printf '%s\n' "$capture_controller_body" | grep -Fq '@@PROFILE_CONTROLLER_READY pid=$workload_pid start_ticks=$workload_start token=$token' || {
    echo "gate fixture: controller readiness is not bound to the workload identity" >&2
    exit 1
}
printf '%s\n' "$capture_controller_body" | grep -Fq 'controller_expected="$workload_pid $workload_start $token"' || {
    echo "gate fixture: controller does not preserve the verified owner tuple" >&2
    exit 1
}
closed_gate_prefix=$(printf '%s\n' "$capture_controller_body" | sed -n '1,/if \[ "$controller_gate_closed" -eq 0 \]; then/p')
if printf '%s\n' "$closed_gate_prefix" | grep -Fq 'owned_process_group'; then
    echo "gate fixture: closed-gate start repeats the task snapshot identity check" >&2
    exit 1
fi
set_control_body=$(sed -n '/^set_control()/,/^}/p' "$guest")
if printf '%s\n' "$set_control_body" | grep -Eq 'owned_process_group|proc_identity|task_snapshot'; then
    echo "gate fixture: control request takes a task snapshot before stopping Cargo" >&2
    exit 1
fi
set_request_line=$(printf '%s\n' "$set_control_body" | grep -n -F 'echo "PROFILE_WINDOW_REQUESTED state=$state token=$token"' | cut -d: -f1)
set_write_line=$(printf '%s\n' "$set_control_body" | grep -n -F 'printf '\''%s\n'\'' "$state" >"$control"' | cut -d: -f1)
ack_stop_body=$(sed -n '/^ack_stop()/,/^}/p' "$guest")
ack_request_line=$(printf '%s\n' "$ack_stop_body" | grep -n -F 'echo "PROFILE_WINDOW_REQUESTED state=snapshot token=$token"' | cut -d: -f1)
ack_write_line=$(printf '%s\n' "$ack_stop_body" | grep -n -F 'printf '\''snapshot\n'\'' >"$control"' | cut -d: -f1)
case "$set_request_line:$set_write_line:$ack_request_line:$ack_write_line" in
    *[!0-9:]*) echo "controller fixture: malformed request/write ordering" >&2; exit 1 ;;
esac
if [ "$set_request_line" -ge "$set_write_line" ] || [ "$ack_request_line" -ge "$ack_write_line" ]; then
    echo "controller fixture: request evidence is published after controller wakeup" >&2
    exit 1
fi
if printf '%s\n' "$capture_controller_body" | grep -Fq 'kill -STOP "-$workload_pid"'; then
    echo "process-group fixture: controller queues a stop outside the verified stop helper" >&2
    exit 1
fi
frozen_cleanup_body=$(printf '%s\n' "$capture_controller_body" | sed -n '/echo "@@PROFILE_WINDOW_STOPPED/,/return 0/p')
printf '%s\n' "$frozen_cleanup_body" | grep -Fq 'kill -KILL "$workload_pid"' || {
    echo "process-group fixture: frozen window does not terminate the verified leader" >&2
    exit 1
}
if printf '%s\n' "$frozen_cleanup_body" | grep -Fq 'terminate_group "$workload_pid"'; then
    echo "process-group fixture: frozen window traverses the process group during teardown" >&2
    exit 1
fi
stopped_line=$(printf '%s\n' "$frozen_cleanup_body" | grep -n -F 'echo "@@PROFILE_WINDOW_STOPPED' | cut -d: -f1)
kill_line=$(printf '%s\n' "$frozen_cleanup_body" | grep -n -F 'kill -KILL "$workload_pid"' | cut -d: -f1)
stop_sent_line=$(printf '%s\n' "$frozen_cleanup_body" | grep -n -F 'echo "PROFILE_STOP_SENT' | cut -d: -f1)
case "$stopped_line:$kill_line:$stop_sent_line" in
    *[!0-9:]*) echo "process-group fixture: malformed stopped/kill ordering" >&2; exit 1 ;;
esac
if [ "$stopped_line" -ge "$kill_line" ] || [ "$kill_line" -ge "$stop_sent_line" ]; then
    echo "process-group fixture: host takeover marker is published after guest teardown starts" >&2
    exit 1
fi
terminate_group_body=$(sed -n '/^terminate_group()/,/^}/p' "$guest")
printf '%s\n' "$terminate_group_body" | grep -Fq 'kill -CONT "-$pgrp"' || {
    echo "process-group fixture: termination does not continue a stopped group" >&2
    exit 1
}
if printf '%s\n' "$terminate_group_body" | grep -Eq 'kill -0|process_group_(exists|state|alive|stopped)'; then
    echo "process-group fixture: termination probes task state during teardown" >&2
    exit 1
fi
grep -Fq 'echo "@@PROFILE_WORKLOAD_EXIT status=$status token=$token"' "$guest" || {
    echo "process-group fixture: final cleanup lost its workload boundary" >&2
    exit 1
}
final_cleanup_body=$(sed -n '/echo "@@PROFILE_WORKLOAD_EXIT status=\$status token=\$token"/,/rm -f "\$owner"/p' "$guest")
if printf '%s\n' "$final_cleanup_body" | grep -Eq 'kill -0|process_group_(exists|state|alive|stopped)|/proc'; then
    echo "process-group fixture: final cleanup probes task state during teardown" >&2
    exit 1
fi
if printf '%s\n' "$final_cleanup_body" | grep -Fq 'terminate_group'; then
    echo "process-group fixture: final runner cleanup repeats process-group termination" >&2
    exit 1
fi
resume_group_body=$(sed -n '/^resume_group()/,/^}/p' "$guest")
if printf '%s\n' "$resume_group_body" | grep -Eq '/proc|kill -0|process_group_(exists|state|alive|stopped)|while '; then
    echo "process-group fixture: resume boundary probes task state" >&2
    exit 1
fi
printf '%s\n' "$resume_group_body" | grep -Fq 'kill -CONT "-$pgrp"' || {
    echo "process-group fixture: resume does not signal the complete group" >&2
    exit 1
}
[ "$(printf '%s\n' "$resume_group_body" | grep -Fc 'kill -CONT "-$pgrp"')" -eq 1 ] || {
    echo "process-group fixture: resume has multiple unconditional continue sites" >&2
    exit 1
}
printf '%s\n' "$capture_controller_body" | grep -Fq 'if [ "$controller_gate_closed" -eq 0 ]; then' || {
    echo "process-group fixture: cooperative start gate does not bypass start STOP/CONT" >&2
    exit 1
}
if grep -Fq 'buildstorm-profile-resume-ack-' "$guest" || grep -Fq 'buildstorm-profile-resume-probe-' "$guest"; then
    echo "process-group fixture: cooperative start retains obsolete resume side channels" >&2
    exit 1
fi
printf '%s\n' "$capture_controller_body" | grep -Fq 'group_resume_mode=gate' || {
    echo "process-group fixture: cooperative start mode is not reported" >&2
    exit 1
}
printf '%s\n' "$capture_controller_body" | grep -Fq '[ "$controller_gate_closed" -eq 1 ] && [ -e "$controller_gate_released" ]' || {
    echo "process-group fixture: cooperative gate is not revalidated before window start" >&2
    exit 1
}
resume_line=$(printf '%s\n' "$capture_controller_body" | grep -n -F 'resume_group "$workload_pid"' | cut -d: -f1)
started_line=$(printf '%s\n' "$capture_controller_body" | grep -n -F 'echo "@@PROFILE_WINDOW_STARTED token=$token"' | cut -d: -f1)
gate_line=$(printf '%s\n' "$capture_controller_body" | grep -n -F 'printf "go\n" >"$controller_gate"' | cut -d: -f1)
case "$resume_line:$started_line:$gate_line" in
    *[!0-9:]*) echo "process-group fixture: malformed resume/gate ordering" >&2; exit 1 ;;
esac
if [ "$resume_line" -ge "$started_line" ] || [ "$started_line" -ge "$gate_line" ]; then
    echo "process-group fixture: Cargo gate opens before resume proof and start marker" >&2
    exit 1
fi

fixture=$(mktemp)
summary_python=$(mktemp)
guest_library=$(mktemp)
summary_dir=$(mktemp -d)
child_pid_file=$(mktemp)
stage_root=$(mktemp -d)
stage_watch_output=$(mktemp)
slow_awk_dir=$(mktemp -d)
group_pid=
stage_group_pid=
stage_watch_pid=
trap '[ -z "$stage_watch_pid" ] || kill -KILL "$stage_watch_pid" 2>/dev/null || true; [ -z "$stage_group_pid" ] || kill -KILL "-$stage_group_pid" 2>/dev/null || true; [ -z "$group_pid" ] || kill -KILL "-$group_pid" 2>/dev/null || true; rm -f "$fixture" "$summary_python" "$guest_library" "$child_pid_file" "$stage_watch_output" /tmp/buildstorm-profile-owner-fixture-mismatch /tmp/buildstorm-profile-owner-fixture-group /tmp/buildstorm-profile-owner-fixture-stage; rm -rf "$summary_dir" "$stage_root" "$slow_awk_dir"' EXIT INT TERM
printf '#!/bin/sh\nsleep 30\n' >"$slow_awk_dir/awk"
chmod +x "$slow_awk_dir/awk"
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
if PROFILE_DURATION_MS=1 PROFILE_BOOT_MODE=invalid "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid boot mode was accepted" >&2
    exit 1
fi
if PROFILE_DURATION_MS=1 PROFILE_BOOT_MODE=linux PROFILE_CAPTURE=1 "$host" >/dev/null 2>&1; then
    echo "validation fixture: Linux accepted MyGO guest capture" >&2
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
if PROFILE_DURATION_MS=1 PROFILE_STAGE_ANCHOR=aws-objects "$host" >/dev/null 2>&1; then
    echo "validation fixture: invalid aws stage anchor was accepted" >&2
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
if "$guest" watch-stage fixture-token unknown-stage >/dev/null 2>&1; then
    echo "validation fixture: guest accepted an unknown stage watcher" >&2
    exit 1
fi

# The stage watcher must be ready before Cargo's gate opens, report exactly the
# first aws-lc-sys object, and terminate if its owned workload disappears.
setsid sleep 300 &
stage_group_pid=$!
stage_stat=$(cat "/proc/$stage_group_pid/stat")
stage_rest=${stage_stat#*) }
set -- $stage_rest
stage_start=${20}
printf '%s %s %s\n' "$stage_group_pid" "$stage_start" fixture-stage \
    >/tmp/buildstorm-profile-owner-fixture-stage
env PROFILE_STAGE_ROOT="$stage_root" timeout 5 "$guest" \
    watch-stage fixture-stage aws-first-object >"$stage_watch_output" &
stage_watch_pid=$!
attempts=0
while ! grep -q '^@@PROFILE_STAGE_WATCH_READY name=aws-first-object token=fixture-stage$' \
    "$stage_watch_output" 2>/dev/null && [ "$attempts" -lt 100 ]; do
    attempts=$((attempts + 1))
    sleep 0.01
done
[ "$attempts" -lt 100 ] || { echo "stage fixture: watcher did not become ready" >&2; exit 1; }
mkdir -p "$stage_root/work/tgoskits/target/debug/build/aws-lc-sys-fixture/out"
: >"$stage_root/work/tgoskits/target/debug/build/aws-lc-sys-fixture/out/first.o"
wait "$stage_watch_pid"
stage_watch_pid=
grep -q '^@@PROFILE_STAGE name=aws-first-object token=fixture-stage path=/work/tgoskits/target/debug/build/aws-lc-sys-fixture/out/first.o$' \
    "$stage_watch_output" || { echo "stage fixture: first object marker is missing" >&2; exit 1; }
[ "$(grep -c '^@@PROFILE_STAGE name=aws-first-object ' "$stage_watch_output")" -eq 1 ] || {
    echo "stage fixture: object marker was not emitted exactly once" >&2
    exit 1
}
rm -rf "$stage_root/work"
: >"$stage_watch_output"
env PROFILE_STAGE_ROOT="$stage_root" timeout 5 "$guest" \
    watch-stage fixture-stage aws-first-object >"$stage_watch_output" &
stage_watch_pid=$!
attempts=0
while ! grep -q '^@@PROFILE_STAGE_WATCH_READY name=aws-first-object token=fixture-stage$' \
    "$stage_watch_output" 2>/dev/null && [ "$attempts" -lt 100 ]; do
    attempts=$((attempts + 1))
    sleep 0.01
done
[ "$attempts" -lt 100 ] || { echo "stage fixture: exit watcher did not become ready" >&2; exit 1; }
rm -f /tmp/buildstorm-profile-owner-fixture-stage
wait "$stage_watch_pid"
stage_watch_pid=
grep -q '^@@PROFILE_STAGE_SKIPPED name=aws-first-object reason=workload-ended token=fixture-stage$' \
    "$stage_watch_output" || { echo "stage fixture: workload exit was not reported" >&2; exit 1; }
kill -KILL "-$stage_group_pid" 2>/dev/null || true
wait "$stage_group_pid" 2>/dev/null || true
stage_group_pid=

stop_output=$("$guest" stop 999999 1 fixture-no-owner)
case "$stop_output" in
    'PROFILE_STOP_SKIPPED reason=missing-owner token=fixture-no-owner') ;;
    *) echo "stop fixture: unexpected output: $stop_output" >&2; exit 1 ;;
esac

# The leader and a TERM-ignoring descendant share one session/process group.
# A stopped group keeps TERM pending until it is continued, then stop must
# still eliminate every member within its fixed deadline.
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
sed '/^\[ "\$#" -ge 1 \] || usage$/,$d' "$guest" >"$guest_library"
quiescence_output=$(PROFILE_BOOT_MODE=linux timeout 5 sh -c '
    . "$1"
    wait_group_stopped "$2" window-end "$3"
    printf "%s %s %s\n" "$group_stop_verified" "$group_stop_empty" "$group_stop_method"
' sh "$guest_library" "$group_pid" "$start_ticks")
[ "$quiescence_output" = '1 0 linux-proc-stat-double' ] || {
    echo "linux fixture: strict proc-stat quiescence failed: $quiescence_output" >&2
    exit 1
}
stop_output=$(PROFILE_BOOT_MODE=linux PATH="$slow_awk_dir:$PATH" timeout 5 "$guest" stop "$group_pid" "$start_ticks" fixture-group)
case "$stop_output" in
    'PROFILE_STOP_SENT token=fixture-group pid='"$group_pid") ;;
    *) echo "process-group fixture: stopped group did not stop cleanly: $stop_output" >&2; exit 1 ;;
esac
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
python3 "$summary_python" "$summary_dir" workload 90 100 110 120 1 null unavailable 0 0 64 101 109 111 110 1 0 1 0 host-qemu-teardown task-snapshot 109
python3 - "$summary_dir/summary.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["schema"] == "mygo.buildstorm-profile"
assert data["schema_version"] == 2
assert data["timing"]["window_start_progress"] == 0
assert data["timing"]["window_stop_progress"] == 64
assert data["timing"]["start_observation_latency_ms"] == 0.000001
assert data["timing"]["stop_observation_latency_ms"] == 0.000001
assert data["timing"]["measurement_stop_monotonic_ns"] == 109
assert data["timing"]["quiescence_observation_latency_ms"] == 0.000001
assert data["timing"]["elapsed_ms"] == 0.000009
assert data["timing"]["cargo_progress_monotonic_ns"]["128"] is None
assert data["profiling"]["report_status"] == "unavailable"
assert data["profiling"]["mode"] == "off"
assert data["host"]["qemu_cpu_ticks"] == 30
assert data["result"]["deadline_stop_sent"] is True
assert data["result"]["runner_status"] is None
assert data["result"]["runner_status_observed"] is False
assert data["result"]["termination_mode"] == "host-qemu-teardown"
assert data["result"]["stop_requested"] is True
assert data["result"]["window_ended_before_stop"] is False
assert data["result"]["quiescence_verified"] is True
assert data["result"]["quiescence_method"] == "task-snapshot"
PY

python3 "$summary_python" "$summary_dir" workload 90 100 110 120 0 0 unavailable 0 0 64 101 109 111 110 0 1 1 1 guest-runner-complete workload-ended 110
python3 - "$summary_dir/summary.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["result"]["deadline_stop_sent"] is False
assert data["result"]["runner_status"] == 0
assert data["result"]["runner_status_observed"] is True
assert data["result"]["termination_mode"] == "guest-runner-complete"
assert data["result"]["stop_requested"] is False
assert data["result"]["window_ended_before_stop"] is True
assert data["result"]["quiescence_method"] == "workload-ended"
PY

echo "buildstorm profile harness fixtures: ok"
