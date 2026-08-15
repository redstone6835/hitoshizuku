#!/usr/bin/env python3
"""把客体探针日志与 QEMU segment 窗口合并为统计模型输入。"""

from __future__ import annotations

import argparse
import json
import shlex
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any

from riscv_instruction_encoding import decode_riscv64_instruction


class MergeError(ValueError):
    pass


_DIFFERENTIAL_METADATA_FIELDS = (
    "probe_contract",
    "operand_set",
    "calibration_profile",
    "suite",
    "contrast",
    "differential_variant",
    "context",
)

_DIFFERENTIAL_PROBE_CONTRACT = "mygo.riscv-instruction-weight-differential.v2"
_DIFFERENTIAL_OPERAND_SET = "nondegenerate-7fedcba987654321-by-1f123"
_CALIBRATION_PROFILES = {
    "standard-v2": (1, 4, 16),
    "long-window-v1": (16, 64, 256),
}
_DIFFERENTIAL_SLOTS_PER_BLOCK = 1024
_DIV_REM_INSTRUCTIONS = {
    "div",
    "divu",
    "rem",
    "remu",
    "divw",
    "divuw",
    "remw",
    "remuw",
}

_INTERACTION_CONTEXTS = {
    ("div-rem-alternation", "homogeneous-div-reset"): (
        "div",
        "homogeneous-reset",
        "reference",
    ),
    ("div-rem-alternation", "alternating-with-rem-reset"): (
        "div",
        "alternating-rem-div-reset",
        "alternating",
    ),
    ("rem-div-alternation", "homogeneous-rem-reset"): (
        "rem",
        "homogeneous-reset",
        "reference",
    ),
    ("rem-div-alternation", "alternating-with-div-reset"): (
        "rem",
        "alternating-div-rem-reset",
        "alternating",
    ),
}


def _metadata_token(value: str) -> bool:
    return isinstance(value, str) and bool(value) and all(
        character.isascii()
        and (character.isalnum() or character in {".", "_", "-", ":"})
        for character in value
    )


def _expected_calibration_multiplier(profile: str, level: int) -> int:
    try:
        return _CALIBRATION_PROFILES[profile][level]
    except (KeyError, IndexError) as error:
        raise MergeError("version=2 calibration count_level 非法") from error


