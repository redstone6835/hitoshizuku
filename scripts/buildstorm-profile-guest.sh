#!/bin/sh
# Run one cold tg-xtask capture, or safely stop a capture started by this script.
set -u

tool_mount=${PROFILE_TOOL_MOUNT:-/tmp/buildstorm-profile-tools}
task_snapshot_expected_header='# mygo.task-snapshot.v1 pid ppid tgid pgrp state start_ticks comm'
profile_boot_mode=${PROFILE_BOOT_MODE:-auto}

case "$profile_boot_mode" in
    auto|mygo|linux) ;;
    *) echo "profile runner: PROFILE_BOOT_MODE must be auto, mygo, or linux" >&2; exit 2 ;;
esac

usage() {
    echo "usage: $0 plan | run <run-token> | watch-stage <run-token> aws-first-object | go <run-token> | arm <run-token> | resume <run-token> | finish <run-token> | ack-stop <run-token> | controller-status <run-token> | stop-token <run-token>" >&2
    exit 2
}

resolve_workload() {
    profile_arch=${PROFILE_ARCH:-$(uname -m 2>/dev/null || echo unknown)}
    profile_target_fs=${PROFILE_TARGET_FS:-extfs}
    case "$profile_arch" in
        riscv64) profile_target=riscv64gc-unknown-linux-musl ;;
        loongarch64) profile_target=loongarch64-unknown-linux-musl ;;
        *) echo "profile runner: PROFILE_ARCH must be riscv64 or loongarch64" >&2; return 2 ;;
    esac
    case "$profile_target_fs" in
        extfs|tmpfs) ;;
        *) echo "profile runner: PROFILE_TARGET_FS must be extfs or tmpfs" >&2; return 2 ;;
    esac
}

print_workload_plan() {
    resolve_workload || return
    cat <<EOF
schema=mygo.buildstorm-workload.v2
arch=$profile_arch
target=$profile_target
target_fs=$profile_target_fs
prebuild=cargo build -p tg-xtask
command=timeout 14400 cargo xtask arceos build -p arceos-helloworld --arch $profile_arch
EOF
}

prepare_target() {
    root_mount=${PROFILE_ROOT_MOUNT:-/mnt}
    mounts_file=${PROFILE_MOUNTS_FILE:-/proc/mounts}
    target_mount=$root_mount/work/tgoskits/target
    mkdir -p "$target_mount" || {
        echo "profile runner: unable to create BuildStorm target directory" >&2
        return 1
    }

    case "$profile_target_fs" in
        extfs)
            if awk -v target="$target_mount" '$2 == target { found = 1 } END { exit !found }' \
                "$mounts_file" 2>/dev/null; then
                echo "profile runner: formal BuildStorm target has an unexpected mount" >&2
                return 1
            fi
            workload_fs=$(awk -v root="$root_mount" \
                '$2 == root { print $3; found = 1; exit } END { exit !found }' \
                "$mounts_file" 2>/dev/null) || {
                echo "profile runner: workload root filesystem is unavailable" >&2
                return 1
            }
            case "$workload_fs" in
                ext2|ext3|ext4) ;;
                *) echo "profile runner: formal BuildStorm requires an ext filesystem" >&2; return 1 ;;
            esac
            rm -rf "$target_mount/$profile_target" || return 1
            [ ! -e "$target_mount/$profile_target" ] || {
                echo "profile runner: unable to remove the formal architecture target" >&2
                return 1
            }
            echo "@@PROFILE_TARGET_FS type=$workload_fs path=/work/tgoskits/target source=workload"
            ;;
        tmpfs)
            if awk -v target="$target_mount" \
                '$2 == target && $3 == "tmpfs" { found = 1 } END { exit !found }' \
                "$mounts_file" 2>/dev/null; then
                :
            elif ! mount -t tmpfs -o size=5G tmpfs "$target_mount"; then
                echo "profile runner: unable to mount BuildStorm target tmpfs" >&2
                return 1
            fi
            rm -rf "$target_mount/debug" || return 1
            echo "@@PROFILE_TARGET_FS type=tmpfs path=/work/tgoskits/target limit=5G"
            ;;
    esac
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

# The MyGO-specific task snapshot avoids /proc/PID/stat's VMA accounting path.
# Keep the stat parser as a Linux/old-kernel fallback for development images.
task_snapshot_available() {
    [ -r /proc/task-snapshot ] || return 1
    IFS= read -r task_snapshot_actual_header </proc/task-snapshot || return 1
    [ "$task_snapshot_actual_header" = "$task_snapshot_expected_header" ]
}

