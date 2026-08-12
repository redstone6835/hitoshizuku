#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <smoke|measure>" >&2
    exit 2
}

[ "$#" -eq 1 ] || usage
mode=$1
case "$mode" in
    smoke)
        default_runs=1
        default_blocks=4
        default_rounds=2
        default_case=addi:4
        default_bootstrap=99
        ;;
    measure)
        default_runs=12
        default_blocks=4
        default_rounds=10
        default_case=all
        default_bootstrap=999
        ;;
    *) usage ;;
esac

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
image=${RISCV_WEIGHT_CONTAINER:-zhouzhouyi/os-contest:20260510}
runs=${RISCV_WEIGHT_RUNS:-$default_runs}
base_blocks=${RISCV_WEIGHT_BASE_BLOCKS:-$default_blocks}
rounds=${RISCV_WEIGHT_ROUNDS:-$default_rounds}
benchmark_case=${RISCV_WEIGHT_CASE:-$default_case}
bootstrap=${RISCV_WEIGHT_BOOTSTRAP:-$default_bootstrap}
bootstrap_jobs=${RISCV_WEIGHT_BOOTSTRAP_JOBS:-8}
timeout_seconds=${RISCV_WEIGHT_TIMEOUT:-1800}
memory=${RISCV_WEIGHT_MEMORY:-1G}
accel=${RISCV_WEIGHT_ACCEL:-tcg,thread=single}
catalog=${RISCV_WEIGHT_CATALOG:-}
skip_build=${RISCV_WEIGHT_SKIP_BUILD:-0}
# 保留 form modifier，使不同指令形式维持独立语义类。
expected_catalog_keys=${RISCV_WEIGHT_EXPECTED_CATALOG_KEYS:-409}

for pair in \
    "RISCV_WEIGHT_RUNS:$runs" \
    "RISCV_WEIGHT_BASE_BLOCKS:$base_blocks" \
    "RISCV_WEIGHT_ROUNDS:$rounds" \
    "RISCV_WEIGHT_BOOTSTRAP:$bootstrap" \
    "RISCV_WEIGHT_BOOTSTRAP_JOBS:$bootstrap_jobs" \
    "RISCV_WEIGHT_TIMEOUT:$timeout_seconds" \
    "RISCV_WEIGHT_EXPECTED_CATALOG_KEYS:$expected_catalog_keys"
do
    name=${pair%%:*}
    value=${pair#*:}
    case "$value" in ''|*[!0-9]*) echo "$name must be a positive integer" >&2; exit 2 ;; esac
    [ "$value" -gt 0 ] || { echo "$name must be a positive integer" >&2; exit 2; }
done
case "$benchmark_case" in
    ''|*[!A-Za-z0-9_.:-]*) echo "RISCV_WEIGHT_CASE has invalid syntax" >&2; exit 2 ;;
esac
case "$skip_build" in
    0|1) ;;
    *) echo "RISCV_WEIGHT_SKIP_BUILD must be 0 or 1" >&2; exit 2 ;;
