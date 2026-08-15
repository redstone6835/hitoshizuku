#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <smoke|measure|differential-smoke|differential-measure|calibration-smoke|calibration-measure>" >&2
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
        default_ml_validate=0
        ;;
    measure)
        default_runs=12
        default_blocks=4
        default_rounds=10
        default_case=all
        default_bootstrap=4999
        default_ml_validate=1
        ;;
    differential-smoke)
        default_runs=3
        default_blocks=2
        default_rounds=3
        default_case=differential-v2
        default_bootstrap=99
        default_ml_validate=1
        ;;
    differential-measure)
        default_runs=20
        default_blocks=4
        default_rounds=10
        default_case=differential-v2
        default_bootstrap=4999
        default_ml_validate=1
        ;;
    calibration-smoke)
        default_runs=3
        default_blocks=4
        default_rounds=5
        default_case=differential-v2-long-calibration
        default_bootstrap=99
        default_ml_validate=0
        ;;
    calibration-measure)
        # random/chronological 两个联合 conformal 族各使用 97.5%
        # 覆盖率：20 train + 39 calibration + 146 future test。
        default_runs=205
        default_blocks=4
        default_rounds=10
        default_case=differential-v2-long-calibration
        default_bootstrap=4999
        default_ml_validate=1
        ;;
    *) usage ;;
esac

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
image=${RISCV_WEIGHT_CONTAINER:-zhouzhouyi/os-contest:20260510}
container_runtime=${RISCV_WEIGHT_CONTAINER_RUNTIME:-docker}
container_mount_suffix=${RISCV_WEIGHT_CONTAINER_MOUNT_SUFFIX:-}
container_run_arguments=${RISCV_WEIGHT_CONTAINER_RUN_ARGUMENTS:-}
runs=${RISCV_WEIGHT_RUNS:-$default_runs}
base_blocks=${RISCV_WEIGHT_BASE_BLOCKS:-$default_blocks}
rounds=${RISCV_WEIGHT_ROUNDS:-$default_rounds}
benchmark_case=${RISCV_WEIGHT_CASE:-$default_case}
bootstrap=${RISCV_WEIGHT_BOOTSTRAP:-$default_bootstrap}
# NumPy worker 内已禁用 BLAS 线程；16 个进程在正式主机上并行执行 4999 次
# bootstrap 保持在半小时内。资源较小的主机可用环境变量降低该值。
bootstrap_jobs=${RISCV_WEIGHT_BOOTSTRAP_JOBS:-16}
linear_algebra_backend=${RISCV_WEIGHT_LINEAR_ALGEBRA_BACKEND:-numpy}
ml_validate=${RISCV_WEIGHT_ML_VALIDATE:-$default_ml_validate}
ml_folds=${RISCV_WEIGHT_ML_FOLDS:-6}
ml_max_iter=${RISCV_WEIGHT_ML_MAX_ITER:-160}
ml_bootstrap=${RISCV_WEIGHT_ML_BOOTSTRAP:-999}
ml_minimum_runs=${RISCV_WEIGHT_ML_MINIMUM_RUNS:-20}
ml_confidence=${RISCV_WEIGHT_ML_CONFIDENCE:-0.975}
conformal_train_runs=${RISCV_WEIGHT_CONFORMAL_TRAIN_RUNS:-}
conformal_calibration_runs=${RISCV_WEIGHT_CONFORMAL_CALIBRATION_RUNS:-}
conformal_test_runs=${RISCV_WEIGHT_CONFORMAL_TEST_RUNS:-}
timeout_seconds=${RISCV_WEIGHT_TIMEOUT:-1800}
memory=${RISCV_WEIGHT_MEMORY:-1G}
accel=${RISCV_WEIGHT_ACCEL:-tcg,thread=single}
launch_seed=${RISCV_WEIGHT_LAUNCH_SEED:-1296847446}
cpuset=${RISCV_WEIGHT_CPUSET:-}
cpuset_mode=${RISCV_WEIGHT_CPUSET_MODE:-auto}
case "$mode" in
    measure|differential-measure|calibration-measure) default_require_exclusive_cpu=1 ;;
    *) default_require_exclusive_cpu=0 ;;
