#!/usr/bin/env python3
"""Extract exact per-syscall aggregates from a BuildStorm serial capture."""

from __future__ import annotations

import argparse
import csv
import json
import re
import tempfile
from collections import defaultdict
from pathlib import Path
from typing import Any


class CaptureError(RuntimeError):
    pass


SYSCALL_REQUIRED = {
    "phase",
    "syscall",
    "calls",
    "completed",
    "inflight",
    "success",
    "errors",
    "cycles",
    "max_cycles",
    "wall_ns",
    "on_cpu_ns",
    "off_cpu_ns",
    "max_latency_ns",
    "migrations",
    "p50_ns",
    "p95_ns",
    "p99_ns",
    "hist",
}
SUM_FIELDS = (
    "calls",
    "completed",
    "inflight",
    "success",
    "errors",
    "cycles",
    "wall_ns",
    "on_cpu_ns",
    "off_cpu_ns",
    "migrations",
)


def atomic_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with open(descriptor, "w", encoding="utf-8", newline="") as stream:
            stream.write(content)
        Path(temporary).replace(path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def atomic_tsv(path: Path, fields: list[str], rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with open(descriptor, "w", encoding="utf-8", newline="") as stream:
            writer = csv.DictWriter(stream, fieldnames=fields, delimiter="\t", extrasaction="ignore")
            writer.writeheader()
            writer.writerows(rows)
        Path(temporary).replace(path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def fields(line: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for token in line.split():
        if "=" not in token:
            raise CaptureError(f"malformed capture token: {token}")
        name, value = token.split("=", 1)
        if not name or not value or name in result:
            raise CaptureError(f"invalid capture token: {token}")
        result[name] = value
    return result


def uint(values: dict[str, str], name: str) -> int:
    try:
        value = int(values[name], 0)
    except (KeyError, ValueError) as error:
        raise CaptureError(f"invalid integer field {name}") from error
    if value < 0:
        raise CaptureError(f"negative integer field {name}={value}")
    return value


def percentile(histogram: list[int], percent: int) -> int:
    total = sum(histogram)
    if total == 0:
        return 0
    target = (total * percent + 99) // 100
    seen = 0
    for bucket, count in enumerate(histogram):
        seen += count
        if seen >= target:
            return 0 if bucket == 0 else 1 << (bucket - 1)
    return 1 << (len(histogram) - 2)


def parse_names(path: Path) -> dict[int, str]:
    pattern = re.compile(r"^pub const SYS_([A-Z0-9_]+): usize = ([0-9]+);")
    result: dict[int, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = pattern.match(line)
        if match is not None:
            result[int(match.group(2))] = match.group(1).lower()
    return result


def clean_line(raw: str) -> str:
    line = raw.rstrip("\r\n")
    if line.startswith("~ # "):
        line = line[4:]
    return line


def parse_serial(path: Path) -> dict[str, Any]:
    active = False
    seen_after = 0
    header: dict[str, str] | None = None
    rows: list[dict[str, Any]] = []
    errnos: list[dict[str, int]] = []
    keys: set[tuple[int, int]] = set()
    workload_root_marker: int | None = None

    for raw in path.read_text(encoding="utf-8", errors="strict").splitlines():
        line = clean_line(raw)
        if line.startswith("@@PROFILE_WORKLOAD_ROOT "):
            marker = fields(line.split(maxsplit=1)[1])
            root = uint(marker, "pid")
            if workload_root_marker is not None and workload_root_marker != root:
                raise CaptureError("conflicting workload root markers")
            workload_root_marker = root
            continue
        if line.startswith("@@PROFILE_STATS_BEGIN "):
            marker = fields(line.split(maxsplit=1)[1])
            active = marker.get("phase") == "after"
            if active:
                seen_after += 1
            continue
        if line.startswith("@@PROFILE_STATS_END "):
            active = False
            continue
        if not active:
            continue
        if line.startswith("state="):
            if header is not None:
                raise CaptureError("duplicate after stats header")
            header = fields(line)
            continue
        if not line.startswith("phase=") or " syscall=" not in line:
            continue
        values = fields(line)
        if "errno" in values:
            errnos.append(
                {
                    "phase": uint(values, "phase"),
                    "nr": uint(values, "syscall"),
                    "errno": uint(values, "errno"),
                    "count": uint(values, "count"),
                }
            )
            continue
        missing = SYSCALL_REQUIRED.difference(values)
        if missing:
            raise CaptureError(f"syscall row misses fields: {', '.join(sorted(missing))}")
        phase = uint(values, "phase")
        nr = uint(values, "syscall")
        key = (phase, nr)
        if key in keys:
            raise CaptureError(f"duplicate syscall row phase={phase} nr={nr}")
        keys.add(key)
        histogram = [int(item, 10) for item in values["hist"].split(",")]
        if len(histogram) != 64 or any(value < 0 for value in histogram):
            raise CaptureError(f"invalid syscall histogram phase={phase} nr={nr}")
        row: dict[str, Any] = {"phase": phase, "nr": nr, "histogram": histogram}
        for name in SYSCALL_REQUIRED - {"phase", "syscall", "hist"}:
            row[name] = uint(values, name)
        if row["completed"] != row["success"] + row["errors"]:
            raise CaptureError(f"completed result mismatch phase={phase} nr={nr}")
        if row["calls"] != row["completed"] + row["inflight"]:
            raise CaptureError(f"entry/completion mismatch phase={phase} nr={nr}")
        if row["wall_ns"] != row["on_cpu_ns"] + row["off_cpu_ns"]:
            raise CaptureError(f"wall/on/off mismatch phase={phase} nr={nr}")
        if sum(histogram) != row["completed"]:
            raise CaptureError(f"latency histogram mismatch phase={phase} nr={nr}")
        for name, percent in (("p50_ns", 50), ("p95_ns", 95), ("p99_ns", 99)):
            if row[name] != percentile(histogram, percent):
                raise CaptureError(f"{name} mismatch phase={phase} nr={nr}")
        if row["max_cycles"] > row["cycles"] or row["max_latency_ns"] > row["wall_ns"]:
            raise CaptureError(f"maximum exceeds total phase={phase} nr={nr}")
        rows.append(row)

    if seen_after != 1 or header is None:
        raise CaptureError(f"expected one after stats section, found {seen_after}")
    for name, expected in (("state", "frozen"), ("enabled", "0"), ("active_writers", "0")):
        if header.get(name) != expected:
            raise CaptureError(f"invalid after stats {name}={header.get(name)!r}")
    header_root = uint(header, "workload_root") if "workload_root" in header else None
    if workload_root_marker is not None and header_root not in (None, 0, workload_root_marker):
        raise CaptureError("workload root marker disagrees with stats header")
    workload_root = workload_root_marker if workload_root_marker is not None else header_root
    if workload_root is None:
        raise CaptureError("missing workload root marker")
    if workload_root == 0:
        raise CaptureError("workload_root is zero; syscall counts are not workload-scoped")
    if not rows:
        raise CaptureError("after stats contains no syscall rows")
    return {
        "header": header,
        "workload_root": workload_root,
        "rows": rows,
        "errnos": errnos,
    }


def aggregate(rows: list[dict[str, Any]], names: dict[int, str]) -> list[dict[str, Any]]:
    grouped: dict[int, dict[str, Any]] = {}
    for row in rows:
        nr = int(row["nr"])
        aggregate_row = grouped.setdefault(
            nr,
            {
                "nr": nr,
                "name": names.get(nr, f"syscall_{nr}"),
                "phases": set(),
                **{name: 0 for name in SUM_FIELDS},
                "max_cycles": 0,
                "max_latency_ns": 0,
                "histogram": [0] * 64,
            },
        )
        aggregate_row["phases"].add(int(row["phase"]))
        for name in SUM_FIELDS:
            aggregate_row[name] += int(row[name])
        aggregate_row["max_cycles"] = max(aggregate_row["max_cycles"], int(row["max_cycles"]))
        aggregate_row["max_latency_ns"] = max(
            aggregate_row["max_latency_ns"], int(row["max_latency_ns"])
        )
        for index, count in enumerate(row["histogram"]):
            aggregate_row["histogram"][index] += int(count)

    result: list[dict[str, Any]] = []
    for row in grouped.values():
        completed = int(row["completed"])
        histogram = row.pop("histogram")
        row["phases"] = ",".join(str(value) for value in sorted(row["phases"]))
        row["avg_wall_ns"] = f"{row['wall_ns'] / completed:.3f}" if completed else ""
        row["avg_on_cpu_ns"] = f"{row['on_cpu_ns'] / completed:.3f}" if completed else ""
        row["avg_off_cpu_ns"] = f"{row['off_cpu_ns'] / completed:.3f}" if completed else ""
        row["p50_ns"] = percentile(histogram, 50)
        row["p95_ns"] = percentile(histogram, 95)
        row["p99_ns"] = percentile(histogram, 99)
        result.append(row)
    return sorted(result, key=lambda row: (-int(row["on_cpu_ns"]), int(row["nr"])))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("serial", type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument(
        "--syscall-table",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "kernel/src/syscalls/nr.rs",
    )
    arguments = parser.parse_args()
    try:
        capture = parse_serial(arguments.serial)
        names = parse_names(arguments.syscall_table)
        rows = capture["rows"]
        aggregate_rows = aggregate(rows, names)
        by_phase = []
        for row in rows:
            exported = {key: value for key, value in row.items() if key != "histogram"}
            exported["name"] = names.get(int(row["nr"]), f"syscall_{row['nr']}")
            completed = int(row["completed"])
            exported["avg_wall_ns"] = f"{int(row['wall_ns']) / completed:.3f}" if completed else ""
            exported["avg_on_cpu_ns"] = f"{int(row['on_cpu_ns']) / completed:.3f}" if completed else ""
            exported["avg_off_cpu_ns"] = f"{int(row['off_cpu_ns']) / completed:.3f}" if completed else ""
            by_phase.append(exported)
        by_phase.sort(key=lambda row: (-int(row["on_cpu_ns"]), int(row["phase"]), int(row["nr"])))
        errno_rows = [
            {**row, "name": names.get(row["nr"], f"syscall_{row['nr']}")}
            for row in capture["errnos"]
        ]
        errno_rows.sort(key=lambda row: (-row["count"], row["nr"], row["errno"]))
        output = arguments.output_dir
        fields_common = [
            "nr", "name", "calls", "completed", "inflight", "success", "errors",
            "cycles", "max_cycles", "wall_ns", "on_cpu_ns", "off_cpu_ns",
            "avg_wall_ns", "avg_on_cpu_ns", "avg_off_cpu_ns", "p50_ns", "p95_ns",
            "p99_ns", "max_latency_ns", "migrations",
        ]
        atomic_tsv(output / "syscalls.tsv", ["phases", *fields_common], aggregate_rows)
        atomic_tsv(output / "syscalls-by-phase.tsv", ["phase", *fields_common], by_phase)
        atomic_tsv(output / "errnos.tsv", ["phase", "nr", "name", "errno", "count"], errno_rows)
        summary = {
            "schema": "mygo.buildstorm-syscalls.v1",
            "serial": str(arguments.serial.resolve()),
            "session": int(capture["header"].get("session", "0"), 0),
            "generation": int(capture["header"].get("generation", "0"), 0),
            "workload_root": int(capture["workload_root"]),
            "unique_syscalls": len(aggregate_rows),
            "phase_rows": len(rows),
            "totals": {name: sum(int(row[name]) for row in aggregate_rows) for name in SUM_FIELDS},
            "outputs": {
                "syscalls": "syscalls.tsv",
                "syscalls_by_phase": "syscalls-by-phase.tsv",
                "errnos": "errnos.tsv",
            },
        }
        atomic_text(output / "summary.json", json.dumps(summary, indent=2, ensure_ascii=False) + "\n")
    except (CaptureError, OSError, ValueError) as error:
        print(f"buildstorm syscall analysis: {error}", file=__import__("sys").stderr)
        return 1
    print(
        "buildstorm syscalls: "
        f"unique={summary['unique_syscalls']} calls={summary['totals']['calls']} "
        f"completed={summary['totals']['completed']} output={arguments.output_dir}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