task_snapshot_records() {
    target=$1
    [ -r /proc/task-snapshot ] || return 2
    LC_ALL=C awk -v target="$target" -v expected="$task_snapshot_expected_header" '
        NR == 1 {
            if ($0 != expected) exit 2
            header_ok = 1
            next
        }
        $1 ~ /^[0-9]+$/ && $4 == target { print }
        END { if (!header_ok) exit 2 }
    ' \
        /proc/task-snapshot 2>/dev/null
}

# Linux does not expose MyGO's atomic task snapshot. One awk process reads all
# threads in pathname order; a disappearing entry makes that attempt fail and
# retry instead of accepting a partial snapshot.
linux_proc_task_snapshot_records() (
    target=$1
    LC_ALL=C awk -v target="$target" '
        {
            path = FILENAME
            sub(/^\/proc\//, "", path)
            path_count = split(path, path_field, "/")

            line = $0
            pid = line
            sub(/ .*/, "", pid)
            sub(/^[0-9]+ \(.*\) /, "", line)
            field_count = split(line, field, " ")

            if (path_count != 4 || path_field[2] != "task" ||
                path_field[4] != "stat" || field_count < 20 ||
                pid !~ /^[0-9]+$/ || path_field[1] !~ /^[0-9]+$/ ||
                path_field[3] !~ /^[0-9]+$/ || pid != path_field[3] ||
                field[1] !~ /^.$/ || field[2] !~ /^[0-9]+$/ ||
                field[3] !~ /^[0-9]+$/ || field[20] !~ /^[0-9]+$/) {
                bad = 1
                next
            }
            if (field[3] == target) {
                printf "%s %s %s %s %s %s linux-proc-stat\n", \
                    pid, field[2], path_field[1], field[3], field[1], field[20]
            }
        }
        END { if (bad) exit 1 }
    ' /proc/[0-9]*/task/[0-9]*/stat 2>/dev/null
)

process_group_snapshot_records() {
    snapshot_method=$1
    snapshot_target=$2
    case "$snapshot_method" in
        task-snapshot) task_snapshot_records "$snapshot_target" ;;
        linux-proc-stat-double) linux_proc_task_snapshot_records "$snapshot_target" ;;
        *) return 2 ;;
    esac
}

