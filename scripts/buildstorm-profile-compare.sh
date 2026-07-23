#!/bin/sh
# Validate two repeated BuildStorm measurement groups and enforce noise/overhead limits.
set -eu

max_cv=${PROFILE_MAX_CV_PCT:-5}
max_regression=${PROFILE_MAX_REGRESSION_PCT:-2}
required_speedup=${PROFILE_REQUIRED_SPEEDUP:-1}
max_boundary_ms=${PROFILE_MAX_BOUNDARY_MS:-6000}
max_boundary_pct=${PROFILE_MAX_BOUNDARY_PCT:-2}

[ "$#" -ge 7 ] || {
    echo "usage: $0 BASELINE_SUMMARY BASELINE_SUMMARY BASELINE_SUMMARY -- CANDIDATE_SUMMARY CANDIDATE_SUMMARY CANDIDATE_SUMMARY" >&2
    exit 2
}

python3 - "$max_cv" "$max_regression" "$required_speedup" "$max_boundary_ms" "$max_boundary_pct" "$@" <<'PY'
import json
import math
import pathlib
import statistics
import sys


def fail(message):
    raise ValueError(message)


def number(name, value, minimum=0.0):
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        fail(f"{name} must be numeric")
    if not math.isfinite(parsed) or parsed < minimum:
        fail(f"{name} must be finite and >= {minimum}")
    return parsed


max_cv = number("PROFILE_MAX_CV_PCT", sys.argv[1])
max_regression = number("PROFILE_MAX_REGRESSION_PCT", sys.argv[2])
speedup = number("PROFILE_REQUIRED_SPEEDUP", sys.argv[3], 1.0)
max_boundary_ms = number("PROFILE_MAX_BOUNDARY_MS", sys.argv[4])
max_boundary_pct = number("PROFILE_MAX_BOUNDARY_PCT", sys.argv[5])
paths = sys.argv[6:]
if paths.count("--") != 1:
    fail("arguments must contain exactly one -- separator")
separator = paths.index("--")
groups = (paths[:separator], paths[separator + 1 :])
if min(map(len, groups)) < 3:
    fail("each group requires at least three runs")


def load(path_text):
    path = pathlib.Path(path_text)
    if path.is_dir():
        path = path / "summary.json"
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read {path}: {error}")
    if data.get("schema") != "mygo.buildstorm-profile.v2":
        fail(f"unsupported summary schema in {path}")
    data["_path"] = str(path)
    return data


runs = tuple([load(path) for path in group] for group in groups)
all_runs = runs[0] + runs[1]
stable_metadata = (
    "base_sha256",
    "qemu_version",
    "container_image",
    "cpuset",
    "duration_ms",
    "warmup_ms",
    "stage_anchor",
    "poll_ms",
    "host_sample_ms",
)
reference = all_runs[0]["metadata"]
for run in all_runs:
    metadata = run.get("metadata", {})
    for key in stable_metadata:
        if metadata.get(key) != reference.get(key):
            fail(f"metadata mismatch for {key}: {run['_path']}")
    timing = run.get("timing", {})
    duration_ms = number("duration_ms", metadata.get("duration_ms"), 1.0)
    boundary_limit_ms = min(max_boundary_ms, duration_ms * max_boundary_pct / 100.0)
    for field in ("start_observation_latency_ms", "stop_observation_latency_ms"):
        latency = number(field, timing.get(field))
        if latency > boundary_limit_ms:
            fail(f"{field}={latency:.3f} exceeds {boundary_limit_ms:.3f}: {run['_path']}")
    result = run.get("result", {})
    if result.get("workload_ended_early") and result.get("runner_status") != 0:
        fail(f"early-complete run failed: {run['_path']}")
    profiling = run.get("profiling", {})
    if profiling.get("capture_started") and profiling.get("report_status") != "available":
        fail(f"capture has no valid report: {run['_path']}")

for group in runs:
    kernels = {run["metadata"].get("kernel_sha256") for run in group}
    modes = {run["profiling"].get("mode") for run in group}
    if len(kernels) != 1 or len(modes) != 1:
        fail("kernel and profiling mode must be uniform inside each group")


