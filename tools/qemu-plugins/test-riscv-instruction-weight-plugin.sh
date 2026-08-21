#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd -P)
image=${RISCV_WEIGHT_CONTAINER:-}
[ -n "$image" ] || {
    echo "RISCV_WEIGHT_CONTAINER must name a build image" >&2
    exit 2
}
timeout_seconds=${RISCV_WEIGHT_TEST_TIMEOUT:-60}
skip_build=${RISCV_WEIGHT_SKIP_BUILD:-0}
mkdir -p "$root/build"
work=$(mktemp -d "$root/build/riscv-weight-plugin-smoke.XXXXXX")
relative_work=${work#"$root"/}
trap 'rm -rf "$work"' EXIT HUP INT TERM

case "$timeout_seconds" in
    ''|*[!0-9]*) echo "RISCV_WEIGHT_TEST_TIMEOUT must be a positive integer" >&2; exit 2 ;;
esac
[ "$timeout_seconds" -gt 0 ] || {
    echo "RISCV_WEIGHT_TEST_TIMEOUT must be a positive integer" >&2
    exit 2
}
case "$skip_build" in
    0|1) ;;
    *) echo "RISCV_WEIGHT_SKIP_BUILD must be 0 or 1" >&2; exit 2 ;;
esac

if [ "$skip_build" -eq 0 ]; then
    echo "instruction-weight probe construction is owned by the external benchmark workspace" >&2
    echo "set RISCV_WEIGHT_SKIP_BUILD=1 and provide prebuilt artifacts" >&2
    exit 2
else
    echo "reusing instruction-weight artifacts (RISCV_WEIGHT_SKIP_BUILD=1)"
fi

kernel=$root/kernel-rv
probe=$root/build/riscv64/instruction-weight/riscv-instruction-weight.elf
initramfs=$root/build/riscv64/compat-initramfs.cpio
for artifact in "$kernel" "$probe" "$initramfs"; do
    [ -s "$artifact" ] || {
        echo "required instruction-weight artifact is missing: $artifact" >&2
        exit 1
    }
done

# 复用时不能只凭文件存在性判断：确认 kernel 嵌入了当前 initramfs，测试模式
# 正确，且其中的 stripped probe 与用于解析 marker PC 的外部 ELF 完全对应。
python3 - "$kernel" "$initramfs" "$work/embedded-probe" <<'PY'
import sys
from pathlib import Path

kernel_path, initramfs_path, embedded_probe_path = map(Path, sys.argv[1:])
kernel = kernel_path.read_bytes()
archive = initramfs_path.read_bytes()
if archive not in kernel:
    raise SystemExit("kernel-rv does not embed the current compat-initramfs.cpio")

position = 0
entries = {}
trailer_seen = False
while position < len(archive):
    header = archive[position : position + 110]
    if len(header) != 110 or header[:6] not in {b"070701", b"070702"}:
        raise SystemExit("compat-initramfs.cpio is not a valid newc archive")
    try:
        fields = [
            int(header[6 + index * 8 : 14 + index * 8], 16)
            for index in range(13)
        ]
    except ValueError as error:
        raise SystemExit("compat-initramfs.cpio has an invalid newc header") from error
    file_size = fields[6]
    name_size = fields[11]
    if name_size <= 0:
        raise SystemExit("compat-initramfs.cpio has an invalid entry name")
    name_start = position + 110
    name_end = name_start + name_size
    if name_end > len(archive) or archive[name_end - 1] != 0:
        raise SystemExit("compat-initramfs.cpio has a truncated entry name")
    name = archive[name_start : name_end - 1].decode("utf-8", "strict")
    data_start = (name_end + 3) & ~3
    data_end = data_start + file_size
    if data_end > len(archive):
        raise SystemExit("compat-initramfs.cpio has truncated entry data")
    if name == "TRAILER!!!":
        trailer_seen = True
        break
    if name in {"etc/mygo-test-mode", "bin/riscv-instruction-weight"}:
        entries[name] = archive[data_start:data_end]
    position = (data_end + 3) & ~3

if not trailer_seen:
    raise SystemExit("compat-initramfs.cpio is missing its newc trailer")
if entries.get("etc/mygo-test-mode", b"").strip() != b"instruction-weight":
    raise SystemExit("compat-initramfs.cpio is not an instruction-weight image")
embedded_probe = entries.get("bin/riscv-instruction-weight", b"")
if not embedded_probe:
    raise SystemExit("compat-initramfs.cpio is missing the instruction-weight probe")
