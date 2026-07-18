#!/bin/sh

# LoongArch64 LTP 分片执行器。测试命令只来自镜像内的官方 runtest 文件，
# 每条命令仍交给 runltp/ltp-pan 执行，本脚本只负责隔离、超时和结构化记录。

PATH=/sbin:/bin:/usr/sbin:/usr/bin
export PATH

LTP_MOUNT=/mnt
LTPROOT="$LTP_MOUNT/glibc/ltp"
GLIBC_LIB="$LTP_MOUNT/glibc/lib"
WORK_MOUNT=/ltp-work
WORK_ROOT="$WORK_MOUNT/run"
TEST_DEV=/dev/vd2
BIG_DEV=/dev/vd3
SKIP_FILE=/etc/ltp-skip.tsv

cmdline_value() {
    key="$1"
    fallback="$2"

    if [ -r /sys/kernel/cmdline ]; then
        for arg in $(cat /sys/kernel/cmdline 2>/dev/null); do
            case "$arg" in
                "$key"=*)
                    printf '%s\n' "${arg#*=}"
                    return 0
                    ;;
            esac
        done
    fi
    printf '%s\n' "$fallback"
}

valid_uint() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

marker() {
    event="$1"
    shift
    printf '@@LTP\t%s' "$event"
    for field in "$@"; do
        printf '\t%s' "$field"
    done
    printf '\n'
}

shutdown_runner() {
    sync 2>/dev/null || true
    poweroff -f >/dev/null 2>&1 || poweroff >/dev/null 2>&1 || \
        reboot -f >/dev/null 2>&1 || true
    sleep 2
    exit "${1:-0}"
}

lookup_skip() {
    scenario="$1"
    tag="$2"

    [ -r "$SKIP_FILE" ] || return 1
    awk -F '\t' -v scenario="$scenario" -v tag="$tag" '
        /^[[:space:]]*#/ || NF < 3 { next }
        $1 == tag || $1 == "@scenario:" scenario {
            print $2 "|" $3
            exit
        }
    ' "$SKIP_FILE"
}

prepare_dynamic_linker() {
    [ -d "$GLIBC_LIB" ] || return 1

    mkdir -p /lib
    if [ ! -e /lib64 ]; then
        ln -s "$GLIBC_LIB" /lib64 2>/dev/null || true
    fi
    for loader in "$GLIBC_LIB"/ld-*.so*; do
        [ -f "$loader" ] || continue
        ln -sf "$loader" "/lib/${loader##*/}" 2>/dev/null || true
    done

    LD_LIBRARY_PATH="$GLIBC_LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export LD_LIBRARY_PATH
    return 0
}

prepare_workdir() {
    mkdir -p "$WORK_MOUNT"
    if ! mount -t ext4 /dev/vd1 "$WORK_MOUNT" 2>/dev/null; then
        marker fatal "reason=work-device-mount-failed" "device=/dev/vd1"
        return 1
    fi
    rm -rf "$WORK_ROOT"
    mkdir -p "$WORK_ROOT/cases" "$WORK_ROOT/tmp" || return 1
    chmod 1777 "$WORK_ROOT/tmp" 2>/dev/null || true
    return 0
}

print_artifact() {
    label="$1"
    path="$2"

    [ -s "$path" ] || return 0
    printf '\n[ltp-artifact] ===== %s =====\n' "$label"
    cat "$path"
    printf '[ltp-artifact] ===== end %s =====\n' "$label"
}

run_case() {
    index="$1"
    tag="$2"
    command_line="$3"
    safe_tag="$(printf '%s' "$tag" | tr -c 'A-Za-z0-9_.-' '_')"
    case_dir="$WORK_ROOT/cases/$(printf '%05d_%s' "$index" "$safe_tag")"
    case_tmp="$case_dir/tmp"
    current_name=".mygo-${RUN_ID}-${SCENARIO}-$$"
    current_file="$LTPROOT/runtest/$current_name"
    console_log="$case_dir/console.log"
    result_log="$case_dir/result.log"
    output_log="$case_dir/output.log"
    failed_log="$case_dir/failed.log"
    tconf_log="$case_dir/tconf.log"

    skip="$(lookup_skip "$SCENARIO" "$tag" 2>/dev/null || true)"
    if [ -n "$skip" ]; then
        category="${skip%%|*}"
        reason="${skip#*|}"
        marker case_start "group=$GROUP" "scenario=$SCENARIO" "index=$index" "tag=$tag"
        marker case_skip "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$tag" "category=$category" "reason=$reason"
        marker case_end "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$tag" "result=skip" "exit=0"
        return 0
    fi

    rm -rf "$case_dir"
    mkdir -p "$case_tmp" || return 2
    printf '%s\n' "$command_line" >"$current_file" || return 2

    marker case_start "group=$GROUP" "scenario=$SCENARIO" "index=$index" "tag=$tag"
    printf '[ltp] command: %s\n' "$command_line"

    setsid "$LTPROOT/runltp" \
        -f "$current_name" \
        -d "$case_tmp" \
        -b "$TEST_DEV" -B ext2 \
        -z "$BIG_DEV" -Z ext2 \
        -l "$result_log" \
        -o "$output_log" \
        -C "$failed_log" \
        -T "$tconf_log" \
        -q -Q >"$console_log" 2>&1 &
    child=$!
    elapsed=0
    timed_out=0

    while kill -0 "$child" 2>/dev/null; do
        if [ "$elapsed" -ge "$CASE_TIMEOUT" ]; then
            timed_out=1
            break
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    if [ "$timed_out" -eq 1 ]; then
        kill -TERM "-$child" 2>/dev/null || kill -TERM "$child" 2>/dev/null || true
        grace=0
        while kill -0 "$child" 2>/dev/null && [ "$grace" -lt "$KILL_GRACE" ]; do
            sleep 1
            grace=$((grace + 1))
        done
        kill -KILL "-$child" 2>/dev/null || kill -KILL "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
        ret=124
    else
        wait "$child"
        ret=$?
        kill -KILL "-$child" 2>/dev/null || true
    fi

    rm -f "$current_file"
    print_artifact console "$console_log"
    print_artifact output "$output_log"
    print_artifact result "$result_log"
    print_artifact failed "$failed_log"
    print_artifact tconf "$tconf_log"

    if [ "$timed_out" -eq 1 ]; then
        marker case_end "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$tag" "result=timeout" "exit=$ret" "elapsed=$elapsed"
        marker shard_abort "group=$GROUP" "scenario=$SCENARIO" "next=$((index + 1))" \
            "reason=case-timeout"
        shutdown_runner 124
    fi

    marker case_end "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
        "tag=$tag" "result=run" "exit=$ret" "elapsed=$elapsed"
    return 0
}