# /proc/PID/stat starts with "PID (comm)". Strip that prefix before selecting
# fields so a command name containing spaces cannot shift pgrp/starttime.
proc_identity() {
    pid=$1
    identity_method=$profile_boot_mode
    if [ "$identity_method" = auto ]; then
        if [ -e /proc/task-snapshot ]; then
            identity_method=mygo
        else
            identity_method=linux
        fi
    fi
    if [ "$identity_method" = mygo ]; then
        identity=$(LC_ALL=C awk -v target="$pid" -v expected="$task_snapshot_expected_header" '
            NR == 1 {
                if ($0 != expected) exit 2
                header_ok = 1
                next
            }
            $1 == target { print $4, $6; exit }
            END { if (!header_ok) exit 2 }
        ' /proc/task-snapshot 2>/dev/null) || return 1
        [ -n "$identity" ] || return 1
        printf '%s\n' "$identity"
        return 0
    fi
    stat=$(cat "/proc/$pid/stat" 2>/dev/null) || return 1
    rest=${stat##*) }
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

owned_process_group() {
    expected_pid=$1
    expected_start=$2
    if identity=$(proc_identity "$expected_pid" 2>/dev/null); then
        [ "$identity" = "$expected_pid $expected_start" ]
    else
        return 1
    fi
}

# Parse stat in one pass so timeout diagnostics do not depend on ps/procps.
process_group_diagnostic_records() (
    if task_snapshot_available; then
        LC_ALL=C awk -v target="$1" \
            '$1 ~ /^[0-9]+$/ && $4 == target { printf "%s|%s|%s|%s|%s\n", $1, $2, $4, $5, $7 }' \
            /proc/task-snapshot 2>/dev/null
        exit 0
    fi
    LC_ALL=C awk -v target="$1" '
        {
            pid = $1
            line = $0
            sub(/^[0-9]+ \(.*\) /, "", line)
            split(line, field, " ")
            if (field[3] != target) next

            comm = $0
            sub(/^[0-9]+ \(/, "", comm)
            sub(/\) [^)]*$/, "", comm)
            gsub(/[[:space:]|=]/, "_", comm)
            if (comm == "") comm = "unknown"
            printf "%s|%s|%s|%s|%s\n", pid, field[2], field[3], field[1], comm
        }
    ' /proc/[0-9]*/stat 2>/dev/null
)

report_group_stop_timeout() {
    group_diag_pgrp=$1
    group_diag_phase=$2
    group_diag_attempts=$3
    group_diag_started=$4
    group_diag_reference_taken=$5
    group_diag_reference=$6
    group_diag_now=unavailable
    if [ -r /proc/uptime ]; then
        read -r group_diag_now group_diag_unused </proc/uptime || group_diag_now=unavailable
    fi
    group_diag_elapsed_ms=unavailable
    case "$group_diag_started:$group_diag_now" in
        *[!0-9.:]*) ;;
        *)
            group_diag_elapsed_ms=$(LC_ALL=C awk \
                -v started="$group_diag_started" -v now="$group_diag_now" \
                'BEGIN { printf "%.0f", (now - started) * 1000 }' </dev/null 2>/dev/null) ||
                group_diag_elapsed_ms=unavailable
            ;;
    esac
    group_diag_budget_ms=$((group_diag_attempts * 10))
    echo "@@PROFILE_GROUP_STOP_TIMEOUT phase=$group_diag_phase pgid=$group_diag_pgrp attempts=$group_diag_attempts expected_ms=$group_diag_budget_ms elapsed_ms=$group_diag_elapsed_ms uptime_start=$group_diag_started uptime_now=$group_diag_now" >&2

    group_diag_members=$(process_group_diagnostic_records "$group_diag_pgrp")
    group_diag_count=0
    group_diag_non_stopped=0
    group_diag_d_state=0
    group_diag_late_forks=0
    if [ -n "$group_diag_members" ]; then
        while IFS='|' read -r group_diag_pid group_diag_ppid \
            group_diag_member_pgrp group_diag_state group_diag_comm; do
            group_diag_wchan=unavailable
            if [ -r "/proc/$group_diag_pid/wchan" ]; then
                group_diag_wchan=$(LC_ALL=C awk '
                    NR == 1 {
                        gsub(/[[:space:]|=]/, "_", $0)
                        print
                        exit
                    }
                ' "/proc/$group_diag_pid/wchan" 2>/dev/null) || group_diag_wchan=unavailable
                [ -n "$group_diag_wchan" ] || group_diag_wchan=unavailable
            fi

            group_diag_seen_at_probe=unknown
            if [ "$group_diag_reference_taken" -eq 1 ]; then
                group_diag_seen_at_probe=0
                case "
$group_diag_reference
" in
                    *"
$group_diag_pid|"*) group_diag_seen_at_probe=1 ;;
                esac
                if [ "$group_diag_seen_at_probe" -eq 0 ]; then
                    group_diag_late_forks=$((group_diag_late_forks + 1))
                fi
            fi
            case "$group_diag_state" in
                D)
                    group_diag_d_state=$((group_diag_d_state + 1))
                    group_diag_non_stopped=$((group_diag_non_stopped + 1))
                    ;;
                T|t|Z) ;;
                *) group_diag_non_stopped=$((group_diag_non_stopped + 1)) ;;
            esac
            group_diag_count=$((group_diag_count + 1))
            echo "@@PROFILE_GROUP_MEMBER phase=$group_diag_phase pid=$group_diag_pid ppid=$group_diag_ppid pgid=$group_diag_member_pgrp state=$group_diag_state wchan=$group_diag_wchan comm=$group_diag_comm seen_at_probe=$group_diag_seen_at_probe" >&2
        done <<EOF
$group_diag_members
EOF
    fi
    echo "@@PROFILE_GROUP_STOP_SUMMARY phase=$group_diag_phase pgid=$group_diag_pgrp members=$group_diag_count non_stopped_members=$group_diag_non_stopped d_state_members=$group_diag_d_state late_fork_candidates=$group_diag_late_forks probe_attempt=40" >&2
}

