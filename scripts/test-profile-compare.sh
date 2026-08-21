#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

make_summary() {
    path=$1
    mode=$2
    kernel=$3
    progress=$4
    milestone_ms=$5
    qemu=$6
    result_kind=${7:-host-qemu-teardown}
    python3 - "$path" "$mode" "$kernel" "$progress" "$milestone_ms" "$qemu" "$result_kind" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
mode, kernel = sys.argv[2], sys.argv[3]
progress, milestone_ms, qemu = int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
result_kind = sys.argv[7]
start = 1_000_000_000
scheduled_deadline = None if result_kind == "natural" else start + 300_000_000_000
measurement_stop = (
    start + 300_000_000_000
    if scheduled_deadline is None
    else scheduled_deadline + 20_000_000
)
stop_request = measurement_stop if scheduled_deadline is not None else measurement_stop - 30_000_000
stop_observed = measurement_stop + 30_000_000 if scheduled_deadline is not None else measurement_stop
if result_kind == "host-qemu-teardown":
    result = {
        "deadline_stop_sent": True,
        "workload_ended_early": False,
        "runner_status": None,
        "runner_status_observed": False,
        "termination_mode": "host-qemu-teardown",
        "stop_requested": True,
        "window_ended_before_stop": False,
        "quiescence_verified": True,
    }
elif result_kind in ("observed-137", "observed-143"):
    result = {
        "deadline_stop_sent": True,
        "workload_ended_early": False,
        "runner_status": int(result_kind.removeprefix("observed-")),
        "runner_status_observed": True,
        "termination_mode": "guest-runner-complete",
        "stop_requested": True,
        "window_ended_before_stop": False,
        "quiescence_verified": True,
    }
elif result_kind == "natural":
    result = {
        "deadline_stop_sent": False,
        "workload_ended_early": True,
        "runner_status": 0,
        "runner_status_observed": True,
        "termination_mode": "guest-runner-complete",
        "stop_requested": False,
        "window_ended_before_stop": True,
        "quiescence_verified": True,
    }
else:
    raise SystemExit(f"unknown result fixture: {result_kind}")
data = {
    "schema": "mygo.profile",
    "schema_version": 2,
    "metadata": {
        "base_sha256": "base", "kernel_sha256": kernel, "arch": "loongarch64",
        "qemu_binary": "qemu-system-loongarch64", "qemu_version": "qemu",
        "container_image": "image", "container_image_id": "sha256:image",
        "container_user": "1000:1000", "cpuset": "0,2", "cpuset_identity": "0,2",
        "duration_ms": "0" if result_kind == "natural" else "300000",
        "done_timeout_ms": "300000",
        "warmup_ms": "0", "stage_anchor": "workload", "poll_ms": "50",
        "host_sample_ms": "1000", "host_clock_ticks_per_second": "100",
        "capture_enabled": "0", "event_mask": "0xfef000000", "event_mask_high": "0x0",
        "sampling_enabled": "0", "trace_enabled": "0", "timing_shift": "8",
        "timing_sampler": "hashed-bernoulli-v1", "guest_boot_mode": "mygo",
        "guest_initramfs_sha256": "initramfs", "guest_workload_device": "/dev/vd0",
        "guest_tools_device": "/dev/vd1", "qemu_machine": "virt",
        "qemu_cpu": "la464", "qemu_bios": "none", "qemu_accel": "tcg,thread=multi",
        "qemu_name": "profile", "qemu_debug_threads": "on",
        "memory": "8G", "memory_bytes": "8589934592", "smp": "8",
        "target_fs": "extfs", "target_triple": "loongarch64-unknown-linux-musl",
        "cold_target": "true", "toolchain": "default",
        "workload_plan_sha256": "plan", "workload_script_sha256": "script",
        "qemu_observer_enabled": "1", "observer_system": "mygo",
        "plugin_sha256": "unavailable", "plugin_period_insns": "50000000",
        "plugin_stack_bytes": "1024", "observer_proc_ms": "1000",
        "symbol_manifest_required": "1", "symbol_manifest_target": "unavailable",
        "symbol_manifest_sha256": f"manifest-{kernel}",
    },
    "timing": {
        "window_start_monotonic_ns": start,
        "window_start_observed_monotonic_ns": start + 20_000_000,
        "elapsed_ms": (measurement_stop - start) / 1_000_000,
        "scheduled_deadline_monotonic_ns": scheduled_deadline,
        "deadline_observation_latency_ms": None if scheduled_deadline is None else 20.0,
        "stop_request_monotonic_ns": stop_request,
        "measurement_stop_monotonic_ns": measurement_stop,
        "stop_monotonic_ns": stop_observed,
        "quiescence_observation_latency_ms": (stop_observed - measurement_stop) / 1_000_000,
        "observer_start_lead_latency_ms": 10.0,
        "observer_stop_lag_latency_ms": 10.0,
        "window_start_progress": 0, "window_stop_progress": progress,
        "start_observation_latency_ms": 20.0, "stop_observation_latency_ms": 30.0,
        "cargo_progress_monotonic_ns": {
            "0": start + 1_000_000, "64": start + milestone_ms * 1_000_000,
            "128": None, "256": None, "384": None, "440": None, "446": None,
        },
    },
    "result": result,
    "profiling": {"mode": mode, "capture_started": mode != "off", "report_status": "available" if mode != "off" else "unavailable"},
    "host": {"qemu_cpu_seconds": qemu},
    "qemu_observer": {
        "capture": {
            "start_monotonic_ns": start - 10_000_000,
            "stop_monotonic_ns": measurement_stop + 10_000_000,
        },
    },
}
path.write_text(json.dumps(data))
PY
}

