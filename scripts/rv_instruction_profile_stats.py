#!/usr/bin/env python3
"""RISC-V 指令时序画像的分段与不确定性统计。

本模块刻意不读取采集文件。调用方先把原始记录整理成两类按时间排序的行：

* epoch row: ``{"time_ns": int, "counts": {name: count}, "rate": float,
  "kernel_share": float}``；
* weighted row: ``{"time_ns": int, "values": {name: weighted_cost}}``。

所有公开结果都只包含 JSON 可序列化的对象，便于报告生成器直接嵌入结果。
"""

from __future__ import annotations

import math
import random
import statistics
from collections.abc import Mapping, Sequence
from typing import Any


NANOSECONDS_PER_SECOND = 1_000_000_000
OTHER_COMPONENT = "OTHER"
DEFAULT_BUCKET_SECONDS = (1, 2, 5, 10)
DEFAULT_PENALTY_MULTIPLIERS = (0.8, 1.0, 1.2)


class StatisticsError(ValueError):
    """表示输入不足以支持所请求的统计分析。"""


def _field(row: Any, name: str, default: Any = None) -> Any:
    """同时支持普通 mapping 和只读 dataclass/对象。"""

    if isinstance(row, Mapping):
        return row.get(name, default)
    return getattr(row, name, default)


def _finite_number(value: Any, owner: str, *, minimum: float = 0.0) -> float:
    """读取有限实数，并拒绝会被 Python 视作整数的 bool。"""

    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise StatisticsError(f"{owner} 必须是数值")
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < minimum:
        raise StatisticsError(f"{owner} 必须是大于等于 {minimum:g} 的有限数值")
    return parsed


def _time_ns(row: Any, owner: str) -> int:
    """读取非负纳秒时间戳。"""

    value = _field(row, "time_ns")
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise StatisticsError(f"{owner}.time_ns 必须是非负整数")
    return value


def _numeric_mapping(row: Any, field: str, owner: str) -> dict[str, float]:
    """复制非负数值 mapping，避免后续分析受调用方修改影响。"""

    value = _field(row, field)
    if not isinstance(value, Mapping):
        raise StatisticsError(f"{owner}.{field} 必须是 mapping")
    result: dict[str, float] = {}
    for raw_name, raw_number in value.items():
        if not isinstance(raw_name, str) or not raw_name:
            raise StatisticsError(f"{owner}.{field} 包含非法指令名")
        if raw_name == OTHER_COMPONENT:
            raise StatisticsError(
                f"{owner}.{field} 不能使用保留名称 {OTHER_COMPONENT!r}"
            )
        result[raw_name] = _finite_number(
            raw_number, f"{owner}.{field}[{raw_name!r}]"
        )
    return result


def _optional_mapping(row: Any, field: str, owner: str) -> Mapping[str, Any]:
    """读取可选的逐指令元数据 mapping。"""

    value = _field(row, field, {})
    if value is None:
        return {}
    if not isinstance(value, Mapping):
        raise StatisticsError(f"{owner}.{field} 必须是 mapping")
    for name in value:
        if not isinstance(name, str) or not name:
            raise StatisticsError(f"{owner}.{field} 包含非法指令名")
    return value


def _validate_epoch_rows(rows: Sequence[Any]) -> list[dict[str, Any]]:
    """校验并冻结一秒 epoch 行。"""

    if not rows:
        raise StatisticsError("epoch rows 不能为空")
    frozen: list[dict[str, Any]] = []
    previous_time = -1
    for index, row in enumerate(rows):
        owner = f"epoch_rows[{index}]"
        timestamp = _time_ns(row, owner)
        if timestamp <= previous_time:
            raise StatisticsError("epoch rows 必须按 time_ns 严格递增")
        previous_time = timestamp
        counts = _numeric_mapping(row, "counts", owner)
        raw_rate = _field(row, "rate")
        rate = (
            sum(counts.values())
            if raw_rate is None
            else _finite_number(raw_rate, f"{owner}.rate")
        )
        raw_share = _field(row, "kernel_share")
        kernel_share = (
            0.0
            if raw_share is None
            else _finite_number(raw_share, f"{owner}.kernel_share")
        )
        if kernel_share > 1.0:
            raise StatisticsError(f"{owner}.kernel_share 必须位于 0..1")
        duration_ns = _field(row, "duration_ns", NANOSECONDS_PER_SECOND)
        if (
            isinstance(duration_ns, bool)
            or not isinstance(duration_ns, int)
            or duration_ns <= 0
        ):
            raise StatisticsError(f"{owner}.duration_ns 必须是正整数")
        frozen.append(
            {
                "time_ns": timestamp,
                "end_time_ns": timestamp + duration_ns,
                "duration_ns": duration_ns,
                "counts": counts,
                "rate": rate,
                "kernel_share": kernel_share,
            }
        )
    return frozen


def select_top_components(
    count_rows: Sequence[Mapping[str, float]], coverage: float = 0.995
) -> list[str]:
    """选择累计计数覆盖率达到 ``coverage`` 的最小指令集合。"""

    if not 0.0 < coverage <= 1.0:
        raise StatisticsError("coverage 必须位于 (0, 1]")
    totals: dict[str, float] = {}
    for row_index, row in enumerate(count_rows):
        if not isinstance(row, Mapping):
            raise StatisticsError(f"count_rows[{row_index}] 必须是 mapping")
        for name, value in row.items():
            if not isinstance(name, str) or not name or name == OTHER_COMPONENT:
                raise StatisticsError(f"count_rows[{row_index}] 包含非法指令名")
            totals[name] = totals.get(name, 0.0) + _finite_number(
                value, f"count_rows[{row_index}][{name!r}]"
            )
    grand_total = sum(totals.values())
    if grand_total <= 0.0:
        return []
    ordered = sorted(totals.items(), key=lambda item: (-item[1], item[0]))
    selected: list[str] = []
    cumulative = 0.0
    target = coverage * grand_total
    for name, value in ordered:
        selected.append(name)
        cumulative += value
        if cumulative >= target:
            break
    return selected


def _validate_vocabulary(vocabulary: Sequence[str]) -> list[str]:
    """校验固定组成词表并返回独立副本。"""

    selected = list(vocabulary)
    if len(set(selected)) != len(selected) or any(
        not isinstance(name, str) or not name or name == OTHER_COMPONENT
        for name in selected
    ):
        raise StatisticsError("vocabulary 必须包含互异的非空指令名")
    return selected


def collapse_counts(
    counts: Mapping[str, float], vocabulary: Sequence[str]
) -> list[float]:
    """按固定词表生成 ``top + OTHER`` 的组成向量。"""

    selected = set(vocabulary)
    values = [float(counts.get(name, 0.0)) for name in vocabulary]
    values.append(sum(float(value) for name, value in counts.items() if name not in selected))
    return values


def jeffreys_clr(values: Sequence[float], alpha: float = 0.5) -> list[float]:
    """对组成计数做 Jeffreys 平滑并返回 centered log-ratio。"""

    if alpha <= 0.0 or not math.isfinite(alpha):
        raise StatisticsError("Jeffreys alpha 必须是正有限数")
    if not values:
        raise StatisticsError("组成向量不能为空")
    logs: list[float] = []
    for index, value in enumerate(values):
        parsed = _finite_number(value, f"values[{index}]")
        logs.append(math.log(parsed + alpha))
    center = math.fsum(logs) / len(logs)
    return [value - center for value in logs]


def standardize_matrix(matrix: Sequence[Sequence[float]]) -> dict[str, Any]:
    """按列做总体 z-score；常量列保持为零并把 scale 记作 1。"""

    if not matrix:
        raise StatisticsError("feature matrix 不能为空")
    width = len(matrix[0])
    if width == 0 or any(len(row) != width for row in matrix):
        raise StatisticsError("feature matrix 必须是非空矩形")
    columns = [[float(row[column]) for row in matrix] for column in range(width)]
    for column, values in enumerate(columns):
        if any(not math.isfinite(value) for value in values):
            raise StatisticsError(f"feature matrix 第 {column} 列包含非有限数")
    means = [math.fsum(values) / len(values) for values in columns]
    raw_scales = [
        math.sqrt(math.fsum((value - mean) ** 2 for value in values) / len(values))
        for values, mean in zip(columns, means)
    ]
    constant = [scale <= 1e-12 for scale in raw_scales]
    scales = [1.0 if is_constant else scale for scale, is_constant in zip(raw_scales, constant)]
    standardized = [
        [(float(value) - means[column]) / scales[column] for column, value in enumerate(row)]
        for row in matrix
    ]
    return {
        "matrix": standardized,
        "means": means,
        "scales": scales,
        "constant": constant,
    }


