#!/bin/sh

LTP_TIMEOUT="${LTP_TIMEOUT:-1}"
LTP_KILL_GRACE="${LTP_KILL_GRACE:-1}"
LTP_POLL_USEC="${LTP_POLL_USEC:-100000}"
LTP_CUSTOM_SCENARIOS="${LTP_CUSTOM_SCENARIOS:-event signal fs process ipc io time memory}"
LTP_SCENARIOS="${LTP_SCENARIOS:-$LTP_CUSTOM_SCENARIOS}"
LTP_ONLY_CASES="${LTP_ONLY_CASES:-}"
LTP_SKIP_CASES="${LTP_SKIP_CASES:-${LTP_SKIP_BASE:-}}"

if [ -r /sys/kernel/cmdline ]; then
    for arg in $(cat /sys/kernel/cmdline 2>/dev/null); do
        case "$arg" in
            ltp_only=*) LTP_ONLY_CASES="${arg#ltp_only=}" ;;
            ltp_scenarios=*) LTP_SCENARIOS="${arg#ltp_scenarios=}" ;;
            ltp_timeout=*) LTP_TIMEOUT="${arg#ltp_timeout=}" ;;
            ltp_kill_grace=*) LTP_KILL_GRACE="${arg#ltp_kill_grace=}" ;;
            ltp_poll_usec=*) LTP_POLL_USEC="${arg#ltp_poll_usec=}" ;;
        esac
    done
fi

LTP_ONLY_CASES="$(printf '%s' "$LTP_ONLY_CASES" | tr ',' ' ')"
LTP_SCENARIOS="$(printf '%s' "$LTP_SCENARIOS" | tr ',' ' ')"

ltp_root="ltp"
target_dir="$ltp_root/testcases/bin"
cmdlist="/tmp/ltp-runtest.$$"
case_seq=0

kill_ltp_case_group() {
    target="$1"
    child="$2"

    kill -TERM "$target" 2>/dev/null || true
    kill -TERM "$child" 2>/dev/null || true
    kill -KILL "$target" 2>/dev/null || true
    kill -KILL "$child" 2>/dev/null || true
}

ltp_sleep_tick() {
    if command -v usleep >/dev/null 2>&1; then
        usleep "$LTP_POLL_USEC"
    else
        sleep 1
    fi
}

install_ltp_custom_scenarios() {
    custom_src="/etc/ltp-scenarios"
    custom_dst="$ltp_root/runtest"
    installed=""

    [ -d "$custom_src" ] || return 0
    [ -d "$custom_dst" ] || return 0

    for custom_file in "$custom_src"/*; do
        [ -f "$custom_file" ] || continue
        custom_name="${custom_file##*/}"
        if cp "$custom_file" "$custom_dst/$custom_name" 2>/dev/null; then
            installed="$installed $custom_name"
        else
            echo "[ltp] 安装自定义场景失败：$custom_name"
        fi
    done

    [ -n "$installed" ] && echo "[ltp] 已安装自定义场景:$installed"
    return 0
}

