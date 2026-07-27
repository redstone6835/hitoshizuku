#!/usr/bin/env python3
"""比较两份 QEMU daemon profile 摘要。"""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from pathlib import Path
from typing import Any, Sequence


SUMMARY_SCHEMA = "mygo.qemu-profile.v1"
COMPARISON_SCHEMA = "mygo.qemu-profile-comparison.v1"
REQUIRED_ENVIRONMENT_FIELDS = frozenset(
    {
        "base_image_sha256",
        "cold_target",
        "container_image_id",
        "container_user",
        "cpuset",
        "guest_initramfs_sha256",
        "memory_bytes",
        "plugin_sha256",
        "qemu_accel",
        "qemu_cpu",
        "qemu_debug_threads",
        "qemu_machine",
        "qemu_name",
        "qemu_version",
        "smp",
        "target_tmpfs",
        "toolchain",
        "workload_plan_sha256",
        "workload_script_sha256",
    }
)
SHA256_ENVIRONMENT_FIELDS = frozenset(
    {
        "base_image_sha256",
        "guest_initramfs_sha256",
        "plugin_sha256",
        "workload_plan_sha256",
        "workload_script_sha256",
    }
)
COMPATIBILITY_METADATA_FIELDS = (
    "workload",
    "vcpu_count",
    "proc_interval_ms",
    "stack_interval_ms",
    "stack_timeout_ms",
    "max_frames",
    "max_pause_ratio",
    "plugin_period_insns",
    "plugin_stack_bytes",
    "unwind",
)


class ComparisonError(ValueError):
    """表示摘要无法安全比较。"""


def _mapping(owner: str, value: Any) -> dict[str, Any]:
    """读取 JSON object，并为格式错误补充字段路径。"""

    if not isinstance(value, dict):
        raise ComparisonError(f"{owner} 必须是 JSON object")
    return value


def _sequence(owner: str, value: Any) -> list[Any]:
    """读取 JSON array，并拒绝其他序列类型。"""

    if not isinstance(value, list):
        raise ComparisonError(f"{owner} 必须是 JSON array")
    return value


def _number(
    owner: str,
    value: Any,
    *,
    minimum: float | None = None,
    maximum: float | None = None,
    positive: bool = False,
) -> float:
    """读取有限数值，避免 bool 被 Python 当作整数接受。"""

    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ComparisonError(f"{owner} 必须是数值")
    parsed = float(value)
    if not math.isfinite(parsed):
        raise ComparisonError(f"{owner} 必须是有限数值")
    if positive and parsed <= 0:
        raise ComparisonError(f"{owner} 必须大于 0")
    if minimum is not None and parsed < minimum:
        raise ComparisonError(f"{owner} 必须大于等于 {minimum:g}")
    if maximum is not None and parsed > maximum:
        raise ComparisonError(f"{owner} 必须小于等于 {maximum:g}")
    return parsed


def _integer(owner: str, value: Any, *, minimum: int = 0) -> int:
    """读取无损整数。"""

    parsed = _number(owner, value, minimum=float(minimum))
    if not parsed.is_integer():
        raise ComparisonError(f"{owner} 必须是整数")
    return int(parsed)


def _text(owner: str, value: Any) -> str:
    """读取非空文本字段。"""

    if not isinstance(value, str) or not value.strip():
        raise ComparisonError(f"{owner} 必须是非空字符串")
    return value


