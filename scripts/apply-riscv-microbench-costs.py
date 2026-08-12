#!/usr/bin/env python3
"""Apply the independent RISC-V microbenchmark model to one BuildStorm run."""

from __future__ import annotations

import argparse
import collections
import csv
import functools
import io
import json
import math
import os
import re
import tempfile
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any

from riscv_instruction_encoding import decode_riscv64_instruction
from rv_instruction_profile_io import (
    PerfSample,
    RvTcgQuality,
    RvTcgTidStats,
    iter_rv_tcg_records,
    read_tid_namespace_tsv,
)


MIX_SCHEMA = "mygo.riscv-instruction-mix.v1"
CATALOG_SCHEMA = "mygo.riscv-tb-catalog.v1"
OUTPUT_SCHEMA = "mygo.riscv-buildstorm-microbench-costs.v1"
MODEL_KEY = "raw-encoding+semantic-decoding+execution-pattern"
VCPU_COMM = re.compile(r"CPU ([0-9]+)/TCG\Z")


class CostError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CostError(message)


def atomic_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def atomic_json(path: Path, value: Any) -> None:
    atomic_text(path, json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n")


def atomic_csv(path: Path, fields: Sequence[str], rows: Iterable[Mapping[str, Any]]) -> None:
    buffer = io.StringIO(newline="")
    writer = csv.DictWriter(buffer, fieldnames=fields, lineterminator="\n")
    writer.writeheader()
    writer.writerows(rows)
    atomic_text(path, buffer.getvalue())


def parse_mix(path: Path) -> dict[str, Any]:
    descriptors: dict[int, dict[str, Any]] = {}
    epochs: list[dict[str, Any]] = []
    window_start: int | None = None
    window_stop: int | None = None
    quality: dict[str, Any] | None = None
    header: dict[str, Any] | None = None
    previous_end: int | None = None
    expected_epoch = 1
    totals: collections.Counter[int] = collections.Counter()
    user_totals: collections.Counter[int] = collections.Counter()
    kernel_totals: collections.Counter[int] = collections.Counter()

    with path.open(encoding="utf-8") as stream:
        for number, line in enumerate(stream, 1):
            record = json.loads(line)
            require(record.get("schema") == MIX_SCHEMA, f"{path}:{number}: bad schema")
            kind = record.get("type")
            if kind == "header":
                require(header is None, "instruction mix has duplicate header")
                header = record
                require(record.get("target") == "riscv64", "instruction mix is not RISC-V64")
            elif kind == "descriptor":
                descriptor_id = int(record["id"])
                require(descriptor_id not in descriptors, f"duplicate descriptor {descriptor_id}")
                descriptors[descriptor_id] = {
                    "descriptor_id": descriptor_id,
                    "mnemonic": str(record["mnemonic"]).lower(),
                    "size_bytes": int(record["size"]),
                }
            elif kind == "window_start":
                require(window_start is None, "instruction mix has duplicate window_start")
                window_start = int(record["monotonic_ns"])
                previous_end = window_start
            elif kind == "sample":
                require(previous_end is not None, "sample precedes window_start")
                epoch_number = int(record["epoch"])
                require(epoch_number == expected_epoch, "instruction epochs are not contiguous")
                expected_epoch += 1
                end = int(record["monotonic_ns"])
                counts: dict[int, dict[str, int]] = {}
                for item in record.get("mix", []):
                    descriptor_id = int(item["id"])
                    user = int(item["user"])
                    kernel = int(item["kernel"])
                    require(user >= 0 and kernel >= 0, "negative instruction count")
                    counts[descriptor_id] = {"user": user, "kernel": kernel}
                    user_totals[descriptor_id] += user
                    kernel_totals[descriptor_id] += kernel
                    totals[descriptor_id] += user + kernel
                epochs.append(
                    {
                        "epoch": epoch_number,
                        "start_monotonic_ns": previous_end,
                        "end_monotonic_ns": end,
                        "duration_ns": end - previous_end,
                        "counts": counts,
                    }
                )
                previous_end = end
            elif kind == "window_stop":
                window_stop = int(record["monotonic_ns"])
            elif kind == "quality":
                quality = record
            else:
                raise CostError(f"{path}:{number}: unsupported record type {kind!r}")

    require(header is not None and window_start is not None, "instruction mix is incomplete")
    require(window_stop is not None and quality is not None, "instruction mix lacks final records")
    require(quality.get("complete") is True, "instruction mix final quality is incomplete")
    require(int(quality.get("windows", 0)) == 1, "instruction mix must contain one window")
    require(int(quality.get("samples", -1)) == len(epochs), "instruction sample count disagrees")
    for name, value in quality.get("errors", {}).items():
        require(int(value) == 0, f"instruction mix error {name}={value}")
    require(set(totals).issubset(descriptors), "dynamic counts reference unknown descriptors")
    total_count = sum(totals.values())
    quality_count = int(quality["instruction_delta"]["total"]) if "instruction_delta" in quality else None
    if quality_count is not None:
        require(total_count == quality_count, "instruction mix total does not close")
    return {
        "header": header,
        "window_start_monotonic_ns": window_start,
        "window_stop_monotonic_ns": window_stop,
        "quality": quality,
        "descriptors": descriptors,
        "epochs": epochs,
        "total_count": total_count,
        "totals": totals,
        "user_totals": user_totals,
        "kernel_totals": kernel_totals,
    }


@functools.lru_cache(maxsize=262_144)
def decode_encoding(size: int, raw_hex: str):
    decoded = decode_riscv64_instruction(bytes.fromhex(raw_hex), None)
    require(decoded.length == size, "catalog instruction size disagrees with encoding")
    return decoded


def parse_descriptor_semantics(
    path: Path, descriptor_ids: set[int]
) -> tuple[dict[tuple[int, str], set[str]], dict[str, Any], dict[str, Any]]:
    semantics: dict[tuple[int, str], set[str]] = collections.defaultdict(set)
    metadata: dict[str, Any] = {}
    header: dict[str, Any] | None = None
    quality: dict[str, Any] | None = None
    records = 0
    duplicate_records = 0
    descriptor_encodings: set[tuple[int, str, int, str]] = set()
    with path.open(encoding="utf-8") as stream:
        for number, line in enumerate(stream, 1):
            record = json.loads(line)
            require(record.get("schema") == CATALOG_SCHEMA, f"{path}:{number}: bad schema")
            kind = record.get("type")
            if kind == "header":
                require(header is None, "catalog has duplicate header")
                header = record
            elif kind == "quality":
                quality = record
            elif kind == "tb":
                records += 1
                if record.get("duplicate_exact") is True:
                    duplicate_records += 1
                domain = str(record.get("mode"))
                require(domain in {"user", "kernel"}, "catalog TB has invalid mode")
                for instruction in record.get("instructions", []):
                    descriptor_id = int(instruction["descriptor_id"])
                    if descriptor_id not in descriptor_ids:
                        continue
                    size = int(instruction["size"])
                    raw_hex = str(instruction["bytes"])
                    require(instruction.get("bytes_complete") is True, "catalog has incomplete bytes")
                    identity = (descriptor_id, domain, size, raw_hex)
                    if identity in descriptor_encodings:
                        continue
                    descriptor_encodings.add(identity)
                    decoded = decode_encoding(size, raw_hex)
                    semantics[(descriptor_id, domain)].add(decoded.key)
                    metadata.setdefault(decoded.key, decoded)
            else:
                raise CostError(f"{path}:{number}: unsupported record type {kind!r}")
    require(header is not None and quality is not None, "catalog lacks final quality")
    require(records == int(quality["records"]), "catalog record count does not close")
    for name in ("write_errors", "dropped_blocks", "tracking_drops"):
        require(int(quality.get(name, 0)) == 0, f"catalog {name} is nonzero")
    missing = descriptor_ids - {descriptor_id for descriptor_id, _ in semantics}
    require(not missing, f"catalog has no semantic encoding for descriptors {sorted(missing)}")
    return semantics, metadata, {
        "records": records,
        "duplicate_exact_records": duplicate_records,
        "unique_descriptor_domain_encodings": len(descriptor_encodings),
        "decoded_cache": decode_encoding.cache_info()._asdict(),
    }


def restricted_reason(decoded: Any) -> str | None:
    mnemonic = decoded.mnemonic
    extension = decoded.extension
    if extension == "priv" or mnemonic in {
        "mret", "sret", "uret", "wfi", "sfence.vma", "hfence.vvma", "hfence.gvma"
    }:
        return "privileged-context"
    if mnemonic in {"ecall", "ebreak", "c.ebreak"}:
        return "trap-context"
    if extension == "zicsr":
        return "csr-context"
    if extension in {"zicbom", "zicboz", "zicbop"} or mnemonic.startswith("cbo."):
        return "cache-block-context"
    if not decoded.recognized:
        return "unknown-encoding"
    return None


def load_model(path: Path) -> tuple[dict[str, list[dict[str, Any]]], dict[str, Any]]:
    model = json.loads(path.read_text(encoding="utf-8"))
    require(model.get("schema_version") == 2, "unsupported weight model schema")
    require(model.get("instruction_key") == MODEL_KEY, "unsupported instruction key")
    by_semantic: dict[str, list[dict[str, Any]]] = collections.defaultdict(list)
    for item in model.get("instructions", []):
        key = item.get("key", {})
        semantic = key.get("semantic_encoding_key")
        require(isinstance(semantic, str) and semantic, "model item lacks semantic key")
        value = item.get("ns_per_instruction")
        interval = item.get("simultaneous_ci")
        require(
            isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value),
            f"model {semantic} has invalid estimate",
        )
        require(
            isinstance(interval, list)
            and len(interval) == 2
            and all(isinstance(x, (int, float)) and math.isfinite(x) for x in interval),
            f"model {semantic} has invalid simultaneous interval",
        )
        by_semantic[semantic].append(item)
    return by_semantic, model


