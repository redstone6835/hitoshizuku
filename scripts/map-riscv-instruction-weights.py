#!/usr/bin/env python3
"""把微基准权重映射到 catalog 中出现过的每个规范化 RISC-V encoding。"""

from __future__ import annotations

import argparse
import csv
import functools
import hashlib
import json
import math
from collections import defaultdict
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from riscv_instruction_encoding import decode_riscv64_instruction
from riscv_weight_model_seal import ModelSealError, verify_model_document_seal
from riscv_weight_provenance import (
    ProvenanceError,
    discover_provenance_root,
    verify_finalized_model,
)


class MappingError(ValueError):
    pass


CATALOG_SCHEMA = "mygo.riscv-tb-catalog.v1"
CATALOG_TARGET = "riscv64"
MODEL_SCHEMA_VERSION = 3
MODEL_INSTRUCTION_KEY = (
    "raw-encoding+semantic-decoding+execution-pattern"
)
REQUIRED_PUBLICATION_COMPONENTS = frozenset(
    {
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
    }
)
RAW_EXACT_LIMIT = 256
RAW_EXAMPLE_LIMIT = 16
HLL_BITS = 10
HLL_SIZE = 1 << HLL_BITS


class _RawEncodingSummary:
    """以有限内存保留样例，并给出可审计的 distinct 编码计数。"""

    def __init__(self) -> None:
        self.occurrences = 0
        self.exact: set[str] | None = set()
        self.examples: set[str] = set()
        self.registers = [0] * HLL_SIZE

    def add(self, size: int, raw_hex: str) -> None:
        identity = f"{size}:{raw_hex}"
        self.occurrences += 1
        if self.exact is not None:
            self.exact.add(identity)
            if len(self.exact) > RAW_EXACT_LIMIT:
                self.exact = None
        self.examples.add(raw_hex)
        if len(self.examples) > RAW_EXAMPLE_LIMIT:
            self.examples.remove(max(self.examples))
        digest = hashlib.blake2b(identity.encode(), digest_size=8).digest()
        value = int.from_bytes(digest, "little")
        index = value & (HLL_SIZE - 1)
        remaining = value >> HLL_BITS
        width = 64 - HLL_BITS
        rank = width + 1 if remaining == 0 else width - remaining.bit_length() + 1
        self.registers[index] = max(self.registers[index], rank)

    def distinct_count(self) -> tuple[int, bool, float | None]:
        if self.exact is not None:
            return len(self.exact), True, None
        alpha = 0.7213 / (1.0 + 1.079 / HLL_SIZE)
        estimate = alpha * HLL_SIZE * HLL_SIZE / math.fsum(
            2.0 ** (-register) for register in self.registers
        )
        zeroes = self.registers.count(0)
        if zeroes:
            estimate = HLL_SIZE * math.log(HLL_SIZE / zeroes)
        relative_standard_error = 1.04 / math.sqrt(HLL_SIZE)
        return max(1, round(estimate)), False, relative_standard_error


@functools.lru_cache(maxsize=65536)
def _decode_catalog_encoding(size: int, raw_hex: str):
    decoded = decode_riscv64_instruction(bytes.fromhex(raw_hex), None)
    if decoded.length != size:
        raise MappingError("指令声明长度与编码长度不一致")
    return decoded


def _catalog_integer(
    row: Mapping[str, Any], name: str, owner: str, *, minimum: int = 0
) -> int:
    value = row.get(name)
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise MappingError(f"{owner}.{name} 必须是大于等于 {minimum} 的整数")
    return value