def _validate_environment(owner: str, value: Any, vcpu_count: int) -> dict[str, str]:
    """校验公平比较所需的宿主、QEMU、镜像和工作负载身份。"""

    environment = _mapping(owner, value)
    missing = sorted(REQUIRED_ENVIRONMENT_FIELDS - environment.keys())
    if missing:
        raise ComparisonError(f"{owner} 缺少必填字段: {', '.join(missing)}")

    validated: dict[str, str] = {}
    for name, contents in environment.items():
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z][A-Za-z0-9_.-]{0,63}", name):
            raise ComparisonError(f"{owner} 包含非法字段名 {name!r}")
        validated[name] = _text(f"{owner}.{name}", contents)

    for name in SHA256_ENVIRONMENT_FIELDS:
        if not re.fullmatch(r"[0-9a-f]{64}", validated[name]):
            raise ComparisonError(f"{owner}.{name} 必须是 64 位小写 SHA-256")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", validated["container_image_id"]):
        raise ComparisonError(f"{owner}.container_image_id 必须是 sha256:<64 位小写十六进制>")
    if not re.fullmatch(r"[0-9]+:[0-9]+", validated["container_user"]):
        raise ComparisonError(f"{owner}.container_user 必须是十进制 UID:GID")

    for name in ("memory_bytes", "smp"):
        contents = validated[name]
        if not contents.isdecimal() or int(contents) <= 0:
            raise ComparisonError(f"{owner}.{name} 必须是正十进制整数")
    if int(validated["smp"]) != vcpu_count:
        raise ComparisonError(f"{owner}.smp 必须等于 metadata.vcpu_count")
    if validated["cold_target"] != "true":
        raise ComparisonError(f"{owner}.cold_target 必须为 true")
    return validated


def load_summary(path_value: str | Path) -> dict[str, Any]:
    """从文件、runner 目录或独立 daemon 摘要目录加载摘要。"""

    path = Path(path_value)
    if path.is_dir():
        observer_summary = path / "qemu-profile-summary.json"
        path = observer_summary if observer_summary.is_file() else path / "summary.json"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ComparisonError(f"无法读取 {path}: {error}") from error
    summary = _mapping(str(path), data)
    if summary.get("schema") != SUMMARY_SCHEMA:
        raise ComparisonError(f"{path} 使用了不支持的 schema")
    summary = dict(summary)
    summary["_path"] = str(path)
    _validate_summary(summary)
    return summary


