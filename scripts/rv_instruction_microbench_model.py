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
import random
import statistics
from collections import defaultdict
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from statistics import NormalDist
from typing import Any


SCHEMA_VERSION = 2
EMPTY_CONTROL = "<empty>"


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
    block: str
    pair: str
    sequence: float
    key: _InstructionKey
    batch: int
    order: float
    plugin_delta_ns: float | None
    guest_delta_ns: float | None
    plugin_off_guest_delta_ns: float | None
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
    translation_effect: float | None
    batch_log_range: float
    residuals: list[float]
    robust_weights: list[float]
    hetero_weights: list[float]
    pairs: list[_Pair]
    predictor_names: list[str]
    irls_converged: bool
    irls_iterations: int
    design_condition_number: float


@dataclass(frozen=True)
class _BootstrapState:
    pairs: tuple[_Pair, ...]
    keys: tuple[_InstructionKey, ...]
    response_names: Mapping[_InstructionKey, str]
    controls: Mapping[_InstructionKey, _InstructionKey | None]
    block_length: int


_BOOTSTRAP_STATE: _BootstrapState | None = None


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
    return _Sample(
        run,
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
    grouped: dict[tuple[str, str], list[_Sample]] = defaultdict(list)
    for sample in samples:
        grouped[(sample.run, sample.pair)].append(sample)
    pairs: list[_Pair] = []
    assumed_empty: set[_InstructionKey] = set()
    for group_key, members in sorted(grouped.items()):
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
                block=probe.block,
                pair=probe.pair,
                sequence=(probe.sequence + baseline.sequence) / 2.0,
                key=key,
                batch=probe.batch,
                order=0.5 if probe.sequence > baseline.sequence else -0.5,
                plugin_delta_ns=plugin_delta,
                guest_delta_ns=guest_delta,
                plugin_off_guest_delta_ns=plugin_off_guest_delta,
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
    max_iterations: int = 60,
    huber_delta: float = 1.345,
    sparse_rows: Sequence[Sequence[tuple[int, float]]] | None = None,
) -> tuple[
    list[float],
    list[float],
    list[float],
    list[list[float]],
    bool,
    int,
]:
    if sparse_rows is None:
        sparse_rows = [
            [(column, value) for column, value in enumerate(row) if value != 0.0]
            for row in matrix
        ]
    robust = [1.0] * len(response)
    coefficients: list[float] = []
    inverse: list[list[float]] = []
    residuals: list[float] = []
    converged = False
    iterations = 0
    for iteration in range(1, max_iterations + 1):
        iterations = iteration
        combined = [base * weight for base, weight in zip(base_weights, robust)]
        coefficients, inverse = _wls(
            matrix, response, combined, sparse_rows=sparse_rows
        )
        residuals = [
            value - math.fsum(coefficient * item for coefficient, item in zip(coefficients, row))
            for row, value in zip(matrix, response)
        ]
        scale = max(1e-15, 1.4826 * _median_absolute_deviation(residuals))
        cutoff = huber_delta * scale
        updated = [
            1.0 if abs(value) <= cutoff else cutoff / abs(value)
            for value in residuals
        ]
        change = math.sqrt(
            math.fsum((new - old) ** 2 for new, old in zip(updated, robust))
            / len(robust)
        )
        robust = updated
        if change <= 1e-6:
            converged = True
            break
    return coefficients, residuals, robust, inverse, converged, iterations


def _design_condition_number(
    matrix: Sequence[Sequence[float]], weights: Sequence[float]
) -> float:
    """返回列标准化加权 Gram 矩阵的无穷范数条件数。"""

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


