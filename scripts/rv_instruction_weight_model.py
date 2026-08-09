#!/usr/bin/env python3
"""从 epoch 级 RISC-V 画像估计逐指令耗时权重。

模型只依赖 Python 标准库，供 BuildStorm 报告生成器直接导入。输入中的
``vcpu_task_clock_ns`` 是响应变量；逐指令精确计数是主解释变量，执行 TB 数、
翻译指令增量、epoch 持续时间和可辨识的截距是 nuisance 变量。

输入应先按规范化 mnemonic 聚合；QEMU 将部分 16/32-bit 编码反汇编为同一
mnemonic（例如 2/4-byte ``addi``），模型有意为它们估计同一个权重，调用方
再把该权重回填到各编码尺寸变体。

估计器使用非负 Huber IRLS + family 分层 ridge。稀疏、低暴露或与其他列
共线的指令会向实测 family prior 收缩，不会用任意常数（尤其不是 1.0）补值。
时间相关不确定性由 moving-block bootstrap 和带 purge gap 的 blocked CV 给出。
所有公开返回值均可直接 JSON 序列化。
"""

from __future__ import annotations

import math
import random
import statistics
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any


NANOSECONDS_PER_SECOND = 1_000_000_000
MODEL_SCHEMA_VERSION = 1


class WeightModelError(ValueError):
    """表示输入不满足指令耗时估计的统计约束。"""


@dataclass(frozen=True)
class _Epoch:
    """经过校验的单个 epoch。"""

    time_ns: int
    duration_ns: int
    counts: dict[str, float]
    task_clock_ns: float
    task_clock_source: str
    external_nuisance: dict[str, float]


@dataclass
class _CoreFit:
    """内部拟合结果，系数已经恢复为原始量纲。"""

    instruction_weights: dict[str, float]
    family_priors: dict[str, float]
    nuisance_coefficients: dict[str, float]
    shrinkage: dict[str, float]
    robust_weights: list[float]
    converged: bool
    robust_weight_rms_change: float
    irls_iterations: int
    coordinate_sweeps: int


def _field(row: Any, name: str, default: Any = None) -> Any:
    if isinstance(row, Mapping):
        return row.get(name, default)
    return getattr(row, name, default)


def _first_field(row: Any, names: Sequence[str]) -> tuple[str | None, Any]:
    sentinel = object()
    for name in names:
        value = _field(row, name, sentinel)
        if value is not sentinel:
            return name, value
    return None, None


def _finite_nonnegative(value: Any, owner: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise WeightModelError(f"{owner} 必须是非负有限数")
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0.0:
        raise WeightModelError(f"{owner} 必须是非负有限数")
    return parsed


def _positive_int(value: Any, owner: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise WeightModelError(f"{owner} 必须是正整数")
    return value


def _normalise_tid(value: Any) -> str:
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise WeightModelError("vCPU TID 必须是整数或字符串")
    result = str(value).strip()
    if not result:
        raise WeightModelError("vCPU TID 不能为空")
    return result


def _sum_task_clock_mapping(
    value: Any,
    owner: str,
    *,
    allowed_tids: set[str] | None,
    mapping_is_already_vcpu_only: bool,
) -> tuple[float, str]:
    if not isinstance(value, Mapping):
        return _finite_nonnegative(value, owner), "preaggregated-vcpu-task-clock"
    if allowed_tids is None and not mapping_is_already_vcpu_only:
        raise WeightModelError(
            f"{owner} 是线程 mapping，必须传入由 jitdump/catalog 映射得到的 vcpu_tids"
        )
    total = 0.0
    matched = 0
    for raw_tid, raw_clock in value.items():
        tid = _normalise_tid(raw_tid)
        clock = _finite_nonnegative(raw_clock, f"{owner}[{tid!r}]")
        if allowed_tids is None or tid in allowed_tids:
            total += clock
            matched += 1
    if allowed_tids is not None and matched == 0:
        raise WeightModelError(f"{owner} 没有匹配任何显式 vCPU TID")
    source = (
        "explicit-jit-vcpu-tids"
        if allowed_tids is not None
        else "preselected-vcpu-tid-map"
    )
    return total, source


def _parse_task_clock(
    row: Any, owner: str, allowed_tids: set[str] | None
) -> tuple[float, str]:
    field_name, value = _first_field(
        row,
        (
            "vcpu_task_clock_ns",
            "vcpu_task_clock_by_tid_ns",
            "task_clock_by_tid_ns",
            "task_clock_ns_by_tid",
            "task_clock_ns",
        ),
    )
    if field_name is None:
        raise WeightModelError(f"{owner} 缺少 vCPU task-clock")
    already_vcpu = field_name.startswith("vcpu_")
    if isinstance(value, Mapping):
        return _sum_task_clock_mapping(
            value,
            f"{owner}.{field_name}",
            allowed_tids=allowed_tids,
            mapping_is_already_vcpu_only=already_vcpu,
        )
    parsed = _finite_nonnegative(value, f"{owner}.{field_name}")
    source = (
        "preaggregated-vcpu-task-clock"
        if already_vcpu
        else "caller-asserted-vcpu-task-clock"
    )
    return parsed, source


_NUISANCE_ALIASES: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "executed_tb_count",
        ("executed_tb_count", "tb_exec_count", "tb_count", "tb_executions"),
    ),
    (
        "translated_tb_delta",
        (
            "translated_tb_delta",
            "translation_tb_delta",
            "translated_tbs_delta",
            "translated_tb_count",
        ),
    ),
    (
        "translated_insns_delta",
        (
            "translated_insns_delta",
            "translated_delta",
            "translated_insns",
            "translation_insns_delta",
        ),
    ),
)


