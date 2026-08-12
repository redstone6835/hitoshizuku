#!/bin/sh

PATH=/sbin:/bin:/usr/sbin:/usr/bin
export PATH

run_native_examples() {
    list=/etc/mygo-native-examples
    [ -r "$list" ] || return 0

    native_status=0
    while IFS= read -r program; do
        [ -n "$program" ] || continue
        if [ ! -x "$program" ]; then
            echo "[init][soyo-tests] missing $program"
            native_status=1
            continue
        fi
        echo "[init][soyo-tests] running $program"
        "$program"
        status=$?
        echo "[init][soyo-tests] $program exit=$status"
        [ "$status" -eq 0 ] || native_status=1
    done < "$list"

    if [ "$native_status" -eq 0 ]; then
        echo "[init][soyo-tests] Native examples PASS"
    else
        echo "[init][soyo-tests] Native examples FAIL"
    fi
    return "$native_status"
}

run_native_examples || true

if [ -x /bin/riscv-instruction-weight ]; then
    riscv_weight_args="$(cat /etc/mygo-riscv-instruction-weight-args 2>/dev/null || true)"
    # shellcheck disable=SC2086
    set -- $riscv_weight_args
    riscv_weight_base_blocks="${1:-256}"
    riscv_weight_rounds="${2:-9}"
    riscv_weight_case="${3:-all}"
    riscv_weight_run_id="${4:-default}"
    for cmdline_file in /proc/cmdline /sys/kernel/cmdline; do
        [ -r "$cmdline_file" ] || continue
        for cmdline_arg in $(cat "$cmdline_file" 2>/dev/null); do
            case "$cmdline_arg" in
                riscv_weight_base_blocks=*)
                    riscv_weight_base_blocks="${cmdline_arg#*=}"
                    ;;
                riscv_weight_rounds=*) riscv_weight_rounds="${cmdline_arg#*=}" ;;
                riscv_weight_case=*) riscv_weight_case="${cmdline_arg#*=}" ;;
                riscv_weight_run_id=*) riscv_weight_run_id="${cmdline_arg#*=}" ;;
            esac
        done
    done
    riscv_weight_args="$riscv_weight_base_blocks $riscv_weight_rounds $riscv_weight_case $riscv_weight_run_id"
    echo "[init][riscv-weight] start args=$riscv_weight_args"
    # 参数由构建系统生成并由探针自身再次严格校验。
    # shellcheck disable=SC2086
    /bin/riscv-instruction-weight $riscv_weight_args
    riscv_weight_status=$?
    echo "[init][riscv-weight] exit=$riscv_weight_status"
    echo "RISCV_WEIGHT_GUEST_DONE status=$riscv_weight_status"
    sync || true
    poweroff -f >/dev/null 2>&1 || poweroff >/dev/null 2>&1 || \
        reboot -f >/dev/null 2>&1 || true
    echo "[init][riscv-weight] shutdown failed"
    exec /bin/sh -i
fi
if [ -x /bin/mm-fault-bench ]; then
    mm_bench_args="$(cat /etc/mygo-mm-bench-args 2>/dev/null || true)"
    # shellcheck disable=SC2086
    set -- $mm_bench_args
    mm_bench_case="${1:-anon-write}"
    mm_bench_pages="${2:-1}"
    mm_bench_threads="${3:-1}"
    mm_bench_repeats="${4:-1}"
    for cmdline_file in /proc/cmdline /sys/kernel/cmdline; do
        [ -r "$cmdline_file" ] || continue
        for cmdline_arg in $(cat "$cmdline_file" 2>/dev/null); do
            case "$cmdline_arg" in
                mm_bench_case=*) mm_bench_case="${cmdline_arg#*=}" ;;
                mm_bench_pages=*) mm_bench_pages="${cmdline_arg#*=}" ;;
                mm_bench_threads=*) mm_bench_threads="${cmdline_arg#*=}" ;;
                mm_bench_repeats=*) mm_bench_repeats="${cmdline_arg#*=}" ;;
            esac
        done
    done
    mm_bench_args="$mm_bench_case $mm_bench_pages $mm_bench_threads $mm_bench_repeats"
    echo "[init][mm-bench] start args=$mm_bench_args"
    # 参数只允许 benchmark 自身接受的枚举与十进制整数。
    # shellcheck disable=SC2086
    /bin/mm-fault-bench $mm_bench_args
    mm_bench_status=$?
    echo "[init][mm-bench] exit=$mm_bench_status"
    echo "MM_FAULT_GUEST_DONE status=$mm_bench_status"
    sync || true
    poweroff -f >/dev/null 2>&1 || poweroff >/dev/null 2>&1 || \
        reboot -f >/dev/null 2>&1 || true
    echo "[init][mm-bench] shutdown failed"
    exec /bin/sh -i
fi

if [ -x /bin/syscall-bench ]; then
    syscall_bench_args="$(cat /etc/mygo-syscall-bench-args 2>/dev/null || true)"
    # shellcheck disable=SC2086
    set -- $syscall_bench_args
    syscall_bench_iterations="${1:-1000000}"
    syscall_bench_repeats="${2:-5}"
    syscall_bench_case="${3:-all}"
    syscall_bench_warmup="${4:-100000}"
    for cmdline_file in /proc/cmdline /sys/kernel/cmdline; do
        [ -r "$cmdline_file" ] || continue
        for cmdline_arg in $(cat "$cmdline_file" 2>/dev/null); do
            case "$cmdline_arg" in
                syscall_bench_iterations=*)
                    syscall_bench_iterations="${cmdline_arg#*=}"
                    ;;
                syscall_bench_repeats=*)
                    syscall_bench_repeats="${cmdline_arg#*=}"
                    ;;
                syscall_bench_case=*)
                    syscall_bench_case="${cmdline_arg#*=}"
                    ;;
                syscall_bench_warmup=*)
                    syscall_bench_warmup="${cmdline_arg#*=}"
                    ;;
            esac
        done
    done
    syscall_bench_args="$syscall_bench_iterations $syscall_bench_repeats $syscall_bench_case $syscall_bench_warmup"
    echo "[init][syscall-bench] start args=$syscall_bench_args"
    # 参数由构建系统写入，故这里有意按空白拆分为 benchmark 的四个参数。
    # shellcheck disable=SC2086
    /bin/syscall-bench $syscall_bench_args
    syscall_bench_status=$?
    echo "[init][syscall-bench] exit=$syscall_bench_status"
    echo "SYSCALL_GUEST_DONE status=$syscall_bench_status"
    sync || true
    poweroff -f >/dev/null 2>&1 || poweroff >/dev/null 2>&1 || \
        reboot -f >/dev/null 2>&1 || true
    echo "[init][syscall-bench] shutdown failed"
    exec /bin/sh -i
fi

if [ -x /bin/pthread-smp-test ]; then
    echo "[init][smp-tests] running pthread SMP tests"
    /bin/pthread-smp-test
    status=$?
    echo "[init][smp-tests] pthread SMP tests exit=$status"
fi

if [ -x /bin/acct-test ]; then
    echo "[init][acct-tests] running process accounting tests"
    /bin/acct-test record
    record_status=$?
    /bin/acct-test verify "$record_status"
    status=$?
    echo "[init][acct-tests] process accounting tests exit=$status"
fi
