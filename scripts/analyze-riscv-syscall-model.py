#!/usr/bin/env python3
"""Price RISC-V syscall-path instruction models with the calibrated TCG model."""

from __future__ import annotations

import argparse
import collections
import csv
import importlib.util
import json
import math
import re
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from types import ModuleType
from typing import Any


INPUT_SCHEMA = "mygo.riscv-syscall-model.v1"
OUTPUT_SCHEMA = "mygo.riscv-syscall-model-costs.v1"
HEX_BYTES = re.compile(r"(?:[0-9a-fA-F]{2})+")
UINT64_MAX = (1 << 64) - 1

MODEL_FIELDS = [
    "nr",
    "entries",
    "exits",
    "blocks",
    "instruction_count",
    "descriptor_instruction_count",
    "unattributed_instruction_count",
    "bounded_instruction_count",
    "bounded_instruction_ratio",
    "unpriced_instruction_count",
    "unpriced_instruction_ratio",
    "restricted_instruction_count",
    "restricted_instruction_ratio",
    "strict_instruction_count",
    "strict_instruction_ratio",
    "model_cost_center_ns",
    "model_cost_low_ns",
    "model_cost_high_ns",
    "strict_point_cost_ns",
    "per_entry_cost_center_ns",
    "per_entry_cost_low_ns",
    "per_entry_cost_high_ns",
    "per_exit_cost_center_ns",
    "per_exit_cost_low_ns",
    "per_exit_cost_high_ns",
]

RUNTIME_FIELDS = [
    "nr",
    "name",
    "runtime_calls",
    "runtime_completed",
    "runtime_inflight",
    "runtime_success",
    "runtime_errors",
    "runtime_call_share",
    "runtime_priced_instances",
    "model_available",
    "model_denominator",
    "model_observations",
    "model_bounded_instruction_ratio",
    "per_call_cost_center_ns",
    "per_call_cost_low_ns",
    "per_call_cost_high_ns",
    "runtime_model_cost_center_ns",
    "runtime_model_cost_low_ns",
    "runtime_model_cost_high_ns",
    "runtime_model_cost_share",
    "runtime_model_cost_share_low",
    "runtime_model_cost_share_high",
]