def _design(pairs: Sequence[_Pair], response_name: str) -> tuple[list[list[float]], list[float], list[str]]:
    response: list[float] = []
    for pair in pairs:
        raw = getattr(pair, response_name)
        if raw is None:
            raise MicrobenchmarkModelError("响应列不完整")
        response.append(raw / pair.target_count)
    runs = sorted({pair.run for pair in pairs})
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

    counts = [pair.target_count for pair in pairs]
    if len(set(counts)) >= 2:
        log_reference = statistics.median(math.log(value) for value in counts)
        names.append("log_batch")
        for row, count in zip(rows, counts):
            row.append(math.log(count) - log_reference)
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
    runs = sorted({pair.run for pair in pairs})
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
) -> _Fit:
    if len(pairs) < 4:
        raise MicrobenchmarkModelError("每个指令变体至少需要 4 个有效 pair")
    ordered = sorted(pairs, key=lambda pair: (pair.run, pair.sequence, pair.pair))
    matrix, response, names = _design(ordered, response_name)
    sparse_rows = [
        [(column, value) for column, value in enumerate(row) if value != 0.0]
        for row in matrix
    ]
    count_reference = statistics.median(pair.target_count for pair in ordered)
    initial_weights = [
        min(16.0, max(1.0 / 16.0, (pair.target_count / count_reference) ** 2))
        for pair in ordered
    ]
    _, initial_residuals, _, _, _, _ = _robust_fit(
        matrix, response, initial_weights, sparse_rows=sparse_rows
    )
    hetero = _heteroscedastic_weights(ordered, initial_residuals)
    coefficients, residuals, robust, inverse, converged, iterations = _robust_fit(
        matrix, response, hetero, sparse_rows=sparse_rows
    )
    combined_weights = [
        base * weight for base, weight in zip(hetero, robust)
    ]
    contrast = _contrast_for_coefficients(ordered, names)
    estimate = math.fsum(value * coefficient for value, coefficient in zip(contrast, coefficients))
    standard_error = _sandwich_standard_error(
        matrix, residuals, hetero, robust, inverse, contrast
    )
    by_name = dict(zip(names, coefficients))
    count_values = [pair.target_count for pair in ordered]
    return _Fit(
        estimate=estimate,
        standard_error=standard_error,
        order_effect=by_name.get("order_ab_ba"),
        drift_effect=by_name.get("within_run_drift"),
        batch_effect=by_name.get("log_batch"),
        translation_effect=by_name.get("translation_per_target"),
        batch_log_range=(
            math.log(max(count_values) / min(count_values))
            if min(count_values) > 0
            else 0.0
        ),
        residuals=residuals,
        robust_weights=robust,
        hetero_weights=hetero,
        pairs=ordered,
        predictor_names=names,
        irls_converged=converged,
        irls_iterations=iterations,
        design_condition_number=(
            _design_condition_number(matrix, combined_weights)
            if compute_condition
            else 1.0
        ),
    )


def _acf_ess(fit: _Fit) -> tuple[float, list[dict[str, Any]], int]:
    rows: list[dict[str, Any]] = []
    total_ess = 0.0
    recommended_block = 1
    for run in sorted({pair.run for pair in fit.pairs}):
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
    """用 Cornish-Fisher 展开近似双侧 Student-t 临界值。"""

    probability = 0.5 + confidence / 2.0
    z_value = NormalDist().inv_cdf(probability)
    if degrees <= 0:
        return math.inf
    inverse_degrees = 1.0 / degrees
    return (
        z_value
        + (z_value**3 + z_value) * inverse_degrees / 4.0
        + (5.0 * z_value**5 + 16.0 * z_value**3 + 3.0 * z_value)
        * inverse_degrees**2
        / 96.0
    )


def _wilson_upper_bound(successes: int, total: int, confidence: float) -> float:
    """返回二项比例单侧 Wilson 上置信界。"""

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


