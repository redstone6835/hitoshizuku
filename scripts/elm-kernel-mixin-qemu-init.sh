#!/bin/sh

PATH=/sbin:/bin:/usr/sbin:/usr/bin
export PATH

for dir in /dev /sys /proc /tmp; do
    [ -d "$dir" ] || mkdir -p "$dir"
done

mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true

status=0

fail() {
    echo "[elm-kernel-mixin-test] FAIL: $*"
    status=1
}

run() {
    echo "[elm-kernel-mixin-test] RUN: $*"
    "$@" || fail "$*"
}

echo "[elm-kernel-mixin-test] begin"
load_output="$(elmctl load-eki /tmp/kernel-mixin-v1.eki 2>&1)"
load_status=$?
echo "$load_output"
if [ "$load_status" -ne 0 ]; then
    fail "load v1"
fi

cell="$(printf '%s\n' "$load_output" | sed -n 's/.*cell=\([0-9][0-9]*\).*/\1/p' | head -n 1)"
if [ -z "$cell" ]; then
    fail "cannot parse loaded cell"
else
    echo "[elm-kernel-mixin-test] cell=$cell"
    run elmctl snapshot
    run elmctl pause "$cell"
    run elmctl snapshot
    run elmctl resume "$cell"
    run elmctl snapshot

    echo "[elm-kernel-mixin-test] RUN: rejected replacement"
    reject_output="$(elmctl replace-eki "$cell" /tmp/kernel-mixin-reject.eki 2>&1)"
    reject_status=$?
    echo "$reject_output"
    if [ "$reject_status" -eq 0 ]; then
        fail "rejected replacement unexpectedly committed"
    else
        echo "[elm-kernel-mixin-test] rejected replacement rolled back"
    fi
    run elmctl snapshot

    run elmctl replace-eki "$cell" /tmp/kernel-mixin-v2.eki
    run elmctl snapshot
    run elmctl detach "$cell"
    run elmctl snapshot
    run elmctl health
fi

if [ "$status" -eq 0 ]; then
    echo "[elm-kernel-mixin-test] PASS"
fi

poweroff -f
exit "$status"