for index in 1 2 3; do
    make_summary "$tmp/off-$index.json" off off-kernel $((100 + index)) $((100000 + index * 100)) "299.$index"
    make_summary "$tmp/on-$index.json" counts-only profile-kernel $((100 + index)) $((100500 + index * 100)) "299.$index"
done

"$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/off-3.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" >"$tmp/pass.json"
grep -q '"accepted": true' "$tmp/pass.json"

make_summary "$tmp/on-3.json" counts-only profile-kernel 70 130000 299.3
if "$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/off-3.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" >"$tmp/fail.json"; then
    echo "profile comparison fixture: noisy regression was accepted" >&2
    exit 1
fi
grep -q '"accepted": false' "$tmp/fail.json"

make_summary "$tmp/on-3.json" counts-only profile-kernel 103 100800 299.3

reject_metadata_change() {
    field=$1
    value=$2
    output=$3
    cp "$tmp/on-3.json" "$tmp/mismatch.json"
    python3 - "$tmp/mismatch.json" "$field" "$value" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["metadata"][sys.argv[2]] = sys.argv[3]
path.write_text(json.dumps(data))
PY
    if "$repo/scripts/profile-compare.sh" \
        "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/off-3.json" -- \
        "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/mismatch.json" \
        >"$tmp/$output.json" 2>"$tmp/$output.err"; then
        echo "profile comparison fixture: metadata mismatch for $field was accepted" >&2
        exit 1
    fi
    grep -q "metadata mismatch for $field" "$tmp/$output.err"
}

reject_metadata_change guest_initramfs_sha256 other-initramfs initramfs-mismatch
reject_metadata_change workload_script_sha256 other-script workload-script-mismatch
reject_metadata_change arch riscv64 arch-mismatch
reject_metadata_change target_fs tmpfs target-fs-mismatch
reject_metadata_change done_timeout_ms 123 timeout-mismatch