GROUP="$(cmdline_value ltp_group default)"
SCENARIO="$(cmdline_value ltp_scenario '')"
START="$(cmdline_value ltp_start 0)"
COUNT="$(cmdline_value ltp_count 50)"
ONLY="$(cmdline_value ltp_only '')"
CASE_TIMEOUT="$(cmdline_value ltp_case_timeout 300)"
KILL_GRACE="$(cmdline_value ltp_kill_grace 5)"
TIMEOUT_MUL="$(cmdline_value ltp_timeout_mul 4)"
RUN_ID="$(cmdline_value ltp_run_id manual)"

valid_uint "$START" || START=0
valid_uint "$COUNT" || COUNT=50
valid_uint "$CASE_TIMEOUT" || CASE_TIMEOUT=300
valid_uint "$KILL_GRACE" || KILL_GRACE=5
valid_uint "$TIMEOUT_MUL" || TIMEOUT_MUL=4
[ "$COUNT" -gt 0 ] 2>/dev/null || COUNT=50
[ "$CASE_TIMEOUT" -gt 0 ] 2>/dev/null || CASE_TIMEOUT=300

if [ -z "$SCENARIO" ]; then
    marker fatal "reason=missing-scenario"
    shutdown_runner 2
fi

RUNT_FILE="$LTPROOT/runtest/$SCENARIO"
if [ ! -f "$RUNT_FILE" ]; then
    marker fatal "reason=missing-runtest" "scenario=$SCENARIO"
    shutdown_runner 2
fi

if ! prepare_dynamic_linker; then
    marker fatal "reason=glibc-runtime-missing" "path=$GLIBC_LIB"
    shutdown_runner 2
fi
if ! prepare_workdir; then
    shutdown_runner 2
fi

export LTPROOT
export PATH="$LTPROOT/testcases/bin:$LTPROOT/bin:$PATH"
export TMPBASE="$WORK_ROOT/tmp"
export TMPDIR="$WORK_ROOT/tmp"
export LTP_DEV="$TEST_DEV"
export LTP_BIG_DEV="$BIG_DEV"
export LTP_DEV_FS_TYPE=ext2
export LTP_BIG_DEV_FS_TYPE=ext2
export LTP_TIMEOUT_MUL="$TIMEOUT_MUL"
export LTP_COLORIZE_OUTPUT=0

marker runner_start "run_id=$RUN_ID" "group=$GROUP" "scenario=$SCENARIO" \
    "start=$START" "count=$COUNT" "only=$ONLY" "timeout=$CASE_TIMEOUT" \
    "timeout_mul=$TIMEOUT_MUL"

index=0
selected=0
last_next="$START"
while IFS= read -r raw_line || [ -n "$raw_line" ]; do
    line="$(printf '%s\n' "$raw_line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    case "$line" in
        ''|'#'*) continue ;;
    esac

    tag="${line%%[ 	]*}"
    if [ -n "$ONLY" ]; then
        [ "$tag" = "$ONLY" ] || {
            index=$((index + 1))
            continue
        }
    else
        [ "$index" -ge "$START" ] || {
            index=$((index + 1))
            continue
        }
        [ "$selected" -lt "$COUNT" ] || break
    fi

    run_case "$index" "$tag" "$line"
    selected=$((selected + 1))
    last_next=$((index + 1))
    index=$((index + 1))
    [ -z "$ONLY" ] || break
done <"$RUNT_FILE"

marker shard_end "run_id=$RUN_ID" "group=$GROUP" "scenario=$SCENARIO" \
    "selected=$selected" "next=$last_next"
shutdown_runner 0
