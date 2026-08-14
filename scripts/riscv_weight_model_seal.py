#!/usr/bin/env python3
"""为最终 RISC-V 指令权重文档生成并复验规范化内容封印。"""

from __future__ import annotations

import hashlib
import hmac
import json
import math
from collections.abc import Mapping, MutableMapping
from typing import Any


SEAL_FIELD = "publication_seal"
SEAL_SCHEMA = "mygo.riscv-instruction-weight-publication-seal.v2"
CANONICALIZATION = "utf8-json-sort-keys-compact-no-nan-v1"
ALGORITHM = "sha256"
FWER_FAMILIES = (
    "raw-absolute-costs",
    "diagnostic-nuisance-effects",
    "auxiliary-clock-consistency",
    "joint-adjusted-anchor-sensitivity",
)
FWER_INFERENCE_FIELDS = (
    "simultaneous_inference",
    "diagnostic_simultaneous_inference",
    "auxiliary_consistency_inference",
    "joint_raw_adjusted_inference",
)
PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES = 4999
PUBLICATION_MINIMUM_MAX_STAT_CALIBRATION_REPLICATES = 4000
PUBLICATION_MAX_STAT_SCALE_REPLICATES = 999
ERROR_BUDGET_FRACTION = 0.5
FWER_METHOD = (
    "union-bound-across-pre-registered-max-stat-families-with-split-"
    "sampling-and-monte-carlo-error-budgets"
)
FWER_COVERAGE_CLAIM = (
    "unconditional family intersection failure probability is bounded by "
    "sampling_alpha_budget plus finite-bootstrap monte_carlo_alpha_budget "
    "under the stated bootstrap model"
)
MONTE_CARLO_METHOD = (
    "one-sided-binomial-order-statistic-upper-confidence-bound"
)
REPLICATE_PARTITION_METHOD = (
    "ordered-independent-prefix-scale-remainder-quantile-v1"
)
INFERENCE_REPLICATE_FIELDS = {
    "simultaneous_inference": ("complete_family_replicates",),
    "diagnostic_simultaneous_inference": (
        "complete_family_replicates",
    ),
    "auxiliary_consistency_inference": ("complete_family_replicates",),
    "joint_raw_adjusted_inference": (
        "complete_replicates",
    ),
}


class ModelSealError(ValueError):
    """最终权重文档缺少封印或内容与封印不一致。"""


def _finite_number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    result = float(value)
    return result if math.isfinite(result) else None


def _plain_integer(value: Any) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value


def _required_binomial_upper_rank(
    count: int, probability: float, confidence: float
) -> int:
    """复算经验分位数的单侧保守 rank，避免信任 JSON 中的缓存。"""

    log_probabilities = [
        math.lgamma(count + 1)
        - math.lgamma(successes + 1)
        - math.lgamma(count - successes + 1)
        + successes * math.log(probability)
        + (count - successes) * math.log1p(-probability)
        for successes in range(count + 1)
    ]
    log_peak = max(log_probabilities)
    probabilities = [math.exp(value - log_peak) for value in log_probabilities]
    normalization = math.fsum(probabilities)
    cumulative = 0.0
    for successes, mass in enumerate(probabilities):
        cumulative += mass / normalization
        if cumulative >= confidence:
            return successes + 1
    return count + 1