esac
if [ -n "$catalog" ]; then
    case "$catalog" in /*) ;; *) catalog=$root/$catalog ;; esac
    case "$catalog" in
        "$root"/*) ;;
        *) echo "RISCV_WEIGHT_CATALOG must be inside the repository" >&2; exit 2 ;;
    esac
    [ -r "$catalog" ] || { echo "RISCV_WEIGHT_CATALOG is not readable" >&2; exit 2; }

    echo "[riscv-weight] 预检 catalog 完整性与规范 key 数"
    python3 - "$root/scripts/map-riscv-instruction-weights.py" \
        "$catalog" "$expected_catalog_keys" <<'PY'
import importlib.util
import sys
from pathlib import Path

mapper_path = Path(sys.argv[1])
catalog_path = Path(sys.argv[2])
expected_key_count = int(sys.argv[3])
sys.path.insert(0, str(mapper_path.parent))
spec = importlib.util.spec_from_file_location(
    "riscv_instruction_weight_mapper_preflight", mapper_path
)
if spec is None or spec.loader is None:
    raise SystemExit("catalog preflight failed: mapper module cannot be loaded")
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
try:
    spec.loader.exec_module(module)
    rows = module.load_catalog(
        catalog_path, expected_key_count=expected_key_count
    )
except Exception as error:
    raise SystemExit(f"catalog preflight failed: {error}") from error
print(f"[riscv-weight] catalog 预检通过：{len(rows)} 个规范 key")
PY
fi

run_stamp=$(date -u +%Y%m%dT%H%M%SZ)-$mode-$$
output=${RISCV_WEIGHT_OUTPUT:-$root/build/riscv-instruction-weight-runs/$run_stamp}
case "$output" in
    "$root"/*) ;;
    *) echo "RISCV_WEIGHT_OUTPUT must be inside the repository" >&2; exit 2 ;;
esac
mkdir -p "$output"

echo "[riscv-weight] 构建探针内核与 QEMU 插件"
if [ "$skip_build" -eq 0 ]; then
    docker run --rm -v "$root":/work -w /work "$image" \
        make instruction-weight-rv \
            RISCV_WEIGHT_BASE_BLOCKS="$base_blocks" \
            RISCV_WEIGHT_ROUNDS="$rounds" \
            RISCV_WEIGHT_CASE="$benchmark_case" \
            RISCV_WEIGHT_RUN_ID=runtime
else
    echo "[riscv-weight] 复用 instruction-weight 构建产物"
fi

probe_elf=$root/build/riscv64/instruction-weight/riscv-instruction-weight.elf
[ -s "$root/kernel-rv" ] && [ -s "$probe_elf" ] || {
    echo "riscv weight build did not produce required artifacts" >&2
    exit 1
}
if [ "$skip_build" -eq 1 ]; then
    initramfs=$root/build/riscv64/compat-initramfs.cpio
    [ -s "$initramfs" ] || {
        echo "required instruction-weight artifact is missing: $initramfs" >&2
        exit 1
    }
    artifact_check=$(mktemp -d "$output/artifact-check.XXXXXX")
    artifact_check_relative=${artifact_check#"$root"/}
    cleanup_artifact_check() {
        if [ -n "${artifact_check:-}" ]; then
            rm -rf -- "$artifact_check"
        fi
    }
    trap cleanup_artifact_check EXIT HUP INT TERM
    python3 - "$root/kernel-rv" "$initramfs" \
        "$artifact_check/embedded-probe" <<'PY'
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
        raise SystemExit(
            "compat-initramfs.cpio has an invalid newc header"
        ) from error
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
    raise SystemExit(
        "compat-initramfs.cpio is missing the instruction-weight probe"
    )
embedded_probe_path.write_bytes(embedded_probe)
PY
    cp "$probe_elf" "$artifact_check/external-probe"
    docker run --rm -v "$root":/work -w /work "$image" \
        riscv64-linux-musl-strip \
        "/work/$artifact_check_relative/external-probe"
    cmp -s "$artifact_check/embedded-probe" \
        "$artifact_check/external-probe" || {
        echo "kernel-rv embeds a probe that does not match the external probe ELF" >&2
        exit 1
    }
    cleanup_artifact_check
    artifact_check=
    echo "[riscv-weight] 复用产物一致性校验通过"
fi

"$root/scripts/build-riscv-instruction-weight-plugin.sh" \
    "$output/riscv_instruction_weight.so" >/dev/null
symbols=$(docker run --rm -v "$root":/work -w /work "$image" \
    riscv64-linux-musl-nm -n /work/build/riscv64/instruction-weight/riscv-instruction-weight.elf)
start_pc=$(printf '%s\n' "$symbols" | awk '$3 == "riscv_weight_profile_start" {print "0x" $1}')
stop_pc=$(printf '%s\n' "$symbols" | awk '$3 == "riscv_weight_profile_stop" {print "0x" $1}')
[ "$(printf '%s\n' "$start_pc" | awk 'NF {n++} END {print n+0}')" -eq 1 ] &&
    [ "$(printf '%s\n' "$stop_pc" | awk 'NF {n++} END {print n+0}')" -eq 1 ] &&
    [ "$start_pc" != "$stop_pc" ] || {
        echo "probe start/stop markers are missing or ambiguous" >&2
        exit 1
    }

exec_load=$(docker run --rm -v "$root":/work -w /work "$image" \
    riscv64-linux-musl-readelf -W -l \
        /work/build/riscv64/instruction-weight/riscv-instruction-weight.elf | \
    awk '$1 == "LOAD" && $7 == "R" && $8 == "E" {print $3, $6}')
[ "$(printf '%s\n' "$exec_load" | awk 'NF {n++} END {print n+0}')" -eq 1 ] || {
    echo "probe ELF must contain exactly one executable LOAD segment" >&2
    exit 1
}
user_min_pc=${exec_load%% *}
user_mem_size=${exec_load#* }
user_max_pc=$(printf '0x%x' "$((user_min_pc + user_mem_size))")

docker run --rm -v "$root":/work -w /work "$image" \
    qemu-system-riscv64 --version >"$output/qemu-version.txt"
sha256sum "$root/kernel-rv" "$probe_elf" \
    "$output/riscv_instruction_weight.so" >"$output/artifacts.sha256"

validation_serial=$output/validation.serial.log
validation_plugin=$output/validation.windows.jsonl
validation_run_id="validation-$run_stamp"
echo "[riscv-weight] QEMU exact-count validation: id=$validation_run_id"
set +e
docker run --rm -v "$root":/work -w /work "$image" sh -c '
    set -eu
    timeout -k 10 "$1" qemu-system-riscv64 \
        -machine virt -global virtio-mmio.force-legacy=false \
        -accel "$2" -bios default -kernel /work/kernel-rv \
        -m "$3" -smp 1 -nographic -no-reboot -rtc base=utc \
        -append "riscv_weight_base_blocks=$4 riscv_weight_rounds=$5 riscv_weight_case=$6 riscv_weight_run_id=$7" \
        -plugin "file=$8,mode=validation,output=$9,start_pc=${10},stop_pc=${11},user_min_pc=${12},user_max_pc=${13}"
' run-qemu "$timeout_seconds" "$accel" "$memory" "$base_blocks" "$rounds" \
    "$benchmark_case" "$validation_run_id" \
    "/work/${output#"$root"/}/riscv_instruction_weight.so" \
    "/work/${validation_plugin#"$root"/}" "$start_pc" "$stop_pc" \
    "$user_min_pc" "$user_max_pc" >"$validation_serial" 2>&1
status=$?
set -e
if [ "$status" -ne 0 ] || ! tr -d '\r' <"$validation_serial" | \
    grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
    echo "[riscv-weight] validation 未正常完成（qemu status=$status）" >&2
    tail -n 120 "$validation_serial" >&2 || true
    exit 1
fi

timing_merge_arguments=
run=1
while [ "$run" -le "$runs" ]; do
    run_id="run-$run_stamp-$run"
    serial=$output/run-$run.serial.log
    timing_plugin=$output/run-$run.timing.jsonl
    plugin_off_serial=$output/run-$run.plugin-off.serial.log
    echo "[riscv-weight] QEMU marker-only timing $run/$runs: id=$run_id"
    set +e
    docker run --rm -v "$root":/work -w /work "$image" sh -c '
        set -eu
        timeout -k 10 "$1" qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$2" -bios default -kernel /work/kernel-rv \
            -m "$3" -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "riscv_weight_base_blocks=$4 riscv_weight_rounds=$5 riscv_weight_case=$6 riscv_weight_run_id=$7" \
            -plugin "file=$8,mode=timing,output=$9,start_pc=${10},stop_pc=${11},user_min_pc=${12},user_max_pc=${13}"
    ' run-qemu "$timeout_seconds" "$accel" "$memory" "$base_blocks" "$rounds" \
        "$benchmark_case" "$run_id" "/work/${output#"$root"/}/riscv_instruction_weight.so" \
        "/work/${timing_plugin#"$root"/}" "$start_pc" "$stop_pc" \
        "$user_min_pc" "$user_max_pc" >"$serial" 2>&1
    status=$?
    set -e
    if [ "$status" -ne 0 ] || ! tr -d '\r' <"$serial" | \
        grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
        echo "[riscv-weight] timing run $run 未正常完成（qemu status=$status）" >&2
        tail -n 120 "$serial" >&2 || true
        exit 1
    fi

    echo "[riscv-weight] QEMU plugin-off cross-check $run/$runs: id=$run_id"
    set +e
    docker run --rm -v "$root":/work -w /work "$image" sh -c '
        set -eu
        timeout -k 10 "$1" qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$2" -bios default -kernel /work/kernel-rv \
            -m "$3" -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "riscv_weight_base_blocks=$4 riscv_weight_rounds=$5 riscv_weight_case=$6 riscv_weight_run_id=$7"
    ' run-qemu "$timeout_seconds" "$accel" "$memory" "$base_blocks" "$rounds" \
        "$benchmark_case" "$run_id" >"$plugin_off_serial" 2>&1
    status=$?
    set -e
    if [ "$status" -ne 0 ] || ! tr -d '\r' <"$plugin_off_serial" | \
        grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
        echo "[riscv-weight] plugin-off run $run 未正常完成（qemu status=$status）" >&2
        tail -n 120 "$plugin_off_serial" >&2 || true
        exit 1
    fi
    timing_merge_arguments="$timing_merge_arguments --guest /work/${serial#"$root"/} --timing-plugin /work/${timing_plugin#"$root"/} --plugin-off-guest /work/${plugin_off_serial#"$root"/}"
    run=$((run + 1))
done

# 参数全部是本脚本生成、且路径已经限制在仓库内。
# shellcheck disable=SC2086
docker run --rm -v "$root":/work -w /work "$image" \
    python3 scripts/merge-riscv-instruction-weight-samples.py \
        --validation-guest "/work/${validation_serial#"$root"/}" \
        --validation-plugin "/work/${validation_plugin#"$root"/}" \
        $timing_merge_arguments --output "/work/${output#"$root"/}/samples.jsonl"

docker run --rm -v "$root":/work -w /work "$image" \
    python3 scripts/rv_instruction_microbench_model.py \
        "/work/${output#"$root"/}/samples.jsonl" \
        --output "/work/${output#"$root"/}/weights.json" \
        --csv "/work/${output#"$root"/}/weights.csv" \
        --bootstrap "$bootstrap" --jobs "$bootstrap_jobs"

if [ -n "$catalog" ]; then
    docker run --rm -v "$root":/work -w /work "$image" \
        python3 scripts/map-riscv-instruction-weights.py \
            --catalog "/work/${catalog#"$root"/}" \
            --weights "/work/${output#"$root"/}/weights.json" \
            --output "/work/${output#"$root"/}/catalog-weights.json" \
            --csv "/work/${output#"$root"/}/catalog-weights.csv" \
            --expected-key-count "$expected_catalog_keys"
fi

echo "[riscv-weight] 输出目录：$output"
