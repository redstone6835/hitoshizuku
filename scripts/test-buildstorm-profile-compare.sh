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
    "schema": "mygo.buildstorm-profile.v2",
    "metadata": {
        "base_sha256": "base", "kernel_sha256": kernel, "qemu_version": "qemu",
        "container_image": "image", "container_image_id": "sha256:image",
        "container_user": "1000:1000", "cpuset": "0,2", "cpuset_identity": "0,2",
        "duration_ms": "300000",
        "warmup_ms": "0", "stage_anchor": "workload", "poll_ms": "50",
        "host_sample_ms": "1000", "host_clock_ticks_per_second": "100",
        "capture_enabled": "0", "event_mask": "0xfef000000",
        "sampling_enabled": "0", "trace_enabled": "0", "timing_shift": "8",
        "timing_sampler": "hashed-bernoulli-v1", "guest_boot_mode": "mygo",
        "guest_initramfs_sha256": "initramfs", "guest_workload_device": "/dev/vd0",
        "guest_tools_device": "/dev/vd1", "qemu_machine": "virt",
        "qemu_cpu": "la464", "qemu_accel": "tcg,thread=multi",
        "qemu_name": "buildstorm-profile", "qemu_debug_threads": "on",
        "memory_bytes": "8589934592", "smp": "8", "target_tmpfs": "size=5G",
        "cold_target": "true", "toolchain": "nightly",
        "workload_plan_sha256": "plan", "workload_script_sha256": "script",
        "qemu_observer_enabled": "0", "observer_system": "mygo",
        "plugin_sha256": "unavailable", "plugin_period_insns": "50000000",
        "plugin_stack_bytes": "1024", "observer_proc_ms": "1000",
        "symbol_manifest_required": "1", "symbol_manifest_target": "unavailable",
        "symbol_manifest_sha256": f"manifest-{kernel}",
    },
    "timing": {
        "window_start_monotonic_ns": start,
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
}
path.write_text(json.dumps(data))
PY
}

for index in 1 2 3; do
    make_summary "$tmp/off-$index.json" off off-kernel $((100 + index)) $((100000 + index * 100)) "299.$index"
    make_summary "$tmp/on-$index.json" counts-only profile-kernel $((100 + index)) $((100500 + index * 100)) "299.$index"
done

"$repo/scripts/buildstorm-profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/off-3.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" >"$tmp/pass.json"
grep -q '"accepted": true' "$tmp/pass.json"

make_summary "$tmp/on-3.json" counts-only profile-kernel 70 130000 299.3
if "$repo/scripts/buildstorm-profile-compare.sh" \
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
    if "$repo/scripts/buildstorm-profile-compare.sh" \
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

"$repo/scripts/buildstorm-profile-compare.sh" \
    "$tmp/observed-off-1.json" "$tmp/observed-off-2.json" "$tmp/observed-off-3.json" -- \
    "$tmp/observed-on-1.json" "$tmp/observed-on-2.json" "$tmp/observed-on-3.json" \
    >"$tmp/observed-pass.json"
grep -q '"accepted": true' "$tmp/observed-pass.json"

"$repo/scripts/buildstorm-profile-compare.sh" \
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
if "$repo/scripts/buildstorm-profile-compare.sh" \
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
if "$repo/scripts/buildstorm-profile-compare.sh" \
    "$tmp/off-1.json" "$tmp/off-2.json" "$tmp/fake-natural.json" -- \
    "$tmp/on-1.json" "$tmp/on-2.json" "$tmp/on-3.json" \
    >"$tmp/fake-natural.out" 2>"$tmp/fake-natural.err"; then
    echo "profile comparison fixture: natural host teardown was accepted" >&2
    exit 1
fi
grep -q 'early-complete run lacks a successful observed runner status' "$tmp/fake-natural.err"

echo "buildstorm profile comparison fixtures: ok"