cp "$tmp/off-3.json" "$tmp/deadline-lag.json"
python3 - "$tmp/deadline-lag.json" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
old_stop = data["timing"]["measurement_stop_monotonic_ns"]
new_stop = data["timing"]["scheduled_deadline_monotonic_ns"] + 7_000_000_000
delta = new_stop - old_stop
data["timing"]["deadline_observation_latency_ms"] = 7000.0
data["timing"]["stop_request_monotonic_ns"] = new_stop
data["timing"]["measurement_stop_monotonic_ns"] = new_stop
data["timing"]["stop_monotonic_ns"] += delta
data["timing"]["elapsed_ms"] += delta / 1_000_000
data["qemu_observer"]["capture"]["stop_monotonic_ns"] += delta
path.write_text(json.dumps(data))
PY
if "$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/deadline-lag.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
    >"$tmp/deadline-lag.out" 2>"$tmp/deadline-lag.err"; then
    echo "profile comparison fixture: excessive deadline observation lag was accepted" >&2
    exit 1
fi
grep -q 'deadline_observation_latency_ms=.*exceeds' "$tmp/deadline-lag.err"

cp "$tmp/off-3.json" "$tmp/deadline-race.json"
python3 - "$tmp/deadline-race.json" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["result"]["window_ended_before_stop"] = True
path.write_text(json.dumps(data))
PY
"$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/deadline-race.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
    >"$tmp/deadline-race.out"
grep -q '"accepted": true' "$tmp/deadline-race.out"

cp "$tmp/off-3.json" "$tmp/observer-lag.json"
python3 - "$tmp/observer-lag.json" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["qemu_observer"]["capture"]["stop_monotonic_ns"] += 7_000_000_000
data["timing"]["observer_stop_lag_latency_ms"] += 7000.0
path.write_text(json.dumps(data))
PY
if "$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/observer-lag.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
    >"$tmp/observer-lag.out" 2>"$tmp/observer-lag.err"; then
    echo "profile comparison fixture: excessive observer stop lag was accepted" >&2
    exit 1
fi
grep -q 'observer_stop_lag_latency_ms=.*exceeds' "$tmp/observer-lag.err"

# 派生延迟不能掩盖原始单调时钟字段之间的矛盾。
cp "$tmp/off-3.json" "$tmp/start-timestamp-mismatch.json"
cp "$tmp/off-3.json" "$tmp/elapsed-mismatch.json"
cp "$tmp/off-3.json" "$tmp/deadline-boundary-mismatch.json"
python3 - "$tmp/start-timestamp-mismatch.json" "$tmp/elapsed-mismatch.json" \
    "$tmp/deadline-boundary-mismatch.json" <<'PY'
import json, pathlib, sys

start_path, elapsed_path, deadline_path = map(pathlib.Path, sys.argv[1:])

data = json.loads(start_path.read_text())
data["timing"]["window_start_observed_monotonic_ns"] += 1_000_000
start_path.write_text(json.dumps(data))

data = json.loads(elapsed_path.read_text())
data["timing"]["elapsed_ms"] += 1.0
elapsed_path.write_text(json.dumps(data))

data = json.loads(deadline_path.read_text())
timing = data["timing"]
timing["measurement_stop_monotonic_ns"] += 1_000_000
timing["elapsed_ms"] += 1.0
timing["quiescence_observation_latency_ms"] -= 1.0
timing["observer_stop_lag_latency_ms"] -= 1.0
deadline_path.write_text(json.dumps(data))
PY

reject_timing_summary() {
    summary=$1
    expected=$2
    name=$3
    if "$repo/scripts/profile-compare.sh" \
        "$tmp/off-1.json" "$tmp/off-2.json" "$summary" -- \
        "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
        >"$tmp/$name.out" 2>"$tmp/$name.err"; then
        echo "profile comparison fixture: inconsistent $name timeline was accepted" >&2
        exit 1
    fi
    grep -q "$expected" "$tmp/$name.err"
}

reject_timing_summary "$tmp/start-timestamp-mismatch.json" \
    'start_observation_latency_ms is inconsistent' start-timestamp
reject_timing_summary "$tmp/elapsed-mismatch.json" \
    'elapsed_ms is inconsistent' elapsed