def load_catalog(
    path: Path, *, expected_key_count: int | None = None
) -> dict[str, dict[str, Any]]:
    if expected_key_count is not None and (
        isinstance(expected_key_count, bool)
        or not isinstance(expected_key_count, int)
        or expected_key_count <= 0
    ):
        raise MappingError("expected_key_count 必须是正整数")
    rows: dict[str, dict[str, Any]] = {}
    header: dict[str, Any] | None = None
    quality: dict[str, Any] | None = None
    tb_records = 0
    with path.open(encoding="utf-8") as stream:
        for line_number, raw_line in enumerate(stream, 1):
            if not raw_line.strip():
                continue
            try:
                record = json.loads(raw_line)
            except json.JSONDecodeError as error:
                raise MappingError(f"{path}:{line_number}: 非法 JSON") from error
            if not isinstance(record, dict):
                raise MappingError(f"{path}:{line_number}: 记录必须是 object")
            owner = f"{path}:{line_number}"
            if record.get("schema") != CATALOG_SCHEMA:
                raise MappingError(
                    f"{owner}: schema={record.get('schema')!r}，期望 {CATALOG_SCHEMA!r}"
                )
            record_type = record.get("type")
            if header is None:
                if record_type != "header":
                    raise MappingError(f"{owner}: catalog 首条记录必须是 header")
                if record.get("target") != CATALOG_TARGET:
                    raise MappingError(
                        f"{owner}: target={record.get('target')!r}，期望 {CATALOG_TARGET!r}"
                    )
                header = record
                continue
            if quality is not None:
                raise MappingError(f"{owner}: final quality 之后仍有记录")
            if record_type == "header":
                raise MappingError(f"{owner}: header 重复")
            if record_type == "quality":
                quality = record
                continue
            if record_type != "tb":
                raise MappingError(f"{owner}: 未知 catalog type={record_type!r}")
            tb_records += 1
            for field in ("descriptor_overflow", "decode_errors"):
                if _catalog_integer(record, field, owner) != 0:
                    raise MappingError(f"{owner}: {field} 必须为零")
            instructions = record.get("instructions")
            if not isinstance(instructions, list):
                raise MappingError(f"{owner}: tb 缺少 instructions")
            if _catalog_integer(record, "instruction_count", owner) != len(
                instructions
            ):
                raise MappingError(f"{owner}: instruction_count 不闭合")
            for instruction in instructions:
                if not isinstance(instruction, dict):
                    raise MappingError(f"{owner}: instruction 必须是 object")
                if instruction.get("bytes_complete") is not True:
                    raise MappingError(f"{owner}: 存在不完整指令字节")
                size = instruction.get("size")
                raw_hex = instruction.get("bytes")
                qemu_name = instruction.get("mnemonic")
                if (
                    not isinstance(size, int)
                    or size not in {2, 4}
                    or not isinstance(raw_hex, str)
                    or len(raw_hex) != size * 2
                    or any(
                        character not in "0123456789abcdef"
                        for character in raw_hex
                    )
                    or not isinstance(qemu_name, str)
                    or not qemu_name
                ):
                    raise MappingError(f"{owner}: 指令字段非法")
                decoded = _decode_catalog_encoding(size, raw_hex)
                row = rows.setdefault(
                    decoded.key,
                    {
                        "encoding_key": decoded.key,
                        "canonical_mnemonic": decoded.mnemonic,
                        "extension": decoded.extension,
                        "size": decoded.length,
                        "recognized": decoded.recognized,
                        "modifiers": list(decoded.modifiers),
                        "raw_summary": _RawEncodingSummary(),
                        "qemu_mnemonics": set(),
                    },
                )
                row["raw_summary"].add(size, raw_hex)
                row["qemu_mnemonics"].add(qemu_name)
    if header is None:
        raise MappingError(f"{path}: 缺少 header")
    if quality is None:
        raise MappingError(f"{path}: 缺少 final quality，catalog 可能被截断")
    quality_owner = f"{path}:quality"
    records = _catalog_integer(quality, "records", quality_owner, minimum=1)
    translated = _catalog_integer(
        quality, "translated_blocks", quality_owner, minimum=1
    )
    if records != tb_records or translated != tb_records:
        raise MappingError(
            f"{quality_owner}: records/translated_blocks={records}/{translated} "
            f"与实际 tb={tb_records} 不闭合"
        )
    for field in ("write_errors", "dropped_blocks", "tracking_drops"):
        if _catalog_integer(quality, field, quality_owner) != 0:
            raise MappingError(f"{quality_owner}: {field} 必须为零")
    if not rows:
        raise MappingError(f"{path}: catalog 中没有完整指令编码")
    if expected_key_count is not None and len(rows) != expected_key_count:
        raise MappingError(
            f"{path}: catalog 规范 key 数 {len(rows)} != 期望 {expected_key_count}"
        )
    for row in rows.values():
        summary = row.pop("raw_summary")
        distinct, exact, relative_error = summary.distinct_count()
        row["raw_encoding_count"] = distinct
        row["raw_encoding_count_exact"] = exact
        row["raw_encoding_count_relative_standard_error"] = relative_error
        row["raw_encoding_occurrences"] = summary.occurrences
        row["raw_encodings"] = set(summary.examples)
        row["raw_encodings_truncated"] = (
            not exact or distinct > len(summary.examples)
        )
    return rows