def _validate_summary(summary: dict[str, Any]) -> None:
    """校验比较器依赖的字段和内部计数关系。"""

    path = str(summary.get("_path", "summary"))
    metadata = _mapping(f"{path}.metadata", summary.get("metadata"))
    _text(f"{path}.metadata.system", metadata.get("system"))
    _text(f"{path}.metadata.workload", metadata.get("workload"))
    vcpu_count = _integer(
        f"{path}.metadata.vcpu_count", metadata.get("vcpu_count"), minimum=1
    )
    _integer(f"{path}.metadata.proc_interval_ms", metadata.get("proc_interval_ms"), minimum=1)
    _number(
        f"{path}.metadata.stack_interval_ms",
        metadata.get("stack_interval_ms"),
        minimum=0,
    )
    _integer(
        f"{path}.metadata.stack_timeout_ms", metadata.get("stack_timeout_ms"), minimum=1
    )
    _integer(f"{path}.metadata.max_frames", metadata.get("max_frames"), minimum=1)
    _number(
        f"{path}.metadata.max_pause_ratio",
        metadata.get("max_pause_ratio"),
        minimum=0,
        maximum=1,
    )
    _integer(
        f"{path}.metadata.plugin_period_insns",
        metadata.get("plugin_period_insns"),
        minimum=1,
    )
    _integer(
        f"{path}.metadata.plugin_stack_bytes",
        metadata.get("plugin_stack_bytes"),
    )
    _text(f"{path}.metadata.unwind", metadata.get("unwind"))
    for field in (
        "kernel_sha256",
        "symbol_map_sha256",
        "symbol_manifest_sha256",
    ):
        value = _text(f"{path}.metadata.{field}", metadata.get(field))
        if not re.fullmatch(r"[0-9a-f]{64}", value):
            raise ComparisonError(f"{path}.metadata.{field} 必须是 64 位小写 SHA-256")
    _text(
        f"{path}.metadata.symbol_manifest_target",
        metadata.get("symbol_manifest_target"),
    )
    _validate_environment(f"{path}.metadata.environment", metadata.get("environment"), vcpu_count)

    quality = _mapping(f"{path}.quality", summary.get("quality"))
    if quality.get("valid") is not True:
        raise ComparisonError(f"{path}.quality.valid 必须为 true")
    if metadata.get("unwind") == "stack-scan-guess-v1" and quality.get(
        "plugin_exit_reconciled"
    ) is not True:
        raise ComparisonError(f"{path}.quality.plugin_exit_reconciled 必须为 true")
    _number(f"{path}.quality.pause_ratio", quality.get("pause_ratio"), minimum=0, maximum=1)
    samples = _integer(f"{path}.quality.stack_samples", quality.get("stack_samples"))
    successes = _integer(f"{path}.quality.stack_successes", quality.get("stack_successes"))
    if successes > samples:
        raise ComparisonError(f"{path}.quality.stack_successes 不能超过 stack_samples")
    _number(
        f"{path}.quality.symbolized_frame_ratio",
        quality.get("symbolized_frame_ratio"),
        minimum=0,
        maximum=1,
    )

    capture = _mapping(f"{path}.capture", summary.get("capture"))
    wall = _integer(f"{path}.capture.wall_duration_ns", capture.get("wall_duration_ns"), minimum=1)
    paused = _integer(f"{path}.capture.paused_ns", capture.get("paused_ns"))
    if paused > wall:
        raise ComparisonError(f"{path}.capture.paused_ns 不能超过 wall_duration_ns")
    _integer(f"{path}.capture.active_duration_ns", capture.get("active_duration_ns"), minimum=1)
    _integer(f"{path}.capture.qemu_cpu_ticks", capture.get("qemu_cpu_ticks"))
    _number(
        f"{path}.capture.clock_ticks_per_second",
        capture.get("clock_ticks_per_second"),
        positive=True,
    )

    milestones = _mapping(f"{path}.cargo_milestones", summary.get("cargo_milestones"))
    normalized_progress: set[int] = set()
    for progress_text, elapsed in milestones.items():
        if not isinstance(progress_text, str) or not progress_text.isdecimal():
            raise ComparisonError(f"{path}.cargo_milestones 的键必须是非负整数字符串")
        progress = int(progress_text)
        if progress in normalized_progress:
            raise ComparisonError(f"{path}.cargo_milestones 包含重复进度 {progress}")
        normalized_progress.add(progress)
        elapsed_ns = _integer(f"{path}.cargo_milestones[{progress_text!r}]", elapsed)
        if progress > 0 and elapsed_ns == 0:
            raise ComparisonError(f"{path}.cargo_milestones[{progress_text!r}] 必须大于 0")

    stage_values = _sequence(f"{path}.stage_spans", summary.get("stage_spans"))
    for index, stage_value in enumerate(stage_values):
        stage = _mapping(f"{path}.stage_spans[{index}]", stage_value)
        _text(f"{path}.stage_spans[{index}].name", stage.get("name"))
        _integer(
            f"{path}.stage_spans[{index}].active_duration_ns",
            stage.get("active_duration_ns"),
        )

    hotspot_names: set[str] = set()
    for index, hotspot_value in enumerate(_sequence(f"{path}.hotspots", summary.get("hotspots"))):
        hotspot = _mapping(f"{path}.hotspots[{index}]", hotspot_value)
        function = _text(f"{path}.hotspots[{index}].function", hotspot.get("function"))
        if function in hotspot_names:
            raise ComparisonError(f"{path}.hotspots 包含重复函数 {function!r}")
        hotspot_names.add(function)
        _integer(f"{path}.hotspots[{index}].samples", hotspot.get("samples"))
        _number(
            f"{path}.hotspots[{index}].percent",
            hotspot.get("percent"),
            minimum=0,
            maximum=100,
        )