reject_timing_summary "$tmp/deadline-boundary-mismatch.json" \
    'deadline measurement boundary is inconsistent' deadline-boundary

for index in 1 2 3; do
    if [ "$index" -eq 2 ]; then observed=observed-137; else observed=observed-143; fi
    make_summary "$tmp/observed-off-$index.json" off off-kernel $((100 + index)) \
        $((100000 + index * 100)) "299.$index" "$observed"
    make_summary "$tmp/observed-on-$index.json" counts-only profile-kernel $((100 + index)) \
        $((100500 + index * 100)) "299.$index" "$observed"
    make_summary "$tmp/natural-off-$index.json" off off-kernel $((100 + index)) \
        $((100000 + index * 100)) "299.$index" natural
    make_summary "$tmp/natural-on-$index.json" counts-only profile-kernel $((100 + index)) \
        $((100500 + index * 100)) "299.$index" natural
done

cp "$tmp/natural-off-3.json" "$tmp/natural-boundary-mismatch.json"
python3 - "$tmp/natural-boundary-mismatch.json" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
timing = data["timing"]
timing["measurement_stop_monotonic_ns"] -= 1_000_000
timing["elapsed_ms"] -= 1.0
timing["quiescence_observation_latency_ms"] += 1.0
timing["observer_stop_lag_latency_ms"] += 1.0
path.write_text(json.dumps(data))
PY
if "$repo/scripts/profile-compare.sh" \
    "$tmp/natural-off-1.json" "$tmp/natural-off-2.json" \
    "$tmp/natural-boundary-mismatch.json" -- \
    "$tmp/natural-on-1.json" "$tmp/natural-on-2.json" "$tmp/natural-on-3.json" \
    >"$tmp/natural-boundary.out" 2>"$tmp/natural-boundary.err"; then
    echo "profile comparison fixture: inconsistent natural boundary was accepted" >&2
    exit 1
fi
grep -q 'natural measurement boundary is inconsistent' "$tmp/natural-boundary.err"

"$repo/scripts/profile-compare.sh" \
    "$tmp/observed-off-1.json" "$tmp/observed-off-2.json" "$tmp/observed-off-3.json" -- \
    "$tmp/observed-on-1.json" "$tmp/observed-on-2.json" "$tmp/observed-on-3.json" \
    >"$tmp/observed-pass.json"
grep -q '"accepted": true' "$tmp/observed-pass.json"

"$repo/scripts/profile-compare.sh" \
    "$tmp/natural-off-1.json" "$tmp/natural-off-2.json" "$tmp/natural-off-3.json" -- \
    "$tmp/natural-on-1.json" "$tmp/natural-on-2.json" "$tmp/natural-on-3.json" \
    >"$tmp/natural-pass.json"
grep -q '"accepted": true' "$tmp/natural-pass.json"

cp "$tmp/off-3.json" "$tmp/fake-status.json"
python3 - "$tmp/fake-status.json" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["result"]["runner_status"] = 137
path.write_text(json.dumps(data))
PY
if "$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/fake-status.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
    >"$tmp/fake-status.out" 2>"$tmp/fake-status.err"; then
    echo "profile comparison fixture: unobserved runner status was accepted" >&2
    exit 1
fi
grep -q 'runner status observation is inconsistent' "$tmp/fake-status.err"

cp "$tmp/off-3.json" "$tmp/fake-natural.json"
python3 - "$tmp/fake-natural.json" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["result"].update({
    "deadline_stop_sent": False,
    "workload_ended_early": True,
    "stop_requested": False,
    "window_ended_before_stop": True,
})
path.write_text(json.dumps(data))
PY
if "$repo/scripts/profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/fake-natural.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
    >"$tmp/fake-natural.out" 2>"$tmp/fake-natural.err"; then
    echo "profile comparison fixture: natural host teardown was accepted" >&2
    exit 1
fi
grep -q 'early-complete run lacks a successful observed runner status' "$tmp/fake-natural.err"

echo "profile profile comparison fixtures: ok"