def _freeze_epochs(
    rows: Sequence[Any], vcpu_tids: Iterable[str | int] | None
) -> tuple[list[_Epoch], list[str], list[str]]:
    if not rows:
        raise WeightModelError("epoch_rows 不能为空")
    allowed_tids = (
        {_normalise_tid(value) for value in vcpu_tids}
        if vcpu_tids is not None
        else None
    )
    if allowed_tids is not None and not allowed_tids:
        raise WeightModelError("vcpu_tids 不能为空集合")

    frozen: list[_Epoch] = []
    previous_time = -1
    nuisance_presence: dict[str, list[bool]] = {
        canonical: [] for canonical, _ in _NUISANCE_ALIASES
    }
    clock_sources: set[str] = set()
    for index, row in enumerate(rows):
        owner = f"epoch_rows[{index}]"
        raw_time = _field(row, "time_ns")
        if isinstance(raw_time, bool) or not isinstance(raw_time, int) or raw_time < 0:
            raise WeightModelError(f"{owner}.time_ns 必须是非负整数")
        if raw_time <= previous_time:
            raise WeightModelError("epoch_rows 必须按 time_ns 严格递增")
        previous_time = raw_time
        duration_ns = _positive_int(
            _field(row, "duration_ns", NANOSECONDS_PER_SECOND),
            f"{owner}.duration_ns",
        )

        count_field, raw_counts = _first_field(row, ("exact_counts", "counts"))
        if count_field is None or not isinstance(raw_counts, Mapping):
            raise WeightModelError(f"{owner} 缺少 exact_counts mapping")
        counts: dict[str, float] = {}
        for raw_name, raw_count in raw_counts.items():
            if not isinstance(raw_name, str) or not raw_name.strip():
                raise WeightModelError(f"{owner}.{count_field} 包含非法指令名")
            name = raw_name.strip().lower()
            count = _finite_nonnegative(
                raw_count, f"{owner}.{count_field}[{raw_name!r}]"
            )
            if count > 0.0:
                counts[name] = counts.get(name, 0.0) + count

        task_clock_ns, clock_source = _parse_task_clock(row, owner, allowed_tids)
        clock_sources.add(clock_source)
        external: dict[str, float] = {}
        for canonical, aliases in _NUISANCE_ALIASES:
            field_name, raw_value = _first_field(row, aliases)
            present = field_name is not None
            nuisance_presence[canonical].append(present)
            if present:
                external[canonical] = _finite_nonnegative(
                    raw_value, f"{owner}.{field_name}"
                )
        frozen.append(
            _Epoch(
                time_ns=raw_time,
                duration_ns=duration_ns,
                counts=counts,
                task_clock_ns=task_clock_ns,
                task_clock_source=clock_source,
                external_nuisance=external,
            )
        )

    for name, presence in nuisance_presence.items():
        if any(presence) and not all(presence):
            raise WeightModelError(
                f"nuisance {name!r} 只能在所有 epoch 都存在或都不存在"
            )
    if math.fsum(row.task_clock_ns for row in frozen) <= 0.0:
        raise WeightModelError("vCPU task-clock 总量必须大于零")
    if math.fsum(math.fsum(row.counts.values()) for row in frozen) <= 0.0:
        raise WeightModelError("逐指令精确计数总量必须大于零")
    active_nuisance = [
        name for name, presence in nuisance_presence.items() if all(presence)
    ]
    return frozen, active_nuisance, sorted(clock_sources)


def _base_mnemonic(mnemonic: str) -> tuple[str, bool]:
    token = mnemonic.strip().lower().split(maxsplit=1)[0]
    compressed = token.startswith("c.")
    if compressed:
        token = token[2:]
    return token, compressed


def instruction_family(mnemonic: str) -> str:
    """把 GNU/QEMU 风格 RISC-V mnemonic 映射到稳定的成本 family。"""

    if not isinstance(mnemonic, str) or not mnemonic.strip():
        raise WeightModelError("mnemonic 必须是非空字符串")
    op, _compressed = _base_mnemonic(mnemonic)

    if op.startswith("amo") or op.startswith("lr.") or op.startswith("sc."):
        return "atomic"
    if op in {
        "beq",
        "bne",
        "blt",
        "bge",
        "bltu",
        "bgeu",
        "beqz",
        "bnez",
        "bgt",
        "ble",
        "bgtu",
        "bleu",
        "bltz",
        "bgez",
        "blez",
        "bgtz",
    }:
        return "conditional-branch"
    if op in {
        "j",
        "jal",
        "jalr",
        "jr",
        "ret",
        "call",
        "tail",
    }:
        return "indirect-or-unconditional-branch"
    if op.startswith(("sfence", "hfence")) or op in {"fence", "fence.i"}:
        return "fence"
    if op.startswith(("csr", "csrr")) or op in {
        "rdcycle",
        "rdcycleh",
        "rdinstret",
        "rdinstreth",
        "rdtime",
        "rdtimeh",
        "frcsr",
        "fscsr",
        "frflags",
        "fsflags",
        "frrm",
        "fsrm",
    }:
        return "csr"
    if op in {"ecall", "ebreak", "mret", "sret", "uret", "wfi"}:
        return "system"

    if op.startswith("v"):
        if op.startswith("vl"):
            return "vector-load"
        if op.startswith(
            ("vse", "vsse", "vsux", "vsox", "vs1r", "vs2r", "vs4r", "vs8r")
        ) or op == "vsm.v":
            return "vector-store"
        return "vector-arithmetic"

    integer_loads = {
        "lb",
        "lbu",
        "lh",
        "lhu",
        "lw",
        "lwu",
        "ld",
        "lq",
        "lwsp",
        "ldsp",
    }
    integer_stores = {"sb", "sh", "sw", "sd", "sq", "swsp", "sdsp"}
    if op in integer_loads:
        return "integer-load"
    if op in integer_stores:
        return "integer-store"
    if op in {"flh", "flw", "fld", "flq", "flwsp", "fldsp"}:
        return "floating-load"
    if op in {"fsh", "fsw", "fsd", "fsq", "fswsp", "fsdsp"}:
        return "floating-store"

    if op.startswith(("fmadd", "fmsub", "fnmadd", "fnmsub")):
        return "floating-fma"
    if op.startswith(("fcvt", "fmv")):
        return "floating-convert-or-move"
    if op.startswith(("feq", "flt", "fle", "fclass")):
        return "floating-compare"
    if op.startswith(("fsgnj", "fmin", "fmax")):
        return "floating-sign-or-minmax"
    if op.startswith(("fadd", "fsub", "fmul", "fdiv", "fsqrt")):
        return "floating-arithmetic"

    if op.startswith("mul"):
        return "integer-multiply"
    if op.startswith(("div", "rem")):
        return "integer-divide"
    if op.startswith(("sll", "srl", "sra", "rol", "ror")):
        return "integer-shift"
    if op.startswith(("slt", "snez", "seqz", "sltz", "sgtz")):
        return "integer-compare"
    if op.startswith(("and", "or", "xor", "xnor")) or op == "not":
        return "integer-logic"
    if op.startswith(("add", "sub", "neg", "sext")) or op in {
        "li",
        "mv",
        "nop",
    }:
        return "integer-add-sub"
    if op in {"lui", "auipc"}:
        return "upper-immediate"
    if op.startswith(("clz", "ctz", "cpop", "clmul", "bset", "bclr", "binv", "bext")):
        return "bit-manipulation"
    return "other"