esac
require_exclusive_cpu=${RISCV_WEIGHT_REQUIRE_EXCLUSIVE_CPU:-$default_require_exclusive_cpu}
startup_warmups=${RISCV_WEIGHT_STARTUP_WARMUPS:-1}
external_host_audit=${RISCV_WEIGHT_HOST_AUDIT:-}
host_audit_require_psi=${RISCV_WEIGHT_HOST_AUDIT_REQUIRE_PSI:-0}
host_audit_require_frequency_floor=${RISCV_WEIGHT_HOST_AUDIT_REQUIRE_FREQUENCY_FLOOR:-0}
host_audit_require_window_frequency=${RISCV_WEIGHT_HOST_AUDIT_REQUIRE_WINDOW_FREQUENCY:-$require_exclusive_cpu}
host_audit_require_frequency_preflight=${RISCV_WEIGHT_HOST_AUDIT_REQUIRE_FREQUENCY_PREFLIGHT:-$require_exclusive_cpu}
host_audit_require_interrupts=${RISCV_WEIGHT_HOST_AUDIT_REQUIRE_INTERRUPTS:-$require_exclusive_cpu}
host_audit_require_schedstat=${RISCV_WEIGHT_HOST_AUDIT_REQUIRE_SCHEDSTAT:-$require_exclusive_cpu}
host_audit_max_interrupts_per_second=${RISCV_WEIGHT_HOST_AUDIT_MAX_INTERRUPTS_PER_SECOND:-25}
host_audit_max_runqueue_wait_fraction=${RISCV_WEIGHT_HOST_AUDIT_MAX_RUNQUEUE_WAIT_FRACTION:-0.01}
host_telemetry_sudo=${RISCV_WEIGHT_HOST_TELEMETRY_SUDO:-0}
physical_core_cpuset=${RISCV_WEIGHT_PHYSICAL_CORE_CPUSET:-}
isolation_state=${RISCV_WEIGHT_ISOLATION_STATE:-}
require_isolation_state=${RISCV_WEIGHT_REQUIRE_ISOLATION_STATE:-$require_exclusive_cpu}
host_audit=
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
    "RISCV_WEIGHT_ML_FOLDS:$ml_folds" \
    "RISCV_WEIGHT_ML_MAX_ITER:$ml_max_iter" \
    "RISCV_WEIGHT_ML_BOOTSTRAP:$ml_bootstrap" \
    "RISCV_WEIGHT_ML_MINIMUM_RUNS:$ml_minimum_runs" \
    "RISCV_WEIGHT_TIMEOUT:$timeout_seconds" \
    "RISCV_WEIGHT_LAUNCH_SEED:$launch_seed" \
    "RISCV_WEIGHT_STARTUP_WARMUPS:$startup_warmups" \
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
case "$require_exclusive_cpu" in
    0|1) ;;
    *) echo "RISCV_WEIGHT_REQUIRE_EXCLUSIVE_CPU must be 0 or 1" >&2; exit 2 ;;
esac
for value in "$host_audit_require_psi" "$host_audit_require_frequency_floor" \
    "$host_audit_require_window_frequency" \
    "$host_audit_require_frequency_preflight" \
    "$host_audit_require_interrupts"; do
    case "$value" in 0|1) ;; *) echo "host audit require flags must be 0 or 1" >&2; exit 2 ;; esac
done
case "$host_audit_require_schedstat" in
    0|1) ;;
    *) echo "RISCV_WEIGHT_HOST_AUDIT_REQUIRE_SCHEDSTAT must be 0 or 1" >&2; exit 2 ;;
esac
case "$host_audit_max_interrupts_per_second" in
    ''|*[!0-9.]*|*.*.*) echo "RISCV_WEIGHT_HOST_AUDIT_MAX_INTERRUPTS_PER_SECOND must be a nonnegative number" >&2; exit 2 ;;
esac
awk -v value="$host_audit_max_interrupts_per_second" 'BEGIN { exit !(value >= 0) }' || {
    echo "RISCV_WEIGHT_HOST_AUDIT_MAX_INTERRUPTS_PER_SECOND must be nonnegative" >&2
    exit 2
}
case "$host_audit_max_runqueue_wait_fraction" in
    ''|*[!0-9.]*|*.*.*) echo "RISCV_WEIGHT_HOST_AUDIT_MAX_RUNQUEUE_WAIT_FRACTION must be a number in [0,1]" >&2; exit 2 ;;
esac
awk -v value="$host_audit_max_runqueue_wait_fraction" \
    'BEGIN { exit !(value >= 0 && value <= 1) }' || {
    echo "RISCV_WEIGHT_HOST_AUDIT_MAX_RUNQUEUE_WAIT_FRACTION must be in [0,1]" >&2
    exit 2
}
case "$container_runtime" in
    *[!A-Za-z0-9_./-]*|'') echo "RISCV_WEIGHT_CONTAINER_RUNTIME has invalid syntax" >&2; exit 2 ;;
esac
case "$container_mount_suffix" in
    ''|:z|:Z) ;;
    *) echo "RISCV_WEIGHT_CONTAINER_MOUNT_SUFFIX must be empty, :z, or :Z" >&2; exit 2 ;;
esac
case "$host_telemetry_sudo" in
    0) host_telemetry_command=python3 ;;
    1) host_telemetry_command="sudo -n python3" ;;
    *) echo "RISCV_WEIGHT_HOST_TELEMETRY_SUDO must be 0 or 1" >&2; exit 2 ;;
esac
case "$physical_core_cpuset" in
    ''|*[!0-9,-]*) [ -z "$physical_core_cpuset" ] || { echo "RISCV_WEIGHT_PHYSICAL_CORE_CPUSET is invalid" >&2; exit 2; } ;;