def _restricted_reason(row: Mapping[str, Any]) -> str | None:
    mnemonic = row["canonical_mnemonic"]
    extension = row["extension"]
    if extension == "priv" or mnemonic in {
        "mret",
        "sret",
        "uret",
        "wfi",
        "sfence.vma",
        "hfence.vvma",
        "hfence.gvma",
    }:
        return "requires-privileged-context-probe"
    if mnemonic in {"ecall", "ebreak", "c.ebreak"}:
        return "trap-path-is-context-dependent"
    if extension == "zicsr":
        return "csr-is-not-safe-or-identifiable-in-user-mode"
    if extension in {"zicbom", "zicboz", "zicbop"} or mnemonic.startswith(
        "cbo."
    ):
        return "cache-block-operation-is-context-dependent"
    if not row["recognized"]:
        return "unknown-or-reserved-encoding"
    return None


def _missing_reason(row: Mapping[str, Any]) -> str:
    restricted = _restricted_reason(row)
    if restricted is not None:
        return restricted
    return "safe-probe-coverage-missing"


def _context_sort_key(item: Mapping[str, Any]) -> str:
    return json.dumps(
        item,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=True,
    )


def map_weights(
    catalog: Mapping[str, dict[str, Any]], model: Mapping[str, Any]
) -> dict[str, Any]:
    if model.get("schema_version") != MODEL_SCHEMA_VERSION:
        raise MappingError(
            "权重模型 schema_version="
            f"{model.get('schema_version')!r}，期望 {MODEL_SCHEMA_VERSION}"
        )
    if model.get("instruction_key") != MODEL_INSTRUCTION_KEY:
        raise MappingError(
            "权重模型 instruction_key="
            f"{model.get('instruction_key')!r}，期望 {MODEL_INSTRUCTION_KEY!r}"
        )
    publication_gate = model.get("publication_gate")
    if not isinstance(publication_gate, Mapping):
        raise MappingError("权重模型缺少显式 publication_gate")
    # 未 finalized 的模型仍可用于输出诊断性的未分配 catalog；只有声称
    # publication gate 已通过的模型必须携带完整内容封印。
    if publication_gate.get("passed") is True:
        try:
            verify_model_document_seal(model)
        except ModelSealError as error:
            raise MappingError(str(error)) from error
    components = publication_gate.get("components")
    host_audit = model.get("host_isolation_audit")
    host_binding = model.get("host_isolation_audit_binding")
    ml_validation = model.get("ml_validation")
    ml_conclusion = (
        ml_validation.get("conclusion")
        if isinstance(ml_validation, Mapping)
        else None
    )
    ml_evidence = model.get("ml_validation_evidence")
    ml_evidence_checks = (
        ml_evidence.get("checks")
        if isinstance(ml_evidence, Mapping)
        else None
    )
    ml_binding_checks = (
        ml_evidence.get("binding_checks")
        if isinstance(ml_evidence, Mapping)
        else None
    )
    publication_allowed = (
        publication_gate.get("passed") is True
        and isinstance(components, Mapping)
        and all(
            components.get(name) is True
            for name in REQUIRED_PUBLICATION_COMPONENTS
        )
        and isinstance(host_audit, Mapping)
        and host_audit.get("schema") == "mygo.riscv-weight-host-audit.v1"
        and host_audit.get("status") == "accepted"
        and model.get("host_isolation_audit_source") == "current"
        and isinstance(host_binding, Mapping)
        and host_binding.get("schema")
        == "mygo.riscv-weight-host-audit-binding.v1"
        and host_binding.get("source") == "current"
        and host_binding.get("publication_allowed") is True
        and isinstance(ml_validation, Mapping)
        and ml_validation.get("schema")
        == "mygo.riscv-instruction-ml-validation.v3"
        and isinstance(ml_conclusion, Mapping)
        and ml_conclusion.get("status") == "supported"
        and ml_conclusion.get("high_confidence_status") == "supported"
        and ml_conclusion.get("high_confidence_gate_passed") is True
        and ml_conclusion.get("may_publish_weights") is False
        and isinstance(ml_evidence, Mapping)
        and ml_evidence.get("schema")
        == "mygo.riscv-instruction-ml-validation.v3"
        and isinstance(ml_evidence_checks, Mapping)
        and bool(ml_evidence_checks)
        and all(value is True for value in ml_evidence_checks.values())
        and isinstance(ml_binding_checks, Mapping)
        and set(ml_binding_checks)
        == {"samples", "statistical_weights_pre_finalization"}
        and all(value is True for value in ml_binding_checks.values())
    )
    instructions = model.get("instructions")
    if not isinstance(instructions, list):
        raise MappingError("权重模型缺少 instructions")
    by_encoding: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for item in instructions:
        if not isinstance(item, dict) or not isinstance(item.get("key"), dict):
            raise MappingError("权重 instructions 项非法")
        encoding_key = item["key"].get(
            "semantic_encoding_key", item["key"].get("encoding_key")
        )
        if not isinstance(encoding_key, str) or not encoding_key:
            raise MappingError("权重项缺少 encoding_key")
        if item.get("calibration_only") is True:
            continue
        adjusted = item.get("anchor_adjusted")
        adjusted_value = (
            adjusted.get("ns_per_instruction")
            if isinstance(adjusted, Mapping)
            else None
        )
        published_value = item.get("published_ns_per_instruction")
        if published_value is not None and (
            isinstance(published_value, bool)
            or not isinstance(published_value, (int, float))
            or not math.isfinite(float(published_value))
            or isinstance(adjusted_value, bool)
            or not isinstance(adjusted_value, (int, float))
            or not math.isfinite(float(adjusted_value))
            or float(published_value) != float(adjusted_value)
        ):
            raise MappingError(
                f"权重项 {encoding_key!r} 的 published 与 anchor-adjusted 不一致"
            )
        by_encoding[encoding_key].append(item)

    catalog_keys = set(catalog)
    model_keys = set(by_encoding)
    orphan_model_keys = sorted(model_keys - catalog_keys)
    output: list[dict[str, Any]] = []
    status_counts: dict[str, int] = defaultdict(int)
    for encoding_key, source in sorted(catalog.items()):
        contexts = sorted(
            by_encoding.get(encoding_key, []), key=_context_sort_key
        )
        numeric = [
            item
            for item in contexts
            if not isinstance(item.get("published_ns_per_instruction"), bool)
            and isinstance(
                item.get("published_ns_per_instruction"), (int, float)
            )
            and math.isfinite(float(item["published_ns_per_instruction"]))
            and float(item["published_ns_per_instruction"]) >= 0
        ]
        acceptable = [
            item
            for item in numeric
            if publication_allowed and item.get("quality") == "high-confidence"
        ]
        assigned: float | None = None
        assignment = "unassigned"
        restricted = _restricted_reason(source)
        if restricted is not None:
            assignment = restricted
        elif len(acceptable) == 1 and len(contexts) == 1:
            assigned = float(acceptable[0]["published_ns_per_instruction"])
            assignment = "semantic-class-transfer-from-one-raw-context"
        elif acceptable and len(acceptable) == len(contexts):
            values = [
                float(item["published_ns_per_instruction"])
                for item in acceptable
            ]
            center = sorted(values)[len(values) // 2]
            tolerance = max(0.05, abs(center) * 0.15)
            if max(values) - min(values) <= tolerance:
                assigned = math.fsum(values) / len(values)
                assignment = "equivalent-context-semantic-class-mean"
            else:
                assignment = "context-dependent"
        elif contexts:
            assignment = (
                "model-publication-gate-failed"
                if not publication_allowed
                else "measured-but-confidence-gates-failed"
            )
        else:
            assignment = _missing_reason(source)
        measured_estimate: float | None = None
        if restricted is not None:
            estimate_quality = "restricted-context"
        elif assigned is not None:
            measured_estimate = assigned
            estimate_quality = "high-confidence"
        elif len(contexts) == 1:
            context_quality = contexts[0].get("quality")
            estimate_quality = (
                context_quality
                if isinstance(context_quality, str) and context_quality
                else "quality-unavailable"
            )
            adjusted = contexts[0].get("anchor_adjusted")
            exploratory = (
                adjusted.get("ns_per_instruction")
                if isinstance(adjusted, Mapping)
                else None
            )
            if (
                context_quality == "low-confidence"
                and not isinstance(exploratory, bool)
                and isinstance(exploratory, (int, float))
                and math.isfinite(float(exploratory))
            ):
                measured_estimate = float(exploratory)
        elif len(contexts) > 1:
            estimate_quality = "context-dependent"
        elif contexts:
            estimate_quality = "estimate-unavailable"
        else:
            estimate_quality = "unmeasured"
        status_counts[assignment] += 1
        output.append(
            {
                "encoding_key": encoding_key,
                "canonical_mnemonic": source["canonical_mnemonic"],
                "extension": source["extension"],
                "size": source["size"],
                "modifiers": source["modifiers"],
                "raw_encoding_count": source.get(
                    "raw_encoding_count", len(source["raw_encodings"])
                ),
                "raw_encoding_count_exact": source.get(
                    "raw_encoding_count_exact", True
                ),
                "raw_encoding_count_relative_standard_error": source.get(
                    "raw_encoding_count_relative_standard_error"
                ),
                "raw_encoding_occurrences": source.get(
                    "raw_encoding_occurrences"
                ),
                "raw_encodings": sorted(source["raw_encodings"]),
                "raw_encodings_truncated": source.get(
                    "raw_encodings_truncated", False
                ),
                "qemu_mnemonics": sorted(source["qemu_mnemonics"]),
                "assigned_ns_per_instruction": assigned,
                "assignment": assignment,
                "measured_estimate_ns_per_instruction": measured_estimate,
                "estimate_quality": estimate_quality,
                "restricted_contexts_ignored": (
                    len(contexts) if restricted is not None else 0
                ),
                "contexts": [
                    {
                        "pattern": item["key"].get("pattern"),
                        "raw_encoding_key": item["key"].get("encoding_key"),
                        "ns_per_instruction": (
                            item.get("anchor_adjusted", {}).get(
                                "ns_per_instruction"
                            )
                            if isinstance(item.get("anchor_adjusted"), Mapping)
                            else None
                        ),
                        "relative_weight": item.get("relative_weight"),
                        "simultaneous_ci": (
                            item.get("anchor_adjusted", {}).get(
                                "simultaneous_ci"
                            )
                            if isinstance(item.get("anchor_adjusted"), Mapping)
                            else None
                        ),
                        "raw_diagnostic_ns_per_instruction": item.get(
                            "ns_per_instruction"
                        ),
                        "raw_diagnostic_simultaneous_ci": item.get(
                            "simultaneous_ci"
                        ),
                        "quality": item.get("quality"),
                        "quality_failures": item.get("quality_failures", []),
                    }
                    for item in contexts
                ],
            }
        )
    result = {
        "schema": "mygo.riscv-instruction-catalog-weights.v3",
        "model_schema_version": model.get("schema_version"),
        "model_publication_gate": publication_gate,
        "catalog_encoding_count": len(output),
        "catalog_key_semantics": "decoded-semantic-class",
        "assignment_scope": (
            "representative raw probes transferred only within one decoded "
            "semantic class; context-dependent classes remain unassigned"
        ),
        "model_encoding_count": len(model_keys),
        "mapped_model_encoding_count": len(model_keys & catalog_keys),
        "orphan_model_encoding_count": len(orphan_model_keys),
        "orphan_model_encoding_keys": orphan_model_keys,
        "status_counts": dict(sorted(status_counts.items())),
        "instructions": output,
    }
    json.dumps(result, allow_nan=False)
    return result


def write_csv(result: Mapping[str, Any], path: Path) -> None:
    fields = [
        "encoding_key",
        "canonical_mnemonic",
        "extension",
        "size",
        "raw_encoding_count",
        "raw_encoding_count_exact",
        "qemu_mnemonics",
        "assigned_ns_per_instruction",
        "assignment",
        "measured_estimate_ns_per_instruction",
        "estimate_quality",
        "context_count",
        "context_patterns",
        "context_qualities",
    ]
    with path.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields)
        writer.writeheader()
        for item in result["instructions"]:
            contexts = item["contexts"]
            writer.writerow(
                {
                    "encoding_key": item["encoding_key"],
                    "canonical_mnemonic": item["canonical_mnemonic"],
                    "extension": item["extension"],
                    "size": item["size"],
                    "raw_encoding_count": item["raw_encoding_count"],
                    "raw_encoding_count_exact": item[
                        "raw_encoding_count_exact"
                    ],
                    "qemu_mnemonics": ";".join(item["qemu_mnemonics"]),
                    "assigned_ns_per_instruction": item["assigned_ns_per_instruction"],
                    "assignment": item["assignment"],
                    "measured_estimate_ns_per_instruction": item[
                        "measured_estimate_ns_per_instruction"
                    ],
                    "estimate_quality": item["estimate_quality"],
                    "context_count": len(contexts),
                    "context_patterns": ";".join(str(row["pattern"]) for row in contexts),
                    "context_qualities": ";".join(str(row["quality"]) for row in contexts),
                }
            )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", required=True)
    parser.add_argument("--weights", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--csv")
    parser.add_argument(
        "--provenance-root",
        help="final weights acquisition root；省略时从 manifest 路径唯一推导",
    )
    parser.add_argument("--expected-key-count", type=int)
    arguments = parser.parse_args(argv)
    if (
        arguments.expected_key_count is not None
        and arguments.expected_key_count <= 0
    ):
        parser.error("--expected-key-count 必须是正整数")
    weights_path = Path(arguments.weights).resolve()
    model = json.loads(weights_path.read_text(encoding="utf-8"))
    if not isinstance(model, dict):
        raise MappingError("权重模型根必须是 object")
    if model.get("publication_gate", {}).get("passed") is True:
        try:
            provenance_root = (
                Path(arguments.provenance_root).resolve()
                if arguments.provenance_root
                else discover_provenance_root(weights_path)
            )
            verify_finalized_model(weights_path, root=provenance_root)
        except ProvenanceError as error:
            raise MappingError(str(error)) from error
    result = map_weights(
        load_catalog(
            Path(arguments.catalog),
            expected_key_count=arguments.expected_key_count,
        ),
        model,
    )
    Path(arguments.output).write_text(
        json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    if arguments.csv:
        write_csv(result, Path(arguments.csv))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