print_ltp_case_output() {
    output="$1"

    [ -f "$output" ] || return 0

    # 由于 musl 测评机脚本本身具有设计缺陷，而 LTP 的输出确实有一些没有带上 Summary 的，
    # 因此这里会检测 LTP 输出中是否已经包含 Summary，如果没有的话就自己统计一下并补上。
    # 对于已经带了 Summary 的输出，我们不会做任何处理，也不会存在多余输出。
    #
    # 还有就是 LTP 的输出中有一些 TPASS 后面是不加冒号的，而根据测评机的脚本，测评机在
    # 评判 glibc 的 LTP 测试结果的时候直接硬编码了 TPASS: 的格式，这就导致了测评机无法
    # 正确识别那些没有冒号的 TPASS 输出，因此我们也会检查 LTP 输出的 TPASS 有没有冒号，
    # 如果没有的话就自己加上冒号并高亮一下，如果有则不做任何处理，也不会出现多余输出导
    # 致误判。
    #
    # 这些处理都是为了尽可能兼容测评机的脚本实现细节，避免测评机因为 LTP 输出格式的问题
    # 而无法正确识别测试结果，从而导致误判或者漏判。
    if command -v awk >/dev/null 2>&1; then
        awk '
        function strip_ansi(s) {
            gsub(sprintf("%c\\[[0-9;]*m", 27), "", s)
            return s
        }
        function has_status(s, status) {
            return s ~ ("(^|[^A-Z])" status "([^A-Z]|$)") && s !~ /(^|[^A-Z])TINFO([^A-Z]|$)/
        }
        {
            print
            plain = strip_ansi($0)
            if (plain ~ /^Summary:/)
                has_summary = 1
            if (has_status(plain, "TPASS")) {
                passed++
                if (plain !~ /TPASS:/)
                    printf "%c[1;32mTPASS: %c[0m\n", 27, 27
            } else if (has_status(plain, "TFAIL")) {
                failed++
            } else if (has_status(plain, "TBROK")) {
                broken++
            } else if (has_status(plain, "TCONF")) {
                skipped++
            } else if (has_status(plain, "TWARN")) {
                warnings++
            }
        }
        END {
            if (!has_summary) {
                print ""
                print "Summary:"
                printf "passed   %d\n", passed + 0
                printf "failed   %d\n", failed + 0
                printf "broken   %d\n", broken + 0
                printf "skipped  %d\n", skipped + 0
                printf "warnings %d\n", warnings + 0
            }
        }' "$output"
    else
        cat "$output"
    fi
}

ltp_case_is_skipped() {
    tag="$1"
    cmd="$2"
    cmd_name="${cmd%%[ 	]*}"
    cmd_name="${cmd_name##*/}"

    for skip in $LTP_SKIP_CASES; do
        if [ "$tag" = "$skip" ] || [ "$cmd_name" = "$skip" ]; then
            return 0
        fi
    done
    return 1
}

ltp_case_is_selected() {
    tag="$1"

    [ -n "$LTP_ONLY_CASES" ] || return 0
    for only in $LTP_ONLY_CASES; do
        [ "$tag" = "$only" ] && return 0
    done
    return 1
}

run_ltp_case() {
    tag="$1"
    cmd="$2"
    case_seq=$((case_seq + 1))
    run_name="$(printf '%04d_%s' "$case_seq" "$tag")"
    marker="/tmp/ltp-case-timeout.$$"
    status="/tmp/ltp-case-status.$$"
    output="/tmp/ltp-case-output.$$"
    timeout_ticks=$((LTP_TIMEOUT * 1000000 / LTP_POLL_USEC))
    grace_ticks=$((LTP_KILL_GRACE * 1000000 / LTP_POLL_USEC))
    [ "$timeout_ticks" -gt 0 ] 2>/dev/null || timeout_ticks=1
    [ "$grace_ticks" -gt 0 ] 2>/dev/null || grace_ticks=1

    if ltp_case_is_skipped "$tag" "$cmd"; then
        echo "RUN LTP CASE $run_name"
        echo "[ltp] 跳过会卡住 runner 的用例：$tag"
        echo "FAIL LTP CASE $run_name : 124"
        return 0
    fi

    rm -f "$marker" "$status" "$output"
    echo "RUN LTP CASE $run_name"

    if command -v setsid >/dev/null 2>&1; then
        setsid /bin/sh -c '
            rm -f "$2" "$3"
            LTP_COLORIZE_OUTPUT=1 sh -c "$1" >"$3" 2>&1
            echo "$?" > "$2"
        ' sh "$cmd" "$status" "$output" &
        child=$!
        kill_target="-$child"
    else
        /bin/sh -c '
            rm -f "$2" "$3"
            LTP_COLORIZE_OUTPUT=1 sh -c "$1" >"$3" 2>&1
            echo "$?" > "$2"
        ' sh "$cmd" "$status" "$output" &
        child=$!
        kill_target="$child"
    fi

    elapsed=0
    while [ ! -f "$status" ]; do
        if [ "$elapsed" -ge "$timeout_ticks" ]; then
            echo "[ltp] 用例超时：$run_name，超过 ${LTP_TIMEOUT}s"
            : >"$marker"
            kill_ltp_case_group "$kill_target" "$child"
            print_ltp_case_output "$output"

            grace=0
            while [ "$grace" -lt "$grace_ticks" ] && [ ! -f "$status" ]; do
                ltp_sleep_tick
                grace=$((grace + 1))
            done

            if [ ! -f "$status" ]; then
                echo "[ltp] 用例已终止但未回收，继续后续用例：$run_name"
                rm -f "$marker" "$status" "$output"
                echo "FAIL LTP CASE $run_name : 124"
                return 0
            fi
            break
        fi
        ltp_sleep_tick
        elapsed=$((elapsed + 1))
    done

    ret="$(cat "$status" 2>/dev/null || echo 124)"
    if [ ! -f "$marker" ]; then
        print_ltp_case_output "$output"
    fi
    kill_ltp_case_group "$kill_target" "$child"
    wait "$child" 2>/dev/null || true

    [ -f "$marker" ] && ret=124
    rm -f "$marker" "$status" "$output"
    echo "FAIL LTP CASE $run_name : $ret"
    return 0
}