esac
case "$require_isolation_state" in 0|1) ;; *) echo "RISCV_WEIGHT_REQUIRE_ISOLATION_STATE must be 0 or 1" >&2; exit 2 ;; esac
if [ -n "$isolation_state" ]; then
    case "$isolation_state" in /*) ;; *) isolation_state=$root/$isolation_state ;; esac
    case "$isolation_state" in "$root"/*) ;; *) echo "RISCV_WEIGHT_ISOLATION_STATE must be inside the repository" >&2; exit 2 ;; esac
    [ -r "$isolation_state" ] || { echo "isolation state is not readable" >&2; exit 2; }
fi
command -v "$container_runtime" >/dev/null 2>&1 || {
    echo "container runtime is unavailable: $container_runtime" >&2
    exit 2
}
container_run() {
    # 额外参数由实验编排器预先声明；普通运行默认为空。
    # shellcheck disable=SC2086
    "$container_runtime" run $container_run_arguments "$@"
}
if [ -n "$cpuset" ]; then
    case "$cpuset" in *[!0-9]*) echo "RISCV_WEIGHT_CPUSET must name one logical CPU" >&2; exit 2 ;; esac
    command -v taskset >/dev/null 2>&1 || {
        echo "RISCV_WEIGHT_CPUSET requires taskset on the host" >&2
        exit 2
    }
    taskset -c "$cpuset" true 2>/dev/null || {
        echo "RISCV_WEIGHT_CPUSET is outside the current affinity mask" >&2
        exit 2
    }
    case "$cpuset_mode" in
        auto|cgroup|taskset) ;;
        *) echo "RISCV_WEIGHT_CPUSET_MODE must be auto, cgroup, or taskset" >&2; exit 2 ;;
    esac
    if [ "$cpuset_mode" = auto ]; then
        # shellcheck disable=SC2086
        if container_run --rm \
            --cpuset-cpus "$cpuset" "$image" true \
            >/dev/null 2>&1; then
            cpuset_mode=cgroup
        else
            cpuset_mode=taskset
        fi
    fi
    if [ "$cpuset_mode" = cgroup ]; then
        container_cpuset_arguments="--cpuset-cpus $cpuset"
        qemu_cpuset_arguments=
    else
        container_cpuset_arguments="--cap-add SYS_NICE"
        qemu_cpuset_arguments="taskset -c $cpuset"
    fi
else
    container_cpuset_arguments=
    qemu_cpuset_arguments=
fi
if [ -n "$external_host_audit" ]; then
    case "$external_host_audit" in
        /*) ;;
        *) external_host_audit=$root/$external_host_audit ;;
    esac
    case "$external_host_audit" in
        "$root"/*) ;;
        *) echo "RISCV_WEIGHT_HOST_AUDIT must be inside the repository" >&2; exit 2 ;;
    esac
    [ -r "$external_host_audit" ] || {
        echo "RISCV_WEIGHT_HOST_AUDIT is not readable" >&2
        exit 2
    }
    if [ "$require_exclusive_cpu" -eq 1 ]; then
        echo "formal measurement rejects RISCV_WEIGHT_HOST_AUDIT; audit must be generated from the current run" >&2
        exit 2
    fi
    if [ -n "$cpuset" ]; then
        echo "RISCV_WEIGHT_HOST_AUDIT cannot be combined with current-run telemetry" >&2
        exit 2
    fi
fi
if [ "$require_exclusive_cpu" -eq 1 ] && [ -z "$cpuset" ]; then
    echo "formal measurement requires RISCV_WEIGHT_CPUSET" >&2
    exit 2
fi
case "$ml_validate" in
    0|1) ;;
    *) echo "RISCV_WEIGHT_ML_VALIDATE must be 0 or 1" >&2; exit 2 ;;
esac
case "$linear_algebra_backend" in
    auto|numpy|python) ;;
    *) echo "RISCV_WEIGHT_LINEAR_ALGEBRA_BACKEND must be auto, numpy, or python" >&2; exit 2 ;;
esac
case "$ml_confidence" in
    0.975) ;;
    *) echo "RISCV_WEIGHT_ML_CONFIDENCE must be 0.975 for the registered two-family policy" >&2; exit 2 ;;
esac
case "$mode" in
    calibration-measure)
        if [ "$runs" -ge 205 ]; then
            conformal_train_runs=${conformal_train_runs:-20}
            conformal_calibration_runs=${conformal_calibration_runs:-39}
        else
            conformal_train_runs=${conformal_train_runs:-$((runs / 3))}
            conformal_calibration_runs=${conformal_calibration_runs:-$((runs / 3))}
        fi
        conformal_test_runs=${conformal_test_runs:-$((
            runs - conformal_train_runs - conformal_calibration_runs
        ))}
        ;;
esac
if [ -n "$conformal_train_runs$conformal_calibration_runs$conformal_test_runs" ]; then
    for pair in \
        "RISCV_WEIGHT_CONFORMAL_TRAIN_RUNS:$conformal_train_runs" \
        "RISCV_WEIGHT_CONFORMAL_CALIBRATION_RUNS:$conformal_calibration_runs" \
        "RISCV_WEIGHT_CONFORMAL_TEST_RUNS:$conformal_test_runs"
    do
        name=${pair%%:*}
        value=${pair#*:}
        case "$value" in ''|*[!0-9]*) echo "$name must be a positive integer" >&2; exit 2 ;; esac
        [ "$value" -gt 0 ] || { echo "$name must be a positive integer" >&2; exit 2; }
    done
    [ "$((conformal_train_runs + conformal_calibration_runs + conformal_test_runs))" \
        -eq "$runs" ] || {
        echo "conformal train/calibration/test runs must sum to RISCV_WEIGHT_RUNS" >&2
        exit 2
    }
fi
echo "[riscv-weight] 预检隔离统计 venv"
analysis_python=$("$root/scripts/setup-riscv-instruction-ml-venv.sh")
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
    container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
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
    # rootless Podman 的容器 root 可能映射为 subordinate uid；临时目录默认
    # 0700 会使只读 bind mount 内的 strip 无法遍历。目录仅含随机命名的
    # 一致性检查副本，并在本段结束时立即删除。
    chmod 0777 "$artifact_check"
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
    chmod 0666 "$artifact_check/external-probe"
    # 复用镜像中的交叉 strip，但通过 stdin/stdout 传输，避免 rootless
    # Podman 在 SELinux 主机上拒绝访问 bind mount 中新建的临时 inode。
    container_run --rm -i "$image" sh -c \
        'temporary=$(mktemp); trap '\''rm -f "$temporary"'\'' EXIT; cat >"$temporary"; riscv64-linux-musl-strip "$temporary"; cat "$temporary"' \
        <"$artifact_check/external-probe" \
        >"$artifact_check/external-probe.stripped"
    mv "$artifact_check/external-probe.stripped" \
        "$artifact_check/external-probe"
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
symbols=$(container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
    riscv64-linux-musl-nm -n /work/build/riscv64/instruction-weight/riscv-instruction-weight.elf)
start_pc=$(printf '%s\n' "$symbols" | awk '$3 == "riscv_weight_profile_start" {print "0x" $1}')
stop_pc=$(printf '%s\n' "$symbols" | awk '$3 == "riscv_weight_profile_stop" {print "0x" $1}')
[ "$(printf '%s\n' "$start_pc" | awk 'NF {n++} END {print n+0}')" -eq 1 ] &&
    [ "$(printf '%s\n' "$stop_pc" | awk 'NF {n++} END {print n+0}')" -eq 1 ] &&
    [ "$start_pc" != "$stop_pc" ] || {
        echo "probe start/stop markers are missing or ambiguous" >&2
        exit 1
    }
trap_entry_pc=$(container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
    riscv64-linux-musl-nm -n /work/kernel-rv | \
    awk '$3 == "__riscv_exception_entry" {print "0x" $1}')
[ "$(printf '%s\n' "$trap_entry_pc" | awk 'NF {n++} END {print n+0}')" -eq 1 ] || {
    echo "kernel-rv trap entry symbol is missing or ambiguous" >&2
    exit 1
}

exec_load=$(container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
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

container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
    qemu-system-riscv64 --version >"$output/qemu-version.txt"
container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
    sh -c 'binary=$(command -v qemu-system-riscv64); test -n "$binary"; sha256sum "$binary"' \
    >"$output/qemu-binary.sha256"
sha256sum "$root/kernel-rv" "$probe_elf" \
    "$output/riscv_instruction_weight.so" >"$output/artifacts.sha256"

validation_serial=$output/validation.serial.log
validation_plugin=$output/validation.windows.jsonl
validation_run_id="validation-$run_stamp"
echo "[riscv-weight] QEMU exact-count validation: id=$validation_run_id"
set +e
# shellcheck disable=SC2086
    container_run --rm $container_cpuset_arguments -v "$root:/work$container_mount_suffix" -w /work "$image" sh -c '
        set -eu
        timeout -k 10 "$1" $2 qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$3" -bios default -kernel /work/kernel-rv \
            -m "$4" -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "timer_hz=0 riscv_weight_base_blocks=$5 riscv_weight_rounds=$6 riscv_weight_case=$7 riscv_weight_run_id=$8" \
            -plugin "file=$9,mode=validation,output=${10},start_pc=${11},stop_pc=${12},trap_entry_pc=${13},user_min_pc=${14},user_max_pc=${15}"
    ' run-qemu "$timeout_seconds" "$qemu_cpuset_arguments" "$accel" "$memory" "$base_blocks" "$rounds" \
        "$benchmark_case" "$validation_run_id" \
    "/work/${output#"$root"/}/riscv_instruction_weight.so" \
    "/work/${validation_plugin#"$root"/}" "$start_pc" "$stop_pc" \
    "$trap_entry_pc" "$user_min_pc" "$user_max_pc" >"$validation_serial" 2>&1
status=$?
set -e
if [ "$status" -ne 0 ] || ! tr -d '\r' <"$validation_serial" | \
    grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
    echo "[riscv-weight] validation 未正常完成（qemu status=$status）" >&2
    tail -n 120 "$validation_serial" >&2 || true
    exit 1
fi

timing_merge_arguments=
launch_order_log=$output/launch-order.tsv
run_design_log=$output/run-design.jsonl
host_telemetry=$output/host-telemetry.jsonl
printf 'super_run\tsuper_run_id\tpattern\tposition\tmode\trun_id\n' >"$launch_order_log"
: >"$run_design_log"
: >"$host_telemetry"

telemetry_snapshot() {
    [ -n "$cpuset" ] || return 0
    physical_core_arguments=
    [ -z "$physical_core_cpuset" ] || \
        physical_core_arguments="--physical-core-cpuset $physical_core_cpuset"
    # shellcheck disable=SC2086
    $host_telemetry_command "$root/scripts/riscv_weight_host_telemetry.py" snapshot \
        --output "$host_telemetry" --phase "$1" --launch-id "$launch_id" \
        --super-run-id "$super_run_id" --run-id "$run_id" --mode "$mode" \
        --launch-position "$position" --cpuset "$cpuset" $physical_core_arguments
}

run_startup_warmup() {
    warmup_id="warmup-$run_stamp-$warmup"
    warmup_serial=$output/warmup-$warmup.serial.log
    echo "[riscv-weight] QEMU 启动级预热 $warmup/$startup_warmups: id=$warmup_id"
    set +e
    # shellcheck disable=SC2086
    container_run --rm $container_cpuset_arguments -v "$root:/work$container_mount_suffix" -w /work "$image" sh -c '
        set -eu
        timeout -k 10 "$1" $2 qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$3" -bios default -kernel /work/kernel-rv \
            -m "$4" -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "timer_hz=0 riscv_weight_base_blocks=1 riscv_weight_rounds=1 riscv_weight_case=calibration-v2-long riscv_weight_run_id=$5"
    ' run-qemu "$timeout_seconds" "$qemu_cpuset_arguments" "$accel" "$memory" "$warmup_id" \
        >"$warmup_serial" 2>&1
    status=$?
    set -e
    if [ "$status" -ne 0 ] || ! tr -d '\r' <"$warmup_serial" | \
        grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
        echo "[riscv-weight] 启动级预热未正常完成（qemu status=$status）" >&2
        tail -n 120 "$warmup_serial" >&2 || true
        exit 1
    fi
}

run_timing_qemu() {
    echo "[riscv-weight] QEMU marker-only timing super-run $super_run/$runs: id=$run_id"
    set +e
    launch_id="$super_run_id-$position-timing"
    telemetry_snapshot before
    # shellcheck disable=SC2086
    container_run --rm $container_cpuset_arguments -v "$root:/work$container_mount_suffix" -w /work "$image" sh -c '
        set -eu
        timeout -k 10 "$1" $2 qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$3" -bios default -kernel /work/kernel-rv \
            -m "$4" -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "timer_hz=0 riscv_weight_base_blocks=$5 riscv_weight_rounds=$6 riscv_weight_case=$7 riscv_weight_run_id=$8" \
            -plugin "file=$9,mode=timing,output=${10},start_pc=${11},stop_pc=${12},trap_entry_pc=${13},user_min_pc=${14},user_max_pc=${15}"
    ' run-qemu "$timeout_seconds" "$qemu_cpuset_arguments" "$accel" "$memory" "$base_blocks" "$rounds" \
        "$benchmark_case" "$run_id" "/work/${output#"$root"/}/riscv_instruction_weight.so" \
        "/work/${timing_plugin#"$root"/}" "$start_pc" "$stop_pc" \
        "$trap_entry_pc" "$user_min_pc" "$user_max_pc" >"$serial" 2>&1
    status=$?
    set -e
    telemetry_snapshot after
    if [ "$status" -ne 0 ] || ! tr -d '\r' <"$serial" | \
        grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
        echo "[riscv-weight] timing super-run $super_run 未正常完成（qemu status=$status）" >&2
        tail -n 120 "$serial" >&2 || true
        exit 1
    fi
}

run_plugin_off_qemu() {
    echo "[riscv-weight] QEMU plugin-off cross-check super-run $super_run/$runs: id=$run_id"
    set +e
    launch_id="$super_run_id-$position-plugin-off"
    telemetry_snapshot before
    # shellcheck disable=SC2086
    container_run --rm $container_cpuset_arguments -v "$root:/work$container_mount_suffix" -w /work "$image" sh -c '
        set -eu
        timeout -k 10 "$1" $2 qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$3" -bios default -kernel /work/kernel-rv \
            -m "$4" -smp 1 -nographic -no-reboot -rtc base=utc \
            -append "timer_hz=0 riscv_weight_base_blocks=$5 riscv_weight_rounds=$6 riscv_weight_case=$7 riscv_weight_run_id=$8"
    ' run-qemu "$timeout_seconds" "$qemu_cpuset_arguments" "$accel" "$memory" "$base_blocks" "$rounds" \
        "$benchmark_case" "$run_id" >"$plugin_off_serial" 2>&1
    status=$?
    set -e
    telemetry_snapshot after
    if [ "$status" -ne 0 ] || ! tr -d '\r' <"$plugin_off_serial" | \
        grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'; then
        echo "[riscv-weight] plugin-off super-run $super_run 未正常完成（qemu status=$status）" >&2
        tail -n 120 "$plugin_off_serial" >&2 || true
        exit 1
    fi
}

warmup=1
while [ "$warmup" -le "$startup_warmups" ]; do
    run_startup_warmup
    warmup=$((warmup + 1))
done

if [ "$host_audit_require_frequency_preflight" -eq 1 ]; then
    [ -n "$cpuset" ] && [ -n "$isolation_state" ] || {
        echo "frequency preflight requires cpuset and isolation-state" >&2
        exit 2
    }
    frequency_preflight=$output/frequency-preflight.json
    echo "[riscv-weight] 固定 CPU APERF/MPERF 满载预检"
    taskset -c "$cpuset" $host_telemetry_command \
        "$root/scripts/riscv_weight_host_telemetry.py" \
        frequency-preflight --cpu "$cpuset" --output "$frequency_preflight" \
        --isolation-state "$isolation_state" --duration-seconds 1.0 \
        --minimum-aperf-mperf-ratio 0.95 \
        --minimum-process-busy-fraction 0.90
fi

super_run=1
run_order=0
design_schedule=$output/super-run-design.tsv
python3 - "$runs" "$launch_seed" "$design_schedule" <<'PY'
import random
import sys
from pathlib import Path

runs = int(sys.argv[1])
seed = int(sys.argv[2])
output = Path(sys.argv[3])
generator = random.Random(seed)
patterns = []
for _ in range(runs // 2):
    block = ["ABBA", "BAAB"]
    generator.shuffle(block)
    patterns.extend(block)
if runs % 2:
    patterns.append(generator.choice(("ABBA", "BAAB")))
with output.open("w", encoding="utf-8") as stream:
    stream.write("super_run\tpattern\tseed\n")
    for index, pattern in enumerate(patterns, 1):
        stream.write(f"{index}\t{pattern}\t{seed}\n")
PY
while [ "$super_run" -le "$runs" ]; do
    super_run_id="super-run-$run_stamp-$super_run"
    pattern=$(awk -F '\t' -v run="$super_run" \
        '$1 == run { print $2 }' "$design_schedule")
    case "$pattern" in ABBA|BAAB) ;; *) echo "invalid generated crossover design" >&2; exit 1 ;; esac
    position=1
    while [ "$position" -le 4 ]; do
        case "$pattern:$position" in
            ABBA:1|ABBA:4|BAAB:2|BAAB:3) mode=timing ;;
            *) mode=plugin-off ;;
        esac
        pair=$((1 + (position - 1) / 2))
        run_id="run-$run_stamp-$super_run-$pair"
        if [ "$mode" = timing ]; then
            serial=$output/super-$super_run-position-$position.serial.log
            timing_plugin=$output/super-$super_run-position-$position.timing.jsonl
            run_timing_qemu
            timing_serial=$serial
            timing_plugin_path=$timing_plugin
            timing_position=$position
        else
            plugin_off_serial=$output/super-$super_run-position-$position.plugin-off.serial.log
            run_plugin_off_qemu
            plugin_off_serial_path=$plugin_off_serial
            plugin_off_position=$position
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$super_run" "$super_run_id" "$pattern" "$position" "$mode" "$run_id" \
            >>"$launch_order_log"
        if [ "$((position % 2))" -eq 0 ]; then
            python3 - "$run_design_log" "$run_id" "$run_order" \
                "$super_run_id" "$((super_run - 1))" "$pair" "$pattern" \
                "$timing_position" "$plugin_off_position" <<'PY'
import json
import sys

path, run_id, run_order, super_id, super_order, pair, design, timing, off = sys.argv[1:]
row = {
    "run_id": run_id,
    "run_order": int(run_order),
    "super_run_id": super_id,
    "super_run_order": int(super_order),
    "crossover_pair": int(pair),
    "crossover_design": design,
    "timing_launch_position": int(timing),
    "plugin_off_launch_position": int(off),
}
with open(path, "a", encoding="utf-8") as stream:
    stream.write(json.dumps(row, separators=(",", ":")) + "\n")
PY
            timing_merge_arguments="$timing_merge_arguments --guest /work/${timing_serial#"$root"/} --timing-plugin /work/${timing_plugin_path#"$root"/} --plugin-off-guest /work/${plugin_off_serial_path#"$root"/}"
            run_order=$((run_order + 1))
        fi
        position=$((position + 1))
    done
    super_run=$((super_run + 1))
done

if [ -n "$cpuset" ]; then
    host_audit_arguments=
    host_audit_arguments="--max-interrupts-per-second $host_audit_max_interrupts_per_second"
    host_audit_arguments="$host_audit_arguments --max-runqueue-wait-fraction $host_audit_max_runqueue_wait_fraction"
    [ "$host_audit_require_psi" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-psi"
    [ "$host_audit_require_frequency_floor" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-frequency-floor"
    [ "$host_audit_require_window_frequency" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-window-frequency"
    [ "$host_audit_require_frequency_preflight" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-frequency-preflight"
    [ "$host_audit_require_interrupts" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-interrupts"
    [ "$host_audit_require_schedstat" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-schedstat"
    [ "$require_isolation_state" -eq 0 ] || \
        host_audit_arguments="$host_audit_arguments --require-isolation-state"
    [ -z "$isolation_state" ] || \
        host_audit_arguments="$host_audit_arguments --isolation-state $isolation_state"
    set +e
    # shellcheck disable=SC2086
    $host_telemetry_command "$root/scripts/riscv_weight_host_telemetry.py" audit \
        --input "$host_telemetry" --run-design "$run_design_log" \
        --output "$output/host-audit.json" $host_audit_arguments
    host_audit_status=$?
    set -e
    if [ "$host_audit_status" -ne 0 ] && [ "$require_exclusive_cpu" -eq 1 ]; then
        echo "[riscv-weight] 宿主隔离门禁失败，拒绝拟合；详见 host-audit.json" >&2
        exit 1
    fi
    host_audit=$output/host-audit.json
    host_audit_source=current
elif [ "$require_exclusive_cpu" -eq 1 ]; then
    echo "RISCV_WEIGHT_REQUIRE_EXCLUSIVE_CPU=1 requires RISCV_WEIGHT_CPUSET" >&2
    exit 2
elif [ -n "$external_host_audit" ]; then
    host_audit=$external_host_audit
    host_audit_source=external
fi

if [ -n "$host_audit" ]; then
    host_audit_binding=$output/host-audit-binding.json
    set +e
    "$analysis_python" "$root/scripts/riscv_weight_host_telemetry.py" \
        verify-binding --audit "$host_audit" --input "$host_telemetry" \
        --run-design "$run_design_log" --source "$host_audit_source" \
        --output "$host_audit_binding"
    host_audit_binding_status=$?
    set -e
    if [ "$host_audit_binding_status" -ne 0 ] && \
        [ "$require_exclusive_cpu" -eq 1 ]; then
        echo "[riscv-weight] 宿主审计与当前采集身份不闭合，拒绝拟合；详见 host-audit-binding.json" >&2
        exit 1
    fi
fi

# 参数全部是本脚本生成、且路径已经限制在仓库内。
# shellcheck disable=SC2086
container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
    python3 scripts/merge-riscv-instruction-weight-samples.py \
        --validation-guest "/work/${validation_serial#"$root"/}" \
        --validation-plugin "/work/${validation_plugin#"$root"/}" \
        --run-design "/work/${run_design_log#"$root"/}" \
        $timing_merge_arguments --output "/work/${output#"$root"/}/samples.jsonl"

# Bootstrap 已按 jobs 并行；单 worker 内禁用 BLAS 二次并行，避免过度订阅。
env \
    OPENBLAS_NUM_THREADS=1 \
    OMP_NUM_THREADS=1 \
    MKL_NUM_THREADS=1 \
    BLIS_NUM_THREADS=1 \
    NUMEXPR_NUM_THREADS=1 \
    "$analysis_python" "$root/scripts/rv_instruction_microbench_model.py" \
        "$output/samples.jsonl" \
        --output "$output/weights.json" \
        --csv "$output/weights.csv" \
        --bootstrap "$bootstrap" --jobs "$bootstrap_jobs" \
        --linear-algebra-backend "$linear_algebra_backend"

if [ -n "$host_audit" ]; then
    "$analysis_python" - "$output/weights.json" "$host_audit" \
        "$host_audit_binding" "$host_telemetry" "$run_design_log" \
        "$host_audit_source" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

weights_path, audit_path, binding_path, telemetry_path, design_path = map(
    Path, sys.argv[1:6]
)
source = sys.argv[6]
audit = json.loads(audit_path.read_text(encoding="utf-8"))
binding = json.loads(binding_path.read_text(encoding="utf-8"))
if audit.get("schema") != "mygo.riscv-weight-host-audit.v1":
    raise SystemExit("host audit schema is not supported")
if binding.get("schema") != "mygo.riscv-weight-host-audit-binding.v1":
    raise SystemExit("host audit binding schema is not supported")
if binding.get("source") != source:
    raise SystemExit("host audit binding source mismatch")
binding_inputs = binding.get("inputs")
if not isinstance(binding_inputs, dict):
    raise SystemExit("host audit binding lacks input identities")
for name, path in (
    ("audit", audit_path),
    ("telemetry", telemetry_path),
    ("run_design", design_path),
):
    identity = binding_inputs.get(name)
    if not isinstance(identity, dict):
        raise SystemExit(f"host audit binding lacks {name} identity")
    if hashlib.sha256(path.read_bytes()).hexdigest() != identity.get("sha256"):
        raise SystemExit(f"host audit binding {name} hash mismatch")
document = json.loads(weights_path.read_text(encoding="utf-8"))
document["host_isolation_audit"] = audit
document["host_isolation_audit_binding"] = binding
document["host_isolation_audit_source"] = source
gate = document.setdefault("publication_gate", {})
components = gate.setdefault("components", {})
components["host_isolation"] = (
    source == "current"
    and audit.get("status") == "accepted"
    and binding.get("publication_allowed") is True
)
failures = gate.setdefault("failures", [])
failures[:] = [
    item
    for item in failures
    if item not in {
        "host-isolation-audit-missing",
        "host-isolation-audit-rejected",
        "host-isolation-audit-binding-rejected",
        "external-host-audit-not-publishable",
    }
]
if not components["host_isolation"]:
    failures.append(
        "external-host-audit-not-publishable"
        if source == "external"
        else "host-isolation-audit-binding-rejected"
    )
gate["passed"] = False
weights_path.write_text(
    json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n",
    encoding="utf-8",
)
PY
elif [ "$require_exclusive_cpu" -eq 1 ]; then
    echo "formal measurement requires an accepted host audit" >&2
    exit 1
fi

if [ "$ml_validate" -eq 1 ]; then
    echo "[riscv-weight] 使用隔离 venv 运行机器学习结论校验"
    # ML artifact 绑定的是 finalize 前的统计模型。保留逐字节副本，避免
    # finalize 覆盖 weights.json 后 provenance 只能相信内嵌摘要。
    cp "$output/weights.json" "$output/weights.pre-final.json"
    conformal_arguments=
    if [ -n "$conformal_train_runs" ]; then
        conformal_arguments="--conformal-train-runs $conformal_train_runs --conformal-calibration-runs $conformal_calibration_runs --conformal-test-runs $conformal_test_runs"
    fi
    # 参数均由上面的正整数校验生成。
    # shellcheck disable=SC2086
    set +e
    "$analysis_python" "$root/scripts/validate-riscv-instruction-ml.py" \
        "$output/samples.jsonl" \
        --weights "$output/weights.json" \
        --output "$output/ml-validation.json" \
        --contexts-csv "$output/ml-contexts.csv" \
        --predictions-csv "$output/ml-predictions.csv" \
        --finalize-weights \
        --folds "$ml_folds" --max-iter "$ml_max_iter" \
        --confidence "$ml_confidence" \
        --bootstrap "$ml_bootstrap" --minimum-runs "$ml_minimum_runs" \
        $conformal_arguments
    ml_status=$?
    set -e
    if [ "$ml_status" -ne 0 ]; then
        if [ "$mode" = calibration-measure ]; then
            echo "[riscv-weight] 正式 calibration-measure 的 ML 发布门禁失败" >&2
            exit "$ml_status"
        fi
        echo "[riscv-weight] ML 诊断未通过；保留 weights/ml-validation，继续保持不可发布" >&2
    fi

    provenance_ready=0
    publication_gate_passed=$("$analysis_python" - "$output/weights.json" <<'PY'
import json
import sys

document = json.load(open(sys.argv[1], encoding="utf-8"))
gate = document.get("publication_gate")
print("1" if isinstance(gate, dict) and gate.get("passed") is True else "0")
PY
)
    if [ "$publication_gate_passed" = 1 ] && \
        [ -n "$host_audit" ] && [ "$host_audit_source" = current ] && \
        [ -n "${host_audit_binding:-}" ] && [ -s "$host_audit_binding" ]; then
        provenance_manifest=$output/provenance.json
        isolation_provenance_argument=
        [ -z "$isolation_state" ] || \
            isolation_provenance_argument="--artifact isolation_state=$isolation_state"
        # 所有路径均由本脚本生成或已限制在仓库 root 内。
        # shellcheck disable=SC2086
        "$analysis_python" "$root/scripts/riscv_weight_provenance.py" create \
            --root "$root" --output "$provenance_manifest" \
            --artifact "kernel=$root/kernel-rv" \
            --artifact "probe=$probe_elf" \
            --artifact "plugin=$output/riscv_instruction_weight.so" \
            --artifact "qemu_version=$output/qemu-version.txt" \
            --artifact "qemu_binary_checksum=$output/qemu-binary.sha256" \
            --artifact "artifact_checksums=$output/artifacts.sha256" \
            --artifact "samples=$output/samples.jsonl" \
            --artifact "run_design=$run_design_log" \
            --artifact "host_telemetry=$host_telemetry" \
            --artifact "host_audit=$host_audit" \
            --artifact "host_audit_binding=$host_audit_binding" \
            --artifact "weights_pre_finalization=$output/weights.pre-final.json" \
            --artifact "ml_validation=$output/ml-validation.json" \
            $isolation_provenance_argument
        "$analysis_python" "$root/scripts/riscv_weight_provenance.py" finalize \
            --root "$root" --manifest "$provenance_manifest" \
            --weights "$output/weights.json"
        "$analysis_python" "$root/scripts/riscv_weight_provenance.py" verify \
            --root "$root" --manifest "$provenance_manifest" \
            --weights "$output/weights.json"
        provenance_ready=1
    else
        echo "[riscv-weight] 未获得 current host audit；保留 ML 诊断结果，拒绝 provenance/publication 封印" >&2
    fi
elif [ -n "$catalog" ]; then
    echo "catalog mapping requires a finalized provenance-bound model" >&2
    exit 1
fi

if [ -n "$catalog" ]; then
    if [ "${provenance_ready:-0}" -ne 1 ]; then
        echo "catalog mapping requires a finalized provenance-bound model" >&2
        exit 1
    fi
    container_run --rm -v "$root:/work$container_mount_suffix" -w /work "$image" \
        python3 scripts/map-riscv-instruction-weights.py \
            --catalog "/work/${catalog#"$root"/}" \
            --weights "/work/${output#"$root"/}/weights.json" \
            --provenance-root /work \
            --output "/work/${output#"$root"/}/catalog-weights.json" \
            --csv "/work/${output#"$root"/}/catalog-weights.csv" \
            --expected-key-count "$expected_catalog_keys"
fi

echo "[riscv-weight] 输出目录：$output"