embedded_probe_path.write_bytes(embedded_probe)
PY
cp "$probe" "$work/external-probe"
docker run --rm -v "$root":/work -w /work "$image" \
    riscv64-linux-musl-strip "/work/$relative_work/external-probe"
cmp -s "$work/embedded-probe" "$work/external-probe" || {
    echo "kernel-rv embeds a probe that does not match the external probe ELF" >&2
    exit 1
}

plugin=$work/riscv_instruction_weight.so
"$root/scripts/build-riscv-instruction-weight-plugin.sh" "$plugin" >/dev/null
symbols=$(docker run --rm -v "$root":/work -w /work "$image" \
    riscv64-linux-musl-nm -n \
        /work/build/riscv64/instruction-weight/riscv-instruction-weight.elf)
start_pc=$(printf '%s\n' "$symbols" | \
    awk '$3 == "riscv_weight_profile_start" {print "0x" $1}')
stop_pc=$(printf '%s\n' "$symbols" | \
    awk '$3 == "riscv_weight_profile_stop" {print "0x" $1}')
[ -n "$start_pc" ] && [ -n "$stop_pc" ] && [ "$start_pc" != "$stop_pc" ] || {
    echo "instruction-weight marker symbols are unavailable" >&2
    exit 1
}
exec_load=$(docker run --rm -v "$root":/work -w /work "$image" \
    riscv64-linux-musl-readelf -W -l \
        /work/build/riscv64/instruction-weight/riscv-instruction-weight.elf | \
    awk '$1 == "LOAD" && $7 == "R" && $8 == "E" {print $3, $6}')
[ "$(printf '%s\n' "$exec_load" | awk 'NF {n++} END {print n+0}')" -eq 1 ]
user_min_pc=${exec_load%% *}
user_mem_size=${exec_load#* }
user_max_pc=$(printf '0x%x' "$((user_min_pc + user_mem_size))")

for mode in timing validation; do
    plugin_options="file=/work/$relative_work/riscv_instruction_weight.so"
    plugin_options="$plugin_options,mode=$mode"
    plugin_options="$plugin_options,output=/work/$relative_work/$mode.jsonl"
    plugin_options="$plugin_options,start_pc=$start_pc,stop_pc=$stop_pc"
    plugin_options="$plugin_options,user_min_pc=$user_min_pc,user_max_pc=$user_max_pc"
    append="riscv_weight_base_blocks=1 riscv_weight_rounds=1"
    append="$append riscv_weight_case=addi:4 riscv_weight_run_id=plugin-$mode"
    docker run --rm -v "$root":/work -w /work "$image" sh -c '
        timeout -k 5 "$1" qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel tcg,thread=single -bios default -kernel /work/kernel-rv \
            -m 1G -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "$2" -plugin "$3"
    ' run-qemu "$timeout_seconds" "$append" "$plugin_options" \
        >"$work/$mode.serial.log" 2>&1
    grep -q '^RISCV_WEIGHT_GUEST_DONE status=0' "$work/$mode.serial.log"
done

python3 - "$work" <<'PY'
import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
for mode in ("timing", "validation"):
    rows = [json.loads(line) for line in (root / f"{mode}.jsonl").read_text().splitlines()]
    header = rows[0]
    footer = rows[-1]
    windows = [row for row in rows if row["type"] == "window"]
    assert header["schema"] == "mygo.riscv-instruction-weight-window.v2"
    assert header["mode"] == footer["mode"] == mode
    assert header["cpu_scope"] == footer["cpu_scope"] == "full-vcpu-thread"
    assert windows
    assert all(row["mode"] == mode for row in windows)
    assert all(isinstance(row["translations_during_window"], int) for row in windows)
    assert all(
        isinstance(row["scoped_translations_during_window"], int)
        for row in windows
    )
    if mode == "timing":
        assert header["count_scope"] == "unavailable"
        assert header["counts_available"] is False
        assert all(row["counts_available"] is False for row in windows)
        assert all(row["instruction_count"] is None and row["counts"] is None for row in windows)
    else:
        assert header["count_scope"] == "user-pc-range"
        assert header["user_min_pc"] is not None
        assert header["user_max_pc"] is not None
        assert header["counts_available"] is True
        assert all(row["counts_available"] is True for row in windows)
        assert all(isinstance(row["instruction_count"], int) for row in windows)
        assert any(
            count["bytes"] == "13051500"
            for row in windows
            for count in row["counts"]
        )
print("riscv instruction weight dual-mode smoke: ok")
PY