def cv_pct(values):
    mean = statistics.fmean(values)
    if mean <= 0:
        fail("cannot calculate CV for a zero mean")
    return statistics.pstdev(values) * 100.0 / mean


def milestone_latency(run, milestone):
    stamp = run["timing"]["cargo_progress_monotonic_ns"].get(milestone)
    if stamp is None:
        return None
    return (stamp - run["timing"]["window_start_monotonic_ns"]) / 1_000_000


milestones = ("446", "440", "384", "256", "128", "64")
common_milestone = next(
    (
        milestone
        for milestone in milestones
        if all(milestone_latency(run, milestone) is not None for run in all_runs)
    ),
    None,
)

checks = []
accepted = True
regression_fraction = max_regression / 100.0

if common_milestone is not None:
    baseline_values = [milestone_latency(run, common_milestone) for run in runs[0]]
    candidate_values = [milestone_latency(run, common_milestone) for run in runs[1]]
    direction = "latency"
    baseline_mean = statistics.fmean(baseline_values)
    candidate_mean = statistics.fmean(candidate_values)
    limit = baseline_mean / speedup * (1.0 + regression_fraction)
    performance_ok = candidate_mean <= limit
else:
    def progress_delta(run):
        timing = run["timing"]
        start = max(0, int(timing["window_start_progress"]))
        stop = max(0, int(timing["window_stop_progress"]))
        return max(0, stop - start)

    baseline_values = [progress_delta(run) for run in runs[0]]
    candidate_values = [progress_delta(run) for run in runs[1]]
    direction = "throughput"
    baseline_mean = statistics.fmean(baseline_values)
    candidate_mean = statistics.fmean(candidate_values)
    if baseline_mean <= 0:
        fail("no common milestone and baseline made no progress")
    limit = baseline_mean * speedup * (1.0 - regression_fraction)
    performance_ok = candidate_mean >= limit

baseline_cv = cv_pct(baseline_values)
candidate_cv = cv_pct(candidate_values)
cv_ok = baseline_cv <= max_cv and candidate_cv <= max_cv
accepted &= performance_ok and cv_ok
checks.append(
    {
        "metric": f"cargo_milestone_{common_milestone}" if common_milestone else "cargo_progress_delta",
        "direction": direction,
        "baseline_values": baseline_values,
        "candidate_values": candidate_values,
        "baseline_mean": baseline_mean,
        "candidate_mean": candidate_mean,
        "required_limit": limit,
        "baseline_cv_pct": baseline_cv,
        "candidate_cv_pct": candidate_cv,
        "performance_ok": performance_ok,
        "cv_ok": cv_ok,
    }
)

qemu_groups = tuple(
    [[number("qemu_cpu_seconds", run["host"].get("qemu_cpu_seconds")) for run in group] for group in runs]
)
qemu_cvs = [cv_pct(values) for values in qemu_groups]
qemu_cv_ok = all(value <= max_cv for value in qemu_cvs)
accepted &= qemu_cv_ok
checks.append(
    {
        "metric": "qemu_cpu_seconds",
        "baseline_values": qemu_groups[0],
        "candidate_values": qemu_groups[1],
        "baseline_cv_pct": qemu_cvs[0],
        "candidate_cv_pct": qemu_cvs[1],
        "cv_ok": qemu_cv_ok,
    }
)

report = {
    "schema": "mygo.buildstorm-profile-comparison.v1",
    "accepted": accepted,
    "thresholds": {
        "max_cv_pct": max_cv,
        "max_regression_pct": max_regression,
        "required_speedup": speedup,
        "max_boundary_ms": max_boundary_ms,
        "max_boundary_pct": max_boundary_pct,
    },
    "baseline": [run["_path"] for run in runs[0]],
    "candidate": [run["_path"] for run in runs[1]],
    "checks": checks,
}
print(json.dumps(report, indent=2, sort_keys=True))
if not accepted:
    sys.exit(1)
PY
