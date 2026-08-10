#!/usr/bin/env python3
"""按显式给定的基线与处理对象计算基准差值。"""

from __future__ import annotations

import argparse
import csv
from pathlib import Path


REQUIRED_COLUMNS = {
    "system",
    "workload",
    "mode",
    "state",
    "complete",
    "valid_samples",
    "failures",
    "median_ns",
}

OUTPUT_COLUMNS = [
    "workload",
    "mode",
    "comparison",
    "baseline",
    "treatment",
    "state",
    "baseline_median_ns",
    "treatment_median_ns",
    "delta_ns",
    "delta_percent",
]


def parse_pair(value: str) -> tuple[str, str, str]:
    name, separator, systems = value.partition("=")
    baseline, comma, treatment = systems.partition(",")
    if (
        separator != "="
        or comma != ","
        or not name
        or not baseline
        or not treatment
        or "," in treatment
        or baseline == treatment
    ):
        raise ValueError(f"比较项格式无效: {value}")
    return name, baseline, treatment


def read_summary(path: Path) -> dict[tuple[str, str, str], dict[str, str]]:
    with path.open("r", encoding="utf-8", newline="") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        missing = REQUIRED_COLUMNS - set(reader.fieldnames or ())
        if missing:
            raise ValueError(f"{path}: 缺少列 {','.join(sorted(missing))}")
        rows = list(reader)

    indexed = {}
    for line_number, row in enumerate(rows, start=2):
        system = row["system"].strip()
        workload = row["workload"].strip()
        mode = row["mode"].strip()
        if not system or not workload or not mode:
            raise ValueError(f"{path}:{line_number}: system/workload/mode 不能为空")
        if row["state"] != "READY" or row["complete"] != "1" or row["failures"] != "0":
            raise ValueError(f"{path}:{line_number}: 汇总未完成")
        for field in ("valid_samples", "median_ns"):
            try:
                value = int(row[field])
            except ValueError as error:
                raise ValueError(f"{path}:{line_number}: {field} 不是整数") from error
            if value <= 0:
                raise ValueError(f"{path}:{line_number}: {field} 必须为正数")
        key = (system, workload, mode)
        if key in indexed:
            raise ValueError(f"{path}:{line_number}: 重复汇总 {key}")
        indexed[key] = row
    if not indexed:
        raise ValueError(f"{path}: 没有汇总记录")
    return indexed


def compare(
    summaries: dict[tuple[str, str, str], dict[str, str]],
    pairs: list[tuple[str, str, str]],
) -> list[dict[str, str | int]]:
    groups = sorted({(workload, mode) for _, workload, mode in summaries})
    rows = []
    for workload, mode in groups:
        for name, baseline_name, treatment_name in pairs:
            baseline = summaries.get((baseline_name, workload, mode))
            treatment = summaries.get((treatment_name, workload, mode))
            if baseline is None or treatment is None:
                raise ValueError(
                    f"{workload}/{mode}/{name}: 缺少 {baseline_name} 或 {treatment_name}"
                )
            baseline_ns = int(baseline["median_ns"])
            treatment_ns = int(treatment["median_ns"])
            delta_ns = treatment_ns - baseline_ns
            rows.append(
                {
                    "workload": workload,
                    "mode": mode,
                    "comparison": name,
                    "baseline": baseline_name,
                    "treatment": treatment_name,
                    "state": "READY",
                    "baseline_median_ns": baseline_ns,
                    "treatment_median_ns": treatment_ns,
                    "delta_ns": delta_ns,
                    "delta_percent": f"{delta_ns * 100 / baseline_ns:.3f}",
                }
            )
    return rows


def write_tsv(path: Path, rows: list[dict[str, str | int]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(
            stream, fieldnames=OUTPUT_COLUMNS, delimiter="\t", lineterminator="\n"
        )
        writer.writeheader()
        writer.writerows(rows)
    temporary.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--pair", action="append", required=True)
    args = parser.parse_args()
    try:
        pairs = [parse_pair(value) for value in args.pair]
        if len({name for name, _, _ in pairs}) != len(pairs):
            raise ValueError("比较项名称不能重复")
        write_tsv(args.output, compare(read_summary(args.input), pairs))
    except (OSError, ValueError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
