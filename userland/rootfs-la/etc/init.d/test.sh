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

if [ -x /bin/pthread-smp-test ]; then
    echo "[init][smp-tests] running pthread SMP tests"
    /bin/pthread-smp-test
    status=$?
    echo "[init][smp-tests] pthread SMP tests exit=$status"
fi
if [ -x /bin/inotify-test ]; then
    echo "[init][fs-tests] running inotify tests"
    /bin/inotify-test
    status=$?
    echo "[init][fs-tests] inotify tests exit=$status"
fi
if [ -x /bin/xattr-test ]; then
    echo "[init][fs-tests] running xattr tests"
    /bin/xattr-test
    status=$?
    echo "[init][fs-tests] xattr tests exit=$status"
fi
if [ -x /bin/fanotify-test ]; then
    echo "[init][fs-tests] running fanotify tests"
    /bin/fanotify-test
    status=$?
    echo "[init][fs-tests] fanotify tests exit=$status"
fi
if [ -x /bin/direct-io-test ]; then
    echo "[init][fs-tests] running direct-io tests"
    /bin/direct-io-test
    status=$?
    echo "[init][fs-tests] direct-io tests exit=$status"
fi
if [ -x /bin/loongarch-sxe-test ]; then
    echo "[init][lazy-sxe-tests] running LoongArch FP/LSX state tests"
    /bin/loongarch-sxe-test
    status=$?
    echo "[init][lazy-sxe-tests] LoongArch FP/LSX state tests exit=$status"
fi
