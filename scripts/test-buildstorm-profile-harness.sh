#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
host=$repo/scripts/buildstorm-profile-host.sh
linux_host=$repo/scripts/buildstorm-profile-linux.sh
guest=$repo/scripts/buildstorm-profile-guest.sh

if grep -Fq 'sudo -n socat' "$host"; then
    echo "socket fixture: same-UID QEMU sockets still require passwordless sudo" >&2
    exit 1
fi

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

rv_config=$(PROFILE_ARCH=riscv64 PROFILE_BOOT_MODE=mygo PROFILE_TARGET_FS=extfs \
    "$host" --print-config)
for expected in \
    'schema=mygo.buildstorm-profile-config.v1' \
    'arch=riscv64' \
    'qemu_binary=qemu-system-riscv64' \
    'kernel_name=kernel-rv' \
    'memory=16G' \
    'smp=8' \
    'block_device=virtio-blk-device' \
    'workload_block_device=virtio-blk-device,drive=x0,bus=virtio-mmio-bus.1' \
    'tools_block_device=virtio-blk-device,drive=x1,bus=virtio-mmio-bus.0' \
    'target_fs=extfs' \
    'histogram_enabled=0' \
    'done_timeout_ms=300000'
do
    printf '%s\n' "$rv_config" | grep -Fxq "$expected" || {
        echo "RV config fixture: missing $expected" >&2
        exit 1
    }
done

histogram_config=$(PROFILE_ARCH=riscv64 PROFILE_HISTOGRAM=1 "$host" --print-config)
printf '%s\n' "$histogram_config" | grep -Fxq 'histogram_enabled=1' || {
    echo "histogram fixture: explicit enable was not preserved" >&2
    exit 1
}

done_timeout_config=$(PROFILE_DONE_TIMEOUT_MS=123 "$host" --print-config)
printf '%s\n' "$done_timeout_config" | grep -Fxq 'done_timeout_ms=123' || {
    echo "timeout fixture: explicit protocol timeout was not preserved" >&2
    exit 1
}
if grep -Fq 'deadline_after_ms "$done_timeout_requested_ms"' "$host"; then
    echo "timeout fixture: protocol waits still use the unbounded requested timeout" >&2
    exit 1
fi
qemu_exit_unbounded_config=$(PROFILE_QEMU_EXIT_TIMEOUT_MS=0 "$host" --print-config)
printf '%s\n' "$qemu_exit_unbounded_config" | grep -Fxq 'qemu_exit_timeout_ms=0' || {
    echo "timeout fixture: explicit unbounded QEMU exit wait was not preserved" >&2
    exit 1
}
qemu_exit_wait_body=$(sed -n '/^if \[ "$qemu_exit_timeout_ms" -eq 0 \]; then$/,/^fi$/p' "$host")
printf '%s\n' "$qemu_exit_wait_body" | grep -Fq 'docker wait "$container"' &&
    printf '%s\n' "$qemu_exit_wait_body" | grep -Fq 'timeout "$qemu_exit_timeout_seconds" docker wait "$container"' || {
    echo "timeout fixture: QEMU exit wait lost its zero/nonzero timeout split" >&2
    exit 1
}

la_config=$(PROFILE_ARCH=loongarch64 PROFILE_BOOT_MODE=mygo PROFILE_TARGET_FS=extfs \
    "$host" --print-config)
for expected in \
    'arch=loongarch64' \
    'qemu_binary=qemu-system-loongarch64' \
    'kernel_name=kernel-la' \
    'memory=36G' \
    'smp=12' \
    'block_device=virtio-blk-pci' \
    'workload_block_device=virtio-blk-pci,drive=x0' \
    'tools_block_device=virtio-blk-pci,drive=x1' \
    'target_fs=extfs'
do
    printf '%s\n' "$la_config" | grep -Fxq "$expected" || {
        echo "LoongArch config fixture: missing $expected" >&2
        exit 1
    }
done