def _random_effects(
    fit: _Fit, response_name: str, confidence: float
) -> dict[str, Any]:
    estimates: list[float] = []
    variances: list[float] = []
    per_run: list[dict[str, Any]] = []
    for run in sorted({pair.run for pair in fit.pairs}):
        subset = [pair for pair in fit.pairs if pair.run == run]
        if len(subset) < 4:
            per_run.append({"run": run, "pairs": len(subset), "estimate": None})
            continue
        try:
            current = _fit_variant(subset, response_name)
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
    if len(estimates) < 2:
        return {
            "runs": per_run,
            "random_effect_estimate": estimates[0] if estimates else None,
            "tau_squared": None,
            "i_squared": None,
            "usable_runs": len(estimates),
            "total_runs": len({pair.run for pair in fit.pairs}),
            "prediction_interval": None,
            "identifiable": False,
        }
    fixed_weights = [1.0 / value for value in variances]
    fixed = math.fsum(weight * value for weight, value in zip(fixed_weights, estimates)) / math.fsum(fixed_weights)
    q = math.fsum(
        weight * (value - fixed) ** 2
        for weight, value in zip(fixed_weights, estimates)
    )
    degrees = len(estimates) - 1
    c_value = math.fsum(fixed_weights) - math.fsum(weight * weight for weight in fixed_weights) / math.fsum(fixed_weights)
    tau_squared = max(0.0, (q - degrees) / c_value) if c_value > 0.0 else 0.0
    random_weights = [1.0 / (variance + tau_squared) for variance in variances]
    random_estimate = math.fsum(
        weight * value for weight, value in zip(random_weights, estimates)
    ) / math.fsum(random_weights)
    random_standard_error = math.sqrt(1.0 / math.fsum(random_weights))
    i_squared = max(0.0, (q - degrees) / q) if q > 0.0 else 0.0
    prediction_half_width = _student_t_critical(
        confidence, degrees
    ) * math.sqrt(tau_squared + random_standard_error * random_standard_error)
    return {
        "runs": per_run,
        "random_effect_estimate": random_estimate,
        "random_effect_standard_error": random_standard_error,
        "tau_squared": tau_squared,
        "i_squared": i_squared,
        "cochran_q": q,
        "degrees_of_freedom": degrees,
        "usable_runs": len(estimates),
        "total_runs": len({pair.run for pair in fit.pairs}),
        "prediction_interval": [
            random_estimate - prediction_half_width,
            random_estimate + prediction_half_width,
        ],
        "prediction_interval_method": "DL-t-approx-with-ESS-inflated-run-SE",
        "identifiable": True,
    }


def _per_run_design_diagnostics(fit: _Fit, response_name: str) -> list[dict[str, Any]]:
    """检查每个独立 run 是否覆盖相同 count-level、顺序和可辨识设计。"""

    global_batches = {pair.batch for pair in fit.pairs}
    diagnostics: list[dict[str, Any]] = []
    for run in sorted({pair.run for pair in fit.pairs}):
        members = [pair for pair in fit.pairs if pair.run == run]
        negative = sum(pair.order < 0.0 for pair in members)
        positive = sum(pair.order > 0.0 for pair in members)
        order_balance = min(negative, positive) / len(members) if members else 0.0
        batches = {pair.batch for pair in members}
        blocks = {pair.block for pair in members}
        current: _Fit | None = None
        try:
            current = _fit_variant(members, response_name)
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


def _hierarchical_resample(
    pairs: Sequence[_Pair], block_length: int, rng: random.Random
) -> list[_Pair]:
    by_run: dict[str, list[_Pair]] = defaultdict(list)
    for pair in pairs:
        by_run[pair.run].append(pair)
    run_names = sorted(by_run)
    selected_runs = [rng.choice(run_names) for _ in run_names]
    output: list[_Pair] = []
    for run_copy, run in enumerate(selected_runs):
        members = by_run[run]
        blocks: dict[str, list[_Pair]] = defaultdict(list)
        for pair in members:
            blocks[pair.block].append(pair)
        block_names = sorted(
            blocks, key=lambda name: min(pair.sequence for pair in blocks[name])
        )
        positions = _moving_block_positions(len(block_names), block_length, rng)
        sequence = 0.0
        synthetic_run = f"bootstrap-run-{run_copy}"
        for block_copy, position in enumerate(positions):
            for pair in sorted(blocks[block_names[position]], key=lambda item: item.sequence):
                output.append(
                    replace(
                        pair,
                        run=synthetic_run,
                        block=f"bootstrap-block-{block_copy}",
                        sequence=sequence,
                    )
                )
                sequence += 1.0
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
) -> tuple[dict[Any, list[float] | None], float | None, int]:
    """以全族 max-standardized-deviation 构造同时区间。"""

    alpha = 1.0 - confidence
    standard_deviations: dict[Any, float | None] = {}
    marginal: dict[Any, list[float] | None] = {}
    for key, point in points.items():
        values = [row[key] for row in rows if key in row]
        standard_deviations[key] = (
            statistics.stdev(values) if len(values) >= 2 else None
        )
        low = _quantile(values, alpha / 2.0)
        high = _quantile(values, 1.0 - alpha / 2.0)
        marginal[key] = None if low is None or high is None else [low, high]
    eligible = [
        key
        for key, scale in standard_deviations.items()
        if scale is not None and scale > 0.0 and math.isfinite(points[key])
    ]
    max_statistics: list[float] = []
    if points and not eligible:
        max_statistics = [
            0.0 for row in rows if all(key in row for key in points)
        ]
    for row in rows:
        if not eligible:
            break
        if any(key not in row for key in eligible):
            continue
        deviations = [
            abs((row[key] - points[key]) / float(standard_deviations[key]))
            for key in eligible
        ]
        if deviations:
            max_statistics.append(max(deviations))
    critical = _quantile(max_statistics, confidence)
    intervals: dict[Any, list[float] | None] = {}
    for key, point in points.items():
        scale = standard_deviations[key]
        if scale is None:
            # 精确常量的 bootstrap 分布仍支持零宽同时区间。
            values = [row[key] for row in rows if key in row]
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
    return intervals, critical, len(max_statistics)


