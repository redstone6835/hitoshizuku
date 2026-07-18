#!/bin/sh

PATH=/sbin:/bin:/usr/sbin:/usr/bin
export PATH

for dir in /dev /sys /proc /tmp; do
    [ -d "$dir" ] || mkdir -p "$dir"
done

mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true

echo "[elm-qemu-smoke] begin"
elmctl trust
ls -l /tmp/demo.hello.eki
elmctl load-eki /tmp/demo.hello.eki
load_status=$?
elmctl snapshot
elmctl health
echo "[elm-qemu-smoke] load_status=$load_status"

poweroff -f
exit "$load_status"