def aggregate_epoch_rows(rows: Sequence[Any], bucket_seconds: int) -> list[dict[str, Any]]:
    """把连续一秒 epoch 聚合成固定宽度桶。

    末尾不完整桶仍会保留，并在 ``duration_ns`` 中记录实际持续时间；这避免
    1200 秒窗口因少量尾部调度抖动而无声丢数据。
    """

    if isinstance(bucket_seconds, bool) or not isinstance(bucket_seconds, int):
        raise StatisticsError("bucket_seconds 必须是正整数")
    if bucket_seconds <= 0:
        raise StatisticsError("bucket_seconds 必须是正整数")
    frozen = _validate_epoch_rows(rows)
    result: list[dict[str, Any]] = []
    for begin in range(0, len(frozen), bucket_seconds):
        group = frozen[begin : begin + bucket_seconds]
        counts: dict[str, float] = {}
        duration_ns = 0
        rate_time = 0.0
        kernel_work = 0.0
        for row in group:
            duration_ns += row["duration_ns"]
            seconds = row["duration_ns"] / NANOSECONDS_PER_SECOND
            rate_time += row["rate"] * seconds
            kernel_work += row["rate"] * seconds * row["kernel_share"]
            for name, value in row["counts"].items():
                counts[name] = counts.get(name, 0.0) + value
        duration_seconds = duration_ns / NANOSECONDS_PER_SECOND
        rate = rate_time / duration_seconds if duration_seconds else 0.0
        if rate_time > 0.0:
            kernel_share = kernel_work / rate_time
        else:
            kernel_share = math.fsum(row["kernel_share"] for row in group) / len(group)
        result.append(
            {
                "time_ns": group[0]["time_ns"],
                "end_time_ns": group[-1]["end_time_ns"],
                "duration_ns": duration_ns,
                "source_epoch_begin": begin,
                "source_epoch_end": begin + len(group),
                "counts": counts,
                "rate": rate,
                "kernel_share": kernel_share,
            }
        )
    return result


def prepare_feature_matrix(
    epoch_rows: Sequence[Any],
    *,
    coverage: float = 0.995,
    alpha: float = 0.5,
    vocabulary: Sequence[str] | None = None,
    include_auxiliary: bool = True,
) -> dict[str, Any]:
    """构造 top-coverage + OTHER 的 Jeffreys-CLR 标准化特征。

    ``log1p(rate)`` 与按 epoch 总计数作 Jeffreys 平滑的
    ``logit(kernel_share)`` 默认作为
    辅助特征加入分段，它们让“指令比例相同但吞吐率骤变”的阶段仍可识别。
    """

    rows = _validate_epoch_rows(epoch_rows)
    if vocabulary is None:
        selected = select_top_components([row["counts"] for row in rows], coverage)
    else:
        selected = _validate_vocabulary(vocabulary)

    component_names = selected + [OTHER_COMPONENT]
    feature_names = [f"clr:{name}" for name in component_names]
    raw: list[list[float]] = []
    for row in rows:
        vector = jeffreys_clr(collapse_counts(row["counts"], selected), alpha)
        if include_auxiliary:
            vector.append(math.log1p(row["rate"]))
            total_count = math.fsum(row["counts"].values())
            kernel_count = row["kernel_share"] * total_count
            user_count = (1.0 - row["kernel_share"]) * total_count
            vector.append(math.log((kernel_count + alpha) / (user_count + alpha)))
        raw.append(vector)
    if include_auxiliary:
        feature_names.extend(("log1p:rate", "logit:kernel_share"))
    standardized = standardize_matrix(raw)

    nonconstant_clr = sum(
        not standardized["constant"][index] for index in range(len(component_names))
    )
    # CLR 有一个精确线性约束；逐列缩放不会改变其秩。
    clr_rank = max(0, nonconstant_clr - 1)
    auxiliary_rank = sum(
        not value for value in standardized["constant"][len(component_names) :]
    )
    effective_dimension = max(1, clr_rank + auxiliary_rank)
    return {
        "vocabulary": selected,
        "components": component_names,
        "feature_names": feature_names,
        "raw_matrix": raw,
        "matrix": standardized["matrix"],
        "means": standardized["means"],
        "scales": standardized["scales"],
        "constant_features": [
            name for name, constant in zip(feature_names, standardized["constant"]) if constant
        ],
        "effective_dimension": effective_dimension,
        "coverage": coverage,
        "alpha": alpha,
    }


def _sse_prefix(matrix: Sequence[Sequence[float]]) -> tuple[list[list[float]], list[float]]:
    """构造分段 SSE 所需的逐维和与平方和前缀。"""

    width = len(matrix[0])
    sums = [[0.0] * (len(matrix) + 1) for _ in range(width)]
    squares = [0.0] * (len(matrix) + 1)
    for row_index, row in enumerate(matrix, 1):
        square = 0.0
        for column, value in enumerate(row):
            number = float(value)
            sums[column][row_index] = sums[column][row_index - 1] + number
            square += number * number
        squares[row_index] = squares[row_index - 1] + square
    return sums, squares


def _segment_sse(
    prefixes: Sequence[Sequence[float]], squares: Sequence[float], begin: int, end: int
) -> float:
    """用前缀和计算半开区间的多元均值内 SSE。"""

    length = end - begin
    if length <= 0:
        raise StatisticsError("SSE 区间必须非空")
    mean_term = math.fsum(
        (column[end] - column[begin]) ** 2 for column in prefixes
    ) / length
    # 浮点舍入可能产生 -1e-13 一类结果，SSE 在数学上不可能为负。
    return max(0.0, squares[end] - squares[begin] - mean_term)


def detect_change_points(
    matrix: Sequence[Sequence[float]],
    *,
    penalty: float,
    min_segment_length: int,
) -> dict[str, Any]:
    """以 PELT 剪枝求解带最短段长的多元 SSE 变点。

    目标函数为 ``sum(segment SSE) + penalty * number_of_segments``。
    常数项不影响变点位置。返回的 ``boundaries`` 总含 0 和样本数。
    """

    if not matrix or not matrix[0]:
        raise StatisticsError("matrix 必须是非空矩形")
    width = len(matrix[0])
    if any(len(row) != width for row in matrix):
        raise StatisticsError("matrix 必须是矩形")
    if not math.isfinite(penalty) or penalty < 0.0:
        raise StatisticsError("penalty 必须是非负有限数")
    if (
        isinstance(min_segment_length, bool)
        or not isinstance(min_segment_length, int)
        or min_segment_length <= 0
    ):
        raise StatisticsError("min_segment_length 必须是正整数")
    sample_count = len(matrix)
    if sample_count < min_segment_length:
        raise StatisticsError("样本数小于最短段长")

    prefixes, squares = _sse_prefix(matrix)
    infinity = float("inf")
    objective = [infinity] * (sample_count + 1)
    previous = [-1] * (sample_count + 1)
    objective[0] = 0.0
    # min segment length 下，经典 PELT 的候选不能在满足剪枝不等式后立刻删除：
    # 当前 end 作为替代变点还要再经过 min_segment_length 个样本才合法。
    # value 是候选最早可安全删除的 end；None 表示尚未被支配。
    active: dict[int, int | None] = {0: None}
    evaluated_candidates = 0

    for end in range(min_segment_length, sample_count + 1):
        active = {
            begin: expiry
            for begin, expiry in active.items()
            if expiry is None or end < expiry
        }
        newly_eligible = end - min_segment_length
        if newly_eligible >= min_segment_length and math.isfinite(objective[newly_eligible]):
            active.setdefault(newly_eligible, None)

        best_value = infinity
        best_begin = -1
        candidate_costs: list[tuple[int, float]] = []
        for begin in active:
            if end - begin < min_segment_length:
                continue
            unpenalized = objective[begin] + _segment_sse(
                prefixes, squares, begin, end
            )
            candidate_costs.append((begin, unpenalized))
            evaluated_candidates += 1
            value = unpenalized + penalty
            if value < best_value - 1e-12 or (
                abs(value - best_value) <= 1e-12 and begin < best_begin
            ):
                best_value = value
                best_begin = begin
        if best_begin < 0:
            continue
        objective[end] = best_value
        previous[end] = best_begin

        # SSE 满足 PELT 的 K=0 超可加条件。保留等号可避免浮点误剪枝。
        tolerance = 1e-10 * max(1.0, abs(best_value))
        for begin, unpenalized in candidate_costs:
            if unpenalized > best_value + tolerance:
                expiry = end + min_segment_length
                current_expiry = active[begin]
                if current_expiry is None or expiry < current_expiry:
                    active[begin] = expiry

    if previous[sample_count] < 0:
        raise StatisticsError("无法在最短段长约束下覆盖全部样本")
    boundaries = [sample_count]
    cursor = sample_count
    while cursor > 0:
        cursor = previous[cursor]
        if cursor < 0:
            raise StatisticsError("变点回溯失败")
        boundaries.append(cursor)
    boundaries.reverse()
    segment_sse = [
        _segment_sse(prefixes, squares, begin, end)
        for begin, end in zip(boundaries, boundaries[1:])
    ]
    return {
        "boundaries": boundaries,
        "change_points": boundaries[1:-1],
        "segment_sse": segment_sse,
        "total_sse": math.fsum(segment_sse),
        "objective": objective[sample_count],
        "penalty": penalty,
        "min_segment_length": min_segment_length,
        "evaluated_candidates": evaluated_candidates,
    }