def _per_run_absolute_estimates(
    fits: Mapping[_InstructionKey, _Fit],
    response_names: Mapping[_InstructionKey, str],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
) -> tuple[
    dict[_InstructionKey, dict[str, float]],
    dict[_InstructionKey, set[str]],
]:
    """在主 fit 的同一批 pair 上按 run 拟合并解析 control 链。"""

    run_names = sorted({pair.run for fit in fits.values() for pair in fit.pairs})
    estimates: dict[_InstructionKey, dict[str, float]] = {
        key: {} for key in fits
    }
    incomplete: dict[_InstructionKey, set[str]] = {
        key: set() for key in fits
    }
    for run in run_names:
        contrasts: dict[_InstructionKey, float] = {}
        for key, fit in fits.items():
            members = [pair for pair in fit.pairs if pair.run == run]
            response_name = response_names[key]
            if len(members) < 4 or any(
                getattr(pair, response_name) is None for pair in members
            ):
                incomplete[key].add(run)
                continue
            try:
                current = _fit_variant(
                    members, response_name, compute_condition=False
                )
            except MicrobenchmarkModelError:
                incomplete[key].add(run)
                continue
            if not current.irls_converged:
                incomplete[key].add(run)
                continue
            contrasts[key] = current.estimate
        absolute, failures = _resolve_absolute(contrasts, controls)
        for key in fits:
            value = absolute.get(key)
            if key in failures or value is None or not math.isfinite(value):
                incomplete[key].add(run)
            else:
                estimates[key][run] = float(value)
    return estimates, incomplete


def _auxiliary_run_cluster_inference(
    fits: Mapping[_InstructionKey, _Fit],
    response_names: Mapping[_InstructionKey, str],
    controls: Mapping[_InstructionKey, _InstructionKey | None],
    comparison_modes: Mapping[_InstructionKey, str | None],
    replicate_seeds: Sequence[int],
    confidence: float,
) -> dict[str, Any]:
    """用一次性 per-run 拟合和廉价 run bootstrap 校验两套辅助时钟。"""

    primary, primary_incomplete = _per_run_absolute_estimates(
        fits, response_names, controls
    )
    guest_names = {key: "guest_delta_ns" for key in fits}
    plugin_off_names = {key: "plugin_off_guest_delta_ns" for key in fits}
    guest, guest_incomplete = _per_run_absolute_estimates(
        fits, guest_names, controls
    )
    plugin_off, plugin_off_incomplete = _per_run_absolute_estimates(
        fits, plugin_off_names, controls
    )
    metric_sources: dict[
        tuple[str, _InstructionKey], tuple[list[str], list[float], list[float]]
    ] = {}
    coverage: dict[_InstructionKey, dict[str, Any]] = {}
    for key, fit in fits.items():
        runs = sorted({pair.run for pair in fit.pairs})
        primary_complete = not primary_incomplete[key] and set(primary[key]) == set(runs)
        guest_complete = not guest_incomplete[key] and set(guest[key]) == set(runs)
        plugin_off_complete = (
            not plugin_off_incomplete[key]
            and set(plugin_off[key]) == set(runs)
        )
        coverage[key] = {
            "required_runs": len(runs),
            "primary_usable_runs": len(primary[key]),
            "guest_usable_runs": len(guest[key]),
            "plugin_off_usable_runs": len(plugin_off[key]),
            "primary_complete": primary_complete,
            "guest_complete": guest_complete,
            "plugin_off_complete": plugin_off_complete,
        }
        mode = comparison_modes.get(key)
        if primary_complete and guest_complete and mode is not None:
            metric_sources[(f"cross-clock-{mode}", key)] = (
                runs,
                [primary[key][run] for run in runs],
                [guest[key][run] for run in runs],
            )
        if guest_complete and plugin_off_complete and mode is not None:
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
        rng = random.Random(replicate_seed ^ 0xA11CE5EED)
        row: dict[tuple[str, _InstructionKey], float] = {}
        sampled_by_runs: dict[tuple[str, ...], list[int]] = {}
        for metric, (runs, denominator, numerator) in metric_sources.items():
            run_key = tuple(runs)
            indices = sampled_by_runs.setdefault(
                run_key,
                [rng.randrange(len(runs)) for _ in runs],
            )
            value = metric_value(
                metric[0],
                [denominator[index] for index in indices],
                [numerator[index] for index in indices],
            )
            if value is not None:
                row[metric] = value
        bootstrap_rows.append(row)
    intervals, critical, valid = _simultaneous_intervals(
        points, bootstrap_rows, confidence
    )
    return {
        "points": points,
        "intervals": intervals,
        "coverage": coverage,
        "critical_value": critical,
        "valid_replicates": valid,
        "requested_replicates": len(replicate_seeds),
    }


