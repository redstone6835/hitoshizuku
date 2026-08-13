#!/usr/bin/env python3
"""汇总使用统一计数器协议采集的基准样本。"""

from __future__ import annotations

import argparse
import csv
import math
import sys
from collections import defaultdict
from pathlib import Path

REQUIRED_COLUMNS = {
    "system",
    "workload",
    "mode",
    "boot",
    "round",
    "sample_ticks",
    "counter_hz",
    "status",
}


def percentile(values: list[int], percentage: int) -> int:
    rank = max(1, math.ceil(len(values) * percentage / 100))
    return values[rank - 1]


def ticks_to_ns(ticks: int, counter_hz: int) -> int:
    return (ticks * 1_000_000_000 + counter_hz // 2) // counter_hz


def empty_group(counter_hz: int) -> dict[str, object]:
    return {
        "values": [],
        "failures": 0,
        "rounds": set(),
        "reasons": [],
        "counter_hz": counter_hz,
    }


def parse_csv(
    path: Path, expected_counter_hz: int
) -> dict[tuple[str, str, str, int], dict[str, object]]:
    groups: dict[tuple[str, str, str, int], dict[str, object]] = defaultdict(
        lambda: empty_group(expected_counter_hz)
    )
    with path.open("r", encoding="utf-8", newline="") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        columns = set(reader.fieldnames or ())
        missing = REQUIRED_COLUMNS - columns
        if missing:
            raise ValueError(f"缺少列: {','.join(sorted(missing))}")
        for line_number, row in enumerate(reader, start=2):
            system = (row.get("system") or "").strip()
            workload = (row.get("workload") or "").strip()
            mode = (row.get("mode") or "").strip()
            status = (row.get("status") or "").strip()
            if (
                not system
                or not workload
                or mode not in {"warm", "cold"}
                or status not in {"ok", "error", "unavailable"}
            ):
                raise ValueError(f"第 {line_number} 行 system/workload/mode/status 无效")
            try:
                boot = int((row.get("boot") or "").strip())
                round_number = int((row.get("round") or "").strip())
                counter_hz = int((row.get("counter_hz") or "").strip())
            except ValueError as error:
                raise ValueError(f"第 {line_number} 行 boot/round/counter_hz 不是整数") from error
            if boot < 0 or round_number < 0 or counter_hz <= 0:
                raise ValueError(f"第 {line_number} 行 boot/round/counter_hz 超出范围")
            if counter_hz != expected_counter_hz:
                raise ValueError(
                    f"第 {line_number} 行 counter_hz={counter_hz}，期望 {expected_counter_hz}"
                )
            group = groups[(system, workload, mode, boot)]
            group["rounds"].add(round_number)
            if status == "ok":
                try:
                    sample_ticks = int((row.get("sample_ticks") or "").strip())
                except ValueError as error:
                    raise ValueError(f"第 {line_number} 行 sample_ticks 不是整数") from error
                if sample_ticks < 0:
                    raise ValueError(f"第 {line_number} 行 sample_ticks 不能为负数")
                group["values"].append(sample_ticks)
            else:
                group["failures"] += 1
                detail = (row.get("detail") or "").strip()
                if detail and detail != "-":
                    group["reasons"].append(detail)
    return groups


def metrics(values: list[int], counter_hz: int) -> dict[str, object]:
    ordered = sorted(values)
    if not ordered:
        return {
            "min_ticks": "",
            "median_ticks": "",
            "p95_ticks": "",
            "p99_ticks": "",
            "max_ticks": "",
            "median_ns": "",
            "p95_ns": "",
            "p99_ns": "",
        }
    median_ticks = percentile(ordered, 50)
    p95_ticks = percentile(ordered, 95)
    p99_ticks = percentile(ordered, 99)
    return {
        "min_ticks": ordered[0],
        "median_ticks": median_ticks,
        "p95_ticks": p95_ticks,
        "p99_ticks": p99_ticks,
        "max_ticks": ordered[-1],
        "median_ns": ticks_to_ns(median_ticks, counter_hz),
        "p95_ns": ticks_to_ns(p95_ticks, counter_hz),
        "p99_ns": ticks_to_ns(p99_ticks, counter_hz),
    }


def boot_row(
    system: str,
    workload: str,
    mode: str,
    boot: int,
    group: dict[str, object],
    expected_rounds: int,
    expected_samples: int,
    present: bool,
) -> dict[str, object]:
    values = list(group["values"])
    failures = int(group["failures"])
    rounds = len(group["rounds"])
    complete = int(
        present
        and len(values) == expected_samples
        and rounds == expected_rounds
        and failures == 0
    )
    state = "READY" if complete else ("INCOMPLETE" if present else "UNAVAILABLE")
    reasons = list(group["reasons"])
    if not present:
        reasons.append("缺少 boot 样本")
    elif len(values) != expected_samples:
        reasons.append(f"样本数 {len(values)}，期望 {expected_samples}")
    if present and rounds != expected_rounds:
        reasons.append(f"round 数 {rounds}，期望 {expected_rounds}")
    row: dict[str, object] = {
        "system": system,
        "workload": workload,
        "mode": mode,
        "boot": boot,
        "state": state,
        "complete": complete,
        "valid_samples": len(values),
        "failures": failures,
        "rounds": rounds,
        "counter_hz": group["counter_hz"],
        "reason": ";".join(reasons),
    }
    row.update(metrics(values, int(group["counter_hz"])))
    return row


def summary_row(
    system: str,
    workload: str,
    mode: str,
    boot_rows: list[dict[str, object]],
    groups: list[dict[str, object]],
    counter_hz: int,
) -> dict[str, object]:
    values = [value for group in groups for value in group["values"]]
    failures = sum(int(group["failures"]) for group in groups)
    complete = int(all(int(row["complete"]) == 1 for row in boot_rows))
    reasons = [str(row["reason"]) for row in boot_rows if row["reason"]]
    row: dict[str, object] = {
        "system": system,
        "workload": workload,
        "mode": mode,
        "state": "READY" if complete else ("INCOMPLETE" if values else "UNAVAILABLE"),
        "complete": complete,
        "boots": sum(1 for row in boot_rows if row["state"] != "UNAVAILABLE"),
        "valid_samples": len(values),
        "failures": failures,
        "rounds": sum(int(row["rounds"]) for row in boot_rows),
        "counter_hz": counter_hz,
        "reason": ";".join(reasons),
    }
    row.update(metrics(values, counter_hz))
    return row


BOOT_COLUMNS = [
    "system",
    "workload",
    "mode",
    "boot",
    "state",
    "complete",
    "valid_samples",
    "failures",
    "rounds",
    "counter_hz",
    "min_ticks",
    "median_ticks",
    "p95_ticks",
    "p99_ticks",
    "max_ticks",
    "median_ns",
    "p95_ns",
    "p99_ns",
    "reason",
]

SUMMARY_COLUMNS = [
    "system",
    "workload",
    "mode",
    "state",
    "complete",
    "boots",
    "valid_samples",
    "failures",
    "rounds",
    "min_ticks",
    "median_ticks",
    "p95_ticks",
    "p99_ticks",
    "max_ticks",
    "counter_hz",
    "median_ns",
    "p95_ns",
    "p99_ns",
    "reason",
]

def write_tsv(path: Path, columns: list[str], rows: list[dict[str, object]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=columns, delimiter="\t", lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def write_outputs(
    output_dir: Path,
    boot_rows: list[dict[str, object]],
    summary_rows: list[dict[str, object]],
) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    write_tsv(output_dir / "boot-summary.tsv", BOOT_COLUMNS, boot_rows)
    write_tsv(output_dir / "summary.tsv", SUMMARY_COLUMNS, summary_rows)
    with (output_dir / "summary.md").open("w", encoding="utf-8") as stream:
        stream.write("# Benchmark Summary\n\n")
        stream.write(
            "| system | workload | mode | state | boots | samples | median ns | p95 ns | p99 ns |\n"
        )
        stream.write("|---|---|---|---|---:|---:|---:|---:|---:|\n")
        for row in summary_rows:
            stream.write(
                "| {system} | {workload} | {mode} | {state} | {boots} | {valid_samples} | "
                "{median_ns} | {p95_ns} | {p99_ns} |\n".format(**row)
            )


def parse_list(value: str, name: str) -> list[str]:
    entries = [entry.strip() for entry in value.split(",") if entry.strip()]
    if not entries:
        raise ValueError(f"{name} 不能为空")
    return entries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--systems", required=True)
    parser.add_argument("--workloads", required=True)
    parser.add_argument("--modes", required=True)
    parser.add_argument("--expected-boots", type=int, default=3)
    parser.add_argument("--expected-rounds", type=int, default=5)
    parser.add_argument("--expected-samples-per-boot", type=int, default=5000)
    parser.add_argument("--counter-hz", type=int, default=10_000_000)
    parser.add_argument("--require-complete", action="store_true")
    args = parser.parse_args()
    if (
        args.expected_boots < 1
        or args.expected_rounds < 1
        or args.expected_samples_per_boot < 1
        or args.counter_hz < 1
    ):
        parser.error("期望 boot/round/样本数和 counter_hz 必须为正数")
    if not args.input.is_file():
        print(f"输入文件不存在: {args.input}", file=sys.stderr)
        return 2
    try:
        systems = parse_list(args.systems, "systems")
        workloads = parse_list(args.workloads, "workloads")
        modes = parse_list(args.modes, "modes")
        if any(mode not in {"warm", "cold"} for mode in modes):
            raise ValueError("modes 只允许 warm,cold")
        groups = parse_csv(args.input, args.counter_hz)
        allowed = {
            (system, workload, mode, boot)
            for system in systems
            for workload in workloads
            for mode in modes
            for boot in range(args.expected_boots)
        }
        unexpected = sorted(set(groups) - allowed)
        if unexpected:
            raise ValueError(f"存在矩阵外样本: {unexpected}")

        all_boot_rows = []
        all_summary_rows = []
        for system in systems:
            for workload in workloads:
                for mode in modes:
                    workload_boot_rows = []
                    workload_groups = []
                    for boot in range(args.expected_boots):
                        key = (system, workload, mode, boot)
                        present = key in groups
                        group = groups.get(key, empty_group(args.counter_hz))
                        row = boot_row(
                            system,
                            workload,
                            mode,
                            boot,
                            group,
                            args.expected_rounds,
                            args.expected_samples_per_boot,
                            present,
                        )
                        workload_boot_rows.append(row)
                        workload_groups.append(group)
                    all_boot_rows.extend(workload_boot_rows)
                    all_summary_rows.append(
                        summary_row(
                            system,
                            workload,
                            mode,
                            workload_boot_rows,
                            workload_groups,
                            args.counter_hz,
                        )
                    )
        write_outputs(args.output_dir, all_boot_rows, all_summary_rows)
    except (OSError, ValueError) as error:
        print(f"无法统计样本: {error}", file=sys.stderr)
        return 2
    if args.require_complete and any(row["complete"] != 1 for row in all_summary_rows):
        return 4
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