def _boundary_time(rows: Sequence[Mapping[str, Any]], boundary: int) -> int:
    """把桶索引边界映射回单调时钟。"""

    if boundary == len(rows):
        return int(rows[-1]["end_time_ns"])
    return int(rows[boundary]["time_ns"])


def _median(values: Sequence[float]) -> float:
    """返回非空序列中位数并保持 JSON 数值。"""

    if not values:
        raise StatisticsError("无法计算空序列中位数")
    return float(statistics.median(values))


def _cluster_sensitivity_boundaries(
    records: Sequence[Mapping[str, Any]], tolerance_ns: int
) -> list[dict[str, Any]]:
    """把不同桶宽/惩罚下相近边界聚为稳定性簇。"""

    points: list[tuple[int, int]] = []
    for configuration, record in enumerate(records):
        points.extend((int(value), configuration) for value in record["change_point_time_ns"])
    if not points:
        return []
    points.sort()
    clusters: list[list[tuple[int, int]]] = []
    for point in points:
        current_center = (
            int(_median([value for value, _ in clusters[-1]])) if clusters else 0
        )
        if not clusters or point[0] - current_center > tolerance_ns:
            clusters.append([point])
        else:
            clusters[-1].append(point)
    configuration_count = len(records)
    return [
        {
            "median_time_ns": int(_median([value for value, _ in cluster])),
            "minimum_time_ns": min(value for value, _ in cluster),
            "maximum_time_ns": max(value for value, _ in cluster),
            "supporting_configurations": len({configuration for _, configuration in cluster}),
            "support_fraction": len({configuration for _, configuration in cluster})
            / configuration_count,
        }
        for cluster in clusters
    ]


def run_segmentation_sensitivity(
    epoch_rows: Sequence[Any],
    *,
    bucket_seconds: Sequence[int] = DEFAULT_BUCKET_SECONDS,
    penalty_multipliers: Sequence[float] = DEFAULT_PENALTY_MULTIPLIERS,
    min_segment_seconds: int = 20,
    coverage: float = 0.995,
    alpha: float = 0.5,
    boundary_tolerance_seconds: int | None = None,
) -> dict[str, Any]:
    """运行 1/2/5/10 秒桶和 0.8/1/1.2 惩罚的敏感性分析。"""

    base_rows = _validate_epoch_rows(epoch_rows)
    buckets = list(bucket_seconds)
    multipliers = list(penalty_multipliers)
    if not buckets or not multipliers:
        raise StatisticsError("桶宽和惩罚敏感性集合都不能为空")
    if min_segment_seconds <= 0:
        raise StatisticsError("min_segment_seconds 必须是正整数")
    vocabulary = select_top_components([row["counts"] for row in base_rows], coverage)
    records: list[dict[str, Any]] = []
    for bucket in buckets:
        if isinstance(bucket, bool) or not isinstance(bucket, int) or bucket <= 0:
            raise StatisticsError("所有 bucket_seconds 都必须是正整数")
        aggregated = aggregate_epoch_rows(base_rows, bucket)
        features = prepare_feature_matrix(
            aggregated,
            coverage=coverage,
            alpha=alpha,
            vocabulary=vocabulary,
        )
        minimum = max(1, math.ceil(min_segment_seconds / bucket))
        if len(aggregated) < minimum:
            raise StatisticsError("桶化后样本数小于最短段长")
        base_penalty = features["effective_dimension"] * math.log(max(2, len(aggregated)))
        for multiplier in multipliers:
            multiplier_value = _finite_number(
                multiplier, "penalty multiplier", minimum=0.0
            )
            if multiplier_value == 0.0:
                raise StatisticsError("penalty multiplier 必须大于 0")
            result = detect_change_points(
                features["matrix"],
                penalty=multiplier_value * base_penalty,
                min_segment_length=minimum,
            )
            records.append(
                {
                    "bucket_seconds": bucket,
                    "penalty_multiplier": multiplier_value,
                    "base_penalty": base_penalty,
                    "penalty": result["penalty"],
                    "effective_dimension": features["effective_dimension"],
                    "sample_count": len(aggregated),
                    "boundaries": result["boundaries"],
                    "change_points": result["change_points"],
                    "boundary_time_ns": [
                        _boundary_time(aggregated, value) for value in result["boundaries"]
                    ],
                    "change_point_time_ns": [
                        _boundary_time(aggregated, value)
                        for value in result["change_points"]
                    ],
                    "total_sse": result["total_sse"],
                    "objective": result["objective"],
                }
            )
    tolerance_seconds = (
        max(buckets)
        if boundary_tolerance_seconds is None
        else boundary_tolerance_seconds
    )
    if tolerance_seconds < 0:
        raise StatisticsError("boundary_tolerance_seconds 不能为负")
    return {
        "vocabulary": vocabulary,
        "coverage": coverage,
        "alpha": alpha,
        "min_segment_seconds": min_segment_seconds,
        "configurations": records,
        "boundary_clusters": _cluster_sensitivity_boundaries(
            records, tolerance_seconds * NANOSECONDS_PER_SECOND
        ),
    }


def _moving_block_indices(length: int, block_length: int, rng: random.Random) -> list[int]:
    """生成圆形 moving-block bootstrap 索引。"""

    if length <= 0 or block_length <= 0:
        raise StatisticsError("moving-block 长度必须为正")
    result: list[int] = []
    width = min(length, block_length)
    while len(result) < length:
        start = rng.randrange(length)
        result.extend((start + offset) % length for offset in range(width))
    return result[:length]


def _moving_block_permutation_indices(
    length: int, block_length: int, rng: random.Random
) -> list[int]:
    """生成随机起点的圆形非重叠块置换索引。

    与 bootstrap 不同，这里每个观测恰好出现一次。随机圆形起点避免把
    块边界固定在原始 epoch 网格上，块内顺序则保留短程相关。
    """

    if length <= 0 or block_length <= 0:
        raise StatisticsError("moving-block permutation 长度必须为正")
    width = min(length, block_length)
    offset = rng.randrange(length)
    circular = [(offset + index) % length for index in range(length)]
    blocks = [
        circular[begin : begin + width]
        for begin in range(0, length, width)
    ]
    rng.shuffle(blocks)
    return [index for block in blocks for index in block]