def _run_bootstrap_replicate(
    state: _BootstrapState, replicate_seed: int
) -> tuple[
    dict[_InstructionKey, float],
    dict[
        _InstructionKey,
        tuple[float | None, float | None, float | None, float | None],
    ],
] | None:
    """运行一个可独立并行的分层 moving-block bootstrap replicate。"""

    resampled = _hierarchical_resample(
        state.pairs, state.block_length, random.Random(replicate_seed)
    )
    by_key: dict[_InstructionKey, list[_Pair]] = defaultdict(list)
    for pair in resampled:
        by_key[pair.key].append(pair)
    contrasts: dict[_InstructionKey, float] = {}
    diagnostics: dict[
        _InstructionKey,
        tuple[float | None, float | None, float | None, float | None],
    ] = {}
    for key in state.keys:
        response_name = state.response_names[key]
        members = [
            pair
            for pair in by_key.get(key, [])
            if getattr(pair, response_name) is not None
        ]
        try:
            current = _fit_variant(
                members, response_name, compute_condition=False
            )
        except MicrobenchmarkModelError:
            return None
        contrasts[key] = current.estimate
        diagnostics[key] = (
            current.order_effect,
            current.drift_effect,
            current.batch_effect,
            current.translation_effect,
        )
    absolute, _failures = _resolve_absolute(contrasts, state.controls)
    resolved = {
        key: float(value)
        for key, value in absolute.items()
        if value is not None
    }
    return (resolved, diagnostics) if resolved else None


