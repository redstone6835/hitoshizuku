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
CONFIG_FILE="$WORK_MOUNT/ltp.conf"

cmdline_value() {
    key="$1"
    fallback="$2"

    if [ -r "$CONFIG_FILE" ]; then
        value="$(awk -F '=' -v key="$key" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$CONFIG_FILE")"
        if [ -n "$value" ]; then
            printf '%s\n' "$value"
            return 0
        fi
    fi
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
    if [ ! -r "$CONFIG_FILE" ] && ! mount -t ext4 /dev/vd1 "$WORK_MOUNT" 2>/dev/null; then
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

count_pan_results() {
    result_log="$1"

    [ -r "$result_log" ] || {
        printf '0\n'
        return 0
    }
    awk '/^tag=/{ count++ } END { print count + 0 }' "$result_log"
}

split_pan_output() {
    output_log="$1"
    shard_dir="$2"

    [ -r "$output_log" ] || return 0
    awk -v output_dir="$shard_dir" '
        /^<<<test_start>>>$/ {
            sequence++
            output = sprintf("%s/output.%05d.log", output_dir, sequence)
        }
        output != "" { print >> output }
        /^<<<test_end>>>$/ {
            close(output)
            output = ""
        }
    ' "$output_log"
}

emit_pan_results() {
    run_map="$1"
    result_log="$2"
    shard_dir="$3"
    parsed_results="$shard_dir/parsed-results.tsv"

    awk '
        FNR == NR {
            fields = split($0, map, "\t")
            if (fields >= 2) {
                map_index[++map_count] = map[1]
                map_tag[map_count] = map[2]
            }
            next
        }
        /^tag=/ {
            sequence++
            reported = ""
            status = "255"
            termination = "unknown"
            duration = "0"
            fields = split($0, result, /[[:space:]]+/)
            for (field = 1; field <= fields; field++) {
                split(result[field], pair, "=")
                if (pair[1] == "tag")
                    reported = pair[2]
                else if (pair[1] == "stat")
                    status = pair[2]
                else if (pair[1] == "exit")
                    termination = pair[2]
                else if (pair[1] == "dur")
                    duration = pair[2]
            }
            printf "%s\t%s\t%s\t%s\t%s\t%s\n", \
                map_index[sequence], map_tag[sequence], reported, status, termination, duration
        }
    ' "$run_map" "$result_log" >"$parsed_results"

    EMITTED_RESULTS=0
    while IFS='	' read -r index expected_tag reported_tag status termination duration || \
        [ -n "$index" ]; do
        [ -n "$index" ] || continue
        if [ "$expected_tag" != "$reported_tag" ]; then
            marker fatal "reason=ltp-pan-tag-order-mismatch" "index=$index" \
                "expected=$expected_tag" "reported=$reported_tag"
            return 2
        fi
        EMITTED_RESULTS=$((EMITTED_RESULTS + 1))
        case_output="$shard_dir/$(printf 'output.%05d.log' "$EMITTED_RESULTS")"
        marker case_start "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$expected_tag"
        print_artifact output "$case_output"
        marker case_end "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$expected_tag" "result=run" "exit=$status" "ltp_stat=$status" \
            "termination=$termination" "elapsed=$duration"
    done <"$parsed_results"
    return 0
}

stop_pan() {
    child="$1"

    kill -TERM "-$child" 2>/dev/null || kill -TERM "$child" 2>/dev/null || true
    grace=0
    while kill -0 "$child" 2>/dev/null && [ "$grace" -lt "$KILL_GRACE" ]; do
        sleep 1
        grace=$((grace + 1))
    done
    kill -KILL "-$child" 2>/dev/null || kill -KILL "$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
}