def descriptor_estimate(
    semantic_keys: set[str], metadata: Mapping[str, Any], model: Mapping[str, list[dict[str, Any]]]
) -> dict[str, Any]:
    missing = sorted(key for key in semantic_keys if key not in model)
    restrictions = sorted(
        {reason for key in semantic_keys if (reason := restricted_reason(metadata[key])) is not None}
    )
    contexts = [item for key in sorted(semantic_keys) for item in model.get(key, [])]
    if missing or not contexts or restrictions:
        diagnostic_values = [float(item["ns_per_instruction"]) for item in contexts]
        return {
            "assignment": "restricted" if restrictions else "unpriced",
            "quality": "restricted-context" if restrictions else "unmeasured",
            "point_ns": None,
            "low_ns": None,
            "high_ns": None,
            "diagnostic_context_center_ns": (
                math.fsum(diagnostic_values) / len(diagnostic_values)
                if diagnostic_values else None
            ),
            "bounded": False,
            "strict": False,
            "missing_semantic_keys": missing,
            "restrictions": restrictions,
            "context_count": len(contexts),
        }
    values = [float(item["ns_per_instruction"]) for item in contexts]
    lows = [max(0.0, float(item["simultaneous_ci"][0])) for item in contexts]
    highs = [max(0.0, float(item["simultaneous_ci"][1])) for item in contexts]
    qualities = {str(item.get("quality")) for item in contexts}
    unique_context = len(contexts) == 1 and len(semantic_keys) == 1
    strict = unique_context and qualities == {"high-confidence"}
    if unique_context:
        assignment = "single-context"
    else:
        assignment = "context-envelope"
    quality = "high-confidence" if strict else "exploratory"
    return {
        "assignment": assignment,
        "quality": quality,
        "point_ns": values[0] if unique_context else None,
        "low_ns": min(lows),
        "high_ns": max(highs),
        "diagnostic_context_center_ns": math.fsum(values) / len(values),
        "bounded": True,
        "strict": strict,
        "missing_semantic_keys": missing,
        "restrictions": restrictions,
        "context_count": len(contexts),
    }


