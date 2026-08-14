#!/usr/bin/env python3
"""确定性比较两个 RISC-V 指令权重 12-super-run pilot。

10% 的区间收窄和 20% 的 ``tau^2`` 收窄是在已看到旧 pilot、尚未采集
新 pilot 时固定的 prospective 工程筛选条件。它们不构成显著性检验、
覆盖保证或高置信声明；通过后仍必须执行预注册的 205-super-run 正式实验。

旧 pilot 必须先用当前模型、固定 ``--seed 5396035 --bootstrap 4999`` 重拟合到
新目录。比较器拒绝不同 schema、模型、阈值和 bootstrap 合同，防止把分析器
升级误算为测量协议改善。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import shlex
import statistics
import sys
from pathlib import Path
from typing import Any, Mapping, Sequence


COMPARISON_SCHEMA = "mygo.riscv-weight-pilot-comparison.v1"
HOST_AUDIT_SCHEMA = "mygo.riscv-weight-host-audit.v1"
EXPECTED_SUPER_RUNS = 12
PILOT_BOOTSTRAP_REPLICATES = 4999

# 这些阈值是启动新正式实验前的固定工程筛选条件，不是 pilot 的显著性声明。
MAX_MEDIAN_INTERVAL_WIDTH_RATIO = 0.90
MAX_Q90_INTERVAL_WIDTH_RATIO = 1.00
MAX_MEDIAN_TAU_SQUARED_RATIO = 0.80
MAX_Q90_TAU_SQUARED_RATIO = 1.00
MAX_ANCHOR_INTERVAL_WIDTH_RATIO = 0.90
MAX_NUISANCE_INTERVAL_WIDTH_RATIO = 0.90

REQUIRED_PUBLICATION_COMPONENTS = {
    "anchor_adjusted",
    "estimator_sensitivity",
    "joint_bootstrap",
    "positive_anchor",
    "raw",
    "raw_adjusted_discrepancy",
    "single_super_run_influence",
    "statistical_core",
}


class PilotComparisonError(ValueError):
    """输入不是可比较的当前协议 pilot。"""


def _load_json(path: Path, name: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise PilotComparisonError(f"无法读取 {name}: {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise PilotComparisonError(f"{name} 不是合法 JSON: {path}: {error}") from error
    if not isinstance(value, dict):
        raise PilotComparisonError(f"{name} 顶层必须是 object: {path}")
    return value


def _artifact_identity(path: Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    size = 0
    try:
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
                size += len(chunk)
    except OSError as error:
        raise PilotComparisonError(f"无法读取被绑定的产物 {path}: {error}") from error
    return {"path": path.name, "sha256": digest.hexdigest(), "size": size}


def _identity_matches(expected: Mapping[str, Any], actual: Mapping[str, Any]) -> bool:
    if expected.get("sha256") != actual.get("sha256"):
        return False
    size = expected.get("size")
    return size is None or size == actual.get("size")


def _sample_identity_bindings(weights: Mapping[str, Any]) -> list[tuple[str, Mapping[str, Any]]]:
    candidates: list[tuple[str, object]] = []
    direct = weights.get("pilot_comparison_input_bindings")
    if isinstance(direct, Mapping):
        candidates.append(("pilot_comparison_input_bindings", direct.get("samples")))
    for name in ("ml_validation", "ml_validation_evidence"):
        evidence = weights.get(name)
        bindings = evidence.get("input_bindings") if isinstance(evidence, Mapping) else None
        if isinstance(bindings, Mapping):
            candidates.append((f"{name}.input_bindings", bindings.get("samples")))
    result: list[tuple[str, Mapping[str, Any]]] = []
    for source, value in candidates:
        if value is None:
            continue
        if not isinstance(value, Mapping):
            raise PilotComparisonError(f"{source}.samples 必须是 object")
        result.append((source, value))
    return result


def _verify_artifact_bindings(
    root: Path,
    weights: Mapping[str, Any],
    audit: Mapping[str, Any],
) -> dict[str, Any]:
    """复算审计输入和可用样本绑定，禁止移动目录后失去证据链。"""

    inputs = _mapping(audit.get("inputs"), "host_audit.inputs")
    host_inputs: dict[str, Any] = {}
    required = {
        "telemetry": "host-telemetry.jsonl",
        "run_design": "run-design.jsonl",
    }
    for name, filename in required.items():
        expected = _mapping(inputs.get(name), f"host_audit.inputs.{name}")
        declared_path = expected.get("path")
        if not isinstance(declared_path, str) or Path(declared_path).name != filename:
            raise PilotComparisonError(
                f"host_audit.inputs.{name}.path 必须指向 {filename}"
            )
        actual = _artifact_identity(root / filename)
        if not _identity_matches(expected, actual):
            raise PilotComparisonError(f"{filename} 与 host audit SHA-256/size 绑定不一致")
        host_inputs[name] = {**actual, "matches_audit": True}

    isolation = inputs.get("isolation_state")
    if audit.get("isolation_state_checks_required") is True and isolation is None:
        raise PilotComparisonError(
            "host audit 要求 isolation state，但 inputs.isolation_state 缺失"
        )
    if isolation is not None:
        expected = _mapping(isolation, "host_audit.inputs.isolation_state")
        declared_path = expected.get("path")
        filename = "isolation-state.json"
        if not isinstance(declared_path, str) or Path(declared_path).name != filename:
            raise PilotComparisonError(
                "host_audit.inputs.isolation_state.path 必须指向 isolation-state.json"
            )
        actual = _artifact_identity(root / filename)
        if not _identity_matches(expected, actual):
            raise PilotComparisonError(f"{filename} 与 host audit SHA-256/size 绑定不一致")
        host_inputs["isolation_state"] = {**actual, "matches_audit": True}

    samples = _artifact_identity(root / "samples.jsonl")
    sample_bindings = _sample_identity_bindings(weights)
    for source, expected in sample_bindings:
        if not _identity_matches(expected, samples):
            raise PilotComparisonError(
                f"samples.jsonl 与 {source}.samples SHA-256/size 绑定不一致"
            )
    return {
        "host_audit_inputs": host_inputs,
        "samples": {
            **samples,
            "binding_available": bool(sample_bindings),
            "binding_sources": [source for source, _expected in sample_bindings],
            "all_available_bindings_match": True,
        },
    }


def load_pilot(path: str | Path) -> dict[str, Any]:
    """读取 pilot 目录或 ``weights.json``，并同时读取宿主审计。"""

    source = Path(path)
    weights_path = source / "weights.json" if source.is_dir() else source
    audit_path = weights_path.parent / "host-audit.json"
    weights = _load_json(weights_path, "weights.json")
    audit = _load_json(audit_path, "host-audit.json")
    embedded = weights.get("host_isolation_audit")
    if embedded != audit:
        raise PilotComparisonError(
            f"weights.json 内嵌 host_isolation_audit 与 {audit_path} 不一致"
        )
    bindings = _verify_artifact_bindings(weights_path.parent, weights, audit)
    return {
        "root": str(weights_path.parent),
        "weights": weights,
        "host_audit": audit,
        "artifact_bindings": bindings,
    }


def bind_refit_artifacts(path: str | Path) -> dict[str, Any]:
    """把已核验的审计与样本身份嵌入当前模型的重拟合输出。"""

    root = Path(path)
    weights_path = root / "weights.json"
    audit_path = root / "host-audit.json"
    weights = _load_json(weights_path, "weights.json")
    audit = _load_json(audit_path, "host-audit.json")
    bindings = _verify_artifact_bindings(root, weights, audit)
    weights["host_isolation_audit"] = audit
    weights["pilot_comparison_input_bindings"] = {
        "samples": {
            name: bindings["samples"][name]
            for name in ("path", "sha256", "size")
        },
        "purpose": "deterministic old/new pilot refit comparison",
    }
    weights_path.write_text(
        json.dumps(weights, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False)
        + "\n",
        encoding="utf-8",
    )
    return bindings


def _finite(value: object, field: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise PilotComparisonError(f"{field} 缺少有限数值")
    result = float(value)
    if not math.isfinite(result):
        raise PilotComparisonError(f"{field} 不是有限数值")
    return result


def _interval_width(value: object, field: str) -> float:
    if not isinstance(value, list) or len(value) != 2:
        raise PilotComparisonError(f"{field} 必须是二元素区间")
    low = _finite(value[0], f"{field}[0]")
    high = _finite(value[1], f"{field}[1]")
    if high < low:
        raise PilotComparisonError(f"{field} 区间上下界颠倒")
    return high - low


def _mapping(value: object, field: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise PilotComparisonError(f"{field} 必须是 object")
    return value


def _instruction_id(item: Mapping[str, Any]) -> str:
    key = _mapping(item.get("key"), "instructions[].key")
    return json.dumps(key, ensure_ascii=True, sort_keys=True, separators=(",", ":"))


def _instruction_map(weights: Mapping[str, Any], owner: str) -> dict[str, Mapping[str, Any]]:
    rows = weights.get("instructions")
    if not isinstance(rows, list) or not rows:
        raise PilotComparisonError(f"{owner}.instructions 必须是非空数组")
    result: dict[str, Mapping[str, Any]] = {}
    for index, raw in enumerate(rows):
        item = _mapping(raw, f"{owner}.instructions[{index}]")
        key = _instruction_id(item)
        if key in result:
            raise PilotComparisonError(f"{owner}.instructions 存在重复 key: {key}")
        result[key] = item
    return result


def _analysis_contract(weights: Mapping[str, Any]) -> dict[str, Any]:
    simultaneous = _mapping(weights.get("simultaneous_inference"), "simultaneous_inference")
    joint = _mapping(weights.get("joint_raw_adjusted_inference"), "joint_raw_adjusted_inference")
    return {
        "schema_version": weights.get("schema_version"),
        "model": weights.get("model"),
        "confidence": weights.get("confidence"),
        "primary_response": weights.get("primary_response"),
        "instruction_key": weights.get("instruction_key"),
        "linear_algebra_backend": weights.get("linear_algebra_backend"),
        "quality_thresholds": weights.get("quality_thresholds"),
        "bootstrap": {
            name: simultaneous.get(name)
            for name in (
                "automatic_block_length",
                "automatic_run_block_length_rule",
                "block_length",
                "block_length_unit",
                "familywise_confidence",
                "method",
                "minimum_valid_fraction",
                "requested_replicates",
                "run_block_length",
                "run_resampling",
                "super_run_is_highest_cluster",
            )
        },
        "joint_bootstrap": {
            name: joint.get(name)
            for name in (
                "familywise_confidence",
                "method",
                "point_family_size",
                "requested_replicates",
            )
        },
    }


def _validate_current_protocol(
    pilot: Mapping[str, Any], owner: str
) -> tuple[dict[str, Mapping[str, Any]], dict[str, Any]]:
    weights = _mapping(pilot.get("weights"), f"{owner}.weights")
    audit = _mapping(pilot.get("host_audit"), f"{owner}.host_audit")
    if weights.get("host_isolation_audit") != audit:
        raise PilotComparisonError(f"{owner} 的内嵌与外部 host audit 不一致")
    artifact_bindings = _mapping(
        pilot.get("artifact_bindings"), f"{owner}.artifact_bindings"
    )
    sample_binding = _mapping(
        artifact_bindings.get("samples"), f"{owner}.artifact_bindings.samples"
    )
    if (
        sample_binding.get("binding_available") is not True
        or sample_binding.get("all_available_bindings_match") is not True
    ):
        raise PilotComparisonError(
            f"{owner} 的 samples.jsonl 未与重拟合 weights.json 建立 SHA-256 绑定"
        )
    if weights.get("schema_version") != 3:
        raise PilotComparisonError(f"{owner} 必须使用权重 schema v3")
    instructions = _instruction_map(weights, owner)
    for key, item in instructions.items():
        runs = item.get("runs")
        if runs != EXPECTED_SUPER_RUNS:
            raise PilotComparisonError(
                f"{owner} 的 {key} 必须恰有 {EXPECTED_SUPER_RUNS} 个 super-run，实际为 {runs!r}"
            )
        for required in (
            "anchor_adjusted",
            "cross_run_random_effects",
            "estimator_sensitivity",
            "guest_time_check",
            "leave_one_super_run_out_sensitivity",
            "plugin_off_check",
            "raw_adjusted_discrepancy",
        ):
            _mapping(item.get(required), f"{owner}.{key}.{required}")

    gate = _mapping(weights.get("publication_gate"), f"{owner}.publication_gate")
    components = _mapping(gate.get("components"), f"{owner}.publication_gate.components")
    missing_components = sorted(REQUIRED_PUBLICATION_COMPONENTS - set(components))
    if missing_components:
        raise PilotComparisonError(
            f"{owner} 缺少当前模型发布组件，必须用同一版本重新拟合: {missing_components}"
        )
    anchor = _mapping(
        weights.get("positive_anchor_scale_inference"),
        f"{owner}.positive_anchor_scale_inference",
    )
    per_super_run = anchor.get("per_super_run")
    if not isinstance(per_super_run, list) or len(per_super_run) != EXPECTED_SUPER_RUNS:
        raise PilotComparisonError(f"{owner} 的正锚点没有覆盖全部 super-run")
    anchor_super_runs: set[str] = set()
    for index, raw in enumerate(per_super_run):
        row = _mapping(raw, f"{owner}.positive_anchor.per_super_run[{index}]")
        super_run = row.get("super_run")
        if not isinstance(super_run, str) or not super_run:
            raise PilotComparisonError(f"{owner} 的正锚点缺少 super_run 身份")
        anchor_super_runs.add(super_run)
    if len(anchor_super_runs) != EXPECTED_SUPER_RUNS:
        raise PilotComparisonError(f"{owner} 的正锚点 super_run 身份重复")
    simultaneous = _mapping(weights.get("simultaneous_inference"), f"{owner}.simultaneous_inference")
    joint = _mapping(weights.get("joint_raw_adjusted_inference"), f"{owner}.joint_raw_adjusted_inference")
    for prefix, inference in (("primary", simultaneous), ("joint", joint)):
        requested = inference.get("requested_replicates")
        valid = inference.get(
            "complete_max_statistic_replicates",
            inference.get("complete_replicates"),
        )
        if (
            not isinstance(requested, int)
            or requested < PILOT_BOOTSTRAP_REPLICATES
            or valid != requested
        ):
            raise PilotComparisonError(
                f"{owner} 的 {prefix} bootstrap 必须至少有 "
                f"{PILOT_BOOTSTRAP_REPLICATES} 个完整 replicate"
            )
    if audit.get("schema") != HOST_AUDIT_SCHEMA:
        raise PilotComparisonError(f"{owner} 缺少 v1 宿主审计")
    return instructions, dict(audit)


def _ratio(candidate: float, baseline: float, field: str) -> float:
    if candidate < 0.0 or baseline < 0.0:
        raise PilotComparisonError(f"{field} 不能为负数")
    if baseline == 0.0:
        return 1.0 if candidate == 0.0 else math.inf
    return candidate / baseline


def _quantile(values: Sequence[float], probability: float) -> float:
    if not values:
        raise PilotComparisonError("无法对空序列计算分位数")
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * probability
    lower = math.floor(position)
    upper = math.ceil(position)
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def _paired_ratio_summary(
    baseline: Mapping[str, float], candidate: Mapping[str, float]
) -> dict[str, Any]:
    if set(baseline) != set(candidate):
        raise PilotComparisonError("成对指标 key 不一致")
    ratios = {
        key: _ratio(candidate[key], baseline[key], key)
        for key in sorted(baseline)
    }
    values = list(ratios.values())
    return {
        "baseline_median": statistics.median(baseline.values()),
        "candidate_median": statistics.median(candidate.values()),
        "median_paired_ratio": statistics.median(values),
        "q90_paired_ratio": _quantile(values, 0.90),
        "maximum_paired_ratio": max(values),
        "per_instruction_ratio": ratios,
    }


def _status_failures(
    instructions: Mapping[str, Mapping[str, Any]], field: str
) -> dict[str, Any]:
    statuses: dict[str, int] = {}
    failures: list[str] = []
    for key, item in instructions.items():
        status = _mapping(item.get(field), f"{key}.{field}").get("status")
        if not isinstance(status, str):
            raise PilotComparisonError(f"{key}.{field}.status 缺失")
        statuses[status] = statuses.get(status, 0) + 1
        if status != "accepted":
            failures.append(key)
    return {"statuses": dict(sorted(statuses.items())), "failures": sorted(failures)}


def _auxiliary_widths(
    instructions: Mapping[str, Mapping[str, Any]], field: str
) -> dict[str, float]:
    """抽取 ratio/difference 同时区间宽度，并按各自等价带归一化。"""

    result: dict[str, float] = {}
    for key, item in instructions.items():
        check = _mapping(item.get(field), f"{key}.{field}")
        ratio_interval = check.get("simultaneous_ratio_ci")
        difference_interval = check.get("simultaneous_difference_ci")
        present = [
            ("ratio", ratio_interval),
            ("difference", difference_interval),
        ]
        present = [(name, value) for name, value in present if value is not None]
        if len(present) != 1:
            raise PilotComparisonError(
                f"{key}.{field} 必须恰有一种 ratio/difference 同时区间"
            )
        mode, interval = present[0]
        if mode == "ratio":
            accepted = check.get("accepted_ratio_range")
            band_width = _interval_width(
                accepted, f"{key}.{field}.accepted_ratio_range"
            )
        else:
            margin = _finite(
                check.get("zero_cost_absolute_margin_ns"),
                f"{key}.{field}.zero_cost_absolute_margin_ns",
            )
            band_width = 2.0 * margin
        if band_width <= 0.0:
            raise PilotComparisonError(f"{key}.{field} 的等价带宽度必须为正数")
        result[f"{mode}:{key}"] = _interval_width(
            interval, f"{key}.{field}.simultaneous_{mode}_ci"
        ) / band_width
    return result


def _host_evidence(audit: Mapping[str, Any]) -> dict[str, Any]:
    launches = audit.get("launches")
    if not isinstance(launches, list) or len(launches) != EXPECTED_SUPER_RUNS * 4:
        raise PilotComparisonError("候选宿主审计必须包含 48 个 launch")
    thresholds = _mapping(audit.get("thresholds"), "candidate.host_audit.thresholds")
    if thresholds.get("require_window_frequency") is not True:
        raise PilotComparisonError("候选宿主审计未要求 APERF/MPERF 窗口证据")
    if thresholds.get("require_interrupt_evidence") is not True:
        raise PilotComparisonError("候选宿主审计未要求中断证据")
    if audit.get("isolation_state_checks_required") is not True:
        raise PilotComparisonError("候选宿主审计未要求 isolation-state 证据")
    inputs = _mapping(audit.get("inputs"), "candidate.host_audit.inputs")
    if not isinstance(inputs.get("isolation_state"), Mapping):
        raise PilotComparisonError("候选宿主审计未绑定 isolation-state.json")
    failures = audit.get("failures")
    if not isinstance(failures, list):
        raise PilotComparisonError("候选宿主审计 failures 字段非法")
    if audit.get("status") == "accepted" and failures:
        raise PilotComparisonError("候选宿主审计 status=accepted 但仍包含 failures")
    external_rates: list[float] = []
    for index, raw in enumerate(launches):
        launch = _mapping(raw, f"candidate.host_audit.launches[{index}]")
        _finite(launch.get("window_aperf_mperf_ratio"), f"launches[{index}].window_aperf_mperf_ratio")
        external_rates.append(
            _finite(
                launch.get("selected_cpu_external_interrupts_per_second"),
                f"launches[{index}].selected_cpu_external_interrupts_per_second",
            )
        )
    return {
        "status": audit.get("status"),
        "failure_count": len(failures),
        "failures": failures,
        "minimum_window_aperf_mperf_ratio": _finite(
            audit.get("minimum_window_aperf_mperf_ratio"),
            "host_audit.minimum_window_aperf_mperf_ratio",
        ),
        "window_frequency_coefficient_of_variation": _finite(
            audit.get("window_frequency_coefficient_of_variation"),
            "host_audit.window_frequency_coefficient_of_variation",
        ),
        "maximum_external_interrupts_per_second": max(external_rates),
        "temperature_span_c": _finite(
            audit.get("temperature_span_c"), "host_audit.temperature_span_c"
        ),
    }


def compare_pilots(
    baseline: Mapping[str, Any], candidate: Mapping[str, Any]
) -> dict[str, Any]:
    """按预注册工程门禁比较精度，并返回可审计的逐项结果。"""

    baseline_items, baseline_audit = _validate_current_protocol(baseline, "baseline")
    candidate_items, candidate_audit = _validate_current_protocol(candidate, "candidate")
    if set(baseline_items) != set(candidate_items):
        raise PilotComparisonError("新旧 pilot 的指令上下文集合不一致")
    baseline_contract = _analysis_contract(_mapping(baseline["weights"], "baseline.weights"))
    candidate_contract = _analysis_contract(_mapping(candidate["weights"], "candidate.weights"))
    if baseline_contract != candidate_contract:
        raise PilotComparisonError("新旧 pilot 的分析合同不一致，必须用相同模型参数重新拟合")
    baseline_samples = _mapping(
        _mapping(baseline.get("artifact_bindings"), "baseline.artifact_bindings").get("samples"),
        "baseline.artifact_bindings.samples",
    )
    candidate_samples = _mapping(
        _mapping(candidate.get("artifact_bindings"), "candidate.artifact_bindings").get("samples"),
        "candidate.artifact_bindings.samples",
    )
    if baseline_samples.get("sha256") == candidate_samples.get("sha256"):
        raise PilotComparisonError("baseline 与 candidate 绑定了同一个 samples.jsonl，不能冒充独立 pilot")

    def widths(items: Mapping[str, Mapping[str, Any]], adjusted: bool) -> dict[str, float]:
        return {
            key: _interval_width(
                _mapping(item.get("anchor_adjusted"), f"{key}.anchor_adjusted").get("simultaneous_ci")
                if adjusted
                else item.get("simultaneous_ci"),
                f"{key}.{'adjusted_' if adjusted else ''}simultaneous_ci",
            )
            for key, item in items.items()
        }

    def tau_squared(items: Mapping[str, Mapping[str, Any]]) -> dict[str, float]:
        return {
            key: _finite(
                _mapping(item.get("cross_run_random_effects"), f"{key}.cross_run_random_effects").get("tau_squared"),
                f"{key}.cross_run_random_effects.tau_squared",
            )
            for key, item in items.items()
        }

    raw_widths = _paired_ratio_summary(widths(baseline_items, False), widths(candidate_items, False))
    adjusted_widths = _paired_ratio_summary(widths(baseline_items, True), widths(candidate_items, True))
    variance = _paired_ratio_summary(tau_squared(baseline_items), tau_squared(candidate_items))

    baseline_anchor = _mapping(
        _mapping(baseline["weights"], "baseline.weights").get("positive_anchor_scale_inference"),
        "baseline.positive_anchor",
    )
    candidate_anchor = _mapping(
        _mapping(candidate["weights"], "candidate.weights").get("positive_anchor_scale_inference"),
        "candidate.positive_anchor",
    )
    baseline_anchor_intervals = _mapping(baseline_anchor.get("simultaneous_intervals"), "baseline.anchor.intervals")
    candidate_anchor_intervals = _mapping(candidate_anchor.get("simultaneous_intervals"), "candidate.anchor.intervals")
    anchor_name = "plugin_off_to_primary_scale"
    anchor_ratio = _ratio(
        _interval_width(candidate_anchor_intervals.get(anchor_name), f"candidate.anchor.{anchor_name}"),
        _interval_width(baseline_anchor_intervals.get(anchor_name), f"baseline.anchor.{anchor_name}"),
        "positive-anchor interval width",
    )
    baseline_nuisance = _mapping(baseline_anchor.get("nuisance_log_scale_intervals"), "baseline.anchor.nuisance")
    candidate_nuisance = _mapping(candidate_anchor.get("nuisance_log_scale_intervals"), "candidate.anchor.nuisance")
    if set(baseline_nuisance) != set(candidate_nuisance) or not baseline_nuisance:
        raise PilotComparisonError("新旧 pilot 的锚点 nuisance 集合不一致或为空")
    nuisance_ratios = {
        key: _ratio(
            _interval_width(candidate_nuisance[key], f"candidate.nuisance.{key}"),
            _interval_width(baseline_nuisance[key], f"baseline.nuisance.{key}"),
            f"nuisance {key}",
        )
        for key in sorted(baseline_nuisance)
    }

    auxiliary: dict[str, Any] = {}
    auxiliary_non_regression = True
    auxiliary_strict_improvement = False
    auxiliary_width_gates: dict[str, bool] = {}
    for output_name, field in (("cross_clock", "guest_time_check"), ("plugin_off", "plugin_off_check")):
        old = _status_failures(baseline_items, field)
        new = _status_failures(candidate_items, field)
        interval_width = _paired_ratio_summary(
            _auxiliary_widths(baseline_items, field),
            _auxiliary_widths(candidate_items, field),
        )
        old_count = len(old["failures"])
        new_count = len(new["failures"])
        auxiliary[output_name] = {
            "baseline": old,
            "candidate": new,
            "failure_delta": new_count - old_count,
            "interval_width": interval_width,
        }
        auxiliary_non_regression &= new_count <= old_count
        auxiliary_strict_improvement |= new_count < old_count
        auxiliary_width_gates[
            f"{output_name}_interval_median_narrowed_by_10_percent"
        ] = interval_width["median_paired_ratio"] <= MAX_MEDIAN_INTERVAL_WIDTH_RATIO
        auxiliary_width_gates[
            f"{output_name}_interval_q90_not_wider"
        ] = interval_width["q90_paired_ratio"] <= MAX_Q90_INTERVAL_WIDTH_RATIO

    estimator_failures = sorted(
        key
        for key, item in candidate_items.items()
        if _mapping(item.get("estimator_sensitivity"), f"{key}.estimator_sensitivity").get("equivalent") is not True
    )
    baseline_estimator_failures = sorted(
        key
        for key, item in baseline_items.items()
        if _mapping(item.get("estimator_sensitivity"), f"{key}.estimator_sensitivity").get("equivalent") is not True
    )
    influence_values: dict[str, float] = {}
    influence_failures: list[str] = []
    for key, item in candidate_items.items():
        influence = _mapping(item.get("leave_one_super_run_out_sensitivity"), f"{key}.influence")
        margin = _finite(influence.get("equivalence_margin_ns"), f"{key}.influence.margin")
        shift = _finite(influence.get("maximum_absolute_shift_ns"), f"{key}.influence.shift")
        influence_values[key] = math.inf if margin == 0.0 and shift > 0.0 else (0.0 if margin == 0.0 else shift / margin)
        if influence.get("complete") is not True or influence.get("stable") is not True:
            influence_failures.append(key)

    host = _host_evidence(candidate_audit)
    gates = {
        "candidate_host_audit_accepted": host["status"] == "accepted",
        "primary_anchor_accepted": candidate_anchor.get("status") == "accepted",
        "anchor_nuisance_gate_passed": candidate_anchor.get("nuisance_interval_gate_passed") is True,
        "raw_interval_median_narrowed_by_10_percent": raw_widths["median_paired_ratio"] <= MAX_MEDIAN_INTERVAL_WIDTH_RATIO,
        "raw_interval_q90_not_wider": raw_widths["q90_paired_ratio"] <= MAX_Q90_INTERVAL_WIDTH_RATIO,
        "adjusted_interval_median_narrowed_by_10_percent": adjusted_widths["median_paired_ratio"] <= MAX_MEDIAN_INTERVAL_WIDTH_RATIO,
        "adjusted_interval_q90_not_wider": adjusted_widths["q90_paired_ratio"] <= MAX_Q90_INTERVAL_WIDTH_RATIO,
        "super_run_tau_squared_median_reduced_by_20_percent": variance["median_paired_ratio"] <= MAX_MEDIAN_TAU_SQUARED_RATIO,
        "super_run_tau_squared_q90_not_increased": variance["q90_paired_ratio"] <= MAX_Q90_TAU_SQUARED_RATIO,
        "anchor_scale_interval_narrowed_by_10_percent": anchor_ratio <= MAX_ANCHOR_INTERVAL_WIDTH_RATIO,
        "every_nuisance_interval_narrowed_by_10_percent": max(nuisance_ratios.values()) <= MAX_NUISANCE_INTERVAL_WIDTH_RATIO,
        **auxiliary_width_gates,
        "auxiliary_failure_counts_do_not_increase": auxiliary_non_regression,
        "at_least_one_auxiliary_failure_count_decreases": auxiliary_strict_improvement,
        "all_estimator_sensitivity_checks_equivalent": not estimator_failures,
        "all_leave_one_super_run_out_checks_stable": not influence_failures and max(influence_values.values()) <= 1.0,
        "all_raw_adjusted_discrepancies_equivalent": all(
            _mapping(item.get("raw_adjusted_discrepancy"), f"{key}.raw_adjusted_discrepancy").get("equivalent") is True
            for key, item in candidate_items.items()
        ),
    }
    return {
        "schema": COMPARISON_SCHEMA,
        "decision_scope": "engineering pilot gate only; not a high-confidence statistical claim",
        "preregistration_provenance": (
            "thresholds fixed after inspecting the baseline pilot and before "
            "collecting the candidate pilot"
        ),
        "formal_follow_up": (
            "passing authorizes only the preregistered 205-super-run experiment"
        ),
        "accepted_for_formal_run": all(gates.values()),
        "failed_gates": sorted(name for name, passed in gates.items() if not passed),
        "gates": gates,
        "preregistered_thresholds": {
            "expected_super_runs": EXPECTED_SUPER_RUNS,
            "maximum_median_interval_width_ratio": MAX_MEDIAN_INTERVAL_WIDTH_RATIO,
            "maximum_q90_interval_width_ratio": MAX_Q90_INTERVAL_WIDTH_RATIO,
            "maximum_median_tau_squared_ratio": MAX_MEDIAN_TAU_SQUARED_RATIO,
            "maximum_q90_tau_squared_ratio": MAX_Q90_TAU_SQUARED_RATIO,
            "maximum_anchor_interval_width_ratio": MAX_ANCHOR_INTERVAL_WIDTH_RATIO,
            "maximum_each_nuisance_interval_width_ratio": MAX_NUISANCE_INTERVAL_WIDTH_RATIO,
        },
        "analysis_contract": baseline_contract,
        "artifact_bindings": {
            "baseline": baseline.get("artifact_bindings"),
            "candidate": candidate.get("artifact_bindings"),
        },
        "baseline_host_audit_status": baseline_audit.get("status"),
        "candidate_host_evidence": host,
        "metrics": {
            "raw_interval_width": raw_widths,
            "anchor_adjusted_interval_width": adjusted_widths,
            "super_run_tau_squared": variance,
            "positive_anchor_scale_interval_width_ratio": anchor_ratio,
            "nuisance_interval_width_ratios": nuisance_ratios,
            "auxiliary_consistency": auxiliary,
            "estimator_sensitivity": {
                "baseline_non_equivalent": baseline_estimator_failures,
                "candidate_non_equivalent": estimator_failures,
            },
            "leave_one_super_run_out": {
                "candidate_unstable": sorted(influence_failures),
                "candidate_maximum_shift_to_margin_ratio": max(influence_values.values()),
                "candidate_per_instruction_shift_to_margin_ratio": dict(sorted(influence_values.items())),
            },
        },
    }


def render_human(report: Mapping[str, Any]) -> str:
    metrics = _mapping(report.get("metrics"), "report.metrics")
    raw = _mapping(metrics.get("raw_interval_width"), "metrics.raw")
    adjusted = _mapping(metrics.get("anchor_adjusted_interval_width"), "metrics.adjusted")
    variance = _mapping(metrics.get("super_run_tau_squared"), "metrics.variance")
    lines = [
        "RISC-V 指令权重 pilot 比较",
        f"正式实验门禁: {'通过' if report.get('accepted_for_formal_run') else '未通过'}",
        f"raw CI 配对宽度比: median={raw['median_paired_ratio']:.4f}, q90={raw['q90_paired_ratio']:.4f}",
        f"adjusted CI 配对宽度比: median={adjusted['median_paired_ratio']:.4f}, q90={adjusted['q90_paired_ratio']:.4f}",
        f"super-run tau^2 配对比: median={variance['median_paired_ratio']:.4f}, q90={variance['q90_paired_ratio']:.4f}",
        f"失败门禁: {', '.join(report.get('failed_gates', [])) or '无'}",
    ]
    return "\n".join(lines)


def render_refit_commands(baseline: str, candidate: str) -> str:
    """输出同一分析合同的确定性重拟合命令模板。"""

    old = Path(baseline)
    new = Path(candidate)
    old_root = old if old.suffix != ".json" else old.parent
    new_root = new if new.suffix != ".json" else new.parent
    old_q = shlex.quote(str(old_root))
    new_q = shlex.quote(str(new_root))
    return "\n".join(
        (
            "# 使用当前工作树中的同一个分析器；不要覆盖原 pilot。",
            "test ! -e build/riscv-instruction-weight-runs/pilot-refit-old "
            "-a ! -e build/riscv-instruction-weight-runs/pilot-refit-new",
            "mkdir -p build/riscv-instruction-weight-runs/pilot-refit-old "
            "build/riscv-instruction-weight-runs/pilot-refit-new",
            f"cp {old_q}/samples.jsonl {old_q}/host-audit.json "
            f"{old_q}/host-telemetry.jsonl {old_q}/run-design.jsonl "
            "build/riscv-instruction-weight-runs/pilot-refit-old/",
            f"[ ! -f {old_q}/isolation-state.json ] || cp "
            f"{old_q}/isolation-state.json "
            "build/riscv-instruction-weight-runs/pilot-refit-old/",
            f"cp {new_q}/samples.jsonl {new_q}/host-audit.json "
            f"{new_q}/host-telemetry.jsonl {new_q}/run-design.jsonl "
            "build/riscv-instruction-weight-runs/pilot-refit-new/",
            f"[ ! -f {new_q}/isolation-state.json ] || cp "
            f"{new_q}/isolation-state.json "
            "build/riscv-instruction-weight-runs/pilot-refit-new/",
            "python3 scripts/rv_instruction_microbench_model.py "
            "build/riscv-instruction-weight-runs/pilot-refit-old/samples.jsonl --output "
            "build/riscv-instruction-weight-runs/pilot-refit-old/weights.json "
            "--csv build/riscv-instruction-weight-runs/pilot-refit-old/weights.csv "
            "--bootstrap 4999 --jobs 16 --seed 5396035 --linear-algebra-backend numpy",
            "python3 scripts/rv_instruction_microbench_model.py "
            "build/riscv-instruction-weight-runs/pilot-refit-new/samples.jsonl --output "
            "build/riscv-instruction-weight-runs/pilot-refit-new/weights.json "
            "--csv build/riscv-instruction-weight-runs/pilot-refit-new/weights.csv "
            "--bootstrap 4999 --jobs 16 --seed 5396035 --linear-algebra-backend numpy",
            "python3 scripts/compare_riscv_weight_pilots.py "
            "--bind-refit-artifacts build/riscv-instruction-weight-runs/pilot-refit-old",
            "python3 scripts/compare_riscv_weight_pilots.py "
            "--bind-refit-artifacts build/riscv-instruction-weight-runs/pilot-refit-new",
            "python3 scripts/compare_riscv_weight_pilots.py "
            "build/riscv-instruction-weight-runs/pilot-refit-old "
            "build/riscv-instruction-weight-runs/pilot-refit-new --json",
        )
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", nargs="?", help="旧 pilot 目录或 weights.json")
    parser.add_argument("candidate", nargs="?", help="新 pilot 目录或 weights.json")
    parser.add_argument("--json", action="store_true", help="输出完整 JSON")
    parser.add_argument(
        "--print-refit-commands",
        action="store_true",
        help="输出用当前模型重拟合两份 samples.jsonl 的固定命令，不执行",
    )
    parser.add_argument(
        "--bind-refit-artifacts",
        metavar="DIRECTORY",
        help="核验并嵌入重拟合目录的 host audit 与 samples 身份",
    )
    arguments = parser.parse_args(argv)
    if arguments.bind_refit_artifacts:
        if arguments.baseline is not None or arguments.candidate is not None:
            parser.error("--bind-refit-artifacts 不能与比较位置参数同时使用")
        try:
            bindings = bind_refit_artifacts(arguments.bind_refit_artifacts)
        except PilotComparisonError as error:
            print(f"compare_riscv_weight_pilots.py: error: {error}", file=sys.stderr)
            return 2
        print(json.dumps(bindings, ensure_ascii=False, indent=2, sort_keys=True))
        return 0
    if arguments.baseline is None or arguments.candidate is None:
        parser.error("比较模式要求 baseline 和 candidate")
    if arguments.print_refit_commands:
        print(render_refit_commands(arguments.baseline, arguments.candidate))
        return 0
    try:
        report = compare_pilots(load_pilot(arguments.baseline), load_pilot(arguments.candidate))
    except PilotComparisonError as error:
        print(f"compare_riscv_weight_pilots.py: error: {error}", file=sys.stderr)
        return 2
    print(
        json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True)
        if arguments.json
        else render_human(report)
    )
    return 0 if report["accepted_for_formal_run"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