def _safe_rms(values: Sequence[float]) -> float:
    maximum = max(values, default=0.0)
    if maximum <= 0.0:
        return 1.0
    scaled = math.fsum((value / maximum) ** 2 for value in values)
    return maximum * math.sqrt(scaled / len(values))


def _quantile(values: Sequence[float], probability: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = probability * (len(ordered) - 1)
    left = int(math.floor(position))
    right = int(math.ceil(position))
    fraction = position - left
    return ordered[left] * (1.0 - fraction) + ordered[right] * fraction


def _weighted_median(values: Sequence[float], weights: Sequence[float]) -> float:
    pairs = sorted(
        ((value, weight) for value, weight in zip(values, weights) if weight > 0.0),
        key=lambda item: item[0],
    )
    if not pairs:
        return statistics.median(values) if values else 0.0
    threshold = math.fsum(weight for _, weight in pairs) / 2.0
    cumulative = 0.0
    for value, weight in pairs:
        cumulative += weight
        if cumulative >= threshold:
            return value
    return pairs[-1][0]


def _design_metadata(
    rows: Sequence[_Epoch], vocabulary: Sequence[str]
) -> dict[str, dict[str, float | int]]:
    """计算暴露和 pairwise 共线性；该结果可在 bootstrap 中安全复用。"""

    count_vectors = [
        [row.counts.get(name, 0.0) for row in rows] for name in vocabulary
    ]
    totals = [math.fsum(vector) for vector in count_vectors]
    nonzero = [sum(value > 0.0 for value in vector) for vector in count_vectors]
    positive_totals = [value for value in totals if value > 0.0]
    reference_total = statistics.median(positive_totals) if positive_totals else 1.0

    centered_norms: list[float] = []
    means: list[float] = []
    for vector in count_vectors:
        mean = math.fsum(vector) / len(rows)
        means.append(mean)
        centered_norms.append(
            math.sqrt(max(0.0, math.fsum((value - mean) ** 2 for value in vector)))
        )
    max_correlations = [0.0] * len(vocabulary)
    for left in range(len(vocabulary)):
        if centered_norms[left] <= 1e-15:
            max_correlations[left] = 1.0
            continue
        for right in range(left):
            if centered_norms[right] <= 1e-15:
                continue
            covariance = math.fsum(
                (a - means[left]) * (b - means[right])
                for a, b in zip(count_vectors[left], count_vectors[right])
            )
            correlation = min(
                1.0,
                abs(covariance / (centered_norms[left] * centered_norms[right])),
            )
            if correlation > max_correlations[left]:
                max_correlations[left] = correlation
            if correlation > max_correlations[right]:
                max_correlations[right] = correlation

    # 与 nuisance 的高相关同样会让“每条指令成本”不可辨识。例如 TB 数通常
    # 与动态指令总数接近线性相关，不能只检查指令列之间的共线性。
    nuisance_names = sorted(
        {name for row in rows for name in row.external_nuisance}
    )
    nuisance_vectors = [
        [row.external_nuisance.get(name, 0.0) for row in rows]
        for name in nuisance_names
    ]
    nuisance_vectors.append([float(row.duration_ns) for row in rows])
    for vector in nuisance_vectors:
        nuisance_mean = math.fsum(vector) / len(rows)
        nuisance_norm = math.sqrt(
            max(0.0, math.fsum((value - nuisance_mean) ** 2 for value in vector))
        )
        if nuisance_norm <= 1e-15:
            continue
        for index, count_vector in enumerate(count_vectors):
            if centered_norms[index] <= 1e-15:
                continue
            covariance = math.fsum(
                (count - means[index]) * (value - nuisance_mean)
                for count, value in zip(count_vector, vector)
            )
            correlation = min(
                1.0,
                abs(covariance / (centered_norms[index] * nuisance_norm)),
            )
            max_correlations[index] = max(max_correlations[index], correlation)

    result: dict[str, dict[str, float | int]] = {}
    row_count = len(rows)
    for index, name in enumerate(vocabulary):
        count_strength = min(1.0, math.sqrt(totals[index] / reference_total))
        support_fraction = nonzero[index] / row_count
        uniqueness = max(0.0, 1.0 - max_correlations[index] ** 2)
        effective_fraction = support_fraction * count_strength * math.sqrt(
            max(0.02, uniqueness)
        )
        result[name] = {
            "total_count": totals[index],
            "nonzero_epochs": nonzero[index],
            "support_fraction": support_fraction,
            "count_strength": count_strength,
            "max_abs_correlation": max_correlations[index],
            "uniqueness": uniqueness,
            "effective_fraction": effective_fraction,
        }
    return result


def _nuisance_specifications(
    rows: Sequence[_Epoch], external_names: Sequence[str]
) -> tuple[list[str], dict[str, str]]:
    names = list(external_names)
    names.append("epoch_duration_ns")
    durations = [float(row.duration_ns) for row in rows]
    mean_duration = math.fsum(durations) / len(durations)
    variance = math.fsum((value - mean_duration) ** 2 for value in durations) / len(rows)
    coefficient_of_variation = (
        math.sqrt(variance) / mean_duration if mean_duration > 0.0 else 0.0
    )
    dropped: dict[str, str] = {}
    if coefficient_of_variation >= 1e-3:
        names.append("intercept")
    else:
        dropped["intercept"] = "与固定 epoch duration 完全混叠"
    return names, dropped


def _nuisance_value(row: _Epoch, name: str) -> float:
    if name == "epoch_duration_ns":
        return float(row.duration_ns)
    if name == "intercept":
        return 1.0
    return row.external_nuisance.get(name, 0.0)


def _family_priors_from_weights(
    weights: Mapping[str, float],
    vocabulary: Sequence[str],
    metadata: Mapping[str, Mapping[str, float | int]],
    initial_global: float,
    sample_size: int,
) -> tuple[dict[str, float], float]:
    reliability = [
        max(
            1e-6,
            float(metadata[name]["effective_fraction"]) * sample_size,
        )
        for name in vocabulary
    ]
    beta_values = [max(0.0, weights[name]) for name in vocabulary]
    empirical_global = _weighted_median(beta_values, reliability)
    global_prior = 0.85 * empirical_global + 0.15 * initial_global
    if not math.isfinite(global_prior) or global_prior < 0.0:
        global_prior = max(0.0, initial_global)

    members: dict[str, list[tuple[str, float]]] = {}
    for name, member_reliability in zip(vocabulary, reliability):
        members.setdefault(instruction_family(name), []).append(
            (name, member_reliability)
        )
    prior_strength = max(2.0, math.sqrt(sample_size))
    result: dict[str, float] = {}
    for family, family_members in members.items():
        numerator = prior_strength * global_prior
        denominator = prior_strength
        for name, member_reliability in family_members:
            numerator += member_reliability * max(0.0, weights[name])
            denominator += member_reliability
        result[family] = max(0.0, numerator / denominator)
    return result, global_prior


def _median_absolute_deviation(values: Sequence[float]) -> float:
    if not values:
        return 0.0
    center = statistics.median(values)
    return statistics.median(abs(value - center) for value in values)


def _fit_core(
    rows: Sequence[_Epoch],
    vocabulary: Sequence[str],
    metadata: Mapping[str, Mapping[str, float | int]],
    nuisance_names: Sequence[str],
    *,
    hierarchy_strength: float,
    nuisance_ridge: float,
    huber_delta: float,
    max_irls_iterations: int,
    max_coordinate_sweeps: int,
    tolerance: float,
) -> _CoreFit:
    n = len(rows)
    y_raw = [row.task_clock_ns for row in rows]
    positive_y = [value for value in y_raw if value > 0.0]
    y_scale = statistics.median(positive_y) if positive_y else 1.0
    if y_scale <= 0.0 or not math.isfinite(y_scale):
        y_scale = max(1.0, math.fsum(y_raw) / max(1, n))
    y = [value / y_scale for value in y_raw]

    raw_instruction_columns: dict[str, list[float]] = {
        name: [row.counts.get(name, 0.0) for row in rows]
        for name in vocabulary
    }
    instruction_scales = {
        name: _safe_rms(column) for name, column in raw_instruction_columns.items()
    }
    instruction_columns: dict[str, list[tuple[int, float]]] = {}
    for name in vocabulary:
        scale = instruction_scales[name]
        instruction_columns[name] = [
            (index, value / scale)
            for index, value in enumerate(raw_instruction_columns[name])
            if value > 0.0
        ]

    raw_nuisance_columns = {
        name: [_nuisance_value(row, name) for row in rows]
        for name in nuisance_names
    }
    nuisance_scales = {
        name: _safe_rms(column) for name, column in raw_nuisance_columns.items()
    }
    nuisance_columns = {
        name: [
            (index, value / nuisance_scales[name])
            for index, value in enumerate(raw_nuisance_columns[name])
            if value > 0.0
        ]
        for name in nuisance_names
    }

    total_counts = math.fsum(
        math.fsum(row.counts.get(name, 0.0) for name in vocabulary)
        for row in rows
    )
    initial_global = math.fsum(y_raw) / total_counts if total_counts > 0.0 else 0.0
    beta = {name: initial_global for name in vocabulary}
    alpha = {
        name: beta[name] * instruction_scales[name] / y_scale
        for name in vocabulary
    }
    nuisance_alpha = {name: 0.0 for name in nuisance_names}

    prediction = [0.0] * n
    for name in vocabulary:
        coefficient = alpha[name]
        for index, value in instruction_columns[name]:
            prediction[index] += coefficient * value
    residual = [target - fitted for target, fitted in zip(y, prediction)]
    robust_weights = [1.0] * n
    family_priors, _ = _family_priors_from_weights(
        beta, vocabulary, metadata, initial_global, n
    )
    total_sweeps = 0
    converged = False
    robust_weight_rms_change = float("inf")

    for irls_iteration in range(1, max_irls_iterations + 1):
        sweep_converged = False
        for _ in range(max_coordinate_sweeps):
            total_sweeps += 1
            family_priors, _ = _family_priors_from_weights(
                beta, vocabulary, metadata, initial_global, n
            )
            maximum_change = 0.0

            for name in vocabulary:
                column = instruction_columns[name]
                old = alpha[name]
                effective_fraction = max(
                    1.0 / max(1, n),
                    float(metadata[name]["effective_fraction"]),
                )
                ridge = hierarchy_strength / effective_fraction
                target_beta = family_priors[instruction_family(name)]
                target_alpha = target_beta * instruction_scales[name] / y_scale
                numerator = ridge * target_alpha
                denominator = ridge
                for index, value in column:
                    weight = robust_weights[index]
                    numerator += weight * value * (residual[index] + value * old)
                    denominator += weight * value * value
                new = max(0.0, numerator / denominator) if denominator > 0.0 else target_alpha
                delta = new - old
                if delta != 0.0:
                    alpha[name] = new
                    beta[name] = new * y_scale / instruction_scales[name]
                    for index, value in column:
                        residual[index] -= value * delta
                    maximum_change = max(
                        maximum_change, abs(delta) / (1.0 + abs(old))
                    )

            for name in nuisance_names:
                column = nuisance_columns[name]
                old = nuisance_alpha[name]
                numerator = 0.0
                denominator = nuisance_ridge
                for index, value in column:
                    weight = robust_weights[index]
                    numerator += weight * value * (residual[index] + value * old)
                    denominator += weight * value * value
                new = max(0.0, numerator / denominator) if denominator > 0.0 else 0.0
                delta = new - old
                if delta != 0.0:
                    nuisance_alpha[name] = new
                    for index, value in column:
                        residual[index] -= value * delta
                    maximum_change = max(
                        maximum_change, abs(delta) / (1.0 + abs(old))
                    )

            if maximum_change <= tolerance:
                sweep_converged = True
                break

        mad = _median_absolute_deviation(residual)
        robust_scale = max(1e-12, 1.4826 * mad)
        cutoff = huber_delta * robust_scale
        new_robust_weights = [
            1.0 if abs(value) <= cutoff else cutoff / abs(value)
            for value in residual
        ]
        robust_weight_rms_change = math.sqrt(
            math.fsum(
                (new - old) ** 2
                for new, old in zip(new_robust_weights, robust_weights)
            )
            / n
        )
        robust_weights = new_robust_weights
        # 单个恰好跨越 Huber cutoff 的 epoch 可能让 max-delta 永不收敛；RMS
        # 判据衡量整体 IRLS 稳定性，同时仍要求坐标下降本身已经收敛。
        if sweep_converged and robust_weight_rms_change <= max(
            math.sqrt(tolerance), 1e-4
        ):
            converged = True
            break

    family_priors, _ = _family_priors_from_weights(
        beta, vocabulary, metadata, initial_global, n
    )
    shrinkage: dict[str, float] = {}
    for name in vocabulary:
        effective_fraction = max(
            1.0 / max(1, n), float(metadata[name]["effective_fraction"])
        )
        ridge = hierarchy_strength / effective_fraction
        curvature = math.fsum(
            robust_weights[index] * value * value
            for index, value in instruction_columns[name]
        )
        shrinkage[name] = ridge / (ridge + curvature) if ridge + curvature > 0.0 else 1.0

    nuisance_coefficients = {
        name: nuisance_alpha[name] * y_scale / nuisance_scales[name]
        for name in nuisance_names
    }
    return _CoreFit(
        instruction_weights={name: max(0.0, beta[name]) for name in vocabulary},
        family_priors=family_priors,
        nuisance_coefficients=nuisance_coefficients,
        shrinkage=shrinkage,
        robust_weights=robust_weights,
        converged=converged,
        robust_weight_rms_change=robust_weight_rms_change,
        irls_iterations=irls_iteration,
        coordinate_sweeps=total_sweeps,
    )


def _predict_epoch(
    row: _Epoch,
    fit: _CoreFit,
    vocabulary: Sequence[str],
    nuisance_names: Sequence[str],
) -> tuple[dict[str, float], dict[str, float]]:
    instruction = {
        name: row.counts.get(name, 0.0) * fit.instruction_weights[name]
        for name in vocabulary
        if row.counts.get(name, 0.0) > 0.0
    }
    nuisance = {
        name: _nuisance_value(row, name) * fit.nuisance_coefficients[name]
        for name in nuisance_names
        if _nuisance_value(row, name) > 0.0
    }
    return instruction, nuisance


def _regression_metrics(actual: Sequence[float], predicted: Sequence[float]) -> dict[str, Any]:
    if len(actual) != len(predicted) or not actual:
        raise WeightModelError("回归指标需要等长非空向量")
    residuals = [target - fitted for target, fitted in zip(actual, predicted)]
    mean_actual = math.fsum(actual) / len(actual)
    sse = math.fsum(value * value for value in residuals)
    sst = math.fsum((value - mean_actual) ** 2 for value in actual)
    mae = math.fsum(abs(value) for value in residuals) / len(actual)
    rmse = math.sqrt(sse / len(actual))
    relative_mae = mae / mean_actual if mean_actual > 0.0 else None
    r_squared = 1.0 - sse / sst if sst > 0.0 else None
    return {
        "observations": len(actual),
        "mean_task_clock_ns": mean_actual,
        "mae_ns": mae,
        "rmse_ns": rmse,
        "median_absolute_error_ns": statistics.median(abs(value) for value in residuals),
        "relative_mae": relative_mae,
        "r_squared": r_squared,
    }


def _blocked_cross_validation(
    rows: Sequence[_Epoch],
    vocabulary: Sequence[str],
    metadata: Mapping[str, Mapping[str, float | int]],
    nuisance_names: Sequence[str],
    *,
    folds: int,
    purge_gap: int,
    hierarchy_strength: float,
    nuisance_ridge: float,
    huber_delta: float,
    max_irls_iterations: int,
    max_coordinate_sweeps: int,
    tolerance: float,
) -> dict[str, Any]:
    n = len(rows)
    if folds < 2 or n < max(20, folds * 4):
        return {
            "quality": "insufficient-data",
            "folds": [],
            "purge_gap_epochs": purge_gap,
            "reason": "blocked CV 至少需要 20 个 epoch 且每折至少约 4 个 epoch",
        }
    folds = min(folds, n // 4)
    predictions: list[tuple[int, float]] = []
    fold_rows: list[dict[str, Any]] = []
    for fold in range(folds):
        begin = fold * n // folds
        end = (fold + 1) * n // folds
        validation_indices = list(range(begin, end))
        excluded_begin = max(0, begin - purge_gap)
        excluded_end = min(n, end + purge_gap)
        training_indices = [
            index for index in range(n) if not excluded_begin <= index < excluded_end
        ]
        if len(training_indices) < 10:
            continue
        training_rows = [rows[index] for index in training_indices]
        # 暴露度和共线性也必须只看训练块，避免 blocked CV 偷看验证期设计矩阵。
        training_metadata = _design_metadata(training_rows, vocabulary)
        fit = _fit_core(
            training_rows,
            vocabulary,
            training_metadata,
            nuisance_names,
            hierarchy_strength=hierarchy_strength,
            nuisance_ridge=nuisance_ridge,
            huber_delta=huber_delta,
            max_irls_iterations=max_irls_iterations,
            max_coordinate_sweeps=max(40, 2 * max_coordinate_sweeps // 3),
            tolerance=max(1e-5, tolerance * 10.0),
        )
        actual: list[float] = []
        fitted_values: list[float] = []
        training_exposure = {
            name: math.fsum(row.counts.get(name, 0.0) for row in training_rows)
            for name in vocabulary
        }
        unseen_count = 0.0
        total_count = 0.0
        for index in validation_indices:
            instruction, nuisance = _predict_epoch(
                rows[index], fit, vocabulary, nuisance_names
            )
            fitted = math.fsum(instruction.values()) + math.fsum(nuisance.values())
            actual.append(rows[index].task_clock_ns)
            fitted_values.append(fitted)
            predictions.append((index, fitted))
            for name, count in rows[index].counts.items():
                total_count += count
                if training_exposure.get(name, 0.0) <= 0.0:
                    unseen_count += count
        metrics = _regression_metrics(actual, fitted_values)
        metrics.update(
            {
                "fold": fold,
                "validation_begin_epoch": begin,
                "validation_end_epoch": end,
                "training_epochs": len(training_rows),
                "fit_converged": fit.converged,
                "unseen_instruction_count_fraction": (
                    unseen_count / total_count if total_count > 0.0 else 0.0
                ),
            }
        )
        fold_rows.append(metrics)

    predictions.sort()
    if len(predictions) != n:
        return {
            "quality": "insufficient-data",
            "folds": fold_rows,
            "purge_gap_epochs": purge_gap,
            "reason": "purge 后没有覆盖全部验证 epoch",
        }
    aggregate = _regression_metrics(
        [row.task_clock_ns for row in rows], [value for _, value in predictions]
    )
    convergence_fraction = math.fsum(
        bool(row["fit_converged"]) for row in fold_rows
    ) / len(fold_rows)
    relative_mae = aggregate["relative_mae"]
    r_squared = aggregate["r_squared"]
    if convergence_fraction < 0.80:
        quality = "numerically-unreliable"
    elif (
        relative_mae is not None
        and relative_mae <= 0.15
        and r_squared is not None
        and r_squared >= 0.50
    ):
        quality = "good"
    elif relative_mae is not None and relative_mae <= 0.30:
        quality = "usable"
    else:
        quality = "poor"
    return {
        "quality": quality,
        "fold_count": len(fold_rows),
        "convergence_fraction": convergence_fraction,
        "purge_gap_epochs": purge_gap,
        "aggregate": aggregate,
        "folds": fold_rows,
    }


def moving_block_indices(
    length: int, block_length: int, rng: random.Random
) -> list[int]:
    """生成非循环 moving-block bootstrap 下标，尾块按需截断。"""

    if length <= 0:
        raise WeightModelError("length 必须为正")
    if block_length <= 0 or block_length > length:
        raise WeightModelError("block_length 必须位于 1..length")
    result: list[int] = []
    last_start = length - block_length
    while len(result) < length:
        start = rng.randint(0, last_start)
        result.extend(range(start, start + block_length))
    return result[:length]


def _bootstrap_weights(
    rows: Sequence[_Epoch],
    vocabulary: Sequence[str],
    metadata: Mapping[str, Mapping[str, float | int]],
    nuisance_names: Sequence[str],
    *,
    replicates: int,
    block_length: int,
    seed: int,
    hierarchy_strength: float,
    nuisance_ridge: float,
    huber_delta: float,
    max_irls_iterations: int,
    max_coordinate_sweeps: int,
    tolerance: float,
) -> dict[str, Any]:
    samples = {name: [] for name in vocabulary}
    family_names = sorted({instruction_family(name) for name in vocabulary})
    family_samples = {name: [] for name in family_names}
    rng = random.Random(seed)
    converged_count = 0
    for _ in range(replicates):
        indices = moving_block_indices(len(rows), block_length, rng)
        sampled_rows = [rows[index] for index in indices]
        fit = _fit_core(
            sampled_rows,
            vocabulary,
            metadata,
            nuisance_names,
            hierarchy_strength=hierarchy_strength,
            nuisance_ridge=nuisance_ridge,
            huber_delta=huber_delta,
            max_irls_iterations=max_irls_iterations,
            # bootstrap 的统计误差远大于 1e-5 级坐标误差；适度放宽数值
            # 容差可避免把计算预算浪费在每个重采样的无意义尾数上。
            max_coordinate_sweeps=max(40, 2 * max_coordinate_sweeps // 3),
            tolerance=max(1e-5, tolerance * 10.0),
        )
        converged_count += int(fit.converged)
        for name in vocabulary:
            samples[name].append(fit.instruction_weights[name])
        for family in family_names:
            family_samples[family].append(fit.family_priors.get(family, 0.0))
    return {
        "instruction_samples": samples,
        "family_samples": family_samples,
        "converged_replicates": converged_count,
    }


def _exposure_label(nonzero_epochs: int, sample_size: int) -> str:
    fraction = nonzero_epochs / sample_size
    if nonzero_epochs < 5:
        return "sparse"
    if fraction < 0.10:
        return "limited"
    if fraction < 0.50:
        return "moderate"
    return "broad"


def _identifiability_label(
    score: float, shrinkage: float, uniqueness: float
) -> str:
    if uniqueness < 0.01 or shrinkage >= 0.95:
        return "not-identifiable"
    if score < 0.20 or shrinkage >= 0.70:
        return "weak"
    if score < 0.50 or shrinkage >= 0.35:
        return "moderate"
    return "strong"


def _source_label(identifiability: str, shrinkage: float) -> str:
    if shrinkage >= 0.95:
        return "measured-family-prior-only"
    if identifiability == "not-identifiable":
        return "vcpu-task-clock-family-constrained-nonidentifiable"
    if shrinkage >= 0.20:
        return "vcpu-task-clock-huber-family-shrinkage"
    return "vcpu-task-clock-huber-exact"


def fit_instruction_weight_model(
    epoch_rows: Sequence[Any],
    *,
    vcpu_tids: Iterable[str | int] | None = None,
    bootstrap_replicates: int = 100,
    block_length: int | None = None,
    confidence: float = 0.95,
    cv_folds: int = 5,
    cv_purge_gap: int | None = None,
    hierarchy_strength: float = 4.0,
    nuisance_ridge: float = 1.0,
    huber_delta: float = 1.345,
    max_irls_iterations: int = 12,
    max_coordinate_sweeps: int = 60,
    tolerance: float = 1e-6,
    seed: int = 0,
) -> dict[str, Any]:
    """拟合逐指令 ``ns/insn``，并返回 bootstrap、CV 与逐 epoch 归因。

    若输入线程级 task-clock mapping，``vcpu_tids`` 必须来自
    jitdump/catalog 的 ``container_tid -> collector host_tid`` 映射；函数不会按
    ``comm`` 猜测 vCPU。标量 ``task_clock_ns`` 被视为调用方已经过滤好的聚合值，
    推荐使用语义更明确的 ``vcpu_task_clock_ns`` 字段。
    """

    if isinstance(bootstrap_replicates, bool) or not isinstance(
        bootstrap_replicates, int
    ):
        raise WeightModelError("bootstrap_replicates 必须是非负整数")
    if bootstrap_replicates < 0:
        raise WeightModelError("bootstrap_replicates 必须是非负整数")
    if not 0.0 < confidence < 1.0:
        raise WeightModelError("confidence 必须位于 (0, 1)")
    for name, value in (
        ("hierarchy_strength", hierarchy_strength),
        ("nuisance_ridge", nuisance_ridge),
        ("huber_delta", huber_delta),
        ("tolerance", tolerance),
    ):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise WeightModelError(f"{name} 必须是正有限数")
        if not math.isfinite(float(value)) or value <= 0.0:
            raise WeightModelError(f"{name} 必须是正有限数")
    _positive_int(max_irls_iterations, "max_irls_iterations")
    _positive_int(max_coordinate_sweeps, "max_coordinate_sweeps")
    if isinstance(cv_folds, bool) or not isinstance(cv_folds, int) or cv_folds < 0:
        raise WeightModelError("cv_folds 必须是非负整数")

    rows, external_nuisance, task_clock_sources = _freeze_epochs(
        epoch_rows, vcpu_tids
    )
    vocabulary = sorted(
        {name for row in rows for name, count in row.counts.items() if count > 0.0}
    )
    metadata = _design_metadata(rows, vocabulary)
    nuisance_names, dropped_nuisance = _nuisance_specifications(
        rows, external_nuisance
    )
    n = len(rows)
    if block_length is None:
        block_length = min(n, max(2, round(n ** (1.0 / 3.0))))
    if isinstance(block_length, bool) or not isinstance(block_length, int):
        raise WeightModelError("block_length 必须是正整数")
    if block_length <= 0 or block_length > n:
        raise WeightModelError("block_length 必须位于 1..epoch 数")
    if cv_purge_gap is None:
        cv_purge_gap = max(0, block_length - 1)
    if (
        isinstance(cv_purge_gap, bool)
        or not isinstance(cv_purge_gap, int)
        or cv_purge_gap < 0
    ):
        raise WeightModelError("cv_purge_gap 必须是非负整数")

    fit = _fit_core(
        rows,
        vocabulary,
        metadata,
        nuisance_names,
        hierarchy_strength=float(hierarchy_strength),
        nuisance_ridge=float(nuisance_ridge),
        huber_delta=float(huber_delta),
        max_irls_iterations=max_irls_iterations,
        max_coordinate_sweeps=max_coordinate_sweeps,
        tolerance=float(tolerance),
    )

    cv = _blocked_cross_validation(
        rows,
        vocabulary,
        metadata,
        nuisance_names,
        folds=cv_folds,
        purge_gap=cv_purge_gap,
        hierarchy_strength=float(hierarchy_strength),
        nuisance_ridge=float(nuisance_ridge),
        huber_delta=float(huber_delta),
        max_irls_iterations=max_irls_iterations,
        max_coordinate_sweeps=max_coordinate_sweeps,
        tolerance=float(tolerance),
    )

    bootstrap: dict[str, Any] | None = None
    if bootstrap_replicates > 0:
        bootstrap = _bootstrap_weights(
            rows,
            vocabulary,
            metadata,
            nuisance_names,
            replicates=bootstrap_replicates,
            block_length=block_length,
            seed=seed,
            hierarchy_strength=float(hierarchy_strength),
            nuisance_ridge=float(nuisance_ridge),
            huber_delta=float(huber_delta),
            max_irls_iterations=max_irls_iterations,
            max_coordinate_sweeps=max_coordinate_sweeps,
            tolerance=float(tolerance),
        )

    alpha = (1.0 - confidence) / 2.0
    instruction_rows: list[dict[str, Any]] = []
    for name in vocabulary:
        meta = metadata[name]
        uniqueness = float(meta["uniqueness"])
        support_fraction = float(meta["support_fraction"])
        count_strength = float(meta["count_strength"])
        score = math.sqrt(support_fraction) * math.sqrt(uniqueness) * math.sqrt(
            count_strength
        )
        shrinkage = fit.shrinkage[name]
        identifiability = _identifiability_label(score, shrinkage, uniqueness)
        samples = (
            bootstrap["instruction_samples"][name] if bootstrap is not None else []
        )
        interval = (
            [_quantile(samples, alpha), _quantile(samples, 1.0 - alpha)]
            if samples
            else None
        )
        instruction_rows.append(
            {
                "instruction": name,
                "family": instruction_family(name),
                "ns_per_instruction": fit.instruction_weights[name],
                "confidence_interval": interval,
                "confidence_level": confidence if interval is not None else None,
                "family_prior_ns_per_instruction": fit.family_priors[
                    instruction_family(name)
                ],
                "total_exact_count": float(meta["total_count"]),
                "nonzero_epochs": int(meta["nonzero_epochs"]),
                "exposure": _exposure_label(int(meta["nonzero_epochs"]), n),
                "support_fraction": support_fraction,
                "max_abs_predictor_correlation": float(meta["max_abs_correlation"]),
                "uniqueness": uniqueness,
                "identifiability_score": score,
                "identifiability": identifiability,
                "shrinkage": shrinkage,
                "source": _source_label(identifiability, shrinkage),
                "bootstrap_positive_probability": (
                    math.fsum(value > 0.0 for value in samples) / len(samples)
                    if samples
                    else None
                ),
            }
        )

    family_rows: list[dict[str, Any]] = []
    for family in sorted(fit.family_priors):
        samples = bootstrap["family_samples"][family] if bootstrap is not None else []
        family_rows.append(
            {
                "family": family,
                "prior_ns_per_instruction": fit.family_priors[family],
                "confidence_interval": (
                    [_quantile(samples, alpha), _quantile(samples, 1.0 - alpha)]
                    if samples
                    else None
                ),
                "members": [
                    name for name in vocabulary if instruction_family(name) == family
                ],
                "source": "empirical-vcpu-task-clock-family-prior",
            }
        )

    epoch_attribution: list[dict[str, Any]] = []
    actual_values: list[float] = []
    fitted_values: list[float] = []
    total_instruction_ns = 0.0
    total_nuisance_ns = 0.0
    total_unattributed_ns = 0.0
    total_overattributed_ns = 0.0
    for row in rows:
        instruction, nuisance = _predict_epoch(row, fit, vocabulary, nuisance_names)
        instruction_ns = math.fsum(instruction.values())
        nuisance_ns = math.fsum(nuisance.values())
        predicted_ns = instruction_ns + nuisance_ns
        residual_ns = row.task_clock_ns - predicted_ns
        unattributed_ns = max(0.0, residual_ns)
        overattributed_ns = max(0.0, -residual_ns)
        total_instruction_ns += instruction_ns
        total_nuisance_ns += nuisance_ns
        total_unattributed_ns += unattributed_ns
        total_overattributed_ns += overattributed_ns
        actual_values.append(row.task_clock_ns)
        fitted_values.append(predicted_ns)
        epoch_attribution.append(
            {
                "time_ns": row.time_ns,
                "duration_ns": row.duration_ns,
                "vcpu_task_clock_ns": row.task_clock_ns,
                "attributed_instruction_ns": instruction_ns,
                "attributed_nuisance_ns": nuisance_ns,
                "predicted_ns": predicted_ns,
                "residual_ns": residual_ns,
                "unattributed_ns": unattributed_ns,
                "overattributed_ns": overattributed_ns,
                "instruction_attribution_ns": instruction,
                "nuisance_attribution_ns": nuisance,
            }
        )

    fit_metrics = _regression_metrics(actual_values, fitted_values)
    total_task_clock = math.fsum(actual_values)
    fit_metrics.update(
        {
            "converged": fit.converged,
            "robust_weight_rms_change": fit.robust_weight_rms_change,
            "irls_iterations": fit.irls_iterations,
            "coordinate_sweeps": fit.coordinate_sweeps,
            "huber_downweighted_fraction": math.fsum(
                weight < 0.999999 for weight in fit.robust_weights
            )
            / len(fit.robust_weights),
            "total_vcpu_task_clock_ns": total_task_clock,
            "attributed_instruction_ns": total_instruction_ns,
            "attributed_nuisance_ns": total_nuisance_ns,
            "unattributed_ns": total_unattributed_ns,
            "overattributed_ns": total_overattributed_ns,
            "instruction_share_of_task_clock": total_instruction_ns / total_task_clock,
            "nuisance_share_of_task_clock": total_nuisance_ns / total_task_clock,
            "unattributed_share_of_task_clock": total_unattributed_ns
            / total_task_clock,
            "overattributed_share_of_task_clock": total_overattributed_ns
            / total_task_clock,
        }
    )

    if cv.get("quality") == "good" and fit.converged:
        overall_quality = "good"
    elif cv.get("quality") in {"good", "usable"}:
        overall_quality = "usable"
    elif cv.get("quality") == "insufficient-data":
        overall_quality = "fit-only"
    else:
        overall_quality = "poor"

    convergence_fraction = (
        bootstrap["converged_replicates"] / bootstrap_replicates
        if bootstrap is not None and bootstrap_replicates > 0
        else None
    )
    if bootstrap_replicates == 0:
        bootstrap_quality = "disabled"
    elif convergence_fraction is not None and convergence_fraction < 0.90:
        bootstrap_quality = "numerically-unreliable"
    elif bootstrap_replicates >= 100:
        bootstrap_quality = "good"
    elif bootstrap_replicates >= 40:
        bootstrap_quality = "usable"
    else:
        bootstrap_quality = "exploratory"

    return {
        "schema_version": MODEL_SCHEMA_VERSION,
        "instruction_key": "normalized-mnemonic-shared-across-encoding-sizes",
        "method": "nonnegative-hierarchical-ridge-huber-irls",
        "configuration": {
            "family_mapping": "riscv-cost-family-v1",
            "hierarchy_strength": float(hierarchy_strength),
            "nuisance_ridge": float(nuisance_ridge),
            "huber_delta": float(huber_delta),
            "max_irls_iterations": max_irls_iterations,
            "max_coordinate_sweeps": max_coordinate_sweeps,
            "tolerance": float(tolerance),
            "cv_folds_requested": cv_folds,
            "cv_purge_gap_epochs": cv_purge_gap,
        },
        "quality": overall_quality,
        "epoch_count": n,
        "task_clock_sources": task_clock_sources,
        "vcpu_tid_selection": (
            "explicit-jitdump-catalog-mapping"
            if vcpu_tids is not None
            else "preaggregated-by-caller"
        ),
        "instructions": instruction_rows,
        "families": family_rows,
        "nuisance": {
            "coefficients": fit.nuisance_coefficients,
            "active": list(nuisance_names),
            "dropped": dropped_nuisance,
        },
        "fit": fit_metrics,
        "blocked_cv": cv,
        "bootstrap": {
            "method": "moving-block-pairs-bootstrap",
            "regularization_design": (
                "fixed-at-full-trajectory-exposure-and-collinearity"
            ),
            "quality": bootstrap_quality,
            "replicates": bootstrap_replicates,
            "block_length_epochs": block_length,
            "confidence_level": confidence,
            "seed": seed,
            "converged_replicates": (
                bootstrap["converged_replicates"] if bootstrap is not None else 0
            ),
            "convergence_fraction": convergence_fraction,
        },
        "epochs": epoch_attribution,
        "warnings": [
            "聚合 epoch 回归给出的是 QEMU TCG 环境下的平均边际耗时，不是硬件单指令延迟",
            "单次 1200s 运行的区间只覆盖该轨迹内的时间相关不确定性；跨冷启动稳定性需独立重复运行",
        ],
    }


__all__ = [
    "MODEL_SCHEMA_VERSION",
    "NANOSECONDS_PER_SECOND",
    "WeightModelError",
    "fit_instruction_weight_model",
    "instruction_family",
    "moving_block_indices",
]