def exact_vcpu_clock(samples_path: Path, tid_map_path: Path) -> dict[str, Any]:
    namespace = read_tid_namespace_tsv(tid_map_path)
    vcpus = sorted(
        (int(VCPU_COMM.fullmatch(entry.comm).group(1)), entry.host_tid)
        for entry in namespace.entries
        if VCPU_COMM.fullmatch(entry.comm)
    )
    require(vcpus and [index for index, _ in vcpus] == list(range(len(vcpus))), "vCPU TID map is incomplete")
    vcpu_tids = {tid for _, tid in vcpus}
    stats: dict[int, RvTcgTidStats] = {}
    sampled_period: collections.Counter[int] = collections.Counter()
    quality: RvTcgQuality | None = None
    last: Any = None
    for record in iter_rv_tcg_records(samples_path):
        last = record
        if isinstance(record, RvTcgTidStats):
            stats[record.tid] = record
        elif isinstance(record, PerfSample) and record.tid in vcpu_tids:
            sampled_period[record.tid] += record.period_ns
        elif isinstance(record, RvTcgQuality):
            quality = record
    require(isinstance(last, RvTcgQuality) and quality is not None, "collector lacks final quality")
    require(quality.status == 0 and quality.lost == 0, "collector quality is invalid")
    require(vcpu_tids.issubset(stats), "collector lacks vCPU final reads")
    exact = sum(stats[tid].task_clock_ns for tid in vcpu_tids)
    sampled = sum(sampled_period.values())
    require(exact >= sampled > 0, "collector vCPU task-clock does not close")
    return {
        "vcpu_count": len(vcpus),
        "exact_vcpu_task_clock_ns": exact,
        "sampled_vcpu_task_clock_ns": sampled,
        "unlocated_tail_task_clock_ns": exact - sampled,
        "collector_gate_active_ns": quality.gate_active_ns,
        "collector_running_ratio_ppm": quality.running_ratio_ppm,
    }


