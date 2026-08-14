#!/usr/bin/env python3
"""RISC-V 指令微基准的稳健权重与同时置信区间模型。

探针必须把同一批量、同一实现形态的 probe/baseline 窗口配成一对，并随机
交错 AB/BA 顺序。模型以 QEMU vCPU 线程 CPU-time 为主响应，通过成对差分
消除锚点、计时器和共同循环开销；guest time 仅用于独立一致性检查。
"""

from __future__ import annotations

import argparse
import concurrent.futures
import csv
import io
import json
import math
import os
import random
import re
import statistics
from collections import defaultdict
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from statistics import NormalDist
from typing import Any


SCHEMA_VERSION = 3
EMPTY_CONTROL = "<empty>"
STABILITY_ANCHOR_PATTERN = "stability-anchor-positive-div"
CALIBRATION_ONLY_PATTERNS = frozenset({STABILITY_ANCHOR_PATTERN})
ANCHOR_REFERENCE_POSITION = "body"
ANCHOR_MAX_SCALE_RATIO = 1.10
MAX_TRANSLATION_EXCLUDED_PAIR_FRACTION = 0.02
MIN_CROSSOVER_DESIGN_FRACTION = 0.40
PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES = 4999
PUBLICATION_MINIMUM_MAX_STAT_CALIBRATION_REPLICATES = 4000
PUBLICATION_MAX_STAT_SCALE_REPLICATES = 999
MAX_STATISTIC_SCALE_REPLICATE_DIVISOR = 5
PUBLICATION_SAMPLING_ALPHA_FRACTION = 0.5
PUBLICATION_MONTE_CARLO_ALPHA_FRACTION = 0.5
PUBLICATION_CONFIDENCE = 0.95
PUBLICATION_BOOTSTRAP_SEED = 0x525643
PUBLICATION_MIN_PAIRS = 30
PUBLICATION_MIN_EFFECTIVE_PAIRS = 20.0
PUBLICATION_MIN_SUPER_RUNS = 10
PUBLICATION_MIN_COUNT_LEVELS = 3
PUBLICATION_MIN_PURITY = 0.99
PUBLICATION_MAX_RELATIVE_CI_HALF_WIDTH = 0.15
PUBLICATION_MAX_I_SQUARED = 0.40
PUBLICATION_EQUIVALENCE_MARGIN = 0.10
PUBLICATION_MIN_CROSS_CLOCK_RATIO = 0.75
PUBLICATION_MAX_CROSS_CLOCK_RATIO = 1.50
PUBLICATION_MIN_PLUGIN_OFF_RATIO = 0.85
PUBLICATION_MAX_PLUGIN_OFF_RATIO = 1.15
PUBLICATION_MAX_ZERO_COST_CI_UPPER_NS = 0.15
PUBLICATION_MAX_TRANSLATION_DENSITY = 0.002
PUBLICATION_MAX_SEVERE_OUTLIER_FRACTION = 0.10
GENERATION_CONFIGURATION_SCHEMA = (
    "mygo.riscv-instruction-weight-generation-configuration.v1"
)
PUBLICATION_INFERENCE_FAMILIES = (
    "raw-absolute-costs",
    "diagnostic-nuisance-effects",
    "auxiliary-clock-consistency",
    "joint-adjusted-anchor-sensitivity",
)


class MicrobenchmarkModelError(ValueError):
    """表示输入或实验设计不足以支持无歧义的权重估计。"""


@dataclass(frozen=True, order=True)
class _InstructionKey:
    """不会把不同原始编码、aq/rl 或 CSR 访问静默合并的稳定键。"""

    mnemonic: str
    size: int
    encoding_key: str
    encoding_hex: str
    aq: bool
    rl: bool
    csr: int | None
    pattern: str

    def public(self) -> dict[str, Any]:
        return {
            "mnemonic": self.mnemonic,
            "size": self.size,
            "encoding_key": f"raw:{self.size}:{self.encoding_hex}",
            "semantic_encoding_key": self.encoding_key,
            "bytes": self.encoding_hex,
            "aq": self.aq,
            "rl": self.rl,
            "csr": self.csr,
            "pattern": self.pattern,
        }


@dataclass(frozen=True)
class _ControlReference:
    mnemonic: str
    size: int
    encoding_key: str | None
    encoding_hex: str | None
    aq: bool
    rl: bool
    csr: int | None
    pattern: str | None


@dataclass(frozen=True)
class _Sample:
    run: str
    run_order: int | None
    super_run: str
    super_run_order: int | None
    crossover_pair: int | None
    crossover_design: str | None
    timing_launch_position: int | None
    plugin_off_launch_position: int | None
    anchor_position: str | None
    block: str
    pair: str
    sequence: int
    role: str
    mnemonic: str
    size: int
    encoding_key: str
    encoding_hex: str
    aq: bool
    rl: bool
    csr: int | None
    pattern: str
    batch: int
    plugin_cpu_ns: float | None
    guest_ns: float | None
    plugin_off_guest_ns: float | None
    target_count: int
    total_count: int | None
    paired_purity: float | None
    timer_reads: int
    plugin_mode: str | None
    translations_during_window: int | None
    control_mnemonic: str | None
    control_size: int | None
    control_encoding_key: str | None
    control_encoding_hex: str | None
    control_aq: bool
    control_rl: bool
    control_csr: int | None
    control_pattern: str | None
    empty_control_declared: bool


@dataclass(frozen=True)
class _Pair:
    run: str
    run_order: int
    run_order_source: str
    super_run: str
    super_run_order: int
    crossover_pair: int | None
    crossover_design: str | None
    timing_launch_position: int | None
    plugin_off_launch_position: int | None
    anchor_position: str | None
    block: str
    pair: str
    sequence: float
    key: _InstructionKey
    batch: int
    order: float
    plugin_delta_ns: float | None
    guest_delta_ns: float | None
    plugin_off_guest_delta_ns: float | None
    cross_clock_difference_ns: float | None
    plugin_off_difference_ns: float | None
    target_count: int
    purity: float | None
    timer_matched: bool
    marker_only_timing: bool
    translation_observed: bool
    translation_free: bool
    translation_delta: int | None
    control_reference: _ControlReference | None


@dataclass
class _Fit:
    estimate: float
    standard_error: float | None
    order_effect: float | None
    drift_effect: float | None
    batch_effect: float | None
    batch_reference: int | None
    batch_levels: tuple[int, ...]
    batch_level_effects: dict[int, float]
    batch_peak_to_peak: float
    translation_effect: float | None
    batch_log_range: float
    residuals: list[float]
    robust_weights: list[float]
    hetero_weights: list[float]
    pairs: list[_Pair]
    run_level_estimates: dict[str, float]
    predictor_names: list[str]
    irls_converged: bool
    irls_iterations: int
    irls_cycle_damping_used: bool
    design_condition_number: float


@dataclass(frozen=True)
class _BootstrapState:
    pairs: tuple[_Pair, ...]
    keys: tuple[_InstructionKey, ...]
    response_names: Mapping[_InstructionKey, str]
    controls: Mapping[_InstructionKey, _InstructionKey | None]
    batch_levels: Mapping[_InstructionKey, tuple[int, ...]]
    batch_references: Mapping[_InstructionKey, int | None]
    block_length: int
    run_block_length: int
    linear_algebra_backend: str


_BOOTSTRAP_STATE: _BootstrapState | None = None
_ACTIVE_LINEAR_ALGEBRA_BACKEND = "python"
_NUMPY: Any | None = None


def _default_cli_jobs() -> int:
    """给正式 bootstrap 选择有上限的进程数。"""

    return max(1, min(16, os.cpu_count() or 1))


def publication_generation_configuration() -> dict[str, Any]:
    """返回不可由结果产物放宽的正式统计重放参数。"""

    return {
        "schema": GENERATION_CONFIGURATION_SCHEMA,
        "bootstrap_replicates": PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES,
        "confidence": PUBLICATION_CONFIDENCE,
        "seed": PUBLICATION_BOOTSTRAP_SEED,
        "block_length": None,
        "run_block_length": None,
        "minimum_pairs": PUBLICATION_MIN_PAIRS,
        "minimum_effective_pairs": PUBLICATION_MIN_EFFECTIVE_PAIRS,
        "minimum_independent_super_runs": PUBLICATION_MIN_SUPER_RUNS,
        "minimum_count_levels": PUBLICATION_MIN_COUNT_LEVELS,
        "minimum_instruction_purity": PUBLICATION_MIN_PURITY,
        "maximum_relative_simultaneous_ci_half_width": (
            PUBLICATION_MAX_RELATIVE_CI_HALF_WIDTH
        ),
        "maximum_i_squared": PUBLICATION_MAX_I_SQUARED,
        "effect_equivalence_margin": PUBLICATION_EQUIVALENCE_MARGIN,
        "cross_clock_ratio_range": [
            PUBLICATION_MIN_CROSS_CLOCK_RATIO,
            PUBLICATION_MAX_CROSS_CLOCK_RATIO,
        ],
        "plugin_off_ratio_range": [
            PUBLICATION_MIN_PLUGIN_OFF_RATIO,
            PUBLICATION_MAX_PLUGIN_OFF_RATIO,
        ],
        "maximum_zero_cost_simultaneous_ci_upper_ns": (
            PUBLICATION_MAX_ZERO_COST_CI_UPPER_NS
        ),
        "maximum_translation_events_per_target_instruction": (
            PUBLICATION_MAX_TRANSLATION_DENSITY
        ),
        "maximum_translation_excluded_pair_fraction": (
            MAX_TRANSLATION_EXCLUDED_PAIR_FRACTION
        ),
        "maximum_severe_outlier_fraction": (
            PUBLICATION_MAX_SEVERE_OUTLIER_FRACTION
        ),
        "linear_algebra_backend": "numpy",
    }


def _numpy_module() -> Any:
    """延迟加载可选 NumPy 后端，避免纯 Python 用户被强制依赖。"""

    global _NUMPY
    if _NUMPY is None:
        # Bootstrap 已经在进程级并行；每个小矩阵再启动一组 BLAS 线程只会
        # 造成过度订阅。调用者仍可通过预先设置环境变量覆盖这些默认值。
        for variable in (
            "OPENBLAS_NUM_THREADS",
            "OMP_NUM_THREADS",
            "MKL_NUM_THREADS",
            "BLIS_NUM_THREADS",
            "NUMEXPR_NUM_THREADS",
        ):
            os.environ.setdefault(variable, "1")
        try:
            import numpy
        except ImportError as error:
            raise MicrobenchmarkModelError(
                "NumPy 线性代数后端不可用；请在 venv 中安装锁定依赖"
            ) from error
        _NUMPY = numpy
    return _NUMPY


def _linear_algebra_backend(name: str) -> str:
    if name not in {"python", "numpy", "auto"}:
        raise MicrobenchmarkModelError(
            "linear_algebra_backend 必须是 python、numpy 或 auto"
        )
    if name == "auto":
        try:
            _numpy_module()
        except MicrobenchmarkModelError:
            return "python"
        return "numpy"
    if name == "numpy":
        _numpy_module()
    return name


def _field(row: Any, names: Sequence[str], default: Any = None) -> Any:
    for name in names:
        if isinstance(row, Mapping) and name in row:
            return row[name]
        if not isinstance(row, Mapping) and hasattr(row, name):
            return getattr(row, name)
    return default


def _identifier(value: Any, owner: str) -> str:
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise MicrobenchmarkModelError(f"{owner} 必须是字符串或整数")
    result = str(value).strip()
    if not result:
        raise MicrobenchmarkModelError(f"{owner} 不能为空")
    return result


def _finite(value: Any, owner: str, *, minimum: float = 0.0) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise MicrobenchmarkModelError(f"{owner} 必须是有限数")
    result = float(value)
    if not math.isfinite(result) or result < minimum:
        raise MicrobenchmarkModelError(f"{owner} 必须是大于等于 {minimum:g} 的有限数")
    return result