def _normalized_milestones(summary: dict[str, Any]) -> dict[int, int]:
    """把字符串进度转换成可排序的整数键。"""

    return {
        int(progress): int(elapsed)
        for progress, elapsed in summary["cargo_milestones"].items()
    }


def _milestone_comparisons(
    baseline: dict[str, Any], candidate: dict[str, Any]
) -> list[dict[str, Any]]:
    """返回双方均到达的正进度里程碑，按进度升序排列。"""

    baseline_values = _normalized_milestones(baseline)
    candidate_values = _normalized_milestones(candidate)
    common = sorted(set(baseline_values) & set(candidate_values))
    comparisons = []
    for progress in common:
        baseline_ns = baseline_values[progress]
        candidate_ns = candidate_values[progress]
        if progress == 0 or baseline_ns == 0 or candidate_ns == 0:
            continue
        comparisons.append(
            {
                "progress": str(progress),
                "baseline_active_elapsed_ns": baseline_ns,
                "candidate_active_elapsed_ns": candidate_ns,
                "speedup": baseline_ns / candidate_ns,
            }
        )
    return comparisons


def _stage_comparisons(
    baseline: dict[str, Any], candidate: dict[str, Any]
) -> list[dict[str, Any]]:
    """比较同名阶段；零时长阶段保留但不构造无穷比值。"""

    def by_name(summary: dict[str, Any]) -> dict[str, int]:
        result: dict[str, int] = {}
        for stage in summary["stage_spans"]:
            name = stage["name"]
            result[name] = result.get(name, 0) + int(stage["active_duration_ns"])
        return result

    baseline_values = by_name(baseline)
    candidate_values = by_name(candidate)
    comparisons = []
    for name in sorted(set(baseline_values) & set(candidate_values)):
        baseline_ns = baseline_values[name]
        candidate_ns = candidate_values[name]
        comparisons.append(
            {
                "name": name,
                "baseline_active_duration_ns": baseline_ns,
                "candidate_active_duration_ns": candidate_ns,
                "speedup": (
                    baseline_ns / candidate_ns
                    if baseline_ns > 0 and candidate_ns > 0
                    else None
                ),
            }
        )
    return comparisons


def _hotspot_differences(
    baseline: dict[str, Any], candidate: dict[str, Any]
) -> list[dict[str, Any]]:
    """按候选相对基线的绝对占比变化排列热点并集。"""

    def by_function(summary: dict[str, Any]) -> dict[str, dict[str, Any]]:
        return {hotspot["function"]: hotspot for hotspot in summary["hotspots"]}

    baseline_values = by_function(baseline)
    candidate_values = by_function(candidate)
    differences = []
    for function in set(baseline_values) | set(candidate_values):
        baseline_hotspot = baseline_values.get(function, {})
        candidate_hotspot = candidate_values.get(function, {})
        baseline_samples = int(baseline_hotspot.get("samples", 0))
        candidate_samples = int(candidate_hotspot.get("samples", 0))
        baseline_percent = float(baseline_hotspot.get("percent", 0.0))
        candidate_percent = float(candidate_hotspot.get("percent", 0.0))
        differences.append(
            {
                "function": function,
                "baseline_samples": baseline_samples,
                "candidate_samples": candidate_samples,
                "sample_delta": candidate_samples - baseline_samples,
                "baseline_percent": baseline_percent,
                "candidate_percent": candidate_percent,
                "percent_point_delta": candidate_percent - baseline_percent,
            }
        )
    differences.sort(key=lambda item: (-abs(item["percent_point_delta"]), item["function"]))
    return differences