rv_linux_config=$(PROFILE_ARCH=riscv64 "$linux_host" --print-config)
for expected in \
    'arch=riscv64' \
    'boot_mode=linux' \
    "kernel=$repo/build/linux-riscv64/vmlinux" \
    'workload_device=/dev/vda' \
    'tools_device=/dev/vdb' \
    'target_fs=extfs'
do
    printf '%s\n' "$rv_linux_config" | grep -Fxq "$expected" || {
        echo "RV Linux config fixture: missing $expected" >&2
        exit 1
    }
done

if PROFILE_ARCH=invalid "$host" --print-config >/dev/null 2>&1; then
    echo "architecture fixture: invalid architecture was accepted" >&2
    exit 1
fi

if printf '~ # \n@@PROFILE_SETUP_1\n' |
    "$host" --serial-has-ordered '@@PROFILE_SETUP_1' '~ # ' >/dev/null 2>&1; then
    echo "serial fixture: stale prompt before the marker completed a new command" >&2
    exit 1
fi
printf '~ # \n@@PROFILE_SETUP_1\n~ # ' |
    "$host" --serial-has-ordered '@@PROFILE_SETUP_1' '~ # ' >/dev/null 2>&1 || {
    echo "serial fixture: prompt after the marker did not complete the command" >&2
    exit 1
}

set +e
memory_error=$(PROFILE_MEMORY=12xG "$host" --print-config 2>&1)
memory_status=$?
set -e
[ "$memory_status" -ne 0 ] && \
    [ "$memory_error" = 'PROFILE_MEMORY must be a positive integer followed by G or M' ] || {
    echo "memory fixture: malformed memory was not rejected by configuration validation" >&2
    exit 1
}

rv_plan=$(PROFILE_ARCH=riscv64 PROFILE_TARGET_FS=extfs "$guest" plan)
for expected in \
    'schema=mygo.buildstorm-workload.v2' \
    'arch=riscv64' \
    'target=riscv64gc-unknown-linux-musl' \
    'target_fs=extfs' \
    'prebuild=cargo build -p tg-xtask' \
    'command=timeout 14400 cargo xtask arceos build -p arceos-helloworld --arch riscv64'
do
    printf '%s\n' "$rv_plan" | grep -Fxq "$expected" || {
        echo "RV workload fixture: missing $expected" >&2
        exit 1
    }
done

la_plan=$(PROFILE_ARCH=loongarch64 PROFILE_TARGET_FS=extfs "$guest" plan)
for expected in \
    'arch=loongarch64' \
    'target=loongarch64-unknown-linux-musl' \
    'target_fs=extfs' \
    'command=timeout 14400 cargo xtask arceos build -p arceos-helloworld --arch loongarch64'
do
    printf '%s\n' "$la_plan" | grep -Fxq "$expected" || {
        echo "LoongArch workload fixture: missing $expected" >&2
        exit 1
    }
done

grep -Fq -- '--kernel-image "$run_dir/$kernel_name"' "$host" || {
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
publish_capture_window_body=$(sed -n '/^publish_capture_window()/,/^}/p' "$guest")
printf '%s\n' "$publish_capture_window_body" | grep -Fq 'expect_controller_state snapshot' || {
    echo "controller fixture: controller does not consume ordered snapshot state" >&2
    exit 1
}
printf '%s\n' "$capture_controller_body" | grep -Fq 'publish_capture_window' || {
    echo "controller fixture: controller does not use the shared window publisher" >&2
    exit 1
}
if { printf '%s\n' "$capture_controller_body"; printf '%s\n' "$publish_capture_window_body"; } | \
    grep -Eq 'cat "\$control"|sleep 0\.05'; then
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
normal_cleanup_body=$(printf '%s\n' "$capture_controller_body" | \
    sed -n '/publish_capture_window "\$ended"/,/return 0/p')
printf '%s\n' "$normal_cleanup_body" | grep -Fq 'kill -KILL "$workload_pid"' || {
    echo "process-group fixture: frozen window does not terminate the verified leader" >&2
    exit 1
}
if printf '%s\n' "$normal_cleanup_body" | grep -Fq 'terminate_group "$workload_pid"'; then
    echo "process-group fixture: frozen window traverses the process group during teardown" >&2
    exit 1
fi
publish_line=$(printf '%s\n' "$normal_cleanup_body" | grep -n -F 'publish_capture_window "$ended"' | cut -d: -f1)
kill_line=$(printf '%s\n' "$normal_cleanup_body" | grep -n -F 'kill -KILL "$workload_pid"' | cut -d: -f1)
stop_sent_line=$(printf '%s\n' "$normal_cleanup_body" | grep -n -F 'echo "PROFILE_STOP_SENT' | cut -d: -f1)
case "$publish_line:$kill_line:$stop_sent_line" in
    *[!0-9:]*) echo "process-group fixture: malformed stopped/kill ordering" >&2; exit 1 ;;