def load_stages(path: Path | None, epoch_count: int) -> list[dict[str, int]]:
    if path is None or not path.is_file():
        return [{"stage": 0, "epoch_begin": 0, "epoch_end_exclusive": epoch_count}]
    with path.open(newline="", encoding="utf-8") as stream:
        rows = list(csv.DictReader(stream))
    stages = [
        {
            "stage": int(row["stage"]),
            "epoch_begin": int(row["epoch_begin"]),
            "epoch_end_exclusive": int(row["epoch_end_exclusive"]),
        }
        for row in rows
    ]
    require(stages and stages[0]["epoch_begin"] == 0, "stages do not start at epoch zero")
    require(stages[-1]["epoch_end_exclusive"] == epoch_count, "stages do not cover all epochs")
    for left, right in zip(stages, stages[1:]):
        require(left["epoch_end_exclusive"] == right["epoch_begin"], "stages are not contiguous")
    return stages


def aggregate_costs(
    counts: Mapping[int, Mapping[str, int]],
    estimates: Mapping[tuple[int, str], Mapping[str, Any]],
) -> dict[str, Any]:
    total = sum(item["user"] + item["kernel"] for item in counts.values())
    identified = bounded = strict_count = restricted_count = 0
    point_terms: list[float] = []
    low_terms: list[float] = []
    high_terms: list[float] = []
    center_terms: list[float] = []
    strict_terms: list[float] = []
    for descriptor_id, domains in counts.items():
        for domain in ("user", "kernel"):
            count = domains[domain]
            if count == 0:
                continue
            estimate = estimates.get((descriptor_id, domain))
            require(estimate is not None, f"descriptor {descriptor_id}/{domain} lacks semantics")
            if estimate["quality"] == "restricted-context":
                restricted_count += count
            if estimate["bounded"]:
                bounded += count
                low_terms.append(count * estimate["low_ns"])
                high_terms.append(count * estimate["high_ns"])
                center_terms.append(count * estimate["diagnostic_context_center_ns"])
            if estimate["point_ns"] is not None:
                identified += count
                point_terms.append(count * estimate["point_ns"])
            if estimate["strict"]:
                strict_count += count
                strict_terms.append(count * estimate["point_ns"])
    return {
        "instruction_count": total,
        "identified_point_instruction_count": identified,
        "identified_point_instruction_ratio": identified / total if total else 0.0,
        "bounded_instruction_count": bounded,
        "bounded_instruction_ratio": bounded / total if total else 0.0,
        "unpriced_instruction_count": total - bounded,
        "unpriced_instruction_ratio": (total - bounded) / total if total else 0.0,
        "restricted_instruction_count": restricted_count,
        "restricted_instruction_ratio": restricted_count / total if total else 0.0,
        "strict_instruction_count": strict_count,
        "strict_instruction_ratio": strict_count / total if total else 0.0,
        "identified_point_cost_ns": math.fsum(point_terms),
        "bounded_cost_envelope_low_ns": math.fsum(low_terms),
        "bounded_cost_envelope_high_ns": math.fsum(high_terms),
        "diagnostic_context_center_cost_ns": math.fsum(center_terms),
        "strict_point_cost_ns": math.fsum(strict_terms),
    }