def _integer(value: Any, owner: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise MicrobenchmarkModelError(f"{owner} 必须是大于等于 {minimum} 的整数")
    return value


def _normalise_mnemonic(value: Any, owner: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise MicrobenchmarkModelError(f"{owner} 必须是非空字符串")
    return value.strip().lower().split(maxsplit=1)[0]


def _semantic_mnemonic(mnemonic: str) -> str:
    return mnemonic[2:] if mnemonic.startswith("c.") else mnemonic


def _optional_bool(value: Any, owner: str) -> bool | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return value
    if isinstance(value, int) and value in {0, 1}:
        return bool(value)
    if isinstance(value, str):
        token = value.strip().lower()
        if token in {"0", "false", "no"}:
            return False
        if token in {"1", "true", "yes"}:
            return True
    raise MicrobenchmarkModelError(f"{owner} 必须是布尔值")


def _normalise_encoding(value: Any, size: int, owner: str) -> str:
    """把整数、byte list 或常见十六进制文本规范化为内存字节序 hex。"""

    if value is None or value == "":
        return "unknown"
    if isinstance(value, bool):
        raise MicrobenchmarkModelError(f"{owner} 不是合法指令编码")
    if isinstance(value, int):
        if value < 0 or value >= 1 << (size * 8):
            raise MicrobenchmarkModelError(f"{owner} 超出 {size} 字节范围")
        return value.to_bytes(size, "little").hex()
    if isinstance(value, (list, tuple)):
        if len(value) != size or any(
            isinstance(item, bool) or not isinstance(item, int) or not 0 <= item <= 255
            for item in value
        ):
            raise MicrobenchmarkModelError(f"{owner} 必须包含恰好 {size} 个字节")
        return bytes(value).hex()
    if not isinstance(value, str):
        raise MicrobenchmarkModelError(f"{owner} 不是合法指令编码")
    token = value.strip().lower()
    if token in {"unknown", "unavailable", "none"}:
        return "unknown"
    token = token.removeprefix("0x")
    for separator in (" ", ":", "_", "-"):
        token = token.replace(separator, "")
    if len(token) != size * 2 or any(character not in "0123456789abcdef" for character in token):
        raise MicrobenchmarkModelError(f"{owner} 必须是恰好 {size} 字节的十六进制编码")
    return token


def _encoding_word(encoding_hex: str) -> int | None:
    if encoding_hex == "unknown":
        return None
    return int.from_bytes(bytes.fromhex(encoding_hex), "little")


def _aq_rl(
    row: Any, mnemonic: str, encoding_hex: str, owner: str
) -> tuple[bool, bool]:
    explicit_aq = _optional_bool(_field(row, ("aq", "acquire")), f"{owner}.aq")
    explicit_rl = _optional_bool(_field(row, ("rl", "release")), f"{owner}.rl")
    parts = set(mnemonic.replace(".aq.rl", ".aqrl").split("."))
    name_aq = "aq" in parts or "aqrl" in parts
    name_rl = "rl" in parts or "aqrl" in parts
    word = _encoding_word(encoding_hex)
    encoding_aq = bool((word >> 26) & 1) if word is not None and word & 0x7F == 0x2F else None
    encoding_rl = bool((word >> 25) & 1) if word is not None and word & 0x7F == 0x2F else None
    aq = explicit_aq if explicit_aq is not None else encoding_aq if encoding_aq is not None else name_aq
    rl = explicit_rl if explicit_rl is not None else encoding_rl if encoding_rl is not None else name_rl
    if encoding_aq is not None and explicit_aq is not None and encoding_aq != explicit_aq:
        raise MicrobenchmarkModelError(f"{owner}.aq 与原始编码不一致")
    if encoding_rl is not None and explicit_rl is not None and encoding_rl != explicit_rl:
        raise MicrobenchmarkModelError(f"{owner}.rl 与原始编码不一致")
    return bool(aq), bool(rl)


def _csr_number(row: Any, mnemonic: str, encoding_hex: str, owner: str) -> int | None:
    raw = _field(row, ("csr", "csr_number", "csr_index"))
    explicit: int | None = None
    if raw is not None and raw != "":
        if isinstance(raw, str):
            try:
                explicit = int(raw, 0)
            except ValueError as error:
                raise MicrobenchmarkModelError(f"{owner}.csr 不是合法编号") from error
        else:
            explicit = _integer(raw, f"{owner}.csr")
        if not 0 <= explicit <= 0xFFF:
            raise MicrobenchmarkModelError(f"{owner}.csr 必须位于 0..0xfff")
    word = _encoding_word(encoding_hex)
    encoded: int | None = None
    if word is not None and word & 0x7F == 0x73 and ((word >> 12) & 0x7) != 0:
        encoded = (word >> 20) & 0xFFF
    if explicit is not None and encoded is not None and explicit != encoded:
        raise MicrobenchmarkModelError(f"{owner}.csr 与原始编码不一致")
    csr_like = mnemonic.startswith("csr") or mnemonic.startswith("csrr")
    return explicit if explicit is not None else encoded if csr_like else None


def _instruction_key(row: Any, mnemonic: str, size: int, pattern: str, owner: str) -> _InstructionKey:
    descriptor = _field(row, ("target_descriptor", "instruction_descriptor"))
    source = descriptor if isinstance(descriptor, Mapping) else row
    encoding = _normalise_encoding(
        _field(
            source,
            ("encoding_hex", "instruction_bytes", "encoding", "bytes", "raw_bytes"),
        ),
        size,
        f"{owner}.encoding",
    )
    aq, rl = _aq_rl(source, mnemonic, encoding, owner)
    csr = _csr_number(source, mnemonic, encoding, owner)
    raw_encoding_key = _field(source, ("encoding_key", "canonical_encoding_key"))
    if raw_encoding_key is None:
        encoding_key = f"raw:{size}:{encoding}"
    elif not isinstance(raw_encoding_key, str) or not raw_encoding_key.strip():
        raise MicrobenchmarkModelError(f"{owner}.encoding_key 必须是非空字符串")
    else:
        encoding_key = raw_encoding_key.strip().lower()
    return _InstructionKey(
        mnemonic, size, encoding_key, encoding, aq, rl, csr, pattern
    )


def _exact_count(
    row: Any, mnemonic: str, size: int, owner: str
) -> tuple[int, int | None]:
    raw_target = _field(row, ("target_count", "exact_target_count"))
    raw_counts = _field(row, ("exact_counts", "instruction_counts"))
    total: int | None = None
    if raw_counts is not None:
        if not isinstance(raw_counts, Mapping):
            raise MicrobenchmarkModelError(f"{owner}.exact_counts 必须是 mapping")
        parsed: dict[str, int] = {}
        for raw_name, raw_value in raw_counts.items():
            if not isinstance(raw_name, str) or not raw_name:
                raise MicrobenchmarkModelError(f"{owner}.exact_counts 包含非法键")
            parsed[raw_name.strip().lower()] = _integer(
                raw_value, f"{owner}.exact_counts[{raw_name!r}]"
            )
        total = sum(parsed.values())
        if raw_target is None:
            candidates = (
                f"{mnemonic}|{size}",
                f"{mnemonic}:{size}",
                f"{mnemonic}/{size}",
                f"{mnemonic}@{size}",
                f"{mnemonic}#{size}",
                mnemonic,
            )
            found = [parsed[name] for name in candidates if name in parsed]
            if len(found) > 1 and len(set(found)) != 1:
                raise MicrobenchmarkModelError(
                    f"{owner}.exact_counts 对目标指令给出冲突计数"
                )
            raw_target = found[0] if found else None
    raw_total = _field(
        row,
        ("total_instruction_count", "total_count", "qemu_instruction_count"),
    )
    if raw_total is not None:
        explicit_total = _integer(raw_total, f"{owner}.total_instruction_count")
        if total is not None and explicit_total != total:
            raise MicrobenchmarkModelError(
                f"{owner} 的 total_instruction_count 与 exact_counts 总和不一致"
            )
        total = explicit_total
    if raw_target is None:
        raise MicrobenchmarkModelError(f"{owner} 缺少 QEMU 精确 target_count")
    target = _integer(raw_target, f"{owner}.target_count")
    if total is not None and target > total:
        raise MicrobenchmarkModelError(f"{owner}.target_count 不能大于总指令数")
    return target, total


def _sample_sequence(row: Any, owner: str, role: str) -> int:
    raw = _field(row, ("sequence", "window_sequence", "window_id"))
    if raw is not None:
        return _integer(raw, f"{owner}.sequence")
    order = _field(row, ("order",))
    if isinstance(order, int) and not isinstance(order, bool):
        return _integer(order, f"{owner}.order")
    if isinstance(order, str):
        token = order.strip().lower().replace("_", "-")
        if token in {"ab", "baseline-first", "control-first"}:
            return 0 if role == "baseline" else 1
        if token in {"ba", "probe-first", "target-first"}:
            return 0 if role == "probe" else 1
    raise MicrobenchmarkModelError(
        f"{owner} 缺少数值 sequence，或可识别的 AB/BA order"
    )


def _parse_sample(row: Any, index: int) -> _Sample:
    owner = f"samples[{index}]"
    role_raw = _field(row, ("role", "case_role"))
    if not isinstance(role_raw, str):
        raise MicrobenchmarkModelError(f"{owner}.role 必须是 probe 或 baseline")
    role = role_raw.strip().lower()
    aliases = {"target": "probe", "control": "baseline"}
    role = aliases.get(role, role)
    if role not in {"probe", "baseline"}:
        raise MicrobenchmarkModelError(f"{owner}.role 必须是 probe 或 baseline")

    mnemonic = _normalise_mnemonic(
        _field(row, ("instruction", "mnemonic", "case")), f"{owner}.instruction"
    )
    size = _integer(
        _field(row, ("encoding_bytes", "size", "instruction_size")),
        f"{owner}.encoding_bytes",
        minimum=1,
    )
    if size not in {2, 4}:
        raise MicrobenchmarkModelError(f"{owner}.encoding_bytes 只允许 2 或 4")
    pattern_raw = _field(row, ("pattern", "dependency_pattern"), "throughput")
    if not isinstance(pattern_raw, str) or not pattern_raw.strip():
        raise MicrobenchmarkModelError(f"{owner}.pattern 必须是非空字符串")
    pattern = pattern_raw.strip().lower()
    instruction_key = _instruction_key(row, mnemonic, size, pattern, owner)
    run = _identifier(_field(row, ("run_id", "run")), f"{owner}.run_id")
    run_order_raw = _field(
        row, ("run_order", "acquisition_order", "run_index")
    )
    run_order = (
        None
        if run_order_raw is None
        else _integer(run_order_raw, f"{owner}.run_order", minimum=0)
    )
    super_run_raw = _field(row, ("super_run_id", "cluster_id"), run)
    super_run = _identifier(super_run_raw, f"{owner}.super_run_id")
    super_run_order_raw = _field(row, ("super_run_order", "cluster_order"))
    super_run_order = (
        run_order
        if super_run_order_raw is None
        else _integer(
            super_run_order_raw, f"{owner}.super_run_order", minimum=0
        )
    )
    crossover_pair_raw = _field(row, ("crossover_pair",))
    crossover_pair = (
        None
        if crossover_pair_raw is None
        else _integer(crossover_pair_raw, f"{owner}.crossover_pair", minimum=1)
    )
    crossover_design_raw = _field(row, ("crossover_design",))
    if crossover_design_raw is None:
        crossover_design = None
    elif not isinstance(crossover_design_raw, str) or not crossover_design_raw.strip():
        raise MicrobenchmarkModelError(
            f"{owner}.crossover_design 必须是非空字符串"
        )
    else:
        crossover_design = crossover_design_raw.strip().upper()
    timing_launch_position_raw = _field(row, ("timing_launch_position",))
    timing_launch_position = (
        None
        if timing_launch_position_raw is None
        else _integer(
            timing_launch_position_raw,
            f"{owner}.timing_launch_position",
            minimum=1,
        )
    )
    plugin_off_launch_position_raw = _field(
        row, ("plugin_off_launch_position",)
    )
    plugin_off_launch_position = (
        None
        if plugin_off_launch_position_raw is None
        else _integer(
            plugin_off_launch_position_raw,
            f"{owner}.plugin_off_launch_position",
            minimum=1,
        )
    )
    anchor_position_raw = _field(row, ("anchor_position",))
    if anchor_position_raw is None:
        anchor_position = None
    elif not isinstance(anchor_position_raw, str) or anchor_position_raw not in {
        "head",
        "body",
        "tail",
        "not-anchor",
    }:
        raise MicrobenchmarkModelError(f"{owner}.anchor_position 非法")
    else:
        anchor_position = anchor_position_raw
    block = _identifier(
        _field(row, ("block_id", "block", "round")), f"{owner}.block_id"
    )
    batch = _integer(
        _field(row, ("requested_count", "batch", "count")),
        f"{owner}.batch",
        minimum=1,
    )
    raw_pair = _field(row, ("pair_id", "segment_id", "segment"))
    if raw_pair is None:
        raw_pair = f"{mnemonic}|{size}|{pattern}|{batch}|{block}"
    pair = _identifier(raw_pair, f"{owner}.pair_id")
    sequence = _sample_sequence(row, owner, role)

    plugin_raw = _field(
        row,
        (
            "plugin_thread_cpu_ns",
            "vcpu_thread_cpu_ns",
            "vcpu_task_clock_ns",
            "plugin_cpu_ns",
        ),
    )
    guest_raw = _field(row, ("guest_ns", "elapsed_ns", "guest_elapsed_ns"))
    plugin_off_guest_raw = _field(
        row, ("plugin_off_guest_ns", "uninstrumented_guest_ns")
    )
    plugin = None if plugin_raw is None else _finite(plugin_raw, f"{owner}.plugin_thread_cpu_ns")
    guest = None if guest_raw is None else _finite(guest_raw, f"{owner}.guest_ns")
    plugin_off_guest = (
        None
        if plugin_off_guest_raw is None
        else _finite(plugin_off_guest_raw, f"{owner}.plugin_off_guest_ns")
    )
    if plugin is None and guest is None:
        raise MicrobenchmarkModelError(f"{owner} 至少需要一种计时响应")
    target, total = _exact_count(row, mnemonic, size, owner)
    raw_paired_purity = _field(
        row, ("paired_contrast_purity", "pair_purity", "contrast_purity")
    )
    paired_purity = (
        None
        if raw_paired_purity is None
        else _finite(raw_paired_purity, f"{owner}.paired_contrast_purity")
    )
    if paired_purity is not None and paired_purity > 1.0:
        raise MicrobenchmarkModelError(
            f"{owner}.paired_contrast_purity 不能大于 1"
        )
    timer_reads = _integer(
        _field(row, ("timer_reads",), 2), f"{owner}.timer_reads"
    )
    plugin_mode_raw = _field(row, ("plugin_mode", "measurement_mode"))
    if plugin_mode_raw is None:
        plugin_mode = None
    elif not isinstance(plugin_mode_raw, str) or not plugin_mode_raw.strip():
        raise MicrobenchmarkModelError(f"{owner}.plugin_mode 必须是非空字符串")
    else:
        plugin_mode = plugin_mode_raw.strip().lower()
    translations_raw = _field(
        row, ("translations_during_window", "translation_count")
    )
    translations_during_window = (
        None
        if translations_raw is None
        else _integer(
            translations_raw, f"{owner}.translations_during_window"
        )
    )

    baseline_descriptor = _field(row, ("baseline_descriptor", "control_descriptor"))
    baseline_source = baseline_descriptor if isinstance(baseline_descriptor, Mapping) else row
    control_raw = (
        _field(baseline_source, ("mnemonic", "instruction"))
        if isinstance(baseline_descriptor, Mapping)
        else _field(row, ("baseline_instruction",))
    )
    if control_raw is None:
        fallback_control = _field(row, ("control_instruction",))
        if not (
            isinstance(fallback_control, str)
            and fallback_control.strip().lower() in {"empty-call", "empty", "none"}
        ):
            control_raw = fallback_control
    baseline_kind = _field(row, ("baseline_kind", "control_kind"))
    control_mnemonic: str | None
    control_size: int | None
    empty_tokens = {"empty", "empty-call", "loop-only", "anchor-only", "none"}
    empty_control_declared = (
        (isinstance(baseline_kind, str) and baseline_kind.strip().lower() in empty_tokens)
        or (isinstance(control_raw, str) and control_raw.strip().lower() in empty_tokens)
        or _field(row, ("baseline_encoding_bytes",)) == 0
    )
    if empty_control_declared:
        control_mnemonic = None
        control_size = None
        control_encoding_key = None
        control_encoding_hex = None
        control_aq = False
        control_rl = False
        control_csr = None
        control_pattern = None
    elif control_raw is None:
        # 旧探针没有 control 字段时只能把结果解释为相对 empty 的斜率；
        # 输出会明确标记该假设，且高置信门禁不会通过。
        control_mnemonic = None
        control_size = None
        control_encoding_key = None
        control_encoding_hex = None
        control_aq = False
        control_rl = False
        control_csr = None
        control_pattern = None
    else:
        control_mnemonic = _normalise_mnemonic(
            control_raw, f"{owner}.control_instruction"
        )
        control_size = _integer(
            (
                _field(baseline_source, ("size", "encoding_bytes"), size)
                if isinstance(baseline_descriptor, Mapping)
                else _field(
                    row,
                    ("control_encoding_bytes", "baseline_encoding_bytes"),
                    size,
                )
            ),
            f"{owner}.control_encoding_bytes",
            minimum=1,
        )
        if control_size not in {2, 4}:
            raise MicrobenchmarkModelError(
                f"{owner}.control_encoding_bytes 只允许 2 或 4"
            )
        control_encoding_raw = (
            _field(
                baseline_source,
                ("encoding_hex", "instruction_bytes", "encoding", "bytes", "raw_bytes"),
            )
            if isinstance(baseline_descriptor, Mapping)
            else _field(
                row,
                (
                    "baseline_encoding_hex",
                    "control_encoding_hex",
                    "baseline_instruction_bytes",
                    "control_instruction_bytes",
                ),
            )
        )
        control_encoding_hex = _normalise_encoding(
            control_encoding_raw,
            control_size,
            f"{owner}.control_encoding",
        )
        control_encoding_hex = (
            None if control_encoding_hex == "unknown" else control_encoding_hex
        )
        raw_control_encoding_key = _field(
            baseline_source, ("encoding_key", "canonical_encoding_key")
        )
        if raw_control_encoding_key is None:
            control_encoding_key = (
                None
                if control_encoding_hex is None
                else f"raw:{control_size}:{control_encoding_hex}"
            )
        elif not isinstance(raw_control_encoding_key, str) or not raw_control_encoding_key.strip():
            raise MicrobenchmarkModelError(
                f"{owner}.control_encoding_key 必须是非空字符串"
            )
        else:
            control_encoding_key = raw_control_encoding_key.strip().lower()
        control_meta = dict(baseline_source) if isinstance(baseline_source, Mapping) else {}
        for source_name, target_name in (
            ("control_aq", "aq"),
            ("baseline_aq", "aq"),
            ("control_rl", "rl"),
            ("baseline_rl", "rl"),
            ("control_csr", "csr"),
            ("baseline_csr", "csr"),
        ):
            value = _field(row, (source_name,))
            if value is not None:
                control_meta[target_name] = value
        control_aq, control_rl = _aq_rl(
            control_meta,
            control_mnemonic,
            control_encoding_hex or "unknown",
            f"{owner}.control",
        )
        control_csr = _csr_number(
            control_meta,
            control_mnemonic,
            control_encoding_hex or "unknown",
            f"{owner}.control",
        )
        raw_control_pattern = _field(row, ("control_pattern", "baseline_pattern"))
        control_pattern = (
            raw_control_pattern.strip().lower()
            if isinstance(raw_control_pattern, str) and raw_control_pattern.strip()
            else None
        )
    if super_run_order is None:
        # 没有显式层级的旧输入由 _pair_samples 在恢复 run 时间轴后补齐。
        super_run_order = -1
    return _Sample(
        run,
        run_order,
        super_run,
        super_run_order,
        crossover_pair,
        crossover_design,
        timing_launch_position,
        plugin_off_launch_position,
        anchor_position,
        block,
        pair,
        sequence,
        role,
        mnemonic,
        size,
        instruction_key.encoding_key,
        instruction_key.encoding_hex,
        instruction_key.aq,
        instruction_key.rl,
        instruction_key.csr,
        pattern,
        batch,
        plugin,
        guest,
        plugin_off_guest,
        target,
        total,
        paired_purity,
        timer_reads,
        plugin_mode,
        translations_during_window,
        control_mnemonic,
        control_size,
        control_encoding_key,
        control_encoding_hex,
        control_aq,
        control_rl,
        control_csr,
        control_pattern,
        empty_control_declared,
    )


def _pair_samples(rows: Sequence[Any]) -> tuple[list[_Pair], set[_InstructionKey]]:
    if not rows:
        raise MicrobenchmarkModelError("samples 不能为空")
    samples = [_parse_sample(row, index) for index, row in enumerate(rows)]
    first_seen_runs = list(dict.fromkeys(sample.run for sample in samples))
    explicit_orders: dict[str, int] = {}
    for run in first_seen_runs:
        values = {
            sample.run_order
            for sample in samples
            if sample.run == run and sample.run_order is not None
        }
        if len(values) > 1:
            raise MicrobenchmarkModelError(
                f"run={run!r} 的 run_order 在同一 run 内不一致"
            )
        if values:
            explicit_orders[run] = next(iter(values))
    if explicit_orders and len(explicit_orders) != len(first_seen_runs):
        missing = [run for run in first_seen_runs if run not in explicit_orders]
        raise MicrobenchmarkModelError(
            f"run_order 必须覆盖所有 run，缺少 {missing!r}"
        )
    if explicit_orders:
        if len(set(explicit_orders.values())) != len(explicit_orders):
            raise MicrobenchmarkModelError("不同 run 不能复用同一 run_order")
        run_orders = explicit_orders
        run_order_source = "explicit-run-order"
    else:
        inferred: dict[str, int] = {}
        prefixes: set[str] = set()
        for run in first_seen_runs:
            match = re.fullmatch(r"(.*[-_])(\d+)", run)
            if match is None:
                inferred = {}
                break
            prefixes.add(match.group(1))
            inferred[run] = int(match.group(2), 10)
        expected = set(range(1, len(first_seen_runs) + 1))
        if len(prefixes) == 1 and set(inferred.values()) == expected:
            run_orders = inferred
            run_order_source = "strict-common-prefix-contiguous-suffix"
        else:
            run_orders = {
                run: position for position, run in enumerate(first_seen_runs)
            }
            run_order_source = "input-first-appearance"
    super_run_orders: dict[str, int] = {}
    super_run_designs: dict[str, str | None] = {}
    super_run_pairs: dict[str, set[int]] = defaultdict(set)
    for sample in samples:
        effective_super_order = (
            run_orders[sample.run]
            if sample.super_run_order < 0
            else sample.super_run_order
        )
        previous_order = super_run_orders.setdefault(
            sample.super_run, effective_super_order
        )
        previous_design = super_run_designs.setdefault(
            sample.super_run, sample.crossover_design
        )
        if previous_order != effective_super_order:
            raise MicrobenchmarkModelError("同一 super-run 的 order 不一致")
        if previous_design != sample.crossover_design:
            raise MicrobenchmarkModelError("同一 super-run 的 crossover design 不一致")
        if sample.crossover_pair is not None:
            super_run_pairs[sample.super_run].add(sample.crossover_pair)
    if len(set(super_run_orders.values())) != len(super_run_orders):
        raise MicrobenchmarkModelError("不同 super-run 不能复用 super_run_order")
    for super_run, design in super_run_designs.items():
        members = [sample for sample in samples if sample.super_run == super_run]
        has_crossover_metadata = any(
            value is not None
            for sample in members
            for value in (
                sample.crossover_pair,
                sample.crossover_design,
                sample.timing_launch_position,
                sample.plugin_off_launch_position,
            )
        )
        if not has_crossover_metadata:
            continue
        if design not in {"ABBA", "BAAB"} or super_run_pairs[super_run] != {1, 2}:
            raise MicrobenchmarkModelError(
                f"super-run={super_run!r} 的 crossover 设计不完整"
            )
        by_pair: dict[int, tuple[int, int]] = {}
        for sample in members:
            if (
                sample.crossover_pair not in {1, 2}
                or sample.timing_launch_position is None
                or sample.plugin_off_launch_position is None
            ):
                raise MicrobenchmarkModelError(
                    f"super-run={super_run!r} 的 crossover 启动位置不完整"
                )
            launch_pair = (
                sample.timing_launch_position,
                sample.plugin_off_launch_position,
            )
            previous = by_pair.setdefault(sample.crossover_pair, launch_pair)
            if previous != launch_pair:
                raise MicrobenchmarkModelError(
                    f"super-run={super_run!r} 的 crossover pair 元数据不一致"
                )
        expected_pairs = (
            {1: (1, 2), 2: (4, 3)}
            if design == "ABBA"
            else {1: (2, 1), 2: (3, 4)}
        )
        expected_timing = {1, 4} if design == "ABBA" else {2, 3}
        timing_positions = {timing for timing, _off in by_pair.values()}
        off_positions = {off for _timing, off in by_pair.values()}
        all_positions = timing_positions | off_positions
        if (
            by_pair != expected_pairs
            or timing_positions != expected_timing
            or off_positions != ({1, 2, 3, 4} - expected_timing)
            or all_positions != {1, 2, 3, 4}
        ):
            raise MicrobenchmarkModelError(
                f"super-run={super_run!r} 的启动位置与 {design} 不一致"
            )
    grouped: dict[tuple[str, str], list[_Sample]] = defaultdict(list)
    for sample in samples:
        grouped[(sample.run, sample.pair)].append(sample)
    pairs: list[_Pair] = []
    assumed_empty: set[_InstructionKey] = set()
    for group_key, members in grouped.items():
        if len(members) != 2 or {member.role for member in members} != {
            "probe",
            "baseline",
        }:
            raise MicrobenchmarkModelError(
                f"run/pair={group_key!r} 必须恰好包含一个 probe 和一个 baseline"
            )
        probe = next(member for member in members if member.role == "probe")
        baseline = next(member for member in members if member.role == "baseline")
        comparable = (
            "mnemonic",
            "size",
            "encoding_key",
            "encoding_hex",
            "aq",
            "rl",
            "csr",
            "pattern",
            "batch",
            "block",
            "paired_purity",
            "control_mnemonic",
            "control_size",
            "control_encoding_key",
            "control_encoding_hex",
            "control_aq",
            "control_rl",
            "control_csr",
            "control_pattern",
            "empty_control_declared",
            "plugin_mode",
            "super_run",
            "super_run_order",
            "crossover_pair",
            "crossover_design",
            "timing_launch_position",
            "plugin_off_launch_position",
            "anchor_position",
        )
        for name in comparable:
            if getattr(probe, name) != getattr(baseline, name):
                raise MicrobenchmarkModelError(
                    f"run/pair={group_key!r} 的 {name} 在 probe/baseline 间不一致"
                )
        if probe.sequence == baseline.sequence:
            raise MicrobenchmarkModelError(
                f"run/pair={group_key!r} 的窗口 sequence 必须互异"
            )
        target_delta = probe.target_count - baseline.target_count
        if target_delta <= 0:
            raise MicrobenchmarkModelError(
                f"run/pair={group_key!r} 的 probe target_count 必须大于 baseline"
            )
        plugin_delta = (
            None
            if probe.plugin_cpu_ns is None or baseline.plugin_cpu_ns is None
            else probe.plugin_cpu_ns - baseline.plugin_cpu_ns
        )
        guest_delta = (
            None
            if probe.guest_ns is None or baseline.guest_ns is None
            else probe.guest_ns - baseline.guest_ns
        )
        plugin_off_guest_delta = (
            None
            if probe.plugin_off_guest_ns is None
            or baseline.plugin_off_guest_ns is None
            else probe.plugin_off_guest_ns - baseline.plugin_off_guest_ns
        )
        cross_clock_difference = (
            None
            if guest_delta is None or plugin_delta is None
            else guest_delta - plugin_delta
        )
        plugin_off_difference = (
            None
            if plugin_off_guest_delta is None or guest_delta is None
            else guest_delta - plugin_off_guest_delta
        )
        purity = probe.paired_purity
        if purity is None:
            purity = (
                None
                if probe.total_count is None or probe.total_count <= 0
                else probe.target_count / probe.total_count
            )
        key = _InstructionKey(
            probe.mnemonic,
            probe.size,
            probe.encoding_key,
            probe.encoding_hex,
            probe.aq,
            probe.rl,
            probe.csr,
            probe.pattern,
        )
        control_reference = (
            None
            if probe.control_mnemonic is None
            else _ControlReference(
                probe.control_mnemonic,
                int(probe.control_size),
                probe.control_encoding_key,
                probe.control_encoding_hex,
                probe.control_aq,
                probe.control_rl,
                probe.control_csr,
                probe.control_pattern,
            )
        )
        # 缺少 baseline_kind/control_instruction 时仍可估计 contrast，但不能证明
        # 它是绝对耗时。这里记录全局假设供质量门禁使用。
        if probe.control_mnemonic is None and not probe.empty_control_declared:
            assumed_empty.add(key)
        pairs.append(
            _Pair(
                run=probe.run,
                run_order=run_orders[probe.run],
                run_order_source=run_order_source,
                super_run=probe.super_run,
                super_run_order=super_run_orders[probe.super_run],
                crossover_pair=probe.crossover_pair,
                crossover_design=probe.crossover_design,
                timing_launch_position=probe.timing_launch_position,
                plugin_off_launch_position=probe.plugin_off_launch_position,
                anchor_position=probe.anchor_position,
                block=probe.block,
                pair=probe.pair,
                sequence=(probe.sequence + baseline.sequence) / 2.0,
                key=key,
                batch=probe.batch,
                order=0.5 if probe.sequence > baseline.sequence else -0.5,
                plugin_delta_ns=plugin_delta,
                guest_delta_ns=guest_delta,
                plugin_off_guest_delta_ns=plugin_off_guest_delta,
                cross_clock_difference_ns=cross_clock_difference,
                plugin_off_difference_ns=plugin_off_difference,
                target_count=target_delta,
                purity=purity,
                timer_matched=probe.timer_reads == baseline.timer_reads,
                marker_only_timing=(
                    probe.plugin_mode in {"timing", "marker-only-timing"}
                    and baseline.plugin_mode
                    in {"timing", "marker-only-timing"}
                ),
                translation_observed=(
                    probe.translations_during_window is not None
                    and baseline.translations_during_window is not None
                ),
                translation_free=(
                    probe.translations_during_window == 0
                    and baseline.translations_during_window == 0
                ),
                translation_delta=(
                    None
                    if probe.translations_during_window is None
                    or baseline.translations_during_window is None
                    else probe.translations_during_window
                    - baseline.translations_during_window
                ),
                control_reference=control_reference,
            )
        )
    return pairs, assumed_empty


def _median_absolute_deviation(values: Sequence[float]) -> float:
    if not values:
        return 0.0
    center = statistics.median(values)
    return statistics.median(abs(value - center) for value in values)


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


def _json_finite(value: float | None) -> float | None:
    return value if value is not None and math.isfinite(value) else None


def _ordered_runs(pairs: Iterable[_Pair]) -> list[str]:
    """按显式或输入派生的采集序号返回 run，且验证序号一致。"""

    by_run: dict[str, list[int]] = defaultdict(list)
    for pair in pairs:
        by_run[pair.run].append(pair.run_order)
    for run, orders in by_run.items():
        if len(set(orders)) != 1:
            raise MicrobenchmarkModelError(
                f"run={run!r} 的内部 run_order 不一致"
            )
    return sorted(
        by_run,
        key=lambda run: (by_run[run][0], run),
    )


def _ordered_super_runs(pairs: Iterable[_Pair]) -> list[str]:
    """返回最高独立层级；旧样本自然退化为一 run 一 cluster。"""

    orders: dict[str, set[int]] = defaultdict(set)
    for pair in pairs:
        orders[pair.super_run].add(pair.super_run_order)
    for super_run, values in orders.items():
        if len(values) != 1:
            raise MicrobenchmarkModelError(
                f"super-run={super_run!r} 的内部 order 不一致"
            )
    if len({next(iter(values)) for values in orders.values()}) != len(orders):
        raise MicrobenchmarkModelError("不同 super-run 不能复用 order")
    return sorted(
        orders,
        key=lambda super_run: (next(iter(orders[super_run])), super_run),
    )


def _invert(matrix: Sequence[Sequence[float]]) -> list[list[float]]:
    size = len(matrix)
    if size == 0 or any(len(row) != size for row in matrix):
        raise MicrobenchmarkModelError("内部设计矩阵不是方阵")
    augmented = [
        [float(value) for value in row]
        + [1.0 if column == index else 0.0 for column in range(size)]
        for index, row in enumerate(matrix)
    ]
    for column in range(size):
        pivot = max(range(column, size), key=lambda row: abs(augmented[row][column]))
        scale = max(1.0, max(abs(value) for value in augmented[pivot][:size]))
        if abs(augmented[pivot][column]) <= 1e-12 * scale:
            raise MicrobenchmarkModelError("微基准设计矩阵秩不足")
        augmented[column], augmented[pivot] = augmented[pivot], augmented[column]
        divisor = augmented[column][column]
        augmented[column] = [value / divisor for value in augmented[column]]
        for row in range(size):
            if row == column:
                continue
            factor = augmented[row][column]
            if factor:
                augmented[row] = [
                    value - factor * pivot_value
                    for value, pivot_value in zip(augmented[row], augmented[column])
                ]
    return [row[size:] for row in augmented]


def _wls(
    matrix: Sequence[Sequence[float]],
    response: Sequence[float],
    weights: Sequence[float],
    *,
    sparse_rows: Sequence[Sequence[tuple[int, float]]] | None = None,
) -> tuple[list[float], list[list[float]]]:
    coefficients, inverse = _wls_native(
        matrix, response, weights, sparse_rows=sparse_rows
    )
    if _ACTIVE_LINEAR_ALGEBRA_BACKEND == "numpy":
        return coefficients.tolist(), inverse.tolist()
    return coefficients, inverse


def _wls_native(
    matrix: Any,
    response: Any,
    weights: Any,
    *,
    sparse_rows: Sequence[Sequence[tuple[int, float]]] | None = None,
    compute_inverse: bool = True,
) -> tuple[Any, Any]:
    if _ACTIVE_LINEAR_ALGEBRA_BACKEND == "numpy":
        np = _numpy_module()
        design = np.asarray(matrix, dtype=np.float64)
        values = np.asarray(response, dtype=np.float64)
        diagonal = np.asarray(weights, dtype=np.float64)
        gram = design.T @ (diagonal[:, None] * design)
        rhs = design.T @ (diagonal * values)
        width = design.shape[1]
        ridge = max(
            1e-14,
            float(np.trace(gram)) * 1e-13 / max(1, width),
        )
        gram = gram.copy()
        indices = np.arange(1, width)
        gram[indices, indices] += ridge
        try:
            if compute_inverse:
                inverse = np.linalg.inv(gram)
                coefficients = inverse @ rhs
            else:
                coefficients = np.linalg.solve(gram, rhs)
                inverse = None
        except np.linalg.LinAlgError as error:
            raise MicrobenchmarkModelError("微基准设计矩阵秩不足") from error
        return coefficients, inverse

    width = len(matrix[0])
    if sparse_rows is None:
        sparse_rows = [
            [(column, value) for column, value in enumerate(row) if value != 0.0]
            for row in matrix
        ]
    gram = [[0.0] * width for _ in range(width)]
    rhs = [0.0] * width
    for entries, value, weight in zip(sparse_rows, response, weights):
        for left_position, (left, left_value) in enumerate(entries):
            rhs[left] += weight * left_value * value
            for right_position in range(left_position + 1):
                right, right_value = entries[right_position]
                gram[left][right] += weight * left_value * right_value
    for left in range(width):
        for right in range(left):
            gram[right][left] = gram[left][right]
    trace = sum(gram[index][index] for index in range(width))
    ridge = max(1e-14, trace * 1e-13 / max(1, width))
    for index in range(1, width):
        gram[index][index] += ridge
    inverse = _invert(gram)
    coefficients = [
        math.fsum(inverse[row][column] * rhs[column] for column in range(width))
        for row in range(width)
    ]
    return coefficients, inverse


def _robust_fit(
    matrix: Sequence[Sequence[float]],
    response: Sequence[float],
    base_weights: Sequence[float],
    *,
    max_iterations: int = 120,
    huber_delta: float = 1.345,
    sparse_rows: Sequence[Sequence[tuple[int, float]]] | None = None,
    compute_inverse: bool = True,
) -> tuple[
    list[float],
    list[float],
    list[float],
    list[list[float]],
    bool,
    int,
    bool,
]:
    np = (
        _numpy_module()
        if _ACTIVE_LINEAR_ALGEBRA_BACKEND == "numpy"
        else None
    )
    native_matrix = (
        np.asarray(matrix, dtype=np.float64) if np is not None else matrix
    )
    native_response = (
        np.asarray(response, dtype=np.float64) if np is not None else response
    )
    native_base_weights = (
        np.asarray(base_weights, dtype=np.float64)
        if np is not None
        else base_weights
    )
    if sparse_rows is None and np is None:
        sparse_rows = [
            [(column, value) for column, value in enumerate(row) if value != 0.0]
            for row in matrix
        ]
    robust: Any = (
        np.ones(len(response), dtype=np.float64)
        if np is not None
        else [1.0] * len(response)
    )
    coefficients: list[float] = []
    inverse: list[list[float]] = []
    residuals: list[float] = []
    converged = False
    iterations = 0
    cycle_damping_used = False
    previous_robust: Any | None = None
    for iteration in range(1, max_iterations + 1):
        iterations = iteration
        combined = (
            native_base_weights * robust
            if np is not None
            else [base * weight for base, weight in zip(base_weights, robust)]
        )
        coefficients, inverse = _wls_native(
            native_matrix,
            native_response,
            combined,
            sparse_rows=sparse_rows,
            compute_inverse=compute_inverse,
        )
        if np is not None:
            native_residuals = native_response - native_matrix @ coefficients
            center = np.median(native_residuals)
            scale = max(
                1e-15,
                1.4826 * float(np.median(np.abs(native_residuals - center))),
            )
            cutoff = huber_delta * scale
            absolute = np.abs(native_residuals)
            fixed_point = np.ones_like(absolute)
            mask = absolute > cutoff
            fixed_point[mask] = cutoff / absolute[mask]
            change = float(np.sqrt(np.mean((fixed_point - robust) ** 2)))
            residuals = native_residuals.tolist()
        else:
            residuals = [
                value
                - math.fsum(
                    coefficient * item
                    for coefficient, item in zip(coefficients, row)
                )
                for row, value in zip(matrix, response)
            ]
            scale = max(
                1e-15, 1.4826 * _median_absolute_deviation(residuals)
            )
            cutoff = huber_delta * scale
            fixed_point = [
                1.0 if abs(value) <= cutoff else cutoff / abs(value)
                for value in residuals
            ]
            change = math.sqrt(
                math.fsum(
                    (new - old) ** 2
                    for new, old in zip(fixed_point, robust)
                )
                / len(robust)
            )
        if (
            not cycle_damping_used
            and iteration >= 8
            and previous_robust is not None
        ):
            cycle_distance = (
                float(
                    np.sqrt(
                        np.mean((fixed_point - previous_robust) ** 2)
                    )
                )
                if np is not None
                else math.sqrt(
                    math.fsum(
                        (new - old) ** 2
                        for new, old in zip(fixed_point, previous_robust)
                    )
                    / len(robust)
                )
            )
            if cycle_distance <= max(1e-12, 0.05 * change):
                cycle_damping_used = True
        old_robust = robust
        robust = (
            robust + 0.75 * (fixed_point - robust)
            if np is not None and cycle_damping_used
            else [
                old + 0.75 * (new - old)
                for old, new in zip(robust, fixed_point)
            ]
            if cycle_damping_used
            else fixed_point
        )
        if change <= 1e-6:
            converged = True
            break
        previous_robust = old_robust
    return (
        coefficients.tolist() if np is not None else coefficients,
        residuals,
        robust.tolist() if np is not None else robust,
        (
            inverse.tolist()
            if np is not None and inverse is not None
            else inverse
        ),
        converged,
        iterations,
        cycle_damping_used,
    )


def _design_condition_number(
    matrix: Sequence[Sequence[float]], weights: Sequence[float]
) -> float:
    """返回列标准化加权 Gram 矩阵的无穷范数条件数。"""

    if _ACTIVE_LINEAR_ALGEBRA_BACKEND == "numpy":
        np = _numpy_module()
        design = np.asarray(matrix, dtype=np.float64)
        diagonal = np.asarray(weights, dtype=np.float64)
        scales = np.sqrt(np.sum(diagonal[:, None] * design * design, axis=0))
        if bool(np.any(scales <= 1e-15)):
            return math.inf
        normalized = design / scales
        gram = normalized.T @ (diagonal[:, None] * normalized)
        try:
            return float(np.linalg.cond(gram, p=np.inf))
        except np.linalg.LinAlgError:
            return math.inf

    width = len(matrix[0])
    scales = [
        math.sqrt(
            math.fsum(weight * row[column] * row[column] for row, weight in zip(matrix, weights))
        )
        for column in range(width)
    ]
    if any(scale <= 1e-15 for scale in scales):
        return math.inf
    gram = [[0.0] * width for _ in range(width)]
    for row, weight in zip(matrix, weights):
        normalized = [row[column] / scales[column] for column in range(width)]
        for left in range(width):
            for right in range(left + 1):
                gram[left][right] += weight * normalized[left] * normalized[right]
    for left in range(width):
        for right in range(left):
            gram[right][left] = gram[left][right]
    try:
        inverse = _invert(gram)
    except MicrobenchmarkModelError:
        return math.inf
    norm = max(math.fsum(abs(value) for value in row) for row in gram)
    inverse_norm = max(math.fsum(abs(value) for value in row) for row in inverse)
    return norm * inverse_norm


def _batch_levels_and_reference(
    pairs: Sequence[_Pair],
    batch_levels: Sequence[int] | None = None,
    batch_reference: int | None = None,
) -> tuple[tuple[int, ...], int | None]:
    """返回固定的 batch 档位和最接近几何中心的实际参考档。"""

    observed = tuple(sorted({pair.batch for pair in pairs}))
    if batch_levels is None:
        levels = observed
    else:
        levels = tuple(sorted(set(batch_levels)))
        if observed != levels:
            raise MicrobenchmarkModelError(
                "拟合样本没有完整覆盖预先声明的 batch 档位"
            )
    if not levels:
        return levels, None
    if batch_reference is None:
        log_center = statistics.median(math.log(level) for level in levels)
        reference = min(
            levels,
            key=lambda level: (abs(math.log(level) - log_center), level),
        )
    else:
        if batch_reference not in levels:
            raise MicrobenchmarkModelError("batch 参考档不属于声明的档位")
        reference = batch_reference
    return levels, reference


def _design(
    pairs: Sequence[_Pair],
    response_name: str,
    *,
    batch_levels: Sequence[int] | None = None,
    batch_reference: int | None = None,
) -> tuple[list[list[float]], list[float], list[str]]:
    response: list[float] = []
    for pair in pairs:
        raw = getattr(pair, response_name)
        if raw is None:
            raise MicrobenchmarkModelError("响应列不完整")
        response.append(raw / pair.target_count)
    runs = _ordered_runs(pairs)
    names = ["intercept"]
    rows: list[list[float]] = [[1.0] for _ in pairs]
    for run in runs[1:]:
        names.append(f"run:{run}")
        for row, pair in zip(rows, pairs):
            row.append(1.0 if pair.run == run else 0.0)

    orders = {pair.order for pair in pairs}
    if len(orders) >= 2:
        names.append("order_ab_ba")
        for row, pair in zip(rows, pairs):
            row.append(pair.order)

    run_bounds: dict[str, tuple[float, float]] = {}
    for run in runs:
        values = [pair.sequence for pair in pairs if pair.run == run]
        run_bounds[run] = (min(values), max(values))
    if any(high > low for low, high in run_bounds.values()):
        names.append("within_run_drift")
        for row, pair in zip(rows, pairs):
            low, high = run_bounds[pair.run]
            row.append(0.0 if high == low else (pair.sequence - low) / (high - low) - 0.5)

    translation_rates = [
        0.0
        if pair.translation_delta is None
        else pair.translation_delta / pair.target_count
        for pair in pairs
    ]
    if any(value != 0.0 for value in translation_rates) and len(
        {round(value, 15) for value in translation_rates}
    ) >= 2:
        names.append("translation_per_target")
        for row, value in zip(rows, translation_rates):
            row.append(value)

    levels, reference = _batch_levels_and_reference(
        pairs, batch_levels, batch_reference
    )
    for level in levels:
        if level == reference:
            continue
        names.append(f"batch_level:{level}")
        for row, pair in zip(rows, pairs):
            row.append(1.0 if pair.batch == level else 0.0)
    return rows, response, names


def _heteroscedastic_weights(
    pairs: Sequence[_Pair], residuals: Sequence[float]
) -> list[float]:
    global_scale = max(1e-15, 1.4826 * _median_absolute_deviation(residuals))
    groups: dict[int, list[float]] = defaultdict(list)
    for pair, residual in zip(pairs, residuals):
        groups[pair.batch].append(residual)
    variances: dict[int, float] = {}
    for batch, values in groups.items():
        local = 1.4826 * _median_absolute_deviation(values)
        local = global_scale if local <= 1e-15 else local
        # 小组样本的 MAD 很不稳定，向全局尺度收缩。
        variance = (len(values) * local * local + 6.0 * global_scale * global_scale) / (
            len(values) + 6.0
        )
        variances[batch] = max(variance, global_scale * global_scale * 1e-4)
    raw = [1.0 / variances[pair.batch] for pair in pairs]
    center = statistics.median(raw)
    return [min(20.0, max(0.05, value / center)) for value in raw]


def _contrast_for_coefficients(
    pairs: Sequence[_Pair], names: Sequence[str]
) -> list[float]:
    runs = _ordered_runs(pairs)
    result = [0.0] * len(names)
    result[0] = 1.0
    for run in runs[1:]:
        result[names.index(f"run:{run}")] = 1.0 / len(runs)
    return result


def _sandwich_standard_error(
    matrix: Sequence[Sequence[float]],
    residuals: Sequence[float],
    base_weights: Sequence[float],
    robust_weights: Sequence[float],
    inverse: Sequence[Sequence[float]],
    contrast: Sequence[float],
) -> float | None:
    width = len(matrix[0])
    if _ACTIVE_LINEAR_ALGEBRA_BACKEND == "numpy":
        np = _numpy_module()
        design = np.asarray(matrix, dtype=np.float64)
        score = (
            np.asarray(base_weights, dtype=np.float64)
            * np.asarray(robust_weights, dtype=np.float64)
            * np.asarray(residuals, dtype=np.float64)
        )
        meat = design.T @ ((score * score)[:, None] * design)
        inv = np.asarray(inverse, dtype=np.float64)
        direction = np.asarray(contrast, dtype=np.float64) @ inv
        variance = float(direction @ meat @ direction)
        degrees = len(matrix) - width
        if degrees <= 0:
            return None
        variance *= len(matrix) / degrees
        return math.sqrt(max(0.0, variance))

    meat = [[0.0] * width for _ in range(width)]
    for row, residual, base, robust in zip(matrix, residuals, base_weights, robust_weights):
        score_scale = base * robust * residual
        for left in range(width):
            for right in range(left + 1):
                meat[left][right] += score_scale * score_scale * row[left] * row[right]
    for left in range(width):
        for right in range(left):
            meat[right][left] = meat[left][right]
    middle = [
        math.fsum(contrast[row] * inverse[row][column] for row in range(width))
        for column in range(width)
    ]
    variance = math.fsum(
        middle[left] * meat[left][right] * middle[right]
        for left in range(width)
        for right in range(width)
    )
    degrees = len(matrix) - width
    if degrees <= 0:
        return None
    variance *= len(matrix) / degrees
    return math.sqrt(max(0.0, variance))


def _fit_variant(
    pairs: Sequence[_Pair],
    response_name: str,
    *,
    compute_condition: bool = True,
    compute_standard_error: bool = True,
    batch_levels: Sequence[int] | None = None,
    batch_reference: int | None = None,
) -> _Fit:
    if len(pairs) < 4:
        raise MicrobenchmarkModelError("每个指令变体至少需要 4 个有效 pair")
    run_rank = {run: rank for rank, run in enumerate(_ordered_runs(pairs))}
    ordered = sorted(
        pairs,
        key=lambda pair: (run_rank[pair.run], pair.sequence, pair.pair),
    )
    levels, reference = _batch_levels_and_reference(
        ordered, batch_levels, batch_reference
    )
    matrix, response, names = _design(
        ordered,
        response_name,
        batch_levels=levels,
        batch_reference=reference,
    )
    sparse_rows = [
        [(column, value) for column, value in enumerate(row) if value != 0.0]
        for row in matrix
    ]
    count_reference = statistics.median(pair.target_count for pair in ordered)
    initial_weights = [
        min(16.0, max(1.0 / 16.0, (pair.target_count / count_reference) ** 2))
        for pair in ordered
    ]
    _, initial_residuals, _, _, _, _, initial_cycle_damping = _robust_fit(
        matrix,
        response,
        initial_weights,
        sparse_rows=sparse_rows,
        compute_inverse=False,
    )
    hetero = _heteroscedastic_weights(ordered, initial_residuals)
    (
        coefficients,
        residuals,
        robust,
        inverse,
        converged,
        iterations,
        final_cycle_damping,
    ) = _robust_fit(
        matrix,
        response,
        hetero,
        sparse_rows=sparse_rows,
        compute_inverse=compute_standard_error,
    )
    combined_weights = [
        base * weight for base, weight in zip(hetero, robust)
    ]
    contrast = _contrast_for_coefficients(ordered, names)
    estimate = math.fsum(value * coefficient for value, coefficient in zip(contrast, coefficients))
    standard_error = (
        _sandwich_standard_error(
            matrix, residuals, hetero, robust, inverse, contrast
        )
        if compute_standard_error
        else None
    )
    by_name = dict(zip(names, coefficients))
    run_level_estimates = {
        run: by_name["intercept"] + by_name.get(f"run:{run}", 0.0)
        for run in _ordered_runs(ordered)
    }
    batch_level_effects = {
        level: (
            0.0
            if level == reference
            else by_name[f"batch_level:{level}"]
        )
        for level in levels
    }
    if len(levels) >= 2:
        low, high = levels[0], levels[-1]
        batch_log_range = math.log(high / low)
        # 兼容旧的 per_log_batch 输出；这是 categorical 两端点的割线，
        # 不参与新门禁，也不能代表中间档位。
        batch_effect = (
            (batch_level_effects[high] - batch_level_effects[low])
            / batch_log_range
        )
    else:
        batch_log_range = 0.0
        batch_effect = None
    batch_peak_to_peak = (
        max(batch_level_effects.values()) - min(batch_level_effects.values())
        if batch_level_effects
        else 0.0
    )
    return _Fit(
        estimate=estimate,
        standard_error=standard_error,
        order_effect=by_name.get("order_ab_ba"),
        drift_effect=by_name.get("within_run_drift"),
        batch_effect=batch_effect,
        batch_reference=reference,
        batch_levels=levels,
        batch_level_effects=batch_level_effects,
        batch_peak_to_peak=batch_peak_to_peak,
        translation_effect=by_name.get("translation_per_target"),
        batch_log_range=batch_log_range,
        residuals=residuals,
        robust_weights=robust,
        hetero_weights=hetero,
        pairs=ordered,
        run_level_estimates=run_level_estimates,
        predictor_names=names,
        irls_converged=converged,
        irls_iterations=iterations,
        irls_cycle_damping_used=(
            initial_cycle_damping or final_cycle_damping
        ),
        design_condition_number=(
            _design_condition_number(matrix, combined_weights)
            if compute_condition
            else 1.0
        ),
    )


def _classical_variant_estimate(
    pairs: Sequence[_Pair],
    response_name: str,
    *,
    batch_levels: Sequence[int] | None = None,
    batch_reference: int | None = None,
    heteroscedastic_weights: Sequence[float] | None = None,
) -> float:
    """返回不使用 Huber 降权的异方差 WLS 对照估计。

    该估计器与主模型共享完全相同的设计矩阵、配对响应和 target-count
    异方差权重；只省略最终 Huber influence 权重。它不是第二个发布模型，
    而是用来检验主估计是否依赖少数观测的敏感性对照。
    """

    if len(pairs) < 4:
        raise MicrobenchmarkModelError("经典 WLS 对照至少需要 4 个有效 pair")
    run_rank = {run: rank for rank, run in enumerate(_ordered_runs(pairs))}
    sort_key = lambda pair: (run_rank[pair.run], pair.sequence, pair.pair)
    supplied_hetero: list[float] | None = None
    if heteroscedastic_weights is None:
        ordered = sorted(pairs, key=sort_key)
    else:
        if len(heteroscedastic_weights) != len(pairs):
            raise MicrobenchmarkModelError("经典 WLS 对照的异方差权重长度不匹配")
        ordered_observations = sorted(
            zip(pairs, heteroscedastic_weights, strict=True),
            key=lambda observation: sort_key(observation[0]),
        )
        ordered = [pair for pair, _weight in ordered_observations]
        supplied_hetero = [
            float(weight) for _pair, weight in ordered_observations
        ]
    levels, reference = _batch_levels_and_reference(
        ordered, batch_levels, batch_reference
    )
    matrix, response, _names = _design(
        ordered,
        response_name,
        batch_levels=levels,
        batch_reference=reference,
    )
    sparse_rows = [
        [(column, value) for column, value in enumerate(row) if value != 0.0]
        for row in matrix
    ]
    count_reference = statistics.median(pair.target_count for pair in ordered)
    initial_weights = [
        min(16.0, max(1.0 / 16.0, (pair.target_count / count_reference) ** 2))
        for pair in ordered
    ]
    if supplied_hetero is None:
        _coefficients, initial_residuals, _robust, _inverse, *_rest = (
            _robust_fit(
                matrix,
                response,
                initial_weights,
                sparse_rows=sparse_rows,
                compute_inverse=False,
            )
        )
        hetero = _heteroscedastic_weights(ordered, initial_residuals)
    else:
        hetero = supplied_hetero
    coefficients, _inverse = _wls_native(
        matrix,
        response,
        hetero,
        sparse_rows=sparse_rows,
        compute_inverse=False,
    )
    contrast = _contrast_for_coefficients(ordered, _names)
    estimate = math.fsum(
        value * coefficient
        for value, coefficient in zip(contrast, coefficients)
    )
    if not math.isfinite(estimate):
        raise MicrobenchmarkModelError("经典 WLS 对照估计不是有限数")
    return float(estimate)


def _acf_ess(fit: _Fit) -> tuple[float, list[dict[str, Any]], int]:
    rows: list[dict[str, Any]] = []
    total_ess = 0.0
    recommended_block = 1
    for run in _ordered_runs(fit.pairs):
        members = [
            (pair, residual, robust * hetero)
            for pair, residual, robust, hetero in zip(
                fit.pairs,
                fit.residuals,
                fit.robust_weights,
                fit.hetero_weights,
            )
            if pair.run == run
        ]
        count = len(members)
        weight_sum = math.fsum(weight for _pair, _residual, weight in members)
        weight_square_sum = math.fsum(
            weight * weight for _pair, _residual, weight in members
        )
        kish_pairs = (
            weight_sum * weight_sum / weight_square_sum
            if weight_square_sum > 0.0
            else 0.0
        )
        by_block: dict[str, list[tuple[_Pair, float, float]]] = defaultdict(list)
        for member in members:
            by_block[member[0].block].append(member)
        ordered_blocks = sorted(
            by_block,
            key=lambda name: min(item[0].sequence for item in by_block[name]),
        )
        values: list[float] = []
        block_weights: list[float] = []
        for block in ordered_blocks:
            block_weight = math.fsum(item[2] for item in by_block[block])
            if block_weight <= 0.0:
                continue
            values.append(
                math.fsum(item[1] * item[2] for item in by_block[block])
                / block_weight
            )
            block_weights.append(block_weight)
        block_count = len(values)
        if block_count < 3:
            tau = float(block_count) if block_count else 1.0
            lag_one = None
        else:
            block_weight_sum = math.fsum(block_weights)
            center = (
                math.fsum(
                    value * weight for value, weight in zip(values, block_weights)
                )
                / block_weight_sum
            )
            denominator = math.fsum(
                weight * (value - center) ** 2
                for value, weight in zip(values, block_weights)
            )
            positive = 0.0
            lag_one = 0.0
            if denominator > 0.0:
                for lag in range(1, min(block_count // 3, 50) + 1):
                    numerator = math.fsum(
                        math.sqrt(block_weights[index] * block_weights[index - lag])
                        * (values[index] - center)
                        * (values[index - lag] - center)
                        for index in range(lag, block_count)
                    )
                    rho = numerator / denominator
                    if lag == 1:
                        lag_one = rho
                    if rho <= 0.0:
                        break
                    positive += rho
            tau = max(1.0, 1.0 + 2.0 * positive)
        ess = kish_pairs / tau if tau else 0.0
        total_ess += ess
        recommended_block = max(recommended_block, int(math.ceil(tau)))
        rows.append(
            {
                "run": run,
                "pairs": count,
                "blocks": block_count,
                "kish_effective_pairs_before_autocorrelation": kish_pairs,
                "lag1_autocorrelation": lag_one,
                "integrated_autocorrelation_time_blocks": tau,
                "effective_pairs": ess,
            }
        )
    return total_ess, rows, recommended_block


def _student_t_critical(confidence: float, degrees: int) -> float:
    """返回双侧 Student-t 临界值；df=1/2 精确，其余用展开。"""

    probability = 0.5 + confidence / 2.0
    if degrees <= 0:
        return math.inf
    # Cornish-Fisher 在最关键的 df=1/2 小样本处会明显偏小；这两档有闭式
    # 逆 CDF，直接使用精确值，避免 mKH 区间反而反保守。
    if degrees == 1:
        return math.tan(math.pi * (probability - 0.5))
    if degrees == 2:
        centered = 2.0 * probability - 1.0
        return math.sqrt(2.0) * centered / math.sqrt(
            1.0 - centered * centered
        )
    z_value = NormalDist().inv_cdf(probability)
    inverse_degrees = 1.0 / degrees
    return (
        z_value
        + (z_value**3 + z_value) * inverse_degrees / 4.0
        + (5.0 * z_value**5 + 16.0 * z_value**3 + 3.0 * z_value)
        * inverse_degrees**2
        / 96.0
    )


def _wilson_upper_bound(
    successes: float, total: float, confidence: float
) -> float:
    """返回二项/准二项比例的单侧 Wilson 上置信界。"""

    if total <= 0:
        return 1.0
    probability = successes / total
    z_value = NormalDist().inv_cdf(confidence)
    z_squared = z_value * z_value
    denominator = 1.0 + z_squared / total
    center = (probability + z_squared / (2.0 * total)) / denominator
    radius = (
        z_value
        * math.sqrt(
            probability * (1.0 - probability) / total
            + z_squared / (4.0 * total * total)
        )
        / denominator
    )
    return min(1.0, center + radius)


def _run_cluster_proportion_upper_bound(
    outcomes: Sequence[bool], runs: Sequence[str], confidence: float
) -> dict[str, Any]:
    """把完整 QEMU run 作为独立单位估计异常比例的保守上界。

    pair 仅在各自 run 内汇总为一个 ``[0, 1]`` 比例。上界取 run-level
    quasi-Wilson score 与 run 均值 Student-t 上界的较大者，避免复制同一
    run 的相关 pair 虚增独立样本量，也避免 run 间方差恰为零时区间坍缩。
    """

    if len(outcomes) != len(runs):
        raise MicrobenchmarkModelError("异常标记与 run 标签数量不一致")
    grouped: dict[str, list[bool]] = defaultdict(list)
    for outcome, run in zip(outcomes, runs):
        grouped[run].append(outcome)
    per_run = [
        {
            "run": run,
            "pairs": len(values),
            "severe_outliers": sum(values),
            "fraction": sum(values) / len(values),
        }
        for run, values in sorted(grouped.items())
        if values
    ]
    if not per_run:
        return {
            "runs": 0,
            "mean_run_fraction": None,
            "pair_fraction": None,
            "score_upper": 1.0,
            "t_upper": 1.0,
            "upper": 1.0,
            "per_run": [],
        }
    fractions = [float(row["fraction"]) for row in per_run]
    run_count = len(fractions)
    mean_fraction = math.fsum(fractions) / run_count
    score_upper = _wilson_upper_bound(
        math.fsum(fractions), run_count, confidence
    )
    if run_count < 2:
        t_upper = 1.0
    else:
        standard_error = statistics.stdev(fractions) / math.sqrt(run_count)
        central_confidence = 2.0 * confidence - 1.0
        t_critical = (
            _student_t_critical(central_confidence, run_count - 1)
            if central_confidence > 0.0
            else 0.0
        )
        t_upper = min(1.0, mean_fraction + t_critical * standard_error)
    pair_fraction = sum(outcomes) / len(outcomes) if outcomes else None
    return {
        "runs": run_count,
        "mean_run_fraction": mean_fraction,
        "pair_fraction": pair_fraction,
        "score_upper": score_upper,
        "t_upper": t_upper,
        "upper": max(score_upper, t_upper),
        "per_run": per_run,
    }


def _paule_mandel_tau_squared(
    estimates: Sequence[float], variances: Sequence[float]
) -> dict[str, Any]:
    """求解 ``Q(tau^2)=k-1`` 的 Paule-Mandel 方差分量。"""

    if len(estimates) != len(variances) or len(estimates) < 2:
        raise MicrobenchmarkModelError("Paule-Mandel 至少需要两个等长 run 估计")
    if any(
        not math.isfinite(value) or value <= 0.0 for value in variances
    ):
        raise MicrobenchmarkModelError("Paule-Mandel 的 run 方差必须为正有限数")

    def location_and_q(tau_squared: float) -> tuple[float, float]:
        weights = [1.0 / (variance + tau_squared) for variance in variances]
        weight_sum = math.fsum(weights)
        location = math.fsum(
            weight * estimate
            for weight, estimate in zip(weights, estimates)
        ) / weight_sum
        q_value = math.fsum(
            weight * (estimate - location) ** 2
            for weight, estimate in zip(weights, estimates)
        )
        return location, q_value

    degrees = len(estimates) - 1
    _fixed, q_zero = location_and_q(0.0)
    if q_zero <= degrees:
        return {
            "tau_squared": 0.0,
            "q_at_zero": q_zero,
            "q_at_tau": q_zero,
            "iterations": 0,
            "converged": True,
        }

    upper = max(
        1e-18,
        statistics.pvariance(estimates),
        statistics.median(variances),
    )
    _location, q_upper = location_and_q(upper)
    bracket_iterations = 0
    while q_upper > degrees and bracket_iterations < 100:
        upper *= 2.0
        _location, q_upper = location_and_q(upper)
        bracket_iterations += 1
    if q_upper > degrees or not math.isfinite(upper):
        return {
            "tau_squared": upper,
            "q_at_zero": q_zero,
            "q_at_tau": q_upper,
            "iterations": bracket_iterations,
            "converged": False,
        }

    lower = 0.0
    converged = False
    q_middle = q_upper
    bisection_iterations = 0
    for bisection_iterations in range(1, 201):
        middle = (lower + upper) / 2.0
        _location, q_middle = location_and_q(middle)
        if abs(q_middle - degrees) <= 1e-10 * max(1.0, degrees):
            lower = upper = middle
            converged = True
            break
        if q_middle > degrees:
            lower = middle
        else:
            upper = middle
        if upper - lower <= 1e-12 * max(1.0, upper):
            converged = True
            break
    tau_squared = (lower + upper) / 2.0
    _location, q_at_tau = location_and_q(tau_squared)
    return {
        "tau_squared": tau_squared,
        "q_at_zero": q_zero,
        "q_at_tau": q_at_tau,
        "iterations": bracket_iterations + bisection_iterations,
        "converged": converged,
    }


def _summarize_random_effects(
    estimates: Sequence[float],
    variances: Sequence[float],
    per_run: list[dict[str, Any]],
    total_runs: int,
    confidence: float,
    *,
    estimand: str,
) -> dict[str, Any]:
    if len(estimates) < 2:
        return {
            "runs": per_run,
            "random_effect_estimate": estimates[0] if estimates else None,
            "tau_squared": None,
            "i_squared": None,
            "usable_runs": len(estimates),
            "total_runs": total_runs,
            "prediction_interval": None,
            "tau_squared_method": "Paule-Mandel",
            "confidence_interval": None,
            "confidence_interval_method": "modified-Hartung-Knapp-t(k-1)",
            "estimand": estimand,
            "identifiable": False,
        }
    paule_mandel = _paule_mandel_tau_squared(estimates, variances)
    degrees = len(estimates) - 1
    tau_squared = float(paule_mandel["tau_squared"])
    random_weights = [1.0 / (variance + tau_squared) for variance in variances]
    random_weight_sum = math.fsum(random_weights)
    random_estimate = math.fsum(
        weight * value for weight, value in zip(random_weights, estimates)
    ) / random_weight_sum
    conventional_standard_error = math.sqrt(1.0 / random_weight_sum)
    q_at_tau = math.fsum(
        weight * (value - random_estimate) ** 2
        for weight, value in zip(random_weights, estimates)
    )
    hartung_knapp_scale = q_at_tau / degrees
    modified_hartung_knapp_scale = max(1.0, hartung_knapp_scale)
    random_standard_error = conventional_standard_error * math.sqrt(
        modified_hartung_knapp_scale
    )
    q_at_zero = float(paule_mandel["q_at_zero"])
    i_squared = (
        max(0.0, (q_at_zero - degrees) / q_at_zero)
        if q_at_zero > 0.0
        else 0.0
    )
    critical = _student_t_critical(confidence, degrees)
    confidence_half_width = critical * random_standard_error
    prediction_half_width = critical * math.sqrt(
        tau_squared + random_standard_error * random_standard_error
    )
    return {
        "runs": per_run,
        "random_effect_estimate": random_estimate,
        "random_effect_standard_error": random_standard_error,
        "conventional_random_effect_standard_error": (
            conventional_standard_error
        ),
        "tau_squared": tau_squared,
        "tau_squared_method": "Paule-Mandel",
        "tau_squared_converged": paule_mandel["converged"],
        "tau_squared_iterations": paule_mandel["iterations"],
        "i_squared": i_squared,
        "cochran_q": q_at_zero,
        "paule_mandel_q_at_tau_squared": q_at_tau,
        "hartung_knapp_scale": hartung_knapp_scale,
        "modified_hartung_knapp_scale": modified_hartung_knapp_scale,
        "degrees_of_freedom": degrees,
        "usable_runs": len(estimates),
        "total_runs": total_runs,
        "confidence_interval": [
            random_estimate - confidence_half_width,
            random_estimate + confidence_half_width,
        ],
        "confidence_interval_method": "modified-Hartung-Knapp-t(k-1)",
        "prediction_interval": [
            random_estimate - prediction_half_width,
            random_estimate + prediction_half_width,
        ],
        "prediction_interval_method": (
            "Paule-Mandel-modified-Hartung-Knapp-t(k-1)-with-ESS-inflated-run-SE"
        ),
        "estimand": estimand,
        "identifiable": True,
    }


def _random_effects(
    fit: _Fit, response_name: str, confidence: float
) -> dict[str, Any]:
    """保留给局部 contrast 诊断；绝对质量门禁使用 control-chain 版本。"""

    estimates: list[float] = []
    variances: list[float] = []
    per_run: list[dict[str, Any]] = []
    run_names = _ordered_runs(fit.pairs)
    for run in run_names:
        subset = [pair for pair in fit.pairs if pair.run == run]
        if len(subset) < 4:
            per_run.append({"run": run, "pairs": len(subset), "estimate": None})
            continue
        try:
            current = _fit_variant(
                subset,
                response_name,
                batch_levels=fit.batch_levels,
                batch_reference=fit.batch_reference,
            )
        except MicrobenchmarkModelError:
            per_run.append({"run": run, "pairs": len(subset), "estimate": None})
            continue
        run_ess, _run_rows, _run_block = _acf_ess(current)
        if run_ess <= 0.0:
            per_run.append(
                {
                    "run": run,
                    "pairs": len(subset),
                    "estimate": None,
                    "reason": "zero-effective-pairs",
                }
            )
            continue
        dependence_inflation = max(1.0, len(subset) / run_ess)
        variance = (
            current.standard_error * current.standard_error * dependence_inflation
            if current.standard_error is not None and current.standard_error > 0.0
            else max(1e-18, statistics.pvariance(current.residuals) / len(subset))
        )
        estimates.append(current.estimate)
        variances.append(variance)
        per_run.append(
            {
                "run": run,
                "pairs": len(subset),
                "estimate": current.estimate,
                "standard_error": math.sqrt(variance),
                "effective_pairs": run_ess,
                "irls_converged": current.irls_converged,
                "irls_iterations": current.irls_iterations,
                "design_condition_number": _json_finite(
                    current.design_condition_number
                ),
            }
        )
    return _summarize_random_effects(
        estimates,
        variances,
        per_run,
        len(run_names),
        confidence,
        estimand="local-target-minus-control-contrast",
    )


def _per_run_design_diagnostics(fit: _Fit, response_name: str) -> list[dict[str, Any]]:
    """检查每个独立 run 是否覆盖相同 count-level、顺序和可辨识设计。"""

    global_batches = {pair.batch for pair in fit.pairs}
    diagnostics: list[dict[str, Any]] = []
    for run in _ordered_runs(fit.pairs):
        members = [pair for pair in fit.pairs if pair.run == run]
        negative = sum(pair.order < 0.0 for pair in members)
        positive = sum(pair.order > 0.0 for pair in members)
        order_balance = min(negative, positive) / len(members) if members else 0.0
        batches = {pair.batch for pair in members}
        blocks = {pair.block for pair in members}
        current: _Fit | None = None
        try:
            current = _fit_variant(
                members,
                response_name,
                batch_levels=fit.batch_levels,
                batch_reference=fit.batch_reference,
            )
        except MicrobenchmarkModelError:
            pass
        diagnostics.append(
            {
                "run": run,
                "pairs": len(members),
                "blocks": len(blocks),
                "count_levels": len(batches),
                "covers_all_count_levels": batches == global_batches,
                "order_balance": order_balance,
                "irls_converged": (
                    None if current is None else current.irls_converged
                ),
                "irls_iterations": (
                    None if current is None else current.irls_iterations
                ),
                "design_condition_number": (
                    None
                    if current is None
                    else _json_finite(current.design_condition_number)
                ),
                "complete": (
                    current is not None
                    and len(members) >= 4
                    and len(blocks) >= 3
                    and batches == global_batches
                    and order_balance >= 0.20
                ),
            }
        )
    return diagnostics


def _moving_block_positions(length: int, block_length: int, rng: random.Random) -> list[int]:
    if length <= 0:
        return []
    block_length = max(1, min(block_length, length))
    result: list[int] = []
    while len(result) < length:
        start = rng.randrange(length)
        result.extend((start + offset) % length for offset in range(block_length))
    return result[:length]


def _run_resample_positions(
    length: int, block_length: int, rng: random.Random
) -> list[int]:
    """主权重与辅助一致性检查共享的 run circular-block 下标。"""

    return _moving_block_positions(length, block_length, rng)


def _hierarchical_resample(
    pairs: Sequence[_Pair],
    block_length: int,
    rng: random.Random,
    *,
    run_block_length: int = 1,
    run_positions: Sequence[int] | None = None,
) -> list[_Pair]:
    by_super_run: dict[str, list[_Pair]] = defaultdict(list)
    for pair in pairs:
        by_super_run[pair.super_run].append(pair)
    super_run_names = _ordered_super_runs(pairs)
    if run_positions is None:
        run_positions = _moving_block_positions(
            len(super_run_names), run_block_length, rng
        )
    if len(run_positions) != len(super_run_names) or any(
        position < 0 or position >= len(super_run_names)
        for position in run_positions
    ):
        raise MicrobenchmarkModelError("super-run bootstrap 下标越界或长度不匹配")
    selected_super_runs = [
        super_run_names[position] for position in run_positions
    ]
    output: list[_Pair] = []
    synthetic_run_order = 0
    for super_copy, super_run in enumerate(selected_super_runs):
        super_members = by_super_run[super_run]
        source_runs = _ordered_runs(super_members)
        synthetic_super_run = f"bootstrap-super-run-{super_copy}"
        for source_run in source_runs:
            members = [pair for pair in super_members if pair.run == source_run]
            blocks: dict[str, list[_Pair]] = defaultdict(list)
            for pair in members:
                blocks[pair.block].append(pair)
            block_names = sorted(
                blocks,
                key=lambda name: min(pair.sequence for pair in blocks[name]),
            )
            head_blocks = [
                name
                for name in block_names
                if any(pair.anchor_position == "head" for pair in blocks[name])
            ]
            tail_blocks = [
                name
                for name in block_names
                if any(pair.anchor_position == "tail" for pair in blocks[name])
            ]
            if head_blocks or tail_blocks:
                if len(head_blocks) != 1 or len(tail_blocks) != 1:
                    raise MicrobenchmarkModelError(
                        "anchor bootstrap 要求每个 QEMU run 恰有一个 head/tail block"
                    )
                body_blocks = [
                    name
                    for name in block_names
                    if name not in {head_blocks[0], tail_blocks[0]}
                ]
                body_positions = _moving_block_positions(
                    len(body_blocks), block_length, rng
                )
                selected_blocks = [head_blocks[0]] + [
                    body_blocks[position] for position in body_positions
                ] + [tail_blocks[0]]
            else:
                positions = _moving_block_positions(
                    len(block_names), block_length, rng
                )
                selected_blocks = [block_names[position] for position in positions]
            sequence = 0.0
            synthetic_run = f"{synthetic_super_run}-qemu-{synthetic_run_order}"
            for block_copy, selected_block in enumerate(selected_blocks):
                for pair in sorted(
                    blocks[selected_block],
                    key=lambda item: item.sequence,
                ):
                    output.append(
                        replace(
                            pair,
                            run=synthetic_run,
                            run_order=synthetic_run_order,
                            run_order_source=(
                                "bootstrap-super-run-circular-moving-block"
                            ),
                            super_run=synthetic_super_run,
                            super_run_order=super_copy,
                            block=f"bootstrap-block-{block_copy}",
                            sequence=sequence,
                        )
                    )
                    sequence += 1.0
            synthetic_run_order += 1
    return output


def _resolve_control_references(
    keys: Sequence[_InstructionKey],
    references: Mapping[_InstructionKey, _ControlReference | None],
) -> tuple[dict[_InstructionKey, _InstructionKey | None], dict[_InstructionKey, str]]:
    """把可能只含粗粒度 baseline 信息的引用收敛到唯一完整编码键。"""

    controls: dict[_InstructionKey, _InstructionKey | None] = {}
    failures: dict[_InstructionKey, str] = {}
    for key, reference in references.items():
        if reference is None:
            controls[key] = None
            continue
        candidates = [
            candidate
            for candidate in keys
            if candidate.mnemonic == reference.mnemonic
            and candidate.size == reference.size
            and candidate.aq == reference.aq
            and candidate.rl == reference.rl
            and candidate.csr == reference.csr
            and (
                reference.encoding_key is None
                or candidate.encoding_key == reference.encoding_key
            )
            and (
                reference.encoding_hex is None
                or candidate.encoding_hex == reference.encoding_hex
            )
            and (reference.pattern is None or candidate.pattern == reference.pattern)
        ]
        if len(candidates) == 1:
            controls[key] = candidates[0]
        elif not candidates:
            failures[key] = "unmeasured-control-reference"
        else:
            failures[key] = "ambiguous-control-reference"
    return controls, failures


def _resolve_absolute(
    contrasts: Mapping[_InstructionKey, float],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
) -> tuple[dict[_InstructionKey, float | None], dict[_InstructionKey, str]]:
    resolved: dict[_InstructionKey, float | None] = {}
    failures: dict[_InstructionKey, str] = {}

    def visit(key: _InstructionKey, stack: set[_InstructionKey]) -> float | None:
        if key in resolved:
            return resolved[key]
        if key in stack:
            failures[key] = "control-reference-cycle"
            resolved[key] = None
            return None
        if key not in controls:
            failures[key] = "control-reference-unresolved"
            resolved[key] = None
            return None
        control = controls.get(key)
        if control is None:
            resolved[key] = contrasts[key]
            return resolved[key]
        if control not in contrasts:
            failures[key] = "unmeasured-control-reference"
            resolved[key] = None
            return None
        base = visit(control, stack | {key})
        if base is None:
            failures[key] = failures.get(key, "unresolved-control-reference")
            resolved[key] = None
        else:
            resolved[key] = contrasts[key] + base
        return resolved[key]

    for key in contrasts:
        visit(key, set())
    return resolved, failures


def _simultaneous_intervals(
    points: Mapping[Any, float],
    rows: Sequence[Mapping[Any, float]],
    confidence: float,
    monte_carlo_confidence: float | None = None,
) -> tuple[dict[Any, list[float] | None], float | None, int, dict[str, Any]]:
    """以全族 max-standardized-deviation 构造同时区间。"""

    alpha = 1.0 - confidence
    family = set(points)
    # 所有 estimand 必须先筛到同一批完整重采样。若允许每个 estimand
    # 各自使用部分 replicate，不同缺失模式会让尺度与联合统计量基于不同
    # 经验分布，进而产生反保守的同时区间。
    complete_rows = [row for row in rows if family.issubset(row)]
    # 用独立的 bootstrap 子样本估计标准化尺度和校准 max-stat 分位数。
    # 若同一批 draw 同时参与样本标准差和 order statistic，max-stat 之间
    # 只有交换性而非二项 rank 证明要求的条件独立性。正式 B=4999 时固定
    # 使用前 999 个 complete draw 拟合尺度、后 4000 个校准临界值。
    if len(complete_rows) < PUBLICATION_MINIMUM_MAX_STAT_CALIBRATION_REPLICATES:
        # 小样本只用于诊断，若再切掉一个尺度子样本会无声丢弃大量有效
        # 观测。这里显式复用完整行，并在 evidence 中标记非独立校准；正式
        # 发布要求 B>=4999，因此永远走下面的独立 999/4000 分区。
        scale_rows = complete_rows
        calibration_rows = complete_rows
        partition_method = "all-complete-replicates-diagnostic-v1"
    else:
        scale_count = min(
            PUBLICATION_MAX_STAT_SCALE_REPLICATES,
            max(2, len(complete_rows) // MAX_STATISTIC_SCALE_REPLICATE_DIVISOR),
            max(0, len(complete_rows) - 1),
        )
        scale_rows = complete_rows[:scale_count]
        calibration_rows = complete_rows[scale_count:]
        partition_method = (
            "ordered-independent-prefix-scale-remainder-quantile-v1"
        )
    standard_deviations: dict[Any, float | None] = {}
    marginal: dict[Any, list[float] | None] = {}
    for key, point in points.items():
        scale_values = [row[key] for row in scale_rows]
        calibration_values = [row[key] for row in calibration_rows]
        complete_values = [row[key] for row in complete_rows]
        scale = (
            statistics.stdev(scale_values)
            if len(scale_values) >= 2
            else None
        )
        if scale == 0.0 and not all(
            value == point for value in complete_values
        ):
            scale = None
        standard_deviations[key] = scale
        low = _quantile(calibration_values, alpha / 2.0)
        high = _quantile(calibration_values, 1.0 - alpha / 2.0)
        marginal[key] = None if low is None or high is None else [low, high]
    eligible = [
        key
        for key, scale in standard_deviations.items()
        if scale is not None and scale > 0.0 and math.isfinite(points[key])
    ]
    max_statistics: list[float] = []
    exact_family = bool(points) and all(
        complete_rows
        and all(row[key] == points[key] for row in complete_rows)
        for key in points
    )
    if exact_family:
        max_statistics = [0.0 for _row in calibration_rows]
    for row in calibration_rows:
        if not eligible:
            break
        deviations = [
            abs((row[key] - points[key]) / float(standard_deviations[key]))
            for key in eligible
        ]
        if deviations:
            max_statistics.append(max(deviations))
    critical, monte_carlo = _conservative_bootstrap_quantile(
        max_statistics,
        confidence,
        confidence
        if monte_carlo_confidence is None
        else monte_carlo_confidence,
    )
    monte_carlo.update(
        {
            "replicate_partition_method": partition_method,
            "complete_family_replicates": len(complete_rows),
            "scale_replicates": len(scale_rows),
            "quantile_replicates": len(calibration_rows),
        }
    )
    intervals: dict[Any, list[float] | None] = {}
    for key, point in points.items():
        scale = standard_deviations[key]
        if scale is None:
            # 精确常量的 bootstrap 分布仍支持零宽同时区间。
            values = [row[key] for row in complete_rows]
            intervals[key] = [point, point] if values and all(
                value == point for value in values
            ) else None
            continue
        if critical is None:
            intervals[key] = None
            continue
        low = point - critical * scale
        high = point + critical * scale
        if marginal[key] is not None:
            low = min(low, marginal[key][0])
            high = max(high, marginal[key][1])
        intervals[key] = [low, high]
    return intervals, critical, len(max_statistics), monte_carlo


def _conservative_bootstrap_quantile(
    values: Sequence[float], probability: float, monte_carlo_confidence: float
) -> tuple[float | None, dict[str, Any]]:
    """返回 bootstrap 分位数的单侧 Monte-Carlo 上置信 order statistic。"""

    count = len(values)
    evidence: dict[str, Any] = {
        "method": "one-sided-binomial-order-statistic-upper-confidence-bound",
        "target_probability": probability,
        "monte_carlo_confidence": monte_carlo_confidence,
        "replicates": count,
        "required_rank": None,
        "selected_rank": None,
        "finite_rank_supported": False,
    }
    if count == 0:
        return None, evidence
    if not 0.0 < probability < 1.0 or not 0.0 < monte_carlo_confidence < 1.0:
        raise MicrobenchmarkModelError("bootstrap 分位数概率必须位于 (0,1)")

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
    required_rank = count + 1
    for successes, mass in enumerate(probabilities):
        cumulative += mass / normalization
        if cumulative >= monte_carlo_confidence:
            # X_(r) >= q_p exactly when at most r-1 bootstrap draws are below q_p.
            required_rank = successes + 1
            break
    supported = required_rank <= count
    selected_rank = required_rank if supported else count
    ordered = sorted(values)
    evidence.update(
        {
            "required_rank": required_rank,
            "selected_rank": selected_rank,
            "finite_rank_supported": supported,
        }
    )
    return ordered[selected_rank - 1], evidence


def _per_run_absolute_estimates(
    fits: Mapping[_InstructionKey, _Fit],
    response_names: Mapping[_InstructionKey, str],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
) -> tuple[
    dict[_InstructionKey, dict[str, float]],
    dict[_InstructionKey, set[str]],
    dict[_InstructionKey, dict[str, float]],
]:
    """按 run 拟合 control 链，并以标准误之和给出最坏相关性方差界。"""

    run_names = _ordered_super_runs(
        pair for fit in fits.values() for pair in fit.pairs
    )
    estimates: dict[_InstructionKey, dict[str, float]] = {
        key: {} for key in fits
    }
    incomplete: dict[_InstructionKey, set[str]] = {
        key: set() for key in fits
    }
    variances: dict[_InstructionKey, dict[str, float]] = {
        key: {} for key in fits
    }
    for run in run_names:
        contrasts: dict[_InstructionKey, float] = {}
        contrast_standard_errors: dict[_InstructionKey, float] = {}
        for key, fit in fits.items():
            members = [pair for pair in fit.pairs if pair.super_run == run]
            response_name = response_names[key]
            if len(members) < 4 or any(
                getattr(pair, response_name) is None for pair in members
            ):
                incomplete[key].add(run)
                continue
            try:
                current = _fit_variant(
                    members,
                    response_name,
                    compute_condition=False,
                    batch_levels=fit.batch_levels,
                    batch_reference=fit.batch_reference,
                )
            except MicrobenchmarkModelError:
                incomplete[key].add(run)
                continue
            if not current.irls_converged:
                incomplete[key].add(run)
                continue
            run_ess, _run_rows, _run_block = _acf_ess(current)
            if run_ess <= 0.0:
                incomplete[key].add(run)
                continue
            dependence_inflation = max(1.0, len(members) / run_ess)
            variance = (
                current.standard_error
                * current.standard_error
                * dependence_inflation
                if current.standard_error is not None
                and current.standard_error > 0.0
                else max(
                    1e-18,
                    statistics.pvariance(current.residuals) / len(members),
                )
            )
            contrasts[key] = current.estimate
            contrast_standard_errors[key] = math.sqrt(variance)
        absolute, failures = _resolve_absolute(contrasts, controls)
        absolute_standard_errors, variance_failures = _resolve_absolute(
            contrast_standard_errors, controls
        )
        for key in fits:
            value = absolute.get(key)
            standard_error = absolute_standard_errors.get(key)
            if (
                key in failures
                or key in variance_failures
                or value is None
                or standard_error is None
                or not math.isfinite(value)
                or not math.isfinite(standard_error)
                or standard_error <= 0.0
            ):
                incomplete[key].add(run)
            else:
                estimates[key][run] = float(value)
                variances[key][run] = float(standard_error * standard_error)
    return estimates, incomplete, variances


def _absolute_random_effects(
    fits: Mapping[_InstructionKey, _Fit],
    response_names: Mapping[_InstructionKey, str],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
    confidence: float,
    per_run_data: tuple[
        dict[_InstructionKey, dict[str, float]],
        dict[_InstructionKey, set[str]],
        dict[_InstructionKey, dict[str, float]],
    ]
    | None = None,
) -> dict[_InstructionKey, dict[str, Any]]:
    """以 per-run absolute cost，而非局部 target-control contrast 做异质性。"""

    estimates, incomplete, variances = (
        per_run_data
        if per_run_data is not None
        else _per_run_absolute_estimates(fits, response_names, controls)
    )
    result: dict[_InstructionKey, dict[str, Any]] = {}
    for key, fit in fits.items():
        run_names = _ordered_super_runs(fit.pairs)
        usable = [
            run
            for run in run_names
            if run in estimates[key] and run in variances[key]
        ]
        per_run = [
            {
                "run": run,
                "pairs": sum(pair.super_run == run for pair in fit.pairs),
                "estimate": estimates[key].get(run),
                "standard_error": (
                    math.sqrt(variances[key][run])
                    if run in variances[key]
                    else None
                ),
                "complete_control_chain": run not in incomplete[key],
            }
            for run in run_names
        ]
        meta = _summarize_random_effects(
            [estimates[key][run] for run in usable],
            [variances[key][run] for run in usable],
            per_run,
            len(run_names),
            confidence,
            estimand="absolute-instruction-cost-through-control-chain",
        )
        meta["run_variance_method"] = (
            "square-of-summed-control-chain-contrast-SEs-with-ESS-inflation"
        )
        meta["run_variance_covariance_assumption"] = (
            "worst-case-perfect-positive-correlation-upper-bound"
        )
        meta["incomplete_control_chain_runs"] = [
            run for run in run_names if run in incomplete[key]
        ]
        result[key] = meta
    return result


def _leave_one_super_run_out_sensitivity(
    fits: Mapping[_InstructionKey, _Fit],
    response_names: Mapping[_InstructionKey, str],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
    full_estimates: Mapping[_InstructionKey, float | None],
) -> dict[_InstructionKey, dict[str, Any]]:
    """删除一个最高层 cluster 后重新拟合发布估计器。

    这是 deterministic influence analysis，不产生额外独立样本，也不把
    jackknife 标准误与 bootstrap 区间混用。每次删除后重新估计异方差权重、
    Huber 权重、run 固定效应和完整 control chain，并直接与全样本发布点估计
    比较。
    """

    super_runs = _ordered_super_runs(
        pair for fit in fits.values() for pair in fit.pairs
    )
    values: dict[_InstructionKey, list[dict[str, Any]]] = {
        key: [] for key in fits
    }
    failures: dict[_InstructionKey, set[str]] = {
        key: set() for key in fits
    }
    if len(super_runs) < 3:
        return {
            key: {
                "method": (
                    "leave-one-complete-crossover-super-run-out full Huber "
                    "heteroscedastic control-chain refit"
                ),
                "complete": False,
                "reason": "fewer-than-three-super-runs",
                "runs": len(super_runs),
                "maximum_absolute_shift_ns": None,
                "per_super_run": [],
            }
            for key in fits
        }
    for omitted in super_runs:
        contrasts: dict[_InstructionKey, float] = {}
        failed_keys: set[_InstructionKey] = set()
        for key, fit in fits.items():
            members = [
                pair for pair in fit.pairs if pair.super_run != omitted
            ]
            try:
                current = _fit_variant(
                    members,
                    response_names[key],
                    compute_condition=False,
                    compute_standard_error=False,
                    batch_levels=fit.batch_levels,
                    batch_reference=fit.batch_reference,
                )
            except MicrobenchmarkModelError:
                failed_keys.add(key)
                continue
            if not current.irls_converged:
                failed_keys.add(key)
                continue
            contrasts[key] = current.estimate
        absolute, resolution_failures = _resolve_absolute(contrasts, controls)
        for key in fits:
            point = full_estimates.get(key)
            estimate = absolute.get(key)
            if (
                key in failed_keys
                or key in resolution_failures
                or point is None
                or estimate is None
                or not math.isfinite(float(point))
                or not math.isfinite(float(estimate))
            ):
                failures[key].add(omitted)
                continue
            shift = float(estimate) - float(point)
            values[key].append(
                {
                    "omitted_super_run": omitted,
                    "ns_per_instruction": float(estimate),
                    "full_estimate_ns_per_instruction": float(point),
                    "shift_ns": shift,
                }
            )
    return {
        key: {
            "method": (
                "leave-one-complete-crossover-super-run-out full Huber "
                "heteroscedastic control-chain refit"
            ),
            "complete": not failures[key]
            and len(values[key]) == len(super_runs),
            "reason": None if not failures[key] else "one-or-more-refits-failed",
            "runs": len(super_runs),
            "full_estimate_ns_per_instruction": full_estimates.get(key),
            "maximum_absolute_shift_ns": (
                max(abs(float(row["shift_ns"])) for row in values[key])
                if values[key]
                else None
            ),
            "failed_super_runs": sorted(failures[key]),
            "per_super_run": values[key],
        }
        for key in fits
    }


def _auxiliary_run_cluster_inference(
    fits: Mapping[_InstructionKey, _Fit],
    response_names: Mapping[_InstructionKey, str],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
    comparison_modes: Mapping[_InstructionKey, str | None],
    replicate_seeds: Sequence[int],
    confidence: float,
    run_block_length: int,
    monte_carlo_confidence: float | None = None,
) -> dict[str, Any]:
    """用一次性 per-run 拟合和与主模型一致的 run 块 bootstrap 校验时钟。"""

    primary, primary_incomplete, _primary_variances = _per_run_absolute_estimates(
        fits, response_names, controls
    )
    guest_names = {key: "guest_delta_ns" for key in fits}
    plugin_off_names = {key: "plugin_off_guest_delta_ns" for key in fits}
    guest, guest_incomplete, _guest_variances = _per_run_absolute_estimates(
        fits, guest_names, controls
    )
    plugin_off, plugin_off_incomplete, _plugin_off_variances = (
        _per_run_absolute_estimates(
            fits, plugin_off_names, controls
        )
    )
    cross_difference_names = {
        key: "cross_clock_difference_ns" for key in fits
    }
    plugin_off_difference_names = {
        key: "plugin_off_difference_ns" for key in fits
    }
    cross_difference, cross_difference_incomplete, _cross_difference_variances = (
        _per_run_absolute_estimates(
            fits, cross_difference_names, controls
        )
    )
    (
        plugin_off_difference,
        plugin_off_difference_incomplete,
        _plugin_off_difference_variances,
    ) = _per_run_absolute_estimates(
        fits, plugin_off_difference_names, controls
    )
    metric_sources: dict[
        tuple[str, _InstructionKey], tuple[list[str], list[float], list[float]]
    ] = {}
    coverage: dict[_InstructionKey, dict[str, Any]] = {}
    for key, fit in fits.items():
        runs = _ordered_super_runs(fit.pairs)
        primary_complete = not primary_incomplete[key] and set(primary[key]) == set(runs)
        guest_complete = not guest_incomplete[key] and set(guest[key]) == set(runs)
        plugin_off_complete = (
            not plugin_off_incomplete[key]
            and set(plugin_off[key]) == set(runs)
        )
        cross_difference_complete = (
            not cross_difference_incomplete[key]
            and set(cross_difference[key]) == set(runs)
        )
        plugin_off_difference_complete = (
            not plugin_off_difference_incomplete[key]
            and set(plugin_off_difference[key]) == set(runs)
        )
        coverage[key] = {
            "required_runs": len(runs),
            "primary_usable_runs": len(primary[key]),
            "guest_usable_runs": len(guest[key]),
            "plugin_off_usable_runs": len(plugin_off[key]),
            "primary_complete": primary_complete,
            "guest_complete": guest_complete,
            "plugin_off_complete": plugin_off_complete,
            "cross_difference_usable_runs": len(cross_difference[key]),
            "plugin_off_difference_usable_runs": len(
                plugin_off_difference[key]
            ),
            "cross_difference_complete": cross_difference_complete,
            "plugin_off_difference_complete": (
                plugin_off_difference_complete
            ),
        }
        mode = comparison_modes.get(key)
        if mode == "difference" and cross_difference_complete:
            metric_sources[("cross-clock-difference", key)] = (
                runs,
                [0.0 for _run in runs],
                [cross_difference[key][run] for run in runs],
            )
        elif primary_complete and guest_complete and mode is not None:
            metric_sources[(f"cross-clock-{mode}", key)] = (
                runs,
                [primary[key][run] for run in runs],
                [guest[key][run] for run in runs],
            )
        if mode == "difference" and plugin_off_difference_complete:
            metric_sources[("plugin-off-difference", key)] = (
                runs,
                [0.0 for _run in runs],
                [plugin_off_difference[key][run] for run in runs],
            )
        elif guest_complete and plugin_off_complete and mode is not None:
            metric_sources[(f"plugin-off-{mode}", key)] = (
                runs,
                [plugin_off[key][run] for run in runs],
                [guest[key][run] for run in runs],
            )

    def metric_value(
        name: str, denominator: Sequence[float], numerator: Sequence[float]
    ) -> float | None:
        denominator_mean = math.fsum(denominator) / len(denominator)
        numerator_mean = math.fsum(numerator) / len(numerator)
        if name.endswith("difference"):
            return numerator_mean - denominator_mean
        if abs(denominator_mean) <= 1e-15:
            return None
        value = numerator_mean / denominator_mean
        return value if math.isfinite(value) else None

    points: dict[tuple[str, _InstructionKey], float] = {}
    for metric, (_runs, denominator, numerator) in metric_sources.items():
        value = metric_value(metric[0], denominator, numerator)
        if value is not None:
            points[metric] = value
    bootstrap_rows: list[dict[tuple[str, _InstructionKey], float]] = []
    for replicate_seed in replicate_seeds:
        row: dict[tuple[str, _InstructionKey], float] = {}
        sampled_by_runs: dict[tuple[str, ...], list[int]] = {}
        for metric, (runs, denominator, numerator) in metric_sources.items():
            run_key = tuple(runs)
            indices = sampled_by_runs.get(run_key)
            if indices is None:
                indices = _run_resample_positions(
                    len(runs),
                    run_block_length,
                    random.Random(replicate_seed),
                )
                sampled_by_runs[run_key] = indices
            value = metric_value(
                metric[0],
                [denominator[index] for index in indices],
                [numerator[index] for index in indices],
            )
            if value is not None:
                row[metric] = value
        bootstrap_rows.append(row)
    intervals, critical, valid, monte_carlo = _simultaneous_intervals(
        points,
        bootstrap_rows,
        confidence,
        monte_carlo_confidence,
    )
    return {
        "points": points,
        "intervals": intervals,
        "coverage": coverage,
        "critical_value": critical,
        "valid_replicates": valid,
        "complete_max_statistic_replicates": valid,
        "complete_family_replicates": monte_carlo["complete_family_replicates"],
        "requested_replicates": len(replicate_seeds),
        "critical_value_monte_carlo": monte_carlo,
    }


def _fit_diagnostic_effects(fit: _Fit) -> dict[str, float | None]:
    """返回 bootstrap 使用的稳定诊断键，包括局部 categorical batch 效应。"""

    process = _process_crossover_effects(fit)
    effects: dict[str, float | None] = {
        "order": fit.order_effect,
        "drift": fit.drift_effect,
        # 兼容旧消费者的端点割线，不用于 categorical batch 门禁。
        "batch": fit.batch_effect,
        "translation": fit.translation_effect,
        "process_design": process["design_abba_minus_baab"],
        "process_period": process["second_pair_minus_first_pair"],
        "process_carryover": process[
            "preceded_by_plugin_off_minus_other_timing"
        ],
    }
    for rank, level in enumerate(fit.batch_levels):
        if level != fit.batch_reference:
            effects[f"batch_level_rank:{rank}"] = (
                fit.batch_level_effects[level]
            )
    for left_rank, left in enumerate(fit.batch_levels):
        for right_rank, right in enumerate(
            fit.batch_levels[left_rank + 1 :], start=left_rank + 1
        ):
            effects[f"batch_pairwise_rank:{left_rank}:{right_rank}"] = (
                fit.batch_level_effects[right]
                - fit.batch_level_effects[left]
            )
    return effects


def _process_crossover_effects(fit: _Fit) -> dict[str, Any]:
    """从 QEMU run 固定效应构造进程级 crossover 对比。"""

    metadata: dict[str, tuple[str, int]] = {}
    for pair in fit.pairs:
        if pair.crossover_design not in {"ABBA", "BAAB"} or pair.crossover_pair not in {
            1,
            2,
        }:
            return {
                "available": False,
                "reason": "process-crossover-metadata-unavailable",
                "design_counts": {},
                "minimum_design_fraction": 0.0,
                "design_abba_minus_baab": None,
                "second_pair_minus_first_pair": None,
                "preceded_by_plugin_off_minus_other_timing": None,
            }
        current = (pair.crossover_design, pair.crossover_pair)
        previous = metadata.setdefault(pair.run, current)
        if previous != current:
            raise MicrobenchmarkModelError("同一 QEMU run 的 crossover 元数据不一致")

    by_super: dict[str, dict[int, float]] = defaultdict(dict)
    designs: dict[str, str] = {}
    run_to_super = {pair.run: pair.super_run for pair in fit.pairs}
    for run, estimate in fit.run_level_estimates.items():
        design, crossover_pair = metadata[run]
        super_run = run_to_super[run]
        previous_design = designs.setdefault(super_run, design)
        if previous_design != design or crossover_pair in by_super[super_run]:
            raise MicrobenchmarkModelError("super-run 的 crossover 固定效应不唯一")
        by_super[super_run][crossover_pair] = estimate
    if not by_super or any(set(values) != {1, 2} for values in by_super.values()):
        return {
            "available": False,
            "reason": "process-crossover-pair-coverage-incomplete",
            "design_counts": {},
            "minimum_design_fraction": 0.0,
            "design_abba_minus_baab": None,
            "second_pair_minus_first_pair": None,
            "preceded_by_plugin_off_minus_other_timing": None,
        }

    centers: dict[str, list[float]] = defaultdict(list)
    period: list[float] = []
    carryover: list[float] = []
    for super_run, pair_estimates in by_super.items():
        first = pair_estimates[1]
        second = pair_estimates[2]
        design = designs[super_run]
        centers[design].append((first + second) / 2.0)
        period.append(second - first)
        # ABBA 的第二个 timing、BAAB 的第一个 timing 紧跟 plugin-off launch。
        carryover.append(second - first if design == "ABBA" else first - second)
    counts = {name: len(values) for name, values in sorted(centers.items())}
    total = len(by_super)
    minimum_fraction = min(
        (counts.get("ABBA", 0) / total, counts.get("BAAB", 0) / total)
    )
    design_effect = (
        statistics.mean(centers["ABBA"]) - statistics.mean(centers["BAAB"])
        if centers.get("ABBA") and centers.get("BAAB")
        else None
    )
    return {
        "available": design_effect is not None,
        "reason": None if design_effect is not None else "process-crossover-design-unbalanced",
        "design_counts": counts,
        "minimum_design_fraction": minimum_fraction,
        "design_abba_minus_baab": design_effect,
        "second_pair_minus_first_pair": statistics.mean(period),
        "preceded_by_plugin_off_minus_other_timing": statistics.mean(carryover),
    }


def _resolve_diagnostic_effects(
    diagnostics: Mapping[_InstructionKey, Mapping[str, float | None]],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
) -> dict[_InstructionKey, dict[str, float]]:
    """沿 control graph 累加可比较的 nuisance contrast。"""

    resolved: dict[_InstructionKey, dict[str, float]] = {
        key: {} for key in diagnostics
    }
    names = sorted(
        {
            name
            for values in diagnostics.values()
            for name, value in values.items()
            if value is not None
        }
    )
    for name in names:
        contrasts = {
            key: float(value)
            for key, values in diagnostics.items()
            if (value := values.get(name)) is not None
        }
        if name.startswith(("batch_level_rank:", "batch_pairwise_rank:")):
            # Batch 档位是当前 target-control edge 的实际执行规模。不同 edge
            # 可以使用不同物理 batch 网格，不能沿 control graph 按 rank 或
            # 原始数值相加；control 自身的稳定性由 quality chain 独立传播。
            for key, value in contrasts.items():
                resolved[key][name] = value
            continue
        absolute, failures = _resolve_absolute(contrasts, controls)
        for key, value in absolute.items():
            if key not in failures and value is not None and math.isfinite(value):
                resolved[key][name] = float(value)
    return resolved


def _anchor_super_run_calibration(
    pairs: Sequence[_Pair], *, minimum_signal: float = 0.5
) -> dict[str, Any]:
    """从每个 super-run 的独立正锚点估计 plugin-off 到主时钟的尺度。

    这里使用同一 anchor contrast 的组内中位数比，而不使用任何目标指令
    响应。最高层 bootstrap 会重新采样 anchor block 并再次调用本函数，
    因而把分母测量误差和宿主速度变化一并传播到 adjusted 权重。
    """

    anchor_keys = {
        pair.key for pair in pairs if pair.key.pattern == STABILITY_ANCHOR_PATTERN
    }
    if len(anchor_keys) != 1:
        return {
            "status": "unavailable",
            "reason": "requires-exactly-one-positive-stability-anchor",
            "anchor_key": None,
            "scales": {},
            "metrics": {},
            "per_super_run": [],
        }
    anchor_key = next(iter(anchor_keys))
    expected_runs = _ordered_super_runs(pairs)
    anchor_pairs = [pair for pair in pairs if pair.key == anchor_key]
    scales: dict[str, float] = {}
    rows: list[dict[str, Any]] = []
    metric_values: dict[str, list[float]] = defaultdict(list)
    for super_run in expected_runs:
        members = [
            pair for pair in anchor_pairs if pair.super_run == super_run
        ]
        if len(members) < 4 or any(
            pair.plugin_delta_ns is None
            or pair.guest_delta_ns is None
            or pair.plugin_off_guest_delta_ns is None
            for pair in members
        ):
            return {
                "status": "unavailable",
                "reason": "anchor-super-run-coverage-incomplete",
                "anchor_key": anchor_key.public(),
                "scales": {},
                "metrics": {},
                "per_super_run": rows,
            }
        positions = {pair.anchor_position for pair in members}
        if positions != {"head", "body", "tail"}:
            return {
                "status": "unavailable",
                "reason": "anchor-position-coverage-incomplete",
                "anchor_key": anchor_key.public(),
                "scales": {},
                "metrics": {},
                "per_super_run": rows,
            }
        body_batches = sorted(
            {pair.batch for pair in members if pair.anchor_position == "body"}
        )
        if not body_batches:
            return {
                "status": "unavailable",
                "reason": "anchor-body-batch-coverage-incomplete",
                "anchor_key": anchor_key.public(),
                "scales": {},
                "metrics": {},
                "per_super_run": rows,
            }
        reference_batch = min(
            body_batches,
            key=lambda value: (
                abs(
                    math.log(value)
                    - statistics.median(math.log(item) for item in body_batches)
                ),
                value,
            ),
        )
        strata: dict[tuple[str, int], list[_Pair]] = defaultdict(list)
        for pair in members:
            if pair.anchor_position is not None:
                strata[(pair.anchor_position, pair.batch)].append(pair)

        def response_median(name: str, subset: Sequence[_Pair]) -> float:
            return statistics.median(
                float(getattr(pair, name)) / pair.target_count for pair in subset
            )

        stratum_metrics: dict[tuple[str, int], dict[str, float]] = {}
        for stratum, subset in strata.items():
            primary_value = response_median("plugin_delta_ns", subset)
            guest_value = response_median("guest_delta_ns", subset)
            plugin_off_value = response_median("plugin_off_guest_delta_ns", subset)
            if min(primary_value, guest_value, plugin_off_value) <= minimum_signal:
                return {
                    "status": "unavailable",
                    "reason": "anchor-signal-below-positive-floor",
                    "anchor_key": anchor_key.public(),
                    "scales": {},
                    "metrics": {},
                    "per_super_run": rows,
                }
            stratum_metrics[stratum] = {
                "primary_signal": primary_value,
                "guest_signal": guest_value,
                "plugin_off_signal": plugin_off_value,
                "guest_to_primary_scale": primary_value / guest_value,
                "plugin_off_to_guest_scale": guest_value / plugin_off_value,
                "plugin_off_to_primary_scale": primary_value / plugin_off_value,
            }
        reference = stratum_metrics.get((ANCHOR_REFERENCE_POSITION, reference_batch))
        if reference is None:
            return {
                "status": "unavailable",
                "reason": "anchor-reference-stratum-missing",
                "anchor_key": anchor_key.public(),
                "scales": {},
                "metrics": {},
                "per_super_run": rows,
            }
        # 尺度点估计固定使用主体中档，避免 head/tail 和不同 batch 的混合
        # 比例把位置/批次漂移吸收到校正因子中。其余层级只作 nuisance 门禁。
        primary = reference["primary_signal"]
        guest = reference["guest_signal"]
        plugin_off = reference["plugin_off_signal"]
        if min(primary, guest, plugin_off) <= minimum_signal:
            return {
                "status": "unavailable",
                "reason": "anchor-signal-below-positive-floor",
                "anchor_key": anchor_key.public(),
                "scales": {},
                "metrics": {
                    "primary_signal": primary,
                    "guest_signal": guest,
                    "plugin_off_signal": plugin_off,
                },
                "per_super_run": rows,
            }
        metrics = {
            "primary_signal": primary,
            "guest_signal": guest,
            "plugin_off_signal": plugin_off,
            "guest_to_primary_scale": primary / guest,
            "plugin_off_to_guest_scale": guest / plugin_off,
            "plugin_off_to_primary_scale": primary / plugin_off,
        }
        for position in ("head", "tail"):
            position_value = stratum_metrics.get((position, reference_batch))
            if position_value is None:
                return {
                    "status": "unavailable",
                    "reason": "anchor-position-reference-batch-missing",
                    "anchor_key": anchor_key.public(),
                    "scales": {},
                    "metrics": {},
                    "per_super_run": rows,
                }
            metrics[f"position_log_scale:{position}"] = math.log(
                position_value["plugin_off_to_primary_scale"]
                / reference["plugin_off_to_primary_scale"]
            )
        for batch in body_batches:
            if batch == reference_batch:
                continue
            metrics[f"batch_log_scale:{batch}"] = math.log(
                stratum_metrics[(ANCHOR_REFERENCE_POSITION, batch)][
                    "plugin_off_to_primary_scale"
                ]
                / reference["plugin_off_to_primary_scale"]
            )
        if any(not math.isfinite(value) for value in metrics.values()) or any(
            value <= 0.0
            for name, value in metrics.items()
            if not name.startswith(("position_log_scale:", "batch_log_scale:"))
        ):
            return {
                "status": "unavailable",
                "reason": "anchor-scale-not-positive-finite",
                "anchor_key": anchor_key.public(),
                "scales": {},
                "metrics": {},
                "per_super_run": rows,
            }
        scales[super_run] = metrics["plugin_off_to_primary_scale"]
        for name, value in metrics.items():
            metric_values[name].append(value)
        rows.append(
            {
                "super_run": super_run,
                "pairs": len(members),
                "positions": sorted(str(value) for value in positions),
                "reference_batch": reference_batch,
                "body_batches": body_batches,
                **metrics,
            }
        )
    return {
        "status": "available",
        "reason": None,
        "anchor_key": anchor_key.public(),
        "minimum_signal_ns_per_instruction": minimum_signal,
        "scales": scales,
        "metrics": {
            name: statistics.median(values)
            for name, values in metric_values.items()
        },
        "per_super_run": rows,
        "estimator": (
            "within-super-run stratified median contrast ratio with body-middle "
            "reference; target-independent errors-in-variables nuisance calibration"
        ),
    }


def _anchor_adjusted_absolute_estimates(
    pairs: Sequence[_Pair],
    keys: Sequence[_InstructionKey],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
    batch_levels: Mapping[_InstructionKey, tuple[int, ...]],
    batch_references: Mapping[_InstructionKey, int | None],
) -> tuple[dict[_InstructionKey, float], dict[str, Any]]:
    calibration = _anchor_super_run_calibration(pairs)
    if calibration["status"] != "available":
        return {}, calibration
    scales = calibration["scales"]
    transformed: dict[_InstructionKey, list[_Pair]] = defaultdict(list)
    for pair in pairs:
        if pair.plugin_off_guest_delta_ns is None or pair.super_run not in scales:
            return {}, {
                **calibration,
                "status": "unavailable",
                "reason": "plugin-off-response-incomplete-for-adjustment",
            }
        transformed[pair.key].append(
            replace(
                pair,
                plugin_delta_ns=(
                    pair.plugin_off_guest_delta_ns * scales[pair.super_run]
                ),
            )
        )
    contrasts: dict[_InstructionKey, float] = {}
    for key in keys:
        try:
            fit = _fit_variant(
                transformed.get(key, []),
                "plugin_delta_ns",
                compute_condition=False,
                compute_standard_error=False,
                batch_levels=batch_levels[key],
                batch_reference=batch_references[key],
            )
        except MicrobenchmarkModelError:
            return {}, {
                **calibration,
                "status": "unavailable",
                "reason": "anchor-adjusted-fit-failed",
            }
        contrasts[key] = fit.estimate
    absolute, failures = _resolve_absolute(contrasts, controls)
    resolved = {
        key: float(value)
        for key, value in absolute.items()
        if key not in failures and value is not None and math.isfinite(value)
    }
    if len(resolved) != len(keys):
        return {}, {
            **calibration,
            "status": "unavailable",
            "reason": "anchor-adjusted-control-chain-incomplete",
        }
    return resolved, calibration


def _run_bootstrap_replicate(
    state: _BootstrapState, replicate_seed: int
) -> tuple[
    dict[_InstructionKey, float],
    dict[_InstructionKey, dict[str, float | None]],
] | None:
    """运行一个可独立并行的分层 moving-block bootstrap replicate。"""

    run_count = len(_ordered_super_runs(state.pairs))
    rng = random.Random(replicate_seed)
    run_positions = _run_resample_positions(
        run_count, state.run_block_length, rng
    )
    resampled = _hierarchical_resample(
        state.pairs,
        state.block_length,
        rng,
        run_block_length=state.run_block_length,
        run_positions=run_positions,
    )
    by_key: dict[_InstructionKey, list[_Pair]] = defaultdict(list)
    for pair in resampled:
        by_key[pair.key].append(pair)
    contrasts: dict[_InstructionKey, float] = {}
    contrast_diagnostics: dict[_InstructionKey, dict[str, float | None]] = {}
    classical_contrasts: dict[_InstructionKey, float] = {}
    for key in state.keys:
        response_name = state.response_names[key]
        members = [
            pair
            for pair in by_key.get(key, [])
            if getattr(pair, response_name) is not None
        ]
        try:
            current = _fit_variant(
                members,
                response_name,
                compute_condition=False,
                compute_standard_error=False,
                batch_levels=state.batch_levels[key],
                batch_reference=state.batch_references[key],
            )
        except MicrobenchmarkModelError:
            return None
        contrasts[key] = current.estimate
        contrast_diagnostics[key] = _fit_diagnostic_effects(current)
        try:
            classical_contrasts[key] = _classical_variant_estimate(
                current.pairs,
                response_name,
                batch_levels=state.batch_levels[key],
                batch_reference=state.batch_references[key],
                heteroscedastic_weights=current.hetero_weights,
            )
        except MicrobenchmarkModelError:
            # 主 bootstrap 仍可作为计时分布；缺失的敏感性对照会在发布
            # 门禁中按 key 明确标记为 inconclusive。
            pass
    absolute, _failures = _resolve_absolute(contrasts, state.controls)
    classical_absolute, _classical_failures = _resolve_absolute(
        classical_contrasts, state.controls
    )
    resolved = {
        key: float(value)
        for key, value in absolute.items()
        if value is not None
    }
    for key, value in classical_absolute.items():
        if key in resolved and value is not None and math.isfinite(value):
            resolved[("estimator-sensitivity", key)] = float(value - resolved[key])
    diagnostics = _resolve_diagnostic_effects(
        contrast_diagnostics, state.controls
    )
    adjusted, calibration = _anchor_adjusted_absolute_estimates(
        resampled,
        state.keys,
        state.controls,
        state.batch_levels,
        state.batch_references,
    )
    if calibration["status"] == "available" and len(adjusted) == len(absolute):
        for key, value in adjusted.items():
            resolved[("anchor-adjusted", key)] = value
            if key in resolved:
                resolved[("raw-adjusted-discrepancy", key)] = value - resolved[key]
        for name, value in calibration["metrics"].items():
            resolved[("anchor-metric", name)] = value
    return (resolved, diagnostics) if resolved else None


def _initialize_bootstrap_worker(state: _BootstrapState) -> None:
    global _ACTIVE_LINEAR_ALGEBRA_BACKEND, _BOOTSTRAP_STATE
    _BOOTSTRAP_STATE = state
    _ACTIVE_LINEAR_ALGEBRA_BACKEND = state.linear_algebra_backend


def _bootstrap_worker(replicate_seed: int):
    if _BOOTSTRAP_STATE is None:
        raise RuntimeError("bootstrap worker 尚未初始化")
    return _run_bootstrap_replicate(_BOOTSTRAP_STATE, replicate_seed)


def _quality_status(failures: Sequence[str], fatal: Sequence[str]) -> str:
    if any(item in fatal for item in failures):
        return "not-identifiable"
    if not failures:
        return "high-confidence"
    return "low-confidence"


def fit_microbenchmark_weight_model(
    samples: Sequence[Any],
    *,
    bootstrap_replicates: int = 999,
    bootstrap_jobs: int = 1,
    confidence: float = PUBLICATION_CONFIDENCE,
    seed: int = PUBLICATION_BOOTSTRAP_SEED,
    block_length: int | None = None,
    run_block_length: int | None = None,
    min_pairs: int = PUBLICATION_MIN_PAIRS,
    min_effective_pairs: float = PUBLICATION_MIN_EFFECTIVE_PAIRS,
    min_runs: int = PUBLICATION_MIN_SUPER_RUNS,
    min_count_levels: int = PUBLICATION_MIN_COUNT_LEVELS,
    min_purity: float = PUBLICATION_MIN_PURITY,
    max_relative_ci_half_width: float = PUBLICATION_MAX_RELATIVE_CI_HALF_WIDTH,
    max_i_squared: float = PUBLICATION_MAX_I_SQUARED,
    equivalence_margin: float = PUBLICATION_EQUIVALENCE_MARGIN,
    min_cross_clock_ratio: float = PUBLICATION_MIN_CROSS_CLOCK_RATIO,
    max_cross_clock_ratio: float = PUBLICATION_MAX_CROSS_CLOCK_RATIO,
    min_plugin_off_ratio: float = PUBLICATION_MIN_PLUGIN_OFF_RATIO,
    max_plugin_off_ratio: float = PUBLICATION_MAX_PLUGIN_OFF_RATIO,
    max_zero_cost_ci_upper_ns: float = PUBLICATION_MAX_ZERO_COST_CI_UPPER_NS,
    max_translation_density: float = PUBLICATION_MAX_TRANSLATION_DENSITY,
    max_translation_excluded_pair_fraction: float = (
        MAX_TRANSLATION_EXCLUDED_PAIR_FRACTION
    ),
    max_severe_outlier_fraction: float = PUBLICATION_MAX_SEVERE_OUTLIER_FRACTION,
    linear_algebra_backend: str = "auto",
) -> dict[str, Any]:
    """拟合逐完整编码键权重并给出 FWER 同时区间。"""

    if (
        isinstance(bootstrap_replicates, bool)
        or not isinstance(bootstrap_replicates, int)
        or bootstrap_replicates < 0
    ):
        raise MicrobenchmarkModelError("bootstrap_replicates 必须是非负整数")
    if (
        isinstance(bootstrap_jobs, bool)
        or not isinstance(bootstrap_jobs, int)
        or bootstrap_jobs <= 0
    ):
        raise MicrobenchmarkModelError("bootstrap_jobs 必须是正整数")
    if not 0.0 < confidence < 1.0:
        raise MicrobenchmarkModelError("confidence 必须位于 (0,1)")
    overall_alpha = 1.0 - confidence
    sampling_alpha = overall_alpha * PUBLICATION_SAMPLING_ALPHA_FRACTION
    monte_carlo_alpha = (
        overall_alpha * PUBLICATION_MONTE_CARLO_ALPHA_FRACTION
    )
    family_sampling_alpha = sampling_alpha / len(
        PUBLICATION_INFERENCE_FAMILIES
    )
    family_confidence = 1.0 - family_sampling_alpha
    family_monte_carlo_alpha = monte_carlo_alpha / len(
        PUBLICATION_INFERENCE_FAMILIES
    )
    family_monte_carlo_confidence = 1.0 - family_monte_carlo_alpha
    if block_length is not None and (
        isinstance(block_length, bool) or not isinstance(block_length, int) or block_length <= 0
    ):
        raise MicrobenchmarkModelError("block_length 必须是正整数")
    if run_block_length is not None and (
        isinstance(run_block_length, bool)
        or not isinstance(run_block_length, int)
        or run_block_length <= 0
    ):
        raise MicrobenchmarkModelError("run_block_length 必须是正整数")
    if not (
        0.0 < min_cross_clock_ratio <= 1.0 <= max_cross_clock_ratio
        and min_cross_clock_ratio < max_cross_clock_ratio
    ):
        raise MicrobenchmarkModelError("交叉时钟比值范围必须跨越 1 且为正")
    if not (
        0.0 < min_plugin_off_ratio <= 1.0 <= max_plugin_off_ratio
        and min_plugin_off_ratio < max_plugin_off_ratio
    ):
        raise MicrobenchmarkModelError("plugin-off 比值范围必须跨越 1 且为正")
    if not math.isfinite(max_zero_cost_ci_upper_ns) or max_zero_cost_ci_upper_ns <= 0:
        raise MicrobenchmarkModelError("零成本置信上界必须为正有限数")
    if not math.isfinite(max_translation_density) or not 0 <= max_translation_density < 1:
        raise MicrobenchmarkModelError("翻译事件密度阈值必须位于 [0,1)")
    if (
        not math.isfinite(max_translation_excluded_pair_fraction)
        or not 0.0 < max_translation_excluded_pair_fraction < 1.0
    ):
        raise MicrobenchmarkModelError("翻译污染 pair 比例阈值必须位于 (0,1)")
    if (
        not math.isfinite(max_severe_outlier_fraction)
        or not 0.0 < max_severe_outlier_fraction < 0.5
    ):
        raise MicrobenchmarkModelError("严重异常比例阈值必须位于 (0,0.5)")
    global _ACTIVE_LINEAR_ALGEBRA_BACKEND
    selected_backend = _linear_algebra_backend(linear_algebra_backend)
    _ACTIVE_LINEAR_ALGEBRA_BACKEND = selected_backend
    all_pairs, assumed_empty = _pair_samples(samples)
    translation_contaminated = [
        pair
        for pair in all_pairs
        if pair.translation_observed and not pair.translation_free
    ]
    pairs = [
        pair
        for pair in all_pairs
        if not pair.translation_observed or pair.translation_free
    ]
    if not pairs:
        raise MicrobenchmarkModelError(
            "剔除发生 QEMU translation 的 pair 后没有可拟合数据"
        )
    excluded_by_key: dict[_InstructionKey, int] = defaultdict(int)
    for pair in translation_contaminated:
        excluded_by_key[pair.key] += 1
    translation_exclusion_inference: dict[_InstructionKey, dict[str, Any]] = {}
    all_pairs_by_key: dict[_InstructionKey, list[_Pair]] = defaultdict(list)
    for pair in all_pairs:
        all_pairs_by_key[pair.key].append(pair)
    for key, members in all_pairs_by_key.items():
        translation_exclusion_inference[key] = _run_cluster_proportion_upper_bound(
            [pair.translation_observed and not pair.translation_free for pair in members],
            [pair.super_run for pair in members],
            confidence,
        )
    grouped: dict[_InstructionKey, list[_Pair]] = defaultdict(list)
    raw_controls: dict[_InstructionKey, _ControlReference | None] = {}
    for pair in pairs:
        grouped[pair.key].append(pair)
        if (
            pair.key in raw_controls
            and raw_controls[pair.key] != pair.control_reference
        ):
            raise MicrobenchmarkModelError(
                f"指令变体 {pair.key!r} 使用了多个 control reference"
            )
        raw_controls[pair.key] = pair.control_reference
    controls, control_resolution_failures = _resolve_control_references(
        list(grouped), raw_controls
    )

    fits: dict[_InstructionKey, _Fit] = {}
    response_sources: dict[_InstructionKey, str] = {}
    response_names: dict[_InstructionKey, str] = {}
    fit_failures: dict[_InstructionKey, str] = {}
    guest_fits: dict[_InstructionKey, _Fit] = {}
    plugin_off_guest_fits: dict[_InstructionKey, _Fit] = {}
    for key, members in grouped.items():
        plugin_members = [pair for pair in members if pair.plugin_delta_ns is not None]
        guest_members = [pair for pair in members if pair.guest_delta_ns is not None]
        selected = plugin_members if len(plugin_members) >= 4 else guest_members
        response_name = "plugin_delta_ns" if selected is plugin_members else "guest_delta_ns"
        response_names[key] = response_name
        if response_name == "plugin_delta_ns":
            response_sources[key] = (
                "qemu-vcpu-thread-cpu-time-marker-only"
                if all(pair.marker_only_timing for pair in selected)
                else "qemu-vcpu-thread-cpu-time-instrumented-or-unknown"
            )
        else:
            response_sources[key] = "guest-time-fallback"
        try:
            fits[key] = _fit_variant(selected, response_name)
        except MicrobenchmarkModelError as error:
            fit_failures[key] = str(error)
            continue
        # 辅助响应必须与主响应使用完全相同的一批 pair，不能以缺失后的
        # 便利子集制造看似一致的点估计。
        if all(pair.guest_delta_ns is not None for pair in fits[key].pairs):
            try:
                guest_fits[key] = _fit_variant(fits[key].pairs, "guest_delta_ns")
            except MicrobenchmarkModelError:
                pass
        if all(
            pair.plugin_off_guest_delta_ns is not None for pair in fits[key].pairs
        ):
            try:
                plugin_off_guest_fits[key] = _fit_variant(
                    fits[key].pairs, "plugin_off_guest_delta_ns"
                )
            except MicrobenchmarkModelError:
                pass
    if not fits:
        raise MicrobenchmarkModelError("没有任何指令变体可完成拟合")

    point_contrasts = {key: fit.estimate for key, fit in fits.items()}
    point_absolute, absolute_failures = _resolve_absolute(point_contrasts, controls)
    absolute_failures.update(control_resolution_failures)
    classical_contrasts: dict[_InstructionKey, float] = {}
    classical_fit_failures: dict[_InstructionKey, str] = {}
    for key, fit in fits.items():
        try:
            classical_contrasts[key] = _classical_variant_estimate(
                fit.pairs,
                response_names[key],
                batch_levels=fit.batch_levels,
                batch_reference=fit.batch_reference,
                heteroscedastic_weights=fit.hetero_weights,
            )
        except MicrobenchmarkModelError as error:
            classical_fit_failures[key] = str(error)
    classical_absolute, classical_absolute_failures = _resolve_absolute(
        classical_contrasts, controls
    )
    estimator_sensitivity_points = {
        key: float(classical_absolute[key] - point_absolute[key])
        for key in fits
        if key in point_absolute
        and key in classical_absolute
        and point_absolute[key] is not None
        and classical_absolute[key] is not None
        and math.isfinite(float(point_absolute[key]))
        and math.isfinite(float(classical_absolute[key]))
    }
    guest_contrasts = {key: fit.estimate for key, fit in guest_fits.items()}
    guest_absolute, _guest_absolute_failures = _resolve_absolute(
        guest_contrasts, controls
    )
    plugin_off_guest_contrasts = {
        key: fit.estimate for key, fit in plugin_off_guest_fits.items()
    }
    plugin_off_guest_absolute, _plugin_off_absolute_failures = _resolve_absolute(
        plugin_off_guest_contrasts, controls
    )

    ess_by_key: dict[_InstructionKey, tuple[float, list[dict[str, Any]], int]] = {
        key: _acf_ess(fit) for key, fit in fits.items()
    }
    automatic_block = max(value[2] for value in ess_by_key.values())
    selected_block = block_length or automatic_block
    run_count = len(_ordered_super_runs(pairs))
    selected_run_block = run_block_length or max(
        1, int(round(run_count ** (1.0 / 3.0)))
    )
    run_order_sources = {pair.run_order_source for pair in pairs}
    run_order_source = (
        next(iter(run_order_sources))
        if len(run_order_sources) == 1
        else "mixed"
    )
    bootstrap_rows: list[dict[Any, float]] = []
    diagnostic_rows: list[dict[tuple[_InstructionKey, str], float]] = []
    seed_rng = random.Random(seed)
    replicate_seeds = [
        seed_rng.getrandbits(64) for _ in range(bootstrap_replicates)
    ]
    bootstrap_state = _BootstrapState(
        pairs=tuple(pairs),
        keys=tuple(fits),
        response_names=response_names,
        controls=controls,
        batch_levels={key: fit.batch_levels for key, fit in fits.items()},
        batch_references={
            key: fit.batch_reference for key, fit in fits.items()
        },
        block_length=selected_block,
        run_block_length=selected_run_block,
        linear_algebra_backend=selected_backend,
    )
    if bootstrap_jobs == 1 or bootstrap_replicates == 0:
        replicate_results = (
            _run_bootstrap_replicate(bootstrap_state, replicate_seed)
            for replicate_seed in replicate_seeds
        )
        executor = None
    else:
        executor = concurrent.futures.ProcessPoolExecutor(
            max_workers=bootstrap_jobs,
            initializer=_initialize_bootstrap_worker,
            initargs=(bootstrap_state,),
        )
        replicate_results = executor.map(
            _bootstrap_worker, replicate_seeds, chunksize=1
        )
    try:
        for result in replicate_results:
            if result is None:
                continue
            resolved_row, diagnostics = result
            bootstrap_rows.append(resolved_row)
            diagnostic_row: dict[tuple[_InstructionKey, str], float] = {}
            for key, values in diagnostics.items():
                # 兼容测试/外部 monkeypatch 仍返回旧四元组的情况；模型自身
                # 始终返回带逐 batch 档位键的 mapping。
                named_values = (
                    values.items()
                    if isinstance(values, Mapping)
                    else zip(
                        ("order", "drift", "batch", "translation"),
                        values,
                    )
                )
                for name, value in named_values:
                    if value is not None:
                        diagnostic_row[(key, name)] = value
            diagnostic_rows.append(diagnostic_row)
    finally:
        if executor is not None:
            executor.shutdown(wait=True, cancel_futures=True)

    alpha = 1.0 - confidence
    point_ci: dict[_InstructionKey, list[float] | None] = {}
    for key in fits:
        values = [row[key] for row in bootstrap_rows if key in row]
        low = _quantile(values, alpha / 2.0)
        high = _quantile(values, 1.0 - alpha / 2.0)
        point_ci[key] = None if low is None or high is None else [low, high]
    finite_points = {
        key: float(value)
        for key, value in point_absolute.items()
        if value is not None and math.isfinite(value)
    }
    (
        simultaneous_ci,
        critical,
        simultaneous_valid,
        simultaneous_monte_carlo,
    ) = _simultaneous_intervals(
        finite_points,
        bootstrap_rows,
        family_confidence,
        family_monte_carlo_confidence,
    )
    for key in fits:
        simultaneous_ci.setdefault(key, None)

    contrast_diagnostic_points = {
        key: _fit_diagnostic_effects(fit) for key, fit in fits.items()
    }
    absolute_diagnostic_points = _resolve_diagnostic_effects(
        contrast_diagnostic_points, controls
    )
    diagnostic_points: dict[tuple[_InstructionKey, str], float] = {
        (key, name): value
        for key, values in absolute_diagnostic_points.items()
        for name, value in values.items()
    }
    diagnostic_complete_rows = sum(
        set(diagnostic_points).issubset(row) for row in diagnostic_rows
    )
    (
        diagnostic_intervals,
        diagnostic_critical,
        diagnostic_valid,
        diagnostic_monte_carlo,
    ) = (
        _simultaneous_intervals(
            diagnostic_points,
            diagnostic_rows,
            family_confidence,
            family_monte_carlo_confidence,
        )
    )

    bootstrap_valid_fraction = (
        len(bootstrap_rows) / bootstrap_replicates
        if bootstrap_replicates > 0
        else 0.0
    )
    auxiliary_inference = _auxiliary_run_cluster_inference(
        fits,
        response_names,
        controls,
        {
            key: (
                "difference"
                if interval is not None
                and interval[0] >= -max_zero_cost_ci_upper_ns
                and interval[1] <= max_zero_cost_ci_upper_ns
                else "ratio"
                if point_absolute.get(key) is not None
                and float(point_absolute[key]) > 0.0
                else None
            )
            for key, interval in simultaneous_ci.items()
        },
        replicate_seeds,
        family_confidence,
        selected_run_block,
        family_monte_carlo_confidence,
    )
    adjusted_points, anchor_calibration = _anchor_adjusted_absolute_estimates(
        pairs,
        tuple(fits),
        controls,
        {key: fit.batch_levels for key, fit in fits.items()},
        {key: fit.batch_reference for key, fit in fits.items()},
    )
    joint_points: dict[Any, float] = {
        ("raw", key): value for key, value in finite_points.items()
    }
    for key, value in estimator_sensitivity_points.items():
        joint_points[("estimator-sensitivity", key)] = value
    if anchor_calibration["status"] == "available":
        for key, value in adjusted_points.items():
            joint_points[("anchor-adjusted", key)] = value
            if key in finite_points:
                joint_points[("raw-adjusted-discrepancy", key)] = (
                    value - finite_points[key]
                )
        for name, value in anchor_calibration["metrics"].items():
            joint_points[("anchor-metric", name)] = value
    joint_rows: list[dict[Any, float]] = []
    for row in bootstrap_rows:
        joint_row: dict[Any, float] = {
            ("raw", key): row[key]
            for key in finite_points
            if key in row
        }
        for key in adjusted_points:
            adjusted_key = ("anchor-adjusted", key)
            discrepancy_key = ("raw-adjusted-discrepancy", key)
            if adjusted_key in row:
                joint_row[adjusted_key] = row[adjusted_key]
            if discrepancy_key in row:
                joint_row[discrepancy_key] = row[discrepancy_key]
        for key in estimator_sensitivity_points:
            sensitivity_key = ("estimator-sensitivity", key)
            if sensitivity_key in row:
                joint_row[sensitivity_key] = row[sensitivity_key]
        for name in anchor_calibration.get("metrics", {}):
            metric_key = ("anchor-metric", name)
            if metric_key in row:
                joint_row[metric_key] = row[metric_key]
        joint_rows.append(joint_row)
    joint_intervals, joint_critical, joint_valid, joint_monte_carlo = (
        _simultaneous_intervals(
            joint_points,
            joint_rows,
            family_confidence,
            family_monte_carlo_confidence,
        )
    )
    joint_complete_rows = sum(
        set(row) == set(joint_points) for row in joint_rows
    )
    anchor_metric_intervals = {
        name: joint_intervals.get(("anchor-metric", name))
        for name in anchor_calibration.get("metrics", {})
    }
    estimator_sensitivity_intervals = {
        key: joint_intervals.get(("estimator-sensitivity", key))
        for key in estimator_sensitivity_points
    }
    scale_interval_names = (
        "guest_to_primary_scale",
        "plugin_off_to_guest_scale",
        "plugin_off_to_primary_scale",
    )
    anchor_interval_ok = all(
        anchor_metric_intervals.get(name) is not None
        and anchor_metric_intervals[name][0] > 0.0
        and anchor_metric_intervals[name][1]
        / anchor_metric_intervals[name][0]
        <= ANCHOR_MAX_SCALE_RATIO
        for name in scale_interval_names
    )
    anchor_signal_interval_ok = all(
        interval is not None
        and interval[0] > 0.0
        for name, interval in anchor_metric_intervals.items()
        if name.endswith("_signal")
    )
    anchor_nuisance_interval_names = sorted(
        name
        for name in anchor_metric_intervals
        if name.startswith(("position_log_scale:", "batch_log_scale:"))
    )
    anchor_log_equivalence_margin = math.log(ANCHOR_MAX_SCALE_RATIO)
    anchor_nuisance_interval_ok = bool(anchor_nuisance_interval_names) and all(
        anchor_metric_intervals[name] is not None
        and anchor_metric_intervals[name][0] >= -anchor_log_equivalence_margin
        and anchor_metric_intervals[name][1] <= anchor_log_equivalence_margin
        for name in anchor_nuisance_interval_names
    )
    anchor_accepted = (
        anchor_calibration["status"] == "available"
        and bootstrap_replicates >= PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES
        and joint_complete_rows == bootstrap_replicates
        and joint_monte_carlo["finite_rank_supported"]
        and anchor_interval_ok
        and anchor_signal_interval_ok
        and anchor_nuisance_interval_ok
    )
    anchor_scale_inference = {
        **anchor_calibration,
        "status": "accepted" if anchor_accepted else (
            "inconclusive"
            if anchor_calibration["status"] == "available"
            else anchor_calibration["status"]
        ),
        "reason": None if anchor_accepted else (
            anchor_calibration.get("reason")
            or "anchor-joint-simultaneous-inference-inconclusive"
        ),
        "simultaneous_intervals": anchor_metric_intervals,
        "maximum_interval_ratio": ANCHOR_MAX_SCALE_RATIO,
        "interval_ratio_applies_to": list(scale_interval_names),
        "nuisance_log_scale_intervals": {
            name: anchor_metric_intervals[name]
            for name in anchor_nuisance_interval_names
        },
        "nuisance_log_equivalence_margin": anchor_log_equivalence_margin,
        "nuisance_interval_gate_passed": anchor_nuisance_interval_ok,
        "nuisance_estimands": (
            "head/tail and body-batch plugin-off-to-primary log scale relative "
            "to the body middle-batch stratum"
        ),
        "requested_replicates": bootstrap_replicates,
        "complete_joint_replicates": joint_complete_rows,
        "joint_critical_value": joint_critical,
        "joint_max_statistic_replicates": joint_valid,
        "joint_critical_value_monte_carlo": joint_monte_carlo,
        "calibration_policy": (
            "same-super-run target-independent EIV scale re-estimated inside "
            "each hierarchical bootstrap replicate"
        ),
    }

    contrast_heterogeneity = {
        key: _random_effects(
            fit,
            response_names[key],
            confidence,
        )
        for key, fit in fits.items()
    }
    per_run_absolute_data = _per_run_absolute_estimates(
        fits, response_names, controls
    )
    heterogeneity = _absolute_random_effects(
        fits,
        response_names,
        controls,
        confidence,
        per_run_data=per_run_absolute_data,
    )
    leave_one_run_out = _leave_one_super_run_out_sensitivity(
        fits,
        response_names,
        controls,
        point_absolute,
    )
    for key, meta in heterogeneity.items():
        contrast_meta = contrast_heterogeneity[key]
        contrast_meta["may_support_absolute_high_confidence"] = False
        meta["local_contrast_only"] = contrast_meta
    per_run_design = {
        key: _per_run_design_diagnostics(fit, response_names[key])
        for key, fit in fits.items()
    }
    items: list[dict[str, Any]] = []
    item_by_key: dict[_InstructionKey, dict[str, Any]] = {}
    fatal_codes = {
        "fit-failed",
        "absolute-reference-unresolved",
        "simultaneous-ci-missing",
    }
    for key in sorted(grouped):
        fit = fits.get(key)
        if fit is None:
            failed_item = {
                    "key": key.public(),
                    "ns_per_instruction": None,
                    "simultaneous_ci": None,
                    "point_ci": None,
                    "ESS": 0.0,
                    "runs": len(_ordered_super_runs(grouped[key])),
                    "qemu_runs": len({pair.run for pair in grouped[key]}),
                    "pairs": len(grouped[key]),
                    "identifiability": "not-identifiable",
                    "quality": "not-identifiable",
                    "source": "unfitted-paired-probe",
                    "quality_failures": ["fit-failed", fit_failures.get(key, "unknown")],
                }
            items.append(failed_item)
            item_by_key[key] = failed_item
            continue
        members = fit.pairs
        raw_point = point_absolute.get(key)
        raw_interval = joint_intervals.get(("raw", key))
        adjusted_raw_point = adjusted_points.get(key)
        adjusted_interval = joint_intervals.get(("anchor-adjusted", key))
        discrepancy_point = (
            None
            if raw_point is None or adjusted_raw_point is None
            else adjusted_raw_point - raw_point
        )
        discrepancy_interval = joint_intervals.get(
            ("raw-adjusted-discrepancy", key)
        )
        zero_cost_equivalent = (
            raw_interval is not None
            and raw_interval[0] >= -max_zero_cost_ci_upper_ns
            and raw_interval[1] <= max_zero_cost_ci_upper_ns
        )
        if raw_point is None:
            point = None
        elif zero_cost_equivalent:
            point = 0.0
        elif raw_point >= 0.0:
            point = raw_point
        else:
            # 物理权重非负，但不能把显著负的无约束结果静默投影成 0。
            point = None
        interval = raw_interval
        relative_half = None
        runs = len(_ordered_super_runs(members))
        qemu_runs = len({pair.run for pair in members})
        count_levels = len({pair.batch for pair in members})
        per_level = [
            sum(pair.batch == level for pair in members)
            for level in {pair.batch for pair in members}
        ]
        ess, autocorrelation, _ = ess_by_key[key]
        purities = [pair.purity for pair in members if pair.purity is not None]
        purity_q05 = _quantile(purities, 0.05)
        translation_density_q95 = 0.0 if all(
            pair.translation_observed for pair in members
        ) else None
        translation_exclusion = translation_exclusion_inference[key]
        translation_exclusion_upper = float(translation_exclusion["upper"])
        huber_downweighted = sum(
            weight < 1.0 - 1e-12 for weight in fit.robust_weights
        ) / len(members)
        severe_outliers = sum(
            weight < 0.25 for weight in fit.robust_weights
        ) / len(members)
        severe_outlier_count = sum(
            weight < 0.25 for weight in fit.robust_weights
        )
        severe_outlier_cluster = _run_cluster_proportion_upper_bound(
            [weight < 0.25 for weight in fit.robust_weights],
            [pair.super_run for pair in members],
            confidence,
        )
        severe_outlier_upper_bound = float(severe_outlier_cluster["upper"])
        order_balance = min(
            sum(pair.order < 0 for pair in members),
            sum(pair.order > 0 for pair in members),
        ) / len(members)
        meta = heterogeneity[key]
        run_design = per_run_design[key]
        failures: list[str] = []
        if key in absolute_failures:
            failures.append("absolute-reference-unresolved")
        if key.encoding_hex == "unknown":
            failures.append("encoding-bytes-unavailable")
        if len(members) < min_pairs:
            failures.append("insufficient-pairs")
        if ess < min_effective_pairs:
            failures.append("insufficient-effective-pairs")
        if runs < min_runs:
            failures.append("insufficient-independent-runs")
        if count_levels < min_count_levels or min(per_level, default=0) < 4:
            failures.append("insufficient-batch-levels")
        if purity_q05 is None:
            failures.append("instruction-purity-unavailable")
        elif purity_q05 < min_purity:
            failures.append("instruction-purity-below-threshold")
        if not all(pair.timer_matched for pair in members):
            failures.append("timer-read-mismatch")
        if response_sources[key] == "guest-time-fallback":
            failures.append("guest-time-primary-response")
        elif response_sources[key] != "qemu-vcpu-thread-cpu-time-marker-only":
            failures.append("primary-response-is-instrumented")
        if len([pair for pair in grouped[key] if pair.plugin_delta_ns is not None]) != len(grouped[key]):
            failures.append("plugin-response-incomplete")
        if not all(pair.translation_observed for pair in members):
            failures.append("translation-observation-unavailable")
        if translation_exclusion_upper > max_translation_excluded_pair_fraction:
            failures.append("translation-exclusion-fraction-too-high")
        if not fit.irls_converged:
            failures.append("irls-not-converged")
        if not math.isfinite(fit.design_condition_number) or fit.design_condition_number > 1e8:
            failures.append("design-matrix-ill-conditioned")
        if any(not row["complete"] for row in run_design):
            failures.append("per-run-design-incomplete")
        if any(row["irls_converged"] is False for row in run_design):
            failures.append("per-run-irls-not-converged")
        if any(
            row["design_condition_number"] is None
            or not math.isfinite(row["design_condition_number"])
            or row["design_condition_number"] > 1e8
            for row in run_design
        ):
            failures.append("per-run-design-ill-conditioned")
        if raw_interval is None:
            failures.append("simultaneous-ci-missing")
        elif raw_point is not None and raw_point < 0.0 and not zero_cost_equivalent:
            failures.append("negative-unconstrained-weight")
        elif zero_cost_equivalent:
            relative_half = None
        elif raw_point is not None and raw_point > 0.0:
            relative_half = (
                raw_interval[1] - raw_interval[0]
            ) / (2.0 * raw_point)
            if (
                raw_interval[0] <= 0.0
                or relative_half > max_relative_ci_half_width
            ):
                failures.append("simultaneous-ci-too-wide")
        else:
            failures.append("zero-cost-ci-too-wide")
        adjusted_zero_cost_equivalent = (
            adjusted_interval is not None
            and adjusted_interval[0] >= -max_zero_cost_ci_upper_ns
            and adjusted_interval[1] <= max_zero_cost_ci_upper_ns
        )
        adjusted_point = (
            0.0
            if adjusted_raw_point is not None and adjusted_zero_cost_equivalent
            else adjusted_raw_point
            if adjusted_raw_point is not None and adjusted_raw_point >= 0.0
            else None
        )
        adjusted_relative_half = None
        if adjusted_raw_point is None or adjusted_interval is None:
            failures.append("anchor-adjusted-estimate-unavailable")
        elif adjusted_raw_point < 0.0 and not adjusted_zero_cost_equivalent:
            failures.append("anchor-adjusted-negative-weight")
        elif adjusted_zero_cost_equivalent:
            adjusted_relative_half = None
        elif adjusted_raw_point > 0.0:
            adjusted_relative_half = (
                adjusted_interval[1] - adjusted_interval[0]
            ) / (2.0 * adjusted_raw_point)
            if (
                adjusted_interval[0] <= 0.0
                or adjusted_relative_half > max_relative_ci_half_width
            ):
                failures.append("anchor-adjusted-ci-too-wide")
        else:
            failures.append("anchor-adjusted-zero-cost-ci-too-wide")
        discrepancy_margin = max(
            max_zero_cost_ci_upper_ns,
            equivalence_margin * abs(float(raw_point or 0.0)),
        )
        discrepancy_equivalent = (
            discrepancy_interval is not None
            and discrepancy_interval[0] >= -discrepancy_margin
            and discrepancy_interval[1] <= discrepancy_margin
        )
        if not discrepancy_equivalent:
            failures.append("raw-adjusted-discrepancy-not-equivalent")
        sensitivity_interval = estimator_sensitivity_intervals.get(key)
        sensitivity_margin = max(
            max_zero_cost_ci_upper_ns,
            equivalence_margin * abs(float(raw_point or 0.0)),
        )
        sensitivity_equivalent = (
            sensitivity_interval is not None
            and sensitivity_interval[0] >= -sensitivity_margin
            and sensitivity_interval[1] <= sensitivity_margin
        )
        if key not in estimator_sensitivity_points:
            failures.append("estimator-sensitivity-unavailable")
        elif not sensitivity_equivalent:
            failures.append("estimator-sensitivity-not-equivalent")
        deletion_margin = max(
            max_zero_cost_ci_upper_ns,
            equivalence_margin * abs(float(raw_point or 0.0)),
        )
        deletion_sensitivity = leave_one_run_out[key]
        maximum_deletion_shift = deletion_sensitivity.get(
            "maximum_absolute_shift_ns"
        )
        deletion_stable = (
            deletion_sensitivity.get("complete") is True
            and isinstance(maximum_deletion_shift, (int, float))
            and not isinstance(maximum_deletion_shift, bool)
            and math.isfinite(float(maximum_deletion_shift))
            and float(maximum_deletion_shift) <= deletion_margin
        )
        if not deletion_stable:
            failures.append("single-super-run-influence-too-high")
        if joint_complete_rows != bootstrap_replicates:
            failures.append("joint-bootstrap-incomplete")
        if len(bootstrap_rows) < PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES:
            failures.append("insufficient-bootstrap-replicates")
        if len(bootstrap_rows) != bootstrap_replicates or (
            simultaneous_monte_carlo["complete_family_replicates"]
            != bootstrap_replicates
        ):
            failures.append("insufficient-bootstrap-valid-fraction")
        if not simultaneous_monte_carlo["finite_rank_supported"]:
            failures.append("max-stat-monte-carlo-inconclusive")
        if (
            diagnostic_complete_rows
            < PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES
        ):
            failures.append("insufficient-diagnostic-bootstrap-replicates")
        if (
            diagnostic_complete_rows != bootstrap_replicates
        ):
            failures.append("insufficient-diagnostic-bootstrap-valid-fraction")
        if not diagnostic_monte_carlo["finite_rank_supported"]:
            failures.append("diagnostic-max-stat-monte-carlo-inconclusive")
        if order_balance < 0.35:
            failures.append("ab-ba-imbalance")
        # Huber 权重是连续的 influence diagnostic，不是独立 Bernoulli 事件。
        # 严重降权比例保留在输出中供审计，但不再单独否决结果；真正的
        # estimator-sensitivity 联合等价门禁在上面执行，避免把同一异常同时
        # 计入比例门禁和稳健估计器影响而造成双重惩罚。
        if meta["i_squared"] is None:
            failures.append("cross-run-heterogeneity-unavailable")
        if meta["usable_runs"] != runs:
            failures.append("cross-run-coverage-incomplete")
        if meta.get("identifiable") and not meta.get(
            "tau_squared_converged", False
        ):
            failures.append("cross-run-tau-estimator-not-converged")
        prediction_interval = meta.get("prediction_interval")
        prediction_half_width = None
        if prediction_interval is None:
            failures.append("cross-run-prediction-interval-unavailable")
        else:
            prediction_half_width = (
                prediction_interval[1] - prediction_interval[0]
            ) / 2.0
            if zero_cost_equivalent:
                if (
                    prediction_interval[0] < -max_zero_cost_ci_upper_ns
                    or prediction_interval[1] > max_zero_cost_ci_upper_ns
                ):
                    failures.append("cross-run-prediction-unstable")
            elif raw_point is not None and raw_point > 0.0:
                practical_half_width = max(
                    max_zero_cost_ci_upper_ns,
                    2.0 * equivalence_margin * raw_point,
                )
                if prediction_half_width > practical_half_width:
                    failures.append("cross-run-heterogeneity-high")
        meta["prediction_interval_half_width"] = prediction_half_width
        meta["relative_prediction_interval_half_width"] = (
            prediction_half_width / raw_point
            if prediction_half_width is not None
            and raw_point is not None
            and raw_point > 0.0
            else None
        )

        absolute_effects = absolute_diagnostic_points.get(key, {})
        contrast_effects = contrast_diagnostic_points[key]
        diagnostic_ci: dict[str, list[float] | None] = {
            name: diagnostic_intervals.get((key, name))
            for name in (
                "order",
                "drift",
                "batch",
                "process_design",
                "process_period",
                "process_carryover",
            )
        }
        diagnostic_ci["translation"] = None
        process_crossover = _process_crossover_effects(fit)
        batch_level_effects: dict[str, float | None] = {}
        batch_level_ci: dict[str, list[float] | None] = {}
        for rank, level in enumerate(fit.batch_levels):
            name = f"batch_level_rank:{rank}"
            batch_level_effects[str(level)] = (
                0.0
                if level == fit.batch_reference
                else contrast_effects.get(name)
            )
            batch_level_ci[str(level)] = (
                [0.0, 0.0]
                if level == fit.batch_reference
                else diagnostic_intervals.get((key, name))
            )
        batch_pairwise_effects: dict[str, float | None] = {}
        batch_pairwise_ci: dict[str, list[float] | None] = {}
        for left_rank, left in enumerate(fit.batch_levels):
            for right_rank, right in enumerate(
                fit.batch_levels[left_rank + 1 :], start=left_rank + 1
            ):
                label = f"{left}:{right}"
                name = f"batch_pairwise_rank:{left_rank}:{right_rank}"
                batch_pairwise_effects[label] = contrast_effects.get(
                    name
                )
                batch_pairwise_ci[label] = diagnostic_intervals.get(
                    (key, name)
                )
        if raw_point is not None:
            margin = (
                max_zero_cost_ci_upper_ns
                if zero_cost_equivalent
                else equivalence_margin * abs(raw_point)
            )
            for name, code in (
                ("order", "order-effect-not-equivalent"),
                ("drift", "drift-effect-not-equivalent"),
                ("process_design", "process-design-effect-not-equivalent"),
                ("process_period", "process-period-effect-not-equivalent"),
                ("process_carryover", "process-carryover-effect-not-equivalent"),
            ):
                current = diagnostic_ci[name]
                if current is None or current[0] < -margin or current[1] > margin:
                    failures.append(code)
            if not process_crossover["available"]:
                failures.append("process-crossover-effect-unavailable")
            elif (
                process_crossover["minimum_design_fraction"]
                < MIN_CROSSOVER_DESIGN_FRACTION
            ):
                failures.append("process-crossover-design-imbalanced")
            pairwise_batch_intervals = list(batch_pairwise_ci.values())
            if not pairwise_batch_intervals or any(
                interval is None
                or interval[0] < -margin
                or interval[1] > margin
                for interval in pairwise_batch_intervals
            ):
                failures.append("batch-size-nonlinearity")

        guest_point = guest_absolute.get(key)
        auxiliary_points = auxiliary_inference["points"]
        auxiliary_intervals = auxiliary_inference["intervals"]
        auxiliary_coverage = auxiliary_inference["coverage"][key]
        cross_clock_ratio = auxiliary_points.get(("cross-clock-ratio", key))
        cross_clock_ratio_ci = auxiliary_intervals.get(
            ("cross-clock-ratio", key)
        )
        cross_clock_difference = auxiliary_points.get(
            ("cross-clock-difference", key)
        )
        cross_clock_difference_ci = auxiliary_intervals.get(
            ("cross-clock-difference", key)
        )
        cross_clock_complete = (
            (
                auxiliary_coverage["cross_difference_complete"]
                and auxiliary_coverage["cross_difference_usable_runs"] == runs
            )
            if zero_cost_equivalent
            else (
                auxiliary_coverage["primary_complete"]
                and auxiliary_coverage["guest_complete"]
                and auxiliary_coverage["primary_usable_runs"] == runs
                and auxiliary_coverage["guest_usable_runs"] == runs
            )
        )
        selected_cross_interval = (
            cross_clock_difference_ci
            if zero_cost_equivalent
            else cross_clock_ratio_ci
        )
        if not cross_clock_complete or selected_cross_interval is None:
            failures.append("cross-clock-check-unavailable")
            cross_clock_status = "unavailable"
        elif auxiliary_inference["complete_family_replicates"] != bootstrap_replicates:
            failures.append("cross-clock-check-unavailable")
            cross_clock_status = "insufficient-bootstrap-valid-fraction"
        elif not auxiliary_inference["critical_value_monte_carlo"][
            "finite_rank_supported"
        ]:
            failures.append("cross-clock-check-unavailable")
            cross_clock_status = "max-stat-monte-carlo-inconclusive"
        elif zero_cost_equivalent and (
            selected_cross_interval[0] < -max_zero_cost_ci_upper_ns
            or selected_cross_interval[1] > max_zero_cost_ci_upper_ns
        ):
            failures.append("cross-clock-check-divergent")
            cross_clock_status = "divergent"
        elif not zero_cost_equivalent and (
            selected_cross_interval[0] < min_cross_clock_ratio
            or selected_cross_interval[1] > max_cross_clock_ratio
        ):
            failures.append("cross-clock-check-divergent")
            cross_clock_status = "divergent"
        else:
            cross_clock_status = "accepted"
        plugin_off_point = plugin_off_guest_absolute.get(key)
        plugin_off_ratio = auxiliary_points.get(("plugin-off-ratio", key))
        plugin_off_ratio_ci = auxiliary_intervals.get(
            ("plugin-off-ratio", key)
        )
        plugin_off_difference = auxiliary_points.get(
            ("plugin-off-difference", key)
        )
        plugin_off_difference_ci = auxiliary_intervals.get(
            ("plugin-off-difference", key)
        )
        plugin_off_complete = (
            (
                auxiliary_coverage["plugin_off_difference_complete"]
                and auxiliary_coverage[
                    "plugin_off_difference_usable_runs"
                ]
                == runs
            )
            if zero_cost_equivalent
            else (
                auxiliary_coverage["guest_complete"]
                and auxiliary_coverage["plugin_off_complete"]
                and auxiliary_coverage["guest_usable_runs"] == runs
                and auxiliary_coverage["plugin_off_usable_runs"] == runs
            )
        )
        selected_plugin_off_interval = (
            plugin_off_difference_ci
            if zero_cost_equivalent
            else plugin_off_ratio_ci
        )
        if not plugin_off_complete or selected_plugin_off_interval is None:
            failures.append("plugin-off-check-unavailable")
            plugin_off_status = "unavailable"
        elif auxiliary_inference["complete_family_replicates"] != bootstrap_replicates:
            failures.append("plugin-off-check-unavailable")
            plugin_off_status = "insufficient-bootstrap-valid-fraction"
        elif not auxiliary_inference["critical_value_monte_carlo"][
            "finite_rank_supported"
        ]:
            failures.append("plugin-off-check-unavailable")
            plugin_off_status = "max-stat-monte-carlo-inconclusive"
        elif zero_cost_equivalent and (
            selected_plugin_off_interval[0] < -max_zero_cost_ci_upper_ns
            or selected_plugin_off_interval[1] > max_zero_cost_ci_upper_ns
        ):
            failures.append("plugin-off-check-divergent")
            plugin_off_status = "divergent"
        elif not zero_cost_equivalent and (
            selected_plugin_off_interval[0] < min_plugin_off_ratio
            or selected_plugin_off_interval[1] > max_plugin_off_ratio
        ):
            failures.append("plugin-off-check-divergent")
            plugin_off_status = "divergent"
        else:
            plugin_off_status = "accepted"
        if anchor_scale_inference["status"] != "accepted":
            failures.append("positive-anchor-scale-inconclusive")

        control = controls.get(key)
        if key in assumed_empty and control is None:
            failures.append("empty-control-assumed-not-declared")
        status = _quality_status(failures, fatal_codes)
        guest = guest_fits.get(key)
        item = {
            "key": key.public(),
            "coarse_key": {
                "mnemonic": key.mnemonic,
                "size": key.size,
                "pattern": key.pattern,
            },
            "semantic_mnemonic": _semantic_mnemonic(key.mnemonic),
            "ns_per_instruction": point,
            "published_ns_per_instruction": None,
            "unconstrained_ns_per_instruction": raw_point,
            "contrast_ns_per_instruction": fit.estimate,
            "control_key": (
                None
                if control is None
                else control.public()
            ),
            "simultaneous_ci": interval,
            "unconstrained_simultaneous_ci": raw_interval,
            "point_ci": point_ci[key],
            "unconstrained_point_ci": point_ci[key],
            "anchor_adjusted": {
                "ns_per_instruction": adjusted_point,
                "unconstrained_ns_per_instruction": adjusted_raw_point,
                "simultaneous_ci": adjusted_interval,
                "relative_simultaneous_ci_half_width": adjusted_relative_half,
                "calibration_only": key.pattern in CALIBRATION_ONLY_PATTERNS,
            },
            "raw_adjusted_discrepancy": {
                "ns_per_instruction": discrepancy_point,
                "simultaneous_ci": discrepancy_interval,
                "equivalence_margin_ns": discrepancy_margin,
                "equivalent": discrepancy_equivalent,
            },
            "estimator_sensitivity": {
                "estimand": "classical-heteroscedastic-wls-minus-huber-absolute-cost",
                "ns_per_instruction": estimator_sensitivity_points.get(key),
                "simultaneous_ci": sensitivity_interval,
                "equivalence_margin_ns": sensitivity_margin,
                "equivalent": sensitivity_equivalent,
                "classical_fit_available": key in classical_contrasts,
                "classical_control_chain_resolved": key in classical_absolute,
            },
            "leave_one_super_run_out_sensitivity": {
                **deletion_sensitivity,
                "equivalence_margin_ns": deletion_margin,
                "stable": deletion_stable,
            },
            "calibration_only": key.pattern in CALIBRATION_ONLY_PATTERNS,
            "zero_cost_equivalent": zero_cost_equivalent,
            "ESS": ess,
                "runs": runs,
                "qemu_runs": qemu_runs,
            "pairs": len(members),
            "total_target_count": sum(pair.target_count for pair in members),
            "count_levels": count_levels,
            "minimum_pairs_per_level": min(per_level, default=0),
            "purity_q05": purity_q05,
            "translation_density_q95": translation_density_q95,
            "translation_contaminated_pairs_excluded": excluded_by_key.get(
                key, 0
            ),
            "translation_exclusion_run_cluster_inference": translation_exclusion,
            "translation_exclusion_fraction_run_cluster_upper": (
                translation_exclusion_upper
            ),
            "maximum_translation_excluded_pair_fraction": (
                max_translation_excluded_pair_fraction
            ),
            "minimum_crossover_design_fraction": (
                MIN_CROSSOVER_DESIGN_FRACTION
            ),
            "identifiability": (
                "strong" if status == "high-confidence" else "weak"
            ),
            "quality": status,
            "source": response_sources[key],
            "quality_failures": failures,
            "relative_simultaneous_ci_half_width": relative_half,
            "conservative_ns_per_instruction": (
                None
                if interval is None or point is None
                else max(0.0, interval[1])
            ),
            "outlier_downweighted_fraction": huber_downweighted,
            "huber_downweighted_fraction": huber_downweighted,
            "severe_outlier_fraction": severe_outliers,
            "severe_outlier_count": severe_outlier_count,
            "severe_outlier_scope": (
                "local-target-minus-control-contrast; absolute quality also requires every control"
            ),
            "severe_outlier_fraction_cluster_mean": (
                severe_outlier_cluster["mean_run_fraction"]
            ),
            "severe_outlier_fraction_run_cluster_upper": (
                severe_outlier_upper_bound
            ),
            "severe_outlier_run_cluster_inference": severe_outlier_cluster,
            # 兼容旧 JSON 消费者；值已切换为 run-cluster 上界，不能再解释
            # 为把每个 pair 当作独立 Bernoulli 的 Wilson 区间。
            "severe_outlier_fraction_wilson_upper": severe_outlier_upper_bound,
            "maximum_severe_outlier_fraction": max_severe_outlier_fraction,
            "order_balance": order_balance,
            "fit_diagnostics": {
                "irls_converged": fit.irls_converged,
                "irls_iterations": fit.irls_iterations,
                "irls_cycle_damping_used": fit.irls_cycle_damping_used,
                "design_condition_number": _json_finite(
                    fit.design_condition_number
                ),
                "per_run": run_design,
            },
            "effects": {
                "estimand": "absolute-instruction-cost-through-control-chain",
                "ab_ba_difference": absolute_effects.get("order"),
                "within_run_end_minus_start": absolute_effects.get("drift"),
                "batch_effect_model": "categorical-reference-batch",
                "batch_quality_estimand": (
                    "local-target-minus-control-contrast; every control edge "
                    "is gated on its own physical batch grid"
                ),
                "batch_reference": fit.batch_reference,
                "batch_levels": list(fit.batch_levels),
                "batch_level_effects_vs_reference": batch_level_effects,
                "batch_level_simultaneous_ci": batch_level_ci,
                "batch_pairwise_contrast_direction": "right-minus-left",
                "batch_pairwise_effects": batch_pairwise_effects,
                "batch_pairwise_simultaneous_ci": batch_pairwise_ci,
                "batch_peak_to_peak": (
                    max(
                        value
                        for value in batch_level_effects.values()
                        if value is not None
                    )
                    - min(
                        value
                        for value in batch_level_effects.values()
                        if value is not None
                    )
                    if all(
                        value is not None
                        for value in batch_level_effects.values()
                    )
                    else None
                ),
                # 兼容旧输出：categorical 两端点的割线，不是模型中的线性项。
                "per_log_batch": absolute_effects.get("batch"),
                "per_log_batch_method": (
                    "compatibility-endpoint-secant-not-used-for-gating"
                ),
                "ns_per_translation_event": absolute_effects.get(
                    "translation"
                ),
                "batch_log_range": fit.batch_log_range,
                "bootstrap_ci": diagnostic_ci,
                "process_launch_crossover": {
                    **process_crossover,
                    "simultaneous_ci": {
                        "design_abba_minus_baab": diagnostic_ci[
                            "process_design"
                        ],
                        "second_pair_minus_first_pair": diagnostic_ci[
                            "process_period"
                        ],
                        "preceded_by_plugin_off_minus_other_timing": diagnostic_ci[
                            "process_carryover"
                        ],
                    },
                    "minimum_required_design_fraction": (
                        MIN_CROSSOVER_DESIGN_FRACTION
                    ),
                },
                "local_contrast_only": {
                    "ab_ba_difference": contrast_effects.get("order"),
                    "within_run_end_minus_start": contrast_effects.get(
                        "drift"
                    ),
                    "per_log_batch": contrast_effects.get("batch"),
                    "batch_level_effects_vs_reference": {
                        str(level): value
                        for level, value in fit.batch_level_effects.items()
                    },
                    "batch_pairwise_effects": {
                        f"{left}:{right}": contrast_effects.get(
                            f"batch_pairwise_rank:{left_rank}:{right_rank}"
                        )
                        for left_rank, left in enumerate(fit.batch_levels)
                        for right_rank, right in enumerate(
                            fit.batch_levels[left_rank + 1 :],
                            start=left_rank + 1,
                        )
                    },
                    "ns_per_translation_event": contrast_effects.get(
                        "translation"
                    ),
                    "may_support_absolute_high_confidence": False,
                },
            },
            "autocorrelation": autocorrelation,
            "cross_run_random_effects": meta,
            "guest_time_check": (
                {
                    "absolute_estimate_ns_per_instruction": guest_point,
                    "contrast_estimate_ns_per_instruction": (
                        None if guest is None else guest.estimate
                    ),
                    "ratio_to_primary_absolute": cross_clock_ratio,
                    "simultaneous_ratio_ci": cross_clock_ratio_ci,
                    "difference_ns_per_instruction": cross_clock_difference,
                    "simultaneous_difference_ci": cross_clock_difference_ci,
                    "equivalence_statistic": (
                        "difference" if zero_cost_equivalent else "ratio"
                    ),
                    "status": cross_clock_status,
                    "accepted_ratio_range": [
                        min_cross_clock_ratio,
                        max_cross_clock_ratio,
                    ],
                    "zero_cost_absolute_margin_ns": max_zero_cost_ci_upper_ns,
                    "run_coverage": auxiliary_coverage,
                }
            ),
            "plugin_off_check": {
                "uninstrumented_guest_estimate_ns_per_instruction": (
                    plugin_off_point
                ),
                "timing_plugin_to_plugin_off_ratio": plugin_off_ratio,
                "simultaneous_ratio_ci": plugin_off_ratio_ci,
                "difference_ns_per_instruction": plugin_off_difference,
                "simultaneous_difference_ci": plugin_off_difference_ci,
                "equivalence_statistic": (
                    "difference" if zero_cost_equivalent else "ratio"
                ),
                "status": plugin_off_status,
                "accepted_ratio_range": [
                    min_plugin_off_ratio,
                    max_plugin_off_ratio,
                ],
                "zero_cost_absolute_margin_ns": max_zero_cost_ci_upper_ns,
                "run_coverage": auxiliary_coverage,
            },
        }
        items.append(item)
        item_by_key[key] = item

    control_chains: dict[_InstructionKey, list[_InstructionKey]] = {}
    for key in item_by_key:
        chain: list[_InstructionKey] = []
        seen = {key}
        control = controls.get(key)
        while control is not None and control not in seen:
            chain.append(control)
            seen.add(control)
            control = controls.get(control)
        control_chains[key] = chain
        if any(
            item_by_key.get(dependency, {}).get("quality")
            != "high-confidence"
            for dependency in chain
        ):
            failures = item_by_key[key]["quality_failures"]
            if "control-quality-not-high" not in failures:
                failures.append("control-quality-not-high")
            status = _quality_status(failures, fatal_codes)
            item_by_key[key]["quality"] = status
            item_by_key[key]["identifiability"] = (
                "strong" if status == "high-confidence" else "weak"
            )
    for key, item in item_by_key.items():
        item["control_quality_chain"] = [
            {
                "key": dependency.public(),
                "quality": item_by_key.get(dependency, {}).get(
                    "quality", "not-identifiable"
                ),
                "quality_failures": item_by_key.get(dependency, {}).get(
                    "quality_failures", ["control-result-unavailable"]
                ),
            }
            for dependency in control_chains[key]
        ]
        item["absolute_quality_requires_control_chain"] = True
        item["published_ns_per_instruction"] = (
            item.get("anchor_adjusted", {}).get("ns_per_instruction")
            if item.get("quality") == "high-confidence"
            and not item.get("calibration_only", False)
            else None
        )

    positive = [
        item["published_ns_per_instruction"]
        for item in items
        if isinstance(item.get("published_ns_per_instruction"), (int, float))
        and item["published_ns_per_instruction"] > 0.0
        and not item.get("calibration_only", False)
    ]
    reference = statistics.median(positive) if positive else None
    for item in items:
        value = item.get("published_ns_per_instruction")
        adjusted_interval = item.get("anchor_adjusted", {}).get(
            "simultaneous_ci"
        )
        conservative = (
            max(0.0, float(adjusted_interval[1]))
            if isinstance(adjusted_interval, list)
            and len(adjusted_interval) == 2
            and value is not None
            else None
        )
        item["relative_weight"] = (
            value / reference
            if reference is not None
            and isinstance(value, (int, float))
            and value >= 0.0
            else None
        )
        item["conservative_relative_weight"] = (
            conservative / reference
            if reference is not None
            and isinstance(conservative, (int, float))
            and conservative > 0.0
            else None
        )

    # 不跨 memory/branch/dependency pattern 静默平均；仅在同 mnemonic+size 只有
    # 一个 pattern，或各 pattern 通过等价性门禁时给推荐权重。
    recommendations: list[dict[str, Any]] = []
    aggregate: dict[tuple[str, int], list[dict[str, Any]]] = defaultdict(list)
    for item in items:
        if item.get("calibration_only", False):
            continue
        key = item["key"]
        aggregate[(key["mnemonic"], key["size"])].append(item)
    for (mnemonic, size), members in sorted(aggregate.items()):
        usable = [
            item for item in members
            if item["published_ns_per_instruction"] is not None
        ]
        if len(members) == 1 and usable:
            recommendation = usable[0]["published_ns_per_instruction"]
            source = "single-context"
        elif len(usable) == len(members) and usable:
            values = [
                float(item["published_ns_per_instruction"])
                for item in usable
            ]
            center = statistics.median(values)
            spread = max(values) - min(values)
            if center > 0.0 and spread <= equivalence_margin * center:
                recommendation = math.fsum(values) / len(values)
                source = "equivalent-context-average"
            else:
                recommendation = None
                source = "context-dependent-no-aggregation"
        else:
            recommendation = None
            source = "insufficient-context-quality"
        recommendations.append(
            {
                "mnemonic": mnemonic,
                "size": size,
                "ns_per_instruction": recommendation,
                "source": source,
                "patterns": [item["key"]["pattern"] for item in members],
            }
        )

    publishable_items = [
        item
        for item in items
        if item.get("published_ns_per_instruction") is not None
        and not item.get("calibration_only", False)
    ]
    publication_failures: list[str] = []
    if anchor_scale_inference["status"] != "accepted":
        publication_failures.append("positive-anchor-scale-inconclusive")
    if bootstrap_replicates < PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES:
        publication_failures.append("insufficient-joint-bootstrap-replicates")
    family_completion = {
        "raw": simultaneous_monte_carlo["complete_family_replicates"],
        "diagnostic": diagnostic_monte_carlo["complete_family_replicates"],
        "auxiliary": auxiliary_inference["complete_family_replicates"],
        "joint": joint_complete_rows,
    }
    for family, complete in family_completion.items():
        if complete != bootstrap_replicates:
            publication_failures.append(
                f"incomplete-{family}-bootstrap-family"
            )
    if joint_complete_rows != bootstrap_replicates:
        publication_failures.append("insufficient-joint-bootstrap-valid-fraction")
    if not joint_monte_carlo["finite_rank_supported"]:
        publication_failures.append("joint-max-stat-monte-carlo-inconclusive")
    if not publishable_items:
        publication_failures.append("no-context-passed-all-publication-gates")
    # 统计核心不能自行证明采集宿主隔离或独立预测验证。这两项只能由
    # runner 在校验输入哈希和协议后注入；裸模型始终 fail closed。
    publication_failures.extend(
        ["host-isolation-audit-missing", "ml-validation-missing"]
    )
    statistical_core_passed = not publication_failures[:-2]
    result = {
        "schema_version": SCHEMA_VERSION,
        "generation_configuration": {
            "schema": GENERATION_CONFIGURATION_SCHEMA,
            "bootstrap_replicates": bootstrap_replicates,
            "confidence": confidence,
            "seed": seed,
            "block_length": block_length,
            "run_block_length": run_block_length,
            "minimum_pairs": min_pairs,
            "minimum_effective_pairs": min_effective_pairs,
            "minimum_independent_super_runs": min_runs,
            "minimum_count_levels": min_count_levels,
            "minimum_instruction_purity": min_purity,
            "maximum_relative_simultaneous_ci_half_width": (
                max_relative_ci_half_width
            ),
            "maximum_i_squared": max_i_squared,
            "effect_equivalence_margin": equivalence_margin,
            "cross_clock_ratio_range": [
                min_cross_clock_ratio,
                max_cross_clock_ratio,
            ],
            "plugin_off_ratio_range": [
                min_plugin_off_ratio,
                max_plugin_off_ratio,
            ],
            "maximum_zero_cost_simultaneous_ci_upper_ns": (
                max_zero_cost_ci_upper_ns
            ),
            "maximum_translation_events_per_target_instruction": (
                max_translation_density
            ),
            "maximum_translation_excluded_pair_fraction": (
                max_translation_excluded_pair_fraction
            ),
            "maximum_severe_outlier_fraction": max_severe_outlier_fraction,
            "linear_algebra_backend": selected_backend,
        },
        "model": "paired-huber-categorical-batch-paule-mandel-mkh-hierarchical-moving-block-max-standardized-deviation",
        "linear_algebra_backend": selected_backend,
        "primary_response": "marker-only-qemu-vcpu-thread-cpu-time",
        "instruction_key": "raw-encoding+semantic-decoding+execution-pattern",
        "confidence": confidence,
        "publication_familywise_error_control": {
            "method": (
                "union-bound-across-pre-registered-max-stat-families-with-"
                "split-sampling-and-monte-carlo-error-budgets"
            ),
            "overall_confidence": confidence,
            "overall_alpha": overall_alpha,
            "sampling_alpha_budget": sampling_alpha,
            "monte_carlo_alpha_budget": monte_carlo_alpha,
            "families": list(PUBLICATION_INFERENCE_FAMILIES),
            "family_count": len(PUBLICATION_INFERENCE_FAMILIES),
            "sampling_alpha_per_family": family_sampling_alpha,
            "sampling_confidence_per_family": family_confidence,
            "monte_carlo_alpha_per_family": family_monte_carlo_alpha,
            "monte_carlo_confidence_per_family": (
                family_monte_carlo_confidence
            ),
            "coverage_claim": (
                "unconditional family intersection failure probability is "
                "bounded by sampling_alpha_budget plus finite-bootstrap "
                "monte_carlo_alpha_budget under the stated bootstrap model"
            ),
        },
        "instructions": items,
        "recommended_by_mnemonic_size": recommendations,
        "normalization_ns_per_instruction": reference,
        "sample_filtering": {
            "input_pairs": len(all_pairs),
            "retained_pairs": len(pairs),
            "translation_contaminated_pairs_excluded": len(
                translation_contaminated
            ),
            "translation_unknown_pairs_retained": sum(
                not pair.translation_observed for pair in pairs
            ),
            "translation_contaminated_pairs_excluded_by_instruction": [
                {"key": key.public(), "pairs": count}
                for key, count in sorted(excluded_by_key.items())
            ],
        },
        "simultaneous_inference": {
            "method": "hierarchical run-cluster moving-block bootstrap max-standardized-deviation",
            "familywise_confidence": family_confidence,
            "requested_replicates": bootstrap_replicates,
            "valid_replicates": len(bootstrap_rows),
            "valid_fraction": bootstrap_valid_fraction,
            "minimum_valid_fraction": 1.0,
            "worker_processes": bootstrap_jobs,
            "quantile_probability_monte_carlo_se": (
                math.sqrt(
                    family_confidence
                    * (1.0 - family_confidence)
                    / len(bootstrap_rows)
                )
                if bootstrap_rows
                else None
            ),
            "critical_value": critical,
            "complete_family_replicates": simultaneous_monte_carlo[
                "complete_family_replicates"
            ],
            "complete_max_statistic_replicates": simultaneous_valid,
            "critical_value_monte_carlo": simultaneous_monte_carlo,
            "run_is_highest_cluster": False,
            "super_run_is_highest_cluster": True,
            "run_order": _ordered_runs(pairs),
            "super_run_order": _ordered_super_runs(pairs),
            "run_order_source": run_order_source,
            "run_resampling": "super-run-circular-moving-block-bootstrap",
            "run_block_length": selected_run_block,
            "automatic_run_block_length_rule": "round(number-of-runs^(1/3))",
            "block_length": selected_block,
            "automatic_block_length": automatic_block,
            "block_length_unit": "probe-round-blocks",
        },
        "diagnostic_simultaneous_inference": {
            "method": "joint-instruction-and-effect max-standardized-deviation",
            "familywise_confidence": family_confidence,
            "critical_value": diagnostic_critical,
            "complete_replicates": diagnostic_valid,
            "complete_family_replicates": diagnostic_monte_carlo[
                "complete_family_replicates"
            ],
            "requested_replicates": bootstrap_replicates,
            "valid_fraction": (
                diagnostic_complete_rows / bootstrap_replicates
                if bootstrap_replicates > 0
                else 0.0
            ),
            "minimum_complete_replicates": (
                PUBLICATION_MINIMUM_MAX_STAT_CALIBRATION_REPLICATES
            ),
            "minimum_valid_fraction": 1.0,
            "critical_value_monte_carlo": diagnostic_monte_carlo,
            "effects": [
                "order",
                "drift",
                "local-edge-batch-categorical-vs-reference",
                "local-edge-batch-categorical-all-pairwise-contrasts",
                "batch-endpoint-secant-compatibility",
                "process-launch-design-abba-minus-baab",
                "process-launch-second-pair-minus-first-pair",
                "process-launch-preceded-by-plugin-off-carryover",
            ],
            "batch_control_chain_policy": (
                "gate-each-control-edge-on-its-own-physical-batch-grid"
            ),
        },
        "auxiliary_consistency_inference": {
            "method": (
                "paired-per-run-estimate run-cluster bootstrap "
                "max-standardized-deviation"
            ),
            "zero_cost_difference_method": (
                "fit-within-pair-difference-of-responses-before-run-bootstrap"
            ),
            "familywise_confidence": family_confidence,
            "requested_replicates": auxiliary_inference[
                "requested_replicates"
            ],
            "valid_replicates": auxiliary_inference["valid_replicates"],
            "complete_family_replicates": auxiliary_inference[
                "complete_family_replicates"
            ],
            "valid_fraction": (
                auxiliary_inference["complete_family_replicates"]
                / auxiliary_inference["requested_replicates"]
                if auxiliary_inference["requested_replicates"] > 0
                else 0.0
            ),
            "critical_value": auxiliary_inference["critical_value"],
            "critical_value_monte_carlo": auxiliary_inference[
                "critical_value_monte_carlo"
            ],
            "requires_same_pairs_and_all_primary_super_runs": True,
            "run_resampling": "super-run-circular-moving-block-bootstrap",
            "run_block_length": selected_run_block,
            "shares_run_indices_with_primary_bootstrap": True,
        },
        "positive_anchor_scale_inference": anchor_scale_inference,
        "estimator_sensitivity_inference": {
            "method": (
                "joint-super-run-moving-block-bootstrap of classical "
                "heteroscedastic-WLS minus Huber absolute estimates"
            ),
            "estimand": "classical-heteroscedastic-wls-minus-huber-absolute-cost",
            "simultaneous_intervals": {
                json.dumps(key.public(), sort_keys=True, separators=(",", ":")): joint_intervals.get(
                    ("estimator-sensitivity", key)
                )
                for key in estimator_sensitivity_points
            },
            "points": {
                json.dumps(key.public(), sort_keys=True, separators=(",", ":")): value
                for key, value in estimator_sensitivity_points.items()
            },
            "shared_bootstrap_family": True,
            "equivalence_policy": (
                "absolute interval must lie within max(zero-cost margin, "
                "relative effect margin)"
            ),
            "severe_outlier_fraction_role": "diagnostic-only",
        },
        "joint_raw_adjusted_inference": {
            "method": (
                "single hierarchical super-run moving-block bootstrap with "
                "one max-standardized-deviation family over raw, adjusted, "
                "anchor nuisance and raw-adjusted discrepancy"
            ),
            "familywise_confidence": family_confidence,
            "requested_replicates": bootstrap_replicates,
            "complete_replicates": joint_complete_rows,
            "complete_family_replicates": joint_monte_carlo[
                "complete_family_replicates"
            ],
            "complete_max_statistic_replicates": joint_valid,
            "critical_value": joint_critical,
            "critical_value_monte_carlo": joint_monte_carlo,
            "point_family_size": len(joint_points),
            "target_and_anchor_share_super_run_indices": True,
            "anchor_reestimated_inside_each_replicate": True,
        },
        "publication_gate": {
            "passed": False,
            "failures": publication_failures,
            "publishable_contexts": len(publishable_items),
            "components": {
                "statistical_core": statistical_core_passed,
                "raw": all(
                    item.get("simultaneous_ci") is not None
                    for item in publishable_items
                ),
                "anchor_adjusted": all(
                    item.get("anchor_adjusted", {}).get("simultaneous_ci")
                    is not None
                    for item in publishable_items
                ),
                "positive_anchor": anchor_scale_inference["status"] == "accepted",
                "raw_adjusted_discrepancy": all(
                    item.get("raw_adjusted_discrepancy", {}).get("equivalent")
                    is True
                    for item in publishable_items
                ),
                "estimator_sensitivity": all(
                    item.get("estimator_sensitivity", {}).get("equivalent")
                    is True
                    for item in publishable_items
                ),
                "single_super_run_influence": all(
                    item.get("leave_one_super_run_out_sensitivity", {}).get(
                        "stable"
                    )
                    is True
                    for item in publishable_items
                ),
                "joint_bootstrap": (
                    bootstrap_replicates
                    >= PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES
                    and joint_complete_rows == bootstrap_replicates
                    and joint_monte_carlo["finite_rank_supported"]
                ),
                "host_isolation": False,
                "ml_validation": False,
            },
            "statistical_core_passed": statistical_core_passed,
            "policy": (
                "publish only non-calibration contexts that pass raw, "
                "anchor-adjusted, anchor-position/batch, discrepancy, estimator-"
                "sensitivity, single-run influence, joint-bootstrap, host-isolation "
                "and independent-ML validation gates"
            ),
        },
        "quality_thresholds": {
            "minimum_pairs": min_pairs,
            "minimum_effective_pairs": min_effective_pairs,
            "minimum_independent_super_runs": min_runs,
            "minimum_count_levels": min_count_levels,
            "minimum_instruction_purity": min_purity,
            "maximum_relative_simultaneous_ci_half_width": max_relative_ci_half_width,
            "maximum_i_squared": max_i_squared,
            "i_squared_role": "diagnostic-only-not-a-prediction-interval-gate",
            "future_run_prediction_interval_gate": (
                "unconditional-practical-half-width"
            ),
            "maximum_design_condition_number": 1e8,
            "irls_weight_rms_tolerance": 1e-6,
            "irls_maximum_iterations": 120,
            "irls_cycle_damping": {
                "trigger": "two-cycle-distance-at-most-five-percent-of-fixed-point-residual",
                "relaxation": 0.75,
            },
            "effect_equivalence_margin": equivalence_margin,
            "cross_clock_ratio_range": [
                min_cross_clock_ratio,
                max_cross_clock_ratio,
            ],
            "plugin_off_ratio_range": [
                min_plugin_off_ratio,
                max_plugin_off_ratio,
            ],
            "maximum_zero_cost_simultaneous_ci_upper_ns": (
                max_zero_cost_ci_upper_ns
            ),
            "maximum_translation_events_per_target_instruction": (
                max_translation_density
            ),
            "maximum_translation_excluded_pair_fraction": (
                max_translation_excluded_pair_fraction
            ),
            "minimum_crossover_design_fraction": (
                MIN_CROSSOVER_DESIGN_FRACTION
            ),
            "translation_exclusion_independent_unit": (
                "complete-crossover-super-run"
            ),
            "minimum_bootstrap_replicates": (
                PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES
            ),
            "minimum_bootstrap_valid_fraction": 1.0,
            "severe_huber_weight_threshold": 0.25,
            "maximum_severe_outlier_fraction": max_severe_outlier_fraction,
            "severe_outlier_fraction_gate": "diagnostic-only; estimator sensitivity is the formal robustness gate",
            "severe_outlier_independent_unit": "complete-crossover-super-run",
            "estimator_sensitivity_gate": (
                "joint simultaneous equivalence of classical heteroscedastic WLS "
                "and Huber absolute estimates"
            ),
            "single_super_run_influence_gate": (
                "all leave-one-super-run-out refits complete and maximum "
                "absolute shift within the practical equivalence margin"
            ),
        },
    }
    json.dumps(result, allow_nan=False)
    return result


def load_samples(path: str | Path) -> list[dict[str, Any]]:
    """读取 JSON、JSONL、带 ``RV_WEIGHT_SAMPLE`` 前缀的 JSONL 或 TSV。"""

    text = Path(path).read_text(encoding="utf-8")
    stripped = text.lstrip()
    if not stripped:
        raise MicrobenchmarkModelError("输入文件为空")
    if stripped.startswith("[") or stripped.startswith("{"):
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            value = None
        if isinstance(value, list):
            return value
        if isinstance(value, Mapping):
            rows = value.get("samples")
            if isinstance(rows, list):
                return rows
    rows: list[dict[str, Any]] = []
    json_lines = True
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if line.startswith("RV_WEIGHT_SAMPLE"):
            line = line[len("RV_WEIGHT_SAMPLE") :].lstrip(" :=\t")
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            json_lines = False
            break
        if not isinstance(value, dict):
            raise MicrobenchmarkModelError("JSONL 每行必须是 object")
        rows.append(value)
    if json_lines and rows:
        return rows
    reader = csv.DictReader(io.StringIO(text), delimiter="\t")
    if not reader.fieldnames:
        raise MicrobenchmarkModelError("无法识别输入格式")
    converted: list[dict[str, Any]] = []
    integer_fields = {
        "sequence",
        "window_sequence",
        "encoding_bytes",
        "size",
        "batch",
        "count",
        "requested_count",
        "target_count",
        "total_instruction_count",
        "timer_reads",
        "translations_during_window",
    }
    float_fields = {
        "plugin_thread_cpu_ns",
        "vcpu_thread_cpu_ns",
        "vcpu_task_clock_ns",
        "guest_ns",
        "elapsed_ns",
        "plugin_off_guest_ns",
    }
    for row in reader:
        output: dict[str, Any] = {}
        for name, value in row.items():
            if value is None or value == "":
                continue
            if name in integer_fields:
                output[name] = int(value, 0)
            elif name in float_fields:
                output[name] = float(value)
            elif name == "exact_counts":
                output[name] = json.loads(value)
            else:
                output[name] = value
        converted.append(output)
    return converted


def write_csv(result: Mapping[str, Any], path: str | Path) -> None:
    """写出稳定、扁平的逐变体权重表。"""

    fieldnames = [
        "mnemonic",
        "size",
        "encoding_key",
        "bytes",
        "aq",
        "rl",
        "csr",
        "pattern",
        "ns_per_instruction",
        "diagnostic_raw_ns_per_instruction",
        "relative_weight",
        "simultaneous_ci_low",
        "simultaneous_ci_high",
        "diagnostic_raw_simultaneous_ci_low",
        "diagnostic_raw_simultaneous_ci_high",
        "point_ci_low",
        "point_ci_high",
        "ESS",
        "runs",
        "pairs",
        "identifiability",
        "quality",
        "source",
        "quality_failures",
    ]
    with Path(path).open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fieldnames)
        writer.writeheader()
        for item in result["instructions"]:
            key = item["key"]
            simultaneous = item.get("anchor_adjusted", {}).get(
                "simultaneous_ci"
            ) or [None, None]
            diagnostic_raw_interval = item.get("simultaneous_ci") or [None, None]
            point = item.get("point_ci") or [None, None]
            writer.writerow(
                {
                    "mnemonic": key["mnemonic"],
                    "size": key["size"],
                    "encoding_key": key["encoding_key"],
                    "bytes": key["bytes"],
                    "aq": key["aq"],
                    "rl": key["rl"],
                    "csr": key["csr"],
                    "pattern": key["pattern"],
                    "ns_per_instruction": item.get(
                        "published_ns_per_instruction"
                    ),
                    "diagnostic_raw_ns_per_instruction": item.get(
                        "ns_per_instruction"
                    ),
                    "relative_weight": item.get("relative_weight"),
                    "simultaneous_ci_low": simultaneous[0],
                    "simultaneous_ci_high": simultaneous[1],
                    "diagnostic_raw_simultaneous_ci_low": (
                        diagnostic_raw_interval[0]
                    ),
                    "diagnostic_raw_simultaneous_ci_high": (
                        diagnostic_raw_interval[1]
                    ),
                    "point_ci_low": point[0],
                    "point_ci_high": point[1],
                    "ESS": item.get("ESS"),
                    "runs": item.get("runs"),
                    "pairs": item.get("pairs"),
                    "identifiability": item.get("identifiability"),
                    "quality": item.get("quality"),
                    "source": item.get("source"),
                    "quality_failures": ";".join(item.get("quality_failures", [])),
                }
            )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", help="探针 JSON/JSONL/TSV")
    parser.add_argument("--output", required=True, help="模型 JSON 输出")
    parser.add_argument("--csv", help="可选的逐指令 CSV 输出")
    parser.add_argument(
        "--bootstrap",
        type=int,
        default=PUBLICATION_MINIMUM_BOOTSTRAP_REPLICATES,
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=_default_cli_jobs(),
        help="bootstrap worker 数；默认 min(16, 可用 CPU)",
    )
    parser.add_argument("--seed", type=int, default=0x525643)
    parser.add_argument("--block-length", type=int)
    parser.add_argument("--run-block-length", type=int)
    parser.add_argument(
        "--linear-algebra-backend",
        choices=("auto", "numpy", "python"),
        default="auto",
        help="线性代数后端；正式大样本建议 numpy",
    )
    arguments = parser.parse_args(argv)
    result = fit_microbenchmark_weight_model(
        load_samples(arguments.input),
        bootstrap_replicates=arguments.bootstrap,
        bootstrap_jobs=arguments.jobs,
        seed=arguments.seed,
        block_length=arguments.block_length,
        run_block_length=arguments.run_block_length,
        linear_algebra_backend=arguments.linear_algebra_backend,
    )
    Path(arguments.output).write_text(
        json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    if arguments.csv:
        write_csv(result, arguments.csv)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