def _initialize_bootstrap_worker(state: _BootstrapState) -> None:
    global _BOOTSTRAP_STATE
    _BOOTSTRAP_STATE = state


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
    confidence: float = 0.95,
    seed: int = 0x525643,
    block_length: int | None = None,
    min_pairs: int = 30,
    min_effective_pairs: float = 20.0,
    min_runs: int = 10,
    min_count_levels: int = 3,
    min_purity: float = 0.99,
    max_relative_ci_half_width: float = 0.15,
    max_i_squared: float = 0.40,
    equivalence_margin: float = 0.10,
    min_cross_clock_ratio: float = 0.75,
    max_cross_clock_ratio: float = 1.50,
    min_plugin_off_ratio: float = 0.85,
    max_plugin_off_ratio: float = 1.15,
    max_zero_cost_ci_upper_ns: float = 0.15,
    max_translation_density: float = 0.002,
    max_severe_outlier_fraction: float = 0.10,
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
    if block_length is not None and (
        isinstance(block_length, bool) or not isinstance(block_length, int) or block_length <= 0
    ):
        raise MicrobenchmarkModelError("block_length 必须是正整数")
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
        not math.isfinite(max_severe_outlier_fraction)
        or not 0.0 < max_severe_outlier_fraction < 0.5
    ):
        raise MicrobenchmarkModelError("严重异常比例阈值必须位于 (0,0.5)")
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
    bootstrap_rows: list[dict[_InstructionKey, float]] = []
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
        block_length=selected_block,
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
                for name, value in zip(
                    ("order", "drift", "batch", "translation"), values
                ):
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
    simultaneous_ci, critical, simultaneous_valid = _simultaneous_intervals(
        finite_points, bootstrap_rows, confidence
    )
    for key in fits:
        simultaneous_ci.setdefault(key, None)

    diagnostic_points: dict[tuple[_InstructionKey, str], float] = {}
    for key, fit in fits.items():
        for name, value in (
            ("order", fit.order_effect),
            ("drift", fit.drift_effect),
            ("batch", fit.batch_effect),
        ):
            if value is not None:
                diagnostic_points[(key, name)] = value
    diagnostic_intervals, diagnostic_critical, diagnostic_valid = (
        _simultaneous_intervals(
            diagnostic_points, diagnostic_rows, confidence
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
        confidence,
    )

    heterogeneity = {
        key: _random_effects(
            fit,
            response_names[key],
            confidence,
        )
        for key, fit in fits.items()
    }
    per_run_design = {
        key: _per_run_design_diagnostics(fit, response_names[key])
        for key, fit in fits.items()
    }
    items: list[dict[str, Any]] = []
    fatal_codes = {
        "fit-failed",
        "absolute-reference-unresolved",
        "simultaneous-ci-missing",
    }
    for key in sorted(grouped):
        fit = fits.get(key)
        if fit is None:
            items.append(
                {
                    "key": key.public(),
                    "ns_per_instruction": None,
                    "simultaneous_ci": None,
                    "point_ci": None,
                    "ESS": 0.0,
                    "runs": len({pair.run for pair in grouped[key]}),
                    "pairs": len(grouped[key]),
                    "identifiability": "not-identifiable",
                    "quality": "not-identifiable",
                    "source": "unfitted-paired-probe",
                    "quality_failures": ["fit-failed", fit_failures.get(key, "unknown")],
                }
            )
            continue
        members = fit.pairs
        raw_point = point_absolute.get(key)
        raw_interval = simultaneous_ci[key]
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
        runs = len({pair.run for pair in members})
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
        huber_downweighted = sum(
            weight < 1.0 - 1e-12 for weight in fit.robust_weights
        ) / len(members)
        severe_outliers = sum(
            weight < 0.25 for weight in fit.robust_weights
        ) / len(members)
        severe_outlier_count = sum(
            weight < 0.25 for weight in fit.robust_weights
        )
        severe_outlier_upper_bound = _wilson_upper_bound(
            severe_outlier_count, len(members), confidence
        )
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
        if len(bootstrap_rows) < 999:
            failures.append("insufficient-bootstrap-replicates")
        if bootstrap_replicates > 0 and bootstrap_valid_fraction < 0.99:
            failures.append("insufficient-bootstrap-valid-fraction")
        if order_balance < 0.35:
            failures.append("ab-ba-imbalance")
        if severe_outlier_upper_bound > max_severe_outlier_fraction:
            failures.append("too-many-severe-outliers")
        if meta["i_squared"] is None:
            failures.append("cross-run-heterogeneity-unavailable")
        if meta["usable_runs"] != runs:
            failures.append("cross-run-coverage-incomplete")
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
                if (
                    meta["i_squared"] is not None
                    and meta["i_squared"] > max_i_squared
                    and prediction_half_width > practical_half_width
                ):
                    failures.append("cross-run-heterogeneity-high")
        meta["prediction_interval_half_width"] = prediction_half_width
        meta["relative_prediction_interval_half_width"] = (
            prediction_half_width / raw_point
            if prediction_half_width is not None
            and raw_point is not None
            and raw_point > 0.0
            else None
        )

        diagnostic_ci: dict[str, list[float] | None] = {
            name: diagnostic_intervals.get((key, name))
            for name in ("order", "drift", "batch")
        }
        diagnostic_ci["translation"] = None
        if raw_point is not None:
            margin = (
                max_zero_cost_ci_upper_ns
                if zero_cost_equivalent
                else equivalence_margin * abs(raw_point)
            )
            for name, code in (
                ("order", "order-effect-not-equivalent"),
                ("drift", "drift-effect-not-equivalent"),
            ):
                current = diagnostic_ci[name]
                if current is None or current[0] < -margin or current[1] > margin:
                    failures.append(code)
            batch_interval = diagnostic_ci["batch"]
            batch_multiplier = fit.batch_log_range
            if (
                batch_interval is None
                or batch_interval[0] * batch_multiplier < -margin
                or batch_interval[1] * batch_multiplier > margin
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
        auxiliary_valid_fraction = (
            auxiliary_inference["valid_replicates"]
            / auxiliary_inference["requested_replicates"]
            if auxiliary_inference["requested_replicates"] > 0
            else 0.0
        )
        cross_clock_complete = (
            auxiliary_coverage["primary_complete"]
            and auxiliary_coverage["guest_complete"]
            and auxiliary_coverage["primary_usable_runs"] == runs
            and auxiliary_coverage["guest_usable_runs"] == runs
        )
        selected_cross_interval = (
            cross_clock_difference_ci
            if zero_cost_equivalent
            else cross_clock_ratio_ci
        )
        if not cross_clock_complete or selected_cross_interval is None:
            failures.append("cross-clock-check-unavailable")
            cross_clock_status = "unavailable"
        elif bootstrap_replicates > 0 and auxiliary_valid_fraction < 0.99:
            failures.append("cross-clock-check-unavailable")
            cross_clock_status = "insufficient-bootstrap-valid-fraction"
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
            auxiliary_coverage["guest_complete"]
            and auxiliary_coverage["plugin_off_complete"]
            and auxiliary_coverage["guest_usable_runs"] == runs
            and auxiliary_coverage["plugin_off_usable_runs"] == runs
        )
        selected_plugin_off_interval = (
            plugin_off_difference_ci
            if zero_cost_equivalent
            else plugin_off_ratio_ci
        )
        if not plugin_off_complete or selected_plugin_off_interval is None:
            failures.append("plugin-off-check-unavailable")
            plugin_off_status = "unavailable"
        elif bootstrap_replicates > 0 and auxiliary_valid_fraction < 0.99:
            failures.append("plugin-off-check-unavailable")
            plugin_off_status = "insufficient-bootstrap-valid-fraction"
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
            "zero_cost_equivalent": zero_cost_equivalent,
            "ESS": ess,
            "runs": runs,
            "pairs": len(members),
            "total_target_count": sum(pair.target_count for pair in members),
            "count_levels": count_levels,
            "minimum_pairs_per_level": min(per_level, default=0),
            "purity_q05": purity_q05,
            "translation_density_q95": translation_density_q95,
            "translation_contaminated_pairs_excluded": excluded_by_key.get(
                key, 0
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
            "severe_outlier_fraction_wilson_upper": severe_outlier_upper_bound,
            "maximum_severe_outlier_fraction": max_severe_outlier_fraction,
            "order_balance": order_balance,
            "fit_diagnostics": {
                "irls_converged": fit.irls_converged,
                "irls_iterations": fit.irls_iterations,
                "design_condition_number": _json_finite(
                    fit.design_condition_number
                ),
                "per_run": run_design,
            },
            "effects": {
                "ab_ba_difference": fit.order_effect,
                "within_run_end_minus_start": fit.drift_effect,
                "per_log_batch": fit.batch_effect,
                "ns_per_translation_event": fit.translation_effect,
                "batch_log_range": fit.batch_log_range,
                "bootstrap_ci": diagnostic_ci,
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

    positive = [
        item["ns_per_instruction"]
        for item in items
        if isinstance(item.get("ns_per_instruction"), (int, float))
        and item["ns_per_instruction"] > 0.0
        and item.get("quality") == "high-confidence"
    ]
    reference = statistics.median(positive) if positive else None
    for item in items:
        value = item.get("ns_per_instruction")
        conservative = item.get("conservative_ns_per_instruction")
        item["relative_weight"] = (
            value / reference
            if reference is not None
            and item.get("quality") == "high-confidence"
            and isinstance(value, (int, float))
            and value >= 0.0
            else None
        )
        item["conservative_relative_weight"] = (
            conservative / reference
            if reference is not None
            and item.get("quality") == "high-confidence"
            and isinstance(conservative, (int, float))
            and conservative > 0.0
            else None
        )

    # 不跨 memory/branch/dependency pattern 静默平均；仅在同 mnemonic+size 只有
    # 一个 pattern，或各 pattern 通过等价性门禁时给推荐权重。
    recommendations: list[dict[str, Any]] = []
    aggregate: dict[tuple[str, int], list[dict[str, Any]]] = defaultdict(list)
    for item in items:
        key = item["key"]
        aggregate[(key["mnemonic"], key["size"])].append(item)
    for (mnemonic, size), members in sorted(aggregate.items()):
        usable = [
            item for item in members
            if item["ns_per_instruction"] is not None
            and item["quality"] == "high-confidence"
        ]
        if len(members) == 1 and usable:
            recommendation = usable[0]["ns_per_instruction"]
            source = "single-context"
        elif len(usable) == len(members) and usable:
            values = [float(item["ns_per_instruction"]) for item in usable]
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

    result = {
        "schema_version": SCHEMA_VERSION,
        "model": "paired-huber-heteroscedastic-hierarchical-moving-block-max-standardized-deviation",
        "primary_response": "marker-only-qemu-vcpu-thread-cpu-time",
        "instruction_key": "raw-encoding+semantic-decoding+execution-pattern",
        "confidence": confidence,
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
            "familywise_confidence": confidence,
            "requested_replicates": bootstrap_replicates,
            "valid_replicates": len(bootstrap_rows),
            "valid_fraction": bootstrap_valid_fraction,
            "minimum_valid_fraction": 0.99,
            "worker_processes": bootstrap_jobs,
            "quantile_probability_monte_carlo_se": (
                math.sqrt(
                    confidence * (1.0 - confidence) / len(bootstrap_rows)
                )
                if bootstrap_rows
                else None
            ),
            "critical_value": critical,
            "complete_max_statistic_replicates": simultaneous_valid,
            "run_is_highest_cluster": True,
            "block_length": selected_block,
            "automatic_block_length": automatic_block,
            "block_length_unit": "probe-round-blocks",
        },
        "diagnostic_simultaneous_inference": {
            "method": "joint-instruction-and-effect max-standardized-deviation",
            "familywise_confidence": confidence,
            "critical_value": diagnostic_critical,
            "complete_replicates": diagnostic_valid,
            "effects": ["order", "drift", "batch"],
        },
        "auxiliary_consistency_inference": {
            "method": "paired-per-run-estimate run-cluster bootstrap max-standardized-deviation",
            "familywise_confidence": confidence,
            "requested_replicates": auxiliary_inference[
                "requested_replicates"
            ],
            "valid_replicates": auxiliary_inference["valid_replicates"],
            "valid_fraction": (
                auxiliary_inference["valid_replicates"]
                / auxiliary_inference["requested_replicates"]
                if auxiliary_inference["requested_replicates"] > 0
                else 0.0
            ),
            "critical_value": auxiliary_inference["critical_value"],
            "requires_same_pairs_and_all_primary_runs": True,
        },
        "quality_thresholds": {
            "minimum_pairs": min_pairs,
            "minimum_effective_pairs": min_effective_pairs,
            "minimum_independent_runs": min_runs,
            "minimum_count_levels": min_count_levels,
            "minimum_instruction_purity": min_purity,
            "maximum_relative_simultaneous_ci_half_width": max_relative_ci_half_width,
            "maximum_i_squared": max_i_squared,
            "maximum_design_condition_number": 1e8,
            "irls_weight_rms_tolerance": 1e-6,
            "irls_maximum_iterations": 60,
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
            "minimum_bootstrap_replicates": 999,
            "minimum_bootstrap_valid_fraction": 0.99,
            "severe_huber_weight_threshold": 0.25,
            "maximum_severe_outlier_fraction": max_severe_outlier_fraction,
            "severe_outlier_fraction_gate": (
                "one-sided-Wilson-upper-bound-at-model-confidence"
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
        "relative_weight",
        "simultaneous_ci_low",
        "simultaneous_ci_high",
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
            simultaneous = item.get("simultaneous_ci") or [None, None]
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
                    "ns_per_instruction": item.get("ns_per_instruction"),
                    "relative_weight": item.get("relative_weight"),
                    "simultaneous_ci_low": simultaneous[0],
                    "simultaneous_ci_high": simultaneous[1],
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
    parser.add_argument("--bootstrap", type=int, default=999)
    parser.add_argument("--jobs", type=int, default=1)
    parser.add_argument("--seed", type=int, default=0x525643)
    parser.add_argument("--block-length", type=int)
    arguments = parser.parse_args(argv)
    result = fit_microbenchmark_weight_model(
        load_samples(arguments.input),
        bootstrap_replicates=arguments.bootstrap,
        bootstrap_jobs=arguments.jobs,
        seed=arguments.seed,
        block_length=arguments.block_length,
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