esac
if [ "$publish_line" -ge "$kill_line" ] || [ "$kill_line" -ge "$stop_sent_line" ]; then
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
final_cleanup_body=$(sed -n '/echo "@@PROFILE_WORKLOAD_EXIT status=\$status token=\$token"/,/finish_natural_capture "\$owner" "\$controller_pid"/p' "$guest")
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
deadline_library=$(mktemp)
termination_library=$(mktemp)
container_user_library=$(mktemp)
summary_dir=$(mktemp -d)
child_pid_file=$(mktemp)
stage_root=$(mktemp -d)
stage_watch_output=$(mktemp)
slow_awk_dir=$(mktemp -d)
natural_dir=$(mktemp -d)
early_dir=$(mktemp -d)
qemu_library=$(mktemp)
qemu_diag_dir=$(mktemp -d)
qemu_fake_bin=$(mktemp -d)
group_pid=
stage_group_pid=
stage_watch_pid=
trap '[ -z "$stage_watch_pid" ] || kill -KILL "$stage_watch_pid" 2>/dev/null || true; [ -z "$stage_group_pid" ] || kill -KILL "-$stage_group_pid" 2>/dev/null || true; [ -z "$group_pid" ] || kill -KILL "-$group_pid" 2>/dev/null || true; rm -f "$fixture" "$summary_python" "$guest_library" "$deadline_library" "$termination_library" "$container_user_library" "$qemu_library" "$child_pid_file" "$stage_watch_output" /tmp/buildstorm-profile-owner-fixture-mismatch /tmp/buildstorm-profile-owner-fixture-group /tmp/buildstorm-profile-owner-fixture-stage; rm -rf "$summary_dir" "$stage_root" "$slow_awk_dir" "$natural_dir" "$early_dir" "$qemu_diag_dir" "$qemu_fake_bin"' EXIT INT TERM
printf '#!/bin/sh\nsleep 30\n' >"$slow_awk_dir/awk"
chmod +x "$slow_awk_dir/awk"
sed -n '/^host_process_alive()/,/^stop_tcg_time_collector()/p' "$host" |
    sed '$d' >"$qemu_library"