run_pan_shard() {
    pan_file="$1"
    run_map="$2"
    run_count="$3"
    shard_dir="$WORK_ROOT/shard-$(printf '%05d' "$START")"
    shard_tmp="$shard_dir/tmp"
    console_log="$shard_dir/console.log"
    result_log="$shard_dir/result.log"
    output_log="$shard_dir/output.log"
    failed_log="$shard_dir/failed.log"
    tconf_log="$shard_dir/tconf.log"
    zoo_file="$shard_dir/zoo"

    rm -rf "$shard_dir"
    mkdir -p "$shard_tmp" || return 2
    chmod 1777 "$shard_tmp" 2>/dev/null || true
    marker batch_start "group=$GROUP" "scenario=$SCENARIO" "start=$START" \
        "count=$run_count"

    (
        cd "$LTPROOT/testcases/bin" || exit 2
        export TMPBASE="$shard_tmp"
        export TMPDIR="$shard_tmp"
        exec setsid "$LTPROOT/bin/ltp-pan" \
            -Q -e -S \
            -a "$zoo_file" -n "$RUN_ID-$SCENARIO-$START" \
            -f "$pan_file" \
            -l "$result_log" \
            -o "$output_log" \
            -C "$failed_log" \
            -T "$tconf_log"
    ) >"$console_log" 2>&1 &
    child=$!
    elapsed=0
    idle=0
    completed=0
    timed_out=0

    while kill -0 "$child" 2>/dev/null; do
        sleep 1
        elapsed=$((elapsed + 1))
        current="$(count_pan_results "$result_log")"
        if [ "$current" -gt "$completed" ] 2>/dev/null; then
            completed="$current"
            idle=0
            marker batch_progress "group=$GROUP" "scenario=$SCENARIO" \
                "completed=$completed" "total=$run_count"
        else
            idle=$((idle + 1))
        fi
        if [ "$idle" -ge "$CASE_TIMEOUT" ]; then
            timed_out=1
            break
        fi
    done

    if [ "$timed_out" -eq 1 ]; then
        stop_pan "$child"
        pan_ret=124
    else
        wait "$child"
        pan_ret=$?
        kill -KILL "-$child" 2>/dev/null || true
    fi
    completed="$(count_pan_results "$result_log")"
    split_pan_output "$output_log" "$shard_dir"
    if ! emit_pan_results "$run_map" "$result_log" "$shard_dir"; then
        shutdown_runner 2
    fi

    print_artifact console "$console_log"
    print_artifact result "$result_log"
    print_artifact failed "$failed_log"
    print_artifact tconf "$tconf_log"

    if [ "$timed_out" -eq 1 ]; then
        missing_number=$((completed + 1))
        missing="$(sed -n "${missing_number}p" "$run_map")"
        missing_index="${missing%%	*}"
        missing_tag="${missing#*	}"
        if [ -z "$missing" ]; then
            marker fatal "reason=timeout-without-current-case" "completed=$completed" \
                "total=$run_count"
            shutdown_runner 2
        fi
        marker case_start "group=$GROUP" "scenario=$SCENARIO" "index=$missing_index" \
            "tag=$missing_tag"
        marker case_end "group=$GROUP" "scenario=$SCENARIO" "index=$missing_index" \
            "tag=$missing_tag" "result=timeout" "exit=$pan_ret" "elapsed=$CASE_TIMEOUT"
        marker shard_abort "group=$GROUP" "scenario=$SCENARIO" \
            "next=$((missing_index + 1))" "reason=case-timeout"
        shutdown_runner 124
    fi

    if [ "$EMITTED_RESULTS" -ne "$run_count" ]; then
        marker fatal "reason=ltp-pan-result-count-mismatch" "expected=$run_count" \
            "actual=$EMITTED_RESULTS" "exit=$pan_ret"
        shutdown_runner 2
    fi
    marker batch_end "group=$GROUP" "scenario=$SCENARIO" "completed=$completed" \
        "exit=$pan_ret" "elapsed=$elapsed"
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

SELECTED_FILE="$WORK_ROOT/selected.runtest"
awk -v start="$START" -v count="$COUNT" -v only="$ONLY" '
    {
        line = $0
        sub(/^[[:space:]]*/, "", line)
        sub(/[[:space:]]*$/, "", line)
        if (line == "" || substr(line, 1, 1) == "#")
            next

        tag = line
        sub(/[[:space:]].*$/, "", tag)
        if (only != "") {
            if (tag == only) {
                printf "%d\t%s\n", case_index, line
                exit
            }
        } else if (case_index >= start && selected < count) {
            printf "%d\t%s\n", case_index, line
            selected++
        }
        case_index++
        if (only == "" && selected >= count)
            exit
    }
' "$RUNT_FILE" >"$SELECTED_FILE"

selected=0
run_selected=0
last_next="$START"
PAN_FILE="$WORK_ROOT/pan.runtest"
RUN_MAP="$WORK_ROOT/run-map.tsv"
: >"$PAN_FILE"
: >"$RUN_MAP"
while IFS= read -r record || [ -n "$record" ]; do
    index="${record%%	*}"
    line="${record#*	}"
    tag="${line%%[ 	]*}"
    skip="$(lookup_skip "$SCENARIO" "$tag" 2>/dev/null || true)"
    if [ -n "$skip" ]; then
        category="${skip%%|*}"
        reason="${skip#*|}"
        marker case_start "group=$GROUP" "scenario=$SCENARIO" "index=$index" "tag=$tag"
        marker case_skip "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$tag" "category=$category" "reason=$reason"
        marker case_end "group=$GROUP" "scenario=$SCENARIO" "index=$index" \
            "tag=$tag" "result=skip" "exit=0"
    else
        printf '%s\n' "$line" >>"$PAN_FILE"
        printf '%s\t%s\n' "$index" "$tag" >>"$RUN_MAP"
        run_selected=$((run_selected + 1))
    fi
    selected=$((selected + 1))
    last_next=$((index + 1))
done <"$SELECTED_FILE"

if [ "$run_selected" -gt 0 ]; then
    run_pan_shard "$PAN_FILE" "$RUN_MAP" "$run_selected"
fi
rm -f "$SELECTED_FILE" "$PAN_FILE" "$RUN_MAP"

marker shard_end "run_id=$RUN_ID" "group=$GROUP" "scenario=$SCENARIO" \
    "selected=$selected" "next=$last_next"
shutdown_runner 0