def _verify_finite_bootstrap_evidence(
    inference: Mapping[str, Any],
    *,
    sampling_confidence: float,
    monte_carlo_confidence: float,
    owner: str,
) -> None:
    requested = _plain_integer(inference.get("requested_replicates"))
    evidence = inference.get("critical_value_monte_carlo")
    if (
        requested is None
        or requested < PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES
        or not isinstance(evidence, Mapping)
        or set(evidence)
        != {
            "method",
            "target_probability",
            "monte_carlo_confidence",
            "replicates",
            "required_rank",
            "selected_rank",
            "finite_rank_supported",
            "replicate_partition_method",
            "complete_family_replicates",
            "scale_replicates",
            "quantile_replicates",
        }
    ):
        raise ModelSealError(f"权重模型 {owner} 缺少正式 finite-bootstrap 证据")
    replicates = _plain_integer(evidence.get("replicates"))
    required_rank = _plain_integer(evidence.get("required_rank"))
    selected_rank = _plain_integer(evidence.get("selected_rank"))
    complete_family = _plain_integer(
        evidence.get("complete_family_replicates")
    )
    scale_replicates = _plain_integer(evidence.get("scale_replicates"))
    quantile_replicates = _plain_integer(
        evidence.get("quantile_replicates")
    )
    target = _finite_number(evidence.get("target_probability"))
    mc_confidence = _finite_number(evidence.get("monte_carlo_confidence"))
    if (
        evidence.get("method") != MONTE_CARLO_METHOD
        or evidence.get("replicate_partition_method")
        != REPLICATE_PARTITION_METHOD
        or evidence.get("finite_rank_supported") is not True
        or replicates is None
        or replicates
        < PUBLICATION_MINIMUM_MAX_STAT_CALIBRATION_REPLICATES
        or replicates > requested
        or complete_family is None
        # 丢失的 replicate 可能正好来自最极端、最难拟合的重采样。只对
        # 成功子集校准尾部分位数会破坏下面 finite-binomial rank 的证明，
        # 因此正式模型必须让每个预注册 family 闭合全部请求的 replicate。
        or complete_family != requested
        or scale_replicates != PUBLICATION_MAX_STAT_SCALE_REPLICATES
        or quantile_replicates is None
        or quantile_replicates
        < PUBLICATION_MINIMUM_MAX_STAT_CALIBRATION_REPLICATES
        or scale_replicates + quantile_replicates != complete_family
        or replicates != quantile_replicates
        or required_rank is None
        or selected_rank != required_rank
        or not 1 <= required_rank <= replicates
        or target is None
        or mc_confidence is None
        or not math.isclose(target, sampling_confidence, abs_tol=1.0e-12)
        or not math.isclose(
            mc_confidence, monte_carlo_confidence, abs_tol=1.0e-12
        )
    ):
        raise ModelSealError(f"权重模型 {owner} finite-bootstrap 证据非法")
    for field in INFERENCE_REPLICATE_FIELDS[owner]:
        count = _plain_integer(inference.get(field))
        if count != complete_family:
            raise ModelSealError(
                f"权重模型 {owner} complete replicate 计数不一致"
            )
    expected_max_stat_field = {
        "simultaneous_inference": "complete_max_statistic_replicates",
        "diagnostic_simultaneous_inference": "complete_replicates",
        "auxiliary_consistency_inference": "valid_replicates",
        "joint_raw_adjusted_inference": "complete_max_statistic_replicates",
    }[owner]
    max_stat_count = _plain_integer(
        inference.get(expected_max_stat_field)
    )
    if max_stat_count != quantile_replicates:
        raise ModelSealError(f"权重模型 {owner} max-stat replicate 计数不一致")
    if required_rank != _required_binomial_upper_rank(
        replicates, target, mc_confidence
    ):
        raise ModelSealError(f"权重模型 {owner} finite-bootstrap rank 非法")


def verify_publication_fwer_contract(document: Mapping[str, Any]) -> None:
    """复验跨发布族的整体 FWER 分配，而不是只信任内容摘要。"""

    control = document.get("publication_familywise_error_control")
    if not isinstance(control, Mapping) or set(control) != {
        "method",
        "overall_confidence",
        "overall_alpha",
        "sampling_alpha_budget",
        "monte_carlo_alpha_budget",
        "families",
        "family_count",
        "sampling_alpha_per_family",
        "sampling_confidence_per_family",
        "monte_carlo_alpha_per_family",
        "monte_carlo_confidence_per_family",
        "coverage_claim",
    }:
        raise ModelSealError("权重模型缺少完整 publication FWER 合同")
    overall_confidence = _finite_number(control.get("overall_confidence"))
    overall_alpha = _finite_number(control.get("overall_alpha"))
    sampling_alpha = _finite_number(control.get("sampling_alpha_budget"))
    monte_carlo_alpha = _finite_number(
        control.get("monte_carlo_alpha_budget")
    )
    family_sampling_alpha = _finite_number(
        control.get("sampling_alpha_per_family")
    )
    family_sampling_confidence = _finite_number(
        control.get("sampling_confidence_per_family")
    )
    family_monte_carlo_alpha = _finite_number(
        control.get("monte_carlo_alpha_per_family")
    )
    family_monte_carlo_confidence = _finite_number(
        control.get("monte_carlo_confidence_per_family")
    )
    if (
        control.get("method") != FWER_METHOD
        or control.get("coverage_claim") != FWER_COVERAGE_CLAIM
        or control.get("families") != list(FWER_FAMILIES)
        or control.get("family_count") != len(FWER_FAMILIES)
        or overall_confidence is None
        or overall_alpha is None
        or sampling_alpha is None
        or monte_carlo_alpha is None
        or family_sampling_alpha is None
        or family_sampling_confidence is None
        or family_monte_carlo_alpha is None
        or family_monte_carlo_confidence is None
        or not 0.0 < overall_confidence < 1.0
        or overall_alpha > 0.05 + 1.0e-12
        or not math.isclose(
            overall_alpha, 1.0 - overall_confidence, abs_tol=1.0e-12
        )
        or not math.isclose(
            sampling_alpha + monte_carlo_alpha, overall_alpha,
            abs_tol=1.0e-12,
        )
        or not math.isclose(
            sampling_alpha,
            overall_alpha * ERROR_BUDGET_FRACTION,
            abs_tol=1.0e-12,
        )
        or not math.isclose(
            monte_carlo_alpha,
            overall_alpha * ERROR_BUDGET_FRACTION,
            abs_tol=1.0e-12,
        )
        or not math.isclose(
            family_sampling_alpha * len(FWER_FAMILIES),
            sampling_alpha,
            abs_tol=1.0e-12,
        )
        or not math.isclose(
            family_monte_carlo_alpha * len(FWER_FAMILIES),
            monte_carlo_alpha,
            abs_tol=1.0e-12,
        )
        or not math.isclose(
            family_sampling_confidence,
            1.0 - family_sampling_alpha,
            abs_tol=1.0e-12,
        )
        or not math.isclose(
            family_monte_carlo_confidence,
            1.0 - family_monte_carlo_alpha,
            abs_tol=1.0e-12,
        )
    ):
        raise ModelSealError("权重模型 publication FWER 分配非法")
    if not math.isclose(
        _finite_number(document.get("confidence")) or math.nan,
        overall_confidence,
        abs_tol=1.0e-12,
    ):
        raise ModelSealError("权重模型总体 confidence 与 FWER 合同不一致")
    for field in FWER_INFERENCE_FIELDS:
        inference = document.get(field)
        if (
            not isinstance(inference, Mapping)
            or not math.isclose(
                _finite_number(inference.get("familywise_confidence"))
                or math.nan,
                family_sampling_confidence,
                abs_tol=1.0e-12,
            )
        ):
            raise ModelSealError(f"权重模型 {field} 未遵守 publication FWER 分配")
        _verify_finite_bootstrap_evidence(
            inference,
            sampling_confidence=family_sampling_confidence,
            monte_carlo_confidence=family_monte_carlo_confidence,
            owner=field,
        )


