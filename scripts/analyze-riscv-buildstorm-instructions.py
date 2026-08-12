#!/usr/bin/env python3
"""分析一次已完成的 RISC-V BuildStorm 指令与 TCG task-clock 画像。

脚本只依赖 Python 标准库。所有公开产物原子写入 ``RUN_DIR/analysis``；
昂贵的原始解析、JIT 映射与耗时模型均带输入指纹缓存，因此中断后可恢复。
"""

from __future__ import annotations

import argparse
import bisect
import collections
import csv
import dataclasses
import io
import json
import math
import os
import re
import sys
import tempfile
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any

from rv_instruction_profile_io import (
    JitCodeClose,
    MatchStatistics,
    PerfSample,
    ProfileIoError,
    RvTcgAttachFailure,
    RvTcgGate,
    RvTcgLost,
    RvTcgQuality,
    RvTcgThread,
    RvTcgTidStats,
    SampleLocation,
    TimeAwareJitMap,
    iter_matched_jit_records,
    iter_rv_tcg_records,
    read_rv_tcg_file_header,
    read_tid_namespace_tsv,
)
from rv_instruction_profile_stats import (
    StatisticsError,
    adjacent_segment_block_permutation_js,
    assess_distribution_confidence,
    diagnose_serial_dependence,
    detect_change_points,
    global_change_point_block_sensitivity_test,
    moving_block_bootstrap_boundary_stability,
    prepare_feature_matrix,
    run_segmentation_sensitivity,
    standardize_matrix,
    weighted_stage_distributions,
)
from rv_instruction_weight_model import WeightModelError, fit_instruction_weight_model


MIX_SCHEMA = "mygo.riscv-instruction-mix.v1"
ANALYSIS_SCHEMA = "mygo.riscv-buildstorm-instruction-analysis.v1"
CACHE_VERSION = 4
VCPU_COMM = re.compile(r"CPU ([0-9]+)/TCG\Z")
NANOSECONDS_PER_SECOND = 1_000_000_000
LOCATION_NAMES = tuple(location.value for location in SampleLocation)


class AnalysisError(RuntimeError):
    """输入不完整、闭包不成立或无法得到可信分析。"""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise AnalysisError(message)


