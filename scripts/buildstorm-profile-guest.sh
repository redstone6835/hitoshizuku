#!/bin/sh
# Run one cold tg-xtask capture, or safely stop a capture started by this script.
set -u

tool_mount=${PROFILE_TOOL_MOUNT:-/tmp/buildstorm-profile-tools}

usage() {
    echo "usage: $0 run <run-token> | watch-stage <run-token> aws-first-object | go <run-token> | arm <run-token> | resume <run-token> | finish <run-token> | stop-token <run-token>" >&2
    exit 2
}

valid_token() {
    case "$1" in
        ''|*[!A-Za-z0-9_.-]*) return 1 ;;
        *) return 0 ;;
    esac
}

valid_event_mask() {
    printf '%s\n' "$1" | LC_ALL=C awk '
        NR == 1 && /^0x[0-9A-Fa-f]+$/ && length($0) >= 3 && length($0) <= 18 { ok = 1 }
        NR != 1 { bad = 1 }
        END { exit !(ok && !bad) }
    '
}

# /proc/PID/stat starts with "PID (comm)". Strip that prefix before selecting
# fields so a command name containing spaces cannot shift pgrp/starttime.
proc_identity() {
    pid=$1
    stat=$(cat "/proc/$pid/stat" 2>/dev/null) || return 1
    rest=${stat#*) }
    [ "$rest" != "$stat" ] || return 1
    set -- $rest
    [ "$#" -ge 20 ] || return 1
    printf '%s %s\n' "$3" "${20}"
}

same_process_group() {
    expected_pid=$1
    expected_start=$2
    identity=$(proc_identity "$expected_pid") || return 1
    [ "$identity" = "$expected_pid $expected_start" ]
}

# Test every non-zombie member because the process-group leader can exit while
# cargo/rustc descendants are still alive.
process_group_state() (
    # A subshell prevents scan variables from overwriting the caller's PGID.
    LC_ALL=C awk -v target="$1" -v wanted="$2" '
        {
            line = $0
            sub(/^[0-9]+ \(.*\) /, "", line)
            split(line, field, " ")
            if (field[3] != target || field[1] == "Z") next
            found = 1
            if (wanted == "stopped" && field[1] != "T" && field[1] != "t") bad = 1
        }
        END {
            if (wanted == "alive") exit !found
            exit !(found && !bad)
        }
    ' /proc/[0-9]*/stat 2>/dev/null
)

process_group_alive() {
    process_group_state "$1" alive
}

process_group_stopped() {
    process_group_state "$1" stopped
}

owned_process_group() {
    expected_pid=$1
    expected_start=$2
    if identity=$(proc_identity "$expected_pid" 2>/dev/null); then
        [ "$identity" = "$expected_pid $expected_start" ]
    else
        process_group_alive "$expected_pid"
    fi
}

wait_group_stopped() {
    pgrp=$1
    group_stop_attempts=0
    while process_group_alive "$pgrp" && ! process_group_stopped "$pgrp"; do
        group_stop_attempts=$((group_stop_attempts + 1))
        [ "$group_stop_attempts" -lt 200 ] || return 1
        sleep 0.01
    done
}

resume_group() {
    pgrp=$1
    group_resume_attempts=0
    while process_group_alive "$pgrp" && process_group_stopped "$pgrp"; do
        kill -CONT "-$pgrp" 2>/dev/null || true
        group_resume_attempts=$((group_resume_attempts + 1))
        [ "$group_resume_attempts" -lt 200 ] || return 1
        sleep 0.01
    done
    process_group_alive "$pgrp" && ! process_group_stopped "$pgrp"
}

terminate_group() {
    pgrp=$1
    kill -TERM "-$pgrp" 2>/dev/null || true
    group_term_attempts=0
    while process_group_alive "$pgrp" && [ "$group_term_attempts" -lt 20 ]; do
        group_term_attempts=$((group_term_attempts + 1))
        sleep 0.01
    done
    if process_group_alive "$pgrp"; then
        kill -KILL "-$pgrp" 2>/dev/null || true
        group_kill_attempts=0
        while process_group_alive "$pgrp" && [ "$group_kill_attempts" -lt 100 ]; do
            group_kill_attempts=$((group_kill_attempts + 1))
            sleep 0.01
        done
    fi
    ! process_group_alive "$pgrp"
}

read_owner() {
    token=$1
    owner=/tmp/buildstorm-profile-owner-$token
    owner_record=$(cat "$owner" 2>/dev/null) || return 1
    set -- $owner_record
    [ "$#" -eq 3 ] || return 1
    owner_pid=$1
    owner_start=$2
    owner_token=$3
    [ "$owner_token" = "$token" ] || return 1
    owned_process_group "$owner_pid" "$owner_start" || return 1
}

set_control() {
    [ "$#" -eq 2 ] || usage
    state=$1
    token=$2
    valid_token "$token" || usage
    if ! read_owner "$token"; then
        echo "PROFILE_WINDOW_SKIPPED state=$state reason=workload-ended token=$token"
        exit 0
    fi
    printf '%s\n' "$state" >"/tmp/buildstorm-profile-control-$token"
    echo "PROFILE_WINDOW_REQUESTED state=$state token=$token"
}

release_workload() {
    [ "$#" -eq 1 ] || usage
    token=$1
    valid_token "$token" || usage
    if ! read_owner "$token"; then
        echo "PROFILE_GATE_SKIPPED reason=workload-ended token=$token"
        exit 0
    fi
    : >"/mnt/run/buildstorm-profile-gate-$token"
    echo "@@PROFILE_GATE_OPENED token=$token"
}

watch_stage() {
    [ "$#" -eq 2 ] || usage
    token=$1
    stage_name=$2
    valid_token "$token" || usage
    case "$stage_name" in
        aws-first-object) ;;
        *) usage ;;
    esac
    stage_root=${PROFILE_STAGE_ROOT:-/mnt}
    case "$stage_root" in
        /*) ;;
        *) echo "profile runner: PROFILE_STAGE_ROOT must be absolute" >&2; return 2 ;;
    esac
    if ! read_owner "$token"; then
        echo "@@PROFILE_STAGE_SKIPPED name=$stage_name reason=workload-ended token=$token"
        return 0
    fi
    echo "@@PROFILE_STAGE_WATCH_READY name=$stage_name token=$token"
    while read_owner "$token"; do
        for object in "$stage_root"/work/tgoskits/target/debug/build/aws-lc-sys-*/out/*.o; do
            [ -f "$object" ] || continue
            relative=${object#"$stage_root"}
            echo "@@PROFILE_STAGE name=$stage_name token=$token path=$relative"
            return 0
        done
        sleep 0.05
    done
    echo "@@PROFILE_STAGE_SKIPPED name=$stage_name reason=workload-ended token=$token"
}

capture_controller() {
    token=$1
    owner=$2
    control=$3
    timeout_ms=${PROFILE_CONTROLLER_TIMEOUT_MS:-1800000}
    case "$timeout_ms" in ''|*[!0-9]*) echo "profile runner: invalid controller timeout" >&2; return 2 ;; esac
    attempts=$((timeout_ms / 50 + 1))

    while [ "$attempts" -gt 0 ] && [ -r "$owner" ]; do
        if [ "$(cat "$control" 2>/dev/null || true)" = start ]; then
            read -r workload_pid workload_start owner_token <"$owner" 2>/dev/null || {
                echo "PROFILE_CAPTURE_SKIPPED reason=workload-ended token=$token"
                return 0
            }
            if [ "$owner_token" != "$token" ] || ! owned_process_group "$workload_pid" "$workload_start"; then
                echo "PROFILE_CAPTURE_SKIPPED reason=identity-mismatch token=$token"
                return 0
            fi
            kill -STOP "-$workload_pid" 2>/dev/null || true
            wait_group_stopped "$workload_pid" || {
                echo "profile runner: workload group did not stop at window start" >&2
                return 1
            }
            if [ "$PROFILE_CAPTURE" -eq 1 ]; then
                PROFILE_LEAVE_FROZEN=1 /bin/sh /tmp/profile-capture.sh start "$PROFILE_WORKLOAD" || return 1
            fi
            echo "@@PROFILE_WINDOW_READY token=$token"
            while [ "$attempts" -gt 0 ] && [ -r "$owner" ] && [ "$(cat "$control" 2>/dev/null || true)" != resume ]; do
                attempts=$((attempts - 1))
                sleep 0.05
            done
            [ -r "$owner" ] || {
                echo "PROFILE_CAPTURE_SKIPPED reason=workload-ended-before-resume token=$token"
                return 0
            }
            : >"/mnt/run/buildstorm-profile-gate-$token"
            if [ "$PROFILE_CAPTURE" -eq 1 ]; then
                printf 'resume\n' >"${PROFILE_CONTROL:-/sys/kernel/profile_control}" || return 1
            fi
            resume_group "$workload_pid" || {
                terminate_group "$workload_pid" || true
                echo "profile runner: workload group did not resume at window start" >&2
                return 1
            }
            echo "@@PROFILE_WINDOW_STARTED token=$token"
            while [ "$attempts" -gt 0 ] && [ -r "$owner" ] && [ "$(cat "$control" 2>/dev/null || true)" != stop ]; do
                attempts=$((attempts - 1))
                sleep 0.05
            done
            ended=0
            if [ -r "$owner" ] && owned_process_group "$workload_pid" "$workload_start"; then
                kill -STOP "-$workload_pid" 2>/dev/null || true
                wait_group_stopped "$workload_pid" || {
                    echo "profile runner: workload group did not stop at window end" >&2
                    return 1
                }
            else
                ended=1
            fi
            if [ "$PROFILE_CAPTURE" -eq 1 ]; then
                printf 'freeze\n' >"${PROFILE_CONTROL:-/sys/kernel/profile_control}" || return 1
            fi
            # This marker defines the shared host/QEMU/profiler stop boundary.
            # Snapshot rendering happens afterwards and is outside the window.
            echo "@@PROFILE_WINDOW_FROZEN ended=$ended token=$token"
            if [ "$PROFILE_CAPTURE" -eq 1 ]; then
                PROFILE_ALREADY_FROZEN=1 /bin/sh /tmp/profile-capture.sh stop "$PROFILE_WORKLOAD" || return 1
            fi
            echo "@@PROFILE_WINDOW_STOPPED ended=$ended token=$token"
            if [ "$ended" -eq 0 ]; then
                # A stopped task in uninterruptible I/O may outlive this first
                # bounded SIGKILL sweep. The parent runner performs the final
                # group check after reaping cargo, so do not strand the host at
                # the post-snapshot handshake while that cleanup converges.
                terminate_group "$workload_pid" ||
                    echo "profile runner: workload group still draining after window termination" >&2
                echo "PROFILE_STOP_SENT token=$token pid=$workload_pid"
            fi
            return 0
        fi
        attempts=$((attempts - 1))
        sleep 0.05
    done
    echo "PROFILE_CAPTURE_SKIPPED reason=not-armed token=$token"
    return 0
}

stop_run() {
    [ "$#" -eq 3 ] || usage
    pid=$1
    start_ticks=$2
    token=$3
    case "$pid:$start_ticks" in
        *[!0-9:]*) usage ;;
    esac
    valid_token "$token" || usage

    owner=/tmp/buildstorm-profile-owner-$token
    expected="$pid $start_ticks $token"
    actual=$(cat "$owner" 2>/dev/null) || {
        echo "PROFILE_STOP_SKIPPED reason=missing-owner token=$token"
        exit 0
    }
    if [ "$actual" != "$expected" ] || ! owned_process_group "$pid" "$start_ticks"; then
        echo "PROFILE_STOP_SKIPPED reason=identity-mismatch token=$token"
        exit 0
    fi

    terminate_group "$pid" || {
        echo "profile runner: process group $pid survived termination" >&2
        exit 1
    }
    echo "PROFILE_STOP_SENT token=$token pid=$pid"
}

stop_token() {
    [ "$#" -eq 1 ] || usage
    token=$1
    valid_token "$token" || usage
    owner=/tmp/buildstorm-profile-owner-$token
    read -r pid start_ticks owner_token <"$owner" 2>/dev/null || {
        echo "PROFILE_STOP_SKIPPED reason=missing-owner token=$token"
        exit 0
    }
    [ "$owner_token" = "$token" ] || {
        echo "PROFILE_STOP_SKIPPED reason=identity-mismatch token=$token"
        exit 0
    }
    stop_run "$pid" "$start_ticks" "$token"
}

run_profile() {
    [ "$#" -eq 1 ] || usage
    token=$1
    valid_token "$token" || usage
    capture=${PROFILE_CAPTURE:-1}
    case "$capture" in 0|1) ;; *) echo "profile runner: PROFILE_CAPTURE must be 0 or 1" >&2; exit 2 ;; esac
    event_mask=${PROFILE_EVENT_MASK:-0xfef000000}
    valid_event_mask "$event_mask" || { echo "profile runner: invalid PROFILE_EVENT_MASK" >&2; exit 2; }
    sampling=${PROFILE_SAMPLING:-0}
    trace_enabled=${PROFILE_TRACE_ENABLED:-0}
    case "$sampling:$trace_enabled" in 0:0|0:1|1:0|1:1) ;; *) echo "profile runner: sampling and trace flags must be 0 or 1" >&2; exit 2 ;; esac
    timing_shift=${PROFILE_TIMING_SHIFT:-8}
    case "$timing_shift" in ''|*[!0-9]*) echo "profile runner: invalid PROFILE_TIMING_SHIFT" >&2; exit 2 ;; esac
    [ "$timing_shift" -le 16 ] || { echo "profile runner: invalid PROFILE_TIMING_SHIFT" >&2; exit 2; }
    workload=${PROFILE_WORKLOAD:-xtask}
    valid_token "$workload" || { echo "profile runner: invalid PROFILE_WORKLOAD" >&2; exit 2; }

    mkdir -p "$tool_mount" /mnt/proc /mnt/sys /mnt/dev /mnt/run /mnt/tmp
    if ! grep -q " $tool_mount " /proc/mounts 2>/dev/null; then
        echo "profile runner: auxiliary tool disk is not mounted" >&2
        exit 1
    fi

    mount -t proc proc /mnt/proc 2>/dev/null || true
    mount -t sysfs sysfs /mnt/sys 2>/dev/null || true
    mount -t devtmpfs devtmpfs /mnt/dev 2>/dev/null || true
    mkdir -p /mnt/dev/shm
    mount -t tmpfs tmpfs /mnt/dev/shm 2>/dev/null || true
    mount -t tmpfs tmpfs /mnt/run 2>/dev/null || true

    if [ "$capture" -eq 1 ] && [ ! -r "$tool_mount/profile-capture.sh" ]; then
        echo "profile runner: capture helper is unavailable" >&2
        exit 1
    fi
    if [ ! -x /mnt/bin/bash ] || [ ! -d /mnt/work/tgoskits ]; then
        echo "profile runner: glibc test root is incomplete" >&2
        exit 1
    fi

    if [ "$capture" -eq 1 ]; then
        cp "$tool_mount/profile-capture.sh" /tmp/profile-capture.sh || exit 1
        chmod 755 /tmp/profile-capture.sh || exit 1
    fi

    echo "@@PROFILE_MEMINFO_BEGIN phase=before"
    cat /proc/meminfo 2>/dev/null || true
    echo "@@PROFILE_MEMINFO_END phase=before"

    # Every run uses a fresh overlay. Keep this explicit deletion as a second
    # invariant, and fail instead of accidentally measuring an incremental run.
    if ! chroot /mnt /bin/sh -c \
        'rm -rf /work/tgoskits/target/debug && test ! -e /work/tgoskits/target/debug'; then
        echo "profile runner: unable to establish a cold target directory" >&2
        exit 1
    fi

    export PROFILE_EVENT_MASK=$event_mask
    export PROFILE_SAMPLING=$sampling
    export PROFILE_TRACE_ENABLED=$trace_enabled
    export PROFILE_TIMING_SHIFT=$timing_shift
    export PROFILE_KERNEL_IMAGE_ID=${PROFILE_KERNEL_IMAGE_ID:-candidate}
    export PROFILE_ROOTFS_IMAGE_ID=${PROFILE_ROOTFS_IMAGE_ID:-unknown}
    export PROFILE_WORKLOAD=$workload

    if [ "$capture" -eq 0 ]; then
        echo "@@PROFILE_CAPTURE status=unavailable reason=disabled"
    fi

    gate=/run/buildstorm-profile-gate-$token
    rm -f "/mnt$gate"
    setsid chroot /mnt /bin/bash -lc \
        'gate=$1; token=$2; while [ ! -e "$gate" ]; do sleep 0.01; done; export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin HOME=/root RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo RUSTUP_TOOLCHAIN=nightly-2026-05-28 CARGO_NET_OFFLINE=true; cd /work/tgoskits; echo "@@PROFILE_CARGO_EXEC token=$token"; exec cargo build -p tg-xtask' bash "$gate" "$token" &
    workload_pid=$!

    attempts=0
    identity=
    while [ "$attempts" -lt 200 ]; do
        identity=$(proc_identity "$workload_pid" 2>/dev/null || true)
        case "$identity" in
            "$workload_pid "*) break ;;
        esac
        attempts=$((attempts + 1))
        sleep 0.01
    done
    case "$identity" in
        "$workload_pid "*) ;;
        *)
            echo "profile runner: workload did not establish a unique process group" >&2
            kill -KILL "$workload_pid" 2>/dev/null || true
            wait "$workload_pid" 2>/dev/null || true
            exit 1
            ;;
    esac
    start_ticks=${identity#* }
    owner=/tmp/buildstorm-profile-owner-$token
    printf '%s %s %s\n' "$workload_pid" "$start_ticks" "$token" >"$owner"
    control=/tmp/buildstorm-profile-control-$token
    rm -f "$control"
    controller_pid=
    capture_controller "$token" "$owner" "$control" &
    controller_pid=$!
    echo "@@PROFILE_WORKLOAD case=$PROFILE_WORKLOAD pid=$workload_pid start_ticks=$start_ticks token=$token"

    set +e
    # The host owns the absolute deadline and invokes the identity-checked
    # stop subcommand. Thus this wait is bounded even on a wedged cargo.
    wait "$workload_pid"
    status=$?
    set -u
    echo "@@PROFILE_WORKLOAD_EXIT status=$status token=$token"
    if process_group_alive "$workload_pid"; then
        terminate_group "$workload_pid" || {
            echo "profile runner: descendant process group survived workload exit" >&2
            status=1
        }
    fi
    rm -f "$owner"

    if [ -n "$controller_pid" ]; then
        # The controller has its own explicit timeout and normally exits as
        # soon as removal of the owner file signals workload completion.
        wait "$controller_pid" || exit 1
    fi
    rm -f "$control" "/mnt$gate"
    echo "@@PROFILE_MEMINFO_BEGIN phase=after"
    cat /proc/meminfo 2>/dev/null || true
    echo "@@PROFILE_MEMINFO_END phase=after"
    echo "PROFILE_RUNNER_DONE status=$status token=$token"
    exit "$status"
}

[ "$#" -ge 1 ] || usage
command=$1
shift
case "$command" in
    run) run_profile "$@" ;;
    watch-stage|w) watch_stage "$@" ;;
    go|g) release_workload "$@" ;;
    arm|a) set_control start "$@" ;;
    resume|c) set_control resume "$@" ;;
    finish|z) set_control stop "$@" ;;
    stop) stop_run "$@" ;;
    stop-token|x) stop_token "$@" ;;
    *) usage ;;
esac