def _canonical_payload(document: Mapping[str, Any]) -> bytes:
    payload = {key: value for key, value in document.items() if key != SEAL_FIELD}
    try:
        text = json.dumps(
            payload,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        )
    except (TypeError, ValueError) as error:
        raise ModelSealError("权重文档不能规范化为有限 JSON") from error
    return text.encode("utf-8")


def seal_model_document(document: MutableMapping[str, Any]) -> dict[str, Any]:
    """覆盖旧封印，并返回绑定当前完整 finalized payload 的新封印。"""

    document.pop(SEAL_FIELD, None)
    gate = document.get("publication_gate")
    if not isinstance(gate, Mapping) or gate.get("passed") is not True:
        raise ModelSealError("只有通过最终 publication gate 的模型才能封印")
    verify_publication_fwer_contract(document)
    payload = _canonical_payload(document)
    seal = {
        "schema": SEAL_SCHEMA,
        "algorithm": ALGORITHM,
        "canonicalization": CANONICALIZATION,
        "payload_sha256": hashlib.sha256(payload).hexdigest(),
        "payload_size": len(payload),
    }
    document[SEAL_FIELD] = seal
    return seal


def verify_model_document_seal(document: Mapping[str, Any]) -> None:
    """严格复验封印；未知字段、旧 schema 和 payload 漂移均拒绝。"""

    gate = document.get("publication_gate")
    if not isinstance(gate, Mapping) or gate.get("passed") is not True:
        raise ModelSealError("权重模型 publication gate 未通过")
    verify_publication_fwer_contract(document)
    seal = document.get(SEAL_FIELD)
    expected_fields = {
        "schema",
        "algorithm",
        "canonicalization",
        "payload_sha256",
        "payload_size",
    }
    if not isinstance(seal, Mapping) or set(seal) != expected_fields:
        raise ModelSealError("权重模型缺少完整 publication seal")
    if (
        seal.get("schema") != SEAL_SCHEMA
        or seal.get("algorithm") != ALGORITHM
        or seal.get("canonicalization") != CANONICALIZATION
    ):
        raise ModelSealError("权重模型 publication seal 协议不受支持")
    digest = seal.get("payload_sha256")
    size = seal.get("payload_size")
    if (
        not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
        or isinstance(size, bool)
        or not isinstance(size, int)
        or size <= 0
    ):
        raise ModelSealError("权重模型 publication seal 字段非法")
    payload = _canonical_payload(document)
    if len(payload) != size or not hmac.compare_digest(
        hashlib.sha256(payload).hexdigest(), digest
    ):
        raise ModelSealError("权重模型 finalized payload 与 publication seal 不一致")