wait_group_stopped() {
    pgrp=$1
    group_stop_phase=${2:-unspecified}
    group_stop_expected_start=${3:-}
    group_stop_phase_file=${4:-}
    group_stop_attempts=0
    case "$group_stop_phase" in
        window-start) group_stop_limit=1 ;;
        *) group_stop_limit=25 ;;
    esac
    group_stop_verified=0
    group_stop_empty=0
    case "$profile_boot_mode" in
        mygo) group_stop_method=task-snapshot ;;
        linux) group_stop_method=linux-proc-stat-double ;;
        auto)
            if [ -e /proc/task-snapshot ]; then
                group_stop_method=task-snapshot
            else
                group_stop_method=linux-proc-stat-double
            fi
            ;;
    esac
    while [ "$group_stop_attempts" -lt "$group_stop_limit" ]; do
        # Cargo/rustc can fork while an end-of-window stop is propagating.
        # Repeated group signals give every such child a bounded chance to
        # observe SIGSTOP.  Before the start gate opens the group is stable,
        # so one signal avoids accumulating redundant stops in the guest.
        [ -z "$group_stop_phase_file" ] ||
            printf 'phase=%s step=signal-begin attempt=%s\n' \
                "$group_stop_phase" "$group_stop_attempts" >"$group_stop_phase_file"
        if ! kill -STOP "-$pgrp" 2>/dev/null; then
            [ -z "$group_stop_phase_file" ] ||
                printf 'phase=%s step=group-empty attempt=%s\n' \
                    "$group_stop_phase" "$group_stop_attempts" >"$group_stop_phase_file"
            group_stop_empty=1
            group_stop_verified=1
            return 0
        fi
        group_stop_attempts=$((group_stop_attempts + 1))
        [ -z "$group_stop_phase_file" ] ||
            printf 'phase=%s step=signal-sent attempt=%s\n' \
                "$group_stop_phase" "$group_stop_attempts" >"$group_stop_phase_file"
        if [ "$group_stop_phase" = window-start ]; then
            # The start marker must not race a SIGSTOP that has been accepted
            # by kill(2) but has not reached the sleeping gate task yet.
            sleep 0.25
        else
            sleep 0.01
        fi
        [ -z "$group_stop_phase_file" ] ||
            printf 'phase=%s step=snapshot-begin attempt=%s method=%s\n' \
                "$group_stop_phase" "$group_stop_attempts" "$group_stop_method" \
                >"$group_stop_phase_file"
        if group_first_snapshot=$(process_group_snapshot_records \
            "$group_stop_method" "$pgrp" 2>/dev/null); then
            [ -z "$group_stop_phase_file" ] ||
                printf 'phase=%s step=snapshot-read attempt=%s bytes=%s method=%s\n' \
                    "$group_stop_phase" "$group_stop_attempts" \
                    "${#group_first_snapshot}" "$group_stop_method" \
                    >"$group_stop_phase_file"
            group_snapshot_stopped=0
            if [ -z "$group_first_snapshot" ] && [ "$group_stop_phase" != window-start ]; then
                sleep 0.01
                [ -z "$group_stop_phase_file" ] ||
                    printf 'phase=%s step=confirm-begin attempt=%s\n' \
                        "$group_stop_phase" "$group_stop_attempts" >"$group_stop_phase_file"
                group_second_snapshot=$(process_group_snapshot_records \
                    "$group_stop_method" "$pgrp" 2>/dev/null) || continue
                [ -z "$group_stop_phase_file" ] ||
                    printf 'phase=%s step=confirm-read attempt=%s bytes=%s method=%s\n' \
                        "$group_stop_phase" "$group_stop_attempts" \
                        "${#group_second_snapshot}" "$group_stop_method" \
                        >"$group_stop_phase_file"
                if [ -z "$group_second_snapshot" ]; then
                    group_stop_empty=1
                    group_stop_verified=1
                    return 0
                fi
            elif [ -n "$group_first_snapshot" ]; then
                if printf '%s\n' "$group_first_snapshot" | LC_ALL=C awk \
                    -v leader="$pgrp" -v expected_start="$group_stop_expected_start" \
                    -v method="$group_stop_method" '
                    $1 == leader {
                        leader_seen = 1
                        if (expected_start != "" && $6 != expected_start) bad = 1
                    }
                    method == "linux-proc-stat-double" && $5 !~ /^[TtZX]$/ { bad = 1 }
                    method != "linux-proc-stat-double" && $1 == $3 && $5 !~ /^[TtZX]$/ { bad = 1 }
                    method != "linux-proc-stat-double" && $1 != $3 && $5 !~ /^[STtZX]$/ { bad = 1 }
                    END { exit !(NR > 0 && leader_seen && !bad) }
                '; then
                    sleep 0.01
                    [ -z "$group_stop_phase_file" ] ||
                        printf 'phase=%s step=confirm-begin attempt=%s\n' \
                            "$group_stop_phase" "$group_stop_attempts" >"$group_stop_phase_file"
                    group_second_snapshot=$(process_group_snapshot_records \
                        "$group_stop_method" "$pgrp" 2>/dev/null) || continue
                    [ -z "$group_stop_phase_file" ] ||
                        printf 'phase=%s step=confirm-read attempt=%s bytes=%s method=%s\n' \
                            "$group_stop_phase" "$group_stop_attempts" \
                            "${#group_second_snapshot}" "$group_stop_method" \
                            >"$group_stop_phase_file"
                    [ "$group_first_snapshot" = "$group_second_snapshot" ] &&
                        group_snapshot_stopped=1
                fi
            fi
            if [ "$group_snapshot_stopped" -eq 1 ]; then
                [ -z "$group_stop_phase_file" ] ||
                    printf 'phase=%s step=verified attempt=%s\n' \
                        "$group_stop_phase" "$group_stop_attempts" >"$group_stop_phase_file"
                group_stop_verified=1
                return 0
            fi
        fi
    done
    group_stop_verified=0
    group_diag_reference=$(process_group_diagnostic_records "$pgrp" 2>/dev/null || true)
    report_group_stop_timeout "$pgrp" "$group_stop_phase" \
        "$group_stop_attempts" unavailable 1 "$group_diag_reference"
    return 1
}