def global_change_point_block_permutation_test(
    matrix: Sequence[Sequence[float]],
    *,
    penalty: float,
    min_segment_length: int,
    permutations: int = 999,
    block_length: int | None = None,
    significance_level: float = 0.05,
    seed: int = 0,
) -> dict[str, Any]:
    """对“全轨迹为单段”做包含边界选择的全局检验。

    观测统计量是最优分段相对单段模型的惩罚后 SSE 改善。每个
    moving-block permutation 都重新运行同一个变点选择器，因此 p 值覆盖
    “先看数据再选边界”的多重性。有效性依赖单段零假设下轨迹近似
    平稳，且 ``block_length`` 足以覆盖主要短程相关。
    """

    if not matrix or not matrix[0]:
        raise StatisticsError("matrix 必须是非空矩形")
    width = len(matrix[0])
    if any(len(row) != width for row in matrix):
        raise StatisticsError("matrix 必须是矩形")
    frozen = [[float(value) for value in row] for row in matrix]
    if any(not math.isfinite(value) for row in frozen for value in row):
        raise StatisticsError("matrix 包含非有限数")
    if isinstance(permutations, bool) or not isinstance(permutations, int):
        raise StatisticsError("permutations 必须是正整数")
    if permutations <= 0:
        raise StatisticsError("permutations 必须是正整数")
    if not 0.0 < significance_level < 1.0:
        raise StatisticsError("significance_level 必须位于 (0, 1)")

    sample_count = len(frozen)
    chosen_block = (
        min(sample_count, max(2, round(sample_count ** (1.0 / 3.0))))
        if block_length is None
        else block_length
    )
    if (
        isinstance(chosen_block, bool)
        or not isinstance(chosen_block, int)
        or chosen_block <= 0
        or chosen_block > sample_count
    ):
        raise StatisticsError("block_length 必须位于 1..len(matrix)")

    prefixes, squares = _sse_prefix(frozen)
    one_segment_sse = _segment_sse(prefixes, squares, 0, sample_count)

    def select_and_score(candidate: Sequence[Sequence[float]]) -> tuple[dict[str, Any], float]:
        selected = detect_change_points(
            candidate,
            penalty=penalty,
            min_segment_length=min_segment_length,
        )
        # detect_change_points 对每段收取一次 penalty，因此单段
        # 基线也含一次 penalty。这与“每多一个变点收费”等价。
        gain = one_segment_sse + penalty - float(selected["objective"])
        return selected, max(0.0, gain)

    observed_selection, observed_gain = select_and_score(frozen)
    rng = random.Random(seed)
    null_gains: list[float] = []
    exceedances = 0
    for _ in range(permutations):
        indices = _moving_block_permutation_indices(
            sample_count, chosen_block, rng
        )
        permuted = [frozen[index] for index in indices]
        _, gain = select_and_score(permuted)
        null_gains.append(gain)
        if gain >= observed_gain - 1e-12:
            exceedances += 1

    p_value = (exceedances + 1) / (permutations + 1)
    return {
        "method": "selection-corrected-moving-block-permutation-penalized-sse",
        "null_hypothesis": "single-stationary-segment",
        "selection_corrected": True,
        "penalty": float(penalty),
        "min_segment_length": min_segment_length,
        "block_length": chosen_block,
        "permutations": permutations,
        "significance_level": significance_level,
        "observed": {
            "boundaries": observed_selection["boundaries"],
            "change_points": observed_selection["change_points"],
            "segments": len(observed_selection["boundaries"]) - 1,
            "single_segment_sse": one_segment_sse,
            "selected_segment_sse": observed_selection["total_sse"],
            "selected_objective": observed_selection["objective"],
            "penalized_sse_gain": observed_gain,
        },
        "null_penalized_sse_gain": {
            "median": _percentile(null_gains, 0.5),
            "p90": _percentile(null_gains, 0.9),
            "p95": _percentile(null_gains, 0.95),
            "p99": _percentile(null_gains, 0.99),
            "maximum": max(null_gains),
        },
        "exceedances": exceedances,
        "p_value": p_value,
        "minimum_resolvable_p": 1.0 / (permutations + 1),
        "monte_carlo_standard_error": math.sqrt(
            p_value * (1.0 - p_value) / (permutations + 1)
        ),
        "reject_single_segment": p_value <= significance_level,
        "assumptions": [
            "single-segment null is approximately stationary",
            "block length covers material short-range dependence",
            "result establishes within-trajectory evidence, not cross-run stability",
        ],
    }