def _uint(value: Any, owner: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise AnalysisError(f"{owner} 必须是非负整数")
    return value


def _mapping(value: Any, owner: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise AnalysisError(f"{owner} 必须是对象")
    return value


def _atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def _atomic_write_json(path: Path, value: Any) -> None:
    _atomic_write_text(
        path,
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
    )


def _atomic_write_csv(path: Path, header: Sequence[str], rows: Iterable[Sequence[Any]]) -> None:
    buffer = io.StringIO(newline="")
    writer = csv.writer(buffer, lineterminator="\n")
    writer.writerow(header)
    writer.writerows(rows)
    _atomic_write_text(path, buffer.getvalue())


def _fingerprint(path: Path) -> dict[str, Any]:
    status = path.stat()
    return {
        "path": str(path.resolve()),
        "device": status.st_dev,
        "inode": status.st_ino,
        "size": status.st_size,
        "mtime_ns": status.st_mtime_ns,
    }


def _cache_key(paths: Sequence[Path], parameters: Mapping[str, Any]) -> dict[str, Any]:
    return {
        "version": CACHE_VERSION,
        "inputs": [_fingerprint(path) for path in paths],
        "parameters": dict(parameters),
    }


def _load_cache(path: Path, expected_key: Mapping[str, Any], resume: bool) -> Any | None:
    if not resume or not path.is_file():
        return None
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(value, dict) or value.get("cache_key") != expected_key:
        return None
    return value.get("payload")


def _write_cache(path: Path, key: Mapping[str, Any], payload: Any) -> None:
    _atomic_write_json(path, {"cache_key": key, "payload": payload})


def _variant_key(mnemonic: str, size: int) -> str:
    return f"{mnemonic} [size={size}]"


def _parse_json_line(raw: str, path: Path, line_number: int) -> dict[str, Any]:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise AnalysisError(f"{path}:{line_number}: JSON 非法：{error}") from error
    if not isinstance(value, dict):
        raise AnalysisError(f"{path}:{line_number}: 记录不是对象")
    return value


def parse_instruction_mix(
    path: Path,
    *,
    max_epoch_snapshot_skew_ratio: float = 0.001,
    max_window_snapshot_skew_ratio: float = 0.0001,
) -> dict[str, Any]:
    """流式解析指令 mix，验证 descriptor 闭包与有界 SMP 快照 skew。"""

    descriptors: dict[int, dict[str, Any]] = {}
    descriptor_totals: dict[int, dict[str, int]] = collections.defaultdict(
        lambda: {"user": 0, "kernel": 0}
    )
    epochs: list[dict[str, Any]] = []
    header: dict[str, Any] | None = None
    window_start: dict[str, Any] | None = None
    window_stop: dict[str, Any] | None = None
    quality: dict[str, Any] | None = None
    expected_epoch = 1
    last_timestamp = -1
    previous_epoch_end: int | None = None
    record_count = 0
    cumulative_snapshot_skew = {"user": 0, "kernel": 0}
    max_snapshot_skew = {"user": 0, "kernel": 0}
    max_relative_snapshot_skew = 0.0
    skewed_epoch_domains = 0

    with path.open("r", encoding="utf-8", newline="") as stream:
        for line_number, raw in enumerate(stream, 1):
            record_count += 1
            _require(raw.strip() != "", f"{path}:{line_number}: 空记录")
            record = _parse_json_line(raw, path, line_number)
            _require(
                record.get("schema") == MIX_SCHEMA,
                f"{path}:{line_number}: schema 不受支持",
            )
            record_type = record.get("type")
            timestamp = _uint(
                record.get("monotonic_ns"), f"{path}:{line_number}.monotonic_ns"
            )
            _require(timestamp >= last_timestamp, f"{path}:{line_number}: 时间戳倒退")
            last_timestamp = timestamp
            _require(quality is None, f"{path}:{line_number}: quality 之后仍有记录")

            if record_type == "header":
                _require(
                    line_number == 1 and header is None,
                    f"{path}:{line_number}: header 重复或位置错误",
                )
                header = record
                _require(
                    _uint(record.get("epoch_ms"), "mix.header.epoch_ms") == 1000,
                    "顶层统计要求 instruction mix 使用 1 秒 epoch",
                )
            elif record_type == "descriptor":
                descriptor_id = _uint(record.get("id"), "mix.descriptor.id")
                mnemonic = record.get("mnemonic")
                size = _uint(record.get("size"), "mix.descriptor.size")
                _require(
                    isinstance(mnemonic, str) and mnemonic.strip() == mnemonic and mnemonic,
                    f"{path}:{line_number}: mnemonic 非法",
                )
                _require(size in (2, 4), f"{path}:{line_number}: RISC-V 指令长度不是 2/4")
                _require(
                    descriptor_id not in descriptors,
                    f"{path}:{line_number}: descriptor {descriptor_id} 重复",
                )
                descriptors[descriptor_id] = {
                    "id": descriptor_id,
                    "mnemonic": mnemonic,
                    "model_mnemonic": mnemonic.lower(),
                    "size": size,
                    "variant": _variant_key(mnemonic, size),
                }
            elif record_type == "window_start":
                _require(window_start is None, "instruction mix 有多个 window_start")
                _require(window_stop is None, "window_start 出现在 window_stop 之后")
                window_start = record
                previous_epoch_end = timestamp
            elif record_type == "sample":
                _require(window_start is not None, "sample 出现在 window_start 之前")
                _require(window_stop is None, "sample 出现在 window_stop 之后")
                epoch_number = _uint(record.get("epoch"), "mix.sample.epoch")
                _require(epoch_number == expected_epoch, "instruction mix epoch 不连续")
                expected_epoch += 1
                _require(
                    record.get("counter_regression") is False,
                    f"epoch {epoch_number} 出现计数器回退",
                )
                _require(previous_epoch_end is not None, "epoch 起点缺失")
                _require(timestamp > previous_epoch_end, f"epoch {epoch_number} 时长非正")

                instruction_delta = _mapping(
                    record.get("instruction_delta"), "mix.sample.instruction_delta"
                )
                mix_delta = _mapping(
                    record.get("mix_instruction_delta"),
                    "mix.sample.mix_instruction_delta",
                )
                tb_delta = _mapping(record.get("tb_delta"), "mix.sample.tb_delta")
                raw_instruction_domain = {
                    domain: _uint(
                        instruction_delta.get(domain),
                        f"mix.sample.instruction_delta.{domain}",
                    )
                    for domain in ("user", "kernel")
                }
                canonical_domain = {
                    domain: _uint(
                        mix_delta.get(domain), f"mix.sample.mix_delta.{domain}"
                    )
                    for domain in ("user", "kernel")
                }
                snapshot_skew = {
                    domain: raw_instruction_domain[domain] - canonical_domain[domain]
                    for domain in ("user", "kernel")
                }
                for domain, skew in snapshot_skew.items():
                    cumulative_snapshot_skew[domain] += skew
                    max_snapshot_skew[domain] = max(max_snapshot_skew[domain], abs(skew))
                    skewed_epoch_domains += int(skew != 0)

                raw_mix = record.get("mix")
                _require(isinstance(raw_mix, list), "mix.sample.mix 必须是数组")
                seen_ids: set[int] = set()
                observed_domain = {"user": 0, "kernel": 0}
                variants: dict[str, dict[str, Any]] = {}
                mnemonic_counts: dict[str, int] = collections.defaultdict(int)
                for row_index, row in enumerate(raw_mix):
                    _require(isinstance(row, dict), "mix.sample.mix 行不是对象")
                    descriptor_id = _uint(row.get("id"), f"mix[{row_index}].id")
                    _require(
                        descriptor_id in descriptors,
                        f"epoch {epoch_number} 引用了未知 descriptor {descriptor_id}",
                    )
                    _require(
                        descriptor_id not in seen_ids,
                        f"epoch {epoch_number} 重复 descriptor {descriptor_id}",
                    )
                    seen_ids.add(descriptor_id)
                    descriptor = descriptors[descriptor_id]
                    counts = {
                        domain: _uint(row.get(domain), f"mix[{row_index}].{domain}")
                        for domain in ("user", "kernel")
                    }
                    _require(sum(counts.values()) > 0, "mix.sample.mix 不应包含全零行")
                    for domain, count in counts.items():
                        observed_domain[domain] += count
                        descriptor_totals[descriptor_id][domain] += count
                    variants[descriptor["variant"]] = {
                        "descriptor_id": descriptor_id,
                        **counts,
                        "total": counts["user"] + counts["kernel"],
                    }
                    mnemonic_counts[descriptor["model_mnemonic"]] += sum(counts.values())
                _require(
                    observed_domain == canonical_domain,
                    f"epoch {epoch_number} 的逐 descriptor 与 mix_delta 不闭合",
                )

                translated = _mapping(record.get("translated"), "mix.sample.translated")
                executed_tb = {
                    domain: _uint(tb_delta.get(domain), f"mix.sample.tb_delta.{domain}")
                    for domain in ("user", "kernel")
                }
                total = sum(canonical_domain.values())
                max_relative_snapshot_skew = max(
                    max_relative_snapshot_skew,
                    sum(abs(value) for value in snapshot_skew.values()) / max(total, 1),
                )
                duration_ns = timestamp - previous_epoch_end
                epochs.append(
                    {
                        "epoch": epoch_number,
                        "time_ns": previous_epoch_end,
                        "end_time_ns": timestamp,
                        "duration_ns": duration_ns,
                        "user_count": canonical_domain["user"],
                        "kernel_count": canonical_domain["kernel"],
                        "raw_instruction_delta": raw_instruction_domain,
                        "counter_snapshot_skew": snapshot_skew,
                        "total_count": total,
                        "rate": total * NANOSECONDS_PER_SECOND / duration_ns,
                        "kernel_share": canonical_domain["kernel"] / total if total else 0.0,
                        "executed_tb": executed_tb,
                        "executed_tb_count": sum(executed_tb.values()),
                        "translated_tb_delta": _uint(
                            translated.get("tb_delta"), "mix.sample.translated.tb_delta"
                        ),
                        "translated_insns_delta": _uint(
                            translated.get("instruction_delta"),
                            "mix.sample.translated.instruction_delta",
                        ),
                        "variant_counts": {
                            name: row["total"] for name, row in variants.items()
                        },
                        "variant_domain_counts": variants,
                        "mnemonic_counts": dict(mnemonic_counts),
                        "reason": str(record.get("reason", "")),
                    }
                )
                previous_epoch_end = timestamp
            elif record_type == "window_stop":
                _require(window_start is not None, "window_stop 缺少 window_start")
                _require(window_stop is None, "instruction mix 有多个 window_stop")
                window_stop = record
            elif record_type == "control_error":
                raise AnalysisError(f"{path}:{line_number}: 插件报告 control_error")
            elif record_type == "quality":
                quality = record
            else:
                raise AnalysisError(f"{path}:{line_number}: 未知记录类型 {record_type!r}")

    _require(header is not None, "instruction mix 缺少 header")
    _require(window_start is not None and window_stop is not None, "测量窗口不完整")
    _require(quality is not None, "instruction mix 尚未结束（缺少 final quality）")
    _require(epochs, "instruction mix 没有 epoch")
    _require(quality.get("complete") is True, "instruction mix quality.complete=false")
    _require(
        window_start.get("window_id") == window_stop.get("window_id") == 1,
        "instruction mix window_id 不闭合",
    )
    _require(
        _uint(quality.get("windows"), "mix.quality.windows") == 1,
        "instruction mix 不是单一测量窗口",
    )
    _require(
        _uint(quality.get("samples"), "mix.quality.samples") == len(epochs),
        "instruction mix quality.samples 与 epoch 数不符",
    )
    _require(
        _uint(quality.get("start_detections"), "mix.quality.start_detections") == 1
        and _uint(quality.get("stop_detections"), "mix.quality.stop_detections") == 1,
        "instruction mix 起止检测次数不为一",
    )
    _require(
        _uint(quality.get("exit_stops"), "mix.quality.exit_stops") == 0,
        "测量窗口只在 QEMU 退出时被动结束",
    )
    for name, value in _mapping(quality.get("errors"), "mix.quality.errors").items():
        _require(_uint(value, f"mix.quality.errors.{name}") == 0, f"插件错误 {name} 非零")
    catalog_quality = _mapping(quality.get("catalog"), "mix.quality.catalog")
    _require(catalog_quality.get("enabled") is True, "TB catalog 未启用")
    for name in ("write_errors", "dropped_blocks", "allocation_failures", "tracking_drops"):
        _require(
            _uint(catalog_quality.get(name), f"mix.quality.catalog.{name}") == 0,
            f"TB catalog {name} 非零",
        )
    _require(
        _uint(header.get("configured_vcpus"), "mix.header.configured_vcpus") > 0,
        "configured_vcpus 必须为正",
    )
    _require(
        window_stop["monotonic_ns"] >= epochs[-1]["end_time_ns"],
        "window_stop 早于最后一个 epoch",
    )
    registered_descriptor_count = _uint(
        quality.get("descriptor_count"), "mix.quality.descriptor_count"
    )
    _require(
        len(descriptors) <= registered_descriptor_count,
        "已输出 descriptor 数超过 quality 中的全局 registry 数",
    )
    descriptor_rows: list[dict[str, Any]] = []
    domain_totals = {"user": 0, "kernel": 0}
    for descriptor_id in sorted(descriptors):
        descriptor = descriptors[descriptor_id]
        counts = descriptor_totals[descriptor_id]
        total = counts["user"] + counts["kernel"]
        if total == 0:
            continue
        descriptor_rows.append({**descriptor, **counts, "total": total})
        for domain in domain_totals:
            domain_totals[domain] += counts[domain]
    epoch_domain_totals = {
        "user": sum(epoch["user_count"] for epoch in epochs),
        "kernel": sum(epoch["kernel_count"] for epoch in epochs),
    }
    _require(
        domain_totals == epoch_domain_totals,
        "全窗口 descriptor 总数与 epoch 总数不闭合",
    )
    window_snapshot_skew_ratio = sum(
        abs(value) for value in cumulative_snapshot_skew.values()
    ) / max(sum(domain_totals.values()), 1)
    _require(
        max_relative_snapshot_skew <= max_epoch_snapshot_skew_ratio,
        "单 epoch instruction/mix 并发快照 skew 超过阈值",
    )
    _require(
        window_snapshot_skew_ratio <= max_window_snapshot_skew_ratio,
        "全窗口 instruction/mix 累计快照 skew 超过阈值",
    )
    return {
        "schema": ANALYSIS_SCHEMA,
        "record_count": record_count,
        "header": header,
        "window": {
            "start_monotonic_ns": window_start["monotonic_ns"],
            "stop_monotonic_ns": window_stop["monotonic_ns"],
            "epoch_covered_end_ns": epochs[-1]["end_time_ns"],
        },
        "quality": quality,
        "descriptors": descriptor_rows,
        "epochs": epochs,
        "totals": {**domain_totals, "total": sum(domain_totals.values())},
        "count_closure": {
            "per_epoch_descriptor_equals_mix_delta": True,
            "per_epoch_instruction_delta_is_asynchronous_snapshot": True,
            "window_cumulative_snapshot_skew_zero": cumulative_snapshot_skew
            == {"user": 0, "kernel": 0},
            "window_cumulative_snapshot_skew_within_bound": True,
            "cumulative_snapshot_skew": cumulative_snapshot_skew,
            "window_cumulative_snapshot_skew_ratio": window_snapshot_skew_ratio,
            "max_absolute_snapshot_skew": max_snapshot_skew,
            "max_relative_snapshot_skew": max_relative_snapshot_skew,
            "thresholds": {
                "max_epoch_snapshot_skew_ratio": max_epoch_snapshot_skew_ratio,
                "max_window_snapshot_skew_ratio": max_window_snapshot_skew_ratio,
            },
            "skewed_epoch_domains": skewed_epoch_domains,
            "descriptor_rows": len(descriptor_rows),
            "emitted_descriptor_rows": len(descriptors),
            "registered_descriptor_rows": registered_descriptor_count,
            "epoch_rows": len(epochs),
        },
    }


def _last_json_record(path: Path) -> dict[str, Any]:
    """只读取 JSONL 最后一条非空记录，避免为 quality 再扫数 GiB。"""

    with path.open("rb") as stream:
        stream.seek(0, os.SEEK_END)
        position = stream.tell()
        buffer = bytearray()
        while position > 0:
            chunk_size = min(position, 64 * 1024)
            position -= chunk_size
            stream.seek(position)
            buffer[:0] = stream.read(chunk_size)
            lines = buffer.splitlines()
            if position == 0 or len(lines) >= 2:
                for raw in reversed(lines):
                    if raw.strip():
                        try:
                            value = json.loads(raw)
                        except (UnicodeDecodeError, json.JSONDecodeError) as error:
                            raise AnalysisError(f"{path}: 末记录 JSON 非法") from error
                        _require(isinstance(value, dict), f"{path}: 末记录不是对象")
                        return value
        raise AnalysisError(f"{path}: 文件为空")


def _load_collector(path: Path) -> tuple[list[PerfSample], dict[str, Any]]:
    header = read_rv_tcg_file_header(path)
    samples: list[PerfSample] = []
    threads: dict[int, RvTcgThread] = {}
    tid_stats: list[RvTcgTidStats] = []
    quality: RvTcgQuality | None = None
    lost = 0
    attach_failures: list[RvTcgAttachFailure] = []
    gates: list[RvTcgGate] = []
    last_record: object | None = None
    for record in iter_rv_tcg_records(path):
        _require(quality is None, "TCG collector final quality 之后仍有记录")
        last_record = record
        if isinstance(record, PerfSample):
            samples.append(record)
        elif isinstance(record, RvTcgThread):
            _require(record.tid not in threads, f"collector thread {record.tid} 重复")
            threads[record.tid] = record
        elif isinstance(record, RvTcgTidStats):
            tid_stats.append(record)
        elif isinstance(record, RvTcgLost):
            lost += record.lost
        elif isinstance(record, RvTcgAttachFailure):
            attach_failures.append(record)
        elif isinstance(record, RvTcgGate):
            gates.append(record)
        elif isinstance(record, RvTcgQuality):
            quality = record
    _require(isinstance(last_record, RvTcgQuality), "TCG collector 缺少末尾 quality")
    _require(quality is not None, "TCG collector 缺少 quality")
    _require(quality.status == 0, f"TCG collector status={quality.status}")
    _require(quality.lost == lost == 0, "TCG collector 存在丢失样本")
    _require(quality.samples_written == len(samples), "collector 样本数不闭合")
    _require(quality.samples_seen == len(samples), "collector seen/written 样本数不闭合")
    _require(quality.samples_discarded == 0, "collector 丢弃了样本")
    _require(quality.throttle_records == quality.unthrottle_records == 0, "perf 被节流")
    _require(quality.attach_failures == len(attach_failures) == 0, "collector attach 失败")
    _require(quality.tids_discovered == quality.tids_attached, "collector 未附加全部线程")
    _require(len(tid_stats) == quality.tids_discovered, "collector TID stats 不完整")
    _require(quality.running_ratio_ppm >= 990_000, "perf running ratio 低于 99%")
    _require(samples, "collector 没有 task-clock 样本")
    samples.sort(key=lambda sample: (sample.time_ns, sample.tid, sample.ip))
    return samples, {
        "header": dataclasses.asdict(header),
        "quality": dataclasses.asdict(quality),
        "threads": [dataclasses.asdict(threads[tid]) for tid in sorted(threads)],
        "tid_stats": [dataclasses.asdict(record) for record in tid_stats],
        "gates": [dataclasses.asdict(record) for record in gates],
        "sample_count": len(samples),
        "sample_task_clock_ns": sum(sample.period_ns for sample in samples),
    }


def _epoch_index(starts: Sequence[int], epochs: Sequence[Mapping[str, Any]], timestamp: int) -> int | None:
    index = bisect.bisect_right(starts, timestamp) - 1
    if index < 0 or timestamp >= int(epochs[index]["end_time_ns"]):
        return None
    return index


def map_perf_samples(
    mix: Mapping[str, Any],
    samples_path: Path,
    jitdump_path: Path,
    catalog_path: Path,
    tid_map_path: Path,
    *,
    min_jit_sample_mapping_ratio: float,
    min_catalog_coverage_ratio: float,
) -> dict[str, Any]:
    """将 task-clock 样本按时间映射到 JIT，并只聚合明确的 vCPU TID。"""

    samples, collector = _load_collector(samples_path)
    namespace = read_tid_namespace_tsv(tid_map_path)
    vcpu_entries: list[tuple[int, Any]] = []
    for entry in namespace.entries:
        match = VCPU_COMM.fullmatch(entry.comm)
        if match:
            vcpu_entries.append((int(match.group(1)), entry))
    vcpu_entries.sort(key=lambda item: item[0])
    configured_vcpus = int(mix["header"]["configured_vcpus"])
    _require(
        [index for index, _ in vcpu_entries] == list(range(configured_vcpus)),
        "TID namespace 中的 vCPU 编号不完整",
    )
    vcpu_host_tids = {entry.host_tid for _, entry in vcpu_entries}
    vcpu_container_tids = {entry.container_tid for _, entry in vcpu_entries}
    collector_threads = {row["tid"]: row for row in collector["threads"]}
    collector_tid_stats = {row["tid"]: row for row in collector["tid_stats"]}
    raw_period_by_tid: collections.Counter[int] = collections.Counter()
    for sample in samples:
        raw_period_by_tid[sample.tid] += sample.period_ns
    task_clock_accounting: dict[int, dict[str, float | int]] = {}
    for _, entry in vcpu_entries:
        _require(entry.host_tid in collector_threads, f"collector 未发现 vCPU TID {entry.host_tid}")
        _require(
            collector_threads[entry.host_tid]["attach_errno"] == 0,
            f"collector 未成功附加 vCPU TID {entry.host_tid}",
        )
        _require(entry.host_tid in collector_tid_stats, f"vCPU TID {entry.host_tid} 缺少 final read")
        raw_period = raw_period_by_tid[entry.host_tid]
        exact_clock = int(collector_tid_stats[entry.host_tid]["task_clock_ns"])
        _require(raw_period > 0 and exact_clock > 0, f"vCPU TID {entry.host_tid} 没有可校准 task-clock")
        residual = exact_clock - raw_period
        _require(residual >= 0, f"vCPU TID {entry.host_tid} 的 sample period 超过 final read")
        task_clock_accounting[entry.host_tid] = {
            "sample_period_ns": raw_period,
            "exact_task_clock_ns": exact_clock,
            "located_task_clock_ns": raw_period,
            "unlocated_tail_task_clock_ns": residual,
            "located_fraction": raw_period / exact_clock,
        }

    gate_records = sorted(collector["gates"], key=lambda row: row["time_ns"])
    enabled_gates = [row for row in gate_records if row["enabled"]]
    _require(len(enabled_gates) == 1, "collector 必须恰有一个 enabled gate 区间")
    gate_start_ns = int(enabled_gates[0]["time_ns"])
    disabled_after = [
        row for row in gate_records if not row["enabled"] and row["time_ns"] > gate_start_ns
    ]
    _require(disabled_after, "collector enabled gate 缺少结束记录")
    gate_stop_ns = int(disabled_after[0]["time_ns"])
    mix_start_ns = int(mix["window"]["start_monotonic_ns"])
    mix_stop_ns = int(mix["window"]["stop_monotonic_ns"])
    max_period_by_vcpu = {
        tid: max(
            sample.period_ns for sample in samples if sample.tid == tid
        )
        for tid in vcpu_host_tids
    }
    boundary_period_uncertainty_ns = sum(max_period_by_vcpu.values())

    epochs = mix["epochs"]
    starts = [int(epoch["time_ns"]) for epoch in epochs]
    per_epoch = [
        {
            "time_ns": int(epoch["time_ns"]),
            "duration_ns": int(epoch["duration_ns"]),
            "samples": {name: 0 for name in LOCATION_NAMES},
            "task_clock_ns": {name: 0 for name in LOCATION_NAMES},
        }
        for epoch in epochs
    ]
    location_samples: collections.Counter[str] = collections.Counter()
    location_clock: collections.Counter[str] = collections.Counter()
    native_ips: collections.Counter[int] = collections.Counter()
    native_ip_clock: collections.Counter[int] = collections.Counter()
    mapped_guest_pcs: collections.Counter[int] = collections.Counter()
    outside_samples: collections.Counter[str] = collections.Counter()
    outside_clock: collections.Counter[str] = collections.Counter()
    non_vcpu_samples = 0
    non_vcpu_clock = 0
    match_stats = MatchStatistics()
    jit_observation = {"close_records": 0, "last_timestamp_ns": None}

    def observed_records() -> Iterable[Any]:
        for record in iter_matched_jit_records(
            catalog_path, jitdump_path, stats=match_stats, include_instructions=False
        ):
            jit_observation["last_timestamp_ns"] = record.timestamp_ns
            if isinstance(record, JitCodeClose):
                jit_observation["close_records"] += 1
            yield record

    mapper = TimeAwareJitMap(observed_records())
    for sample_number, mapped in enumerate(
        mapper.map_sorted_samples(samples, tid_namespace=namespace), 1
    ):
        if sample_number % 100_000 == 0:
            print(f"analysis: 已映射 {sample_number:,} 个 perf 样本", file=sys.stderr)
        sample = mapped.sample
        if sample.tid not in vcpu_host_tids:
            non_vcpu_samples += 1
            non_vcpu_clock += sample.period_ns
            continue
        located_clock = sample.period_ns
        location = mapped.location.value
        location_samples[location] += 1
        location_clock[location] += located_clock
        index = _epoch_index(starts, epochs, sample.time_ns)
        if index is None:
            outside_samples[location] += 1
            outside_clock[location] += located_clock
        else:
            per_epoch[index]["samples"][location] += 1
            per_epoch[index]["task_clock_ns"][location] += located_clock
        if mapped.location is SampleLocation.NATIVE_QEMU:
            native_ips[sample.ip] += 1
            native_ip_clock[sample.ip] += located_clock
        elif mapped.location is SampleLocation.MAPPED_TCG and mapped.catalog is not None:
            mapped_guest_pcs[mapped.catalog.guest_pc] += 1
    mapper.drain()
    # iter_jitdump_records 只有在完整 record boundary 到达 EOF 时才会正常耗尽。
    jit_observation["eof_record_boundary_complete"] = True

    catalog_tail = _last_json_record(catalog_path)
    _require(catalog_tail.get("type") == "quality", "TB catalog 末记录不是 quality")
    _require(
        catalog_tail.get("schema") == "mygo.riscv-tb-catalog.v1",
        "TB catalog quality schema 非法",
    )
    for name in ("write_errors", "dropped_blocks", "tracking_drops"):
        _require(_uint(catalog_tail.get(name), f"catalog.quality.{name}") == 0, f"catalog {name} 非零")
    _require(
        match_stats.catalog_records == _uint(catalog_tail.get("records"), "catalog.records"),
        "流式匹配看到的 catalog 记录数与末尾 quality 不闭合",
    )
    mix_catalog = mix["quality"]["catalog"]
    _require(
        match_stats.catalog_records == int(mix_catalog["records"]),
        "mix quality 与 catalog 记录数不闭合",
    )
    _require(
        match_stats.catalog_match_ratio >= min_catalog_coverage_ratio,
        f"catalog coverage {match_stats.catalog_match_ratio:.6f} 低于阈值",
    )
    _require(
        match_stats.guest_jit_match_ratio == 1.0
        and match_stats.unmatched_guest_loads == 0,
        "存在无法关联 catalog 的 guest JIT load",
    )
    _require(
        match_stats.catalog_container_tids == vcpu_container_tids,
        "catalog 翻译 TID 与 TID namespace 的完整 vCPU 集合不一致",
    )

    total_vcpu_samples = sum(location_samples.values())
    located_vcpu_clock = math.fsum(location_clock.values())
    exact_vcpu_clock = sum(
        int(row["exact_task_clock_ns"]) for row in task_clock_accounting.values()
    )
    unlocated_tail_clock = sum(
        int(row["unlocated_tail_task_clock_ns"])
        for row in task_clock_accounting.values()
    )
    mapped_ratio = (
        location_clock[SampleLocation.MAPPED_TCG.value] / located_vcpu_clock
        if located_vcpu_clock
        else 0.0
    )
    outside_window_clock = math.fsum(outside_clock.values())
    outside_window_ratio = (
        outside_window_clock / located_vcpu_clock if located_vcpu_clock else 0.0
    )
    mean_epoch_located_clock = located_vcpu_clock / len(epochs)
    boundary_period_uncertainty_ratio = (
        boundary_period_uncertainty_ns / mean_epoch_located_clock
        if mean_epoch_located_clock
        else 1.0
    )
    _require(total_vcpu_samples > 0 and located_vcpu_clock > 0, "没有 vCPU task-clock 样本")
    _require(
        located_vcpu_clock + unlocated_tail_clock == exact_vcpu_clock,
        "已定位 sample period + 未定位尾部残量与 per-TID final read 不闭合",
    )
    _require(
        mapped_ratio >= min_jit_sample_mapping_ratio,
        f"vCPU JIT task-clock 映射率 {mapped_ratio:.6f} 低于阈值",
    )
    _require(
        location_clock[SampleLocation.UNKNOWN.value] == 0,
        "存在落入未匹配 guest JIT 区间的 vCPU task-clock",
    )
    _require(
        math.isclose(
            math.fsum(
                row["task_clock_ns"][name]
                for row in per_epoch
                for name in LOCATION_NAMES
            )
            + math.fsum(outside_clock.values()),
            located_vcpu_clock,
            rel_tol=1e-12,
            abs_tol=1e-3,
        ),
        "vCPU task-clock 的 epoch/location 聚合不闭合",
    )

    return {
        "schema": ANALYSIS_SCHEMA,
        "collector": collector,
        "gate_alignment": {
            "collector_gate_start_ns": gate_start_ns,
            "collector_gate_stop_ns": gate_stop_ns,
            "mix_window_start_ns": mix_start_ns,
            "mix_window_stop_ns": mix_stop_ns,
            "start_skew_ns": gate_start_ns - mix_start_ns,
            "stop_skew_ns": gate_stop_ns - mix_stop_ns,
            "overlap_start_ns": max(gate_start_ns, mix_start_ns),
            "overlap_stop_ns": min(gate_stop_ns, mix_stop_ns),
            "sample_period_boundary_uncertainty_ns_per_epoch_boundary": (
                boundary_period_uncertainty_ns
            ),
            "sample_period_boundary_uncertainty_to_mean_epoch_ratio": (
                boundary_period_uncertainty_ratio
            ),
            "max_sample_period_ns_by_vcpu_tid": {
                str(tid): value for tid, value in sorted(max_period_by_vcpu.items())
            },
            "endpoint_policy": "首尾 mix epoch 不进入耗时权重拟合；period 跨内部边界误差只给上界，不伪分摊",
        },
        "vcpu": {
            "identity_source": "CPU-index namespace entries exactly cross-checked by catalog translation container TIDs",
            "host_tids": sorted(vcpu_host_tids),
            "container_tids": sorted(vcpu_container_tids),
            "entries": [dataclasses.asdict(entry) for _, entry in vcpu_entries],
            "task_clock_accounting": {
                str(tid): row for tid, row in sorted(task_clock_accounting.items())
            },
        },
        "epochs": per_epoch,
        "locations": {
            "sample_count": dict(location_samples),
            "task_clock_ns": dict(location_clock),
            "total_samples": total_vcpu_samples,
            "located_task_clock_ns": located_vcpu_clock,
            "exact_task_clock_ns": exact_vcpu_clock,
            "unlocated_tail_task_clock_ns": unlocated_tail_clock,
            "unlocated_tail_task_clock_ratio": unlocated_tail_clock / exact_vcpu_clock,
            "mapped_tcg_task_clock_ratio": mapped_ratio,
            "outside_window_samples": dict(outside_samples),
            "outside_window_task_clock_ns": dict(outside_clock),
            "outside_window_task_clock_total_ns": outside_window_clock,
            "outside_window_task_clock_ratio": outside_window_ratio,
        },
        "non_vcpu": {
            "samples": non_vcpu_samples,
            "task_clock_ns": non_vcpu_clock,
        },
        "native_ips": [
            {
                "ip": ip,
                "samples": native_ips[ip],
                "task_clock_ns": native_ip_clock[ip],
            }
            for ip in sorted(native_ips, key=lambda value: (-native_ip_clock[value], value))
        ],
        "mapped_guest_pcs": [
            {"guest_pc": pc, "samples": count}
            for pc, count in sorted(mapped_guest_pcs.items(), key=lambda item: (-item[1], item[0]))
        ],
        "translation_match": {
            "catalog_records": match_stats.catalog_records,
            "jit_loads": match_stats.jit_loads,
            "guest_jit_loads": match_stats.guest_jit_loads,
            "matched_loads": match_stats.matched_loads,
            "unmatched_guest_loads": match_stats.unmatched_guest_loads,
            "non_guest_loads": match_stats.non_guest_loads,
            "unmatched_catalog_records": match_stats.unmatched_catalog_records,
            "catalog_coverage_ratio": match_stats.catalog_match_ratio,
            "guest_jit_match_ratio": match_stats.guest_jit_match_ratio,
            "catalog_container_tids": sorted(match_stats.catalog_container_tids),
            "jit_container_tids": sorted(match_stats.jit_container_tids),
            **jit_observation,
        },
        "catalog_quality": catalog_tail,
    }


def _segmentation_rows(mix: Mapping[str, Any]) -> list[dict[str, Any]]:
    return [
        {
            "time_ns": epoch["time_ns"],
            "duration_ns": epoch["duration_ns"],
            "counts": epoch["variant_counts"],
            "rate": epoch["rate"],
            "kernel_share": epoch["kernel_share"],
        }
        for epoch in mix["epochs"]
    ]


def analyze_segmentation(
    mix: Mapping[str, Any],
    *,
    min_segment_seconds: int,
    boundary_bootstrap_replicates: int,
    global_permutation_replicates: int,
    permutation_replicates: int,
    seed: int,
) -> dict[str, Any]:
    rows = _segmentation_rows(mix)
    sensitivity = run_segmentation_sensitivity(
        rows,
        bucket_seconds=(1, 2, 5, 10),
        penalty_multipliers=(0.8, 1.0, 1.2),
        min_segment_seconds=min_segment_seconds,
    )
    reference_candidates = [
        record
        for record in sensitivity["configurations"]
        if record["bucket_seconds"] == 1 and record["penalty_multiplier"] == 1.0
    ]
    _require(len(reference_candidates) == 1, "无法确定 1s/1.0 参考分段")
    reference = reference_candidates[0]
    features = prepare_feature_matrix(
        rows, vocabulary=sensitivity["vocabulary"], coverage=sensitivity["coverage"]
    )
    direct = detect_change_points(
        features["matrix"],
        penalty=reference["penalty"],
        min_segment_length=min_segment_seconds,
    )
    _require(direct["boundaries"] == reference["boundaries"], "参考分段重算不一致")
    dependence = diagnose_serial_dependence(
        features["matrix"],
        boundaries=direct["boundaries"],
        feature_names=features["feature_names"],
    )
    bootstrap = moving_block_bootstrap_boundary_stability(
        features["matrix"],
        direct["boundaries"],
        penalty=direct["penalty"],
        min_segment_length=min_segment_seconds,
        replicates=boundary_bootstrap_replicates,
        block_length=dependence["primary_block_length"],
        seed=seed,
    )
    bootstrap["block_length_source"] = "serial_dependence.primary_block_length"
    global_sensitivity = global_change_point_block_sensitivity_test(
        features["matrix"],
        penalty=direct["penalty"],
        min_segment_length=min_segment_seconds,
        dependence=dependence,
        primary_permutations=global_permutation_replicates,
        long_permutations=min(
            global_permutation_replicates,
            max(19, global_permutation_replicates // 3),
        ),
        seed=seed + 1,
    )
    global_test = global_sensitivity["tests"][0]
    permutation = adjacent_segment_block_permutation_js(
        rows,
        direct["boundaries"],
        vocabulary=sensitivity["vocabulary"],
        block_length=min(
            dependence["primary_block_length"],
            min(
                right - left
                for left, right in zip(direct["boundaries"], direct["boundaries"][1:])
            ),
        ),
        permutations=permutation_replicates,
        seed=seed + 2,
    )
    stages = []
    epochs = mix["epochs"]
    window_start = mix["window"]["start_monotonic_ns"]
    for stage, (begin, end) in enumerate(zip(direct["boundaries"], direct["boundaries"][1:])):
        user_count = sum(epoch["user_count"] for epoch in epochs[begin:end])
        kernel_count = sum(epoch["kernel_count"] for epoch in epochs[begin:end])
        stages.append(
            {
                "stage": stage,
                "epoch_begin": begin,
                "epoch_end": end,
                "epoch_count": end - begin,
                "start_monotonic_ns": epochs[begin]["time_ns"],
                "end_monotonic_ns": epochs[end - 1]["end_time_ns"],
                "relative_start_seconds": (epochs[begin]["time_ns"] - window_start)
                / NANOSECONDS_PER_SECOND,
                "relative_end_seconds": (epochs[end - 1]["end_time_ns"] - window_start)
                / NANOSECONDS_PER_SECOND,
                "instruction_count": sum(epoch["total_count"] for epoch in epochs[begin:end]),
                "user_instruction_count": user_count,
                "kernel_instruction_count": kernel_count,
                "kernel_instruction_share": (
                    kernel_count / (user_count + kernel_count)
                    if user_count + kernel_count
                    else 0.0
                ),
            }
        )
    return {
        "method": "Jeffreys-CLR + rate/kernel-share + penalized-SSE",
        "feature_model": {
            key: features[key]
            for key in (
                "vocabulary",
                "components",
                "feature_names",
                "constant_features",
                "effective_dimension",
                "coverage",
                "alpha",
            )
        },
        "reference": direct,
        "sensitivity": sensitivity,
        "boundary_bootstrap": bootstrap,
        "serial_dependence": dependence,
        "global_change_point_test": global_test,
        "global_change_point_block_sensitivity": global_sensitivity,
        "adjacent_permutation": permutation,
        "stages": stages,
    }


def _load_progress_timeline(run_dir: Path, mix: Mapping[str, Any]) -> dict[str, Any]:
    """读取宿主单调时钟上的 Cargo 里程碑；只作解释性标注。"""

    points: dict[int, int] = {}
    source = "none"
    progress_path = run_dir / "progress.tsv"
    if progress_path.is_file():
        with progress_path.open("r", encoding="utf-8", newline="") as stream:
            reader = csv.DictReader(stream, delimiter="\t")
            _require(
                reader.fieldnames == ["milestone", "monotonic_ns"],
                "progress.tsv 表头非法",
            )
            for line_number, row in enumerate(reader, 2):
                try:
                    milestone = int(row["milestone"])
                    timestamp = int(row["monotonic_ns"])
                except (TypeError, ValueError) as error:
                    raise AnalysisError(f"progress.tsv:{line_number}: 整数非法") from error
                _require(0 <= milestone <= 446 and timestamp >= 0, "progress.tsv 值越界")
                points[milestone] = timestamp
        source = "progress.tsv"

    summary_path = run_dir / "summary.json"
    summary: dict[str, Any] | None = None
    if summary_path.is_file():
        try:
            parsed = json.loads(summary_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            raise AnalysisError("summary.json 非法") from error
        if isinstance(parsed, dict):
            summary = parsed
            timing = parsed.get("timing", {})
            if isinstance(timing, dict):
                raw_progress = timing.get("cargo_progress_monotonic_ns", {})
                if isinstance(raw_progress, dict):
                    for raw_milestone, raw_timestamp in raw_progress.items():
                        if raw_timestamp is not None:
                            points.setdefault(int(raw_milestone), int(raw_timestamp))
                    if points and source == "none":
                        source = "summary.json"
                for progress_name, time_name in (
                    ("window_start_progress", "window_start_monotonic_ns"),
                    ("window_stop_progress", "measurement_stop_monotonic_ns"),
                ):
                    progress = timing.get(progress_name)
                    timestamp = timing.get(time_name)
                    if isinstance(progress, int) and progress >= 0 and isinstance(timestamp, int):
                        points.setdefault(progress, timestamp)

    serial_max: int | None = None
    serial_path = run_dir / "profile.serial.log"
    if serial_path.is_file():
        pattern = re.compile(r"(?<![0-9])([0-9]{1,3})/446(?![0-9])")
        with serial_path.open("r", encoding="utf-8", errors="replace") as stream:
            for line in stream:
                for match in pattern.finditer(line.replace("\r", "\n")):
                    value = int(match.group(1))
                    if value <= 446:
                        serial_max = max(serial_max if serial_max is not None else -1, value)
        if source == "none" and serial_max is not None:
            source = "profile.serial.log:max-only"

    ordered = [
        {"progress": milestone, "monotonic_ns": timestamp}
        for milestone, timestamp in sorted(points.items(), key=lambda item: (item[1], item[0]))
    ]
    return {
        "source": source,
        "points": ordered,
        "serial_max_progress": serial_max,
        "window_start_ns": mix["window"]["start_monotonic_ns"],
    }


def _annotate_stages(
    segmentation: dict[str, Any], progress: Mapping[str, Any]
) -> None:
    points = list(progress["points"])
    for stage in segmentation["stages"]:
        begin = stage["start_monotonic_ns"]
        end = stage["end_monotonic_ns"]
        before_begin = [row["progress"] for row in points if row["monotonic_ns"] <= begin]
        before_end = [row["progress"] for row in points if row["monotonic_ns"] <= end]
        covered = [
            row["progress"]
            for row in points
            if begin < row["monotonic_ns"] <= end
        ]
        stage["cargo_progress"] = {
            "source": progress["source"],
            "start": max(before_begin) if before_begin else None,
            "end": max(before_end) if before_end else None,
            "milestones_reached": sorted(set(covered)),
            "serial_max_progress": progress["serial_max_progress"],
            "annotation_only": True,
        }


def fit_weights(
    mix: Mapping[str, Any],
    perf: Mapping[str, Any],
    boundaries: Sequence[int],
    *,
    weight_bootstrap_replicates: int,
    distribution_bootstrap_replicates: int,
    block_length: int,
    seed: int,
) -> dict[str, Any]:
    _require(len(mix["epochs"]) == len(perf["epochs"]), "mix/perf epoch 数不一致")
    model_rows: list[dict[str, Any]] = []
    for mix_epoch, perf_epoch in zip(mix["epochs"], perf["epochs"], strict=True):
        clock = sum(perf_epoch["task_clock_ns"].values())
        model_rows.append(
            {
                "time_ns": mix_epoch["time_ns"],
                "duration_ns": mix_epoch["duration_ns"],
                "exact_counts": mix_epoch["mnemonic_counts"],
                "vcpu_task_clock_ns": clock,
                "executed_tb_count": mix_epoch["executed_tb_count"],
                "translated_tb_delta": mix_epoch["translated_tb_delta"],
                "translated_insns_delta": mix_epoch["translated_insns_delta"],
            }
        )
    _require(len(model_rows) >= 22, "排除首尾边界 epoch 后不足 20 个耗时模型样本")
    fit_rows = model_rows[1:-1]
    fit_boundaries = sorted(
        {
            min(len(fit_rows), max(0, int(boundary) - 1))
            for boundary in boundaries
        }
    )
    _require(
        fit_boundaries[0] == 0
        and fit_boundaries[-1] == len(fit_rows)
        and all(
            right - left >= 3
            for left, right in zip(fit_boundaries, fit_boundaries[1:])
        ),
        "排除首尾 epoch 后的权重依赖诊断阶段过短",
    )
    dependence_feature_names = [
        "log1p:vcpu-task-clock-rate",
        "log1p:instruction-rate",
        "log1p:executed-tb-rate",
        "log1p:translated-tb-rate",
        "log1p:translated-insn-rate",
    ]
    dependence_matrix = standardize_matrix(
        [
            [
                math.log1p(
                    float(row["vcpu_task_clock_ns"])
                    * NANOSECONDS_PER_SECOND
                    / int(row["duration_ns"])
                ),
                math.log1p(
                    math.fsum(float(value) for value in row["exact_counts"].values())
                    * NANOSECONDS_PER_SECOND
                    / int(row["duration_ns"])
                ),
                math.log1p(
                    float(row["executed_tb_count"])
                    * NANOSECONDS_PER_SECOND
                    / int(row["duration_ns"])
                ),
                math.log1p(
                    float(row["translated_tb_delta"])
                    * NANOSECONDS_PER_SECOND
                    / int(row["duration_ns"])
                ),
                math.log1p(
                    float(row["translated_insns_delta"])
                    * NANOSECONDS_PER_SECOND
                    / int(row["duration_ns"])
                ),
            ]
            for row in fit_rows
        ]
    )["matrix"]
    weight_dependence = diagnose_serial_dependence(
        dependence_matrix,
        boundaries=fit_boundaries,
        feature_names=dependence_feature_names,
    )
    model_block_length = min(
        len(fit_rows),
        max(block_length, int(weight_dependence["primary_block_length"])),
    )
    model = fit_instruction_weight_model(
        fit_rows,
        bootstrap_replicates=weight_bootstrap_replicates,
        block_length=model_block_length,
        seed=seed,
    )
    model["bootstrap"]["block_length_source"] = (
        "max(segmentation,weight-response)-acf-variogram-iat-diagnostic"
    )
    weights = {row["instruction"]: row for row in model["instructions"]}
    descriptors = {row["variant"]: row for row in mix["descriptors"]}
    weighted_rows: list[dict[str, Any]] = []
    model_epochs_by_time = {row["time_ns"]: row for row in model["epochs"]}
    for mix_epoch in mix["epochs"]:
        values: dict[str, float] = {}
        exact_count: dict[str, float] = {}
        attributed: dict[str, float] = {}
        weight_map: dict[str, float] = {}
        shrinkage: dict[str, float] = {}
        source: dict[str, str] = {}
        unattributed: dict[str, float] = {}
        for variant, count in mix_epoch["variant_counts"].items():
            descriptor = descriptors[variant]
            mnemonic = descriptor["model_mnemonic"]
            exact_count[variant] = count
            model_weight = weights.get(mnemonic)
            if model_weight is None:
                unattributed[variant] = count
                source[variant] = "missing-mnemonic-weight"
                continue
            cost = float(model_weight["ns_per_instruction"]) * count
            values[variant] = cost
            attributed[variant] = cost
            weight_map[variant] = float(model_weight["ns_per_instruction"])
            shrinkage[variant] = float(model_weight["shrinkage"])
            source[variant] = (
                f"{model_weight['source']};mnemonic-shared-across-encoding-size"
            )
        model_epoch = model_epochs_by_time.get(mix_epoch["time_ns"])
        if model_epoch is not None:
            _require(
                math.isclose(
                    sum(values.values()),
                    float(model_epoch["attributed_instruction_ns"]),
                    rel_tol=1e-9,
                    abs_tol=1e-3,
                ),
                "mnemonic 权重回填到 encoding-size 变体后不闭合",
            )
        weighted_rows.append(
            {
                "time_ns": mix_epoch["time_ns"],
                "values": values,
                "exact_count": exact_count,
                "attributed_task_clock_ns": attributed,
                "weight_ns_per_instruction": weight_map,
                "shrinkage": shrinkage,
                "source": source,
                "unattributed": unattributed,
            }
        )
    stage_dependence: list[dict[str, Any]] = []
    stage_block_lengths: list[int] = []
    for stage, (begin, end) in enumerate(zip(boundaries, boundaries[1:])):
        stage_rows = weighted_rows[begin:end]
        stage_features = prepare_feature_matrix(
            [
                {
                    "time_ns": row["time_ns"],
                    "counts": row["values"],
                    "rate": math.fsum(row["values"].values()),
                    "kernel_share": 0.0,
                }
                for row in stage_rows
            ]
        )
        diagnostic = diagnose_serial_dependence(
            stage_features["matrix"],
            feature_names=stage_features["feature_names"],
        )
        requested_block = max(
            model_block_length, int(diagnostic["primary_block_length"])
        )
        effective_block = min(len(stage_rows), requested_block)
        stage_block_lengths.append(effective_block)
        stage_dependence.append(
            {
                "stage": stage,
                "diagnostic": diagnostic,
                "minimum_shared_block_length": model_block_length,
                "requested_block_length": requested_block,
                "effective_block_length": effective_block,
            }
        )
    stage_distributions = weighted_stage_distributions(
        weighted_rows,
        boundaries,
        block_lengths=stage_block_lengths,
        vocabulary=sorted(descriptors),
        replicates=distribution_bootstrap_replicates,
        top_k=10,
        seed=seed + 2,
    )
    variant_to_mnemonic = {
        variant: descriptor["model_mnemonic"]
        for variant, descriptor in descriptors.items()
    }
    weight_bounds: dict[str, tuple[float, float]] = {}
    for mnemonic, row in weights.items():
        interval = row.get("confidence_interval")
        if (
            isinstance(interval, list)
            and len(interval) == 2
            and all(isinstance(value, (int, float)) for value in interval)
        ):
            weight_bounds[mnemonic] = (float(interval[0]), float(interval[1]))
    for stage, stage_dependency in zip(
        stage_distributions, stage_dependence, strict=True
    ):
        distribution = stage["distribution"]
        effective_block = int(stage_dependency["effective_block_length"])
        diagnostic = stage_dependency["diagnostic"]
        stage["dependence_support"] = {
            "source": "stage-weighted-acf-variogram-iat-with-shared-minimum",
            "diagnostic": diagnostic,
            "minimum_shared_block_length": model_block_length,
            "requested_block_length": stage_dependency["requested_block_length"],
            "effective_block_length": effective_block,
            "nominal_blocks": int(distribution["row_count"]) / effective_block,
            "minimum_nominal_blocks": 8,
            "adequate_for_high_confidence": (
                diagnostic["adequate_for_high_confidence"]
                and stage_dependency["requested_block_length"]
                <= int(distribution["row_count"]) // 8
                and
                int(distribution["row_count"]) / effective_block >= 8.0
            ),
        }
        items = {
            item["instruction"]: item
            for item in distribution["items"]
            if item["instruction"] != "OTHER"
        }
        for item in items.values():
            item["confidence_interval_scope"] = "conditional-on-point-estimated-weights"
        missing_bounds = sorted(
            {
                variant_to_mnemonic[variant]
                for variant, item in items.items()
                if item["exact_count"] > 0
                and variant_to_mnemonic[variant] not in weight_bounds
            }
        )
        ranked_point = sorted(
            items,
            key=lambda name: (-float(items[name]["weighted_cost"]), name),
        )
        point_top10 = ranked_point[: min(10, len(ranked_point))]
        nonrobust: list[str] = []
        if not missing_bounds:
            for candidate, candidate_item in items.items():
                candidate_mnemonic = variant_to_mnemonic[candidate]

                def scenario(candidate_upper: bool) -> dict[str, float]:
                    costs: dict[str, float] = {}
                    for variant, item in items.items():
                        if float(item["exact_count"]) == 0.0:
                            costs[variant] = 0.0
                            continue
                        mnemonic = variant_to_mnemonic[variant]
                        lower, upper = weight_bounds[mnemonic]
                        if mnemonic == candidate_mnemonic:
                            selected_weight = upper if candidate_upper else lower
                        else:
                            selected_weight = lower if candidate_upper else upper
                        costs[variant] = float(item["exact_count"]) * selected_weight
                    return costs

                lower_costs = scenario(False)
                upper_costs = scenario(True)
                lower_total = math.fsum(lower_costs.values())
                upper_total = math.fsum(upper_costs.values())
                candidate_item["weight_ci_share_envelope"] = [
                    lower_costs[candidate] / lower_total if lower_total else 0.0,
                    upper_costs[candidate] / upper_total if upper_total else 0.0,
                ]
                worst_rank = sorted(
                    lower_costs,
                    key=lambda name: (-lower_costs[name], name),
                )
                robust = candidate in worst_rank[: min(10, len(worst_rank))]
                candidate_item["top10_robust_to_weight_ci"] = robust
                if candidate in point_top10 and not robust:
                    nonrobust.append(candidate)
        else:
            for item in items.values():
                item["weight_ci_share_envelope"] = None
                item["top10_robust_to_weight_ci"] = None
            nonrobust = list(point_top10)
        stage["weight_uncertainty"] = {
            "method": "marginal-weight-CI-adversarial-envelope",
            "available": not missing_bounds,
            "missing_mnemonic_intervals": missing_bounds,
            "point_top10": point_top10,
            "nonrobust_point_top10": nonrobust,
            "all_point_top10_robust": not missing_bounds and not nonrobust,
            "interpretation": (
                "每个候选 mnemonic 取下界、所有竞争 mnemonic 取上界检查最坏排名；"
                "share 上下界为相反两种对抗组合。它是边际区间敏感性包络，不是联合置信区间。"
            ),
        }
    return {
        "model": model,
        "fit_epoch_selection": {
            "method": "exclude-first-and-last-mix-epochs",
            "included_epoch_begin": 1,
            "included_epoch_end": len(model_rows) - 1,
            "excluded_epochs": [0, len(model_rows) - 1],
            "reason": "降低 perf gate/mix 检测偏差与端点 sample-period 跨界对权重的污染",
        },
        "bootstrap_dependence": {
            "source": "max(segmentation.serial_dependence,weight-response diagnostic)",
            "segmentation_primary_block_length": block_length,
            "weight_response_diagnostic": weight_dependence,
            "model_block_length": model_block_length,
            "adequate_for_high_confidence": (
                weight_dependence["adequate_for_high_confidence"]
            ),
            "stage_support": [
                {
                    "stage": stage["stage"],
                    **stage["dependence_support"],
                }
                for stage in stage_distributions
            ],
        },
        "weighted_rows": weighted_rows,
        "stage_distributions": stage_distributions,
    }


def _write_raw_outputs(analysis_dir: Path, mix: Mapping[str, Any]) -> None:
    total = mix["totals"]["total"]
    _atomic_write_csv(
        analysis_dir / "instruction-list.csv",
        (
            "descriptor_id",
            "mnemonic",
            "size_bytes",
            "user_count",
            "kernel_count",
            "total_count",
            "total_share",
        ),
        (
            (
                row["id"],
                row["mnemonic"],
                row["size"],
                row["user"],
                row["kernel"],
                row["total"],
                row["total"] / total if total else 0.0,
            )
            for row in sorted(mix["descriptors"], key=lambda item: (-item["total"], item["id"]))
        ),
    )
    _atomic_write_csv(
        analysis_dir / "epoch-instruction-counts.csv",
        ("epoch", "descriptor_id", "mnemonic", "size_bytes", "user", "kernel", "total"),
        (
            (
                epoch["epoch"],
                row["descriptor_id"],
                next(
                    item["mnemonic"]
                    for item in mix["descriptors"]
                    if item["id"] == row["descriptor_id"]
                ),
                next(
                    item["size"]
                    for item in mix["descriptors"]
                    if item["id"] == row["descriptor_id"]
                ),
                row["user"],
                row["kernel"],
                row["total"],
            )
            for epoch in mix["epochs"]
            for row in epoch["variant_domain_counts"].values()
        ),
    )


def _write_perf_outputs(analysis_dir: Path, perf: Mapping[str, Any]) -> None:
    total_native = sum(row["task_clock_ns"] for row in perf["native_ips"])
    _atomic_write_csv(
        analysis_dir / "perf-location-summary.csv",
        ("location", "samples", "task_clock_ns", "task_clock_share"),
        (
            (
                name,
                perf["locations"]["sample_count"].get(name, 0),
                perf["locations"]["task_clock_ns"].get(name, 0),
                perf["locations"]["task_clock_ns"].get(name, 0)
                / perf["locations"]["located_task_clock_ns"],
            )
            for name in LOCATION_NAMES
        ),
    )
    _atomic_write_csv(
        analysis_dir / "native-ip-distribution.csv",
        ("ip", "samples", "task_clock_ns", "native_task_clock_share"),
        (
            (
                f"0x{row['ip']:x}",
                row["samples"],
                row["task_clock_ns"],
                row["task_clock_ns"] / total_native if total_native else 0.0,
            )
            for row in perf["native_ips"]
        ),
    )


def _write_timeline(
    analysis_dir: Path,
    mix: Mapping[str, Any],
    perf: Mapping[str, Any],
    segmentation: Mapping[str, Any],
) -> None:
    stage_by_epoch: dict[int, int] = {}
    for stage in segmentation["stages"]:
        for index in range(stage["epoch_begin"], stage["epoch_end"]):
            stage_by_epoch[index] = stage["stage"]
    window_start = mix["window"]["start_monotonic_ns"]
    _atomic_write_csv(
        analysis_dir / "epoch-timeline.csv",
        (
            "epoch",
            "stage",
            "relative_start_seconds",
            "duration_seconds",
            "user_instructions",
            "kernel_instructions",
            "total_instructions",
            "raw_instruction_delta_user",
            "raw_instruction_delta_kernel",
            "counter_snapshot_skew_user",
            "counter_snapshot_skew_kernel",
            "instructions_per_second",
            "kernel_share",
            "executed_tb_count",
            "translated_tb_delta",
            "translated_insns_delta",
            "vcpu_task_clock_ns",
            "mapped_tcg_task_clock_ns",
            "native_qemu_task_clock_ns",
            "unknown_task_clock_ns",
        ),
        (
            (
                epoch["epoch"],
                stage_by_epoch[index],
                (epoch["time_ns"] - window_start) / NANOSECONDS_PER_SECOND,
                epoch["duration_ns"] / NANOSECONDS_PER_SECOND,
                epoch["user_count"],
                epoch["kernel_count"],
                epoch["total_count"],
                epoch["raw_instruction_delta"]["user"],
                epoch["raw_instruction_delta"]["kernel"],
                epoch["counter_snapshot_skew"]["user"],
                epoch["counter_snapshot_skew"]["kernel"],
                epoch["rate"],
                epoch["kernel_share"],
                epoch["executed_tb_count"],
                epoch["translated_tb_delta"],
                epoch["translated_insns_delta"],
                sum(perf["epochs"][index]["task_clock_ns"].values()),
                perf["epochs"][index]["task_clock_ns"][SampleLocation.MAPPED_TCG.value],
                perf["epochs"][index]["task_clock_ns"][SampleLocation.NATIVE_QEMU.value],
                perf["epochs"][index]["task_clock_ns"][SampleLocation.UNKNOWN.value],
            )
            for index, epoch in enumerate(mix["epochs"])
        ),
    )
    _atomic_write_csv(
        analysis_dir / "stages.csv",
        (
            "stage",
            "epoch_begin",
            "epoch_end_exclusive",
            "epoch_count",
            "relative_start_seconds",
            "relative_end_seconds",
            "instruction_count",
            "user_instruction_count",
            "kernel_instruction_count",
            "kernel_instruction_share",
            "cargo_progress_start",
            "cargo_progress_end",
            "cargo_milestones_reached",
        ),
        (
            (
                row["stage"],
                row["epoch_begin"],
                row["epoch_end"],
                row["epoch_count"],
                row["relative_start_seconds"],
                row["relative_end_seconds"],
                row["instruction_count"],
                row["user_instruction_count"],
                row["kernel_instruction_count"],
                row["kernel_instruction_share"],
                row["cargo_progress"]["start"] if row["cargo_progress"]["start"] is not None else "",
                row["cargo_progress"]["end"] if row["cargo_progress"]["end"] is not None else "",
                ",".join(str(value) for value in row["cargo_progress"]["milestones_reached"]),
            )
            for row in segmentation["stages"]
        ),
    )


def _write_weight_outputs(
    analysis_dir: Path,
    mix: Mapping[str, Any],
    segmentation: Mapping[str, Any],
    weights: Mapping[str, Any],
) -> None:
    model_by_mnemonic = {
        row["instruction"]: row for row in weights["model"]["instructions"]
    }
    rows: list[tuple[Any, ...]] = []
    for stage_result, stage in zip(
        weights["stage_distributions"], segmentation["stages"], strict=True
    ):
        domain_counts: dict[str, dict[str, int]] = collections.defaultdict(
            lambda: {"user": 0, "kernel": 0}
        )
        for epoch in mix["epochs"][stage["epoch_begin"] : stage["epoch_end"]]:
            for variant, counts in epoch["variant_domain_counts"].items():
                domain_counts[variant]["user"] += counts["user"]
                domain_counts[variant]["kernel"] += counts["kernel"]
        items = {
            item["instruction"]: item
            for item in stage_result["distribution"]["items"]
            if item["instruction"] != "OTHER"
        }
        _require(
            set(items) == set(domain_counts) | {
                descriptor["variant"] for descriptor in mix["descriptors"]
            },
            "阶段带权分布没有保留完整 encoding-size 词表",
        )
        _require(
            math.isclose(
                sum(item["exact_count"] for item in items.values()),
                stage["instruction_count"],
            ),
            "阶段带权分布的 exact_count 与 mix 不闭合",
        )
        _require(
            math.isclose(
                sum(item["weighted_cost"] for item in stage_result["distribution"]["items"]),
                stage_result["distribution"]["weighted_cost_total"],
                rel_tol=1e-12,
                abs_tol=1e-3,
            ),
            "阶段 weighted cost 不闭合",
        )
        _require(
            math.isclose(
                sum(item["share"] for item in stage_result["distribution"]["items"]),
                1.0,
                rel_tol=1e-12,
                abs_tol=1e-12,
            ),
            "阶段 weighted share 不闭合",
        )
        for descriptor in mix["descriptors"]:
            item = items.get(descriptor["variant"])
            model = model_by_mnemonic.get(descriptor["model_mnemonic"])
            domains = domain_counts[descriptor["variant"]]
            domain_total = domains["user"] + domains["kernel"]
            if item is not None:
                _require(
                    math.isclose(item["exact_count"], domain_total),
                    "阶段逐域计数与 weighted exact_count 不闭合",
                )
            rows.append(
                (
                    stage["stage"],
                    descriptor["id"],
                    descriptor["mnemonic"],
                    descriptor["size"],
                    domains["user"],
                    domains["kernel"],
                    domain_total,
                    domains["kernel"] / domain_total if domain_total else 0.0,
                    model["ns_per_instruction"] if model else "",
                    (
                        model["confidence_interval"][0]
                        if model and model["confidence_interval"] is not None
                        else ""
                    ),
                    (
                        model["confidence_interval"][1]
                        if model and model["confidence_interval"] is not None
                        else ""
                    ),
                    item["weighted_cost"] if item else 0.0,
                    item["share"] if item else 0.0,
                    item["confidence_interval"][0] if item else "",
                    item["confidence_interval"][1] if item else "",
                    (
                        item["weight_ci_share_envelope"][0]
                        if item and item["weight_ci_share_envelope"] is not None
                        else ""
                    ),
                    (
                        item["weight_ci_share_envelope"][1]
                        if item and item["weight_ci_share_envelope"] is not None
                        else ""
                    ),
                    item["top10_robust_to_weight_ci"] if item else "",
                    item["effective_sample_size"] if item else "",
                    item["top_k_probability"] if item else "",
                    model["identifiability"] if model else "missing",
                    model["shrinkage"] if model else "",
                    (
                        f"{model['source']};mnemonic-shared-across-encoding-size"
                        if model
                        else "missing-mnemonic-weight"
                    ),
                )
            )
    _atomic_write_csv(
        analysis_dir / "stage-weighted-instructions.csv",
        (
            "stage",
            "descriptor_id",
            "mnemonic",
            "size_bytes",
            "user_count",
            "kernel_count",
            "exact_count",
            "kernel_share",
            "weight_ns_per_instruction",
            "weight_ci_low_ns_per_instruction",
            "weight_ci_high_ns_per_instruction",
            "weighted_cost_ns",
            "weighted_share",
            "conditional_point_weight_share_ci_low",
            "conditional_point_weight_share_ci_high",
            "weight_ci_share_envelope_low",
            "weight_ci_share_envelope_high",
            "top10_robust_to_weight_ci",
            "effective_sample_size",
            "top10_probability",
            "identifiability",
            "shrinkage",
            "source",
        ),
        rows,
    )
    public_model = dict(weights["model"])
    public_model["fit_epoch_selection"] = weights["fit_epoch_selection"]
    _atomic_write_json(analysis_dir / "weight-model.json", public_model)
    _atomic_write_json(
        analysis_dir / "stage-weighted-distributions.json",
        weights["stage_distributions"],
    )


def _quality_summary(
    mix: Mapping[str, Any],
    perf: Mapping[str, Any],
    segmentation: Mapping[str, Any],
    weights: Mapping[str, Any],
) -> dict[str, Any]:
    confidence = assess_distribution_confidence(
        segmentation["sensitivity"],
        segmentation["boundary_bootstrap"],
        segmentation["adjacent_permutation"],
        weights["stage_distributions"],
    )
    conditional_warnings: list[str] = []
    if not confidence["adjacent_distributions_distinct"]:
        conditional_warnings.append(
            "给定已选择边界后，相邻段 JS/Holm 条件检验未全部显著（探索性结果）"
        )
    confidence["reasons"] = [
        reason
        for reason in confidence["reasons"]
        if reason != "相邻阶段分布差异未通过 Holm 校正"
    ]
    confidence["conditional_warnings"] = conditional_warnings
    if len(segmentation["stages"]) == 1:
        confidence["adjacent_distributions_distinct"] = True
        confidence["conditional_warnings"] = []
        confidence["reasons"] = [
            reason
            for reason in confidence["reasons"]
            if reason != "相邻阶段分布差异未通过 Holm 校正"
        ]
        confidence["high_confidence"] = all(
            (
                confidence["sensitivity_ok"],
                confidence["boundary_stability_ok"],
                confidence["weighted_effective_sample_size_ok"],
            )
        )
    model = weights["model"]
    descriptors = {row["variant"]: row for row in mix["descriptors"]}
    model_by_mnemonic = {row["instruction"]: row for row in model["instructions"]}
    weak_weights = [
        row["instruction"]
        for row in model["instructions"]
        if row["identifiability"] in ("weak", "not-identifiable")
    ]
    weak_stage_top10: list[dict[str, Any]] = []
    for stage in weights["stage_distributions"]:
        ranked = [
            item
            for item in stage["distribution"]["items"]
            if item["instruction"] != "OTHER"
        ][:10]
        for item in ranked:
            descriptor = descriptors.get(item["instruction"])
            model_row = (
                model_by_mnemonic.get(descriptor["model_mnemonic"])
                if descriptor is not None
                else None
            )
            if model_row is None or model_row["identifiability"] in (
                "weak",
                "not-identifiable",
            ):
                weak_stage_top10.append(
                    {
                        "stage": stage["stage"],
                        "instruction": item["instruction"],
                        "mnemonic": descriptor["model_mnemonic"] if descriptor else None,
                        "identifiability": (
                            model_row["identifiability"] if model_row else "missing"
                        ),
                    }
                )
    cv_quality = model["blocked_cv"].get("quality")
    bootstrap_replicates = int(model["bootstrap"].get("replicates", 0))
    bootstrap_converged = int(model["bootstrap"].get("converged_replicates", 0))
    bootstrap_convergence = (
        bootstrap_converged / bootstrap_replicates if bootstrap_replicates else 0.0
    )
    fit = model["fit"]
    weight_model_checks = {
        "fit_converged": fit.get("converged") is True,
        "counter_snapshot_skew_bounded": mix["count_closure"][
            "window_cumulative_snapshot_skew_within_bound"
        ],
        "blocked_cv_good": cv_quality == "good",
        "bootstrap_replicates_sufficient": bootstrap_replicates >= 100,
        "bootstrap_convergence_at_least_90pct": bootstrap_convergence >= 0.90,
        "residual_fraction_acceptable": fit.get(
            "unattributed_share_of_task_clock", 1.0
        )
        <= 0.10
        and fit.get("overattributed_share_of_task_clock", 1.0) <= 0.10,
        "perf_window_alignment": perf["locations"].get(
            "outside_window_task_clock_ratio", 1.0
        )
        <= 0.005,
        "collector_mix_gate_skew": max(
            abs(int(perf["gate_alignment"]["start_skew_ns"])),
            abs(int(perf["gate_alignment"]["stop_skew_ns"])),
        )
        <= 1_100_000_000,
        "gate_mismatch_covered_by_excluded_boundary_epochs": (
            max(0, int(perf["gate_alignment"]["start_skew_ns"]))
            <= int(mix["epochs"][0]["duration_ns"])
            and max(0, -int(perf["gate_alignment"]["stop_skew_ns"]))
            <= int(mix["epochs"][-1]["duration_ns"])
        ),
        "sample_period_boundary_uncertainty": perf["gate_alignment"][
            "sample_period_boundary_uncertainty_to_mean_epoch_ratio"
        ]
        <= 0.01,
        "unlocated_tail_fraction": perf["locations"][
            "unlocated_tail_task_clock_ratio"
        ]
        <= 0.005,
        "boundary_epoch_duration": min(
            int(mix["epochs"][0]["duration_ns"]),
            int(mix["epochs"][-1]["duration_ns"]),
        )
        >= NANOSECONDS_PER_SECOND // 2,
        "stage_top10_identifiable": not weak_stage_top10,
        "stage_top10_robust_to_weight_intervals": all(
            stage["weight_uncertainty"]["all_point_top10_robust"]
            for stage in weights["stage_distributions"]
        ),
        "weight_response_dependence_diagnostic_adequate": weights[
            "bootstrap_dependence"
        ]["adequate_for_high_confidence"],
    }
    weight_model_high_confidence = all(weight_model_checks.values())
    block_sensitivity = segmentation["global_change_point_block_sensitivity"]
    selection_corrected_change_evidence = (
        block_sensitivity["high_confidence_eligible"]
        and (
            block_sensitivity["all_fail_to_reject_single_segment"]
            if len(segmentation["stages"]) == 1
            else block_sensitivity["all_reject_single_segment"]
        )
    )
    count_distribution_high_confidence = (
        confidence["sensitivity_ok"]
        and confidence["boundary_stability_ok"]
        and selection_corrected_change_evidence
    )
    stage_dependence_adequate = all(
        stage["dependence_support"]["adequate_for_high_confidence"]
        for stage in weights["stage_distributions"]
    )
    weighted_distribution_sampling_high_confidence = (
        confidence["weighted_effective_sample_size_ok"]
        and stage_dependence_adequate
    )
    confidence["count_distribution_high_confidence"] = (
        count_distribution_high_confidence
    )
    confidence["selection_corrected_change_evidence"] = (
        selection_corrected_change_evidence
    )
    confidence["adjacent_js_is_conditional_exploratory"] = True
    confidence["weight_model_high_confidence"] = weight_model_high_confidence
    confidence["weighted_distribution_sampling_high_confidence"] = (
        weighted_distribution_sampling_high_confidence
    )
    confidence["stage_dependence_diagnostics_adequate"] = (
        stage_dependence_adequate
    )
    confidence["weight_model_checks"] = weight_model_checks
    confidence["weight_bootstrap_convergence_fraction"] = bootstrap_convergence
    confidence["weak_stage_top10_weights"] = weak_stage_top10
    confidence["high_confidence"] = (
        count_distribution_high_confidence
        and weighted_distribution_sampling_high_confidence
        and weight_model_high_confidence
    )
    if not weight_model_high_confidence:
        confidence["reasons"].append("耗时权重模型未通过收敛/CV/bootstrap/残差/可辨识性门禁")
    if not stage_dependence_adequate:
        confidence["reasons"].append("至少一个阶段无法容纳足够的相关长度 block")
    if not block_sensitivity["dependence_adequate"]:
        confidence["reasons"].append("ACF/IAT 所需相关长度无法容纳足够的独立长 block")
    if not block_sensitivity["conclusions_agree"]:
        confidence["reasons"].append("选择校正全局检验对主/长 block 敏感")
    elif not selection_corrected_change_evidence:
        confidence["reasons"].append(
            "选择校正的主/长 block 全局检验与参考阶段数不一致"
        )
    return {
        "schema": ANALYSIS_SCHEMA,
        "valid": True,
        "count_closure": mix["count_closure"],
        "measurement": {
            "epochs": len(mix["epochs"]),
            "registered_descriptor_count": mix["quality"]["descriptor_count"],
            "observed_dynamic_descriptor_count": len(mix["descriptors"]),
            "instructions": mix["totals"],
            "located_vcpu_task_clock_ns": perf["locations"]["located_task_clock_ns"],
            "exact_vcpu_task_clock_ns": perf["locations"]["exact_task_clock_ns"],
            "unlocated_tail_task_clock_ns": perf["locations"][
                "unlocated_tail_task_clock_ns"
            ],
            "unlocated_tail_task_clock_ratio": perf["locations"][
                "unlocated_tail_task_clock_ratio"
            ],
            "mapped_tcg_task_clock_ratio": perf["locations"][
                "mapped_tcg_task_clock_ratio"
            ],
            "outside_window_task_clock_ratio": perf["locations"][
                "outside_window_task_clock_ratio"
            ],
            "first_epoch_duration_ns": mix["epochs"][0]["duration_ns"],
            "last_epoch_duration_ns": mix["epochs"][-1]["duration_ns"],
            "gate_alignment": perf["gate_alignment"],
            "catalog_coverage_ratio": perf["translation_match"]["catalog_coverage_ratio"],
            "guest_jit_match_ratio": perf["translation_match"]["guest_jit_match_ratio"],
            "collector_running_ratio_ppm": perf["collector"]["quality"][
                "running_ratio_ppm"
            ],
        },
        "segmentation": {
            "stages": len(segmentation["stages"]),
            "serial_dependence": segmentation["serial_dependence"],
            "sensitivity_configurations": len(
                segmentation["sensitivity"]["configurations"]
            ),
            "all_adjacent_pairs_significant": segmentation[
                "adjacent_permutation"
            ]["all_adjacent_pairs_significant"],
            "selection_corrected_global_test": segmentation[
                "global_change_point_test"
            ],
            "selection_corrected_block_sensitivity": segmentation[
                "global_change_point_block_sensitivity"
            ],
        },
        "weight_model": {
            "quality": model["quality"],
            "fit": model["fit"],
            "blocked_cv": model["blocked_cv"],
            "bootstrap": model["bootstrap"],
            "fit_epoch_selection": weights["fit_epoch_selection"],
            "bootstrap_dependence": weights["bootstrap_dependence"],
            "weak_or_unidentifiable_instructions": weak_weights,
            "weak_stage_top10_weights": weak_stage_top10,
        },
        "statistical_confidence": confidence,
        "scope": {
            "within_run": "量化该轨迹内、给定分段/点权重/平稳块假设的条件不确定性；不构成无条件证明",
            "cross_run": "单次运行不能证明跨冷启动稳定；至少需要 3 次独立冷运行",
        },
    }


def _format_percent(value: float) -> str:
    return f"{100.0 * value:.2f}%"


def _build_report(
    run_dir: Path,
    mix: Mapping[str, Any],
    perf: Mapping[str, Any],
    segmentation: Mapping[str, Any],
    weights: Mapping[str, Any],
    quality: Mapping[str, Any],
) -> str:
    confidence = quality["statistical_confidence"]
    fit = weights["model"]["fit"]
    r_squared = fit.get("r_squared")
    r_squared_text = f"{r_squared:.4f}" if r_squared is not None else "不可定义"
    unlocated_tail = perf["locations"]["unlocated_tail_task_clock_ns"]
    lines = [
        "# RISC-V BuildStorm 指令耗时分析",
        "",
        f"运行目录：`{run_dir}`。本报告覆盖 {len(mix['epochs'])} 个 1 秒 epoch、"
        f"{mix['totals']['total']:,} 条动态指令和 "
        f"{perf['locations']['located_task_clock_ns'] / 1e9:.3f} 秒已定位 vCPU task-clock"
        f"（final read 精确总量 {perf['locations']['exact_task_clock_ns'] / 1e9:.3f} 秒）。",
        "",
        "## 数据完整性",
        "",
        f"逐 descriptor 与 mix_delta 在每个 epoch 闭合；独立 instruction counter 的 SMP 异步快照"
        f"最大相对 skew 为 {_format_percent(mix['count_closure']['max_relative_snapshot_skew'])}，"
        f"全窗口累计相对 skew 为 "
        f"{_format_percent(mix['count_closure']['window_cumulative_snapshot_skew_ratio'])}，"
        "均低于显式门槛。perf running ratio 为 "
        f"{perf['collector']['quality']['running_ratio_ppm'] / 10000:.3f}%，"
        f"无 lost/throttle/discard；"
        f"guest JIT load→catalog 匹配率为 "
        f"{_format_percent(perf['translation_match']['guest_jit_match_ratio'])}；"
        f"catalog→JIT coverage 为 "
        f"{_format_percent(perf['translation_match']['catalog_coverage_ratio'])}，"
        f"尾部孤儿记录 {perf['translation_match']['unmatched_catalog_records']} 条；jitdump 在完整 record "
        f"边界到达 EOF（QEMU 10 的 CODE_CLOSE={perf['translation_match']['close_records']}，可为 0）。",
        f"插件全局 registry 注册了 {mix['quality']['descriptor_count']} 个 descriptor；测量窗口实际执行并"
        f"懒输出 {len(mix['descriptors'])} 个 `(mnemonic,size)` 变体，完整动态列表指后者。",
        f"vCPU task-clock 中映射到 TCG JIT 的比例为 "
        f"{_format_percent(perf['locations']['mapped_tcg_task_clock_ratio'])}；"
        f"per-TID final read 中未形成 overflow sample、因而不能定位到 epoch/IP 的尾部残量为 "
        f"{unlocated_tail:.0f} ns（{_format_percent(perf['locations']['unlocated_tail_task_clock_ratio'])}）；"
        "该残量不按比例伪分摊。"
        "耗时模型只使用落入 mix epoch 的已定位 sample period，并排除首尾 epoch；final-read 尾部残量"
        "和窗口外样本不进入回归。能由 TB/translation/duration nuisance 解释的成本单列，剩余误差"
        "记为 unattributed，指令成本是 QEMU-TCG epoch 级稳健回归边际估计。",
        f"mix/perf 窗口外 task-clock 占比为 "
        f"{_format_percent(perf['locations']['outside_window_task_clock_ratio'])}；"
        f"首/尾 epoch 时长分别为 {mix['epochs'][0]['duration_ns'] / 1e9:.6f}s / "
        f"{mix['epochs'][-1]['duration_ns'] / 1e9:.6f}s。",
        f"collector gate 相对 mix 检测的起/止偏差为 "
        f"{perf['gate_alignment']['start_skew_ns'] / 1e6:.3f}ms / "
        f"{perf['gate_alignment']['stop_skew_ns'] / 1e6:.3f}ms；每个 epoch 边界因 overflow "
        f"period 可能跨界的 task-clock 上界为 "
        f"{perf['gate_alignment']['sample_period_boundary_uncertainty_ns_per_epoch_boundary']:.0f}ns。"
        "首尾 epoch 不进入耗时权重拟合，但仍保留在精确计数和阶段分布中。",
        "",
        "## 分段结果",
        "",
        "参考分段使用 1 秒 epoch、Jeffreys 平滑的 CLR 组成特征，并加入吞吐率和内核占比；"
        "惩罚为有效维数乘以 log(n)。同时运行 1/2/5/10 秒桶与 0.8/1.0/1.2 倍惩罚，"
        "共 12 组敏感性检查。另以每次重新选择边界的 moving-block permutation 对单阶段零假设做"
        "选择校正全局检验；block 不是按 n^(1/3) 盲选，而是由不跨阶段边界的 ACF、稳健增量"
        "variogram 与等效 IAT 诊断，并在更长 block 上重复检验。",
        f"选择校正全局检验 p={segmentation['global_change_point_test']['p_value']:.6g}，"
        f"{'拒绝' if segmentation['global_change_point_test']['reject_single_segment'] else '未拒绝'}"
        "单阶段平稳零假设；"
        f"主/长 block={segmentation['serial_dependence']['primary_block_length']}/"
        f"{segmentation['serial_dependence']['long_block_length']} 秒，长 block p="
        f"{segmentation['global_change_point_block_sensitivity']['tests'][1]['p_value']:.6g}，"
        f"结论{'一致' if segmentation['global_change_point_block_sensitivity']['conclusions_agree'] else '不一致'}。",
        "",
        "| 阶段 | epoch | 相对时间（秒） | Cargo 进度 | 动态指令 | 内核占比 |",
        "|---:|---:|---:|---:|---:|---:|",
    ]
    for stage in segmentation["stages"]:
        lines.append(
            f"| {stage['stage']} | {stage['epoch_begin']}..{stage['epoch_end'] - 1} | "
            f"{stage['relative_start_seconds']:.3f}–{stage['relative_end_seconds']:.3f} | "
            f"{stage['cargo_progress']['start'] if stage['cargo_progress']['start'] is not None else '?'}"
            f"→{stage['cargo_progress']['end'] if stage['cargo_progress']['end'] is not None else '?'} / 446 | "
            f"{stage['instruction_count']:,} | "
            f"{_format_percent(stage['kernel_instruction_share'])} |"
        )
    lines.extend(
        [
            "",
            "## 指令耗时权重",
            "",
            "权重不是硬件单指令延迟，而是该次 QEMU TCG 运行中，以精确动态指令计数解释 vCPU "
            "task-clock 的非负分层 ridge + Huber 稳健边际均值。模型将 executed TB、translated TB "
            "和 translated instruction 作为 nuisance，稀疏/共线指令向实测 family 均值收缩。"
            "同 mnemonic 的 2/4 字节编码共享权重，随后按每个编码变体的精确计数回填。",
            f"模型质量为 `{weights['model']['quality']}`，拟合 R²={r_squared_text}，"
            f"blocked-CV=`{weights['model']['blocked_cv'].get('quality')}`，"
            f"权重 bootstrap 收敛="
            f"{weights['model']['bootstrap'].get('converged_replicates', 0)}/"
            f"{weights['model']['bootstrap'].get('replicates', 0)}；"
            f"ACF/IAT 诊断选择的模型 block="
            f"{weights['bootstrap_dependence']['model_block_length']} 秒；"
            f"指令项解释 task-clock 的 {_format_percent(fit['instruction_share_of_task_clock'])}，"
            f"unattributed={_format_percent(fit['unattributed_share_of_task_clock'])}，"
            f"overattributed={_format_percent(fit['overattributed_share_of_task_clock'])}。",
            "完整逐阶段结果见 `stage-weighted-instructions.csv`；下面列出各阶段权重占比最高的 10 项。",
            "",
        ]
    )
    for stage in weights["stage_distributions"]:
        lines.extend(
            [
                f"### 阶段 {stage['stage']}",
                "",
                "| 指令变体 | 带权占比 | 条件 95% CI | 权重区间包络 | ESS | Top-10 概率/稳健 |",
                "|---|---:|---:|---:|---:|---:|",
            ]
        )
        items = [
            item
            for item in stage["distribution"]["items"]
            if item["instruction"] != "OTHER"
        ][:10]
        for item in items:
            interval = item["confidence_interval"]
            envelope = item["weight_ci_share_envelope"]
            envelope_text = (
                f"{_format_percent(envelope[0])}–{_format_percent(envelope[1])}"
                if envelope is not None
                else "不可用"
            )
            lines.append(
                f"| `{item['instruction']}` | {_format_percent(item['share'])} | "
                f"{_format_percent(interval[0])}–{_format_percent(interval[1])} | "
                f"{envelope_text} | "
                f"{item['effective_sample_size']:.1f} | "
                f"{_format_percent(item['top_k_probability'])} / "
                f"{'是' if item['top10_robust_to_weight_ci'] else '否'} |"
            )
        lines.append("")
    lines.extend(
        [
            "## 统计置信度",
            "",
            "边界稳定性由分段残差 circular moving-block bootstrap 给出；全局 block permutation "
            "在每次置换后重新选择边界，用于修正 post-selection。相邻阶段使用同一依赖诊断 block 的 Jensen–Shannon/"
            "Holm 结果只是在给定已选边界后的条件性探索检验。带权占比的 moving-block bootstrap "
            "同样以点估计权重为条件；每阶段重新诊断 weighted-series ACF/IAT，并且不得短于权重模型 block。"
            "ESS 同时扣除了不等权和自相关；模型权重区间另作保守包络。",
            f"本次轨迹的高置信度判定：**{'通过' if confidence['high_confidence'] else '未通过'}**。",
            f"其中纯计数分段/分布={'通过' if confidence['count_distribution_high_confidence'] else '未通过'}，"
            f"耗时权重模型={'通过' if confidence['weight_model_high_confidence'] else '未通过'}，"
            f"阶段带权占比抽样稳定性={'通过' if confidence['weighted_distribution_sampling_high_confidence'] else '未通过'}。",
        ]
    )
    if confidence["reasons"]:
        lines.append("")
        for reason in confidence["reasons"]:
            lines.append(f"- {reason}")
    if confidence["conditional_warnings"]:
        lines.append("")
        for warning in confidence["conditional_warnings"]:
            lines.append(f"- 条件性提示：{warning}")
    lines.extend(
        [
            "",
            "这些统计量仅支持并量化这一次捕获轨迹内、给定采集与模型假设的条件稳定性，不构成"
            "数学证明或跨运行外推。跨冷启动、宿主负载和 QEMU 版本的稳定性，至少需要 3 次相互"
            "独立的冷运行后做分层/随机效应比较。",
            "",
            "## 产物索引",
            "",
            "- `instruction-list.csv`：完整 `(mnemonic, size)` 动态指令列表。",
            "- `epoch-timeline.csv` 与 `epoch-instruction-counts.csv`：分段前的完整时间序列。",
            "- `segmentation.json`：12 组敏感性、边界 bootstrap 与相邻段 permutation。",
            "- `native-ip-distribution.csv`：vCPU 线程中的 native QEMU IP 热点。",
            "- `weight-model.json` 与 `stage-weighted-distributions.json`：模型和完整阶段分布。",
            "- `quality.json`：机器可读的数据质量与置信度门槛。",
            "",
        ]
    )
    return "\n".join(lines)


def _discover_jitdump(run_dir: Path) -> Path:
    candidates = sorted(run_dir.glob("jit-*.dump"))
    _require(len(candidates) == 1, f"{run_dir} 中应恰有一个 jit-*.dump")
    return candidates[0]


def _arguments(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", type=Path, help="已完成的 BuildStorm profile 运行目录")
    parser.add_argument("--analysis-dir", type=Path)
    parser.add_argument("--no-resume", action="store_true", help="忽略已有原子缓存")
    parser.add_argument("--min-segment-seconds", type=int, default=20)
    parser.add_argument("--max-epoch-counter-skew-ratio", type=float, default=0.001)
    parser.add_argument("--max-window-counter-skew-ratio", type=float, default=0.0001)
    parser.add_argument("--boundary-bootstrap-replicates", type=int, default=200)
    parser.add_argument("--global-permutation-replicates", type=int, default=999)
    parser.add_argument("--permutation-replicates", type=int, default=999)
    parser.add_argument("--weight-bootstrap-replicates", type=int, default=200)
    parser.add_argument("--distribution-bootstrap-replicates", type=int, default=1000)
    parser.add_argument("--min-jit-sample-mapping-ratio", type=float, default=0.10)
    parser.add_argument(
        "--min-catalog-coverage-ratio",
        "--min-catalog-match-ratio",
        dest="min_catalog_coverage_ratio",
        type=float,
        default=0.9999,
        help="matched JIT loads / catalog records；旧 option 名保留兼容",
    )
    parser.add_argument("--seed", type=int, default=20260809)
    arguments = parser.parse_args(argv)
    for name in (
        "min_segment_seconds",
        "boundary_bootstrap_replicates",
        "global_permutation_replicates",
        "permutation_replicates",
        "distribution_bootstrap_replicates",
    ):
        _require(getattr(arguments, name) > 0, f"--{name.replace('_', '-')} 必须为正")
    _require(arguments.weight_bootstrap_replicates >= 0, "weight bootstrap 次数不能为负")
    for name in ("min_jit_sample_mapping_ratio", "min_catalog_coverage_ratio"):
        _require(0.0 <= getattr(arguments, name) <= 1.0, f"--{name.replace('_', '-')} 必须位于 0..1")
    for name in ("max_epoch_counter_skew_ratio", "max_window_counter_skew_ratio"):
        _require(0.0 <= getattr(arguments, name) <= 1.0, f"--{name.replace('_', '-')} 必须位于 0..1")
    return arguments


def main(argv: Sequence[str] | None = None) -> int:
    try:
        arguments = _arguments(argv)
        run_dir = arguments.run_dir.resolve()
        _require(run_dir.is_dir(), f"运行目录不存在：{run_dir}")
        analysis_dir = (
            arguments.analysis_dir.resolve()
            if arguments.analysis_dir
            else run_dir / "analysis"
        )
        analysis_dir.mkdir(parents=True, exist_ok=True)
        mix_path = run_dir / "instruction-mix.jsonl"
        catalog_path = run_dir / "instruction-catalog.jsonl"
        samples_path = run_dir / "tcg-time-samples.bin"
        tid_map_path = run_dir / "tid-namespace-map.tsv"
        jitdump_path = _discover_jitdump(run_dir)
        for path in (mix_path, catalog_path, samples_path, tid_map_path, jitdump_path):
            _require(path.is_file() and path.stat().st_size > 0, f"输入缺失或为空：{path}")
        resume = not arguments.no_resume

        mix_key = _cache_key(
            (mix_path,),
            {
                "kind": "strict-canonical-mix-v3",
                "max_epoch_counter_skew_ratio": arguments.max_epoch_counter_skew_ratio,
                "max_window_counter_skew_ratio": arguments.max_window_counter_skew_ratio,
            },
        )
        mix = _load_cache(analysis_dir / ".mix-cache.json", mix_key, resume)
        if mix is None:
            print("analysis: 解析并闭合 instruction mix", file=sys.stderr)
            mix = parse_instruction_mix(
                mix_path,
                max_epoch_snapshot_skew_ratio=arguments.max_epoch_counter_skew_ratio,
                max_window_snapshot_skew_ratio=arguments.max_window_counter_skew_ratio,
            )
            _write_cache(analysis_dir / ".mix-cache.json", mix_key, mix)
        else:
            print("analysis: 恢复 instruction mix 缓存", file=sys.stderr)
        _write_raw_outputs(analysis_dir, mix)

        perf_key = _cache_key(
            (mix_path, catalog_path, samples_path, tid_map_path, jitdump_path),
            {
                "kind": "time-aware-vcpu-jit-map-v2",
                "min_jit_sample_mapping_ratio": arguments.min_jit_sample_mapping_ratio,
                "min_catalog_coverage_ratio": arguments.min_catalog_coverage_ratio,
            },
        )
        perf = _load_cache(analysis_dir / ".perf-cache.json", perf_key, resume)
        if perf is None:
            print("analysis: 映射 vCPU task-clock -> JIT/native/unknown", file=sys.stderr)
            perf = map_perf_samples(
                mix,
                samples_path,
                jitdump_path,
                catalog_path,
                tid_map_path,
                min_jit_sample_mapping_ratio=arguments.min_jit_sample_mapping_ratio,
                min_catalog_coverage_ratio=arguments.min_catalog_coverage_ratio,
            )
            _write_cache(analysis_dir / ".perf-cache.json", perf_key, perf)
        else:
            print("analysis: 恢复 perf/JIT 映射缓存", file=sys.stderr)
        _write_perf_outputs(analysis_dir, perf)

        segmentation_parameters = {
            "min_segment_seconds": arguments.min_segment_seconds,
            "boundary_bootstrap_replicates": arguments.boundary_bootstrap_replicates,
            "global_permutation_replicates": arguments.global_permutation_replicates,
            "permutation_replicates": arguments.permutation_replicates,
            "seed": arguments.seed,
        }
        segmentation_key = _cache_key((mix_path,), segmentation_parameters)
        segmentation = _load_cache(
            analysis_dir / ".segmentation-cache.json", segmentation_key, resume
        )
        if segmentation is None:
            print("analysis: 运行参考分段、12 组敏感性与边界检验", file=sys.stderr)
            segmentation = analyze_segmentation(mix, **segmentation_parameters)
            _write_cache(
                analysis_dir / ".segmentation-cache.json", segmentation_key, segmentation
            )
        else:
            print("analysis: 恢复分段统计缓存", file=sys.stderr)
        progress = _load_progress_timeline(run_dir, mix)
        _annotate_stages(segmentation, progress)
        segmentation["progress_annotation"] = progress
        _atomic_write_json(analysis_dir / "segmentation.json", segmentation)
        _write_timeline(analysis_dir, mix, perf, segmentation)

        weight_parameters = {
            "weight_bootstrap_replicates": arguments.weight_bootstrap_replicates,
            "distribution_bootstrap_replicates": arguments.distribution_bootstrap_replicates,
            "block_length": segmentation["serial_dependence"][
                "primary_block_length"
            ],
            "seed": arguments.seed,
            "boundaries": segmentation["reference"]["boundaries"],
        }
        weight_key = _cache_key(
            (mix_path, catalog_path, samples_path, tid_map_path, jitdump_path),
            weight_parameters,
        )
        weights = _load_cache(analysis_dir / ".weight-cache.json", weight_key, resume)
        if weights is None:
            print("analysis: 拟合指令耗时并 bootstrap 各阶段完整分布", file=sys.stderr)
            weights = fit_weights(
                mix,
                perf,
                segmentation["reference"]["boundaries"],
                **{
                    key: weight_parameters[key]
                    for key in (
                        "weight_bootstrap_replicates",
                        "distribution_bootstrap_replicates",
                        "block_length",
                        "seed",
                    )
                },
            )
            _write_cache(analysis_dir / ".weight-cache.json", weight_key, weights)
        else:
            print("analysis: 恢复耗时模型缓存", file=sys.stderr)
        _write_weight_outputs(analysis_dir, mix, segmentation, weights)

        quality = _quality_summary(mix, perf, segmentation, weights)
        _atomic_write_json(analysis_dir / "quality.json", quality)
        _atomic_write_text(
            analysis_dir / "analysis-report.md",
            _build_report(run_dir, mix, perf, segmentation, weights, quality),
        )
        _atomic_write_json(
            analysis_dir / "analysis-state.json",
            {
                "schema": ANALYSIS_SCHEMA,
                "complete": True,
                "inputs": {
                    path.name: _fingerprint(path)
                    for path in (
                        mix_path,
                        catalog_path,
                        samples_path,
                        tid_map_path,
                        jitdump_path,
                    )
                },
                "outputs": sorted(
                    path.name
                    for path in analysis_dir.iterdir()
                    if path.is_file() and not path.name.startswith(".")
                ),
            },
        )
        print(
            f"analysis: 完成，阶段={len(segmentation['stages'])}，"
            f"轨迹内高置信度={quality['statistical_confidence']['high_confidence']}，"
            f"输出={analysis_dir}",
            file=sys.stderr,
        )
        return 0
    except (
        AnalysisError,
        OSError,
        ProfileIoError,
        StatisticsError,
        WeightModelError,
        ValueError,
    ) as error:
        print(f"analyze-riscv-buildstorm-instructions.py: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