resume_group() {
    pgrp=$1
    group_resume_attempts=1
    kill -CONT "-$pgrp" 2>/dev/null
}

terminate_group() {
    pgrp=$1
    # SIGTERM remains pending for a SIGSTOP-ed task.  Continue the complete
    # group so normal TERM handling can run before escalating to SIGKILL.
    kill -TERM "-$pgrp" 2>/dev/null || true
    kill -CONT "-$pgrp" 2>/dev/null || true
    group_term_attempts=0
    while [ "$group_term_attempts" -lt 10 ]; do
        group_term_attempts=$((group_term_attempts + 1))
        sleep 0.01
    done

    # A child can fork between TERM delivery and its own exit.  Sweep the
    # process group repeatedly, keeping cleanup independent of procfs state
    # rendering and bounded even while the leader is waiting to be reaped.
    group_kill_attempts=0
    while [ "$group_kill_attempts" -lt 25 ]; do
        kill -KILL "-$pgrp" 2>/dev/null || break
        group_kill_attempts=$((group_kill_attempts + 1))
        sleep 0.01
    done

    # The group leader can remain as a zombie until the parent runner reaps
    # it.  Do not scan /proc here to distinguish that state: MyGO procfs can
    # block behind teardown of a just-killed task.  This function promises a
    # bounded signal sweep; the parent performs one final sweep after wait.
    return 0
}

read_owner_record() {
    token=$1
    owner=/tmp/buildstorm-profile-owner-$token
    owner_record=$(cat "$owner" 2>/dev/null) || return 1
    set -- $owner_record
    [ "$#" -eq 3 ] || return 1
    owner_pid=$1
    owner_start=$2
    owner_token=$3
    [ "$owner_token" = "$token" ] || return 1
}

set_control() {
    [ "$#" -eq 2 ] || usage
    state=$1
    token=$2
    valid_token "$token" || usage
    if ! read_owner_record "$token"; then
        echo "PROFILE_WINDOW_SKIPPED state=$state reason=workload-ended token=$token"
        exit 0
    fi
    control=/mnt/run/buildstorm-profile-control-$token
    [ -p "$control" ] || {
        echo "PROFILE_WINDOW_SKIPPED state=$state reason=missing-control token=$token"
        exit 0
    }
    echo "PROFILE_WINDOW_REQUESTED state=$state token=$token"
    printf '%s\n' "$state" >"$control"
}

ack_stop() {
    [ "$#" -eq 1 ] || usage
    token=$1
    valid_token "$token" || usage
    control=/mnt/run/buildstorm-profile-control-$token
    [ -p "$control" ] || {
        echo "PROFILE_WINDOW_SKIPPED state=snapshot reason=missing-control token=$token"
        exit 0
    }
    echo "PROFILE_WINDOW_REQUESTED state=snapshot token=$token"
    printf 'snapshot\n' >"$control"
}

controller_status() {
    [ "$#" -eq 1 ] || usage
    token=$1
    valid_token "$token" || usage
    controller_status_value=unavailable
    IFS= read -r controller_status_value \
        <"/tmp/buildstorm-profile-controller-phase-$token" 2>/dev/null || true
    echo "PROFILE_CONTROLLER_STATUS token=$token $controller_status_value"
}

