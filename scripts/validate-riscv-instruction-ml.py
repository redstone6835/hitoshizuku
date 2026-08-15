#!/usr/bin/env python3
"""用按 crossover super-run 留出的机器学习预测交叉检查指令权重结论。

预测结果只用于发现统计权重的上下文遗漏、非线性和外推失败，不会产生或
覆盖正式权重。严格结论仍由微基准统计模型及其同时区间给出。
"""

from __future__ import annotations

import argparse
import csv
import dataclasses
import hashlib
import json
import math
import random
import re
import statistics
from collections import defaultdict
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from rv_instruction_microbench_model import (
    fit_microbenchmark_weight_model,
    load_samples,
    publication_generation_configuration,
)
from riscv_weight_model_seal import (
    ModelSealError,
    PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES,
    seal_model_document,
    verify_publication_fwer_contract,
)


OUTPUT_SCHEMA = "mygo.riscv-instruction-ml-validation.v3"
PUBLICATION_POLICY_SCHEMA = "mygo.riscv-instruction-ml-publication-policy.v3"
PUBLICATION_FWER_SCHEMA = "mygo.riscv-instruction-ml-familywise-error-control.v1"
PREDICTION_EVIDENCE_SCHEMA = "mygo.riscv-instruction-ml-predictions.v1"
PUBLICATION_SUPER_RUNS = 205
PUBLICATION_MINIMUM_SUPER_RUNS = PUBLICATION_SUPER_RUNS
PUBLICATION_TRAIN_SUPER_RUNS = 20
PUBLICATION_CALIBRATION_SUPER_RUNS = 39
PUBLICATION_TEST_SUPER_RUNS = 146
PUBLICATION_BOOTSTRAP_REPLICATES = 999
PUBLICATION_FOLDS = 6
PUBLICATION_MAX_ITER = 160
PUBLICATION_MINIMUM_RUNS = 20
PUBLICATION_MINIMUM_SKILL_IMPROVEMENT = 0.10
PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS = 0.15
PUBLICATION_EQUIVALENCE_ABSOLUTE_NS = 0.15
PUBLICATION_EQUIVALENCE_RELATIVE = 0.10
PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS = 20
PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS = PUBLICATION_TEST_SUPER_RUNS
PUBLICATION_SEED = 0x4D4C5256
PUBLICATION_OVERALL_ALPHA = 0.05
PUBLICATION_OVERALL_CONFIDENCE = 1.0 - PUBLICATION_OVERALL_ALPHA
PUBLICATION_CONFORMAL_FAMILIES = (
    "random-joint-structural-differential",
    "chronological-joint-structural-differential",
)
PUBLICATION_ALPHA_PER_FAMILY = (
    PUBLICATION_OVERALL_ALPHA / len(PUBLICATION_CONFORMAL_FAMILIES)
)
PUBLICATION_FAMILY_CONFIDENCE = 1.0 - PUBLICATION_ALPHA_PER_FAMILY
DIFFERENTIAL_PREFIX = "diff:"
CALIBRATION_ONLY_SUITES = frozenset({"stability-anchor-v1"})
REQUIRED_PUBLICATION_COMPONENTS = (
    "statistical_core",
    "raw",
    "anchor_adjusted",
    "positive_anchor",
    "raw_adjusted_discrepancy",
    "estimator_sensitivity",
    "single_super_run_influence",
    "joint_bootstrap",
    "host_isolation",
    "ml_validation",
)
REQUIRED_ML_GATE_COMPONENTS = frozenset(
    {
        "random_joint_conformal_family",
        "chronological_joint_conformal_family",
    }
)
REQUIRED_CONFORMAL_CHECKS = frozenset(
    {
        "minimum_train_runs",
        "joint_finite_sample_calibration",
        "joint_informative_interval",
        "independent_joint_test_evidence",
        "differential_conclusion_validation",
    }
)
REQUIRED_CHRONOLOGICAL_CHECKS = REQUIRED_CONFORMAL_CHECKS | {
    "forward_structural_temporal_stability",
    "forward_differential_temporal_stability",
}
STATISTICAL_EXTERNAL_FIELDS = frozenset(
    {
        "artifact_provenance",
        "host_isolation_audit",
        "host_isolation_audit_binding",
        "host_isolation_audit_source",
        "ml_validation",
        "ml_validation_evidence",
        "publication_seal",
    }
)


class MlValidationError(ValueError):
    """输入数据、依赖或交叉验证设计不满足预测校验契约。"""


