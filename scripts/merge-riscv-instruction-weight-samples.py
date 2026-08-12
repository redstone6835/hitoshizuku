#!/usr/bin/env python3
"""把客体探针日志与 QEMU segment 窗口合并为统计模型输入。"""

from __future__ import annotations

import argparse
import json
import shlex
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any

from riscv_instruction_encoding import decode_riscv64_instruction


class MergeError(ValueError):
    pass


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

    grouped: dict[tuple[str, str], list[dict[str, str]]] = {}
    for row in guest_rows:
        grouped.setdefault(
            (row.get("run_id", ""), row.get("pair_id", "")), []
        ).append(row)

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
        result: dict[str, Any] = {
            "schema": "mygo.riscv-instruction-weight-sample.v1",
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
        output.extend(merged)
    return sorted(
        output,
        key=lambda row: (
            str(row["run_id"]),
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
        return tuple(row[name] for name in _SIGNATURE_FIELDS)
    except KeyError as error:
        raise MergeError(f"客体样本缺少签名字段 {error.args[0]}") from error


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
) -> dict[tuple[str, ...], dict[str, Any]]:
    merged = merge_samples(guest_rows, windows)
    guest = _guest_by_sequence(guest_rows, owner="validation guest")
    templates: dict[tuple[str, ...], dict[str, Any]] = {}
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
        payload = {name: row[name] for name in payload_fields if name in row}
        previous = templates.get(signature)
        if previous is not None and previous != payload:
            raise MergeError(
                f"validation 模板在 signature={signature!r} 上不稳定"
            )
        templates[signature] = payload
    if not templates:
        raise MergeError("validation 未生成任何模板")
    return templates


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
) -> list[dict[str, Any]]:
    """用一次 exact-count validation 模板连接多个 marker-only timing run。"""

    templates = _validation_templates(validation_guest_rows, validation_windows)
    output: list[dict[str, Any]] = []
    seen_runs: set[str] = set()
    for input_index, (guest_rows, timing_windows, plugin_off_rows) in enumerate(
        timing_inputs
    ):
        guest = _guest_by_sequence(guest_rows, owner=f"timing input {input_index}")
        if set(guest) != set(timing_windows):
            raise MergeError(
                f"timing input {input_index}: guest 与插件 sequence 集合不一致"
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
        for sequence in sorted(guest):
            guest_row = guest[sequence]
            signature = _guest_signature(guest_row)
            template = templates.get(signature)
            if template is None:
                raise MergeError(
                    f"timing input {input_index} 的 signature={signature!r} "
                    "没有 validation 模板"
                )
            window = timing_windows[sequence]
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
                "schema": "mygo.riscv-instruction-weight-sample.v2",
                "run_id": guest_row["run_id"],
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
                "translations_during_window": window.get(
                    "scoped_translations_during_window",
                    window.get("translations_during_window"),
                ),
                "all_guest_translations_during_window": window.get(
                    "translations_during_window"
                ),
                "guest_ns": _integer(guest_row, "guest_elapsed_ns"),
                "rdtime_delta": _integer(guest_row, "rdtime_delta"),
                "timer_reads": _integer(guest_row, "timer_reads"),
                **template,
            }
            if plugin_off is not None:
                result["plugin_off_guest_ns"] = _integer(
                    plugin_off[sequence], "guest_elapsed_ns"
                )
                result["plugin_off_rdtime_delta"] = _integer(
                    plugin_off[sequence], "rdtime_delta"
                )
            output.append(result)
    return sorted(
        output,
        key=lambda row: (
            str(row["run_id"]),
            int(row["sequence"]),
            str(row["pair_id"]),
            str(row["role"]),
        ),
    )


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
    parser.add_argument("--output", required=True)
    arguments = parser.parse_args(argv)
    if len(arguments.guest) != len(arguments.timing_plugin):
        parser.error("--guest 与 --timing-plugin 数量必须相同")
    if arguments.plugin_off_guest is not None and len(
        arguments.plugin_off_guest
    ) != len(arguments.guest):
        parser.error("--plugin-off-guest 必须与 --guest 数量相同")
    plugin_off_paths = arguments.plugin_off_guest or [None] * len(arguments.guest)
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
        ]
    )
    Path(arguments.output).write_text(
        "".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n" for row in rows),
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