def compare_summaries(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
    *,
    required_speedup: float | None = None,
) -> dict[str, Any]:
    """校验兼容性并生成机器可读比较结果。"""

    _validate_summary(baseline)
    _validate_summary(candidate)
    if required_speedup is not None:
        required_speedup = _number("required_speedup", required_speedup, positive=True)

    baseline_metadata = baseline["metadata"]
    candidate_metadata = candidate["metadata"]
    for field in COMPATIBILITY_METADATA_FIELDS:
        if baseline_metadata[field] != candidate_metadata[field]:
            raise ComparisonError(
                f"metadata.{field} 不一致: "
                f"{baseline_metadata[field]!r} != {candidate_metadata[field]!r}"
            )
    baseline_environment = baseline_metadata["environment"]
    candidate_environment = candidate_metadata["environment"]
    environment_fields = sorted(set(baseline_environment) | set(candidate_environment))
    for field in environment_fields:
        baseline_value = baseline_environment.get(field)
        candidate_value = candidate_environment.get(field)
        if baseline_value != candidate_value:
            raise ComparisonError(
                f"metadata.environment.{field} 不一致: "
                f"{baseline_value!r} != {candidate_value!r}"
            )

    baseline_capture = baseline["capture"]
    candidate_capture = candidate["capture"]
    active_speedup = (
        int(baseline_capture["active_duration_ns"])
        / int(candidate_capture["active_duration_ns"])
    )
    baseline_cpu_seconds = (
        int(baseline_capture["qemu_cpu_ticks"])
        / float(baseline_capture["clock_ticks_per_second"])
    )
    candidate_cpu_seconds = (
        int(candidate_capture["qemu_cpu_ticks"])
        / float(candidate_capture["clock_ticks_per_second"])
    )
    cpu_speedup = (
        baseline_cpu_seconds / candidate_cpu_seconds
        if baseline_cpu_seconds > 0 and candidate_cpu_seconds > 0
        else None
    )

    milestones = _milestone_comparisons(baseline, candidate)
    common_milestone = milestones[-1] if milestones else None
    common_milestone_speedup = common_milestone["speedup"] if common_milestone else None
    if common_milestone is not None:
        gate_metric = f"milestone:{common_milestone['progress']}"
        gate_speedup = common_milestone_speedup
    else:
        gate_metric = "active_duration"
        gate_speedup = active_speedup
    accepted = gate_speedup >= required_speedup if required_speedup is not None else None

    return {
        "schema": COMPARISON_SCHEMA,
        "baseline": {
            "path": baseline.get("_path"),
            "system": baseline_metadata["system"],
        },
        "candidate": {
            "path": candidate.get("_path"),
            "system": candidate_metadata["system"],
        },
        "workload": baseline_metadata["workload"],
        "vcpu_count": int(baseline_metadata["vcpu_count"]),
        "stack_interval_ms": float(baseline_metadata["stack_interval_ms"]),
        "environment": dict(sorted(baseline_environment.items())),
        "profiling": {
            field: baseline_metadata[field]
            for field in COMPATIBILITY_METADATA_FIELDS
            if field not in {"workload", "vcpu_count"}
        },
        "active_duration": {
            "baseline_ns": int(baseline_capture["active_duration_ns"]),
            "candidate_ns": int(candidate_capture["active_duration_ns"]),
        },
        "active_speedup": active_speedup,
        "milestone_speedups": milestones,
        "common_milestone": common_milestone,
        "common_milestone_speedup": common_milestone_speedup,
        "qemu_cpu": {
            "baseline_seconds": baseline_cpu_seconds,
            "candidate_seconds": candidate_cpu_seconds,
        },
        "cpu_speedup": cpu_speedup,
        "stage_speedups": _stage_comparisons(baseline, candidate),
        "hotspot_differences": _hotspot_differences(baseline, candidate),
        "gate_metric": gate_metric,
        "gate_speedup": gate_speedup,
        "required_speedup": required_speedup,
        "accepted": accepted,
    }


def _duration_text(duration_ns: int) -> str:
    """把纳秒时长格式化成紧凑秒数。"""

    return f"{duration_ns / 1_000_000_000:.3f}s"


