#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
qemu=${QEMU_SYSTEM_RISCV64:-qemu-system-riscv64}
cc=${CC:-cc}

command -v "$qemu" >/dev/null 2>&1 || {
    echo "tcg profile smoke: missing QEMU: $qemu" >&2
    exit 1
}
command -v "$cc" >/dev/null 2>&1 || {
    echo "tcg profile smoke: missing C compiler: $cc" >&2
    exit 1
}
command -v pkg-config >/dev/null 2>&1 || {
    echo "tcg profile smoke: missing pkg-config" >&2
    exit 1
}

mkdir -p "$root/build/qemu-plugins"
work=$(mktemp -d "$root/build/qemu-plugins/tcg-profile-test.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM
plugin=$work/mygo-tcg-profile.so
report=$work/report.txt

# 使用固件窗口避免依赖内核、initramfs 和测试盘，同时覆盖实际 TB 翻译与退出导出。
# shellcheck disable=SC2046
"$cc" -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared $(pkg-config --cflags glib-2.0) \
    "$root/tools/qemu-plugins/mygo-tcg-profile.c" -o "$plugin"

set +e
timeout -s TERM 2 "$qemu" -machine virt -m 128M -smp 2 -nographic -bios default \
    -plugin "file=$plugin,output=$report,table_bits=12" >"$work/serial.log" 2>&1
qemu_status=$?
set -e
case "$qemu_status" in
    0|124|143) ;;
    *)
        cat "$work/serial.log" >&2
        echo "tcg profile smoke: QEMU exited with $qemu_status" >&2
        exit 1
        ;;
esac

"$root/scripts/profile-tcg-validate.sh" "$report" riscv64 2
sed 's/dropped=0/dropped=1/' "$report" >"$work/dropped.txt"
"$root/scripts/profile-tcg-validate.sh" "$work/dropped.txt" riscv64 2

python3 - "$root/scripts/profile-snapshot-analyze.py" "$report" <<'PY'
import importlib.util
import sys
from pathlib import Path

analyzer_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("profile_snapshot_analyze", analyzer_path)
module = importlib.util.module_from_spec(spec)
assert spec and spec.loader
spec.loader.exec_module(module)
profile = module.parse_tcg_profile(report_path)
header = profile["header"]
assert header["version"] == "2"
assert header["table_bits"] == "12"
assert len(profile["vcpus"]) == int(header["active_vcpus"])
assert len(profile["hot"]) == int(header["reported_hotspots"])
assert profile["complete"]
Path(report_path).write_text(Path(report_path).read_text().replace("dropped=0", "dropped=1"))
assert not module.parse_tcg_profile(report_path)["complete"]
PY

echo "tcg profile smoke: ok"
