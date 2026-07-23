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
    python3 - "$path" "$mode" "$kernel" "$progress" "$milestone_ms" "$qemu" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
mode, kernel = sys.argv[2], sys.argv[3]
progress, milestone_ms, qemu = int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
start = 1_000_000_000
data = {
    "schema": "mygo.buildstorm-profile.v2",
    "metadata": {
        "base_sha256": "base", "kernel_sha256": kernel, "qemu_version": "qemu",
        "container_image": "image", "cpuset": "0,2", "duration_ms": "300000",
        "warmup_ms": "0", "stage_anchor": "workload", "poll_ms": "50",
        "host_sample_ms": "1000",
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
    "result": {"workload_ended_early": False, "runner_status": 143},
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

echo "buildstorm profile comparison fixtures: ok"