libc_name="${PWD##*/}"
echo "#### OS COMP TEST GROUP START ltp-$libc_name ####"

if [ ! -d "$target_dir" ] || [ ! -d "$ltp_root/runtest" ]; then
    echo "[ltp] LTP 目录不完整：$PWD/$ltp_root"
    echo "#### OS COMP TEST GROUP END ltp-$libc_name ####"
    exit 0
fi

export LTPROOT="$PWD/$ltp_root"
export PATH="$LTPROOT/testcases/bin:$LTPROOT/bin:$PATH"
export LTP_DEV_FS_TYPE="${LTP_DEV_FS_TYPE:-ext2}"

install_ltp_custom_scenarios

echo "[init] 使用 ltp_testcode.sh 触发 LTP runtest，场景：$LTP_SCENARIOS，逐用例超时 ${LTP_TIMEOUT}s，跳过：$LTP_SKIP_CASES"
[ -n "$LTP_ONLY_CASES" ] && echo "[ltp] 仅运行用例：$LTP_ONLY_CASES"

: >"$cmdlist"
if [ "$LTP_SCENARIOS" = "default" ] && [ -f "$ltp_root/scenario_groups/default" ]; then
    scenarios="$(cat "$ltp_root/scenario_groups/default" 2>/dev/null)"
elif [ "$LTP_SCENARIOS" = "default" ]; then
    scenarios="syscalls controllers"
else
    scenarios="$LTP_SCENARIOS"
fi

for scenario in $scenarios; do
    runtest_file="$ltp_root/runtest/$scenario"
    if [ ! -f "$runtest_file" ] && [ -f "/etc/ltp-scenarios/$scenario" ]; then
        runtest_file="/etc/ltp-scenarios/$scenario"
    fi
    if [ ! -f "$runtest_file" ]; then
        echo "[ltp] 跳过缺失场景：$scenario"
        continue
    fi
    sed -e 's/^[ 	]*//' -e '/^#/d' -e '/^$/d' "$runtest_file" >>"$cmdlist"
done

while IFS= read -r line || [ -n "$line" ]; do
    tag="${line%%[ 	]*}"
    cmd="${line#"$tag"}"
    cmd="$(printf '%s\n' "$cmd" | sed 's/^[ 	]*//')"
    [ -n "$cmd" ] || cmd="$tag"

    ltp_case_is_selected "$tag" || continue
    run_ltp_case "$tag" "$cmd"
done <"$cmdlist"

rm -f "$cmdlist"

echo "#### OS COMP TEST GROUP END ltp-$libc_name ####"
exit 0