class AnalysisError(RuntimeError):
    """The model or runtime input cannot support a sound analysis."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AnalysisError(message)


def load_cost_module() -> ModuleType:
    script = Path(__file__).resolve().with_name("apply-riscv-microbench-costs.py")
    script_dir = str(script.parent)
    if script_dir not in sys.path:
        sys.path.insert(0, script_dir)
    spec = importlib.util.spec_from_file_location("_mygo_riscv_microbench_costs", script)
    require(spec is not None and spec.loader is not None, f"cannot load {script}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


COST_MODEL = load_cost_module()


def json_uint(value: Any, label: str) -> int:
    require(
        isinstance(value, int) and not isinstance(value, bool),
        f"{label} must be an integer",
    )
    require(0 <= value <= UINT64_MAX, f"{label} is outside uint64")
    return value


def text_uint(value: str | None, label: str, *, base_zero: bool = False) -> int:
    require(value is not None and value.strip() != "", f"{label} is empty")
    raw = value.strip()
    try:
        parsed = int(raw, 0 if base_zero and raw.lower().startswith("0x") else 10)
    except ValueError as error:
        raise AnalysisError(f"{label} is not an integer: {raw!r}") from error
    require(0 <= parsed <= UINT64_MAX, f"{label} is outside uint64")
    return parsed


def normalize_encoding(value: Any, label: str, size: int) -> str:
    if isinstance(value, Mapping):
        require(set(value) >= {"bytes"}, f"{label} lacks bytes")
        value = value["bytes"]
    require(isinstance(value, str), f"{label} must be a hex string")
    require(HEX_BYTES.fullmatch(value) is not None, f"{label} is not compact hex bytes")
    normalized = value.lower()
    require(len(normalized) == size * 2, f"{label} length disagrees with size={size}")
    return normalized


def parse_descriptor_counts(value: Any, label: str) -> dict[int, int]:
    result: dict[int, int] = {}
    if isinstance(value, Mapping):
        items: list[tuple[Any, Any]] = list(value.items())
        for raw_id, raw_count in items:
            descriptor_id = text_uint(str(raw_id), f"{label} descriptor id")
            require(descriptor_id not in result, f"{label} duplicates descriptor {descriptor_id}")
            result[descriptor_id] = json_uint(raw_count, f"{label}[{descriptor_id}]")
        return result

    require(isinstance(value, list), f"{label} must be a list or object")
    for index, item in enumerate(value):
        require(isinstance(item, Mapping), f"{label}[{index}] must be an object")
        require("id" in item and "count" in item, f"{label}[{index}] lacks id/count")
        descriptor_id = json_uint(item["id"], f"{label}[{index}].id")
        require(descriptor_id not in result, f"{label} duplicates descriptor {descriptor_id}")
        result[descriptor_id] = json_uint(item["count"], f"{label}[{index}].count")
    return result


def parse_plugin_document(document: Any, *, source: str = "syscall model") -> dict[str, Any]:
    require(isinstance(document, Mapping), f"{source} root must be an object")
    require(document.get("schema") == INPUT_SCHEMA, f"{source} has unsupported schema")
    require(document.get("target") == "riscv64", f"{source} is not RISC-V64")

    raw_descriptors = document.get("descriptors")
    require(isinstance(raw_descriptors, list), f"{source}.descriptors must be a list")
    descriptors: dict[int, dict[str, Any]] = {}
    for index, item in enumerate(raw_descriptors):
        label = f"{source}.descriptors[{index}]"
        require(isinstance(item, Mapping), f"{label} must be an object")
        descriptor_id = json_uint(item.get("id"), f"{label}.id")
        require(descriptor_id not in descriptors, f"{source} duplicates descriptor {descriptor_id}")
        mnemonic = item.get("mnemonic")
        require(isinstance(mnemonic, str) and mnemonic.strip(), f"{label}.mnemonic is invalid")
        size = json_uint(item.get("size"), f"{label}.size")
        require(size > 0, f"{label}.size must be positive")
        raw_encodings = item.get("encodings")
        require(isinstance(raw_encodings, list), f"{label}.encodings must be a list")
        encodings = [
            normalize_encoding(value, f"{label}.encodings[{encoding_index}]", size)
            for encoding_index, value in enumerate(raw_encodings)
        ]
        require(len(encodings) == len(set(encodings)), f"{label} has duplicate encodings")
        descriptors[descriptor_id] = {
            "id": descriptor_id,
            "mnemonic": mnemonic.strip().lower(),
            "size": size,
            "encodings": encodings,
        }

    raw_syscalls = document.get("syscalls")
    require(isinstance(raw_syscalls, list), f"{source}.syscalls must be a list")
    syscalls: dict[int, dict[str, Any]] = {}
    for index, item in enumerate(raw_syscalls):
        label = f"{source}.syscalls[{index}]"
        require(isinstance(item, Mapping), f"{label} must be an object")
        nr = json_uint(item.get("nr"), f"{label}.nr")
        require(nr not in syscalls, f"{source} duplicates syscall {nr}")
        counts = parse_descriptor_counts(item.get("descriptor_counts"), f"{label}.descriptor_counts")
        unknown = set(counts) - set(descriptors)
        require(not unknown, f"{label} references unknown descriptors {sorted(unknown)}")
        descriptor_count = sum(counts.values())
        instructions = (
            json_uint(item["instructions"], f"{label}.instructions")
            if "instructions" in item
            else descriptor_count
        )
        require(
            descriptor_count <= instructions,
            f"{label} descriptor counts exceed its instruction total",
        )
        unattributed = json_uint(
            item.get("unattributed_instructions", 0),
            f"{label}.unattributed_instructions",
        )
        require(
            descriptor_count + unattributed <= instructions,
            f"{label} accounted instructions exceed its instruction total",
        )
        syscalls[nr] = {
            "nr": nr,
            "entries": json_uint(item.get("entries"), f"{label}.entries"),
            "exits": json_uint(item.get("exits"), f"{label}.exits"),
            "blocks": json_uint(item.get("blocks", 0), f"{label}.blocks"),
            "instructions": instructions,
            "unattributed_instructions": unattributed,
            "descriptor_counts": counts,
            "descriptor_count_sum": descriptor_count,
        }

    aggregate = {
        "entries": sum(row["entries"] for row in syscalls.values()),
        "exits": sum(row["exits"] for row in syscalls.values()),
        "blocks": sum(row["blocks"] for row in syscalls.values()),
        "instructions": sum(row["instructions"] for row in syscalls.values()),
        "descriptor_count_sum": sum(
            row["descriptor_count_sum"] for row in syscalls.values()
        ),
        "unattributed_instructions": sum(
            row["unattributed_instructions"] for row in syscalls.values()
        ),
    }
    totals = document.get("totals")
    if totals is not None:
        require(isinstance(totals, Mapping), f"{source}.totals must be an object")
        for field, actual in aggregate.items():
            if field in totals:
                declared = json_uint(totals[field], f"{source}.totals.{field}")
                require(declared == actual, f"{source}.totals.{field} does not close")

    closure = document.get("closure")
    require(isinstance(closure, Mapping), f"{source}.closure must be an object")
    expected_closure = {
        "entry_exit_delta": aggregate["entries"] - aggregate["exits"],
        "instructions_minus_accounted": (
            aggregate["instructions"]
            - aggregate["descriptor_count_sum"]
            - aggregate["unattributed_instructions"]
        ),
    }
    for field, actual in expected_closure.items():
        require(field in closure, f"{source}.closure lacks {field}")
        declared = closure[field]
        require(
            isinstance(declared, int) and not isinstance(declared, bool),
            f"{source}.closure.{field} must be an integer",
        )
        require(declared == actual, f"{source}.closure.{field} does not close")
    require(isinstance(closure.get("closed"), bool), f"{source}.closure.closed must be boolean")

    if "vcpus" in document:
        require(json_uint(document["vcpus"], f"{source}.vcpus") > 0, f"{source}.vcpus must be positive")
    if "config" in document:
        require(isinstance(document["config"], Mapping), f"{source}.config must be an object")

    return {
        "source": source,
        "descriptors": descriptors,
        "syscalls": syscalls,
        "aggregate": aggregate,
        "vcpus": document.get("vcpus"),
        "config": document.get("config"),
        "closure": closure,
        "overflow": document.get("overflow"),
        "errors": document.get("errors"),
    }


def parse_plugin(path: Path) -> dict[str, Any]:
    return parse_plugin_document(
        json.loads(path.read_text(encoding="utf-8")), source=str(path)
    )


def counters_are_zero(value: Any) -> bool:
    if value is None:
        return True
    if isinstance(value, Mapping):
        return all(counters_are_zero(item) for item in value.values())
    if isinstance(value, bool):
        return not value
    if isinstance(value, (int, float)) and math.isfinite(value):
        return value == 0
    return False


def descriptor_estimates(
    plugin: Mapping[str, Any], weights_path: Path
) -> tuple[dict[tuple[int, str], Mapping[str, Any]], dict[str, Any]]:
    by_semantic, model_metadata = COST_MODEL.load_model(weights_path)
    estimates: dict[tuple[int, str], Mapping[str, Any]] = {}
    semantic_metadata: dict[str, Any] = {}
    semantics: dict[int, set[str]] = {}
    for descriptor_id, descriptor in plugin["descriptors"].items():
        keys: set[str] = set()
        for raw_hex in descriptor["encodings"]:
            decoded = COST_MODEL.decode_encoding(descriptor["size"], raw_hex)
            require(
                decoded.length == descriptor["size"],
                f"descriptor {descriptor_id} decoded length does not close",
            )
            keys.add(decoded.key)
            semantic_metadata.setdefault(decoded.key, decoded)
        semantics[descriptor_id] = keys
        estimates[(descriptor_id, "kernel")] = COST_MODEL.descriptor_estimate(
            keys, semantic_metadata, by_semantic
        )
    return estimates, {
        "metadata": model_metadata,
        "semantics": semantics,
    }


def divide(value: float, count: int) -> float | None:
    return value / count if count else None


def analyze_syscalls(
    plugin: Mapping[str, Any], estimates: Mapping[tuple[int, str], Mapping[str, Any]]
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for nr, syscall in plugin["syscalls"].items():
        counts = {
            descriptor_id: {"user": 0, "kernel": count}
            for descriptor_id, count in syscall["descriptor_counts"].items()
        }
        aggregate = COST_MODEL.aggregate_costs(counts, estimates)
        instruction_count = syscall["instructions"]
        bounded = aggregate["bounded_instruction_count"]
        restricted = aggregate["restricted_instruction_count"]
        strict = aggregate["strict_instruction_count"]
        center = aggregate["diagnostic_context_center_cost_ns"]
        low = aggregate["bounded_cost_envelope_low_ns"]
        high = aggregate["bounded_cost_envelope_high_ns"]
        row = {
            "nr": nr,
            "entries": syscall["entries"],
            "exits": syscall["exits"],
            "blocks": syscall["blocks"],
            "instruction_count": instruction_count,
            "descriptor_instruction_count": aggregate["instruction_count"],
            "unattributed_instruction_count": (
                instruction_count - aggregate["instruction_count"]
            ),
            "bounded_instruction_count": bounded,
            "bounded_instruction_ratio": bounded / instruction_count if instruction_count else 0.0,
            "unpriced_instruction_count": instruction_count - bounded,
            "unpriced_instruction_ratio": (
                (instruction_count - bounded) / instruction_count if instruction_count else 0.0
            ),
            "restricted_instruction_count": restricted,
            "restricted_instruction_ratio": (
                restricted / instruction_count if instruction_count else 0.0
            ),
            "strict_instruction_count": strict,
            "strict_instruction_ratio": strict / instruction_count if instruction_count else 0.0,
            "model_cost_center_ns": center,
            "model_cost_low_ns": low,
            "model_cost_high_ns": high,
            "strict_point_cost_ns": aggregate["strict_point_cost_ns"],
            "per_entry_cost_center_ns": divide(center, syscall["entries"]),
            "per_entry_cost_low_ns": divide(low, syscall["entries"]),
            "per_entry_cost_high_ns": divide(high, syscall["entries"]),
            "per_exit_cost_center_ns": divide(center, syscall["exits"]),
            "per_exit_cost_low_ns": divide(low, syscall["exits"]),
            "per_exit_cost_high_ns": divide(high, syscall["exits"]),
        }
        rows.append(row)
    rows.sort(key=lambda row: (-row["model_cost_center_ns"], row["nr"]))
    return rows


def parse_runtime_syscalls(path: Path) -> list[dict[str, Any]]:
    aggregate: dict[int, dict[str, Any]] = {}
    with path.open(newline="", encoding="utf-8-sig") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        require(reader.fieldnames is not None, f"{path} lacks a TSV header")
        require(len(reader.fieldnames) == len(set(reader.fieldnames)), f"{path} has duplicate columns")
        require({"nr", "calls"}.issubset(reader.fieldnames), f"{path} lacks nr/calls columns")
        for line, raw in enumerate(reader, 2):
            if not any((value or "").strip() for value in raw.values()):
                continue
            nr = text_uint(raw.get("nr"), f"{path}:{line}:nr", base_zero=True)
            calls = text_uint(raw.get("calls"), f"{path}:{line}:calls")
            name = (raw.get("name") or "").strip()
            completed = text_uint(raw.get("completed"), f"{path}:{line}:completed") if "completed" in reader.fieldnames else calls
            inflight = text_uint(raw.get("inflight"), f"{path}:{line}:inflight") if "inflight" in reader.fieldnames else calls - completed
            success = text_uint(raw.get("success"), f"{path}:{line}:success") if "success" in reader.fieldnames else completed
            errors = text_uint(raw.get("errors"), f"{path}:{line}:errors") if "errors" in reader.fieldnames else 0
            require(inflight >= 0, f"{path}:{line}: completed exceeds calls")
            require(completed + inflight == calls, f"{path}:{line}: calls do not equal completed+inflight")
            require(success + errors == completed, f"{path}:{line}: completed does not equal success+errors")
            row = aggregate.setdefault(
                nr,
                {"nr": nr, "calls": 0, "completed": 0, "inflight": 0,
                 "success": 0, "errors": 0, "names": set(), "rows": 0},
            )
            for field, value in (("calls", calls), ("completed", completed),
                                 ("inflight", inflight), ("success", success),
                                 ("errors", errors)):
                row[field] += value
                require(row[field] <= UINT64_MAX, f"{path}: syscall {nr} {field} overflow uint64")
            row["rows"] += 1
            if name:
                row["names"].add(name)
    rows: list[dict[str, Any]] = []
    for nr, row in aggregate.items():
        require(len(row["names"]) <= 1, f"{path}: syscall {nr} has conflicting names")
        rows.append(
            {
                "nr": nr,
                "calls": row["calls"],
                "completed": row["completed"],
                "inflight": row["inflight"],
                "success": row["success"],
                "errors": row["errors"],
                "name": next(iter(row["names"]), f"syscall_{nr}"),
                "source_rows": row["rows"],
            }
        )
    rows.sort(key=lambda row: row["nr"])
    return rows


def analyze_runtime(
    runtime: Sequence[Mapping[str, Any]],
    model_rows: Sequence[Mapping[str, Any]],
    *,
    denominator: str,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    require(denominator in {"entry", "exit"}, "runtime denominator must be entry or exit")
    models = {int(row["nr"]): row for row in model_rows}
    total_calls = sum(int(row["calls"]) for row in runtime)
    total_completed = sum(int(row.get("completed", row["calls"])) for row in runtime)
    total_inflight = sum(int(row.get("inflight", 0)) for row in runtime)
    rows: list[dict[str, Any]] = []
    prefix = f"per_{denominator}"
    observations_field = "entries" if denominator == "entry" else "exits"
    for item in runtime:
        nr = int(item["nr"])
        calls = int(item["calls"])
        completed = int(item.get("completed", calls))
        inflight = int(item.get("inflight", calls - completed))
        success = int(item.get("success", completed))
        errors = int(item.get("errors", 0))
        priced_instances = calls if denominator == "entry" else completed
        model = models.get(nr)
        available = model is not None and int(model[observations_field]) > 0
        center = model[f"{prefix}_cost_center_ns"] if available else None
        low = model[f"{prefix}_cost_low_ns"] if available else None
        high = model[f"{prefix}_cost_high_ns"] if available else None
        rows.append(
            {
                "nr": nr,
                "name": item["name"],
                "runtime_calls": calls,
                "runtime_completed": completed,
                "runtime_inflight": inflight,
                "runtime_success": success,
                "runtime_errors": errors,
                "runtime_call_share": calls / total_calls if total_calls else 0.0,
                "runtime_priced_instances": priced_instances,
                "model_available": available,
                "model_denominator": denominator,
                "model_observations": model[observations_field] if model is not None else None,
                "model_bounded_instruction_ratio": (
                    model["bounded_instruction_ratio"] if model is not None else None
                ),
                "per_call_cost_center_ns": center,
                "per_call_cost_low_ns": low,
                "per_call_cost_high_ns": high,
                "runtime_model_cost_center_ns": priced_instances * center if center is not None else None,
                "runtime_model_cost_low_ns": priced_instances * low if low is not None else None,
                "runtime_model_cost_high_ns": priced_instances * high if high is not None else None,
                "runtime_model_cost_share": None,
                "runtime_model_cost_share_low": None,
                "runtime_model_cost_share_high": None,
            }
        )

    priced = [row for row in rows if row["runtime_model_cost_center_ns"] is not None]
    total_center = math.fsum(row["runtime_model_cost_center_ns"] for row in priced)
    total_low = math.fsum(row["runtime_model_cost_low_ns"] for row in priced)
    total_high = math.fsum(row["runtime_model_cost_high_ns"] for row in priced)
    for row in priced:
        own_center = row["runtime_model_cost_center_ns"]
        own_low = row["runtime_model_cost_low_ns"]
        own_high = row["runtime_model_cost_high_ns"]
        row["runtime_model_cost_share"] = own_center / total_center if total_center else None
        low_denominator = own_low + total_high - own_high
        high_denominator = own_high + total_low - own_low
        row["runtime_model_cost_share_low"] = (
            own_low / low_denominator if low_denominator > 0.0 else None
        )
        row["runtime_model_cost_share_high"] = (
            own_high / high_denominator if high_denominator > 0.0 else None
        )

    rows.sort(
        key=lambda row: (
            row["runtime_model_cost_center_ns"] is None,
            -(row["runtime_model_cost_center_ns"] or 0.0),
            row["nr"],
        )
    )
    matched_calls = sum(row["runtime_calls"] for row in priced)
    priced_instances = sum(row["runtime_priced_instances"] for row in priced)
    return rows, {
        "denominator": denominator,
        "runtime_call_count": total_calls,
        "runtime_completed_count": total_completed,
        "runtime_inflight_count": total_inflight,
        "priced_instance_count": priced_instances,
        "unpriced_inflight_count": total_inflight if denominator == "exit" else 0,
        "model_available_call_count": matched_calls,
        "model_available_call_ratio": matched_calls / total_calls if total_calls else 0.0,
        "unmodeled_call_count": total_calls - matched_calls,
        "model_cost_center_ns": total_center,
        "model_cost_low_ns": total_low,
        "model_cost_high_ns": total_high,
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("model", type=Path, help="QEMU syscall model JSON")
    parser.add_argument("--weights", type=Path, required=True)
    parser.add_argument("--runtime-syscalls", "--runtime", dest="runtime", type=Path)
    parser.add_argument("--runtime-denominator", choices=("entry", "exit"), default="exit")
    parser.add_argument(
        "--allow-dirty-capture",
        action="store_true",
        help="price a capture with truncated instances or diagnostic errors",
    )
    parser.add_argument("--output-dir", type=Path)
    arguments = parser.parse_args(argv)

    model_path = arguments.model.resolve()
    weights_path = arguments.weights.resolve()
    output_dir = (
        arguments.output_dir.resolve()
        if arguments.output_dir is not None
        else model_path.parent / "syscall-model-costs"
    )
    plugin = parse_plugin(model_path)
    estimates, model = descriptor_estimates(plugin, weights_path)
    model_rows = analyze_syscalls(plugin, estimates)

    total_instructions = plugin["aggregate"]["instructions"]
    total_bounded = sum(row["bounded_instruction_count"] for row in model_rows)
    total_center = math.fsum(row["model_cost_center_ns"] for row in model_rows)
    total_low = math.fsum(row["model_cost_low_ns"] for row in model_rows)
    total_high = math.fsum(row["model_cost_high_ns"] for row in model_rows)
    closure = plugin["closure"]
    capture_clean = (
        closure.get("closed") is True
        and counters_are_zero(plugin["overflow"])
        and counters_are_zero(plugin["errors"])
    )
    require(
        capture_clean or arguments.allow_dirty_capture,
        "capture is not clean; inspect closure/errors/overflow or pass --allow-dirty-capture",
    )
    summary: dict[str, Any] = {
        "schema": OUTPUT_SCHEMA,
        "model": str(model_path),
        "weights": str(weights_path),
        "scope": {
            "response": model["metadata"].get("primary_response"),
            "weight_model": model["metadata"].get("model"),
            "confidence": model["metadata"].get("confidence"),
            "interpretation": "QEMU TCG marginal CPU-time cost for the bounded kernel instruction subset",
            "restricted_and_unmeasured_instructions_are_unpriced": True,
            "diagnostic_center_is_not_an_identified_point_estimate": True,
        },
        "capture": {
            "vcpus": plugin["vcpus"],
            "config": plugin["config"],
            "closure": closure,
            "overflow": plugin["overflow"],
            "errors": plugin["errors"],
            "clean": capture_clean,
        },
        "aggregate": {
            **plugin["aggregate"],
            "syscall_count": len(model_rows),
            "bounded_instruction_count": total_bounded,
            "bounded_instruction_ratio": total_bounded / total_instructions if total_instructions else 0.0,
            "unpriced_instruction_count": total_instructions - total_bounded,
            "unpriced_instruction_ratio": (
                (total_instructions - total_bounded) / total_instructions
                if total_instructions
                else 0.0
            ),
            "model_cost_center_ns": total_center,
            "model_cost_low_ns": total_low,
            "model_cost_high_ns": total_high,
        },
        "outputs": {"model_costs": "syscall-model-costs.csv"},
    }

    COST_MODEL.atomic_csv(output_dir / "syscall-model-costs.csv", MODEL_FIELDS, model_rows)
    if arguments.runtime is not None:
        runtime_rows, runtime_summary = analyze_runtime(
            parse_runtime_syscalls(arguments.runtime.resolve()),
            model_rows,
            denominator=arguments.runtime_denominator,
        )
        COST_MODEL.atomic_csv(
            output_dir / "syscall-runtime-costs.csv", RUNTIME_FIELDS, runtime_rows
        )
        summary["runtime"] = {
            "source": str(arguments.runtime.resolve()),
            **runtime_summary,
        }
        summary["outputs"]["runtime_costs"] = "syscall-runtime-costs.csv"
    COST_MODEL.atomic_json(output_dir / "summary.json", summary)
    print(
        f"syscall model costs: syscalls={len(model_rows)} "
        f"instructions={total_instructions:,} "
        f"bounded={summary['aggregate']['bounded_instruction_ratio']:.3%} "
        f"center={total_center / 1e9:.9f}s output={output_dir}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (
        AnalysisError,
        COST_MODEL.CostError,
        OSError,
        ValueError,
        json.JSONDecodeError,
    ) as error:
        print(f"syscall model costs: {error}", file=sys.stderr)
        raise SystemExit(1)