def render_human(report: dict[str, Any]) -> str:
    """生成适合终端阅读的比较摘要。"""

    baseline = report["baseline"]
    candidate = report["candidate"]
    active = report["active_duration"]
    cpu = report["qemu_cpu"]
    lines = [
        "QEMU profile 比较",
        f"基线: {baseline['system']} ({baseline['path'] or '-'})",
        f"候选: {candidate['system']} ({candidate['path'] or '-'})",
        (
            "活动时长: "
            f"{_duration_text(active['baseline_ns'])} -> "
            f"{_duration_text(active['candidate_ns'])} "
            f"({report['active_speedup']:.3f}x)"
        ),
    ]
    milestone = report["common_milestone"]
    if milestone is None:
        lines.append("共同 Cargo 里程碑: 无")
    else:
        lines.append(
            f"共同 Cargo 里程碑 {milestone['progress']}: "
            f"{_duration_text(milestone['baseline_active_elapsed_ns'])} -> "
            f"{_duration_text(milestone['candidate_active_elapsed_ns'])} "
            f"({milestone['speedup']:.3f}x)"
        )
    cpu_speedup = "n/a" if report["cpu_speedup"] is None else f"{report['cpu_speedup']:.3f}x"
    lines.append(
        f"QEMU CPU: {cpu['baseline_seconds']:.3f}s -> "
        f"{cpu['candidate_seconds']:.3f}s ({cpu_speedup})"
    )

    if report["stage_speedups"]:
        lines.append("共同阶段:")
        for stage in report["stage_speedups"]:
            speedup = "n/a" if stage["speedup"] is None else f"{stage['speedup']:.3f}x"
            lines.append(
                f"  {stage['name']}: "
                f"{_duration_text(stage['baseline_active_duration_ns'])} -> "
                f"{_duration_text(stage['candidate_active_duration_ns'])} ({speedup})"
            )

    if report["hotspot_differences"]:
        lines.append("热点占比变化（候选 - 基线）:")
        for hotspot in report["hotspot_differences"]:
            lines.append(
                f"  {hotspot['percent_point_delta']:+.2f}pp "
                f"{hotspot['function']} "
                f"({hotspot['baseline_percent']:.2f}% -> {hotspot['candidate_percent']:.2f}%)"
            )

    if report["accepted"] is None:
        lines.append(
            f"结论: 仅分析（门禁指标 {report['gate_metric']}，未设置 --required-speedup）"
        )
    elif report["accepted"]:
        lines.append(
            f"结论: 通过（{report['gate_metric']} 要求 >= {report['required_speedup']:.3f}x）"
        )
    else:
        lines.append(
            f"结论: 未通过（{report['gate_metric']} 要求 >= {report['required_speedup']:.3f}x）"
        )
    return "\n".join(lines)


def _positive_float(value: str) -> float:
    """解析 argparse 使用的正有限浮点数。"""

    try:
        parsed = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("必须是数值") from error
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("必须是大于 0 的有限数值")
    return parsed


def build_parser() -> argparse.ArgumentParser:
    """构造命令行解析器。"""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path, help="基线 summary.json 或其所在目录")
    parser.add_argument("candidate", type=Path, help="候选 summary.json 或其所在目录")
    parser.add_argument("--json", action="store_true", help="输出机器可读 JSON")
    parser.add_argument(
        "--required-speedup",
        type=_positive_float,
        help="要求候选最大共同里程碑（无则活动时长）加速比达到该值",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """运行比较器并返回适合 CI 使用的退出码。"""

    args = build_parser().parse_args(argv)
    try:
        baseline = load_summary(args.baseline)
        candidate = load_summary(args.candidate)
        report = compare_summaries(
            baseline,
            candidate,
            required_speedup=args.required_speedup,
        )
    except ComparisonError as error:
        print(f"qemu_profile_compare.py: error: {error}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    else:
        print(render_human(report))
    return 1 if report["accepted"] is False else 0


if __name__ == "__main__":
    raise SystemExit(main())