def descriptor_cost_row(
    *,
    domain: str,
    domain_count: int,
    descriptor_total_count: int,
    total_count: int,
    descriptor: Mapping[str, Any],
    semantic_keys: set[str],
    estimate: Mapping[str, Any],
) -> dict[str, Any]:
    point = estimate["point_ns"]
    bounded = bool(estimate["bounded"])
    return {
        **descriptor,
        "domain": domain,
        "domain_count": domain_count,
        "descriptor_total_count": descriptor_total_count,
        "instruction_share": domain_count / total_count if total_count else 0.0,
        "semantic_key_count": len(semantic_keys),
        "semantic_keys": ";".join(sorted(semantic_keys)),
        "context_count": estimate["context_count"],
        "assignment": estimate["assignment"],
        "quality": estimate["quality"],
        "restrictions": ";".join(estimate["restrictions"]),
        "missing_semantic_keys": ";".join(estimate["missing_semantic_keys"]),
        "identified_weight_ns": point,
        "weight_envelope_low_ns": estimate["low_ns"],
        "weight_envelope_high_ns": estimate["high_ns"],
        "diagnostic_context_center_ns": estimate["diagnostic_context_center_ns"],
        "identified_cost_ns": domain_count * point if point is not None else None,
        "bounded_cost_low_ns": domain_count * estimate["low_ns"] if bounded else None,
        "bounded_cost_high_ns": domain_count * estimate["high_ns"] if bounded else None,
        "diagnostic_context_center_cost_ns": (
            domain_count * estimate["diagnostic_context_center_ns"]
            if bounded and estimate["diagnostic_context_center_ns"] is not None
            else None
        ),
        "bounded": bounded,
        "strict": estimate["strict"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--weights", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--stages", type=Path)
    arguments = parser.parse_args(argv)
    run_dir = arguments.run_dir.resolve()
    output_dir = (arguments.output_dir or run_dir / "microbench-costs").resolve()
    mix = parse_mix(run_dir / "instruction-mix.jsonl")
    model_by_semantic, model_metadata = load_model(arguments.weights.resolve())
    semantics, semantic_metadata, catalog_stats = parse_descriptor_semantics(
        run_dir / "instruction-catalog.jsonl", set(mix["totals"])
    )
    estimates = {
        descriptor_domain: descriptor_estimate(keys, semantic_metadata, model_by_semantic)
        for descriptor_domain, keys in semantics.items()
    }

    descriptor_rows: list[dict[str, Any]] = []
    for descriptor_id, count in mix["totals"].most_common():
        descriptor = mix["descriptors"][descriptor_id]
        for domain, domain_count in (
            ("user", mix["user_totals"][descriptor_id]),
            ("kernel", mix["kernel_totals"][descriptor_id]),
        ):
            if domain_count == 0:
                continue
            estimate = estimates[(descriptor_id, domain)]
            descriptor_rows.append(
                descriptor_cost_row(
                    domain=domain,
                    domain_count=domain_count,
                    descriptor_total_count=count,
                    total_count=mix["total_count"],
                    descriptor=descriptor,
                    semantic_keys=semantics[(descriptor_id, domain)],
                    estimate=estimate,
                )
            )
    descriptor_rows.sort(
        key=lambda row: (
            row["bounded_cost_high_ns"] is not None,
            row["bounded_cost_high_ns"] or 0.0,
            row["domain_count"],
        ),
        reverse=True,
    )

    epoch_rows: list[dict[str, Any]] = []
    epoch_counts: list[dict[int, dict[str, int]]] = []
    for epoch in mix["epochs"]:
        counts = {
            descriptor_id: domains
            for descriptor_id, domains in epoch["counts"].items()
            if domains["user"] + domains["kernel"] > 0
        }
        epoch_counts.append(counts)
        aggregate = aggregate_costs(counts, estimates)
        epoch_rows.append(
            {key: value for key, value in epoch.items() if key != "counts"}
            | aggregate
        )

    stages_path = arguments.stages or run_dir / "analysis" / "stages.csv"
    stages = load_stages(stages_path, len(epoch_rows))
    stage_rows: list[dict[str, Any]] = []
    stage_instruction_rows: list[dict[str, Any]] = []
    for stage in stages:
        merged: dict[int, dict[str, int]] = collections.defaultdict(
            lambda: {"user": 0, "kernel": 0}
        )
        for counts in epoch_counts[stage["epoch_begin"] : stage["epoch_end_exclusive"]]:
            for descriptor_id, domains in counts.items():
                merged[descriptor_id]["user"] += domains["user"]
                merged[descriptor_id]["kernel"] += domains["kernel"]
        stage_aggregate = aggregate_costs(merged, estimates)
        stage_rows.append(stage | stage_aggregate)
        for descriptor_id, domains in merged.items():
            descriptor = mix["descriptors"][descriptor_id]
            descriptor_total_count = domains["user"] + domains["kernel"]
            for domain in ("user", "kernel"):
                domain_count = domains[domain]
                if domain_count == 0:
                    continue
                row = descriptor_cost_row(
                    domain=domain,
                    domain_count=domain_count,
                    descriptor_total_count=descriptor_total_count,
                    total_count=stage_aggregate["instruction_count"],
                    descriptor=descriptor,
                    semantic_keys=semantics[(descriptor_id, domain)],
                    estimate=estimates[(descriptor_id, domain)],
                )
                stage_instruction_rows.append(
                    {
                        **stage,
                        "stage_instruction_count": stage_aggregate["instruction_count"],
                        "stage_instruction_share": row.pop("instruction_share"),
                        **row,
                    }
                )
    stage_instruction_rows.sort(
        key=lambda row: (
            row["stage"],
            -(row["bounded_cost_high_ns"] or 0.0),
            -row["domain_count"],
        )
    )

    whole_counts = {
        descriptor_id: {
            "user": mix["user_totals"][descriptor_id],
            "kernel": mix["kernel_totals"][descriptor_id],
        }
        for descriptor_id in mix["totals"]
    }
    aggregate = aggregate_costs(whole_counts, estimates)
    clock = exact_vcpu_clock(
        run_dir / "tcg-time-samples.bin", run_dir / "tid-namespace-map.tsv"
    )
    summary_path = run_dir / "summary.json"
    run_summary = (
        json.loads(summary_path.read_text(encoding="utf-8"))
        if summary_path.is_file()
        else None
    )
    exact_clock = clock["exact_vcpu_task_clock_ns"]
    comparison = {
        "strict_cost_to_exact_vcpu_ratio": aggregate["strict_point_cost_ns"] / exact_clock,
        "identified_point_to_exact_vcpu_ratio": aggregate["identified_point_cost_ns"] / exact_clock,
        "bounded_low_to_exact_vcpu_ratio": aggregate["bounded_cost_envelope_low_ns"] / exact_clock,
        "bounded_high_to_exact_vcpu_ratio": aggregate["bounded_cost_envelope_high_ns"] / exact_clock,
        "diagnostic_context_center_to_exact_vcpu_ratio": (
            aggregate["diagnostic_context_center_cost_ns"] / exact_clock
        ),
        "exact_vcpu_minus_identified_point_ns": exact_clock - aggregate["identified_point_cost_ns"],
    }
    if run_summary is not None:
        qemu_cpu_ns = float(run_summary["host"]["qemu_cpu_seconds"]) * 1_000_000_000
        comparison.update(
            {
                "wall_elapsed_ms": float(run_summary["timing"]["elapsed_ms"]),
                "qemu_process_cpu_ns": qemu_cpu_ns,
                "qemu_process_minus_vcpu_cpu_ns": qemu_cpu_ns - exact_clock,
                "vcpu_share_of_qemu_process_cpu": exact_clock / qemu_cpu_ns,
            }
        )

    result = {
        "schema": OUTPUT_SCHEMA,
        "run_dir": str(run_dir),
        "weights": str(arguments.weights.resolve()),
        "scope": {
            "response": model_metadata["primary_response"],
            "model": model_metadata["model"],
            "confidence": model_metadata["confidence"],
            "interpretation": "QEMU TCG marginal CPU-time cost under measured execution patterns, not hardware latency",
            "catalog_occurrences_used_as_dynamic_counts": False,
            "context_point_policy": "point estimates require one semantic execution context; unresolved contexts have only a simultaneous min/max envelope",
            "diagnostic_center_is_not_an_identified_estimate": True,
        },
        "configuration": {
            "configured_vcpus": mix["header"]["configured_vcpus"],
            "window_start_monotonic_ns": mix["window_start_monotonic_ns"],
            "window_stop_monotonic_ns": mix["window_stop_monotonic_ns"],
            "epochs": len(mix["epochs"]),
        },
        "catalog": catalog_stats,
        "aggregate": aggregate,
        "clock": clock,
        "comparison": comparison,
        "quality_counts": dict(collections.Counter(row["quality"] for row in descriptor_rows)),
        "assignment_counts": dict(collections.Counter(row["assignment"] for row in descriptor_rows)),
        "outputs": {
            "instruction_costs": "instruction-costs.csv",
            "epoch_costs": "epoch-costs.csv",
            "stage_costs": "stage-costs.csv",
            "stage_instruction_costs": "stage-instruction-costs.csv",
        },
    }
    atomic_csv(output_dir / "instruction-costs.csv", list(descriptor_rows[0]), descriptor_rows)
    atomic_csv(output_dir / "epoch-costs.csv", list(epoch_rows[0]), epoch_rows)
    atomic_csv(output_dir / "stage-costs.csv", list(stage_rows[0]), stage_rows)
    atomic_csv(
        output_dir / "stage-instruction-costs.csv",
        list(stage_instruction_rows[0]),
        stage_instruction_rows,
    )
    atomic_json(output_dir / "summary.json", result)
    print(
        f"microbench costs: instructions={aggregate['instruction_count']:,} "
        f"bounded={aggregate['bounded_instruction_ratio']:.3%} "
        f"identified={aggregate['identified_point_cost_ns'] / 1e9:.6f}s "
        f"vcpu={exact_clock / 1e9:.6f}s output={output_dir}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (CostError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"microbench costs: {error}", file=os.sys.stderr)
        raise SystemExit(1)