def _canonical_json_bytes(value: Any) -> bytes:
    """返回用于发布重放比较的唯一有限 JSON 表示。"""

    try:
        return json.dumps(
            value,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise MlValidationError("ML 发布证据不能规范化为有限 JSON") from error


def _artifact_identity(path: Path) -> dict[str, Any]:
    """返回参与校验的不可变输入标识，供最终发布门禁复验。"""

    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
            size += len(chunk)
    return {
        "path": path.name,
        "sha256": digest.hexdigest(),
        "size": size,
    }


def _binding_matches(binding: Any, identity: Mapping[str, Any]) -> bool:
    return (
        isinstance(binding, Mapping)
        and binding.get("sha256") == identity.get("sha256")
        and binding.get("size") == identity.get("size")
    )


def _publication_policy_document() -> dict[str, Any]:
    """返回唯一受支持的、不可由验证产物覆盖的发布政策。"""

    return {
        "schema": PUBLICATION_POLICY_SCHEMA,
        "complete_crossover_super_runs": PUBLICATION_SUPER_RUNS,
        "minimum_complete_crossover_super_runs": PUBLICATION_MINIMUM_SUPER_RUNS,
        "bootstrap_replicates": PUBLICATION_BOOTSTRAP_REPLICATES,
        "minimum_bootstrap_replicates": 999,
        "folds": PUBLICATION_FOLDS,
        "max_iter": PUBLICATION_MAX_ITER,
        "confidence": PUBLICATION_FAMILY_CONFIDENCE,
        "familywise_error_control": _publication_fwer_document(),
        "minimum_independent_super_runs": PUBLICATION_MINIMUM_RUNS,
        "minimum_skill_improvement": PUBLICATION_MINIMUM_SKILL_IMPROVEMENT,
        "omitted_structure_equivalence_ns": (
            PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS
        ),
        "equivalence_absolute_ns": PUBLICATION_EQUIVALENCE_ABSOLUTE_NS,
        "equivalence_relative": PUBLICATION_EQUIVALENCE_RELATIVE,
        "conformal_split": {
            "train_super_runs": PUBLICATION_TRAIN_SUPER_RUNS,
            "calibration_super_runs": PUBLICATION_CALIBRATION_SUPER_RUNS,
            "test_super_runs": PUBLICATION_TEST_SUPER_RUNS,
            "minimum_train_super_runs": (
                PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
            ),
            "minimum_test_super_runs": PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS,
        },
        "formal_ml_gate": {
            "components": sorted(REQUIRED_ML_GATE_COMPONENTS),
            "combination": "all-pre-registered-joint-conformal-families",
            "incremental_prediction_value": (
                "diagnostic-only; fixed-OOF resampling excludes model-training "
                "uncertainty"
            ),
        },
        "seed": PUBLICATION_SEED,
        "verification": "deterministic-full-replay-and-canonical-json-equality",
    }


def _publication_fwer_document() -> dict[str, Any]:
    """返回 random/chronological 联合 conformal 族的独立诊断合同。"""

    return {
        "schema": PUBLICATION_FWER_SCHEMA,
        "method": "bonferroni-across-pre-registered-joint-conformal-families",
        "overall_alpha": PUBLICATION_OVERALL_ALPHA,
        "overall_confidence": PUBLICATION_OVERALL_CONFIDENCE,
        "families": list(PUBLICATION_CONFORMAL_FAMILIES),
        "family_count": len(PUBLICATION_CONFORMAL_FAMILIES),
        "alpha_per_family": PUBLICATION_ALPHA_PER_FAMILY,
        "confidence_per_family": PUBLICATION_FAMILY_CONFIDENCE,
        "within_family_combination": (
            "per-super-run maximum standardized nonconformity across "
            "structural and differential layers"
        ),
        "coverage_claim": (
            "Bonferroni bounds the probability that either pre-registered "
            "joint conformal family misses its complete-super-run target"
        ),
        "scope": "independent-ml-falsification-diagnostic-only",
        "statistical_weight_coverage": "not-proven-or-upgraded-by-ml",
        "combined_overall_confidence_claim": None,
    }


def _diagnostic_fwer_document(confidence: float) -> dict[str, Any]:
    """显式披露普通 CLI 两个 conformal 族的 union-bound 上界。"""

    family_alpha = 1.0 - confidence
    return {
        "schema": PUBLICATION_FWER_SCHEMA,
        "method": "bonferroni-union-bound-summary",
        "families": list(PUBLICATION_CONFORMAL_FAMILIES),
        "family_count": len(PUBLICATION_CONFORMAL_FAMILIES),
        "alpha_per_family": family_alpha,
        "confidence_per_family": confidence,
        "overall_alpha_bound": min(1.0, len(PUBLICATION_CONFORMAL_FAMILIES) * family_alpha),
        "publication_contract": False,
        "scope": "independent-ml-falsification-diagnostic-only",
        "statistical_weight_coverage": "not-proven-or-upgraded-by-ml",
        "combined_overall_confidence_claim": None,
    }


def _prediction_evidence(predictions: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """把未内嵌 JSON 的逐 pair 预测绑定到最终验证产物。"""

    payload = _canonical_json_bytes(list(predictions))
    return {
        "schema": PREDICTION_EVIDENCE_SCHEMA,
        "rows": len(predictions),
        "canonical_payload_size": len(payload),
        "canonical_payload_sha256": hashlib.sha256(payload).hexdigest(),
    }


def _assert_publication_policy(
    validation: Mapping[str, Any], *, run_count: int
) -> None:
    """拒绝样本数不足或由产物自行选择的宽松发布参数。"""

    if run_count != PUBLICATION_SUPER_RUNS:
        raise MlValidationError(
            "ML 发布固定要求 "
            f"{PUBLICATION_SUPER_RUNS} 个完整 crossover super-run"
        )
    if validation.get("publication_policy") != _publication_policy_document():
        raise MlValidationError("ML validation 未绑定受支持的固定发布政策")
    configuration = validation.get("configuration")
    if not isinstance(configuration, Mapping):
        raise MlValidationError("ML validation 缺少发布配置")
    if validation.get("publication_familywise_error_control") != (
        _publication_fwer_document()
    ):
        raise MlValidationError("ML validation 未绑定固定两族 FWER 合同")
    expected_test_runs = PUBLICATION_TEST_SUPER_RUNS
    expected = {
        "folds_requested": PUBLICATION_FOLDS,
        "max_iter": PUBLICATION_MAX_ITER,
        "confidence": PUBLICATION_FAMILY_CONFIDENCE,
        "bootstrap_replicates": PUBLICATION_BOOTSTRAP_REPLICATES,
        "minimum_independent_super_runs": PUBLICATION_MINIMUM_RUNS,
        "minimum_skill_improvement_over_context_batch": (
            PUBLICATION_MINIMUM_SKILL_IMPROVEMENT
        ),
        "omitted_structure_equivalence_ns": (
            PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS
        ),
        "equivalence_absolute_ns": PUBLICATION_EQUIVALENCE_ABSOLUTE_NS,
        "equivalence_relative": PUBLICATION_EQUIVALENCE_RELATIVE,
        "conformal_explicit_run_counts": {
            "train": PUBLICATION_TRAIN_SUPER_RUNS,
            "calibration": PUBLICATION_CALIBRATION_SUPER_RUNS,
            "test": expected_test_runs,
        },
        "conformal_minimum_train_runs": (
            PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
        ),
        "conformal_minimum_test_runs": PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS,
        "seed": PUBLICATION_SEED,
    }
    for name, value in expected.items():
        if configuration.get(name) != value:
            raise MlValidationError(f"ML 发布配置 {name} 不符合固定政策")
    bootstrap = configuration.get("bootstrap_replicates")
    if (
        isinstance(bootstrap, bool)
        or not isinstance(bootstrap, int)
        or bootstrap < 999
    ):
        raise MlValidationError("ML 发布 bootstrap replicate 必须不少于 999")
    prediction_evidence = validation.get("prediction_evidence")
    data = validation.get("data")
    if (
        not isinstance(prediction_evidence, Mapping)
        or prediction_evidence.get("schema") != PREDICTION_EVIDENCE_SCHEMA
        or not isinstance(data, Mapping)
        or prediction_evidence.get("rows") != data.get("pairs")
    ):
        raise MlValidationError("ML validation 缺少完整逐 pair 预测绑定")


def _plain_number(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def _plain_nonnegative_integer(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _same_number(left: Any, right: Any, *, tolerance: float = 1.0e-12) -> bool:
    if not _plain_number(left) or not _plain_number(right):
        return False
    return math.isclose(
        float(left), float(right), rel_tol=tolerance, abs_tol=tolerance
    )


def _finite_interval(value: Any) -> tuple[float, float] | None:
    if (
        not isinstance(value, list)
        or len(value) != 2
        or not all(_plain_number(item) for item in value)
    ):
        return None
    lower, upper = map(float, value)
    return (lower, upper) if lower <= upper else None


def _instruction_identity(item: Mapping[str, Any]) -> tuple[str, str, str]:
    key = item.get("key")
    if not isinstance(key, Mapping):
        raise MlValidationError("weights instruction 缺少 key")
    identity = (
        key.get("semantic_encoding_key"),
        key.get("encoding_key"),
        key.get("pattern"),
    )
    if not all(isinstance(value, str) and value for value in identity):
        raise MlValidationError("weights instruction key 不完整")
    return identity  # type: ignore[return-value]


def _statistical_replay_payload(document: Mapping[str, Any]) -> dict[str, Any]:
    """移除只能由采集/ML/provenance 阶段注入的外部状态。"""

    payload = {
        key: value
        for key, value in document.items()
        if key not in STATISTICAL_EXTERNAL_FIELDS
    }
    # 通过规范化 round-trip 同时深拷贝并拒绝 NaN/不可序列化对象。
    copied = json.loads(_canonical_json_bytes(payload).decode("utf-8"))
    gate = copied.get("publication_gate")
    if not isinstance(gate, dict):
        raise MlValidationError("统计重放 payload 缺少 publication_gate")
    for field in ("passed", "failures", "required_components"):
        gate.pop(field, None)
    components = gate.get("components")
    if not isinstance(components, dict):
        raise MlValidationError("统计重放 payload 缺少 publication_gate.components")
    components.pop("host_isolation", None)
    components.pop("ml_validation", None)
    return copied


def _replay_publication_statistical_model(
    samples: Sequence[Mapping[str, Any]], *, worker_processes: int
) -> dict[str, Any]:
    """按唯一正式配置从原始样本完整重拟合统计权重。"""

    if (
        isinstance(worker_processes, bool)
        or not isinstance(worker_processes, int)
        or worker_processes <= 0
    ):
        raise MlValidationError("统计模型 worker_processes 非法")
    configuration = publication_generation_configuration()
    try:
        return fit_microbenchmark_weight_model(
            samples,
            bootstrap_replicates=configuration["bootstrap_replicates"],
            bootstrap_jobs=worker_processes,
            confidence=configuration["confidence"],
            seed=configuration["seed"],
            block_length=configuration["block_length"],
            run_block_length=configuration["run_block_length"],
            min_pairs=configuration["minimum_pairs"],
            min_effective_pairs=configuration["minimum_effective_pairs"],
            min_runs=configuration["minimum_independent_super_runs"],
            min_count_levels=configuration["minimum_count_levels"],
            min_purity=configuration["minimum_instruction_purity"],
            max_relative_ci_half_width=configuration[
                "maximum_relative_simultaneous_ci_half_width"
            ],
            max_i_squared=configuration["maximum_i_squared"],
            equivalence_margin=configuration["effect_equivalence_margin"],
            min_cross_clock_ratio=configuration["cross_clock_ratio_range"][0],
            max_cross_clock_ratio=configuration["cross_clock_ratio_range"][1],
            min_plugin_off_ratio=configuration["plugin_off_ratio_range"][0],
            max_plugin_off_ratio=configuration["plugin_off_ratio_range"][1],
            max_zero_cost_ci_upper_ns=configuration[
                "maximum_zero_cost_simultaneous_ci_upper_ns"
            ],
            max_translation_density=configuration[
                "maximum_translation_events_per_target_instruction"
            ],
            max_translation_excluded_pair_fraction=configuration[
                "maximum_translation_excluded_pair_fraction"
            ],
            max_severe_outlier_fraction=configuration[
                "maximum_severe_outlier_fraction"
            ],
            linear_algebra_backend=configuration["linear_algebra_backend"],
        )
    except (OSError, ValueError, TypeError, KeyError) as error:
        raise MlValidationError(f"统计模型完整重放失败：{error}") from error


def _verify_statistical_full_replay(
    document: Mapping[str, Any], samples: Sequence[Mapping[str, Any]]
) -> dict[str, Any]:
    """拒绝任何不能从绑定 samples 和固定政策确定性重放的统计字段。"""

    if document.get("generation_configuration") != (
        publication_generation_configuration()
    ):
        raise MlValidationError("统计模型未绑定唯一正式 generation configuration")
    inference = document.get("simultaneous_inference")
    workers = (
        inference.get("worker_processes")
        if isinstance(inference, Mapping)
        else None
    )
    replayed = _replay_publication_statistical_model(
        samples, worker_processes=workers  # type: ignore[arg-type]
    )
    actual_payload = _statistical_replay_payload(document)
    replayed_payload = _statistical_replay_payload(replayed)
    actual_bytes = _canonical_json_bytes(actual_payload)
    replayed_bytes = _canonical_json_bytes(replayed_payload)
    if actual_bytes != replayed_bytes:
        raise MlValidationError(
            "统计 weights 与绑定 samples 在固定发布政策下的完整重放结果不一致"
        )
    return {
        "matched": True,
        "canonical_payload_sha256": hashlib.sha256(replayed_bytes).hexdigest(),
        "canonical_payload_size": len(replayed_bytes),
        "generation_configuration": publication_generation_configuration(),
    }


def _equivalence_from_detail(detail: Any, *, owner: str) -> bool:
    if not isinstance(detail, Mapping):
        raise MlValidationError(f"{owner} 缺少明细")
    interval = _finite_interval(detail.get("simultaneous_ci"))
    margin = detail.get("equivalence_margin_ns")
    if interval is None or not _plain_number(margin) or float(margin) < 0.0:
        expected = False
    else:
        expected = interval[0] >= -float(margin) and interval[1] <= float(margin)
    if detail.get("equivalent") is not expected:
        raise MlValidationError(f"{owner}.equivalent 与区间明细矛盾")
    return expected


def _stable_influence_from_detail(detail: Any) -> bool:
    if not isinstance(detail, Mapping):
        raise MlValidationError("leave-one-super-run-out 缺少明细")
    runs = detail.get("runs")
    per_run = detail.get("per_super_run")
    failed_runs = detail.get("failed_super_runs", [])
    maximum = detail.get("maximum_absolute_shift_ns")
    margin = detail.get("equivalence_margin_ns")
    complete = detail.get("complete") is True
    structurally_complete = (
        _plain_nonnegative_integer(runs)
        and int(runs) > 0
        and isinstance(per_run, list)
        and len(per_run) == runs
        and isinstance(failed_runs, list)
        and not failed_runs
    )
    stable = (
        complete
        and structurally_complete
        and _plain_number(maximum)
        and _plain_number(margin)
        and float(margin) >= 0.0
        and float(maximum) <= float(margin)
    )
    if detail.get("stable") is not stable:
        raise MlValidationError("leave-one-super-run-out stable 与明细矛盾")
    return stable


def _recompute_statistical_components(
    document: Mapping[str, Any],
) -> tuple[dict[str, bool], int, dict[tuple[str, str, str], Mapping[str, Any]]]:
    """从逐指令结果重算统计门禁，拒绝空集和真值缓存篡改。"""

    try:
        verify_publication_fwer_contract(document)
    except ModelSealError as error:
        raise MlValidationError(str(error)) from error
    gate = document.get("publication_gate")
    if not isinstance(gate, Mapping):
        raise MlValidationError("weights JSON 缺少 publication_gate")
    stored = gate.get("components")
    if not isinstance(stored, Mapping):
        raise MlValidationError("weights JSON 缺少 publication_gate.components")
    if set(stored) != set(REQUIRED_PUBLICATION_COMPONENTS):
        raise MlValidationError("publication_gate.components 键集合不符合契约")
    instructions = document.get("instructions")
    if not isinstance(instructions, list) or not instructions:
        raise MlValidationError("weights instructions 必须是非空数组")

    indexed: dict[tuple[str, str, str], Mapping[str, Any]] = {}
    publishable: list[Mapping[str, Any]] = []
    for index, raw_item in enumerate(instructions):
        if not isinstance(raw_item, Mapping):
            raise MlValidationError(f"weights instructions[{index}] 不是 object")
        identity = _instruction_identity(raw_item)
        if identity in indexed:
            raise MlValidationError("weights instructions 含重复稳定键")
        indexed[identity] = raw_item
        should_publish = (
            raw_item.get("quality") == "high-confidence"
            and raw_item.get("calibration_only") is False
        )
        published = raw_item.get("published_ns_per_instruction")
        if should_publish != (published is not None):
            raise MlValidationError("published weight 与逐指令 quality 状态矛盾")
        if should_publish:
            adjusted = raw_item.get("anchor_adjusted")
            if (
                not _plain_number(published)
                or not isinstance(adjusted, Mapping)
                or not _same_number(
                    published, adjusted.get("ns_per_instruction")
                )
            ):
                raise MlValidationError("published weight 与 anchor-adjusted 点估计矛盾")
            publishable.append(raw_item)
    if not publishable:
        raise MlValidationError("weights 没有可发布的非校准指令")

    raw_ok = True
    adjusted_ok = True
    discrepancy_ok = True
    sensitivity_ok = True
    influence_ok = True
    for item in publishable:
        point = item.get("ns_per_instruction")
        interval = _finite_interval(item.get("simultaneous_ci"))
        raw_ok &= _plain_number(point) and interval is not None
        adjusted = item.get("anchor_adjusted")
        adjusted_interval = (
            None
            if not isinstance(adjusted, Mapping)
            else _finite_interval(adjusted.get("simultaneous_ci"))
        )
        adjusted_ok &= (
            isinstance(adjusted, Mapping)
            and _plain_number(adjusted.get("ns_per_instruction"))
            and adjusted_interval is not None
        )
        discrepancy_ok &= _equivalence_from_detail(
            item.get("raw_adjusted_discrepancy"),
            owner="raw-adjusted discrepancy",
        )
        sensitivity_ok &= _equivalence_from_detail(
            item.get("estimator_sensitivity"),
            owner="estimator sensitivity",
        )
        influence_ok &= _stable_influence_from_detail(
            item.get("leave_one_super_run_out_sensitivity")
        )

    anchor = document.get("positive_anchor_scale_inference")
    positive_anchor = isinstance(anchor, Mapping) and anchor.get("status") == "accepted"
    simultaneous = document.get("simultaneous_inference")
    joint = document.get("joint_raw_adjusted_inference")
    requested = (
        simultaneous.get("requested_replicates")
        if isinstance(simultaneous, Mapping)
        else None
    )
    joint_requested = (
        joint.get("requested_replicates") if isinstance(joint, Mapping) else None
    )
    complete = joint.get("complete_replicates") if isinstance(joint, Mapping) else None
    joint_bootstrap = (
        _plain_nonnegative_integer(requested)
        and _plain_nonnegative_integer(joint_requested)
        and _plain_nonnegative_integer(complete)
        and requested == joint_requested
        and requested >= PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES
        and complete == requested
    )
    host_audit = document.get("host_isolation_audit")
    host_binding = document.get("host_isolation_audit_binding")
    host_isolation = (
        isinstance(host_audit, Mapping)
        and host_audit.get("status") == "accepted"
        and document.get("host_isolation_audit_source") == "current"
        and isinstance(host_binding, Mapping)
        and host_binding.get("schema")
        == "mygo.riscv-weight-host-audit-binding.v1"
        and host_binding.get("source") == "current"
        and host_binding.get("publication_allowed") is True
    )
    recomputed = {
        "raw": bool(raw_ok),
        "anchor_adjusted": bool(adjusted_ok),
        "positive_anchor": positive_anchor,
        "raw_adjusted_discrepancy": bool(discrepancy_ok),
        "estimator_sensitivity": bool(sensitivity_ok),
        "single_super_run_influence": bool(influence_ok),
        "joint_bootstrap": bool(joint_bootstrap),
        "host_isolation": host_isolation,
    }
    recomputed["statistical_core"] = all(
        recomputed[name]
        for name in REQUIRED_PUBLICATION_COMPONENTS
        if name not in {"statistical_core", "host_isolation", "ml_validation"}
    )
    for name, expected in recomputed.items():
        if stored.get(name) is not expected:
            raise MlValidationError(f"publication component {name} 与明细矛盾")
    if gate.get("statistical_core_passed") is not recomputed["statistical_core"]:
        raise MlValidationError("statistical_core_passed 与逐指令明细矛盾")
    if gate.get("publishable_contexts") != len(publishable):
        raise MlValidationError("publishable_contexts 与逐指令明细矛盾")
    return recomputed, len(publishable), indexed


def _require_mapping(value: Any, owner: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise MlValidationError(f"ML validation 缺少 {owner}")
    return value


def _require_boolean(value: Any, owner: str) -> bool:
    if not isinstance(value, bool):
        raise MlValidationError(f"ML validation {owner} 不是 boolean")
    return value


def _recompute_temporal_stability(value: Any, *, owner: str) -> bool:
    temporal = _require_mapping(value, owner)
    rows = temporal.get("contexts")
    if not isinstance(rows, list):
        raise MlValidationError(f"{owner}.contexts 不是数组")
    failed = []
    for index, row in enumerate(rows):
        detail = _require_mapping(row, f"{owner}.contexts[{index}]")
        identity = detail.get("identity")
        if not isinstance(identity, list) or not identity:
            raise MlValidationError(f"{owner} temporal identity 非法")
        trend = detail.get("spearman_run_order")
        lag = detail.get("lag1_pearson")
        trend_threshold = detail.get("trend_threshold")
        dependence_threshold = detail.get("dependence_threshold")
        if not _plain_number(trend_threshold) or not _plain_number(
            dependence_threshold
        ):
            raise MlValidationError(f"{owner} temporal threshold 非法")
        trend_flag = _plain_number(trend) and abs(float(trend)) >= float(
            trend_threshold
        )
        dependence_flag = _plain_number(lag) and abs(float(lag)) >= float(
            dependence_threshold
        )
        stable = not (trend_flag or dependence_flag)
        if detail.get("stable") is not stable:
            raise MlValidationError(f"{owner} temporal stable 与相关系数矛盾")
        if not stable:
            failed.append(identity)
    if temporal.get("failed_contexts") != failed:
        raise MlValidationError(f"{owner} failed_contexts 与明细矛盾")
    stable = not failed
    if temporal.get("stable") is not stable:
        raise MlValidationError(f"{owner}.stable 与 context 明细矛盾")
    return stable


def _recompute_standardized_layer(
    value: Any,
    *,
    owner: str,
    calibration_runs: set[str],
    test_runs: set[str],
    confidence: float,
    minimum_test_runs: int,
    differential: bool,
) -> dict[str, bool]:
    layer = _require_mapping(value, owner)
    if differential and layer.get("status") == "unavailable-no-differential-comparisons":
        if (
            layer.get("centers") != []
            or layer.get("finite_sample") != {"gate_passed": True}
            or layer.get("calibration") != {"sharpness_gate_passed": True}
            or layer.get("test") != {"evidence_gate_passed": True}
        ):
            raise MlValidationError(f"{owner} 不适用占位明细非法")
        conclusion = _require_mapping(
            layer.get("conclusion_validation"), f"{owner}.conclusion_validation"
        )
        if (
            conclusion.get("status") != "not-applicable"
            or conclusion.get("gate_passed") is not True
            or conclusion.get("details") != []
        ):
            raise MlValidationError(f"{owner} 不适用 conclusion 非法")
        return {
            "finite": True,
            "informative": True,
            "evidence": True,
            "conclusion": True,
            "temporal": True,
        }

    finite = _require_mapping(layer.get("finite_sample"), f"{owner}.finite_sample")
    calibration = _require_mapping(
        layer.get("calibration"), f"{owner}.calibration"
    )
    test = _require_mapping(layer.get("test"), f"{owner}.test")
    calibration_scores = calibration.get("run_scores")
    test_scores = test.get("run_scores")
    if (
        not isinstance(calibration_scores, Mapping)
        or set(calibration_scores) != calibration_runs
        or not all(_plain_number(score) for score in calibration_scores.values())
        or not isinstance(test_scores, Mapping)
        or set(test_scores) != test_runs
        or not all(_plain_number(score) for score in test_scores.values())
    ):
        raise MlValidationError(f"{owner} run_scores 与 split 不一致")
    ordered_scores = sorted(float(score) for score in calibration_scores.values())
    rank = _conformal_rank(len(ordered_scores), confidence)
    finite_passed = rank <= len(ordered_scores)
    quantile = ordered_scores[rank - 1] if finite_passed else None
    if (
        finite.get("calibration_runs") != len(calibration_runs)
        or finite.get("rank") != rank
        or finite.get("gate_passed") is not finite_passed
        or (
            quantile is not None
            and not _same_number(calibration.get("standardized_quantile"), quantile)
        )
        or (quantile is None and calibration.get("standardized_quantile") is not None)
    ):
        raise MlValidationError(f"{owner} finite conformal 分位数明细矛盾")
    centers = layer.get("centers")
    if not isinstance(centers, list) or not centers:
        raise MlValidationError(f"{owner} 缺少 structural center 明细")
    informative = finite_passed
    for index, row in enumerate(centers):
        detail = _require_mapping(row, f"{owner}.centers[{index}]")
        margin = detail.get("equivalence_margin_ns")
        normalizer = detail.get("conformal_normalizer_ns")
        half_width = detail.get("half_width_ns")
        if (
            not _plain_number(margin)
            or float(margin) <= 0.0
            or not _same_number(normalizer, margin)
        ):
            raise MlValidationError(f"{owner} center scale/margin 非法")
        expected_half_width = (
            None if quantile is None else quantile * float(normalizer)
        )
        if expected_half_width is None:
            if half_width is not None:
                raise MlValidationError(f"{owner} 非有限分位数却存在 half-width")
            informative = False
        elif not _same_number(half_width, expected_half_width):
            raise MlValidationError(f"{owner} half-width 与分位数/尺度矛盾")
        elif float(half_width) > float(margin):
            informative = False
    if calibration.get("sharpness_gate_passed") is not informative:
        raise MlValidationError(f"{owner} sharpness gate 与 center 明细矛盾")

    covered_runs = (
        None
        if quantile is None
        else sum(float(score) <= quantile for score in test_scores.values())
    )
    expected_evidence = _coverage_evidence_gate(
        covered_runs,
        len(test_runs),
        confidence=confidence,
        minimum_test_runs=minimum_test_runs,
    )
    if (
        test.get("runs") != len(test_runs)
        or test.get("covered_runs") != covered_runs
        or test.get("evidence_gate_passed") is not expected_evidence
        or (
            covered_runs is not None
            and not _same_number(
                test.get("run_coverage"), covered_runs / len(test_runs)
            )
        )
    ):
        raise MlValidationError(f"{owner} 独立 test 覆盖证据矛盾")
    temporal = _recompute_temporal_stability(
        layer.get("temporal_diagnostics"),
        owner=f"{owner}.temporal_diagnostics",
    )
    conclusion_passed = True
    if differential:
        conclusion = _require_mapping(
            layer.get("conclusion_validation"), f"{owner}.conclusion_validation"
        )
        details = conclusion.get("details")
        if not isinstance(details, list) or not details:
            raise MlValidationError(f"{owner} 缺少 differential conclusion 明细")
        checks = []
        detail_runs = []
        for row in details:
            detail = _require_mapping(row, f"{owner}.conclusion.details[]")
            actual = detail.get("actual_effect_ns")
            predicted = detail.get("predicted_effect_ns")
            half_width = detail.get("half_width_ns")
            margin = detail.get("equivalence_margin_ns")
            if not all(
                _plain_number(value)
                for value in (actual, predicted, half_width, margin)
            ) or float(half_width) < 0.0 or float(margin) < 0.0:
                raise MlValidationError(
                    f"{owner} differential conclusion 数值明细非法"
                )
            observed, interval_class, covered, check = (
                _evaluate_conformal_conclusion(
                    actual_effect=float(actual),
                    predicted_effect=float(predicted),
                    half_width=float(half_width),
                    margin=float(margin),
                )
            )
            if (
                detail.get("observed_conclusion") != observed
                or detail.get("conformal_interval_conclusion") != interval_class
                or detail.get("actual_covered") is not covered
                or detail.get("conclusion_check") != check
            ):
                raise MlValidationError(
                    f"{owner} differential conclusion 与数值明细矛盾"
                )
            run_id = detail.get("run_id")
            if not isinstance(run_id, str) or run_id not in test_runs:
                raise MlValidationError(
                    f"{owner} differential conclusion 含非 test run"
                )
            detail_runs.append(run_id)
            checks.append(check)
        comparisons = conclusion.get("comparisons_per_run")
        if (
            not isinstance(comparisons, int)
            or isinstance(comparisons, bool)
            or comparisons <= 0
            or len(details) != len(test_runs) * comparisons
            or any(detail_runs.count(run) != comparisons for run in test_runs)
        ):
            raise MlValidationError(
                f"{owner} differential conclusion run/comparison 矩阵不闭合"
            )
        conclusion_passed = (
            len(test_runs) >= minimum_test_runs
            and bool(checks)
            and all(check == "supported" for check in checks)
        )
        expected_status = (
            "supported"
            if conclusion_passed
            else ("contradicted" if "contradicted" in checks else "inconclusive")
        )
        if (
            conclusion.get("test_runs") != len(test_runs)
            or conclusion.get("supported") != checks.count("supported")
            or conclusion.get("inconclusive") != checks.count("inconclusive")
            or conclusion.get("contradicted") != checks.count("contradicted")
            or conclusion.get("gate_passed") is not conclusion_passed
            or conclusion.get("status") != expected_status
        ):
            raise MlValidationError(f"{owner} differential conclusion 汇总矛盾")
    return {
        "finite": finite_passed,
        "informative": informative,
        "evidence": expected_evidence,
        "conclusion": conclusion_passed,
        "temporal": temporal,
    }


def _recompute_differential_conclusion(
    value: Any,
    *,
    owner: str,
    test_runs: set[str],
    minimum_test_runs: int,
    not_applicable: bool,
) -> bool:
    """从联合族逐比较明细重算 differential 结论门禁。"""

    conclusion = _require_mapping(value, owner)
    if not_applicable:
        if (
            conclusion.get("status") != "not-applicable"
            or conclusion.get("gate_passed") is not True
            or conclusion.get("details") != []
        ):
            raise MlValidationError(f"{owner} 不适用 conclusion 非法")
        return True

    details = conclusion.get("details")
    if not isinstance(details, list) or not details:
        raise MlValidationError(f"{owner} 缺少 differential conclusion 明细")
    checks: list[str] = []
    detail_runs: list[str] = []
    for index, row in enumerate(details):
        detail = _require_mapping(row, f"{owner}.details[{index}]")
        values = (
            detail.get("actual_effect_ns"),
            detail.get("predicted_effect_ns"),
            detail.get("half_width_ns"),
            detail.get("equivalence_margin_ns"),
        )
        if (
            not all(_plain_number(value) for value in values)
            or float(values[2]) < 0.0
            or float(values[3]) < 0.0
        ):
            raise MlValidationError(f"{owner} differential conclusion 数值非法")
        observed, interval_class, covered, check = (
            _evaluate_conformal_conclusion(
                actual_effect=float(values[0]),
                predicted_effect=float(values[1]),
                half_width=float(values[2]),
                margin=float(values[3]),
            )
        )
        if (
            detail.get("observed_conclusion") != observed
            or detail.get("conformal_interval_conclusion") != interval_class
            or detail.get("actual_covered") is not covered
            or detail.get("conclusion_check") != check
        ):
            raise MlValidationError(f"{owner} differential conclusion 明细矛盾")
        run_id = detail.get("run_id")
        if not isinstance(run_id, str) or run_id not in test_runs:
            raise MlValidationError(f"{owner} differential conclusion 含非 test run")
        detail_runs.append(run_id)
        checks.append(check)

    comparisons = conclusion.get("comparisons_per_run")
    if (
        not isinstance(comparisons, int)
        or isinstance(comparisons, bool)
        or comparisons <= 0
        or len(details) != len(test_runs) * comparisons
        or any(detail_runs.count(run) != comparisons for run in test_runs)
    ):
        raise MlValidationError(f"{owner} differential conclusion 矩阵不闭合")
    passed = (
        len(test_runs) >= minimum_test_runs
        and bool(checks)
        and all(check == "supported" for check in checks)
    )
    expected_status = (
        "supported"
        if passed
        else ("contradicted" if "contradicted" in checks else "inconclusive")
    )
    if (
        conclusion.get("test_runs") != len(test_runs)
        or conclusion.get("supported") != checks.count("supported")
        or conclusion.get("inconclusive") != checks.count("inconclusive")
        or conclusion.get("contradicted") != checks.count("contradicted")
        or conclusion.get("gate_passed") is not passed
        or conclusion.get("status") != expected_status
    ):
        raise MlValidationError(f"{owner} differential conclusion 汇总矛盾")
    return passed


def _recompute_joint_family(
    value: Any,
    *,
    owner: str,
    structural: Mapping[str, Any],
    differential: Mapping[str, Any],
    calibration_runs: set[str],
    test_runs: set[str],
    confidence: float,
    minimum_test_runs: int,
    chronological: bool,
) -> dict[str, bool]:
    joint = _require_mapping(value, owner)
    expected_family = PUBLICATION_CONFORMAL_FAMILIES[chronological]
    expected_layers = ["structural"]
    if differential.get("status") != "unavailable-no-differential-comparisons":
        expected_layers.append("differential")
    if (
        joint.get("schema")
        != "mygo.riscv-instruction-ml-joint-conformal-family.v1"
        or joint.get("family") != expected_family
        or joint.get("included_layers") != expected_layers
        or not _same_number(joint.get("target_coverage"), confidence)
        or not _same_number(joint.get("alpha"), 1.0 - confidence)
    ):
        raise MlValidationError(f"{owner} 联合族元数据非法")
    calibration = _require_mapping(joint.get("calibration"), f"{owner}.calibration")
    test = _require_mapping(joint.get("test"), f"{owner}.test")
    finite = _require_mapping(joint.get("finite_sample"), f"{owner}.finite_sample")
    calibration_layers = _require_mapping(
        calibration.get("layer_run_scores"), f"{owner}.calibration.layer_run_scores"
    )
    test_layers = _require_mapping(
        test.get("layer_run_scores"), f"{owner}.test.layer_run_scores"
    )
    if list(calibration_layers) != expected_layers or list(test_layers) != expected_layers:
        raise MlValidationError(f"{owner} 联合族 layer 集合非法")

    nested_layers = {"structural": structural, "differential": differential}
    for role, stored_layers, expected_runs in (
        ("calibration", calibration_layers, calibration_runs),
        ("test", test_layers, test_runs),
    ):
        for name in expected_layers:
            nested = _require_mapping(
                _require_mapping(
                    nested_layers[name].get(role), f"{owner}.{name}.{role}"
                ).get("run_scores"),
                f"{owner}.{name}.{role}.run_scores",
            )
            stored = _require_mapping(
                stored_layers.get(name), f"{owner}.{role}.layer_run_scores.{name}"
            )
            if set(stored) != expected_runs or dict(stored) != dict(nested):
                raise MlValidationError(f"{owner} {role} layer score 与明细不一致")

    def maxima(
        layers: Mapping[str, Any], runs: set[str]
    ) -> dict[str, float]:
        return {
            run: max(float(layers[name][run]) for name in expected_layers)
            for run in runs
        }

    calibration_scores = maxima(calibration_layers, calibration_runs)
    test_scores = maxima(test_layers, test_runs)
    if dict(calibration.get("run_scores", {})) != dict(sorted(calibration_scores.items())):
        raise MlValidationError(f"{owner} calibration joint max score 非法")
    if dict(test.get("run_scores", {})) != dict(sorted(test_scores.items())):
        raise MlValidationError(f"{owner} test joint max score 非法")
    ordered_scores = sorted(calibration_scores.values())
    rank = _conformal_rank(len(ordered_scores), confidence)
    finite_passed = rank <= len(ordered_scores)
    quantile = ordered_scores[rank - 1] if finite_passed else None
    if (
        finite.get("calibration_runs") != len(calibration_runs)
        or finite.get("rank") != rank
        or finite.get("gate_passed") is not finite_passed
        or not _same_number(
            finite.get("maximum_achievable_finite_coverage"),
            len(calibration_runs) / (len(calibration_runs) + 1),
        )
        or (
            finite_passed
            and not _same_number(
                finite.get("guaranteed_coverage_lower_bound"),
                rank / (len(calibration_runs) + 1),
            )
        )
        or (quantile is not None and not _same_number(calibration.get("standardized_quantile"), quantile))
        or (quantile is None and calibration.get("standardized_quantile") is not None)
    ):
        raise MlValidationError(f"{owner} finite joint conformal 明细非法")
    informative = finite_passed
    widths = []
    for name in expected_layers:
        centers = nested_layers[name].get("centers")
        if not isinstance(centers, list) or not centers:
            raise MlValidationError(f"{owner} {name} 缺少 center")
        for center in centers:
            detail = _require_mapping(center, f"{owner}.{name}.centers[]")
            normalizer = detail.get("conformal_normalizer_ns")
            margin = detail.get("equivalence_margin_ns")
            if not _plain_number(normalizer) or not _plain_number(margin):
                raise MlValidationError(f"{owner} {name} center scale 非法")
            width = None if quantile is None else quantile * float(normalizer)
            if width is None:
                informative = False
                if detail.get("joint_family_half_width_ns") is not None:
                    raise MlValidationError(f"{owner} joint half-width 非法")
            else:
                widths.append(width)
                informative &= width <= float(margin)
                if not _same_number(detail.get("joint_family_half_width_ns"), width):
                    raise MlValidationError(f"{owner} joint half-width 与分位数矛盾")
    expected_max_width = None if not widths else 2.0 * max(widths)
    if (
        calibration.get("sharpness_gate_passed") is not informative
        or (
            expected_max_width is not None
            and not _same_number(calibration.get("maximum_interval_width_ns"), expected_max_width)
        )
    ):
        raise MlValidationError(f"{owner} joint sharpness 明细非法")
    covered = None if quantile is None else sum(
        score <= quantile for score in test_scores.values()
    )
    evidence = _coverage_evidence_gate(
        covered,
        len(test_runs),
        confidence=confidence,
        minimum_test_runs=minimum_test_runs,
    )
    lower = None if covered is None else _clopper_pearson_lower_bound(
        covered, len(test_runs), confidence=confidence
    )
    if (
        test.get("runs") != len(test_runs)
        or test.get("covered_runs") != covered
        or test.get("evidence_gate_passed") is not evidence
        or (covered is not None and not _same_number(test.get("run_coverage"), covered / len(test_runs)))
        or (lower is not None and not _same_number(test.get("run_coverage_clopper_pearson_one_sided_lower"), lower))
    ):
        raise MlValidationError(f"{owner} joint test 证据非法")
    return {"finite": finite_passed, "informative": informative, "evidence": evidence}


def _recompute_conformal_gate(
    value: Any,
    *,
    owner: str,
    chronological: bool,
    expected_runs: set[str],
    ordered_runs: list[str],
    expected_pairs: int,
    pair_counts_by_run: Mapping[str, int],
) -> bool:
    conformal = _require_mapping(value, owner)
    split = _require_mapping(conformal.get("split"), f"{owner}.split")
    required_checks = (
        REQUIRED_CHRONOLOGICAL_CHECKS
        if chronological
        else REQUIRED_CONFORMAL_CHECKS
    )
    strategy = "chronological" if chronological else "random"
    if conformal.get("split_strategy") != strategy or split.get("strategy") != strategy:
        raise MlValidationError(f"{owner} split strategy 与所属层不一致")
    train = split.get("train_runs")
    calibration = split.get("calibration_runs")
    test = split.get("test_runs")
    if not all(
        isinstance(group, list)
        and group
        and all(isinstance(run, str) and run for run in group)
        for group in (train, calibration, test)
    ):
        raise MlValidationError(f"{owner} 缺少非空 train/calibration/test run 明细")
    train_set, calibration_set, test_set = map(set, (train, calibration, test))
    if (
        len(train_set) != len(train)
        or len(calibration_set) != len(calibration)
        or len(test_set) != len(test)
        or train_set & calibration_set
        or train_set & test_set
        or calibration_set & test_set
        or train_set | calibration_set | test_set != expected_runs
    ):
        raise MlValidationError(f"{owner} run 分组不互斥、不完整或含重复项")
    if chronological and train + calibration + test != ordered_runs:
        raise MlValidationError(f"{owner} 未按采集时间连续划分 run")
    if split.get("leakage_check_passed") is not True:
        raise MlValidationError(f"{owner} 未提供通过的泄漏检查")
    pair_counts = (
        split.get("train_pairs"),
        split.get("calibration_pairs"),
        split.get("test_pairs"),
    )
    expected_pair_counts = tuple(
        sum(pair_counts_by_run[run] for run in group)
        for group in (train, calibration, test)
    )
    if (
        not all(_plain_nonnegative_integer(count) and count > 0 for count in pair_counts)
        or sum(pair_counts) != expected_pairs
        or pair_counts != expected_pair_counts
    ):
        raise MlValidationError(f"{owner} pair 计数与样本不一致")

    finite = _require_mapping(conformal.get("finite_sample"), f"{owner}.finite_sample")
    calibration_rows = _require_mapping(
        conformal.get("calibration"), f"{owner}.calibration"
    )
    test_rows = _require_mapping(conformal.get("test"), f"{owner}.test")
    structural = _require_mapping(conformal.get("structural"), f"{owner}.structural")
    differential = _require_mapping(
        conformal.get("differential_effects"), f"{owner}.differential_effects"
    )
    minimum_train = conformal.get("required_minimum_train_runs_for_high_confidence")
    minimum_test = conformal.get("required_minimum_test_runs_for_high_confidence")
    target_coverage = conformal.get("target_coverage")
    if not all(
        isinstance(value, int) and not isinstance(value, bool) and value > 0
        for value in (minimum_train, minimum_test)
    ) or not _plain_number(target_coverage) or not 0.0 < float(target_coverage) < 1.0:
        raise MlValidationError(f"{owner} 缺少高置信 run 阈值")
    structural_result = _recompute_standardized_layer(
        structural,
        owner=f"{owner}.structural",
        calibration_runs=calibration_set,
        test_runs=test_set,
        confidence=float(target_coverage),
        minimum_test_runs=minimum_test,
        differential=False,
    )
    differential_result = _recompute_standardized_layer(
        differential,
        owner=f"{owner}.differential_effects",
        calibration_runs=calibration_set,
        test_runs=test_set,
        confidence=float(target_coverage),
        minimum_test_runs=minimum_test,
        differential=True,
    )
    joint_result = _recompute_joint_family(
        conformal.get("joint_family"),
        owner=f"{owner}.joint_family",
        structural=structural,
        differential=differential,
        calibration_runs=calibration_set,
        test_runs=test_set,
        confidence=float(target_coverage),
        minimum_test_runs=minimum_test,
        chronological=chronological,
    )
    finite_expected = joint_result["finite"]
    if finite.get("gate_passed") is not finite_expected:
        raise MlValidationError(f"{owner} finite_sample gate 与嵌套明细矛盾")
    if finite.get("calibration_runs") != len(calibration):
        raise MlValidationError(f"{owner} calibration run 计数不一致")
    calibration_scores = calibration_rows.get("run_scores")
    test_scores = test_rows.get("run_scores")
    if (
        not isinstance(calibration_scores, Mapping)
        or set(calibration_scores) != calibration_set
        or not all(_plain_number(score) for score in calibration_scores.values())
        or not isinstance(test_scores, Mapping)
        or set(test_scores) != test_set
        or not all(_plain_number(score) for score in test_scores.values())
    ):
        raise MlValidationError(f"{owner} conformal run score 明细与 split 不一致")
    if test_rows.get("runs") != len(test):
        raise MlValidationError(f"{owner} test run 计数不一致")
    covered_runs = test_rows.get("covered_runs")
    if (
        not _plain_nonnegative_integer(covered_runs)
        or covered_runs > len(test)
        or not _same_number(test_rows.get("run_coverage"), covered_runs / len(test))
    ):
        raise MlValidationError(f"{owner} test coverage 明细不一致")
    expected_coverage_gate = _coverage_evidence_gate(
        covered_runs,
        len(test),
        confidence=float(target_coverage),
        minimum_test_runs=minimum_test,
    )
    if test_rows.get("evidence_gate_passed") is not expected_coverage_gate:
        raise MlValidationError(f"{owner} coverage gate 与精确覆盖证据矛盾")

    joint_conclusion = _require_mapping(
        _require_mapping(
            conformal.get("joint_family"), f"{owner}.joint_family"
        ).get("differential_conclusion_validation"),
        f"{owner}.joint_family.differential_conclusion_validation",
    )
    joint_conclusion_passed = _recompute_differential_conclusion(
        joint_conclusion,
        owner=f"{owner}.joint_family.differential_conclusion_validation",
        test_runs=test_set,
        minimum_test_runs=minimum_test,
        not_applicable=(
            differential.get("status")
            == "unavailable-no-differential-comparisons"
        ),
    )
    checks = {
        "minimum_train_runs": len(train) >= minimum_train,
        "joint_finite_sample_calibration": joint_result["finite"],
        "joint_informative_interval": joint_result["informative"],
        "independent_joint_test_evidence": joint_result["evidence"],
        "differential_conclusion_validation": joint_conclusion_passed,
    }
    if chronological:
        checks.update(
            {
                "forward_structural_temporal_stability": structural_result[
                    "temporal"
                ],
                "forward_differential_temporal_stability": differential_result[
                    "temporal"
                ],
            }
        )
    gate = _require_mapping(
        conformal.get("high_confidence_gate"), f"{owner}.high_confidence_gate"
    )
    stored_checks = gate.get("checks")
    if not isinstance(stored_checks, Mapping) or set(stored_checks) != required_checks:
        raise MlValidationError(f"{owner} high-confidence check 键集合不完整")
    if dict(stored_checks) != checks:
        raise MlValidationError(f"{owner} high-confidence checks 与嵌套明细矛盾")
    failures = [name for name in required_checks if not checks[name]]
    if (
        not isinstance(gate.get("failed_checks"), list)
        or set(gate["failed_checks"]) != set(failures)
        or len(gate["failed_checks"]) != len(failures)
        or gate.get("passed") is not (not failures)
    ):
        raise MlValidationError(f"{owner} high-confidence gate 与 checks 矛盾")
    return not failures


def _recompute_ml_checks(
    validation: Mapping[str, Any],
    *,
    samples: Sequence[Mapping[str, Any]],
    statistical_keys: set[tuple[str, str, str]],
) -> tuple[dict[str, bool], dict[str, Any]]:
    observations = [
        row
        for row in pair_observations(samples)
        if row.suite not in CALIBRATION_ONLY_SUITES
    ]
    if not observations:
        raise MlValidationError("ML finalization 没有可校验 pair")
    if validation.get("publication_familywise_error_control") != (
        _publication_fwer_document()
    ):
        raise MlValidationError("ML validation 两族 FWER 合同缺失或非法")
    expected_runs = set(_ordered_super_runs(observations))
    ordered_runs = _ordered_super_runs(observations)
    pair_counts_by_run: dict[str, int] = defaultdict(int)
    for row in observations:
        pair_counts_by_run[row.super_run_id] += 1
    cross_validation = _require_mapping(
        validation.get("cross_validation"), "cross_validation"
    )
    data = _require_mapping(validation.get("data"), "data")
    if (
        data.get("runs") != len(expected_runs)
        or data.get("pairs") != len(observations)
        or data.get("super_run_ids") != _ordered_super_runs(observations)
        or not expected_runs
    ):
        raise MlValidationError("ML validation data 摘要与绑定样本不一致")
    if cross_validation.get("available") is not True:
        raise MlValidationError("ML cross-validation 不可用")
    folds = cross_validation.get("folds")
    if not isinstance(folds, list) or len(folds) < 2:
        raise MlValidationError("ML cross-validation 缺少至少两个 fold")
    tested_runs: list[str] = []
    tested_pairs = 0
    for index, fold in enumerate(folds):
        detail = _require_mapping(fold, f"cross_validation.folds[{index}]")
        train = detail.get("train_runs")
        test = detail.get("test_runs")
        if (
            not isinstance(train, list)
            or not train
            or not isinstance(test, list)
            or not test
            or set(train) & set(test)
            or set(train) | set(test) != expected_runs
            or len(set(test)) != len(test)
        ):
            raise MlValidationError("GroupKFold run 明细泄漏或不完整")
        if not all(
            _plain_nonnegative_integer(detail.get(name)) and detail.get(name) > 0
            for name in ("train_pairs", "test_pairs")
        ):
            raise MlValidationError("GroupKFold pair 计数非法")
        tested_runs.extend(test)
        tested_pairs += detail["test_pairs"]
    if set(tested_runs) != expected_runs or len(tested_runs) != len(expected_runs):
        raise MlValidationError("GroupKFold test run 未恰好覆盖一次")
    if tested_pairs != len(observations):
        raise MlValidationError("GroupKFold test pair 总数与样本不一致")

    split_passed = _recompute_conformal_gate(
        cross_validation.get("split_conformal"),
        owner="cross_validation.split_conformal",
        chronological=False,
        expected_runs=expected_runs,
        ordered_runs=ordered_runs,
        expected_pairs=len(observations),
        pair_counts_by_run=pair_counts_by_run,
    )
    chronological_passed = _recompute_conformal_gate(
        cross_validation.get("chronological_split_conformal"),
        owner="cross_validation.chronological_split_conformal",
        chronological=True,
        expected_runs=expected_runs,
        ordered_runs=ordered_runs,
        expected_pairs=len(observations),
        pair_counts_by_run=pair_counts_by_run,
    )
    incremental = _require_mapping(
        cross_validation.get("incremental_value"),
        "cross_validation.incremental_value",
    )
    interval = _finite_interval(
        incremental.get("mae_improvement_run_cluster_ci")
    )
    margin = incremental.get("practical_equivalence_ns")
    omitted_structure = (
        incremental.get("status") == "available"
        and interval is not None
        and _plain_number(margin)
        and float(margin) >= 0.0
        and interval[0] >= -float(margin)
        and interval[1] <= float(margin)
    )
    expected_interpretation = (
        "no-practically-material-omitted-structure"
        if omitted_structure
        else (
            "practically-material-omitted-structure-detected"
            if interval is not None
            and _plain_number(margin)
            and interval[0] > float(margin)
            else (
                "flexible-model-materially-worse-than-structured-baseline"
                if interval is not None
                and _plain_number(margin)
                and interval[1] < -float(margin)
                else "inconclusive-against-practical-equivalence-band"
            )
        )
    )
    if (
        incremental.get("role") != "diagnostic-only"
        or incremental.get("formal_gate") is not False
        or incremental.get("gate_passed") is not None
        or incremental.get("diagnostic_equivalence_passed")
        is not omitted_structure
        or incremental.get("training_uncertainty_included") is not False
        or incremental.get("interpretation") != expected_interpretation
    ):
        raise MlValidationError("incremental-value 诊断角色或区间明细矛盾")

    context_rows = validation.get("contexts")
    differential_rows = validation.get("differential_checks")
    if not isinstance(context_rows, list) or not context_rows:
        raise MlValidationError("ML validation 缺少 context 明细")
    if not isinstance(differential_rows, list) or not differential_rows:
        raise MlValidationError("ML validation 缺少 differential 明细")
    context_checks = []
    covered_statistical_keys: set[tuple[str, str, str]] = set()
    for row in context_rows:
        detail = _require_mapping(row, "contexts[]")
        identity = (
            detail.get("semantic_key"),
            detail.get("raw_key"),
            detail.get("pattern"),
        )
        if all(isinstance(value, str) and value for value in identity):
            covered_statistical_keys.add(identity)  # type: ignore[arg-type]
        runs = detail.get("runs")
        interval = _finite_interval(detail.get("ml_bias_cluster_ci"))
        margin = detail.get("equivalence_margin_ns")
        if not isinstance(runs, int) or isinstance(runs, bool) or runs <= 0:
            raise MlValidationError("ML context runs 非法")
        if runs < int(
            _require_mapping(validation.get("configuration"), "configuration").get(
                "minimum_independent_super_runs"
            )
        ):
            expected_context = "inconclusive-insufficient-runs"
        elif interval is None:
            expected_context = "inconclusive-no-prediction-interval"
        elif not _plain_number(margin) or float(margin) < 0.0:
            raise MlValidationError("ML context equivalence margin 非法")
        elif interval[0] >= -float(margin) and interval[1] <= float(margin):
            expected_context = "consistent"
        elif interval[0] > float(margin) or interval[1] < -float(margin):
            expected_context = "contradicted"
        else:
            expected_context = "inconclusive"
        if detail.get("conclusion_check") != expected_context:
            raise MlValidationError("ML context conclusion 与区间明细矛盾")
        context_checks.append(expected_context)
    observation_keys = {
        (row.semantic_key, row.raw_key, row.pattern) for row in observations
    }
    relevant_statistical_keys = statistical_keys & observation_keys
    if not relevant_statistical_keys or not relevant_statistical_keys <= covered_statistical_keys:
        raise MlValidationError("ML contexts 未覆盖全部统计指令稳定键")
    differential_checks = []
    for row in differential_rows:
        detail = _require_mapping(row, "differential_checks[]")
        observed = detail.get("observed_effect_ns")
        predicted = detail.get("ml_oof_effect_ns")
        observed_interval = _finite_interval(detail.get("observed_effect_cluster_ci"))
        margin = detail.get("equivalence_margin_ns")
        if not _plain_number(observed) or not _plain_number(margin):
            raise MlValidationError("ML differential 数值明细非法")
        if predicted is None or observed_interval is None:
            expected_check = "inconclusive"
        elif not _plain_number(predicted):
            raise MlValidationError("ML differential prediction 非法")
        elif abs(float(predicted) - float(observed)) <= float(margin):
            expected_check = "supported"
        elif (
            detail.get("observed_conclusion") == "context-dependent"
            and float(predicted) * float(observed) < 0.0
        ):
            expected_check = "contradicted"
        else:
            expected_check = "inconclusive"
        if detail.get("ml_conclusion_check") != expected_check:
            raise MlValidationError("ML differential conclusion 与数值明细矛盾")
        differential_checks.append(expected_check)
    all_contexts = all(value == "consistent" for value in context_checks)
    all_differentials = all(value == "supported" for value in differential_checks)
    contradicted = "contradicted" in context_checks or "contradicted" in differential_checks
    minimum_runs = int(
        _require_mapping(validation.get("configuration"), "configuration").get(
            "minimum_independent_super_runs"
        )
    )
    if len(expected_runs) < minimum_runs:
        expected_status = "inconclusive-insufficient-independent-runs"
    elif expected_interpretation == "practically-material-omitted-structure-detected":
        expected_status = "contradicted-practically-material-omitted-structure"
    elif expected_interpretation == (
        "flexible-model-materially-worse-than-structured-baseline"
    ):
        expected_status = "inconclusive-flexible-model-underperforms-baseline"
    elif expected_interpretation != "no-practically-material-omitted-structure":
        expected_status = "inconclusive-omitted-structure-equivalence"
    elif contradicted:
        expected_status = "contradicted"
    elif (
        differential_checks
        and all_differentials
        and all_contexts
    ):
        expected_status = "supported"
    else:
        expected_status = "inconclusive"
    conclusion = _require_mapping(validation.get("conclusion"), "conclusion")
    components = conclusion.get("high_confidence_gate_components")
    expected_components = {
        "random_joint_conformal_family": split_passed,
        "chronological_joint_conformal_family": chronological_passed,
    }
    if not isinstance(components, Mapping) or set(components) != REQUIRED_ML_GATE_COMPONENTS:
        raise MlValidationError("ML high-confidence component 键集合不完整")
    if dict(components) != expected_components:
        raise MlValidationError("ML high-confidence components 与明细矛盾")
    high_confidence = all(expected_components.values())
    if conclusion.get("context_checks") != context_checks:
        raise MlValidationError("ML conclusion context 摘要与明细矛盾")
    if conclusion.get("differential_checks") != differential_checks:
        raise MlValidationError("ML conclusion differential 摘要与明细矛盾")
    if conclusion.get("high_confidence_gate_passed") is not high_confidence:
        raise MlValidationError("ML high-confidence gate 与明细矛盾")
    if conclusion.get("diagnostic_status") != expected_status:
        raise MlValidationError("ML diagnostic conclusion 与明细矛盾")
    expected_high_status = (
        "supported" if high_confidence else "inconclusive-ml-high-confidence-gate"
    )
    if conclusion.get("high_confidence_status") != expected_high_status:
        raise MlValidationError("ML high-confidence status 与明细矛盾")
    if conclusion.get("status") != expected_high_status:
        raise MlValidationError("ML formal conclusion 与 conformal 门禁矛盾")
    checks = {
        "supported_conclusion": (
            high_confidence and expected_high_status == "supported"
        ),
        "high_confidence_gate": high_confidence,
        "all_ml_components": all(expected_components.values()),
        "diagnostic_not_weight_source": conclusion.get("may_publish_weights") is False,
    }
    return checks, {
        "components": expected_components,
        "diagnostics": {
            "incremental_equivalence": omitted_structure,
            "context_checks": context_checks,
            "differential_checks": differential_checks,
            "status": expected_status,
            "formal_gate_effect": "none",
        },
    }


def finalize_publication_gate(
    *,
    weights_path: Path,
    samples_path: Path,
    validation_path: Path,
) -> bool:
    """复验 ML 输入绑定，并把独立校验结论并入最终发布门禁。"""

    weights_bytes = weights_path.read_bytes()
    samples_bytes = samples_path.read_bytes()
    weights_identity = {
        "path": weights_path.name,
        "sha256": hashlib.sha256(weights_bytes).hexdigest(),
        "size": len(weights_bytes),
    }
    samples_identity = {
        "path": samples_path.name,
        "sha256": hashlib.sha256(samples_bytes).hexdigest(),
        "size": len(samples_bytes),
    }
    validation_identity = _artifact_identity(validation_path)
    validation = json.loads(validation_path.read_text(encoding="utf-8"))
    if not isinstance(validation, Mapping):
        raise MlValidationError("ML validation artifact 必须是 object")
    bindings = validation.get("input_bindings")
    conclusion = validation.get("conclusion")
    binding_checks = {
        "samples": isinstance(bindings, Mapping)
        and _binding_matches(bindings.get("samples"), samples_identity),
        "statistical_weights_pre_finalization": isinstance(bindings, Mapping)
        and _binding_matches(
            bindings.get("statistical_weights_pre_finalization"),
            weights_identity,
        ),
    }
    document = json.loads(weights_bytes.decode("utf-8"))
    if not isinstance(document, Mapping):
        raise MlValidationError("weights JSON 必须是 object")
    gate = document.get("publication_gate")
    if not isinstance(gate, dict):
        raise MlValidationError("weights JSON 缺少可写 publication_gate")
    components = gate.get("components")
    if not isinstance(components, dict):
        raise MlValidationError("weights JSON 缺少 publication_gate.components")
    statistical_replay: dict[str, Any] | None = None
    try:
        statistical_components, _publishable_count, statistical = (
            _recompute_statistical_components(document)
        )
        samples = load_samples(samples_path)
        if not isinstance(samples, list) or not samples:
            raise MlValidationError("绑定 samples 必须是非空样本数组")
        statistical_replay = _verify_statistical_full_replay(document, samples)
    except (OSError, ValueError, TypeError, KeyError) as error:
        components["ml_validation"] = False
        failures = gate.get("failures")
        if not isinstance(failures, list) or not all(
            isinstance(item, str) for item in failures
        ):
            raise MlValidationError("weights JSON publication_gate.failures 非法")
        failures.extend(["statistical-detail-rejected", "ml-validation-rejected"])
        gate["failures"] = sorted(set(failures))
        gate["required_components"] = list(REQUIRED_PUBLICATION_COMPONENTS)
        gate["passed"] = False
        document["ml_validation_evidence"] = {
            "schema": validation.get("schema"),
            "artifact": validation_identity,
            "input_bindings": bindings,
            "binding_checks": binding_checks,
            "checks": {"statistical_details": False},
            "recomputed": {
                "error": str(error),
                "statistical_full_replay": {"matched": False},
            },
        }
        temporary = weights_path.with_name(f".{weights_path.name}.ml-finalize.tmp")
        temporary.write_text(
            json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n",
            encoding="utf-8",
        )
        temporary.replace(weights_path)
        return False
    detailed_evidence_error = None
    try:
        observations = [
            row
            for row in pair_observations(samples)
            if row.suite not in CALIBRATION_ONLY_SUITES
        ]
        run_count = len(_ordered_super_runs(observations))
        _assert_publication_policy(validation, run_count=run_count)
        replay_bindings = {
            "samples": samples_identity,
            "statistical_weights_pre_finalization": weights_identity,
        }
        replayed_validation, replayed_predictions = (
            _replay_publication_validation(
                samples,
                statistical,
                input_bindings=replay_bindings,
            )
        )
        if _canonical_json_bytes(validation) != _canonical_json_bytes(
            replayed_validation
        ):
            raise MlValidationError(
                "ML validation 与固定发布政策下的确定性完整重放结果不一致"
            )
        detailed_ml_checks, recomputed_ml = _recompute_ml_checks(
            replayed_validation,
            samples=samples,
            statistical_keys=set(statistical),
        )
        recomputed_ml = {
            **recomputed_ml,
            "publication_policy": _publication_policy_document(),
            "full_replay": {
                "matched": True,
                "validation_sha256": hashlib.sha256(
                    _canonical_json_bytes(replayed_validation)
                ).hexdigest(),
                "prediction_evidence": _prediction_evidence(
                    replayed_predictions
                ),
            },
            "statistical_full_replay": statistical_replay,
        }
    except (OSError, ValueError, TypeError, KeyError) as error:
        detailed_evidence_error = str(error)
        detailed_ml_checks = {
            "supported_conclusion": False,
            "high_confidence_gate": False,
            "all_ml_components": False,
            "diagnostic_not_weight_source": False,
        }
        recomputed_ml = {"error": detailed_evidence_error}
    ml_checks = {
        "schema": validation.get("schema") == OUTPUT_SCHEMA,
        "input_bindings": all(binding_checks.values()),
        "fixed_publication_policy": detailed_evidence_error is None,
        "deterministic_full_replay": detailed_evidence_error is None,
        "statistical_full_replay": statistical_replay is not None,
        "detailed_evidence": detailed_evidence_error is None,
        **detailed_ml_checks,
    }
    ml_passed = all(ml_checks.values())

    statistical_core = statistical_components["statistical_core"]
    components["ml_validation"] = ml_passed
    failures = gate.get("failures")
    if not isinstance(failures, list) or not all(
        isinstance(item, str) for item in failures
    ):
        raise MlValidationError("weights JSON publication_gate.failures 非法")
    failures = [
        item
        for item in failures
        if item not in {"ml-validation-missing", "ml-validation-rejected"}
    ]
    if not ml_passed:
        failures.append("ml-validation-rejected")
    failures = sorted(set(failures))
    gate["failures"] = failures
    gate["required_components"] = list(REQUIRED_PUBLICATION_COMPONENTS)
    gate["passed"] = not failures and all(
        components.get(name) is True for name in REQUIRED_PUBLICATION_COMPONENTS
    )
    evidence = {
        "schema": validation.get("schema"),
        "artifact": validation_identity,
        "input_bindings": bindings,
        "binding_checks": binding_checks,
        "checks": ml_checks,
        "recomputed": recomputed_ml,
        "statistical_components_recomputed": statistical_components,
        "high_confidence_status": (
            conclusion.get("high_confidence_status")
            if isinstance(conclusion, Mapping)
            else None
        ),
    }
    document["ml_validation_evidence"] = evidence
    document["ml_validation"] = {
        "schema": validation.get("schema"),
        "input_bindings": bindings,
        "conclusion": conclusion,
        "evidence_artifact": validation_identity,
        "checks": ml_checks,
        "recomputed": recomputed_ml,
    }
    if gate["passed"]:
        seal_model_document(document)
    else:
        document.pop("publication_seal", None)
    temporary = weights_path.with_name(f".{weights_path.name}.ml-finalize.tmp")
    temporary.write_text(
        json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    temporary.replace(weights_path)
    return bool(gate["passed"])


def _replay_publication_validation(
    samples: Sequence[Mapping[str, Any]],
    statistical: Mapping[tuple[str, str, str], Mapping[str, Any]],
    *,
    input_bindings: Mapping[str, Any],
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """按版本化固定政策重放发布分析，禁止 artifact 选择参数。"""

    run_count = len(
        _ordered_super_runs(
            [
                row
                for row in pair_observations(samples)
                if row.suite not in CALIBRATION_ONLY_SUITES
            ]
        )
    )
    if run_count != PUBLICATION_SUPER_RUNS:
        raise MlValidationError(
            "ML 发布固定要求 "
            f"{PUBLICATION_SUPER_RUNS} 个完整 crossover super-run"
        )
    test_runs = PUBLICATION_TEST_SUPER_RUNS
    if test_runs < PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS:
        raise MlValidationError("固定 ML 发布切分没有足够的独立 test super-run")
    result, predictions = validate_predictions(
        samples,
        statistical_weights=statistical,
        input_bindings=input_bindings,
        folds=PUBLICATION_FOLDS,
        max_iter=PUBLICATION_MAX_ITER,
        confidence=PUBLICATION_FAMILY_CONFIDENCE,
        bootstrap_replicates=PUBLICATION_BOOTSTRAP_REPLICATES,
        minimum_runs=PUBLICATION_MINIMUM_RUNS,
        minimum_skill_improvement=PUBLICATION_MINIMUM_SKILL_IMPROVEMENT,
        omitted_structure_equivalence_ns=(
            PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS
        ),
        equivalence_absolute_ns=PUBLICATION_EQUIVALENCE_ABSOLUTE_NS,
        equivalence_relative=PUBLICATION_EQUIVALENCE_RELATIVE,
        conformal_train_runs=PUBLICATION_TRAIN_SUPER_RUNS,
        conformal_calibration_runs=PUBLICATION_CALIBRATION_SUPER_RUNS,
        conformal_test_runs=test_runs,
        conformal_minimum_train_runs=(
            PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
        ),
        conformal_minimum_test_runs=PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS,
        seed=PUBLICATION_SEED,
    )
    result["publication_policy"] = _publication_policy_document()
    result["prediction_evidence"] = _prediction_evidence(predictions)
    return result, predictions


@dataclasses.dataclass(frozen=True)
class PairObservation:
    run_id: str
    run_order: int
    run_order_source: str
    super_run_id: str
    super_run_order: int
    pair_id: str
    block_id: str
    batch: int
    order: str
    instruction: str
    size: int
    semantic_key: str
    raw_key: str
    extension: str
    pattern: str
    suite: str | None
    contrast: str | None
    context: str | None
    differential_variant: str | None
    response_ns: float
    sequence_midpoint: float
    drift: float = 0.0


def _finite(value: Any, owner: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise MlValidationError(f"{owner} 必须是有限数")
    result = float(value)
    if not math.isfinite(result):
        raise MlValidationError(f"{owner} 必须是有限数")
    return result


def _positive_integer(value: Any, owner: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise MlValidationError(f"{owner} 必须是正整数")
    return value


def _nonnegative_integer(value: Any, owner: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise MlValidationError(f"{owner} 必须是非负整数")
    return value


def _exact_target_count(row: Mapping[str, Any], owner: str) -> int:
    """读取与统计模型相同的 QEMU 实测目标计数契约。"""

    raw_target = row.get("target_count", row.get("exact_target_count"))
    if raw_target is None:
        raise MlValidationError(f"{owner} 缺少 QEMU 精确 target_count")
    target = _nonnegative_integer(raw_target, f"{owner}.target_count")
    raw_total = row.get(
        "total_instruction_count",
        row.get("total_count", row.get("qemu_instruction_count")),
    )
    if raw_total is not None:
        total = _nonnegative_integer(
            raw_total, f"{owner}.total_instruction_count"
        )
        if target > total:
            raise MlValidationError(f"{owner}.target_count 不能大于总指令数")
    return target


def _descriptor(row: Mapping[str, Any], owner: str) -> Mapping[str, Any]:
    value = row.get("target_descriptor")
    if not isinstance(value, Mapping):
        raise MlValidationError(f"{owner} 缺少 target_descriptor")
    return value


def _translation_count(row: Mapping[str, Any]) -> int:
    value = row.get("translations_during_window", 0)
    if value is None:
        return 0
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise MlValidationError("translations_during_window 必须是非负整数")
    return value


def pair_observations(
    samples: Sequence[Mapping[str, Any]], *, exclude_translations: bool = True
) -> list[PairObservation]:
    """把 probe/baseline 窗口转换为每目标指令的成对响应。"""

    grouped: dict[tuple[str, str], list[Mapping[str, Any]]] = defaultdict(list)
    first_seen_runs: list[str] = []
    seen_runs: set[str] = set()
    for index, row in enumerate(samples):
        if not isinstance(row, Mapping):
            raise MlValidationError(f"samples[{index}] 必须是 object")
        run_id = row.get("run_id")
        pair_id = row.get("pair_id")
        if not isinstance(run_id, str) or not run_id:
            raise MlValidationError(f"samples[{index}].run_id 非法")
        if not isinstance(pair_id, (str, int)) or str(pair_id) == "":
            raise MlValidationError(f"samples[{index}].pair_id 非法")
        if run_id not in seen_runs:
            first_seen_runs.append(run_id)
            seen_runs.add(run_id)
        grouped[(run_id, str(pair_id))].append(row)

    explicit_orders: dict[str, int] = {}
    for run_id in first_seen_runs:
        values = {
            row.get("run_order")
            for row in samples
            if row.get("run_id") == run_id and row.get("run_order") is not None
        }
        if len(values) > 1:
            raise MlValidationError(f"run={run_id!r} 的 run_order 不一致")
        if values:
            value = next(iter(values))
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise MlValidationError(f"run={run_id!r} 的 run_order 非法")
            explicit_orders[run_id] = value
    if explicit_orders and len(explicit_orders) != len(first_seen_runs):
        raise MlValidationError("run_order 必须覆盖所有 QEMU run")
    if explicit_orders:
        if len(set(explicit_orders.values())) != len(explicit_orders):
            raise MlValidationError("不同 QEMU run 不能复用 run_order")
        run_orders = explicit_orders
        run_order_source = "explicit-run-order"
    else:
        inferred: dict[str, int] = {}
        prefixes: set[str] = set()
        for run_id in first_seen_runs:
            match = re.fullmatch(r"(.*[-_])(\d+)", run_id)
            if match is None:
                inferred = {}
                break
            prefixes.add(match.group(1))
            inferred[run_id] = int(match.group(2), 10)
        expected = set(range(1, len(first_seen_runs) + 1))
        if len(prefixes) == 1 and set(inferred.values()) == expected:
            run_orders = inferred
            run_order_source = "strict-common-prefix-contiguous-suffix"
        else:
            run_orders = {
                run_id: position
                for position, run_id in enumerate(first_seen_runs)
            }
            run_order_source = "input-first-appearance"

    super_runs_by_run: dict[str, str] = {}
    super_orders: dict[str, int] = {}
    super_designs: dict[str, str | None] = {}
    super_pair_positions: dict[str, dict[int, tuple[int, int]]] = defaultdict(dict)
    for run_id in first_seen_runs:
        run_rows = [row for row in samples if row.get("run_id") == run_id]
        identifiers = {
            str(row.get("super_run_id", run_id)) for row in run_rows
        }
        if len(identifiers) != 1 or not next(iter(identifiers)):
            raise MlValidationError(f"run={run_id!r} 的 super_run_id 不一致")
        super_run_id = next(iter(identifiers))
        raw_orders = {
            row.get("super_run_order")
            for row in run_rows
            if row.get("super_run_order") is not None
        }
        if len(raw_orders) > 1:
            raise MlValidationError(
                f"run={run_id!r} 的 super_run_order 不一致"
            )
        super_run_order = (
            run_orders[run_id]
            if not raw_orders
            else next(iter(raw_orders))
        )
        if (
            isinstance(super_run_order, bool)
            or not isinstance(super_run_order, int)
            or super_run_order < 0
        ):
            raise MlValidationError(
                f"run={run_id!r} 的 super_run_order 非法"
            )
        previous = super_orders.setdefault(super_run_id, super_run_order)
        if previous != super_run_order:
            raise MlValidationError(
                f"super-run={super_run_id!r} 的采集序号不一致"
            )
        super_runs_by_run[run_id] = super_run_id
        design_values = {row.get("crossover_design") for row in run_rows}
        if len(design_values) != 1:
            raise MlValidationError(
                f"run={run_id!r} 的 crossover_design 不一致"
            )
        design = next(iter(design_values))
        if design is not None and design not in {"ABBA", "BAAB"}:
            raise MlValidationError(
                f"run={run_id!r} 的 crossover_design 非法"
            )
        previous_design = super_designs.setdefault(super_run_id, design)
        if previous_design != design:
            raise MlValidationError(
                f"super-run={super_run_id!r} 的 crossover_design 不一致"
            )
        metadata_present = any(
            row.get(name) is not None
            for row in run_rows
            for name in (
                "crossover_pair",
                "crossover_design",
                "timing_launch_position",
                "plugin_off_launch_position",
            )
        )
        if metadata_present:
            triples = {
                (
                    row.get("crossover_pair"),
                    row.get("timing_launch_position"),
                    row.get("plugin_off_launch_position"),
                )
                for row in run_rows
            }
            if len(triples) != 1:
                raise MlValidationError(
                    f"run={run_id!r} 的 crossover 启动元数据不一致"
                )
            pair, timing, plugin_off = next(iter(triples))
            if (
                isinstance(pair, bool)
                or pair not in {1, 2}
                or isinstance(timing, bool)
                or timing not in {1, 2, 3, 4}
                or isinstance(plugin_off, bool)
                or plugin_off not in {1, 2, 3, 4}
                or timing == plugin_off
            ):
                raise MlValidationError(
                    f"run={run_id!r} 的 crossover 启动位置不完整"
                )
            previous_positions = super_pair_positions[super_run_id].setdefault(
                int(pair), (int(timing), int(plugin_off))
            )
            if previous_positions != (timing, plugin_off):
                raise MlValidationError(
                    f"super-run={super_run_id!r} 的 crossover pair 元数据不一致"
                )
    if len(set(super_orders.values())) != len(super_orders):
        raise MlValidationError("不同 super-run 不能复用 super_run_order")
    for super_run_id, design in super_designs.items():
        positions = super_pair_positions.get(super_run_id, {})
        if design is None and not positions:
            continue
        expected = (
            {1: (1, 2), 2: (4, 3)}
            if design == "ABBA"
            else {1: (2, 1), 2: (3, 4)}
            if design == "BAAB"
            else None
        )
        if positions != expected:
            raise MlValidationError(
                f"super-run={super_run_id!r} 的启动位置与 {design} 不一致"
            )

    output: list[PairObservation] = []
    for (run_id, pair_id), rows in sorted(grouped.items()):
        roles = {str(row.get("role")): row for row in rows}
        if len(rows) != 2 or set(roles) != {"probe", "baseline"}:
            raise MlValidationError(
                f"run={run_id} pair={pair_id} 不是唯一 probe/baseline 对"
            )
        probe = roles["probe"]
        baseline = roles["baseline"]
        for name in (
            "block_id",
            "instruction",
            "encoding_bytes",
            "pattern",
            "requested_count",
            "target_descriptor",
        ):
            if probe.get(name) != baseline.get(name):
                raise MlValidationError(
                    f"run={run_id} pair={pair_id} 的 {name} 结构不一致"
                )
        for name in (
            "probe_version",
            "probe_contract",
            "operand_set",
            "calibration_profile",
            "suite",
            "contrast",
            "differential_variant",
            "context",
        ):
            if probe.get(name) != baseline.get(name):
                raise MlValidationError(
                    f"run={run_id} pair={pair_id} 的 {name} 元数据不一致"
                )
        if exclude_translations and (
            _translation_count(probe) or _translation_count(baseline)
        ):
            continue
        requested = _positive_integer(
            probe.get("requested_count"), f"run={run_id} pair={pair_id}.requested_count"
        )
        if baseline.get("requested_count") != requested:
            raise MlValidationError(
                f"run={run_id} pair={pair_id} 的 requested_count 不一致"
            )
        descriptor = _descriptor(probe, f"run={run_id} pair={pair_id}")
        semantic_key = descriptor.get("encoding_key")
        raw_bytes = descriptor.get("bytes")
        size = probe.get("encoding_bytes")
        if not isinstance(semantic_key, str) or not semantic_key:
            raise MlValidationError("target_descriptor.encoding_key 非法")
        if not isinstance(raw_bytes, str) or not raw_bytes:
            raise MlValidationError("target_descriptor.bytes 非法")
        if isinstance(size, bool) or not isinstance(size, int) or size not in {2, 4}:
            raise MlValidationError("encoding_bytes 必须是 2 或 4")
        pattern = probe.get("pattern")
        instruction = probe.get("instruction")
        if not isinstance(pattern, str) or not pattern:
            raise MlValidationError("pattern 必须是非空字符串")
        if not isinstance(instruction, str) or not instruction:
            raise MlValidationError("instruction 必须是非空字符串")
        probe_sequence = _finite(probe.get("sequence"), "probe.sequence")
        baseline_sequence = _finite(baseline.get("sequence"), "baseline.sequence")
        probe_target = _exact_target_count(
            probe, f"run={run_id} pair={pair_id}.probe"
        )
        baseline_target = _exact_target_count(
            baseline, f"run={run_id} pair={pair_id}.baseline"
        )
        target_delta = probe_target - baseline_target
        if target_delta <= 0:
            raise MlValidationError(
                f"run={run_id} pair={pair_id} 的 probe target_count 必须大于 baseline"
            )
        response = (
            _finite(probe.get("plugin_thread_cpu_ns"), "probe.plugin_thread_cpu_ns")
            - _finite(
                baseline.get("plugin_thread_cpu_ns"),
                "baseline.plugin_thread_cpu_ns",
            )
        ) / target_delta
        extension = descriptor.get("extension")
        if not isinstance(extension, str) or not extension:
            parts = semantic_key.split(":")
            extension = parts[2] if len(parts) > 2 else "unknown"
        output.append(
            PairObservation(
                run_id=run_id,
                run_order=run_orders[run_id],
                run_order_source=run_order_source,
                super_run_id=super_runs_by_run[run_id],
                super_run_order=super_orders[super_runs_by_run[run_id]],
                pair_id=pair_id,
                block_id=str(probe.get("block_id", "")),
                batch=requested,
                order="probe-first"
                if probe_sequence < baseline_sequence
                else "baseline-first",
                instruction=instruction.lower(),
                size=size,
                semantic_key=semantic_key,
                raw_key=f"raw:{size}:{raw_bytes.lower()}",
                extension=extension,
                pattern=pattern.lower(),
                suite=(
                    str(probe["suite"]).lower() if probe.get("suite") else None
                ),
                contrast=(
                    str(probe["contrast"]).lower()
                    if probe.get("contrast")
                    else None
                ),
                context=(
                    str(probe["context"]).lower()
                    if probe.get("context")
                    else None
                ),
                differential_variant=(
                    str(probe["differential_variant"]).lower()
                    if probe.get("differential_variant")
                    else None
                ),
                response_ns=response,
                sequence_midpoint=(probe_sequence + baseline_sequence) / 2.0,
            )
        )
    if not output:
        raise MlValidationError("过滤后没有可用 probe/baseline pair")

    by_run: dict[str, list[PairObservation]] = defaultdict(list)
    for row in output:
        by_run[row.run_id].append(row)
    normalized: list[PairObservation] = []
    for run_rows in by_run.values():
        low = min(row.sequence_midpoint for row in run_rows)
        high = max(row.sequence_midpoint for row in run_rows)
        for row in run_rows:
            drift = 0.0 if high == low else (row.sequence_midpoint - low) / (high - low)
            normalized.append(dataclasses.replace(row, drift=drift - 0.5))
    return sorted(
        normalized,
        key=lambda row: (row.run_order, row.run_id, row.sequence_midpoint),
    )


def _ordered_runs(observations: Sequence[PairObservation]) -> list[str]:
    """按显式采集序号返回 QEMU run，拒绝时间轴歧义。"""

    orders: dict[str, int] = {}
    for row in observations:
        previous = orders.setdefault(row.run_id, row.run_order)
        if previous != row.run_order:
            raise MlValidationError(f"run={row.run_id!r} 的采集序号不一致")
    if len(set(orders.values())) != len(orders):
        raise MlValidationError("不同 QEMU run 的采集序号重复")
    return sorted(orders, key=lambda run_id: (orders[run_id], run_id))


def _ordered_super_runs(observations: Sequence[PairObservation]) -> list[str]:
    """按显式采集序号返回最高独立层级，拒绝时间轴歧义。"""

    orders: dict[str, int] = {}
    for row in observations:
        previous = orders.setdefault(row.super_run_id, row.super_run_order)
        if previous != row.super_run_order:
            raise MlValidationError(
                f"super-run={row.super_run_id!r} 的采集序号不一致"
            )
    if len(set(orders.values())) != len(orders):
        raise MlValidationError("不同 super-run 的采集序号重复")
    return sorted(orders, key=lambda run_id: (orders[run_id], run_id))


def _feature(row: PairObservation) -> dict[str, float | str]:
    result: dict[str, float | str] = {
        "instruction": row.instruction,
        "size": str(row.size),
        "semantic": row.semantic_key,
        "raw": row.raw_key,
        "extension": row.extension,
        "pattern": row.pattern,
        "order": row.order,
        "log2_batch": math.log2(row.batch),
        "within_run_drift": row.drift,
    }
    for name, value in (
        ("suite", row.suite),
        ("contrast", row.contrast),
        ("differential_variant", row.differential_variant),
        ("context", row.context),
    ):
        if value is not None:
            result[name] = value
    return result


def _load_sklearn() -> dict[str, Any]:
    try:
        import numpy
        import sklearn
        from sklearn.dummy import DummyRegressor
        from sklearn.ensemble import HistGradientBoostingRegressor
        from sklearn.feature_extraction import DictVectorizer
        from sklearn.model_selection import GroupKFold
        from sklearn.pipeline import Pipeline
    except ImportError as error:
        raise MlValidationError(
            "缺少机器学习依赖；请先运行 scripts/setup-riscv-instruction-ml-venv.sh"
        ) from error
    return {
        "numpy": numpy,
        "sklearn": sklearn,
        "DummyRegressor": DummyRegressor,
        "HistGradientBoostingRegressor": HistGradientBoostingRegressor,
        "DictVectorizer": DictVectorizer,
        "GroupKFold": GroupKFold,
        "Pipeline": Pipeline,
    }


def _regression_metrics(actual: Sequence[float], predicted: Sequence[float]) -> dict[str, Any]:
    if len(actual) != len(predicted) or not actual:
        raise MlValidationError("回归指标需要非空等长序列")
    errors = [estimate - value for value, estimate in zip(actual, predicted)]
    absolute = [abs(value) for value in errors]
    mean_actual = math.fsum(actual) / len(actual)
    denominator = math.fsum((value - mean_actual) ** 2 for value in actual)
    squared = math.fsum(value * value for value in errors)
    scale = statistics.median(abs(value) for value in actual)
    return {
        "pairs": len(actual),
        "mae_ns": math.fsum(absolute) / len(absolute),
        "median_absolute_error_ns": statistics.median(absolute),
        "rmse_ns": math.sqrt(squared / len(errors)),
        "r_squared": None if denominator == 0.0 else 1.0 - squared / denominator,
        "relative_mae_to_median_absolute_response": (
            None if scale == 0.0 else math.fsum(absolute) / len(absolute) / scale
        ),
    }


def _new_model(ml: Mapping[str, Any], *, seed: int, max_iter: int) -> Any:
    return ml["Pipeline"](
        [
            ("features", ml["DictVectorizer"](sparse=False, sort=True)),
            (
                "regressor",
                ml["HistGradientBoostingRegressor"](
                    loss="absolute_error",
                    learning_rate=0.075,
                    max_iter=max_iter,
                    max_leaf_nodes=31,
                    min_samples_leaf=20,
                    l2_regularization=1.0,
                    early_stopping=False,
                    random_state=seed,
                ),
            ),
        ]
    )


def _context_batch_median_predictions(
    observations: Sequence[PairObservation],
    target: Sequence[float],
    train_indices: Sequence[int],
    test_indices: Sequence[int],
) -> list[float]:
    """训练折 context+batch 中位数；未知层级依次回退，不读取测试响应。"""

    train_target = [target[index] for index in train_indices]
    if not train_target:
        raise MlValidationError("context+batch 基线缺少训练 pair")
    global_median = statistics.median(train_target)
    by_context: dict[tuple[str, ...], list[float]] = defaultdict(list)
    by_context_batch: dict[tuple[tuple[str, ...], int], list[float]] = defaultdict(list)
    for index in train_indices:
        identity = _context_identity(observations[index])
        by_context[identity].append(target[index])
        by_context_batch[(identity, observations[index].batch)].append(target[index])
    context_medians = {
        identity: statistics.median(values) for identity, values in by_context.items()
    }
    context_batch_medians = {
        identity: statistics.median(values)
        for identity, values in by_context_batch.items()
    }
    output = []
    for index in test_indices:
        identity = _context_identity(observations[index])
        output.append(
            context_batch_medians.get(
                (identity, observations[index].batch),
                context_medians.get(identity, global_median),
            )
        )
    return output


def _cross_validated_predictions(
    observations: Sequence[PairObservation],
    *,
    folds: int,
    seed: int,
    max_iter: int,
) -> tuple[
    list[float] | None,
    list[float] | None,
    list[float] | None,
    list[dict[str, Any]],
    dict[str, str],
]:
    ml = _load_sklearn()
    features = [_feature(row) for row in observations]
    target = [row.response_ns for row in observations]
    groups = [row.super_run_id for row in observations]
    runs = sorted(set(groups))
    versions = {
        "scikit_learn": str(ml["sklearn"].__version__),
        "numpy": str(ml["numpy"].__version__),
    }
    if len(runs) < 2 or folds < 2:
        return None, None, None, [], versions
    split_count = min(folds, len(runs))
    splitter = ml["GroupKFold"](n_splits=split_count)
    predicted = [math.nan] * len(observations)
    dummy_predicted = [math.nan] * len(observations)
    context_batch_predicted = [math.nan] * len(observations)
    fold_rows: list[dict[str, Any]] = []
    for fold_index, (train, test) in enumerate(
        splitter.split(features, target, groups), 1
    ):
        train_features = [features[index] for index in train]
        train_target = [target[index] for index in train]
        test_features = [features[index] for index in test]
        model = _new_model(ml, seed=seed + fold_index, max_iter=max_iter)
        model.fit(train_features, train_target)
        estimates = model.predict(test_features)
        dummy = ml["DummyRegressor"](strategy="median")
        dummy.fit([[0.0] for _ in train], train_target)
        dummy_estimates = dummy.predict([[0.0] for _ in test])
        baseline_estimates = _context_batch_median_predictions(
            observations, target, train, test
        )
        for index, estimate, dummy_estimate, baseline_estimate in zip(
            test, estimates, dummy_estimates, baseline_estimates, strict=True
        ):
            predicted[index] = float(estimate)
            dummy_predicted[index] = float(dummy_estimate)
            context_batch_predicted[index] = float(baseline_estimate)
        train_runs = sorted({groups[index] for index in train})
        test_runs = sorted({groups[index] for index in test})
        if set(train_runs) & set(test_runs):
            raise MlValidationError("GroupKFold 泄漏了 crossover super-run")
        fold_rows.append(
            {
                "fold": fold_index,
                "train_runs": train_runs,
                "test_runs": test_runs,
                "train_pairs": len(train),
                "test_pairs": len(test),
            }
        )
    if any(
        not math.isfinite(value)
        for value in predicted + dummy_predicted + context_batch_predicted
    ):
        raise MlValidationError("交叉验证没有覆盖全部 pair")
    return predicted, dummy_predicted, context_batch_predicted, fold_rows, versions


def _quantile(values: Sequence[float], probability: float) -> float:
    if not values:
        raise MlValidationError("空序列没有分位数")
    ordered = sorted(values)
    position = probability * (len(ordered) - 1)
    left = int(math.floor(position))
    right = int(math.ceil(position))
    fraction = position - left
    return ordered[left] * (1.0 - fraction) + ordered[right] * fraction


def _moving_block_positions(
    length: int, block_length: int, rng: random.Random
) -> list[int]:
    """循环 moving-block 索引，保留相邻 run 的慢漂移相关。"""

    if length <= 0:
        return []
    block_length = max(1, min(block_length, length))
    output: list[int] = []
    while len(output) < length:
        start = rng.randrange(length)
        output.extend(
            (start + offset) % length for offset in range(block_length)
        )
    return output[:length]


def _cluster_bootstrap_interval(
    values_by_run: Mapping[str, Sequence[float]],
    *,
    confidence: float,
    replicates: int,
    seed: int,
    block_length: int | None = None,
) -> tuple[float, list[float] | None]:
    runs = list(values_by_run)
    run_means = {
        run: math.fsum(values_by_run[run]) / len(values_by_run[run]) for run in runs
    }
    point = math.fsum(run_means.values()) / len(run_means)
    if replicates <= 0 or len(runs) < 2:
        return point, None
    rng = random.Random(seed)
    estimates = []
    selected_block = block_length or max(
        1, int(round(len(runs) ** (1.0 / 3.0)))
    )
    for _ in range(replicates):
        selected = [
            runs[index]
            for index in _moving_block_positions(
                len(runs), selected_block, rng
            )
        ]
        estimates.append(math.fsum(run_means[run] for run in selected) / len(selected))
    alpha = (1.0 - confidence) / 2.0
    return point, [_quantile(estimates, alpha), _quantile(estimates, 1.0 - alpha)]


def _incremental_prediction_value(
    observations: Sequence[PairObservation],
    actual: Sequence[float],
    predicted: Sequence[float] | None,
    baseline: Sequence[float] | None,
    *,
    confidence: float,
    bootstrap_replicates: int,
    minimum_relative_improvement: float,
    practical_equivalence_ns: float,
    seed: int,
) -> dict[str, Any]:
    """诊断 HGB 是否发现 context+batch 基线遗漏的实用结构。

    这里重采样的是一次 grouped cross-fitting 已产生的固定 OOF 误差；replicate
    内没有重新拟合 HGB 或基线，因此区间不包含训练样本变化造成的不确定性。
    它只适合作为探索性效应大小诊断，正式 ML 门禁由互斥 train/calibration/test
    的 random 与 chronological 联合 split-conformal family 承担。
    """

    if predicted is None or baseline is None:
        return {
            "status": "unavailable",
            "role": "diagnostic-only",
            "formal_gate": False,
            "gate_passed": None,
            "diagnostic_equivalence_passed": False,
            "training_uncertainty_included": False,
            "reason": "cross-validation-unavailable",
            "interpretation": "inconclusive-against-practical-equivalence-band",
        }
    differences_by_run: dict[str, list[float]] = defaultdict(list)
    baseline_errors = []
    ml_errors = []
    for row, value, estimate, baseline_estimate in zip(
        observations, actual, predicted, baseline, strict=True
    ):
        ml_error = abs(value - estimate)
        baseline_error = abs(value - baseline_estimate)
        ml_errors.append(ml_error)
        baseline_errors.append(baseline_error)
        differences_by_run[row.super_run_id].append(baseline_error - ml_error)
    ml_mae = math.fsum(ml_errors) / len(ml_errors)
    baseline_mae = math.fsum(baseline_errors) / len(baseline_errors)
    relative_improvement = (
        None if baseline_mae == 0.0 else 1.0 - ml_mae / baseline_mae
    )
    ordered_differences = {
        run: differences_by_run[run]
        for run in _ordered_super_runs(observations)
        if run in differences_by_run
    }
    improvement, interval = _cluster_bootstrap_interval(
        ordered_differences,
        confidence=confidence,
        replicates=bootstrap_replicates,
        seed=seed,
    )
    omitted_structure_gate = (
        interval is not None
        and interval[0] >= -practical_equivalence_ns
        and interval[1] <= practical_equivalence_ns
    )
    if omitted_structure_gate:
        interpretation = "no-practically-material-omitted-structure"
    elif interval is not None and interval[0] > practical_equivalence_ns:
        interpretation = "practically-material-omitted-structure-detected"
    elif interval is not None and interval[1] < -practical_equivalence_ns:
        interpretation = "flexible-model-materially-worse-than-structured-baseline"
    else:
        interpretation = "inconclusive-against-practical-equivalence-band"
    return {
        "status": "available",
        "baseline": (
            "train-fold context+batch median; unseen batch falls back to context, "
            "unseen context falls back to train-fold global median"
        ),
        "cluster_unit": "complete ABBA/BAAB crossover super-run",
        "run_resampling": "super-run-circular-moving-block-bootstrap",
        "resampling_target": "fixed grouped-OOF absolute-error differences",
        "automatic_run_block_length_rule": "round(number-of-super-runs^(1/3))",
        "mae_improvement_baseline_minus_ml_ns": improvement,
        "mae_improvement_run_cluster_ci": interval,
        "mae_difference_ml_minus_baseline_ns": -improvement,
        "mae_difference_ml_minus_baseline_run_cluster_ci": (
            None if interval is None else [-interval[1], -interval[0]]
        ),
        "relative_mae_improvement": relative_improvement,
        "minimum_relative_improvement": minimum_relative_improvement,
        "relative_improvement_is_diagnostic_only": True,
        "practical_equivalence_ns": practical_equivalence_ns,
        # 旧字段为 JSON 兼容；其数值现在是双侧等价带半宽。
        "minimum_practical_improvement_ns": practical_equivalence_ns,
        "equivalence_interval_ns": [
            -practical_equivalence_ns,
            practical_equivalence_ns,
        ],
        "role": "diagnostic-only",
        "formal_gate": False,
        "gate_passed": None,
        "diagnostic_equivalence_passed": omitted_structure_gate,
        "training_uncertainty_included": False,
        "uncertainty_limitation": (
            "bootstrap conditions on one fitted grouped-OOF prediction vector; "
            "replicates do not refit HGB or the context+batch baseline"
        ),
        "formal_evidence_replacement": (
            "random and chronological joint split-conformal families over complete "
            "crossover super-runs"
        ),
        "interpretation": interpretation,
        "warning": (
            "该区间不包含模型训练不确定性，禁止作为发布门禁；相对全局常数"
            "中位数的高 skill 也只说明上下文可预测"
        ),
    }


def _conformal_rank(calibration_runs: int, confidence: float) -> int:
    """返回 split-conformal 的有限样本 order-statistic rank。"""

    # 减去微小容差，避免 20 * 0.95 被二进制浮点表示成 19.000...002。
    return math.ceil((calibration_runs + 1) * confidence - 1.0e-12)


def _minimum_calibration_runs(confidence: float) -> int:
    """返回目标覆盖率能够使用有限分位数时所需的最少 calibration run。"""

    runs = 1
    while _conformal_rank(runs, confidence) > runs:
        runs += 1
    return runs


def _wilson_interval(
    successes: int, total: int, *, confidence: float
) -> list[float] | None:
    """给独立 test run 覆盖率提供仅用于诊断的 Wilson 区间。"""

    if total <= 0:
        return None
    probability = successes / total
    z = statistics.NormalDist().inv_cdf(0.5 + confidence / 2.0)
    denominator = 1.0 + z * z / total
    center = (probability + z * z / (2.0 * total)) / denominator
    radius = (
        z
        * math.sqrt(
            probability * (1.0 - probability) / total
            + z * z / (4.0 * total * total)
        )
        / denominator
    )
    return [max(0.0, center - radius), min(1.0, center + radius)]


def _clopper_pearson_lower_bound(
    successes: int, total: int, *, confidence: float
) -> float | None:
    """精确二项分布单侧下界；实现不依赖 SciPy。"""

    if total <= 0 or successes < 0 or successes > total:
        return None
    if successes == 0:
        return 0.0
    alpha = 1.0 - confidence
    if successes == total:
        return alpha ** (1.0 / total)

    def upper_tail(probability: float) -> float:
        return math.fsum(
            math.comb(total, count)
            * probability**count
            * (1.0 - probability) ** (total - count)
            for count in range(successes, total + 1)
        )

    low = 0.0
    high = successes / total
    for _ in range(80):
        midpoint = (low + high) / 2.0
        if upper_tail(midpoint) < alpha:
            low = midpoint
        else:
            high = midpoint
    return (low + high) / 2.0


def _coverage_evidence_gate(
    successes: int | None,
    total: int,
    *,
    confidence: float,
    minimum_test_runs: int,
) -> bool:
    """要求独立留出覆盖率的精确单侧下界达到目标，而非只看点估计。"""

    if successes is None or total < minimum_test_runs:
        return False
    lower = _clopper_pearson_lower_bound(
        successes, total, confidence=confidence
    )
    return lower is not None and lower >= confidence


def _automatic_conformal_split_counts(
    run_count: int,
    *,
    required_calibration_runs: int,
    confidence: float = 0.95,
    minimum_train_runs: int = 20,
    minimum_test_runs: int = 20,
) -> tuple[int, int, int]:
    """优先给独立 test 留足精确覆盖率证据所需的 run。"""

    if run_count < 3:
        return 0, 0, 0
    perfect_test_runs = max(
        minimum_test_runs,
        math.ceil(math.log(1.0 - confidence) / math.log(confidence)),
    )
    calibration_target = max(required_calibration_runs, 20)
    if run_count >= minimum_train_runs + calibration_target + perfect_test_runs:
        return (
            minimum_train_runs,
            calibration_target,
            run_count - minimum_train_runs - calibration_target,
        )
    # 中等样本保持平衡，且由精确覆盖门禁明确标记 test 证据不足。
    if run_count >= 60 and run_count // 3 >= required_calibration_runs:
        train = run_count // 3
        calibration = run_count // 3
        return train, calibration, run_count - train - calibration
    # 40 run 时刻意得到 20/19/1。这样 95% 有限样本分位数刚好可用，
    # 但一个 test run 会被门禁明确标为证据不足。
    if run_count >= 20 + required_calibration_runs + 1:
        return 20, required_calibration_runs, run_count - 20 - required_calibration_runs
    test = 1
    train = max(1, (run_count - test + 1) // 2)
    calibration = run_count - train - test
    if calibration <= 0:
        calibration = 1
        train = run_count - calibration - test
    return train, calibration, test


def _context_identity(
    row: PairObservation,
) -> tuple[str, str, str, str, str, str, str]:
    return (
        row.semantic_key,
        row.raw_key,
        row.pattern,
        row.suite or "",
        row.contrast or "",
        row.differential_variant or "",
        row.context or "",
    )


def _context_descriptor(identity: tuple[str, ...]) -> dict[str, Any]:
    return {
        "semantic_key": identity[0],
        "raw_key": identity[1],
        "pattern": identity[2],
        "suite": identity[3] or None,
        "contrast": identity[4] or None,
        "differential_variant": identity[5] or None,
        "context": identity[6] or None,
    }


def _subset_group_oof_predictions(
    observations: Sequence[PairObservation],
    indices: Sequence[int],
    *,
    features: Sequence[Mapping[str, float | str]],
    target: Sequence[float],
    ml: Mapping[str, Any],
    seed: int,
    max_iter: int,
) -> dict[int, float]:
    """只在 train run 内产生 OOF 预测，供尺度估计使用。"""

    subset = list(indices)
    groups = [observations[index].super_run_id for index in subset]
    runs = sorted(set(groups))
    if len(runs) < 2:
        model = _new_model(ml, seed=seed, max_iter=max_iter)
        model.fit([features[index] for index in subset], [target[index] for index in subset])
        estimates = model.predict([features[index] for index in subset])
        return {
            index: float(estimate)
            for index, estimate in zip(subset, estimates, strict=True)
        }
    splitter = ml["GroupKFold"](n_splits=min(5, len(runs)))
    output: dict[int, float] = {}
    subset_features = [features[index] for index in subset]
    subset_target = [target[index] for index in subset]
    for fold, (train_positions, test_positions) in enumerate(
        splitter.split(subset_features, subset_target, groups), 1
    ):
        model = _new_model(ml, seed=seed + fold, max_iter=max_iter)
        model.fit(
            [subset_features[position] for position in train_positions],
            [subset_target[position] for position in train_positions],
        )
        estimates = model.predict(
            [subset_features[position] for position in test_positions]
        )
        for position, estimate in zip(test_positions, estimates, strict=True):
            output[subset[position]] = float(estimate)
    if len(output) != len(subset):
        raise MlValidationError("train-only GroupKFold 没有覆盖全部 pair")
    return output


def _context_residual_centers(
    observations: Sequence[PairObservation],
    indices: Sequence[int],
    predictions: Mapping[int, float],
) -> dict[str, dict[tuple[str, ...], float]]:
    grouped: dict[tuple[str, tuple[str, ...]], list[float]] = defaultdict(list)
    for index in indices:
        estimate = predictions.get(index)
        if estimate is None:
            raise MlValidationError("上下文中心缺少预测")
        row = observations[index]
        grouped[(row.super_run_id, _context_identity(row))].append(
            row.response_ns - estimate
        )
    output: dict[str, dict[tuple[str, ...], float]] = defaultdict(dict)
    for (run, identity), residuals in grouped.items():
        output[run][identity] = statistics.median(residuals)
    return dict(output)


def _differential_residual_centers(
    observations: Sequence[PairObservation],
    indices: Sequence[int],
    predictions: Mapping[int, float],
) -> tuple[
    dict[str, dict[tuple[str, str, str], float]],
    dict[str, dict[tuple[str, str, str], float]],
    dict[str, dict[tuple[str, str, str], float]],
]:
    values: dict[
        tuple[str, str, str, str, int, str], list[int]
    ] = defaultdict(list)
    for index in indices:
        row = observations[index]
        identity = _differential_identity(row)
        if identity is None:
            continue
        group, variant = identity
        values[
            (
                row.suite or "legacy",
                group,
                variant,
                row.super_run_id,
                row.batch,
                row.block_id,
            )
        ].append(index)

    variants: dict[tuple[str, str], set[str]] = defaultdict(set)
    for suite, group, variant, _run, _batch, _block in values:
        variants[(suite, group)].add(variant)
    grouped: dict[
        tuple[str, tuple[str, str, str]], list[tuple[float, float]]
    ] = defaultdict(list)
    for suite, group in sorted(variants):
        if "reference" not in variants[(suite, group)]:
            continue
        for variant in sorted(variants[(suite, group)] - {"reference"}):
            treatment_keys = {
                (run, batch, block)
                for current_suite, current_group, current_variant, run, batch, block in values
                if current_suite == suite
                and current_group == group
                and current_variant == variant
            }
            for run, batch, block in treatment_keys:
                reference = values.get(
                    (suite, group, "reference", run, batch, block)
                )
                treatment = values.get((suite, group, variant, run, batch, block))
                if not reference or not treatment:
                    continue
                actual_effect = statistics.median(
                    observations[index].response_ns for index in treatment
                ) - statistics.median(
                    observations[index].response_ns for index in reference
                )
                predicted_effect = statistics.median(
                    predictions[index] for index in treatment
                ) - statistics.median(predictions[index] for index in reference)
                grouped[(run, (suite, group, variant))].append(
                    (actual_effect, predicted_effect)
                )
    residual_output: dict[str, dict[tuple[str, str, str], float]] = defaultdict(dict)
    actual_output: dict[str, dict[tuple[str, str, str], float]] = defaultdict(dict)
    predicted_output: dict[str, dict[tuple[str, str, str], float]] = defaultdict(dict)
    for (run, identity), effects in grouped.items():
        actual_center = statistics.median(value[0] for value in effects)
        predicted_center = statistics.median(value[1] for value in effects)
        residual_output[run][identity] = actual_center - predicted_center
        actual_output[run][identity] = actual_center
        predicted_output[run][identity] = predicted_center
    return dict(residual_output), dict(actual_output), dict(predicted_output)


def _robust_train_scales(
    centers: Mapping[str, Mapping[tuple[str, ...], float]],
    margins: Mapping[tuple[str, ...], float],
) -> dict[tuple[str, ...], dict[str, float]]:
    runs = sorted(centers)
    identities = sorted({identity for rows in centers.values() for identity in rows})
    output = {}
    for identity in identities:
        values = [centers[run][identity] for run in runs if identity in centers[run]]
        center = statistics.median(values)
        mad_scale = 1.4826 * statistics.median(abs(value - center) for value in values)
        q1 = _quantile(values, 0.25)
        q3 = _quantile(values, 0.75)
        iqr_scale = (q3 - q1) / 1.349
        # floor 仅防止离散计时产生零尺度；它取自预先给定的等价阈值，
        # 不读取 calibration/test 响应。
        floor = max(1.0e-9, margins[identity] * 0.01)
        output[identity] = {
            "train_center_ns": center,
            "train_mad_scale_ns": mad_scale,
            "train_iqr_scale_ns": iqr_scale,
            "scale_floor_ns": floor,
            "scale_ns": max(mad_scale, iqr_scale, floor),
            "conformal_normalizer_ns": margins[identity],
            "equivalence_margin_ns": margins[identity],
        }
    return output


def _validate_center_matrix(
    centers: Mapping[str, Mapping[tuple[str, ...], float]],
    *,
    runs: Sequence[str],
    identities: set[tuple[str, ...]],
    owner: str,
) -> None:
    for run in runs:
        actual = set(centers.get(run, {}))
        if actual != identities:
            missing = len(identities - actual)
            extra = len(actual - identities)
            raise MlValidationError(
                f"{owner} run={run} 上下文矩阵不闭合: missing={missing} extra={extra}"
            )


def _pearson_correlation(left: Sequence[float], right: Sequence[float]) -> float | None:
    if len(left) != len(right) or len(left) < 3:
        return None
    left_center = statistics.mean(left)
    right_center = statistics.mean(right)
    numerator = math.fsum(
        (a - left_center) * (b - right_center)
        for a, b in zip(left, right, strict=True)
    )
    denominator = math.sqrt(
        math.fsum((value - left_center) ** 2 for value in left)
        * math.fsum((value - right_center) ** 2 for value in right)
    )
    return None if denominator <= 0.0 else numerator / denominator


def _rankdata(values: Sequence[float]) -> list[float]:
    order = sorted(range(len(values)), key=lambda index: values[index])
    ranks = [0.0] * len(values)
    position = 0
    while position < len(order):
        end = position + 1
        while end < len(order) and values[order[end]] == values[order[position]]:
            end += 1
        rank = (position + end - 1) / 2.0
        for index in order[position:end]:
            ranks[index] = rank
        position = end
    return ranks


def _temporal_diagnostics(
    centers: Mapping[str, Mapping[tuple[str, ...], float]],
) -> dict[str, Any]:
    """报告中心残差随采集时间的趋势与 lag-1 相关。"""

    runs = list(centers)
    identities = sorted({identity for rows in centers.values() for identity in rows})
    rows = []
    failed = []
    for identity in identities:
        values = [centers[run][identity] for run in runs if identity in centers[run]]
        if len(values) != len(runs):
            continue
        ordinal = [float(index) for index in range(len(values))]
        rank_correlation = _pearson_correlation(
            _rankdata(ordinal), _rankdata(values)
        )
        lag_one = _pearson_correlation(values[:-1], values[1:])
        trend_flag = (
            rank_correlation is not None and abs(rank_correlation) >= 0.30
        )
        dependence_flag = lag_one is not None and abs(lag_one) >= 0.30
        if trend_flag or dependence_flag:
            failed.append(identity)
        rows.append(
            {
                "identity": list(identity),
                "runs": len(values),
                "spearman_run_order": rank_correlation,
                "lag1_pearson": lag_one,
                "trend_threshold": 0.30,
                "dependence_threshold": 0.30,
                "stable": not (trend_flag or dependence_flag),
            }
        )
    return {
        "method": "run-order Spearman and lag-1 Pearson on robust residual centers",
        "contexts": rows,
        "stable": not failed,
        "failed_contexts": [list(identity) for identity in failed],
        "role": "diagnostic; forward coverage is the formal temporal gate",
    }


def _standardized_conformal_layer(
    *,
    train_centers: Mapping[str, Mapping[tuple[str, ...], float]],
    calibration_centers: Mapping[str, Mapping[tuple[str, ...], float]],
    test_centers: Mapping[str, Mapping[tuple[str, ...], float]],
    descriptors: Mapping[tuple[str, ...], Mapping[str, Any]],
    margins: Mapping[tuple[str, ...], float],
    confidence: float,
    minimum_test_runs: int,
    coverage_unit: str,
    center_definition: str,
) -> dict[str, Any]:
    """校准 run-max standardized score，并验证独立 test run。"""

    train_runs = list(train_centers)
    calibration_runs = list(calibration_centers)
    test_runs = list(test_centers)
    identities = set(descriptors)
    _validate_center_matrix(
        train_centers, runs=train_runs, identities=identities, owner="train"
    )
    _validate_center_matrix(
        calibration_centers,
        runs=calibration_runs,
        identities=identities,
        owner="calibration",
    )
    _validate_center_matrix(
        test_centers, runs=test_runs, identities=identities, owner="test"
    )
    scales = _robust_train_scales(train_centers, margins)

    def run_scores(
        centers: Mapping[str, Mapping[tuple[str, ...], float]],
    ) -> dict[str, float]:
        return {
            run: max(
                abs(rows[identity])
                / scales[identity]["conformal_normalizer_ns"]
                for identity in identities
            )
            for run, rows in centers.items()
        }

    calibration_scores = run_scores(calibration_centers)
    test_scores = run_scores(test_centers)
    temporal = _temporal_diagnostics(
        {**calibration_centers, **test_centers}
    )
    ordered_scores = sorted(calibration_scores.values())
    rank = _conformal_rank(len(ordered_scores), confidence)
    finite_gate = rank <= len(ordered_scores)
    quantile = ordered_scores[rank - 1] if finite_gate else None
    maximum_coverage = len(ordered_scores) / (len(ordered_scores) + 1)
    guaranteed_coverage = (
        rank / (len(ordered_scores) + 1) if finite_gate else None
    )
    quantile_tail_depth = (
        None if not finite_gate else len(ordered_scores) - rank + 1
    )
    center_rows = []
    informative = finite_gate
    for identity in sorted(identities):
        scale_row = scales[identity]
        half_width = (
            None
            if quantile is None
            else quantile * scale_row["conformal_normalizer_ns"]
        )
        if half_width is None or half_width > scale_row["equivalence_margin_ns"]:
            informative = False
        center_rows.append(
            {
                **descriptors[identity],
                **scale_row,
                "half_width_ns": half_width,
                "interval_width_ns": None if half_width is None else 2.0 * half_width,
            }
        )

    covered_runs = None
    covered_centers = None
    run_coverage = None
    center_coverage = None
    if quantile is not None:
        covered_runs = sum(score <= quantile for score in test_scores.values())
        covered_centers = sum(
            abs(rows[identity])
            <= quantile * scales[identity]["conformal_normalizer_ns"]
            for rows in test_centers.values()
            for identity in identities
        )
        run_coverage = covered_runs / len(test_runs)
        center_coverage = covered_centers / (len(test_runs) * len(identities))
    evidence_gate = _coverage_evidence_gate(
        covered_runs,
        len(test_runs),
        confidence=confidence,
        minimum_test_runs=minimum_test_runs,
    )
    coverage_lower_bound = (
        None
        if covered_runs is None
        else _clopper_pearson_lower_bound(
            covered_runs, len(test_runs), confidence=confidence
        )
    )
    widths = [row["interval_width_ns"] for row in center_rows]
    finite_widths = [float(value) for value in widths if value is not None]
    return {
        "status": "calibrated" if finite_gate else "insufficient-calibration-runs",
        "coverage_unit": coverage_unit,
        "score": (
            "run maximum of practical-margin-normalized robust-center residuals"
        ),
        "center": center_definition,
        "scale": (
            "nonconformity is normalized by the predeclared practical "
            "equivalence margin; train-only MAD/IQR are retained as "
            "distribution diagnostics"
        ),
        "finite_sample": {
            "calibration_runs": len(calibration_runs),
            "rank": rank,
            "maximum_achievable_finite_coverage": maximum_coverage,
            "guaranteed_coverage_lower_bound": guaranteed_coverage,
            "quantile_tail_depth": quantile_tail_depth,
            "quantile_is_calibration_maximum": (
                None if quantile_tail_depth is None else quantile_tail_depth == 1
            ),
            "gate_passed": finite_gate,
        },
        "calibration": {
            "run_scores": dict(sorted(calibration_scores.items())),
            "standardized_quantile": quantile,
            "median_score": statistics.median(ordered_scores),
            "maximum_score": max(ordered_scores),
            "sharpness_gate_passed": informative,
            "median_interval_width_ns": (
                None if not finite_widths else statistics.median(finite_widths)
            ),
            "maximum_interval_width_ns": (
                None if not finite_widths else max(finite_widths)
            ),
        },
        "centers": center_rows,
        "temporal_diagnostics": temporal,
        "test": {
            "runs": len(test_runs),
            "centers": len(test_runs) * len(identities),
            "covered_runs": covered_runs,
            "covered_centers": covered_centers,
            "run_coverage": run_coverage,
            "center_coverage": center_coverage,
            "run_coverage_wilson_interval": (
                None
                if covered_runs is None
                else _wilson_interval(
                    covered_runs, len(test_runs), confidence=confidence
                )
            ),
            "run_coverage_clopper_pearson_one_sided_lower": coverage_lower_bound,
            "coverage_evidence_rule": (
                "one-sided Clopper-Pearson lower bound at model confidence "
                "must reach target coverage"
            ),
            "run_scores": dict(sorted(test_scores.items())),
            "median_interval_width_ns": (
                None if not finite_widths else statistics.median(finite_widths)
            ),
            "maximum_interval_width_ns": (
                None if not finite_widths else max(finite_widths)
            ),
            "evidence_gate_passed": evidence_gate,
        },
    }


def _joint_conformal_family(
    structural: dict[str, Any],
    differential: dict[str, Any],
    *,
    confidence: float,
    minimum_test_runs: int,
    split_strategy: str,
) -> dict[str, Any]:
    """用一个 run-max score 同时覆盖 structural 与 differential 层。"""

    layers = [("structural", structural)]
    if differential.get("status") != "unavailable-no-differential-comparisons":
        layers.append(("differential", differential))
    calibration_by_layer = {
        name: dict(layer["calibration"]["run_scores"])
        for name, layer in layers
    }
    test_by_layer = {
        name: dict(layer["test"]["run_scores"])
        for name, layer in layers
    }
    calibration_runs = set(next(iter(calibration_by_layer.values())))
    test_runs = set(next(iter(test_by_layer.values())))
    if any(set(rows) != calibration_runs for rows in calibration_by_layer.values()):
        raise MlValidationError("联合 conformal calibration run 集合不一致")
    if any(set(rows) != test_runs for rows in test_by_layer.values()):
        raise MlValidationError("联合 conformal test run 集合不一致")

    calibration_scores = {
        run: max(rows[run] for rows in calibration_by_layer.values())
        for run in calibration_runs
    }
    test_scores = {
        run: max(rows[run] for rows in test_by_layer.values())
        for run in test_runs
    }
    ordered_scores = sorted(calibration_scores.values())
    rank = _conformal_rank(len(ordered_scores), confidence)
    finite = rank <= len(ordered_scores)
    quantile = ordered_scores[rank - 1] if finite else None
    quantile_tail_depth = (
        None if not finite else len(ordered_scores) - rank + 1
    )
    informative = finite
    widths: list[float] = []
    for _name, layer in layers:
        for center in layer["centers"]:
            width = (
                None
                if quantile is None
                else quantile * center["conformal_normalizer_ns"]
            )
            center["joint_family_half_width_ns"] = width
            if width is None or width > center["equivalence_margin_ns"]:
                informative = False
            if width is not None:
                widths.append(float(width))
    covered_runs = (
        None
        if quantile is None
        else sum(score <= quantile for score in test_scores.values())
    )
    evidence = _coverage_evidence_gate(
        covered_runs,
        len(test_runs),
        confidence=confidence,
        minimum_test_runs=minimum_test_runs,
    )
    return {
        "schema": "mygo.riscv-instruction-ml-joint-conformal-family.v1",
        "family": (
            "random-joint-structural-differential"
            if split_strategy == "random"
            else "chronological-joint-structural-differential"
        ),
        "combination": (
            "per-super-run maximum standardized nonconformity across layers"
        ),
        "included_layers": [name for name, _layer in layers],
        "target_coverage": confidence,
        "alpha": 1.0 - confidence,
        "finite_sample": {
            "calibration_runs": len(calibration_runs),
            "rank": rank,
            "maximum_achievable_finite_coverage": (
                len(calibration_runs) / (len(calibration_runs) + 1)
            ),
            "guaranteed_coverage_lower_bound": (
                rank / (len(calibration_runs) + 1) if finite else None
            ),
            "quantile_tail_depth": quantile_tail_depth,
            "quantile_is_calibration_maximum": (
                None
                if quantile_tail_depth is None
                else quantile_tail_depth == 1
            ),
            "gate_passed": finite,
        },
        "calibration": {
            "layer_run_scores": {
                name: dict(sorted(rows.items()))
                for name, rows in calibration_by_layer.items()
            },
            "run_scores": dict(sorted(calibration_scores.items())),
            "standardized_quantile": quantile,
            "sharpness_gate_passed": informative,
            "maximum_interval_width_ns": (
                None if not widths else 2.0 * max(widths)
            ),
        },
        "test": {
            "runs": len(test_runs),
            "layer_run_scores": {
                name: dict(sorted(rows.items()))
                for name, rows in test_by_layer.items()
            },
            "run_scores": dict(sorted(test_scores.items())),
            "covered_runs": covered_runs,
            "run_coverage": (
                None if covered_runs is None else covered_runs / len(test_runs)
            ),
            "run_coverage_clopper_pearson_one_sided_lower": (
                None
                if covered_runs is None
                else _clopper_pearson_lower_bound(
                    covered_runs, len(test_runs), confidence=confidence
                )
            ),
            "evidence_gate_passed": evidence,
        },
    }


def _evaluate_conformal_conclusion(
    *,
    actual_effect: float,
    predicted_effect: float,
    half_width: float | None,
    margin: float,
) -> tuple[str, str, bool | None, str]:
    """分别计算结论分类与数值覆盖，避免把 coverage miss 当成结论矛盾。"""

    observed_class = (
        "equivalent" if abs(actual_effect) <= margin else "context-dependent"
    )
    if half_width is None:
        interval_class = "inconclusive-no-finite-interval"
        covered = None
    else:
        lower = predicted_effect - half_width
        upper = predicted_effect + half_width
        covered = lower <= actual_effect <= upper
        if lower >= -margin and upper <= margin:
            interval_class = "equivalent"
        elif lower > margin or upper < -margin:
            interval_class = "context-dependent"
        else:
            interval_class = "inconclusive"

    definitive = {"equivalent", "context-dependent"}
    if observed_class == interval_class:
        check = "supported"
    elif observed_class in definitive and interval_class in definitive:
        check = "contradicted"
    else:
        check = "inconclusive"
    return observed_class, interval_class, covered, check


def _split_group_conformal(
    observations: Sequence[PairObservation],
    *,
    confidence: float,
    seed: int,
    max_iter: int,
    equivalence_absolute_ns: float,
    equivalence_relative: float,
    explicit_counts: tuple[int, int, int] | None,
    minimum_train_runs: int,
    minimum_test_runs: int,
    split_strategy: str = "random",
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """用互斥的完整 crossover super-run 训练、校准并测试共形区间。"""

    if split_strategy not in {"random", "chronological"}:
        raise MlValidationError("split_strategy 必须是 random 或 chronological")
    runs = _ordered_super_runs(observations)
    order_sources = {row.run_order_source for row in observations}
    if len(order_sources) != 1:
        raise MlValidationError("crossover super-run 的采集顺序来源不一致")
    order_source = next(iter(order_sources))
    required_calibration = _minimum_calibration_runs(confidence)
    empty_predictions = [
        {
            "role": None,
            "predicted_ns": None,
            "residual_ns": None,
            "lower_ns": None,
            "upper_ns": None,
            "covered": None,
        }
        for _ in observations
    ]
    common: dict[str, Any] = {
        "method": f"super-run-grouped {split_strategy} split conformal",
        "score": "super-run-max standardized robust-center residual",
        "target_coverage": confidence,
        "required_minimum_calibration_runs": required_calibration,
        "required_minimum_train_runs_for_high_confidence": minimum_train_runs,
        "required_minimum_test_runs_for_high_confidence": minimum_test_runs,
        "coverage_unit": (
            "a complete future ABBA/BAAB crossover super-run "
            "(all structural centers simultaneously)"
        ),
        "assumption": (
            "complete crossover super-runs are the independent clusters; random split "
            "additionally requires super-run exchangeability, chronological split "
            "evaluates forward-time drift"
        ),
        "split_strategy": split_strategy,
        "run_order_source": order_source,
    }
    if len(runs) < 3:
        return (
            {
                **common,
                "status": "insufficient-runs",
                "reason": "split-conformal 至少需要互斥的 train/calibration/test run",
                "split": {
                    "train_runs": [],
                    "calibration_runs": [],
                    "test_runs": [],
                },
                "finite_sample": {
                    "calibration_runs": 0,
                    "rank": None,
                    "maximum_achievable_finite_coverage": 0.0,
                    "guaranteed_coverage_lower_bound": None,
                    "gate_passed": False,
                },
                "test": {
                    "runs": 0,
                    "pairs": 0,
                    "run_coverage": None,
                    "pair_coverage": None,
                    "run_coverage_wilson_interval": None,
                    "interval_width_ns": None,
                    "evidence_gate_passed": False,
                },
                "high_confidence_gate": {
                    "passed": False,
                    "failed_checks": ["three-way-run-split"],
                },
            },
            empty_predictions,
        )

    if explicit_counts is None:
        train_count, calibration_count, test_count = _automatic_conformal_split_counts(
            len(runs),
            required_calibration_runs=required_calibration,
            confidence=confidence,
            minimum_train_runs=minimum_train_runs,
            minimum_test_runs=minimum_test_runs,
        )
        split_source = "automatic-deterministic-counts"
    else:
        train_count, calibration_count, test_count = explicit_counts
        if (
            min(explicit_counts) <= 0
            or train_count + calibration_count + test_count != len(runs)
        ):
            raise MlValidationError(
                "显式 conformal train/calibration/test run 数必须均为正且总和等于数据 run 数"
            )
        split_source = "explicit-counts"

    assigned_runs = list(runs)
    split_seed: int | None = None
    if split_strategy == "random":
        split_seed = seed ^ 0x53504C4954
        random.Random(split_seed).shuffle(assigned_runs)
    train_runs = assigned_runs[:train_count]
    calibration_runs = assigned_runs[train_count : train_count + calibration_count]
    test_runs = assigned_runs[train_count + calibration_count :]
    if not train_runs or not calibration_runs or not test_runs:
        raise MlValidationError(
            "split-conformal 三个分组都必须包含至少一个完整 crossover super-run"
        )
    if (
        set(train_runs) & set(calibration_runs)
        or set(train_runs) & set(test_runs)
        or set(calibration_runs) & set(test_runs)
    ):
        raise MlValidationError("split-conformal crossover super-run 分组发生泄漏")

    role_by_run = {
        **{run: "train" for run in train_runs},
        **{run: "calibration" for run in calibration_runs},
        **{run: "test" for run in test_runs},
    }
    features = [_feature(row) for row in observations]
    target = [row.response_ns for row in observations]
    indices_by_role: dict[str, list[int]] = defaultdict(list)
    for index, row in enumerate(observations):
        indices_by_role[role_by_run[row.super_run_id]].append(index)

    ml = _load_sklearn()
    model = _new_model(ml, seed=seed ^ 0x434F4E46, max_iter=max_iter)
    train_indices = indices_by_role["train"]
    model.fit(
        [features[index] for index in train_indices],
        [target[index] for index in train_indices],
    )
    split_predictions = empty_predictions
    for role in ("calibration", "test"):
        indices = indices_by_role[role]
        estimates = model.predict([features[index] for index in indices])
        for index, estimate in zip(indices, estimates, strict=True):
            prediction = float(estimate)
            split_predictions[index] = {
                "role": role,
                "predicted_ns": prediction,
                "residual_ns": target[index] - prediction,
                "lower_ns": None,
                "upper_ns": None,
                "covered": None,
            }
    for index in train_indices:
        split_predictions[index]["role"] = "train"

    heldout_predictions = {
        index: float(split_predictions[index]["predicted_ns"])
        for role in ("calibration", "test")
        for index in indices_by_role[role]
    }
    train_oof_predictions = _subset_group_oof_predictions(
        observations,
        train_indices,
        features=features,
        target=target,
        ml=ml,
        seed=seed ^ 0x5343414C,
        max_iter=max_iter,
    )

    train_context_centers = _context_residual_centers(
        observations, train_indices, train_oof_predictions
    )
    calibration_context_centers = _context_residual_centers(
        observations, indices_by_role["calibration"], heldout_predictions
    )
    test_context_centers = _context_residual_centers(
        observations, indices_by_role["test"], heldout_predictions
    )
    context_values: dict[tuple[str, ...], list[float]] = defaultdict(list)
    context_descriptors: dict[tuple[str, ...], dict[str, Any]] = {}
    for index in train_indices:
        row = observations[index]
        identity = _context_identity(row)
        context_values[identity].append(row.response_ns)
        context_descriptors[identity] = {
            **_context_descriptor(identity),
            "instruction": row.instruction,
        }
    context_margins = {
        identity: max(
            equivalence_absolute_ns,
            equivalence_relative * abs(statistics.median(values)),
        )
        for identity, values in context_values.items()
    }
    structural = _standardized_conformal_layer(
        train_centers=train_context_centers,
        calibration_centers=calibration_context_centers,
        test_centers=test_context_centers,
        descriptors=context_descriptors,
        margins=context_margins,
        confidence=confidence,
        minimum_test_runs=minimum_test_runs,
        coverage_unit=(
            "all super-run×context centers in one future complete crossover super-run"
        ),
        center_definition="median pair residual within each run×context",
    )

    (
        train_differential_centers,
        _train_actual_differential,
        _train_predicted_differential,
    ) = _differential_residual_centers(
        observations, train_indices, train_oof_predictions
    )
    (
        calibration_differential_centers,
        _calibration_actual_differential,
        _calibration_predicted_differential,
    ) = _differential_residual_centers(
        observations, indices_by_role["calibration"], heldout_predictions
    )
    (
        test_differential_centers,
        test_actual_differential,
        test_predicted_differential,
    ) = _differential_residual_centers(
        observations, indices_by_role["test"], heldout_predictions
    )
    differential_identities = sorted(
        {
            identity
            for rows in train_differential_centers.values()
            for identity in rows
        }
    )
    if differential_identities:
        reference_values: dict[tuple[str, str], list[float]] = defaultdict(list)
        for index in train_indices:
            row = observations[index]
            identity = _differential_identity(row)
            if identity is None or identity[1] != "reference":
                continue
            reference_values[(row.suite or "legacy", identity[0])].append(
                row.response_ns
            )
        differential_descriptors = {
            identity: {
                "suite": None if identity[0] == "legacy" else identity[0],
                "group": identity[1],
                "variant": identity[2],
            }
            for identity in differential_identities
        }
        differential_margins = {
            identity: max(
                equivalence_absolute_ns,
                equivalence_relative
                * abs(
                    statistics.median(reference_values[(identity[0], identity[1])])
                ),
            )
            for identity in differential_identities
        }
        differential = _standardized_conformal_layer(
            train_centers=train_differential_centers,
            calibration_centers=calibration_differential_centers,
            test_centers=test_differential_centers,
            descriptors=differential_descriptors,
            margins=differential_margins,
            confidence=confidence,
            minimum_test_runs=minimum_test_runs,
            coverage_unit=(
                "all matched differential effects in one future complete crossover super-run"
            ),
            center_definition=(
                "median matched observed effect minus median matched predicted effect "
                "within each run×comparison"
            ),
        )
        half_width_by_identity = {
            identity: row["half_width_ns"]
            for identity, row in zip(
                differential_identities, differential["centers"], strict=True
            )
        }
        conclusion_details = []
        for run in test_runs:
            for identity in differential_identities:
                actual_effect = test_actual_differential[run][identity]
                predicted_effect = test_predicted_differential[run][identity]
                margin = differential_margins[identity]
                half_width = half_width_by_identity[identity]
                (
                    observed_class,
                    interval_class,
                    covered,
                    check,
                ) = _evaluate_conformal_conclusion(
                    actual_effect=actual_effect,
                    predicted_effect=predicted_effect,
                    half_width=half_width,
                    margin=margin,
                )
                conclusion_details.append(
                    {
                        **differential_descriptors[identity],
                        "run_id": run,
                        "actual_effect_ns": actual_effect,
                        "predicted_effect_ns": predicted_effect,
                        "half_width_ns": half_width,
                        "equivalence_margin_ns": margin,
                        "observed_conclusion": observed_class,
                        "conformal_interval_conclusion": interval_class,
                        "actual_covered": covered,
                        "conclusion_check": check,
                    }
                )
        direct_checks = [row["conclusion_check"] for row in conclusion_details]
        direct_gate = (
            len(test_runs) >= minimum_test_runs
            and bool(direct_checks)
            and all(value == "supported" for value in direct_checks)
        )
        differential["conclusion_validation"] = {
            "status": (
                "supported"
                if direct_gate
                else (
                    "contradicted"
                    if "contradicted" in direct_checks
                    else "inconclusive"
                )
            ),
            "test_runs": len(test_runs),
            "comparisons_per_run": len(differential_identities),
            "supported": direct_checks.count("supported"),
            "inconclusive": direct_checks.count("inconclusive"),
            "contradicted": direct_checks.count("contradicted"),
            "gate_passed": direct_gate,
            "details": conclusion_details,
        }
    else:
        differential = {
            "status": "unavailable-no-differential-comparisons",
            "finite_sample": {"gate_passed": True},
            "calibration": {"sharpness_gate_passed": True},
            "test": {"evidence_gate_passed": True},
            "centers": [],
            "conclusion_validation": {
                "status": "not-applicable",
                "gate_passed": True,
                "details": [],
            },
        }

    joint_family = _joint_conformal_family(
        structural,
        differential,
        confidence=confidence,
        minimum_test_runs=minimum_test_runs,
        split_strategy=split_strategy,
    )
    if differential_identities:
        joint_half_width_by_identity = {
            identity: row["joint_family_half_width_ns"]
            for identity, row in zip(
                differential_identities, differential["centers"], strict=True
            )
        }
        joint_conclusion_details = []
        for run in test_runs:
            for identity in differential_identities:
                actual_effect = test_actual_differential[run][identity]
                predicted_effect = test_predicted_differential[run][identity]
                margin = differential_margins[identity]
                half_width = joint_half_width_by_identity[identity]
                observed_class, interval_class, covered, check = (
                    _evaluate_conformal_conclusion(
                        actual_effect=actual_effect,
                        predicted_effect=predicted_effect,
                        half_width=half_width,
                        margin=margin,
                    )
                )
                joint_conclusion_details.append(
                    {
                        **differential_descriptors[identity],
                        "run_id": run,
                        "actual_effect_ns": actual_effect,
                        "predicted_effect_ns": predicted_effect,
                        "half_width_ns": half_width,
                        "equivalence_margin_ns": margin,
                        "observed_conclusion": observed_class,
                        "conformal_interval_conclusion": interval_class,
                        "actual_covered": covered,
                        "conclusion_check": check,
                    }
                )
        joint_checks = [row["conclusion_check"] for row in joint_conclusion_details]
        joint_direct_gate = (
            len(test_runs) >= minimum_test_runs
            and bool(joint_checks)
            and all(value == "supported" for value in joint_checks)
        )
        joint_family["differential_conclusion_validation"] = {
            "status": (
                "supported"
                if joint_direct_gate
                else (
                    "contradicted"
                    if "contradicted" in joint_checks
                    else "inconclusive"
                )
            ),
            "interval_source": "joint-structural-differential-family-quantile",
            "test_runs": len(test_runs),
            "comparisons_per_run": len(differential_identities),
            "supported": joint_checks.count("supported"),
            "inconclusive": joint_checks.count("inconclusive"),
            "contradicted": joint_checks.count("contradicted"),
            "gate_passed": joint_direct_gate,
            "details": joint_conclusion_details,
        }
    else:
        joint_family["differential_conclusion_validation"] = {
            "status": "not-applicable",
            "gate_passed": True,
            "details": [],
        }

    # 单 pair 尾部层仍然有诊断价值，但其最大值容易被极少数计时毛刺主导，
    # 与“上下文权重中心”不是同一个 estimand，因此绝不参与结构门禁。
    pair_scores_by_role: dict[str, dict[str, float]] = {}
    for role in ("calibration", "test"):
        residuals_by_run: dict[str, list[float]] = defaultdict(list)
        for index in indices_by_role[role]:
            residual = split_predictions[index]["residual_ns"]
            if not isinstance(residual, float):
                raise MlValidationError("split-conformal 预测残差缺失")
            residuals_by_run[observations[index].super_run_id].append(
                abs(residual)
            )
        pair_scores_by_role[role] = {
            run: max(values) for run, values in residuals_by_run.items()
        }
    pair_calibration_scores = sorted(pair_scores_by_role["calibration"].values())
    pair_rank = _conformal_rank(len(pair_calibration_scores), confidence)
    pair_half_width = (
        pair_calibration_scores[pair_rank - 1]
        if pair_rank <= len(pair_calibration_scores)
        else None
    )
    pair_covered_runs = None
    pair_covered_pairs = None
    if pair_half_width is not None:
        pair_covered_runs = sum(
            score <= pair_half_width for score in pair_scores_by_role["test"].values()
        )
        pair_covered_pairs = 0
        for role in ("calibration", "test"):
            for index in indices_by_role[role]:
                prediction = split_predictions[index]["predicted_ns"]
                if not isinstance(prediction, float):
                    raise MlValidationError("split-conformal held-out 预测缺失")
                covered = abs(target[index] - prediction) <= pair_half_width
                if role == "test":
                    pair_covered_pairs += covered
                split_predictions[index].update(
                    {
                        "lower_ns": prediction - pair_half_width,
                        "upper_ns": prediction + pair_half_width,
                        "covered": covered if role == "test" else None,
                    }
                )
    test_run_count = len(test_runs)
    test_pair_count = len(indices_by_role["test"])
    pair_diagnostic = {
        "status": (
            "finite-diagnostic"
            if pair_half_width is not None
            else "insufficient-calibration-runs"
        ),
        "estimand": "individual pair response; not a structural weight center",
        "score": "maximum absolute pair residual per complete crossover super-run",
        "calibration": {
            "runs": len(calibration_runs),
            "rank": pair_rank,
            "run_scores_ns": dict(sorted(pair_scores_by_role["calibration"].items())),
            "half_width_ns": pair_half_width,
            "interval_width_ns": (
                None if pair_half_width is None else 2.0 * pair_half_width
            ),
        },
        "test": {
            "runs": test_run_count,
            "pairs": test_pair_count,
            "covered_runs": pair_covered_runs,
            "covered_pairs": pair_covered_pairs,
            "run_coverage": (
                None if pair_covered_runs is None else pair_covered_runs / test_run_count
            ),
            "pair_coverage": (
                None if pair_covered_pairs is None else pair_covered_pairs / test_pair_count
            ),
            "run_scores_ns": dict(sorted(pair_scores_by_role["test"].items())),
        },
        "warning": "仅诊断 pair 尾部噪声，不参与结构置信度或权重发布门禁",
    }

    finite_sample_gate = joint_family["finite_sample"]["gate_passed"]
    joint_test_gate = joint_family["test"]["evidence_gate_passed"]
    checks = {
        "minimum_train_runs": len(train_runs) >= minimum_train_runs,
        "joint_finite_sample_calibration": finite_sample_gate,
        "joint_informative_interval": joint_family["calibration"][
            "sharpness_gate_passed"
        ],
        "independent_joint_test_evidence": joint_test_gate,
        "differential_conclusion_validation": joint_family[
            "differential_conclusion_validation"
        ]["gate_passed"],
    }
    if split_strategy == "chronological":
        checks["forward_structural_temporal_stability"] = structural[
            "temporal_diagnostics"
        ]["stable"]
        checks["forward_differential_temporal_stability"] = differential.get(
            "temporal_diagnostics", {"stable": True}
        )["stable"]
    failed_checks = [name for name, passed in checks.items() if not passed]
    status = "calibrated" if finite_sample_gate else "insufficient-calibration-runs"
    structural_max_width = structural["calibration"]["maximum_interval_width_ns"]
    structural_half_width = (
        None if structural_max_width is None else structural_max_width / 2.0
    )
    return (
        {
            **common,
            "score": (
                "formal: run-max standardized median residual by context; "
                "pair maximum retained only as diagnostic"
            ),
            "coverage_unit": structural["coverage_unit"],
            "status": status,
            "split": {
                "source": split_source,
                "strategy": split_strategy,
                "seed": split_seed,
                "run_order_source": order_source,
                "train_runs": train_runs,
                "calibration_runs": calibration_runs,
                "test_runs": test_runs,
                "train_pairs": len(train_indices),
                "calibration_pairs": len(indices_by_role["calibration"]),
                "test_pairs": test_pair_count,
                "leakage_check_passed": True,
            },
            "finite_sample": {
                "calibration_runs": len(calibration_runs),
                "rank": joint_family["finite_sample"]["rank"],
                "maximum_achievable_finite_coverage": joint_family[
                    "finite_sample"
                ]["maximum_achievable_finite_coverage"],
                "guaranteed_coverage_lower_bound": joint_family["finite_sample"][
                    "guaranteed_coverage_lower_bound"
                ],
                "quantile_tail_depth": joint_family["finite_sample"][
                    "quantile_tail_depth"
                ],
                "quantile_is_calibration_maximum": joint_family[
                    "finite_sample"
                ]["quantile_is_calibration_maximum"],
                "gate_passed": finite_sample_gate,
                "exchangeability_required": True,
                "guarantee_status": (
                    "unconditional-under-exchangeability"
                    if split_strategy == "random"
                    else "conditional-forward-validation-not-exchangeable-conformal"
                ),
                "explanation": (
                    "rank=ceil((n_calibration+1)*target_coverage) 必须不超过 "
                    "n_calibration；门禁针对未来完整 crossover super-run 的同时覆盖"
                ),
            },
            "calibration": {
                "run_scores": joint_family["calibration"]["run_scores"],
                "standardized_quantile": joint_family["calibration"][
                    "standardized_quantile"
                ],
                "half_width_ns": structural_half_width,
                "interval_width_ns": joint_family["calibration"][
                    "maximum_interval_width_ns"
                ],
                "median_interval_width_ns": structural["calibration"][
                    "median_interval_width_ns"
                ],
                "sharpness_gate_passed": structural["calibration"][
                    "sharpness_gate_passed"
                ],
            },
            "test": {
                "runs": test_run_count,
                "centers": structural["test"]["centers"],
                "covered_runs": joint_family["test"]["covered_runs"],
                "covered_centers": structural["test"]["covered_centers"],
                "run_coverage": joint_family["test"]["run_coverage"],
                "center_coverage": structural["test"]["center_coverage"],
                "run_coverage_wilson_interval": structural["test"][
                    "run_coverage_wilson_interval"
                ],
                "run_coverage_clopper_pearson_one_sided_lower": joint_family[
                    "test"
                ]["run_coverage_clopper_pearson_one_sided_lower"],
                "coverage_evidence_rule": structural["test"][
                    "coverage_evidence_rule"
                ],
                "interval_width_ns": structural["test"][
                    "maximum_interval_width_ns"
                ],
                "run_scores": joint_family["test"]["run_scores"],
                "evidence_gate_passed": joint_test_gate,
                "warning": (
                    "test 覆盖率是独立诊断；少于要求的 test run 时不得据此宣称验证充分"
                ),
            },
            "structural": structural,
            "differential_effects": differential,
            "joint_family": joint_family,
            "pair_level_diagnostic": pair_diagnostic,
            "high_confidence_gate": {
                "passed": not failed_checks,
                "checks": checks,
                "failed_checks": failed_checks,
                "warning": (
                    "通过只表示在独立 run、交换性和当前等价阈值下预测校准充分；"
                    "不能单独证明统计权重或外部平台有效性"
                ),
            },
        },
        split_predictions,
    )


def _load_statistical_weights(path: Path | None) -> dict[tuple[str, str, str], dict[str, Any]]:
    if path is None:
        return {}
    document = json.loads(path.read_text(encoding="utf-8"))
    gate = document.get("publication_gate")
    if not isinstance(gate, Mapping):
        raise MlValidationError("weights JSON 缺少显式 publication_gate")
    instructions = document.get("instructions")
    if not isinstance(instructions, list):
        raise MlValidationError("weights JSON 缺少 instructions")
    output = {}
    for item in instructions:
        key = item.get("key", {})
        identity = (
            str(key.get("semantic_encoding_key", "")),
            str(key.get("encoding_key", "")),
            str(key.get("pattern", "")),
        )
        if not all(identity):
            raise MlValidationError("weights instruction key 不完整")
        output[identity] = item
    return output


def _context_rows(
    observations: Sequence[PairObservation],
    predicted: Sequence[float] | None,
    statistical: Mapping[tuple[str, str, str], Mapping[str, Any]],
    *,
    confidence: float,
    bootstrap_replicates: int,
    minimum_runs: int,
    equivalence_absolute_ns: float,
    equivalence_relative: float,
    seed: int,
) -> list[dict[str, Any]]:
    # 差分标签属于实验上下文；不能只按 mnemonic/raw encoding 合并，
    # 否则同一条指令在不同 suite 中会互相污染 OOF 诊断。
    grouped: dict[tuple[str, str, str, str, str, str, str], list[int]] = defaultdict(list)
    for index, row in enumerate(observations):
        grouped[
            (
                row.semantic_key,
                row.raw_key,
                row.pattern,
                row.suite or "",
                row.contrast or "",
                row.differential_variant or "",
                row.context or "",
            )
        ].append(index)
    output = []
    for context_index, (identity, indices) in enumerate(sorted(grouped.items())):
        rows = [observations[index] for index in indices]
        actual = [row.response_ns for row in rows]
        estimates = None if predicted is None else [predicted[index] for index in indices]
        residuals_by_run: dict[str, list[float]] = defaultdict(list)
        if estimates is not None:
            for row, estimate in zip(rows, estimates, strict=True):
                residuals_by_run[row.super_run_id].append(
                    estimate - row.response_ns
                )
        bias, bias_interval = (
            (None, None)
            if not residuals_by_run
            else _cluster_bootstrap_interval(
                {
                    run: residuals_by_run[run]
                    for run in _ordered_super_runs(rows)
                    if run in residuals_by_run
                },
                confidence=confidence,
                replicates=bootstrap_replicates,
                seed=seed + context_index,
            )
        )
        statistical_row = statistical.get(identity[:3])
        statistical_point = None if statistical_row is None else (
            statistical_row.get("anchor_adjusted", {}).get(
                "ns_per_instruction"
            )
            if isinstance(statistical_row.get("anchor_adjusted"), Mapping)
            else None
        )
        margin = max(
            equivalence_absolute_ns,
            equivalence_relative
            * abs(
                float(statistical_point)
                if isinstance(statistical_point, (int, float))
                else statistics.median(actual)
            ),
        )
        if len({row.super_run_id for row in rows}) < minimum_runs:
            conclusion = "inconclusive-insufficient-runs"
        elif bias_interval is None:
            conclusion = "inconclusive-no-prediction-interval"
        elif bias_interval[0] >= -margin and bias_interval[1] <= margin:
            conclusion = "consistent"
        elif bias_interval[0] > margin or bias_interval[1] < -margin:
            conclusion = "contradicted"
        else:
            conclusion = "inconclusive"
        output.append(
            {
                "semantic_key": identity[0],
                "raw_key": identity[1],
                "pattern": identity[2],
                "suite": identity[3] or None,
                "contrast": identity[4] or None,
                "differential_variant": identity[5] or None,
                "context": identity[6] or None,
                "instruction": rows[0].instruction,
                "runs": len({row.super_run_id for row in rows}),
                "pairs": len(rows),
                "observed_median_ns": statistics.median(actual),
                "ml_oof_median_ns": None if estimates is None else statistics.median(estimates),
                "ml_oof_mae_ns": (
                    None
                    if estimates is None
                    else math.fsum(abs(a - b) for a, b in zip(actual, estimates))
                    / len(actual)
                ),
                "ml_bias_ns": bias,
                "ml_bias_cluster_ci": bias_interval,
                "equivalence_margin_ns": margin,
                "statistical_point_ns": statistical_point,
                "statistical_simultaneous_ci": (
                    None
                    if statistical_row is None
                    or not isinstance(
                        statistical_row.get("anchor_adjusted"), Mapping
                    )
                    else statistical_row["anchor_adjusted"].get(
                        "simultaneous_ci"
                    )
                ),
                "statistical_raw_point_ns": (
                    None
                    if statistical_row is None
                    else statistical_row.get("ns_per_instruction")
                ),
                "statistical_raw_simultaneous_ci": (
                    None
                    if statistical_row is None
                    else statistical_row.get("simultaneous_ci")
                ),
                "statistical_quality": (
                    None if statistical_row is None else statistical_row.get("quality")
                ),
                "conclusion_check": conclusion,
            }
        )
    return output


def _differential_identity(row: PairObservation) -> tuple[str, str] | None:
    if row.suite in CALIBRATION_ONLY_SUITES:
        return None
    if row.contrast is not None and row.context is not None:
        if row.differential_variant is not None:
            return row.contrast, row.differential_variant
        # v2 初始 schema 没有显式 variant；只接受约定明确的 reference
        # pattern，避免根据输出顺序或点估计大小猜基线。
        if row.pattern in {
            "dependency-chain",
            "homogeneous-dependency",
            "homogeneous-reset",
        }:
            return row.contrast, "reference"
        return row.contrast, row.context
    if not row.pattern.startswith(DIFFERENTIAL_PREFIX):
        return None
    parts = row.pattern.split(":")
    if len(parts) != 3 or not parts[1] or not parts[2]:
        raise MlValidationError(
            f"差分 pattern={row.pattern!r} 必须为 diff:<group>:<variant>"
        )
    return parts[1], parts[2]


def _differential_rows(
    observations: Sequence[PairObservation],
    predicted: Sequence[float] | None,
    *,
    confidence: float,
    bootstrap_replicates: int,
    minimum_runs: int,
    equivalence_absolute_ns: float,
    equivalence_relative: float,
    seed: int,
) -> list[dict[str, Any]]:
    values: dict[
        tuple[str, str, str, str, int, str], list[tuple[float, float | None]]
    ] = defaultdict(list)
    reference_center: dict[tuple[str, str], list[float]] = defaultdict(list)
    for index, row in enumerate(observations):
        identity = _differential_identity(row)
        if identity is None:
            continue
        group, variant = identity
        estimate = None if predicted is None else predicted[index]
        suite = row.suite or "legacy"
        values[
            (
                suite,
                group,
                variant,
                row.super_run_id,
                row.batch,
                row.block_id,
            )
        ].append((row.response_ns, estimate))
        if variant == "reference":
            reference_center[(suite, group)].append(row.response_ns)

    variants: dict[tuple[str, str], set[str]] = defaultdict(set)
    for suite, group, variant, _run, _batch, _block in values:
        variants[(suite, group)].add(variant)
    output = []
    comparison_index = 0
    for suite, group in sorted(variants):
        if "reference" not in variants[(suite, group)]:
            raise MlValidationError(f"差分组 {group!r} 缺少 reference")
        margin = max(
            equivalence_absolute_ns,
            equivalence_relative
            * abs(statistics.median(reference_center[(suite, group)])),
        )
        for variant in sorted(variants[(suite, group)] - {"reference"}):
            observed_by_run: dict[str, list[float]] = defaultdict(list)
            predicted_by_run: dict[str, list[float]] = defaultdict(list)
            matched_batches: set[tuple[str, int]] = set()
            keys = {
                (run, batch, block)
                for current_suite, current_group, current_variant, run, batch, block in values
                if current_suite == suite
                and current_group == group
                and current_variant == variant
            }
            for run, batch, block in sorted(keys):
                reference = values.get(
                    (suite, group, "reference", run, batch, block)
                )
                treatment = values.get(
                    (suite, group, variant, run, batch, block)
                )
                if not reference or not treatment:
                    continue
                matched_batches.add((run, batch))
                reference_observed = statistics.median(item[0] for item in reference)
                treatment_observed = statistics.median(item[0] for item in treatment)
                observed_by_run[run].append(treatment_observed - reference_observed)
                if predicted is not None and all(
                    item[1] is not None for item in reference + treatment
                ):
                    reference_prediction = statistics.median(
                        float(item[1]) for item in reference
                    )
                    treatment_prediction = statistics.median(
                        float(item[1]) for item in treatment
                    )
                    predicted_by_run[run].append(
                        treatment_prediction - reference_prediction
                    )
            if not observed_by_run:
                raise MlValidationError(
                    f"差分组 {group}/{variant} 没有配对 run/batch/block"
                )
            observed_effect, observed_interval = _cluster_bootstrap_interval(
                {
                    run: observed_by_run[run]
                    for run in _ordered_super_runs(observations)
                    if run in observed_by_run
                },
                confidence=confidence,
                replicates=bootstrap_replicates,
                seed=seed + comparison_index * 2,
            )
            predicted_effect, predicted_interval = (
                (None, None)
                if not predicted_by_run
                else _cluster_bootstrap_interval(
                    {
                        run: predicted_by_run[run]
                        for run in _ordered_super_runs(observations)
                        if run in predicted_by_run
                    },
                    confidence=confidence,
                    replicates=bootstrap_replicates,
                    seed=seed + comparison_index * 2 + 1,
                )
            )
            comparison_index += 1
            run_count = len(observed_by_run)
            if run_count < minimum_runs:
                observed_class = "inconclusive-insufficient-runs"
            elif observed_interval is None:
                observed_class = "inconclusive"
            elif observed_interval[0] >= -margin and observed_interval[1] <= margin:
                observed_class = "equivalent"
            elif observed_interval[0] > margin or observed_interval[1] < -margin:
                observed_class = "context-dependent"
            else:
                observed_class = "inconclusive"
            if predicted_effect is None or observed_interval is None:
                ml_check = "inconclusive"
            elif abs(predicted_effect - observed_effect) <= margin:
                ml_check = "supported"
            elif (
                observed_class == "context-dependent"
                and predicted_effect * observed_effect < 0.0
            ):
                ml_check = "contradicted"
            else:
                ml_check = "inconclusive"
            output.append(
                {
                    "group": group,
                    "suite": None if suite == "legacy" else suite,
                    "reference": "reference",
                    "variant": variant,
                    "runs": run_count,
                    "matched_run_batches": len(matched_batches),
                    "matched_run_blocks": sum(
                        len(rows) for rows in observed_by_run.values()
                    ),
                    "equivalence_margin_ns": margin,
                    "observed_effect_ns": observed_effect,
                    "observed_effect_cluster_ci": observed_interval,
                    "observed_conclusion": observed_class,
                    "ml_oof_effect_ns": predicted_effect,
                    "ml_oof_effect_cluster_ci": predicted_interval,
                    "ml_conclusion_check": ml_check,
                }
            )
    return output


def validate_predictions(
    samples: Sequence[Mapping[str, Any]],
    *,
    statistical_weights: Mapping[tuple[str, str, str], Mapping[str, Any]] | None = None,
    input_bindings: Mapping[str, Any] | None = None,
    folds: int = 6,
    max_iter: int = 160,
    confidence: float = 0.95,
    bootstrap_replicates: int = 999,
    minimum_runs: int = 20,
    minimum_skill_improvement: float = 0.10,
    omitted_structure_equivalence_ns: float = 0.15,
    minimum_incremental_improvement_ns: float | None = None,
    equivalence_absolute_ns: float = 0.15,
    equivalence_relative: float = 0.10,
    conformal_train_runs: int | None = None,
    conformal_calibration_runs: int | None = None,
    conformal_test_runs: int | None = None,
    conformal_minimum_train_runs: int = 20,
    conformal_minimum_test_runs: int = 20,
    seed: int = 0x4D4C5256,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """训练非线性预测器，并用完全留出的 crossover super-run 检查结论。"""

    if (
        folds < 0
        or max_iter <= 0
        or bootstrap_replicates < 0
        or minimum_runs <= 0
        or conformal_minimum_train_runs <= 0
        or conformal_minimum_test_runs <= 0
        or not math.isfinite(minimum_skill_improvement)
        or not 0.0 <= minimum_skill_improvement <= 1.0
    ):
        raise MlValidationError("folds/bootstrap/minimum_runs/max_iter 参数非法")
    if not 0.0 < confidence < 1.0:
        raise MlValidationError("confidence 必须位于 (0, 1)")
    if minimum_incremental_improvement_ns is not None:
        if (
            not math.isfinite(minimum_incremental_improvement_ns)
            or minimum_incremental_improvement_ns < 0.0
        ):
            raise MlValidationError("minimum_incremental_improvement_ns 参数非法")
        omitted_structure_equivalence_ns = minimum_incremental_improvement_ns
    if (
        not math.isfinite(omitted_structure_equivalence_ns)
        or omitted_structure_equivalence_ns < 0.0
    ):
        raise MlValidationError("omitted_structure_equivalence_ns 参数非法")
    explicit_conformal_values = (
        conformal_train_runs,
        conformal_calibration_runs,
        conformal_test_runs,
    )
    if any(value is not None for value in explicit_conformal_values) and not all(
        isinstance(value, int) and not isinstance(value, bool)
        for value in explicit_conformal_values
    ):
        raise MlValidationError(
            "显式 conformal 分组必须同时给出 train/calibration/test run 数"
        )
    explicit_conformal_counts = (
        None
        if all(value is None for value in explicit_conformal_values)
        else (
            int(conformal_train_runs),
            int(conformal_calibration_runs),
            int(conformal_test_runs),
        )
    )
    all_observations = pair_observations(samples)
    calibration_only_observations = [
        row for row in all_observations if row.suite in CALIBRATION_ONLY_SUITES
    ]
    observations = [
        row for row in all_observations if row.suite not in CALIBRATION_ONLY_SUITES
    ]
    if not observations:
        raise MlValidationError("排除 calibration-only anchor 后没有可用样本")
    (
        predicted,
        dummy_predicted,
        context_batch_predicted,
        fold_rows,
        versions,
    ) = _cross_validated_predictions(
        observations, folds=folds, seed=seed, max_iter=max_iter
    )
    actual = [row.response_ns for row in observations]
    ml_metrics = None if predicted is None else _regression_metrics(actual, predicted)
    dummy_metrics = (
        None if dummy_predicted is None else _regression_metrics(actual, dummy_predicted)
    )
    context_batch_metrics = (
        None
        if context_batch_predicted is None
        else _regression_metrics(actual, context_batch_predicted)
    )
    global_dummy_skill = (
        None
        if ml_metrics is None
        or dummy_metrics is None
        or dummy_metrics["mae_ns"] == 0.0
        else 1.0 - ml_metrics["mae_ns"] / dummy_metrics["mae_ns"]
    )
    skill = (
        None
        if ml_metrics is None
        or context_batch_metrics is None
        or context_batch_metrics["mae_ns"] == 0.0
        else 1.0 - ml_metrics["mae_ns"] / context_batch_metrics["mae_ns"]
    )
    incremental_value = _incremental_prediction_value(
        observations,
        actual,
        predicted,
        context_batch_predicted,
        confidence=confidence,
        bootstrap_replicates=bootstrap_replicates,
        minimum_relative_improvement=minimum_skill_improvement,
        practical_equivalence_ns=omitted_structure_equivalence_ns,
        seed=seed + 30_000,
    )
    context_rows = _context_rows(
        observations,
        predicted,
        statistical_weights or {},
        confidence=confidence,
        bootstrap_replicates=bootstrap_replicates,
        minimum_runs=minimum_runs,
        equivalence_absolute_ns=equivalence_absolute_ns,
        equivalence_relative=equivalence_relative,
        seed=seed + 10_000,
    )
    differential_rows = _differential_rows(
        observations,
        predicted,
        confidence=confidence,
        bootstrap_replicates=bootstrap_replicates,
        minimum_runs=minimum_runs,
        equivalence_absolute_ns=equivalence_absolute_ns,
        equivalence_relative=equivalence_relative,
        seed=seed + 20_000,
    )
    split_conformal, split_predictions = _split_group_conformal(
        observations,
        confidence=confidence,
        seed=seed,
        max_iter=max_iter,
        equivalence_absolute_ns=equivalence_absolute_ns,
        equivalence_relative=equivalence_relative,
        explicit_counts=explicit_conformal_counts,
        minimum_train_runs=conformal_minimum_train_runs,
        minimum_test_runs=conformal_minimum_test_runs,
        split_strategy="random",
    )
    chronological_conformal, _chronological_predictions = _split_group_conformal(
        observations,
        confidence=confidence,
        seed=seed,
        max_iter=max_iter,
        equivalence_absolute_ns=equivalence_absolute_ns,
        equivalence_relative=equivalence_relative,
        explicit_counts=explicit_conformal_counts,
        minimum_train_runs=conformal_minimum_train_runs,
        minimum_test_runs=conformal_minimum_test_runs,
        split_strategy="chronological",
    )
    runs = _ordered_super_runs(observations)
    differential_checks = [row["ml_conclusion_check"] for row in differential_rows]
    context_checks = [row["conclusion_check"] for row in context_rows]
    contradicted = "contradicted" in differential_checks or "contradicted" in context_checks
    omitted_structure_interpretation = incremental_value["interpretation"]
    # 以下 conclusion 保留为 OOF 探索诊断；它不包含重训不确定性，因此不能
    # 影响 formal high-confidence gate。
    if len(runs) < minimum_runs:
        conclusion = "inconclusive-insufficient-independent-runs"
    elif omitted_structure_interpretation == (
        "practically-material-omitted-structure-detected"
    ):
        conclusion = "contradicted-practically-material-omitted-structure"
    elif omitted_structure_interpretation == (
        "flexible-model-materially-worse-than-structured-baseline"
    ):
        conclusion = "inconclusive-flexible-model-underperforms-baseline"
    elif omitted_structure_interpretation != (
        "no-practically-material-omitted-structure"
    ):
        conclusion = "inconclusive-omitted-structure-equivalence"
    elif contradicted:
        conclusion = "contradicted"
    elif (
        differential_checks
        and all(value == "supported" for value in differential_checks)
        and all(value == "consistent" for value in context_checks)
    ):
        conclusion = "supported"
    else:
        conclusion = "inconclusive"
    split_conformal_gate = split_conformal["high_confidence_gate"]["passed"]
    chronological_conformal_gate = chronological_conformal[
        "high_confidence_gate"
    ]["passed"]
    high_confidence_gate = (
        split_conformal_gate
        and chronological_conformal_gate
    )
    high_confidence_conclusion = (
        "supported"
        if high_confidence_gate
        else "inconclusive-ml-high-confidence-gate"
    )
    predictions = [
        {
            "run_id": row.run_id,
            "super_run_id": row.super_run_id,
            "pair_id": row.pair_id,
            "batch": row.batch,
            "semantic_key": row.semantic_key,
            "raw_key": row.raw_key,
            "pattern": row.pattern,
            "suite": row.suite,
            "contrast": row.contrast,
            "differential_variant": row.differential_variant,
            "context": row.context,
            "observed_ns": row.response_ns,
            "ml_oof_predicted_ns": None if predicted is None else predicted[index],
            "context_batch_median_oof_predicted_ns": (
                None
                if context_batch_predicted is None
                else context_batch_predicted[index]
            ),
            "residual_ns": (
                None if predicted is None else row.response_ns - predicted[index]
            ),
            "split_conformal_role": split_predictions[index]["role"],
            "split_conformal_predicted_ns": split_predictions[index]["predicted_ns"],
            "split_conformal_residual_ns": split_predictions[index]["residual_ns"],
            "split_conformal_lower_ns": split_predictions[index]["lower_ns"],
            "split_conformal_upper_ns": split_predictions[index]["upper_ns"],
            "split_conformal_covered": split_predictions[index]["covered"],
        }
        for index, row in enumerate(observations)
    ]
    result = {
        "schema": OUTPUT_SCHEMA,
        "purpose": "independent-run prediction check; never a source of published weights",
        "input_bindings": dict(input_bindings or {}),
        "configuration": {
            "model": "one-hot-hist-gradient-boosting-absolute-error",
            "cv": "GroupKFold by complete ABBA/BAAB crossover super-run",
            "conformal": (
                "disjoint train/calibration/test complete-crossover-super-run split"
            ),
            "folds_requested": folds,
            "max_iter": max_iter,
            "confidence": confidence,
            "bootstrap_replicates": bootstrap_replicates,
            "minimum_independent_super_runs": minimum_runs,
            # 旧字段仅为 JSON 兼容；主门禁改用下方 context+batch 字段。
            "minimum_skill_improvement_over_median": minimum_skill_improvement,
            "minimum_skill_improvement_over_context_batch": minimum_skill_improvement,
            "omitted_structure_equivalence_ns": omitted_structure_equivalence_ns,
            # 旧字段为 JSON 兼容，现表示双侧实践等价带半宽。
            "minimum_incremental_improvement_ns": omitted_structure_equivalence_ns,
            "equivalence_absolute_ns": equivalence_absolute_ns,
            "equivalence_relative": equivalence_relative,
            "conformal_explicit_run_counts": (
                None
                if explicit_conformal_counts is None
                else {
                    "train": explicit_conformal_counts[0],
                    "calibration": explicit_conformal_counts[1],
                    "test": explicit_conformal_counts[2],
                }
            ),
            "conformal_minimum_train_runs": conformal_minimum_train_runs,
            "conformal_minimum_test_runs": conformal_minimum_test_runs,
            "seed": seed,
            "dependencies": versions,
        },
        "data": {
            "runs": len(runs),
            "super_run_ids": runs,
            "pairs": len(observations),
            "calibration_only_pairs_excluded": len(
                calibration_only_observations
            ),
            "calibration_only_suites": sorted(CALIBRATION_ONLY_SUITES),
            "contexts": len(context_rows),
            "differential_comparisons": len(differential_rows),
        },
        "cross_validation": {
            "available": predicted is not None,
            "folds": fold_rows,
            "ml": ml_metrics,
            "median_dummy": dummy_metrics,
            "context_batch_median": context_batch_metrics,
            # 旧字段保持相对全局 median 的原语义，禁止作为主要证据。
            "mae_skill_improvement": global_dummy_skill,
            "primary_mae_skill_improvement": skill,
            "mae_skill_improvement_over_context_batch": skill,
            "mae_skill_improvement_over_global_median": global_dummy_skill,
            "incremental_value": incremental_value,
            # 保留旧键名以兼容现有 JSON 消费者；其内容现已是真正互斥的
            # split-conformal，不再复用 OOF 残差。
            "group_conformal": split_conformal,
            "split_conformal": split_conformal,
            "chronological_split_conformal": chronological_conformal,
        },
        "contexts": context_rows,
        "differential_checks": differential_rows,
        "conclusion": {
            "status": high_confidence_conclusion,
            "high_confidence_status": high_confidence_conclusion,
            "high_confidence_gate_passed": high_confidence_gate,
            "high_confidence_gate_components": {
                "random_joint_conformal_family": split_conformal_gate,
                "chronological_joint_conformal_family": chronological_conformal_gate,
            },
            "predictive_interpretation": incremental_value["interpretation"],
            "diagnostic_status": conclusion,
            "formal_gate_basis": (
                "random and chronological joint split-conformal families only; "
                "fixed-OOF bootstrap diagnostics are excluded"
            ),
            "may_publish_weights": False,
            "context_checks": context_checks,
            "differential_checks": differential_checks,
            "rule": (
                "ML 只检测统计结论；split-conformal 高置信门禁通过也不得将预测升级为正式权重"
            ),
        },
    }
    result["diagnostic_familywise_error_control"] = _diagnostic_fwer_document(
        confidence
    )
    result["prediction_evidence"] = _prediction_evidence(predictions)
    expected_publication_split = (
        PUBLICATION_TRAIN_SUPER_RUNS,
        PUBLICATION_CALIBRATION_SUPER_RUNS,
        PUBLICATION_TEST_SUPER_RUNS,
    )
    if (
        len(runs) == PUBLICATION_SUPER_RUNS
        and folds == PUBLICATION_FOLDS
        and max_iter == PUBLICATION_MAX_ITER
        and confidence == PUBLICATION_FAMILY_CONFIDENCE
        and bootstrap_replicates == PUBLICATION_BOOTSTRAP_REPLICATES
        and minimum_runs == PUBLICATION_MINIMUM_RUNS
        and minimum_skill_improvement
        == PUBLICATION_MINIMUM_SKILL_IMPROVEMENT
        and omitted_structure_equivalence_ns
        == PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS
        and equivalence_absolute_ns == PUBLICATION_EQUIVALENCE_ABSOLUTE_NS
        and equivalence_relative == PUBLICATION_EQUIVALENCE_RELATIVE
        and explicit_conformal_counts == expected_publication_split
        and conformal_minimum_train_runs
        == PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
        and conformal_minimum_test_runs
        == PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS
        and seed == PUBLICATION_SEED
    ):
        result["publication_policy"] = _publication_policy_document()
        result["publication_familywise_error_control"] = (
            _publication_fwer_document()
        )
    json.dumps(result, allow_nan=False)
    return result, predictions


def _write_csv(path: Path, rows: Sequence[Mapping[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if not rows:
        path.write_text("\n", encoding="utf-8")
        return
    fieldnames = list(rows[0])
    with path.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", help="微基准 samples.jsonl")
    parser.add_argument("--weights", help="可选的统计 weights.json")
    parser.add_argument("--output", required=True)
    parser.add_argument("--contexts-csv")
    parser.add_argument("--predictions-csv")
    parser.add_argument(
        "--finalize-weights",
        action="store_true",
        help="复验输入绑定并把 ML 结论并入 weights.json 最终发布门禁",
    )
    parser.add_argument("--folds", type=int, default=6)
    parser.add_argument("--max-iter", type=int, default=160)
    parser.add_argument(
        "--confidence",
        type=float,
        default=0.95,
        help="普通诊断的逐族置信度；固定 publication replay 忽略此参数",
    )
    parser.add_argument("--bootstrap", type=int, default=999)
    parser.add_argument("--minimum-runs", type=int, default=20)
    parser.add_argument("--minimum-skill-improvement", type=float, default=0.10)
    parser.add_argument(
        "--omitted-structure-equivalence-ns",
        "--minimum-incremental-improvement-ns",
        dest="omitted_structure_equivalence_ns",
        type=float,
        default=0.15,
        help="HGB 与 context+batch 基线改善 CI 的双侧实践等价带半宽",
    )
    parser.add_argument(
        "--conformal-train-runs",
        type=int,
        help="显式指定 split-conformal train run 数（须同时指定另两组）",
    )
    parser.add_argument(
        "--conformal-calibration-runs",
        type=int,
        help="显式指定 split-conformal calibration run 数",
    )
    parser.add_argument(
        "--conformal-test-runs",
        type=int,
        help="显式指定 split-conformal honest test run 数",
    )
    parser.add_argument("--seed", type=int, default=0x4D4C5256)
    arguments = parser.parse_args(argv)
    samples_path = Path(arguments.input)
    weights_path = None if arguments.weights is None else Path(arguments.weights)
    if arguments.finalize_weights and weights_path is None:
        raise MlValidationError("--finalize-weights 要求同时提供 --weights")
    input_bindings = {
        "samples": _artifact_identity(samples_path),
        **(
            {}
            if weights_path is None
            else {
                "statistical_weights_pre_finalization": _artifact_identity(
                    weights_path
                )
            }
        ),
    }
    statistical = _load_statistical_weights(
        weights_path
    )
    result, predictions = validate_predictions(
        load_samples(samples_path),
        statistical_weights=statistical,
        input_bindings=input_bindings,
        folds=arguments.folds,
        max_iter=arguments.max_iter,
        confidence=arguments.confidence,
        bootstrap_replicates=arguments.bootstrap,
        minimum_runs=arguments.minimum_runs,
        minimum_skill_improvement=arguments.minimum_skill_improvement,
        omitted_structure_equivalence_ns=arguments.omitted_structure_equivalence_ns,
        conformal_train_runs=arguments.conformal_train_runs,
        conformal_calibration_runs=arguments.conformal_calibration_runs,
        conformal_test_runs=arguments.conformal_test_runs,
        seed=arguments.seed,
    )
    output = Path(arguments.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    if arguments.contexts_csv:
        _write_csv(Path(arguments.contexts_csv), result["contexts"])
    if arguments.predictions_csv:
        _write_csv(Path(arguments.predictions_csv), predictions)
    publication_passed = None
    if arguments.finalize_weights:
        assert weights_path is not None
        publication_passed = finalize_publication_gate(
            weights_path=weights_path,
            samples_path=samples_path,
            validation_path=output,
        )
    print(
        "riscv instruction ML validation: "
        f"runs={result['data']['runs']} pairs={result['data']['pairs']} "
        f"status={result['conclusion']['status']} "
        f"publication={publication_passed} output={output}"
    )
    return 1 if publication_passed is False else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (MlValidationError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"riscv instruction ML validation: {error}", file=__import__("sys").stderr)
        raise SystemExit(1)