qemu_identity_output=$(sh -c '
    . "$1"
    sleep 30 & pid=$!
    qemu_pid=$pid
    qemu_start_ticks=$(host_process_start_ticks "$pid")
    qemu_process_alive || exit 10
    qemu_start_ticks=$((qemu_start_ticks + 1))
    if qemu_process_alive; then exit 11; fi
    kill "$pid"
    wait "$pid" 2>/dev/null || true
    printf "identity-bound\n"
' sh "$qemu_library")
[ "$qemu_identity_output" = identity-bound ] || {
    echo "QEMU liveness fixture: PID/start-time identity was not enforced" >&2
    exit 1
}
printf '#!/bin/sh\ncase "$1" in logs) echo docker-panic-line ;; inspect) echo container-stopped ;; *) exit 2 ;; esac\n' \
    >"$qemu_fake_bin/docker"
chmod +x "$qemu_fake_bin/docker"
printf 'old-line\nserial-panic-line\n' >"$qemu_diag_dir/profile.serial.log"
set +e
qemu_failure_output=$(PATH="$qemu_fake_bin:$PATH" sh -c '
    . "$1"
    run_dir=$2 container=fixture-container qemu_pid=99999999 qemu_start_ticks=1
    qemu_fail_fast guest-prebuild
' sh "$qemu_library" "$qemu_diag_dir" 2>&1)
qemu_failure_status=$?
set -e
[ "$qemu_failure_status" -ne 0 ] || {
    echo "QEMU liveness fixture: dead QEMU did not terminate the wait" >&2
    exit 1
}
printf '%s\n' "$qemu_failure_output" | grep -Fq 'QEMU exited while waiting for guest-prebuild' &&
    printf '%s\n' "$qemu_failure_output" | grep -Fq serial-panic-line &&
    printf '%s\n' "$qemu_failure_output" | grep -Fq docker-panic-line &&
    grep -Fxq guest-prebuild "$qemu_diag_dir/qemu-failure-phase.txt" &&
    grep -Fq docker-panic-line "$qemu_diag_dir/qemu-failure-docker.log" &&
    grep -Fq container-stopped "$qemu_diag_dir/qemu-failure-container-inspect.json" || {
    echo "QEMU liveness fixture: failure diagnostics are incomplete" >&2
    exit 1
}
sed -n '/^window_deadline_ns()/,/^}/p' "$host" >"$deadline_library"
sed -n '/^classify_window_state()/,/^}/p' "$host" >>"$deadline_library"
deadline_output=$(sh -c '. "$1"; printf "zero=%s positive=%s\n" \
    "$(window_deadline_ns 123456789 0)" \
    "$(window_deadline_ns 123456789 250)"' sh "$deadline_library")
[ "$deadline_output" = 'zero= positive=373456789' ] || {
    echo "boundary fixture: window deadline does not preserve zero-duration semantics: $deadline_output" >&2
    exit 1
}
classification_output=$(sh -c '
    . "$1"
    classify_window_state 200 200 1; first=$window_state
    classify_window_state 199 200 1; second=$window_state
    classify_window_state 200 "" 1; third=$window_state
    classify_window_state 199 200 0; fourth=$window_state
    printf "%s %s %s %s\n" "$first" "$second" "$third" "$fourth"
' sh "$deadline_library")
[ "$classification_output" = 'deadline natural natural running' ] || {
    echo "boundary fixture: deadline/workload classification is wrong: $classification_output" >&2
    exit 1
}
sed -n '/^classify_termination()/,/^}/p' "$host" >"$termination_library"
termination_output=$(sh -c '
    . "$1"
    classify_termination 1
    printf "deadline=%s:%s:%s " "$deadline_stop_sent" "$termination_mode" "$runner_status_required"
    classify_termination 0
    printf "natural=%s:%s:%s\n" "$deadline_stop_sent" "$termination_mode" "$runner_status_required"
' sh "$termination_library")
[ "$termination_output" = \
    'deadline=1:host-qemu-teardown:0 natural=0:guest-runner-complete:1' ] || {
    echo "boundary fixture: termination classification is wrong: $termination_output" >&2
    exit 1
}
sed -n '/^select_container_user()/,/^}/p' "$host" >"$container_user_library"
container_user_output=$(sh -c '
    . "$1"
    select_container_user 1000 100 '\''["name=seccomp","name=rootless"]'\''
    printf "rootless=%s:%s:%s " "$container_uid" "$container_gid" "$container_user_flag"
    select_container_user 1000 100 '\''["name=seccomp"]'\''
    printf "rootful=%s:%s:%s\n" "$container_uid" "$container_gid" "$container_user_flag"
' sh "$container_user_library")
[ "$container_user_output" = 'rootless=0:0:0 rootful=1000:100:1' ] || {
    echo "container fixture: Docker user mapping is wrong: $container_user_output" >&2
    exit 1
}
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
mkdir -p "$stage_root/work/tgoskits/target/riscv64gc-unknown-linux-musl/debug/build/aws-lc-sys-fixture/out"
: >"$stage_root/work/tgoskits/target/riscv64gc-unknown-linux-musl/debug/build/aws-lc-sys-fixture/out/first.o"
wait "$stage_watch_pid"
stage_watch_pid=
grep -q '^@@PROFILE_STAGE name=aws-first-object token=fixture-stage path=/work/tgoskits/target/riscv64gc-unknown-linux-musl/debug/build/aws-lc-sys-fixture/out/first.o$' \
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
natural_output=$(timeout 5 sh -c '
    set -eu
    . "$1"
    owner=$2
    control=$3
    output=$4
    printf "fixture-owner\n" >"$owner"
    mkfifo "$control"
    exec 7<>"$control"
    (
        exec 8<>"$control"
        IFS= read -r state <&8
        if [ -r "$owner" ]; then owner_state=present; else owner_state=missing; fi
        printf "state=%s owner=%s\n" "$state" "$owner_state"
    ) >"$output" &
    controller_pid=$!
    finish_natural_capture "$owner" "$controller_pid"
    [ ! -e "$owner" ]
    cat "$output"
' sh "$guest_library" "$natural_dir/owner" "$natural_dir/control" "$natural_dir/output")
[ "$natural_output" = 'state=stop owner=present' ] || {
    echo "controller fixture: natural completion did not preserve owner through stop: $natural_output" >&2
    exit 1
}
early_output=$(PROFILE_CAPTURE=0 PROFILE_BOOT_MODE=linux PROFILE_WORKLOAD=fixture \
    timeout 5 sh -c '
    set -eu
    . "$1"
    early_dir=$2
    setsid sleep 0.2 &
    early_pid=$!
    early_stat=$(cat "/proc/$early_pid/stat")
    early_rest=${early_stat#*) }
    set -- $early_rest
    early_start=${20}
    wait "$early_pid"
    printf "%s %s %s\n" "$early_pid" "$early_start" fixture-early >"$early_dir/owner"
    mkfifo "$early_dir/control"
    exec 7<>"$early_dir/control"
    (set +e; capture_controller fixture-early "$early_dir/owner" "$early_dir/control" \
        "$early_pid" "$early_start") >"$early_dir/output" 2>&1 &
    early_controller_pid=$!
    attempts=0
    while ! grep -q "^@@PROFILE_CONTROLLER_READY .* token=fixture-early$" \
        "$early_dir/output" 2>/dev/null && [ "$attempts" -lt 100 ]; do
        attempts=$((attempts + 1))
        sleep 0.01
    done
    [ "$attempts" -lt 100 ] || exit 1
    # deadline 与自然退出竞争时，host 和 runner 都可能请求停止。
    printf "stop\nstop\n" >&7
    attempts=0
    while ! grep -q "^@@PROFILE_WINDOW_FROZEN ended=1 token=fixture-early " \
        "$early_dir/output" 2>/dev/null && [ "$attempts" -lt 100 ]; do
        attempts=$((attempts + 1))
        sleep 0.01
    done
    [ "$attempts" -lt 100 ] || exit 1
    printf "snapshot\n" >&7
    wait "$early_controller_pid"
    cat "$early_dir/output"
' sh "$guest_library" "$early_dir") || {
    echo "controller fixture: duplicate stop did not complete the empty window: $(cat "$early_dir/output" 2>/dev/null)" >&2
    exit 1
}
printf '%s\n' "$early_output" | grep -q '^@@PROFILE_WINDOW_STOPPED ended=1 token=fixture-early$' || {
    echo "controller fixture: early workload exit did not complete the empty snapshot" >&2
    exit 1
}
mkdir -p "$stage_root/work/tgoskits/target/riscv64gc-unknown-linux-musl" \
    "$stage_root/work/tgoskits/target/debug"
: >"$stage_root/work/tgoskits/target/riscv64gc-unknown-linux-musl/stale.o"
: >"$stage_root/work/tgoskits/target/debug/tg-xtask"
printf '/dev/vda %s ext4 rw 0 0\n' "$stage_root" >"$fixture"
target_output=$(PROFILE_ARCH=riscv64 PROFILE_TARGET_FS=extfs \
    PROFILE_ROOT_MOUNT="$stage_root" PROFILE_MOUNTS_FILE="$fixture" \
    sh -c '. "$1"; resolve_workload; prepare_target' sh "$guest_library")
[ ! -e "$stage_root/work/tgoskits/target/riscv64gc-unknown-linux-musl" ] || {
    echo "target fixture: formal architecture target was not removed" >&2
    exit 1
}
[ -f "$stage_root/work/tgoskits/target/debug/tg-xtask" ] || {
    echo "target fixture: untimed tg-xtask cache was removed" >&2
    exit 1
}
[ "$target_output" = '@@PROFILE_TARGET_FS type=ext4 path=/work/tgoskits/target source=workload' ] || {
    echo "target fixture: unexpected filesystem marker: $target_output" >&2
    exit 1
}
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
printf 'milestone\tmonotonic_ns\n0\t100000000\n64\t110000000\n' >"$summary_dir/progress.tsv"
printf '%b\n' \
    'monotonic_ns\tphase\tprogress\tqemu_utime_ticks\tqemu_stime_ticks\tload1\tload5\tload15\trunnable_total\tlast_pid\tcpu_some_avg10\tcpu_some_total\tio_some_avg10\tio_some_total\tio_full_avg10\tio_full_total\tmemory_some_avg10\tmemory_some_total\tmemory_full_avg10\tmemory_full_total' \
    '100000000\tstart\t0\t10\t5\t1\t1\t1\t1/10\t10\t0\t1\t0\t1\t0\t1\t0\t1\t0\t1' \
    '110000002\tstop\t64\t30\t15\t1\t1\t1\t1/10\t10\t0\t1\t0\t1\t0\t1\t0\t1\t0\t1' \
    '120000000\tfinal\t64\t31\t16\t1\t1\t1\t1/10\t10\t0\t1\t0\t1\t0\t1\t0\t1\t0\t1' \
    >"$summary_dir/host-samples.tsv"
printf '%b\n' \
    'monotonic_ns\tphase\tqemu_utime_ticks\tqemu_stime_ticks' \
    '100000000\tstart\t10\t5' '110000001\tstop\t30\t15' \
    >"$summary_dir/qemu-cpu-boundaries.tsv"
python3 "$summary_python" "$summary_dir" workload 90000000 100000000 110000002 120000000 1 null unavailable 0 0 64 100000001 110000001 111000000 110000003 1 0 1 0 host-qemu-teardown task-snapshot 110000001
python3 - "$summary_dir/summary.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["schema"] == "mygo.buildstorm-profile"
assert data["schema_version"] == 2
assert data["timing"]["window_start_progress"] == 0
assert data["timing"]["window_stop_progress"] == 64
assert data["timing"]["start_observation_latency_ms"] == 0.000001
assert data["timing"]["stop_observation_latency_ms"] == 0.000001
assert data["timing"]["scheduled_deadline_monotonic_ns"] == 110000000
assert data["timing"]["deadline_observation_latency_ms"] == 0.000001
assert data["timing"]["measurement_stop_monotonic_ns"] == 110000001
assert data["timing"]["quiescence_observation_latency_ms"] == 0.000001
assert data["timing"]["elapsed_ms"] == 10.000001
assert data["timing"]["observer_start_lead_latency_ms"] is None
assert data["timing"]["observer_stop_lag_latency_ms"] is None
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

python3 "$summary_python" "$summary_dir" workload 90000000 100000000 110000002 120000000 0 0 unavailable 0 0 64 100000001 109000000 111000000 110000003 0 1 1 1 guest-runner-complete workload-ended 110000002
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