def _validate_calibration_batches(
    rows: Sequence[Mapping[str, str]], metadata: Mapping[str, Any]
) -> None:
    if metadata["suite"] not in {
        "differential-calibration-v2",
        "stability-anchor-v1",
    }:
        return
    profile = str(metadata["calibration_profile"])
    base_candidates: set[int] = set()
    levels_by_round: dict[str, dict[int, list[Mapping[str, str]]]] = {}
    for row in rows:
        level = _integer(row, "count_level")
        multiplier = _expected_calibration_multiplier(profile, level)
        blocks = _integer(row, "blocks")
        slots_per_block = _integer(row, "slots_per_block")
        requested_count = _integer(row, "requested_count")
        if slots_per_block != _DIFFERENTIAL_SLOTS_PER_BLOCK:
            raise MergeError(
                "version=2 calibration slots_per_block 与探针契约不一致"
            )
        if requested_count != blocks * slots_per_block:
            raise MergeError(
                "version=2 calibration requested_count 与 blocks/slots 不闭合"
            )
        if blocks == 0 or blocks % multiplier != 0:
            raise MergeError("version=2 calibration blocks 与 profile 不一致")
        base_candidates.add(blocks // multiplier)
        round_id = row.get("block_id", "")
        if not round_id:
            raise MergeError("version=2 calibration 缺少 block_id")
        levels_by_round.setdefault(round_id, {}).setdefault(level, []).append(row)
    if len(base_candidates) != 1:
        raise MergeError("version=2 calibration 三档 blocks 不共享 base_blocks")
    expected_levels = set(range(len(_CALIBRATION_PROFILES[profile])))
    for round_id, by_level in levels_by_round.items():
        if metadata["suite"] == "stability-anchor-v1" and set(by_level) == {1}:
            for level_rows in by_level.values():
                roles = {row.get("role") for row in level_rows}
                pair_ids = {row.get("pair_id") for row in level_rows}
                if len(level_rows) != 2 or roles != {"probe", "baseline"} or len(pair_ids) != 1:
                    raise MergeError("stability anchor 必须是唯一 probe/baseline 对")
            continue
        if set(by_level) != expected_levels:
            raise MergeError(
                f"version=2 calibration round={round_id!r} 三档 count_level 不完整"
            )
        for level, level_rows in by_level.items():
            roles = {row.get("role") for row in level_rows}
            pair_ids = {row.get("pair_id") for row in level_rows}
            if (
                len(level_rows) != 2
                or roles != {"probe", "baseline"}
                or len(pair_ids) != 1
                ):
                    raise MergeError(
                        "version=2 calibration 每个 round/count_level 必须恰有一对 "
                        f"probe/baseline，round={round_id!r} level={level}"
                    )
    if metadata["suite"] == "stability-anchor-v1":
        try:
            rounds = sorted(int(round_id, 10) for round_id in levels_by_round)
        except ValueError as error:
            raise MergeError("stability anchor block_id 必须是十进制轮次") from error
        if len(rounds) < 3 or rounds != list(range(rounds[-1] + 1)):
            raise MergeError("stability anchor 必须连续覆盖首部、主体轮次和尾部")
        if (
            set(levels_by_round[str(rounds[0])]) != {1}
            or set(levels_by_round[str(rounds[-1])]) != {1}
            or any(
                set(levels_by_round[str(round_id)]) != expected_levels
                for round_id in rounds[1:-1]
            )
        ):
            raise MergeError(
                "stability anchor 必须由首尾中档锚点和主体三档重复组成"
            )
        for round_id, by_level in levels_by_round.items():
            expected_position = (
                "head"
                if int(round_id, 10) == rounds[0]
                else "tail"
                if int(round_id, 10) == rounds[-1]
                else "body"
            )
            if any(
                row.get("anchor_position") != expected_position
                for level_rows in by_level.values()
                for row in level_rows
            ):
                raise MergeError(
                    "stability anchor 的 anchor_position 与轮次布局不一致"
                )


def _validate_differential_semantics(
    row: Mapping[str, str], metadata: Mapping[str, Any]
) -> None:
    if metadata["probe_contract"] != _DIFFERENTIAL_PROBE_CONTRACT:
        raise MergeError("version=2 客体样本 probe_contract 不受支持")
    if metadata["operand_set"] != _DIFFERENTIAL_OPERAND_SET:
        raise MergeError("version=2 客体样本 operand_set 不受支持")
    calibration_profile = metadata["calibration_profile"]
    if calibration_profile not in _CALIBRATION_PROFILES:
        raise MergeError("version=2 客体样本 calibration_profile 不受支持")
    blocks = _integer(row, "blocks")
    slots_per_block = _integer(row, "slots_per_block")
    requested_count = _integer(row, "requested_count")
    if blocks <= 0:
        raise MergeError("version=2 客体样本 blocks 必须为正数")
    if slots_per_block != _DIFFERENTIAL_SLOTS_PER_BLOCK:
        raise MergeError("version=2 客体样本 slots_per_block 与探针契约不一致")
    if requested_count <= 0 or requested_count != blocks * slots_per_block:
        raise MergeError(
            "version=2 客体样本 requested_count 与 blocks/slots 不闭合"
        )

    fixed_fields = {
        "encoding_bytes": "4",
        "control_instruction": "empty-call",
    }
    suite = metadata["suite"]
    if suite == "differential-calibration-v2":
        fixed_fields.update(
            {
                "baseline_instruction": "empty",
                "baseline_encoding_bytes": "0",
            }
        )

        multiplier = _expected_calibration_multiplier(
            calibration_profile, _integer(row, "count_level")
        )
        if blocks == 0 or blocks % multiplier != 0:
            raise MergeError("version=2 calibration blocks 与 profile 不一致")
    else:
        fixed_fields.update(
            {
                "baseline_instruction": "nop",
                "baseline_encoding_bytes": "4",
            }
        )
        # calibration_profile 是整次 differential-v2 实验的根校准策略标签，
        # 不是每个上下文的 batch profile。长窗口只放大 nop -> empty
        # calibration，其余上下文仍严格使用 standard-v2 的 1/4/16。
        multiplier = _expected_calibration_multiplier(
            calibration_profile
            if suite == "stability-anchor-v1"
            else "standard-v2",
            _integer(row, "count_level"),
        )
        if blocks % multiplier != 0:
            raise MergeError(
                "version=2 non-calibration blocks 与 standard profile 不一致"
            )
    for name, expected in fixed_fields.items():
        if row.get(name) != expected:
            raise MergeError(
                f"version=2 客体样本 {name}={row.get(name)!r}，期望 {expected!r}"
            )

    instruction = row.get("instruction")
    pattern = row.get("pattern")
    contrast = metadata["contrast"]
    context = metadata["context"]
    variant = metadata["differential_variant"]
    expected: tuple[str, str, str] | None = None
    if suite == "differential-calibration-v2":
        if (contrast, context) == ("nop-reference", "independent-nop"):
            expected = ("nop", "independent", "reference")
    elif suite == "stability-anchor-v1":
        if (contrast, context) == (
            "positive-div-anchor",
            "repeated-positive-anchor",
        ):
            expected = ("div", "stability-anchor-positive-div", "anchor")
    elif suite == "div-rem-dataflow-v2" and instruction in _DIV_REM_INSTRUCTIONS:
        if contrast != f"{instruction}-dataflow":
            raise MergeError("version=2 dataflow contrast 与 instruction 不一致")
        if context == "evolving-dependency-chain":
            expected = (instruction, "dependency-chain", "reference")
        elif context == "per-slot-reset-nondegenerate":
            expected = (
                instruction,
                "independent-reset",
                "independent",
            )
    elif suite == "mixed-tb-interaction-v2":
        expected = _INTERACTION_CONTEXTS.get((contrast, context))
    if expected is None:
        raise MergeError("version=2 客体样本 suite/contrast/context 组合不受支持")
    if (instruction, pattern, variant) != expected:
        raise MergeError(
            "version=2 客体样本 instruction/pattern/differential_variant "
            "与差分上下文不一致"
        )

    role = row.get("role")
    if role not in {"probe", "baseline"}:
        raise MergeError("version=2 客体样本 role 必须是 probe 或 baseline")
    expected_executed = (
        instruction
        if role == "probe"
        else "empty"
        if suite == "differential-calibration-v2"
        else "nop"
    )
    if row.get("executed_instruction") != expected_executed:
        raise MergeError(
            "version=2 客体样本 executed_instruction 与 role/instruction 不一致"
        )


def _guest_metadata(row: Mapping[str, str]) -> dict[str, Any]:
    raw_version = row.get("version", "1")
    if not isinstance(raw_version, str):
        raise MergeError(f"客体样本 version={raw_version!r} 非法")
    if raw_version not in {"1", "2"}:
        raise MergeError(f"客体样本 version={raw_version!r} 不受支持")
    version = int(raw_version, 10)
    if version == 1:
        return {}
    if version != 2:
        raise MergeError(f"客体样本 version={version} 不受支持")

    metadata: dict[str, Any] = {"probe_version": version}
    for name in _DIFFERENTIAL_METADATA_FIELDS:
        # 早期 differential-v2 日志没有 profile 字段；它们只使用 1/4/16
        # 标准窗口。长窗口必须显式声明，且会由 blocks 契约再次校验。
        value = row.get(name, "standard-v2" if name == "calibration_profile" else None)
        if not isinstance(value, str) or not _metadata_token(value):
            raise MergeError(f"version=2 客体样本缺少或含非法 {name}")
        metadata[name] = value
    anchor_position = row.get("anchor_position")
    if metadata["suite"] == "stability-anchor-v1":
        if anchor_position not in {"head", "body", "tail"}:
            raise MergeError("stability anchor 缺少合法 anchor_position")
    elif anchor_position not in {None, "not-anchor"}:
        raise MergeError("非 anchor 样本不得声明 anchor_position")
    metadata["anchor_position"] = anchor_position or "not-anchor"
    _validate_differential_semantics(row, metadata)
    return metadata


def _metadata_by_pair(
    guest_rows: Sequence[dict[str, str]],
) -> dict[tuple[str, str], dict[str, Any]]:
    result: dict[tuple[str, str], dict[str, Any]] = {}
    for row in guest_rows:
        pair = (row.get("run_id", ""), row.get("pair_id", ""))
        metadata = _guest_metadata(row)
        previous = result.get(pair)
        if previous is not None and previous != metadata:
            raise MergeError(
                f"pair={pair!r} 的 version/差分元数据不一致"
            )
        result[pair] = metadata
    by_profile: dict[tuple[str, str, str, str], list[Mapping[str, str]]] = {}
    for row in guest_rows:
        metadata = _guest_metadata(row)
        if not metadata:
            continue
        key = (
            str(row.get("run_id", "")),
            str(metadata["suite"]),
            str(metadata["contrast"]),
            str(metadata["calibration_profile"]),
        )
        by_profile.setdefault(key, []).append(row)
    for rows in by_profile.values():
        _validate_calibration_batches(rows, _guest_metadata(rows[0]))
    profiles = {
        str(metadata["calibration_profile"])
        for metadata in result.values()
        if metadata
    }
    if len(profiles) > 1:
        raise MergeError("同一输入不得混合 calibration_profile")
    return result


def _parse_guest(path: Path) -> list[dict[str, str]]:
    samples: list[dict[str, str]] = []
    for line_number, raw_line in enumerate(
        path.read_text(encoding="utf-8", errors="replace").splitlines(), 1
    ):
        if not raw_line.startswith("RV_WEIGHT_SAMPLE "):
            continue
        row: dict[str, str] = {}
        for token in shlex.split(raw_line[len("RV_WEIGHT_SAMPLE ") :]):
            if "=" not in token:
                raise MergeError(f"{path}:{line_number}: 非 key=value 字段")
            name, value = token.split("=", 1)
            if not name or name in row:
                raise MergeError(f"{path}:{line_number}: 重复或空字段 {name!r}")
            row[name] = value
        samples.append(row)
    if not samples:
        raise MergeError(f"{path}: 未找到 RV_WEIGHT_SAMPLE")
    return samples


def _parse_plugin(
    path: Path, *, expected_mode: str | None = None
) -> dict[int, dict[str, Any]]:
    windows: dict[int, dict[str, Any]] = {}
    header = None
    footer = None
    for line_number, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw_line.strip():
            continue
        try:
            row = json.loads(raw_line)
        except json.JSONDecodeError as error:
            raise MergeError(f"{path}:{line_number}: 非法 JSON") from error
        if not isinstance(row, dict):
            raise MergeError(f"{path}:{line_number}: 记录必须是 object")
        record_type = row.get("type")
        if record_type == "header":
            if header is not None:
                raise MergeError(f"{path}: header 重复")
            header = row
        elif record_type == "footer":
            if footer is not None:
                raise MergeError(f"{path}: footer 重复")
            footer = row
        elif record_type == "window":
            sequence = row.get("sequence")
            if not isinstance(sequence, int) or sequence <= 0 or sequence in windows:
                raise MergeError(f"{path}:{line_number}: 非法或重复 sequence")
            windows[sequence] = row
        else:
            raise MergeError(f"{path}:{line_number}: 未知 type={record_type!r}")
    if header is None or footer is None:
        raise MergeError(f"{path}: 缺少 header/footer")
    if expected_mode is not None:
        if expected_mode not in {"timing", "validation"}:
            raise MergeError(f"内部错误：未知插件模式 {expected_mode!r}")
        for owner, row in (("header", header), ("footer", footer)):
            if row.get("schema") != "mygo.riscv-instruction-weight-window.v2":
                raise MergeError(f"{path}: {owner}.schema 不是 v2")
            if row.get("mode") != expected_mode:
                raise MergeError(
                    f"{path}: {owner}.mode={row.get('mode')!r}，"
                    f"期望 {expected_mode!r}"
                )
            if row.get("cpu_scope") != "full-vcpu-thread":
                raise MergeError(f"{path}: {owner}.cpu_scope 非法")
        expected_counts = expected_mode == "validation"
        if header.get("counts_available") is not expected_counts:
            raise MergeError(f"{path}: header.counts_available 与模式不一致")
        if footer.get("counts_available") is not expected_counts:
            raise MergeError(f"{path}: footer.counts_available 与模式不一致")
        for sequence, window in windows.items():
            if window.get("schema") != header.get("schema"):
                raise MergeError(f"{path}: window {sequence} schema 不一致")
            if window.get("mode") != expected_mode:
                raise MergeError(f"{path}: window {sequence} mode 不一致")
            if window.get("counts_available") is not expected_counts:
                raise MergeError(
                    f"{path}: window {sequence} counts_available 不一致"
                )
            translations = window.get("translations_during_window")
            if (
                isinstance(translations, bool)
                or not isinstance(translations, int)
                or translations < 0
            ):
                raise MergeError(
                    f"{path}: window {sequence} translations_during_window 非法"
                )
            scoped_translations = window.get(
                "scoped_translations_during_window"
            )
            if scoped_translations is not None and (
                isinstance(scoped_translations, bool)
                or not isinstance(scoped_translations, int)
                or scoped_translations < 0
            ):
                raise MergeError(
                    f"{path}: window {sequence} scoped translation 非法"
                )
            guest_trap_entries = window.get(
                "guest_trap_entries_during_window"
            )
            if (
                isinstance(guest_trap_entries, bool)
                or not isinstance(guest_trap_entries, int)
                or guest_trap_entries < 0
            ):
                raise MergeError(
                    f"{path}: window {sequence} "
                    "guest_trap_entries_during_window 非法"
                )
            if expected_mode == "timing" and (
                window.get("instruction_count") is not None
                or window.get("counts") is not None
            ):
                raise MergeError(f"{path}: timing window 不得携带计数")
            if expected_mode == "validation" and not isinstance(
                window.get("counts"), list
            ):
                raise MergeError(f"{path}: validation window 缺少计数")
    if footer.get("active_at_exit") is not False:
        raise MergeError(f"{path}: QEMU 退出时仍在 segment 中")
    for field in ("nested_starts", "inactive_stops", "translation_failures", "timer_failures"):
        if footer.get(field) != 0:
            raise MergeError(f"{path}: footer.{field}={footer.get(field)!r}")
    if footer.get("windows") != len(windows):
        raise MergeError(f"{path}: footer 窗口数不闭合")
    return windows


def _integer(row: Mapping[str, str], name: str) -> int:
    try:
        value = int(row[name], 0)
    except (KeyError, ValueError) as error:
        raise MergeError(f"客体样本缺少或错误的整数 {name}") from error
    if value < 0:
        raise MergeError(f"客体样本 {name} 不能为负")
    return value


def _mnemonic(value: str) -> str:
    token = value.strip().lower().replace(".aq.rl", ".aqrl")
    return token[2:] if token.startswith("c.") else token


def _descriptor_key(descriptor: Mapping[str, Any]) -> str:
    return str(descriptor["encoding_key"])


def _descriptor_names(descriptor: Mapping[str, Any]) -> set[str]:
    names = {
        _mnemonic(str(descriptor.get("mnemonic", ""))),
        _mnemonic(str(descriptor.get("canonical_mnemonic", ""))),
    }
    modifiers = descriptor.get("encoding_modifiers", [])
    if isinstance(modifiers, list):
        for modifier in modifiers:
            if isinstance(modifier, str) and modifier.startswith("form="):
                names.add(_mnemonic(modifier.removeprefix("form=")))
    names.discard("")
    return names


def _validated_counts(window: Mapping[str, Any]) -> list[dict[str, Any]]:
    raw_counts = window.get("counts")
    if not isinstance(raw_counts, list):
        raise MergeError("插件窗口缺少 counts")
    result: list[dict[str, Any]] = []
    total = 0
    seen: set[tuple[int, str]] = set()
    for descriptor in raw_counts:
        if not isinstance(descriptor, dict):
            raise MergeError("插件 counts 项必须是 object")
        size = descriptor.get("size")
        raw_bytes = descriptor.get("bytes")
        mnemonic = descriptor.get("mnemonic")
        count = descriptor.get("count")
        if (
            not isinstance(size, int)
            or size not in {2, 4}
            or not isinstance(raw_bytes, str)
            or len(raw_bytes) != size * 2
            or any(character not in "0123456789abcdef" for character in raw_bytes)
            or not isinstance(mnemonic, str)
            or not mnemonic
            or not isinstance(count, int)
            or count <= 0
        ):
            raise MergeError("插件 counts 项字段非法")
        identity = (size, raw_bytes)
        if identity in seen:
            raise MergeError("插件窗口中原始 encoding 重复")
        seen.add(identity)
        total += count
        decoded = decode_riscv64_instruction(
            bytes.fromhex(raw_bytes), mnemonic
        )
        annotated = dict(descriptor)
        annotated.update(
            {
                "encoding_key": decoded.key,
                "canonical_mnemonic": decoded.mnemonic,
                "extension": decoded.extension,
                "encoding_recognized": decoded.recognized,
                "encoding_modifiers": list(decoded.modifiers),
            }
        )
        result.append(annotated)
    if window.get("instruction_count") != total:
        raise MergeError("插件窗口 instruction_count 不闭合")
    return sorted(
        result,
        key=lambda item: (
            str(item["encoding_key"]),
            int(item["size"]),
            str(item["bytes"]),
        ),
    )


def _select_descriptor(
    counts: Iterable[Mapping[str, Any]],
    *,
    instruction: str,
    size: int,
    expected_count: int,
    counterpart_counts: Iterable[Mapping[str, Any]],
) -> dict[str, Any]:
    candidates = [item for item in counts if item.get("size") == size]
    exact_name = [
        item
        for item in candidates
        if _mnemonic(instruction) in _descriptor_names(item)
    ]
    if not exact_name:
        summary = [
            (
                item.get("mnemonic"),
                item.get("canonical_mnemonic"),
                item.get("size"),
                item.get("bytes"),
                item.get("count"),
            )
            for item in candidates
        ]
        raise MergeError(
            f"{instruction}/{size} 没有 mnemonic 或 canonical mnemonic 匹配，"
            f"禁止按计数猜测，候选={summary!r}"
        )
    candidates = exact_name
    counterpart = {
        (int(item["size"]), str(item["bytes"])): int(item["count"])
        for item in counterpart_counts
    }
    exact_contrast = [
        item
        for item in candidates
        if int(item["count"])
        - counterpart.get((int(item["size"]), str(item["bytes"])), 0)
        == expected_count
    ]
    if not exact_contrast:
        summary = [
            (
                item.get("mnemonic"),
                item.get("bytes"),
                item.get("count"),
                counterpart.get((int(item["size"]), str(item["bytes"])), 0),
            )
            for item in candidates
        ]
        raise MergeError(
            f"{instruction}/{size} 的 raw encoding 对比计数必须精确等于 "
            f"requested_count={expected_count} 的候选，候选={summary!r}"
        )
    candidates = exact_contrast
    if len(candidates) != 1:
        summary = [
            (item.get("mnemonic"), item.get("size"), item.get("bytes"), item.get("count"))
            for item in candidates
        ]
        raise MergeError(
            f"无法唯一识别 {instruction}/{size} 的目标 encoding，候选={summary!r}"
        )
    return dict(candidates[0])


def _canonical_counts(counts: Iterable[Mapping[str, Any]]) -> dict[str, int]:
    result: dict[str, int] = {}
    for item in counts:
        key = _descriptor_key(item)
        result[key] = result.get(key, 0) + int(item["count"])
    return result


def merge_samples(
    guest_rows: Sequence[dict[str, str]],
    windows: Mapping[int, dict[str, Any]],
) -> list[dict[str, Any]]:
    if len(guest_rows) != len(windows):
        raise MergeError(f"客体样本数 {len(guest_rows)} != 插件窗口数 {len(windows)}")
    guest_by_sequence: dict[int, dict[str, str]] = {}
    for row in guest_rows:
        sequence = _integer(row, "sequence")
        if sequence == 0 or sequence in guest_by_sequence:
            raise MergeError("客体 sequence 非法或重复")
        guest_by_sequence[sequence] = row
    if set(guest_by_sequence) != set(windows):
        raise MergeError("客体与插件 sequence 集合不一致")

    metadata_by_pair = _metadata_by_pair(guest_rows)
    grouped: dict[tuple[str, str], list[dict[str, str]]] = {}
    for row in guest_rows:
        grouped.setdefault(
            (row.get("run_id", ""), row.get("pair_id", "")), []
        ).append(row)

    # v2 的 batch/round 元数据属于 pair 结构，而不是单个窗口的自由文本。
    # role、sequence、executed_instruction 可以不同；其余实验设计字段必须相同。
    pair_fields = (
        "run_id",
        "block_id",
        "instruction",
        "encoding_bytes",
        "pattern",
        "count_level",
        "requested_count",
        "blocks",
        "slots_per_block",
        "baseline_instruction",
        "baseline_encoding_bytes",
        "control_instruction",
    )
    for pair, rows in grouped.items():
        if any(_guest_metadata(row) for row in rows):
            if not pair[0] or not pair[1]:
                raise MergeError(f"version=2 pair={pair!r} 的 run_id/pair_id 不能为空")
            if len(rows) == 2:
                for field in pair_fields:
                    if rows[0].get(field) != rows[1].get(field):
                        raise MergeError(
                            f"version=2 pair={pair!r} 的 {field} 在 probe/baseline 间不一致"
                        )

    target_by_pair: dict[tuple[str, str], dict[str, Any]] = {}
    control_by_pair: dict[tuple[str, str], dict[str, Any] | None] = {}
    purity_by_pair: dict[tuple[str, str], float] = {}
    for pair, rows in grouped.items():
        if len(rows) != 2 or {row.get("role") for row in rows} != {
            "probe",
            "baseline",
        }:
            raise MergeError(f"pair={pair!r} 不是一对 probe/baseline")
        probe = next(row for row in rows if row["role"] == "probe")
        baseline = next(row for row in rows if row["role"] == "baseline")
        expected = _integer(probe, "requested_count")
        probe_counts = _validated_counts(windows[_integer(probe, "sequence")])
        baseline_validated_counts = _validated_counts(
            windows[_integer(baseline, "sequence")]
        )
        target_by_pair[pair] = _select_descriptor(
            probe_counts,
            instruction=probe["instruction"],
            size=_integer(probe, "encoding_bytes"),
            expected_count=expected,
            counterpart_counts=baseline_validated_counts,
        )
        if baseline.get("baseline_instruction") == "empty":
            control_by_pair[pair] = None
        else:
            control_by_pair[pair] = _select_descriptor(
                baseline_validated_counts,
                instruction=baseline["baseline_instruction"],
                size=_integer(baseline, "baseline_encoding_bytes"),
                expected_count=expected,
                counterpart_counts=probe_counts,
            )
        target = target_by_pair[pair]
        control = control_by_pair[pair]
        baseline_counts = _canonical_counts(
            _validated_counts(windows[_integer(baseline, "sequence")])
        )
        probe_canonical = _canonical_counts(probe_counts)
        target_key = _descriptor_key(target)
        target_delta = probe_canonical.get(target_key, 0) - baseline_counts.get(
            target_key, 0
        )
        if target_delta != expected:
            raise MergeError(
                f"pair={pair!r} 的 canonical 目标差 {target_delta} != "
                f"requested_count {expected}"
            )
        ignored = {target_key}
        residual = 0
        if control is not None:
            control_key = _descriptor_key(control)
            ignored.add(control_key)
            control_delta = baseline_counts.get(
                control_key, 0
            ) - probe_canonical.get(control_key, 0)
            if control_delta != target_delta:
                raise MergeError(
                    f"pair={pair!r} 的 control 差 {control_delta} != "
                    f"目标差 {target_delta}"
                )
        for key in set(probe_canonical) | set(baseline_counts):
            if key not in ignored:
                residual += abs(
                    probe_canonical.get(key, 0) - baseline_counts.get(key, 0)
                )
        if residual != 0:
            raise MergeError(f"pair={pair!r} 存在未闭合指令 residual={residual}")
        purity_by_pair[pair] = 1.0

    output: list[dict[str, Any]] = []
    for sequence in sorted(guest_by_sequence):
        guest = guest_by_sequence[sequence]
        window = windows[sequence]
        counts = _validated_counts(window)
        pair = (guest.get("run_id", ""), guest.get("pair_id", ""))
        target = target_by_pair[pair]
        control = control_by_pair[pair]
        exact_counts: dict[str, int] = {}
        for item in counts:
            key = _descriptor_key(item)
            exact_counts[key] = exact_counts.get(key, 0) + item["count"]
        target_count = next(
            (
                item["count"]
                for item in counts
                if item["size"] == target["size"]
                and item["bytes"] == target["bytes"]
            ),
            0,
        )
        metadata = metadata_by_pair[pair]
        declared_target = guest.get("target_count")
        expected_declared_target: int | None = None
        if guest.get("role") == "probe":
            expected_declared_target = _integer(guest, "requested_count")
        elif metadata.get("suite") == "differential-calibration-v2":
            expected_declared_target = 0
        elif metadata:
            expected_declared_target = _integer(guest, "requested_count")
        if metadata and (
            declared_target is not None
            and expected_declared_target is not None
            and _integer(guest, "target_count") != expected_declared_target
        ):
            raise MergeError(
                f"pair={pair!r} sequence={sequence} 的 guest target_count "
                "与 role/requested_count 契约不一致"
            )
        result: dict[str, Any] = {
            "schema": (
                "mygo.riscv-instruction-weight-sample.v2"
                if metadata
                else "mygo.riscv-instruction-weight-sample.v1"
            ),
            "run_id": guest["run_id"],
            "block_id": guest["block_id"],
            "pair_id": guest["pair_id"],
            "sequence": sequence,
            "role": guest["role"],
            "order": guest["order"],
            "instruction": guest["instruction"],
            "encoding_bytes": _integer(guest, "encoding_bytes"),
            "pattern": guest["pattern"],
            "requested_count": _integer(guest, "requested_count"),
            "target_count": target_count,
            "total_instruction_count": window["instruction_count"],
            "plugin_thread_cpu_ns": window.get("plugin_thread_cpu_ns"),
            "plugin_monotonic_ns": window.get("plugin_monotonic_ns"),
            "guest_ns": _integer(guest, "guest_elapsed_ns"),
            "rdtime_delta": _integer(guest, "rdtime_delta"),
            "timer_reads": _integer(guest, "timer_reads"),
            "paired_contrast_purity": purity_by_pair[pair],
            "target_descriptor": target,
            "exact_counts": exact_counts,
        }
        result.update(metadata)
        if control is None:
            result["baseline_kind"] = "empty"
        else:
            result["baseline_descriptor"] = control
            result["control_pattern"] = "independent"
        output.append(result)
    return output


def merge_timing_runs(
    run_inputs: Sequence[
        tuple[Sequence[dict[str, str]], Mapping[int, dict[str, Any]]]
    ],
) -> list[dict[str, Any]]:
    """合并多个独立 timing run，并保持跨输入顺序的确定性。

    validation 插件应先由其独立 schema 校验器验证，再按 ``run_id`` 和
    ``sequence`` 与这里的 timing 样本连接；本层不猜测尚未定义的 schema。
    """

    output: list[dict[str, Any]] = []
    seen_runs: set[str] = set()
    for input_index, (guest_rows, timing_windows) in enumerate(run_inputs):
        merged = merge_samples(guest_rows, timing_windows)
        run_ids = {str(row["run_id"]) for row in merged}
        if len(run_ids) != 1:
            raise MergeError(
                f"run input {input_index} 必须恰好包含一个 run_id，得到 {sorted(run_ids)!r}"
            )
        run_id = next(iter(run_ids))
        if run_id in seen_runs:
            raise MergeError(f"多个输入重复使用 run_id={run_id!r}")
        seen_runs.add(run_id)
        for row in merged:
            row["run_order"] = input_index
        output.extend(merged)
    return sorted(
        output,
        key=lambda row: (
            int(row["run_order"]),
            int(row["sequence"]),
            str(row["pair_id"]),
            str(row["role"]),
        ),
    )


_SIGNATURE_FIELDS = (
    "instruction",
    "encoding_bytes",
    "pattern",
    "role",
    "count_level",
    "requested_count",
    "blocks",
    "slots_per_block",
    "executed_instruction",
    "baseline_instruction",
    "baseline_encoding_bytes",
)


def _guest_signature(row: Mapping[str, str]) -> tuple[str, ...]:
    try:
        signature = tuple(row[name] for name in _SIGNATURE_FIELDS)
    except KeyError as error:
        raise MergeError(f"客体样本缺少签名字段 {error.args[0]}") from error
    metadata = _guest_metadata(row)
    if not metadata:
        return signature
    return signature + (
        f"probe_version={metadata['probe_version']}",
        *(f"{name}={metadata[name]}" for name in _DIFFERENTIAL_METADATA_FIELDS),
        f"anchor_position={metadata['anchor_position']}",
    )


def _guest_by_sequence(
    rows: Sequence[dict[str, str]], *, owner: str
) -> dict[int, dict[str, str]]:
    result: dict[int, dict[str, str]] = {}
    for row in rows:
        sequence = _integer(row, "sequence")
        if sequence == 0 or sequence in result:
            raise MergeError(f"{owner}: sequence 非法或重复")
        result[sequence] = row
    return result


def _assert_equivalent_guest_runs(
    timing_rows: Sequence[dict[str, str]],
    plugin_off_rows: Sequence[dict[str, str]],
) -> dict[int, dict[str, str]]:
    timing = _guest_by_sequence(timing_rows, owner="timing guest")
    plugin_off = _guest_by_sequence(plugin_off_rows, owner="plugin-off guest")
    if set(timing) != set(plugin_off):
        raise MergeError("timing 与 plugin-off guest sequence 集合不一致")
    ignored = {"elapsed_ns", "guest_elapsed_ns", "rdtime_delta", "checksum"}
    for sequence in sorted(timing):
        left = {key: value for key, value in timing[sequence].items() if key not in ignored}
        right = {
            key: value
            for key, value in plugin_off[sequence].items()
            if key not in ignored
        }
        if left != right:
            raise MergeError(
                f"timing 与 plugin-off guest 在 sequence={sequence} 的结构字段不一致"
            )
    return plugin_off


def _validation_templates(
    guest_rows: Sequence[dict[str, str]],
    windows: Mapping[int, dict[str, Any]],
) -> tuple[dict[tuple[str, ...], dict[str, Any]], Counter[tuple[str, ...]]]:
    merged = merge_samples(guest_rows, windows)
    guest = _guest_by_sequence(guest_rows, owner="validation guest")
    templates: dict[tuple[str, ...], dict[str, Any]] = {}
    signature_counts: Counter[tuple[str, ...]] = Counter()
    payload_fields = (
        "target_count",
        "total_instruction_count",
        "paired_contrast_purity",
        "target_descriptor",
        "exact_counts",
        "baseline_kind",
        "baseline_descriptor",
        "control_pattern",
    )
    for row in merged:
        sequence = int(row["sequence"])
        signature = _guest_signature(guest[sequence])
        signature_counts[signature] += 1
        payload = {name: row[name] for name in payload_fields if name in row}
        previous = templates.get(signature)
        if previous is not None and previous != payload:
            raise MergeError(
                f"validation 模板在 signature={signature!r} 上不稳定"
            )
        templates[signature] = payload
    if not templates:
        raise MergeError("validation 未生成任何模板")
    return templates, signature_counts


def merge_dual_mode_runs(
    validation_guest_rows: Sequence[dict[str, str]],
    validation_windows: Mapping[int, dict[str, Any]],
    timing_inputs: Sequence[
        tuple[
            Sequence[dict[str, str]],
            Mapping[int, dict[str, Any]],
            Sequence[dict[str, str]] | None,
        ]
    ],
    run_design: Mapping[str, Mapping[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    """用一次 exact-count validation 模板连接多个 marker-only timing run。"""

    templates, validation_signature_counts = _validation_templates(
        validation_guest_rows, validation_windows
    )
    output: list[dict[str, Any]] = []
    seen_runs: set[str] = set()
    for input_index, (guest_rows, timing_windows, plugin_off_rows) in enumerate(
        timing_inputs
    ):
        metadata_by_pair = _metadata_by_pair(guest_rows)
        guest = _guest_by_sequence(guest_rows, owner=f"timing input {input_index}")
        if set(guest) != set(timing_windows):
            raise MergeError(
                f"timing input {input_index}: guest 与插件 sequence 集合不一致"
            )
        timing_signature_counts = Counter(
            _guest_signature(guest_row) for guest_row in guest.values()
        )
        if timing_signature_counts != validation_signature_counts:
            raise MergeError(
                f"timing input {input_index}: signature 多重集与 validation 不一致"
            )
        run_ids = {row.get("run_id", "") for row in guest_rows}
        if len(run_ids) != 1 or not next(iter(run_ids)):
            raise MergeError(
                f"timing input {input_index} 必须恰好包含一个非空 run_id"
            )
        run_id = next(iter(run_ids))
        if run_id in seen_runs:
            raise MergeError(f"多个 timing 输入重复使用 run_id={run_id!r}")
        seen_runs.add(run_id)
        plugin_off = (
            None
            if plugin_off_rows is None
            else _assert_equivalent_guest_runs(guest_rows, plugin_off_rows)
        )
        if run_design is None:
            design = {
                "run_order": input_index,
                "super_run_id": run_id,
                "super_run_order": input_index,
                "crossover_pair": 1,
                "crossover_design": "single-pair",
                "timing_launch_position": 1,
                "plugin_off_launch_position": 2,
            }
        else:
            if run_id not in run_design:
                raise MergeError(f"run design 缺少 run_id={run_id!r}")
            design = dict(run_design[run_id])
        for sequence in sorted(guest):
            guest_row = guest[sequence]
            metadata = metadata_by_pair[
                (guest_row.get("run_id", ""), guest_row.get("pair_id", ""))
            ]
            signature = _guest_signature(guest_row)
            template = templates.get(signature)
            if template is None:
                raise MergeError(
                    f"timing input {input_index} 的 signature={signature!r} "
                    "没有 validation 模板"
                )
            window = timing_windows[sequence]
            guest_trap_entries = window.get(
                "guest_trap_entries_during_window"
            )
            if guest_trap_entries != 0:
                raise MergeError(
                    f"timing input {input_index} window {sequence} 在测量窗口内"
                    f"进入了 {guest_trap_entries!r} 次客体 trap"
                )
            cpu_ns = window.get("plugin_thread_cpu_ns")
            if (
                isinstance(cpu_ns, bool)
                or not isinstance(cpu_ns, (int, float))
                or cpu_ns < 0
            ):
                raise MergeError(
                    f"timing input {input_index} window {sequence} CPU 时间非法"
                )
            result: dict[str, Any] = {
                # dual-mode 的公开 API 本身就是 marker-only v2；旧 v1 guest
                # 只影响输入兼容性，不应改变既有输出 schema。
                "schema": "mygo.riscv-instruction-weight-sample.v2",
                "run_id": guest_row["run_id"],
                "run_order": design["run_order"],
                "super_run_id": design["super_run_id"],
                "super_run_order": design["super_run_order"],
                "crossover_pair": design["crossover_pair"],
                "crossover_design": design["crossover_design"],
                "timing_launch_position": design["timing_launch_position"],
                "plugin_off_launch_position": design[
                    "plugin_off_launch_position"
                ],
                "block_id": guest_row["block_id"],
                "pair_id": guest_row["pair_id"],
                "sequence": sequence,
                "role": guest_row["role"],
                "order": guest_row["order"],
                "instruction": guest_row["instruction"],
                "encoding_bytes": _integer(guest_row, "encoding_bytes"),
                "pattern": guest_row["pattern"],
                "requested_count": _integer(guest_row, "requested_count"),
                "plugin_thread_cpu_ns": cpu_ns,
                "plugin_monotonic_ns": window.get("plugin_monotonic_ns"),
                "plugin_mode": window.get("mode"),
                # 任意 guest TB 的窗口内翻译都会消耗同一个 vCPU thread，
                # 因而主污染字段必须使用全 guest 计数。用户 ELF 范围计数只
                # 保留为定位诊断，不能让 kernel/firmware 翻译漏过过滤门禁。
                "translations_during_window": window.get(
                    "translations_during_window"
                ),
                "scoped_translations_during_window": window.get(
                    "scoped_translations_during_window"
                ),
                "all_guest_translations_during_window": window.get(
                    "translations_during_window"
                ),
                "guest_trap_entries_during_window": guest_trap_entries,
                "guest_ns": _integer(guest_row, "guest_elapsed_ns"),
                "rdtime_delta": _integer(guest_row, "rdtime_delta"),
                "timer_reads": _integer(guest_row, "timer_reads"),
                **template,
            }
            result.update(metadata)
            if plugin_off is not None:
                result["plugin_off_guest_ns"] = _integer(
                    plugin_off[sequence], "guest_elapsed_ns"
                )
                result["plugin_off_rdtime_delta"] = _integer(
                    plugin_off[sequence], "rdtime_delta"
                )
            output.append(result)
    if run_design is not None and set(run_design) != seen_runs:
        extra = sorted(set(run_design) - seen_runs)
        raise MergeError(f"run design 包含未使用的 run_id: {extra!r}")
    return sorted(
        output,
        key=lambda row: (
            int(row["run_order"]),
            int(row["sequence"]),
            str(row["pair_id"]),
            str(row["role"]),
        ),
    )


def _parse_run_design(path: Path) -> dict[str, dict[str, Any]]:
    """读取并严格校验进程级 crossover 设计清单。"""

    rows: list[dict[str, Any]] = []
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw.strip():
            continue
        try:
            row = json.loads(raw)
        except json.JSONDecodeError as error:
            raise MergeError(f"run design 第 {line_number} 行不是合法 JSON") from error
        if not isinstance(row, dict):
            raise MergeError(f"run design 第 {line_number} 行必须是 object")
        rows.append(row)
    if not rows:
        raise MergeError("run design 不能为空")

    required = {
        "run_id",
        "run_order",
        "super_run_id",
        "super_run_order",
        "crossover_pair",
        "crossover_design",
        "timing_launch_position",
        "plugin_off_launch_position",
    }
    result: dict[str, dict[str, Any]] = {}
    pair_keys: set[tuple[str, int]] = set()
    orders: set[int] = set()
    super_orders: dict[str, int] = {}
    super_designs: dict[str, str] = {}
    launch_positions: dict[str, set[int]] = {}
    timing_positions: dict[str, set[int]] = {}
    for index, row in enumerate(rows):
        missing = sorted(required - set(row))
        if missing:
            raise MergeError(f"run design[{index}] 缺少字段 {missing!r}")
        run_id = row["run_id"]
        super_run_id = row["super_run_id"]
        design = row["crossover_design"]
        if not all(
            isinstance(value, str) and _metadata_token(value)
            for value in (run_id, super_run_id, design)
        ):
            raise MergeError(f"run design[{index}] 标识符非法")
        integer_fields = (
            "run_order",
            "super_run_order",
            "crossover_pair",
            "timing_launch_position",
            "plugin_off_launch_position",
        )
        for name in integer_fields:
            value = row[name]
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise MergeError(f"run design[{index}].{name} 必须是非负整数")
        if row["crossover_pair"] not in {1, 2}:
            raise MergeError("crossover_pair 必须为 1 或 2")
        if row["timing_launch_position"] == row["plugin_off_launch_position"]:
            raise MergeError("timing/plugin-off 不能复用启动位置")
        if run_id in result or row["run_order"] in orders:
            raise MergeError("run design 的 run_id/run_order 必须唯一")
        pair_key = (super_run_id, row["crossover_pair"])
        if pair_key in pair_keys:
            raise MergeError("每个 super-run 的 crossover_pair 必须唯一")
        previous_order = super_orders.setdefault(
            super_run_id, row["super_run_order"]
        )
        previous_design = super_designs.setdefault(super_run_id, design)
        if previous_order != row["super_run_order"] or previous_design != design:
            raise MergeError("同一 super-run 的 order/design 不一致")
        pair_keys.add(pair_key)
        orders.add(row["run_order"])
        launch_positions.setdefault(super_run_id, set()).update(
            (row["timing_launch_position"], row["plugin_off_launch_position"])
        )
        timing_positions.setdefault(super_run_id, set()).add(
            row["timing_launch_position"]
        )
        result[run_id] = {name: row[name] for name in required if name != "run_id"}

    for super_run_id, positions in launch_positions.items():
        pairs = {pair for owner, pair in pair_keys if owner == super_run_id}
        if pairs != {1, 2} or positions != {1, 2, 3, 4}:
            raise MergeError(
                f"super-run={super_run_id!r} 必须完整覆盖两个 pair 和四个启动位置"
            )
        if super_designs[super_run_id] not in {"ABBA", "BAAB"}:
            raise MergeError("crossover_design 必须为 ABBA 或 BAAB")
        expected_timing = {1, 4} if super_designs[super_run_id] == "ABBA" else {2, 3}
        if timing_positions[super_run_id] != expected_timing:
            raise MergeError(
                f"super-run={super_run_id!r} 的启动位置与设计标签不一致"
            )
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--validation-guest", required=True)
    parser.add_argument("--validation-plugin", required=True)
    parser.add_argument("--guest", action="append", required=True)
    parser.add_argument(
        "--timing-plugin",
        dest="timing_plugin",
        action="append",
        required=True,
        help="marker-only QEMU timing segment JSONL",
    )
    parser.add_argument("--plugin-off-guest", action="append")
    parser.add_argument("--run-design")
    parser.add_argument("--output", required=True)
    arguments = parser.parse_args(argv)
    if len(arguments.guest) != len(arguments.timing_plugin):
        parser.error("--guest 与 --timing-plugin 数量必须相同")
    if arguments.plugin_off_guest is not None and len(
        arguments.plugin_off_guest
    ) != len(arguments.guest):
        parser.error("--plugin-off-guest 必须与 --guest 数量相同")
    plugin_off_paths = arguments.plugin_off_guest or [None] * len(arguments.guest)
    run_design = (
        None
        if arguments.run_design is None
        else _parse_run_design(Path(arguments.run_design))
    )
    rows = merge_dual_mode_runs(
        _parse_guest(Path(arguments.validation_guest)),
        _parse_plugin(
            Path(arguments.validation_plugin), expected_mode="validation"
        ),
        [
            (
                _parse_guest(Path(guest)),
                _parse_plugin(Path(timing_plugin), expected_mode="timing"),
                None if plugin_off is None else _parse_guest(Path(plugin_off)),
            )
            for guest, timing_plugin, plugin_off in zip(
                arguments.guest, arguments.timing_plugin, plugin_off_paths
            )
        ],
        run_design=run_design,
    )
    Path(arguments.output).write_text(
        "".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n" for row in rows),
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