def _percentile(values: Sequence[float], probability: float) -> float:
    """使用线性插值的样本分位数。"""

    if not values:
        raise StatisticsError("无法计算空序列分位数")
    if not 0.0 <= probability <= 1.0:
        raise StatisticsError("分位概率必须位于 0..1")
    ordered = sorted(float(value) for value in values)
    position = probability * (len(ordered) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def _dependence_boundaries(
    sample_count: int, boundaries: Sequence[int] | None
) -> list[int]:
    """校验相关长度诊断使用的阶段边界。"""

    points = [0, sample_count] if boundaries is None else list(boundaries)
    if (
        len(points) < 2
        or points[0] != 0
        or points[-1] != sample_count
        or any(left >= right for left, right in zip(points, points[1:]))
    ):
        raise StatisticsError("dependence boundaries 必须严格递增并覆盖全部样本")
    return points


def _trimmed_semivariance(
    values: Sequence[float],
    boundaries: Sequence[int],
    lag: int,
    trim_fraction: float,
) -> tuple[float | None, int]:
    """计算不跨阶段边界的上尾截尾半方差。"""

    differences = sorted(
        (values[index + lag] - values[index]) ** 2
        for begin, end in zip(boundaries, boundaries[1:])
        for index in range(begin, end - lag)
    )
    if len(differences) < 4:
        return None, len(differences)
    kept = max(1, math.floor(len(differences) * (1.0 - trim_fraction)))
    return 0.5 * math.fsum(differences[:kept]) / kept, len(differences)


def _segment_centered_acf(
    values: Sequence[float], boundaries: Sequence[int], maximum_lag: int
) -> list[float]:
    """计算不跨阶段边界的 pooled ACF。"""

    centered = [0.0] * len(values)
    for begin, end in zip(boundaries, boundaries[1:]):
        mean = math.fsum(values[begin:end]) / (end - begin)
        for index in range(begin, end):
            centered[index] = values[index] - mean
    result: list[float] = []
    for lag in range(1, maximum_lag + 1):
        pairs = [
            (centered[index], centered[index + lag])
            for begin, end in zip(boundaries, boundaries[1:])
            for index in range(begin, end - lag)
        ]
        if not pairs:
            result.append(0.0)
            continue
        numerator = math.fsum(left * right for left, right in pairs)
        left_energy = math.fsum(left * left for left, _ in pairs)
        right_energy = math.fsum(right * right for _, right in pairs)
        denominator = math.sqrt(left_energy * right_energy)
        result.append(
            max(-1.0, min(1.0, numerator / denominator))
            if denominator > 1e-30
            else 0.0
        )
    return result


def _initial_positive_sequence_iat(acf: Sequence[float]) -> float:
    """以 Geyer 初始正对序列给出保守的积分自相关时间。"""

    paired: list[float] = []
    for begin in range(0, len(acf), 2):
        pair = math.fsum(acf[begin : begin + 2])
        if pair <= 0.0:
            break
        paired.append(pair)
    # 初始单调序列避免有限样本中后续 pair 的偶然反弹放大 IAT。
    for index in range(1, len(paired)):
        paired[index] = min(paired[index], paired[index - 1])
    return max(1.0, 1.0 + 2.0 * math.fsum(paired))


def _fit_variogram_ar1_phi(semivariances: Sequence[float | None]) -> float:
    """由稳健 variogram 比率拟合 AR(1) 等效持久度。"""

    if not semivariances or semivariances[0] is None:
        return 0.0
    baseline = float(semivariances[0])
    if baseline <= 1e-30:
        return 0.0
    observations = [
        (lag, float(value) / baseline)
        for lag, value in enumerate(semivariances[1:], 2)
        if value is not None and value > 0.0
    ]
    if len(observations) < 2:
        return 0.0

    def loss(phi: float) -> float:
        errors = []
        for lag, observed in observations:
            predicted = math.fsum(phi**power for power in range(lag))
            errors.append(abs(math.log(max(observed, 1e-15) / predicted)))
        return float(statistics.median(errors))

    # 0.001 的 phi 网格足以区分 1 秒 epoch 下会影响 block 选择的相关长度，
    # 又避免为每个 feature 引入非确定性的数值优化器。
    return min(
        ((loss(step / 1000.0), step / 1000.0) for step in range(1000)),
        key=lambda item: (item[0], item[1]),
    )[1]


def diagnose_serial_dependence(
    matrix: Sequence[Sequence[float]],
    *,
    boundaries: Sequence[int] | None = None,
    feature_names: Sequence[str] | None = None,
    max_lag: int | None = None,
    trim_fraction: float = 0.10,
    decorrelation_threshold: float = 0.10,
    block_iat_multiplier: float = 1.5,
    longer_block_multiplier: float = 1.5,
    minimum_independent_blocks: int = 8,
) -> dict[str, Any]:
    """诊断短程依赖并选择主/长两组保守 moving-block 长度。

    ACF 在每个候选阶段内去均值后 pooled；AR(1) 等效 IAT 则由不跨阶段
    边界的上尾截尾 variogram 拟合。后者对少数真正的均值跳变和离群 epoch
    不敏感，也不会像在已选段内再次去均值那样系统性抹掉慢相关。block 至少
    覆盖 ``1.5 * max(IAT)``、ACF/AR(1) 的 0.1 decorrelation lag 和
    ``n^(1/3)`` 基线。若轨迹不足以同时容纳至少八个更长 block，诊断会明确
    标记为不足，调用方不得据此声称高置信度。
    """

    if not matrix or not matrix[0]:
        raise StatisticsError("dependence matrix 必须是非空矩形")
    width = len(matrix[0])
    if any(len(row) != width for row in matrix):
        raise StatisticsError("dependence matrix 必须是矩形")
    frozen = [[float(value) for value in row] for row in matrix]
    if any(not math.isfinite(value) for row in frozen for value in row):
        raise StatisticsError("dependence matrix 包含非有限数")
    sample_count = len(frozen)
    points = _dependence_boundaries(sample_count, boundaries)
    names = (
        [f"feature:{index}" for index in range(width)]
        if feature_names is None
        else list(feature_names)
    )
    if len(names) != width or len(set(names)) != width or any(
        not isinstance(name, str) or not name for name in names
    ):
        raise StatisticsError("feature_names 必须与 matrix 列一一对应")
    if not 0.0 <= trim_fraction < 0.5:
        raise StatisticsError("trim_fraction 必须位于 [0, 0.5)")
    if not 0.0 < decorrelation_threshold < 1.0:
        raise StatisticsError("decorrelation_threshold 必须位于 (0, 1)")
    if block_iat_multiplier < 1.0 or longer_block_multiplier <= 1.0:
        raise StatisticsError("block multiplier 配置不够保守")
    if minimum_independent_blocks < 4:
        raise StatisticsError("minimum_independent_blocks 至少为 4")

    shortest_segment = min(right - left for left, right in zip(points, points[1:]))
    automatic_lag = max(4, round(2.0 * sample_count ** (1.0 / 3.0)))
    chosen_max_lag = (
        min(shortest_segment - 1, automatic_lag)
        if max_lag is None
        else max_lag
    )
    if (
        isinstance(chosen_max_lag, bool)
        or not isinstance(chosen_max_lag, int)
        or chosen_max_lag < 2
        or chosen_max_lag >= shortest_segment
    ):
        raise StatisticsError("max_lag 必须位于 2..最短阶段长度-1")

    per_feature: list[dict[str, Any]] = []
    for column, name in enumerate(names):
        values = [row[column] for row in frozen]
        acf = _segment_centered_acf(values, points, chosen_max_lag)
        direct_iat = _initial_positive_sequence_iat(acf)
        semivariances: list[float | None] = []
        pair_counts: list[int] = []
        for lag in range(1, chosen_max_lag + 1):
            semivariance, pairs = _trimmed_semivariance(
                values, points, lag, trim_fraction
            )
            semivariances.append(semivariance)
            pair_counts.append(pairs)
        phi = _fit_variogram_ar1_phi(semivariances)
        ar1_iat = (1.0 + phi) / max(1e-6, 1.0 - phi)
        ar1_decorrelation_lag = (
            math.ceil(math.log(decorrelation_threshold) / math.log(phi))
            if 0.0 < phi < 1.0
            else 1
        )
        acf_decorrelation_lag = chosen_max_lag + 1
        stable_run = min(3, chosen_max_lag)
        for lag in range(1, chosen_max_lag - stable_run + 2):
            if all(
                abs(acf[index]) <= decorrelation_threshold
                for index in range(lag - 1, lag - 1 + stable_run)
            ):
                acf_decorrelation_lag = lag
                break
        effective_iat = max(direct_iat, ar1_iat)
        effective_decorrelation_lag = max(
            acf_decorrelation_lag, ar1_decorrelation_lag
        )
        per_feature.append(
            {
                "feature": name,
                "acf": acf,
                "initial_positive_sequence_iat": direct_iat,
                "trimmed_variogram": semivariances,
                "variogram_pair_counts": pair_counts,
                "variogram_ar1_phi": phi,
                "ar1_equivalent_iat": ar1_iat,
                "acf_decorrelation_lag": acf_decorrelation_lag,
                "ar1_decorrelation_lag": ar1_decorrelation_lag,
                "effective_iat": effective_iat,
                "effective_decorrelation_lag": effective_decorrelation_lag,
            }
        )

    maximum_iat = max(row["effective_iat"] for row in per_feature)
    maximum_decorrelation = max(
        row["effective_decorrelation_lag"] for row in per_feature
    )
    cubic_root_baseline = max(2, round(sample_count ** (1.0 / 3.0)))
    required_primary = math.ceil(
        max(
            cubic_root_baseline,
            block_iat_multiplier * maximum_iat,
            maximum_decorrelation,
        )
    )
    maximum_supported = max(2, sample_count // minimum_independent_blocks)
    primary = min(required_primary, maximum_supported)
    required_long = math.ceil(
        max(primary + 1, longer_block_multiplier * primary)
    )
    longer = min(required_long, maximum_supported)
    adequate = (
        required_primary <= maximum_supported
        and required_long <= maximum_supported
        and longer > primary
    )
    return {
        "method": "segment-aware-acf-trimmed-variogram-ar1-iat",
        "sample_count": sample_count,
        "feature_count": width,
        "boundaries": points,
        "max_lag": chosen_max_lag,
        "trim_fraction": trim_fraction,
        "decorrelation_threshold": decorrelation_threshold,
        "block_iat_multiplier": block_iat_multiplier,
        "longer_block_multiplier": longer_block_multiplier,
        "minimum_independent_blocks": minimum_independent_blocks,
        "maximum_effective_iat": maximum_iat,
        "maximum_effective_decorrelation_lag": maximum_decorrelation,
        "cubic_root_baseline": cubic_root_baseline,
        "required_primary_block_length": required_primary,
        "primary_block_length": primary,
        "required_long_block_length": required_long,
        "long_block_length": longer,
        "maximum_supported_block_length": maximum_supported,
        "nominal_blocks_at_primary": sample_count / primary,
        "nominal_blocks_at_long": sample_count / longer,
        "adequate_for_high_confidence": adequate,
        "per_feature": per_feature,
        "assumptions": [
            "within-stage dependence is approximately short-memory and stationary",
            "trimmed within-stage increments remove only sparse jumps/outliers",
            "at least eight non-overlapping long blocks are needed for a high-confidence claim",
        ],
    }


def global_change_point_block_sensitivity_test(
    matrix: Sequence[Sequence[float]],
    *,
    penalty: float,
    min_segment_length: int,
    dependence: Mapping[str, Any],
    primary_permutations: int = 999,
    long_permutations: int = 399,
    significance_level: float = 0.05,
    seed: int = 0,
) -> dict[str, Any]:
    """在诊断得到的主/长 block 上重复选择校正全局检验。"""

    primary_block = int(dependence.get("primary_block_length", 0))
    long_block = int(dependence.get("long_block_length", 0))
    if primary_block <= 0 or long_block < primary_block:
        raise StatisticsError("dependence 未提供合法的主/长 block")
    primary = global_change_point_block_permutation_test(
        matrix,
        penalty=penalty,
        min_segment_length=min_segment_length,
        permutations=primary_permutations,
        block_length=primary_block,
        significance_level=significance_level,
        seed=seed,
    )
    longer = global_change_point_block_permutation_test(
        matrix,
        penalty=penalty,
        min_segment_length=min_segment_length,
        permutations=long_permutations,
        block_length=long_block,
        significance_level=significance_level,
        seed=seed + 1,
    )
    conclusions = [
        bool(primary["reject_single_segment"]),
        bool(longer["reject_single_segment"]),
    ]
    conclusions_agree = conclusions[0] == conclusions[1]
    dependence_adequate = dependence.get("adequate_for_high_confidence") is True
    return {
        "method": "dependence-diagnosed-two-block-selection-corrected-permutation",
        "significance_level": significance_level,
        "dependence_adequate": dependence_adequate,
        "tests": [primary, longer],
        "conclusions_agree": conclusions_agree,
        "all_reject_single_segment": all(conclusions),
        "all_fail_to_reject_single_segment": not any(conclusions),
        "high_confidence_eligible": dependence_adequate and conclusions_agree,
        "interpretation": (
            "高置信门禁要求由ACF/IAT选择的主block与更长block结论一致；"
            "两次检验都重新选择边界，相邻段JS不参与该门禁。"
        ),
    }


def _match_boundaries(
    expected: Sequence[int], observed: Sequence[int], tolerance: int
) -> dict[int, int]:
    """按全局最小距离贪心匹配边界，且每个观测最多使用一次。"""

    candidates = sorted(
        (abs(left - right), left, right)
        for left in expected
        for right in observed
        if abs(left - right) <= tolerance
    )
    result: dict[int, int] = {}
    used: set[int] = set()
    for _, left, right in candidates:
        if left not in result and right not in used:
            result[left] = right
            used.add(right)
    return result


def moving_block_bootstrap_boundary_stability(
    matrix: Sequence[Sequence[float]],
    boundaries: Sequence[int],
    *,
    penalty: float,
    min_segment_length: int,
    replicates: int = 200,
    block_length: int | None = None,
    match_tolerance: int | None = None,
    confidence: float = 0.95,
    seed: int = 0,
) -> dict[str, Any]:
    """用分段残差 moving-block bootstrap 评估边界稳定性。

    每段先减去自身均值，再在段内圆形重采样残差并加回拟合均值。这样既保留
    候选阶段，又保留短程自相关，不会像逐点 bootstrap 那样人为提高置信度。
    """

    sample_count = len(matrix)
    if sample_count == 0 or any(len(row) != len(matrix[0]) for row in matrix):
        raise StatisticsError("matrix 必须是非空矩形")
    expected_boundaries = list(boundaries)
    if (
        len(expected_boundaries) < 2
        or expected_boundaries[0] != 0
        or expected_boundaries[-1] != sample_count
        or any(left >= right for left, right in zip(expected_boundaries, expected_boundaries[1:]))
    ):
        raise StatisticsError("boundaries 必须严格递增并覆盖 0..len(matrix)")
    if any(
        right - left < min_segment_length
        for left, right in zip(expected_boundaries, expected_boundaries[1:])
    ):
        raise StatisticsError("boundaries 含短于 min_segment_length 的段")
    if isinstance(replicates, bool) or not isinstance(replicates, int) or replicates <= 0:
        raise StatisticsError("replicates 必须是正整数")
    if not 0.0 < confidence < 1.0:
        raise StatisticsError("confidence 必须位于 (0, 1)")
    chosen_block = (
        max(2, round(sample_count ** (1.0 / 3.0)))
        if block_length is None
        else block_length
    )
    if isinstance(chosen_block, bool) or not isinstance(chosen_block, int) or chosen_block <= 0:
        raise StatisticsError("block_length 必须是正整数")
    tolerance = max(1, chosen_block) if match_tolerance is None else match_tolerance
    if isinstance(tolerance, bool) or not isinstance(tolerance, int) or tolerance < 0:
        raise StatisticsError("match_tolerance 必须是非负整数")

    width = len(matrix[0])
    fitted: list[list[float]] = [[0.0] * width for _ in range(sample_count)]
    residuals: list[list[float]] = [[0.0] * width for _ in range(sample_count)]
    for begin, end in zip(expected_boundaries, expected_boundaries[1:]):
        means = [
            math.fsum(float(matrix[row][column]) for row in range(begin, end))
            / (end - begin)
            for column in range(width)
        ]
        for row in range(begin, end):
            fitted[row] = list(means)
            residuals[row] = [float(matrix[row][column]) - means[column] for column in range(width)]

    rng = random.Random(seed)
    expected = expected_boundaries[1:-1]
    matches: dict[int, list[int]] = {boundary: [] for boundary in expected}
    observed_change_counts: list[int] = []
    unmatched_count = 0
    exact_change_count = 0
    for _ in range(replicates):
        synthetic: list[list[float]] = []
        for begin, end in zip(expected_boundaries, expected_boundaries[1:]):
            segment_length = end - begin
            sampled = _moving_block_indices(segment_length, chosen_block, rng)
            for local_index in sampled:
                source = begin + local_index
                synthetic.append(
                    [fitted[begin][column] + residuals[source][column] for column in range(width)]
                )
        detected = detect_change_points(
            synthetic,
            penalty=penalty,
            min_segment_length=min_segment_length,
        )["change_points"]
        observed_change_counts.append(len(detected))
        if len(detected) == len(expected):
            exact_change_count += 1
        matched = _match_boundaries(expected, detected, tolerance)
        for boundary, observed in matched.items():
            matches[boundary].append(observed)
        unmatched_count += len(detected) - len(matched)

    tail = (1.0 - confidence) / 2.0
    boundary_results: list[dict[str, Any]] = []
    for boundary in expected:
        locations = matches[boundary]
        errors = [abs(value - boundary) for value in locations]
        boundary_results.append(
            {
                "boundary": boundary,
                "matched_replicates": len(locations),
                "stability_probability": len(locations) / replicates,
                "conditional_median": _median(locations) if locations else None,
                "conditional_ci": (
                    [_percentile(locations, tail), _percentile(locations, 1.0 - tail)]
                    if locations
                    else None
                ),
                "conditional_median_absolute_error": _median(errors) if errors else None,
            }
        )
    return {
        "method": "piecewise-residual-circular-moving-block-bootstrap",
        "replicates": replicates,
        "block_length": chosen_block,
        "match_tolerance": tolerance,
        "confidence": confidence,
        "boundaries": boundary_results,
        "exact_change_count_probability": exact_change_count / replicates,
        "mean_detected_change_points": math.fsum(observed_change_counts) / replicates,
        "mean_unmatched_change_points": unmatched_count / replicates,
    }


def jensen_shannon_divergence(
    left: Sequence[float], right: Sequence[float], *, alpha: float = 0.5
) -> float:
    """计算使用自然对数、Jeffreys 平滑的 Jensen-Shannon divergence。"""

    if len(left) != len(right) or not left:
        raise StatisticsError("JS 输入必须是同宽非空向量")
    if not math.isfinite(alpha) or alpha <= 0.0:
        raise StatisticsError("JS alpha 必须是正有限数")
    left_values = [_finite_number(value, "left JS value") + alpha for value in left]
    right_values = [_finite_number(value, "right JS value") + alpha for value in right]
    left_total = math.fsum(left_values)
    right_total = math.fsum(right_values)
    left_probability = [value / left_total for value in left_values]
    right_probability = [value / right_total for value in right_values]
    middle = [
        (left_value + right_value) / 2.0
        for left_value, right_value in zip(left_probability, right_probability)
    ]
    return 0.5 * math.fsum(
        value * math.log(value / center)
        for value, center in zip(left_probability, middle)
    ) + 0.5 * math.fsum(
        value * math.log(value / center)
        for value, center in zip(right_probability, middle)
    )


def holm_adjust(p_values: Sequence[float]) -> list[float]:
    """返回保持原顺序的 Holm step-down family-wise 校正 p 值。"""

    parsed = []
    for index, value in enumerate(p_values):
        number = _finite_number(value, f"p_values[{index}]")
        if number > 1.0:
            raise StatisticsError("p 值必须位于 0..1")
        parsed.append(number)
    ordered = sorted(range(len(parsed)), key=lambda index: (parsed[index], index))
    adjusted = [0.0] * len(parsed)
    running = 0.0
    count = len(parsed)
    for rank, index in enumerate(ordered):
        running = max(running, (count - rank) * parsed[index])
        adjusted[index] = min(1.0, running)
    return adjusted


def _sum_count_rows(
    rows: Sequence[Mapping[str, float]], vocabulary: Sequence[str]
) -> list[float]:
    """合计一组 count rows 并折叠到固定词表。"""

    result = [0.0] * (len(vocabulary) + 1)
    for row in rows:
        vector = collapse_counts(row, vocabulary)
        for index, value in enumerate(vector):
            result[index] += value
    return result


def _count_blocks(
    count_rows: Sequence[Mapping[str, float]], block_length: int
) -> list[list[Mapping[str, float]]]:
    """切出不跨越原阶段边界、等长的非重叠置换块。"""

    return [
        list(count_rows[begin : begin + block_length])
        for begin in range(0, len(count_rows) - block_length + 1, block_length)
    ]


def adjacent_segment_block_permutation_js(
    epoch_rows: Sequence[Any],
    boundaries: Sequence[int],
    *,
    vocabulary: Sequence[str] | None = None,
    coverage: float = 0.995,
    alpha: float = 0.5,
    block_length: int = 5,
    permutations: int = 999,
    family_alpha: float = 0.05,
    seed: int = 0,
) -> dict[str, Any]:
    """对每对相邻阶段做 block permutation JS 检验，并作 Holm 校正。"""

    rows = _validate_epoch_rows(epoch_rows)
    sample_count = len(rows)
    points = list(boundaries)
    if (
        len(points) < 2
        or points[0] != 0
        or points[-1] != sample_count
        or any(left >= right for left, right in zip(points, points[1:]))
    ):
        raise StatisticsError("boundaries 必须严格递增并覆盖全部 epoch")
    if isinstance(block_length, bool) or not isinstance(block_length, int) or block_length <= 0:
        raise StatisticsError("block_length 必须是正整数")
    if isinstance(permutations, bool) or not isinstance(permutations, int) or permutations <= 0:
        raise StatisticsError("permutations 必须是正整数")
    if not 0.0 < family_alpha < 1.0:
        raise StatisticsError("family_alpha 必须位于 (0, 1)")
    selected = (
        select_top_components([row["counts"] for row in rows], coverage)
        if vocabulary is None
        else _validate_vocabulary(vocabulary)
    )
    rng = random.Random(seed)
    tests: list[dict[str, Any]] = []
    raw_p_values: list[float] = []
    for segment_index in range(len(points) - 2):
        left_begin, boundary, right_end = points[segment_index : segment_index + 3]
        left_rows = [row["counts"] for row in rows[left_begin:boundary]]
        right_rows = [row["counts"] for row in rows[boundary:right_end]]
        left_blocks = _count_blocks(left_rows, block_length)
        right_blocks = _count_blocks(right_rows, block_length)
        if not left_blocks or not right_blocks:
            raise StatisticsError("相邻阶段长度必须至少覆盖一个完整置换块")
        usable_left = [row for block in left_blocks for row in block]
        usable_right = [row for block in right_blocks for row in block]
        observed = jensen_shannon_divergence(
            _sum_count_rows(usable_left, selected),
            _sum_count_rows(usable_right, selected),
            alpha=alpha,
        )
        pooled = left_blocks + right_blocks
        exceedances = 0
        for _ in range(permutations):
            shuffled = list(pooled)
            rng.shuffle(shuffled)
            permuted_left = [row for block in shuffled[: len(left_blocks)] for row in block]
            permuted_right = [row for block in shuffled[len(left_blocks) :] for row in block]
            divergence = jensen_shannon_divergence(
                _sum_count_rows(permuted_left, selected),
                _sum_count_rows(permuted_right, selected),
                alpha=alpha,
            )
            if divergence >= observed - 1e-15:
                exceedances += 1
        p_value = (exceedances + 1) / (permutations + 1)
        raw_p_values.append(p_value)
        tests.append(
            {
                "left_segment": segment_index,
                "right_segment": segment_index + 1,
                "boundary": boundary,
                "left_epochs": len(left_rows),
                "right_epochs": len(right_rows),
                "left_epochs_used": len(usable_left),
                "right_epochs_used": len(usable_right),
                "left_tail_epochs_discarded": len(left_rows) - len(usable_left),
                "right_tail_epochs_discarded": len(right_rows) - len(usable_right),
                "left_blocks": len(left_blocks),
                "right_blocks": len(right_blocks),
                "js_divergence_nats": observed,
                "p_value": p_value,
                "monte_carlo_standard_error": math.sqrt(
                    p_value * (1.0 - p_value) / (permutations + 1)
                ),
                "minimum_resolvable_p": 1.0 / (permutations + 1),
            }
        )
    adjusted = holm_adjust(raw_p_values)
    for test, adjusted_value in zip(tests, adjusted):
        test["holm_adjusted_p"] = adjusted_value
        test["reject_equal_distribution"] = adjusted_value <= family_alpha
    return {
        "method": "adjacent-segment-block-permutation-jensen-shannon",
        "vocabulary": selected,
        "components": selected + [OTHER_COMPONENT],
        "block_length": block_length,
        "permutations": permutations,
        "family_alpha": family_alpha,
        "tests": tests,
        "all_adjacent_pairs_significant": bool(tests)
        and all(test["reject_equal_distribution"] for test in tests),
    }


def _validate_weighted_rows(rows: Sequence[Any]) -> list[dict[str, Any]]:
    """校验 weighted rows，并保留逐指令归因元数据。"""

    if not rows:
        raise StatisticsError("weighted rows 不能为空")
    result: list[dict[str, Any]] = []
    previous_time = -1
    numeric_metadata = (
        "exact_count",
        "attributed_task_clock_ns",
        "weight_ns_per_instruction",
        "shrinkage",
        "unattributed",
    )
    for index, row in enumerate(rows):
        owner = f"weighted_rows[{index}]"
        timestamp = _time_ns(row, owner)
        if timestamp <= previous_time:
            raise StatisticsError("weighted rows 必须按 time_ns 严格递增")
        previous_time = timestamp
        frozen: dict[str, Any] = {
            "time_ns": timestamp,
            "values": _numeric_mapping(row, "values", owner),
        }
        for field in numeric_metadata:
            raw = _optional_mapping(row, field, owner)
            parsed: dict[str, float] = {}
            for name, value in raw.items():
                parsed[name] = _finite_number(value, f"{owner}.{field}[{name!r}]")
            frozen[field] = parsed
        source = _optional_mapping(row, "source", owner)
        frozen["source"] = {
            name: str(value) for name, value in source.items() if str(value)
        }
        result.append(frozen)
    return result


def _aggregate_metadata(
    rows: Sequence[Mapping[str, Any]], members: Sequence[str]
) -> dict[str, Any]:
    """聚合一条指令或 OTHER 成员的时间归因元数据。"""

    member_set = set(members)
    exact_count = math.fsum(
        value
        for row in rows
        for name, value in row["exact_count"].items()
        if name in member_set
    )
    attributed_ns = math.fsum(
        value
        for row in rows
        for name, value in row["attributed_task_clock_ns"].items()
        if name in member_set
    )
    weighted_weight_sum = 0.0
    weight_denominator = 0.0
    shrinkage_weighted_sum = 0.0
    shrinkage_denominator = 0.0
    reported_weights: list[float] = []
    reported_shrinkage: list[float] = []
    sources: set[str] = set()
    for row in rows:
        for name in member_set:
            count = row["exact_count"].get(name, 0.0)
            if name in row["weight_ns_per_instruction"]:
                reported = row["weight_ns_per_instruction"][name]
                reported_weights.append(reported)
                if count > 0.0:
                    weighted_weight_sum += reported * count
                    weight_denominator += count
            if name in row["shrinkage"]:
                reported = row["shrinkage"][name]
                reported_shrinkage.append(reported)
                if count > 0.0:
                    shrinkage_weighted_sum += reported * count
                    shrinkage_denominator += count
            if name in row["source"]:
                sources.add(row["source"][name])
    return {
        "exact_count": exact_count,
        "attributed_task_clock_ns": attributed_ns,
        "weight_ns_per_instruction": (
            weighted_weight_sum / weight_denominator if weight_denominator else None
        ),
        "reported_weight_ns_per_instruction_range": (
            [min(reported_weights), max(reported_weights)] if reported_weights else None
        ),
        "shrinkage": (
            shrinkage_weighted_sum / shrinkage_denominator
            if shrinkage_denominator
            else None
        ),
        "reported_shrinkage_range": (
            [min(reported_shrinkage), max(reported_shrinkage)]
            if reported_shrinkage
            else None
        ),
        "sources": sorted(sources),
    }


def _integrated_autocorrelation_time(values: Sequence[float]) -> float:
    """以初始正序列估计积分自相关时间。"""

    count = len(values)
    if count < 3:
        return 1.0
    mean = math.fsum(values) / count
    centered = [value - mean for value in values]
    variance = math.fsum(value * value for value in centered)
    if variance <= 1e-30:
        return 1.0
    rho_sum = 0.0
    maximum_lag = min(count - 1, max(1, round(10.0 * math.sqrt(count))))
    for lag in range(1, maximum_lag + 1):
        covariance = math.fsum(
            centered[index] * centered[index + lag]
            for index in range(count - lag)
        )
        rho = covariance / variance
        if rho <= 0.0:
            break
        rho_sum += rho
    return max(1.0, 1.0 + 2.0 * rho_sum)


def _weighted_ess(
    component: Sequence[float], totals: Sequence[float], estimate: float
) -> dict[str, float]:
    """结合 Kish 不等权修正与组成影响函数自相关估计 ESS。"""

    total_sum = math.fsum(totals)
    square_sum = math.fsum(value * value for value in totals)
    kish = total_sum * total_sum / square_sum if square_sum > 0.0 else 0.0
    influence = [value - estimate * total for value, total in zip(component, totals)]
    tau = _integrated_autocorrelation_time(influence)
    return {
        "kish_epoch_ess": kish,
        "integrated_autocorrelation_time": tau,
        "effective_sample_size": min(float(len(component)), kish / tau) if kish else 0.0,
    }


def weighted_distribution_block_bootstrap(
    weighted_rows: Sequence[Any],
    *,
    coverage: float = 0.995,
    vocabulary: Sequence[str] | None = None,
    block_length: int | None = None,
    replicates: int = 1000,
    confidence: float = 0.95,
    top_k: int = 10,
    seed: int = 0,
) -> dict[str, Any]:
    """估计带权指令分布的 block-bootstrap CI、ESS 与 top-k 概率。

    ``values`` 必须已经由可信耗时估计乘以精确执行次数。只有 exact_count、但
    没有可靠 ``values`` 的指令会进入 ``unattributed``，绝不会回退为 1.0。
    """

    rows = _validate_weighted_rows(weighted_rows)
    if isinstance(replicates, bool) or not isinstance(replicates, int) or replicates <= 0:
        raise StatisticsError("replicates 必须是正整数")
    if not 0.0 < confidence < 1.0:
        raise StatisticsError("confidence 必须位于 (0, 1)")
    if isinstance(top_k, bool) or not isinstance(top_k, int) or top_k <= 0:
        raise StatisticsError("top_k 必须是正整数")
    selected = (
        select_top_components([row["values"] for row in rows], coverage)
        if vocabulary is None
        else _validate_vocabulary(vocabulary)
    )
    all_attributed = sorted({name for row in rows for name in row["values"]})
    selected_set = set(selected)
    other_members = [name for name in all_attributed if name not in selected_set]
    components = selected + [OTHER_COMPONENT]
    vectors = [collapse_counts(row["values"], selected) for row in rows]
    totals = [math.fsum(vector) for vector in vectors]
    grand_total = math.fsum(totals)
    if grand_total <= 0.0:
        raise StatisticsError("weighted rows 没有正的已归因成本")
    aggregate = [math.fsum(row[column] for row in vectors) for column in range(len(components))]
    estimate = [value / grand_total for value in aggregate]

    chosen_block = max(2, round(len(rows) ** (1.0 / 3.0))) if block_length is None else block_length
    if isinstance(chosen_block, bool) or not isinstance(chosen_block, int) or chosen_block <= 0:
        raise StatisticsError("block_length 必须是正整数")
    rng = random.Random(seed)
    bootstrap_shares: list[list[float]] = [[] for _ in components]
    top_counts = {name: 0 for name in selected}
    valid_replicates = 0
    for _ in range(replicates):
        indices = _moving_block_indices(len(rows), chosen_block, rng)
        sample = [
            math.fsum(vectors[row][column] for row in indices)
            for column in range(len(components))
        ]
        sample_total = math.fsum(sample)
        if sample_total <= 0.0:
            continue
        shares = [value / sample_total for value in sample]
        for column, value in enumerate(shares):
            bootstrap_shares[column].append(value)
        ranked = sorted(
            ((shares[index], name) for index, name in enumerate(selected)),
            key=lambda item: (-item[0], item[1]),
        )
        for _, name in ranked[: min(top_k, len(ranked))]:
            top_counts[name] += 1
        valid_replicates += 1
    if valid_replicates == 0:
        raise StatisticsError("所有 bootstrap replicate 都没有已归因成本")

    tail = (1.0 - confidence) / 2.0
    items: list[dict[str, Any]] = []
    for index, name in enumerate(components):
        members = [name] if name != OTHER_COMPONENT else other_members
        ess = _weighted_ess(
            [row[index] for row in vectors], totals, estimate[index]
        )
        metadata = _aggregate_metadata(rows, members)
        items.append(
            {
                "instruction": name,
                "members": members if name == OTHER_COMPONENT else None,
                "weighted_cost": aggregate[index],
                "share": estimate[index],
                "confidence_interval": [
                    _percentile(bootstrap_shares[index], tail),
                    _percentile(bootstrap_shares[index], 1.0 - tail),
                ],
                "effective_sample_size": ess["effective_sample_size"],
                "kish_epoch_ess": ess["kish_epoch_ess"],
                "integrated_autocorrelation_time": ess[
                    "integrated_autocorrelation_time"
                ],
                "top_k_probability": (
                    top_counts[name] / valid_replicates
                    if name != OTHER_COMPONENT
                    else None
                ),
                **metadata,
            }
        )
    items.sort(key=lambda item: (-item["share"], item["instruction"]))

    unattributed_counts: dict[str, float] = {}
    unattributed_sources: dict[str, set[str]] = {}
    for row in rows:
        explicit = row["unattributed"]
        names = set(explicit) | (set(row["exact_count"]) - set(row["values"]))
        for name in names:
            count = explicit.get(name, row["exact_count"].get(name, 0.0))
            unattributed_counts[name] = unattributed_counts.get(name, 0.0) + count
            if name in row["source"]:
                unattributed_sources.setdefault(name, set()).add(row["source"][name])
    unattributed = [
        {
            "instruction": name,
            "exact_count": count,
            "sources": sorted(unattributed_sources.get(name, set())),
        }
        for name, count in sorted(
            unattributed_counts.items(), key=lambda item: (-item[1], item[0])
        )
    ]
    return {
        "method": "circular-moving-block-bootstrap-ratio-estimator",
        "coverage": coverage,
        "vocabulary": selected,
        "components": components,
        "other_members": other_members,
        "weighted_cost_total": grand_total,
        "row_count": len(rows),
        "block_length": chosen_block,
        "nominal_independent_blocks": len(rows) / min(len(rows), chosen_block),
        "replicates_requested": replicates,
        "replicates_valid": valid_replicates,
        "confidence": confidence,
        "top_k": top_k,
        "items": items,
        "unattributed": unattributed,
        "unattributed_exact_count": math.fsum(unattributed_counts.values()),
    }


def weighted_stage_distributions(
    weighted_rows: Sequence[Any],
    boundaries: Sequence[int],
    *,
    block_lengths: Sequence[int] | None = None,
    **bootstrap_options: Any,
) -> list[dict[str, Any]]:
    """按已确认的 epoch 边界生成逐阶段带权分布。"""

    rows = list(weighted_rows)
    points = list(boundaries)
    if (
        len(points) < 2
        or points[0] != 0
        or points[-1] != len(rows)
        or any(left >= right for left, right in zip(points, points[1:]))
    ):
        raise StatisticsError("boundaries 必须严格递增并覆盖 weighted rows")
    stage_blocks = None if block_lengths is None else list(block_lengths)
    if stage_blocks is not None and (
        len(stage_blocks) != len(points) - 1
        or any(
            isinstance(value, bool) or not isinstance(value, int) or value <= 0
            for value in stage_blocks
        )
    ):
        raise StatisticsError("block_lengths 必须为每个阶段提供一个正整数")
    result: list[dict[str, Any]] = []
    for stage, (begin, end) in enumerate(zip(points, points[1:])):
        options = dict(bootstrap_options)
        if stage_blocks is not None:
            options["block_length"] = stage_blocks[stage]
        distribution = weighted_distribution_block_bootstrap(
            rows[begin:end], **options
        )
        result.append(
            {
                "stage": stage,
                "row_begin": begin,
                "row_end": end,
                "start_time_ns": _time_ns(rows[begin], f"weighted_rows[{begin}]"),
                "end_time_ns": _time_ns(rows[end - 1], f"weighted_rows[{end - 1}]"),
                "distribution": distribution,
            }
        )
    return result


def assess_distribution_confidence(
    sensitivity: Mapping[str, Any],
    boundary_stability: Mapping[str, Any],
    permutation_tests: Mapping[str, Any],
    weighted_distributions: Sequence[Mapping[str, Any]],
    *,
    minimum_sensitivity_support: float = 0.75,
    minimum_boundary_stability: float = 0.8,
    minimum_ess: float = 20.0,
    minimum_top_k_probability: float = 0.9,
) -> dict[str, Any]:
    """把可审计的统计门槛汇总成“高置信度”判定，而非口头断言。"""

    sensitivity_ok = all(
        cluster["support_fraction"] >= minimum_sensitivity_support
        for cluster in sensitivity.get("boundary_clusters", [])
    )
    stability_rows = boundary_stability.get("boundaries", [])
    stability_ok = all(
        row["stability_probability"] >= minimum_boundary_stability
        for row in stability_rows
    )
    permutation_rows = permutation_tests.get("tests", [])
    permutation_ok = bool(permutation_rows) and all(
        row.get("reject_equal_distribution") is True for row in permutation_rows
    )
    weak_items: list[dict[str, Any]] = []
    for stage in weighted_distributions:
        distribution = stage.get("distribution", stage)
        ranked = sorted(
            (
                item
                for item in distribution.get("items", [])
                if item.get("instruction") != OTHER_COMPONENT
            ),
            key=lambda item: (-item.get("share", 0.0), str(item.get("instruction"))),
        )
        expected_top_k = int(distribution.get("top_k", len(ranked)))
        for item in ranked[:expected_top_k]:
            probability = item.get("top_k_probability")
            ess = item.get("effective_sample_size", 0.0)
            if (
                probability is None
                or probability < minimum_top_k_probability
                or ess < minimum_ess
            ):
                weak_items.append(
                    {
                        "stage": stage.get("stage"),
                        "instruction": item.get("instruction"),
                        "effective_sample_size": ess,
                        "top_k_probability": probability,
                    }
                )
    weighted_ok = not weak_items
    reasons: list[str] = []
    if not sensitivity_ok:
        reasons.append("分段边界对桶宽/惩罚敏感")
    if not stability_ok:
        reasons.append("bootstrap 边界复现概率不足")
    if not permutation_ok:
        reasons.append("相邻阶段分布差异未通过 Holm 校正")
    if not weighted_ok:
        reasons.append("稳定 top-k 指令的有效样本量不足")
    return {
        "high_confidence": sensitivity_ok and stability_ok and permutation_ok and weighted_ok,
        "sensitivity_ok": sensitivity_ok,
        "boundary_stability_ok": stability_ok,
        "adjacent_distributions_distinct": permutation_ok,
        "weighted_effective_sample_size_ok": weighted_ok,
        "weak_top_k_items": weak_items,
        "reasons": reasons,
        "thresholds": {
            "minimum_sensitivity_support": minimum_sensitivity_support,
            "minimum_boundary_stability": minimum_boundary_stability,
            "minimum_ess": minimum_ess,
            "minimum_top_k_probability": minimum_top_k_probability,
        },
    }