release_workload() {
    [ "$#" -eq 1 ] || usage
    token=$1
    valid_token "$token" || usage
    if ! read_owner_record "$token"; then
        echo "PROFILE_GATE_SKIPPED reason=workload-ended token=$token"
        exit 0
    fi
    gate=/mnt/run/buildstorm-profile-gate-$token
    gate_released=$gate.released
    [ ! -e "$gate_released" ] || {
        echo "PROFILE_GATE_SKIPPED reason=already-open token=$token"
        exit 0
    }
    printf "go\n" >"$gate" || {
        echo "profile runner: unable to release workload gate" >&2
        exit 1
    }
    : >"$gate_released" || exit 1
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
    if ! read_owner_record "$token"; then
        echo "@@PROFILE_STAGE_SKIPPED name=$stage_name reason=workload-ended token=$token"
        return 0
    fi
    echo "@@PROFILE_STAGE_WATCH_READY name=$stage_name token=$token"
    while read_owner_record "$token"; do
        for object in \
            "$stage_root"/work/tgoskits/target/*/debug/build/aws-lc-sys-*/out/*.o \
            "$stage_root"/work/tgoskits/target/debug/build/aws-lc-sys-*/out/*.o
        do
            [ -f "$object" ] || continue
            relative=${object#"$stage_root"}
            echo "@@PROFILE_STAGE name=$stage_name token=$token path=$relative"
            return 0
        done
        sleep 0.05
    done
    echo "@@PROFILE_STAGE_SKIPPED name=$stage_name reason=workload-ended token=$token"
}

expect_controller_state() {
    controller_expected_state=$1
    IFS= read -r controller_state <&8 || {
        echo "profile runner: controller channel closed while waiting for $controller_expected_state" >&2
        return 1
    }
    printf 'phase=controller step=state-read expected=%s actual=%s\n' \
        "$controller_expected_state" "$controller_state" >"$controller_phase_file"
    case "$controller_state" in
        "$controller_expected_state") return 0 ;;
        stop) return 3 ;;
        abort)
            echo "PROFILE_CAPTURE_SKIPPED reason=workload-ended state=$controller_expected_state token=$token"
            return 2
            ;;
        *)
            echo "profile runner: unexpected controller state $controller_state while waiting for $controller_expected_state" >&2
            return 1
            ;;
    esac
}

publish_capture_window() {
    ended=$1
    quiescence_verified=$2
    quiescence_method=$3
    capture_prepared=$4
    if [ "$capture_prepared" -eq 1 ] && [ "$PROFILE_CAPTURE" -eq 1 ]; then
        printf 'freeze\n' >"${PROFILE_CONTROL:-/sys/kernel/profile_control}" || return 1
    fi
    echo "@@PROFILE_WINDOW_FROZEN ended=$ended token=$token quiescence_verified=$quiescence_verified quiescence_method=$quiescence_method"

    while :; do
        expect_controller_state snapshot
        controller_wait_status=$?
        case "$controller_wait_status" in
            0) break ;;
            2) return 0 ;;
            3) continue ;;
            *) return 1 ;;
        esac
    done
    if [ "$capture_prepared" -eq 1 ] && [ "$PROFILE_CAPTURE" -eq 1 ]; then
        PROFILE_ALREADY_FROZEN=1 /bin/sh /tmp/profile-capture.sh stop "$PROFILE_WORKLOAD" || return 1
    fi
    echo "@@PROFILE_WINDOW_STOPPED ended=$ended token=$token"
}

capture_controller() {
    token=$1
    owner=$2
    control=$3
    workload_pid=$4
    workload_start=$5
    controller_gate=/mnt/run/buildstorm-profile-gate-$token
    controller_gate_released=$controller_gate.released
    controller_phase_file=/tmp/buildstorm-profile-controller-phase-$token
    exec 8<>"$control" || {
        echo "profile runner: unable to open controller FIFO" >&2
        return 1
    }
    controller_expected="$workload_pid $workload_start $token"
    controller_actual=$(cat "$owner" 2>/dev/null) || {
        echo "PROFILE_CAPTURE_SKIPPED reason=workload-ended token=$token"
        return 0
    }
    [ "$controller_actual" = "$controller_expected" ] || {
        echo "PROFILE_CAPTURE_SKIPPED reason=identity-mismatch token=$token"
        return 0
    }
    printf 'phase=controller step=ready\n' >"$controller_phase_file"
    echo "@@PROFILE_CONTROLLER_READY pid=$workload_pid start_ticks=$workload_start token=$token"

    expect_controller_state start
    controller_wait_status=$?
    case "$controller_wait_status" in
        0) ;;
        2) return 0 ;;
        3) publish_capture_window 1 1 workload-ended 0; return $? ;;
        *) return 1 ;;
    esac
    controller_actual=$(cat "$owner" 2>/dev/null) || {
        echo "PROFILE_CAPTURE_SKIPPED reason=workload-ended token=$token"
        return 0
    }
    if [ "$controller_actual" != "$controller_expected" ]; then
        echo "PROFILE_CAPTURE_SKIPPED reason=identity-mismatch token=$token"
        return 0
    fi
    controller_gate_closed=0
    if [ ! -e "$controller_gate_released" ]; then
        controller_gate_closed=1
    fi
    # workload/0 尚未越过 cooperative gate，不需要依赖进程组 STOP/CONT。
    # 其它 anchor 已经放行 Cargo，才使用信号建立窗口边界。
    if [ "$controller_gate_closed" -eq 0 ]; then
        wait_group_stopped "$workload_pid" window-start "$workload_start" \
            "$controller_phase_file" || {
            echo "profile runner: workload group did not stop at window start" >&2
            return 1
        }
        [ "$group_stop_empty" -eq 0 ] || {
            publish_capture_window 1 1 workload-ended 0
            return $?
        }
    fi
    if [ "$PROFILE_CAPTURE" -eq 1 ]; then
        PROFILE_LEAVE_FROZEN=1 /bin/sh /tmp/profile-capture.sh start "$PROFILE_WORKLOAD" || return 1
    fi
    echo "@@PROFILE_WINDOW_READY token=$token"

    expect_controller_state resume
    controller_wait_status=$?
    case "$controller_wait_status" in
        0) ;;
        2) return 0 ;;
        3) publish_capture_window 1 1 workload-ended 1; return $? ;;
        *) return 1 ;;
    esac
    [ -r "$owner" ] || {
        publish_capture_window 1 1 workload-ended 1
        return $?
    }
    if [ "$controller_gate_closed" -eq 1 ] && [ -e "$controller_gate_released" ]; then
        echo "profile runner: cooperative start gate opened before window resume" >&2
        return 1
    fi
    if [ "$PROFILE_CAPTURE" -eq 1 ]; then
        printf 'resume\n' >"${PROFILE_CONTROL:-/sys/kernel/profile_control}" || return 1
    fi
    group_resume_attempts=0
    group_resume_mode=gate
    if [ "$controller_gate_closed" -eq 0 ]; then
        group_resume_mode=signal
        resume_group "$workload_pid" || {
            terminate_group "$workload_pid"
            echo "profile runner: workload group did not resume at window start" >&2
            return 1
        }
    fi
    echo "@@PROFILE_GROUP_RESUMED attempts=$group_resume_attempts mode=$group_resume_mode gate_closed=$controller_gate_closed token=$token"
    echo "@@PROFILE_WINDOW_STARTED token=$token"
    if [ "$controller_gate_closed" -eq 1 ]; then
        printf "go\n" >"$controller_gate" || return 1
        : >"$controller_gate_released" || return 1
    fi

    expect_controller_state stop
    controller_wait_status=$?
    case "$controller_wait_status" in 0) ;; 2) return 0 ;; *) return 1 ;; esac
    ended=0
    quiescence_verified=1
    quiescence_method=workload-ended
    if [ -r "$owner" ]; then
        wait_group_stopped "$workload_pid" window-end "$workload_start" \
            "$controller_phase_file" || {
            echo "profile runner: workload group did not stop at window end" >&2
            return 1
        }
        quiescence_verified=$group_stop_verified
        quiescence_method=$group_stop_method
        [ "$group_stop_empty" -eq 0 ] || ended=1
    else
        ended=1
    fi
    publish_capture_window "$ended" "$quiescence_verified" "$quiescence_method" 1 || return 1
    if [ "$ended" -eq 0 ]; then
        # 窗口快照已经完成；只终止已校验身份的 leader，让 host 的 QEMU
        # teardown 回收仍冻结的后代，避免再次遍历正在退出的进程组。
        kill -KILL "$workload_pid" 2>/dev/null || true
        echo "PROFILE_STOP_SENT token=$token pid=$workload_pid"
    fi
    exec 8>&-
    return 0
}

finish_natural_capture() {
    natural_owner=$1
    natural_controller_pid=$2
    printf 'stop\n' >&7 || {
        echo "profile runner: unable to notify controller of workload completion" >&2
        return 1
    }
    if [ -n "$natural_controller_pid" ]; then
        wait "$natural_controller_pid" || return 1
    fi
    rm -f "$natural_owner"
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

    terminate_group "$pid"
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
    event_mask_high=${PROFILE_EVENT_MASK_HIGH:-0x0}
    valid_event_mask "$event_mask_high" || { echo "profile runner: invalid PROFILE_EVENT_MASK_HIGH" >&2; exit 2; }
    sampling=${PROFILE_SAMPLING:-0}
    trace_enabled=${PROFILE_TRACE_ENABLED:-0}
    case "$sampling:$trace_enabled" in 0:0|0:1|1:0|1:1) ;; *) echo "profile runner: sampling and trace flags must be 0 or 1" >&2; exit 2 ;; esac
    timing_shift=${PROFILE_TIMING_SHIFT:-8}
    case "$timing_shift" in ''|*[!0-9]*) echo "profile runner: invalid PROFILE_TIMING_SHIFT" >&2; exit 2 ;; esac
    [ "$timing_shift" -le 16 ] || { echo "profile runner: invalid PROFILE_TIMING_SHIFT" >&2; exit 2; }
    workload=${PROFILE_WORKLOAD:-xtask}
    valid_token "$workload" || { echo "profile runner: invalid PROFILE_WORKLOAD" >&2; exit 2; }
    resolve_workload || exit $?
    actual_arch=$(uname -m 2>/dev/null || echo unknown)
    [ "$actual_arch" = "$profile_arch" ] || {
        echo "profile runner: configured architecture does not match the guest" >&2
        exit 1
    }

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

    prepare_target || exit 1

    if [ "$capture" -eq 1 ]; then
        cp "$tool_mount/profile-capture.sh" /tmp/profile-capture.sh || exit 1
        chmod 755 /tmp/profile-capture.sh || exit 1
    fi

    echo "@@PROFILE_MEMINFO_BEGIN phase=before"
    cat /proc/meminfo 2>/dev/null || true
    echo "@@PROFILE_MEMINFO_END phase=before"

    # tg-xtask 预编不属于正式计分窗口，但必须和官方脚本一样在计时构建前完成。
    echo "@@PROFILE_PREBUILD_BEGIN command=tg-xtask"
    if ! chroot /mnt /bin/bash -lc \
        'export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin HOME=/root RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo RUSTUP_TOOLCHAIN=nightly-2026-05-28 CARGO_NET_OFFLINE=true; cd /work/tgoskits; cargo build -p tg-xtask'; then
        echo "profile runner: tg-xtask prebuild failed" >&2
        exit 1
    fi
    echo "@@PROFILE_PREBUILD_END command=tg-xtask status=0"

    export PROFILE_EVENT_MASK=$event_mask
    export PROFILE_EVENT_MASK_HIGH=$event_mask_high
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
    gate_released=$gate.released
    rm -f "/mnt$gate" "/mnt$gate_released"
    mkfifo "/mnt$gate" || {
        echo "profile runner: unable to create workload gate FIFO" >&2
        exit 1
    }
    [ -p "/mnt$gate" ] || {
        echo "profile runner: workload gate is not a FIFO" >&2
        exit 1
    }
    # cargo 的 `Compiling N/446` progress 走 **stderr**。此前这里没有做任何
    # 重定向，于是串口日志里一行 Compiling 都没有——历史上所有 run 的 cargo
    # 里程碑全是 N/A，根因就是这个，而不是之前推测的"ANSI 转义导致正则不匹配"。
    #
    # 仅靠 `2>&1` 继承还不够：`setsid` 会让子进程脱离控制终端，继承来的
    # fd 未必仍指向串口。这里显式把 stdout/stderr 绑到 /dev/console，
    # 保证 progress 一定落到串口日志上；同时不引入任何转发进程——转发进程
    # 会被窗口起止的 SIGSTOP 组停止一起冻住，反而让 cargo 阻塞在管道上。
    setsid chroot /mnt /bin/bash -lc \
        'gate=$1; token=$2; arch=$3; exec 9<>"$gate" || exit 1; echo "@@PROFILE_GATE_READY token=$token"; IFS= read -r gate_word <&9; [ "$gate_word" = go ] || exit 1; exec 9>&-; export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin HOME=/root RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo RUSTUP_TOOLCHAIN=nightly-2026-05-28 CARGO_NET_OFFLINE=true; cd /work/tgoskits; if [ -w /dev/console ]; then exec >/dev/console 2>&1; echo "@@PROFILE_BUILD_SINK sink=console"; else echo "@@PROFILE_BUILD_SINK sink=inherited"; exec 2>&1; fi; echo "@@PROFILE_CARGO_EXEC token=$token"; exec timeout 14400 cargo xtask arceos build -p arceos-helloworld --arch "$arch"' bash "$gate" "$token" "$profile_arch" &
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
    control=/mnt/run/buildstorm-profile-control-$token
    rm -f "$control"
    mkfifo "$control" || {
        echo "profile runner: unable to create controller FIFO" >&2
        exit 1
    }
    [ -p "$control" ] || {
        echo "profile runner: controller channel is not a FIFO" >&2
        exit 1
    }
    exec 7<>"$control" || {
        echo "profile runner: unable to hold controller FIFO" >&2
        exit 1
    }
    controller_pid=
    capture_controller "$token" "$owner" "$control" "$workload_pid" "$start_ticks" &
    controller_pid=$!
    echo "@@PROFILE_WORKLOAD case=$PROFILE_WORKLOAD pid=$workload_pid start_ticks=$start_ticks token=$token"

    set +e
    # The host owns the absolute deadline and invokes the identity-checked
    # stop subcommand. Thus this wait is bounded even on a wedged cargo.
    wait "$workload_pid"
    status=$?
    set -u
    echo "@@PROFILE_WORKLOAD_EXIT status=$status token=$token"
    finish_natural_capture "$owner" "$controller_pid" || exit 1
    exec 7>&-
    rm -f "$control" "/mnt$gate" "/mnt$gate_released"
    rm -f "/tmp/buildstorm-profile-controller-phase-$token"
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
    plan) [ "$#" -eq 0 ] || usage; print_workload_plan ;;
    run) run_profile "$@" ;;
    watch-stage|w) watch_stage "$@" ;;
    go|g) release_workload "$@" ;;
    arm|a) set_control start "$@" ;;
    resume|c) set_control resume "$@" ;;
    finish|z) set_control stop "$@" ;;
    ack-stop|k) ack_stop "$@" ;;
    controller-status|d) controller_status "$@" ;;
    stop) stop_run "$@" ;;
    stop-token|x) stop_token "$@" ;;
    *) usage ;;
esac
