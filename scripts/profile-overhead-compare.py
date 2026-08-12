#!/usr/bin/env python3
"""Compare formal, counter-only and sampled workload runs."""

import argparse
import json
import re
import statistics
import sys
from pathlib import Path


def elapsed(path: Path, elapsed_pattern: re.Pattern[str], success_pattern: re.Pattern[str]) -> float:
    matches = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        match = elapsed_pattern.search(line)
        if match:
            if not success_pattern.search(line):
                raise ValueError(f"workload result is not successful in {path}")
            matches.append(float(match.group(1)))
    if len(matches) != 1 or matches[0] <= 0:
        raise ValueError(f"expected one successful workload result in {path}")
    return matches[0]


def elapsed_group(value: str, elapsed_pattern: re.Pattern[str],
                  success_pattern: re.Pattern[str]) -> tuple[list[float], float]:
    paths = [Path(item) for item in value.split(",") if item]
    if not paths:
        raise ValueError("empty run group")
    values = [elapsed(path, elapsed_pattern, success_pattern) for path in paths]
    return values, statistics.median(values)


def overhead(measured: float, baseline: float) -> float:
    return (measured / baseline - 1.0) * 100.0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("formal", help="one serial log or comma-separated repeated logs")
    parser.add_argument("counter", help="one serial log or comma-separated repeated logs")
    parser.add_argument("sample", help="one serial log or comma-separated repeated logs")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--counter-limit", type=float, default=5.0)
    parser.add_argument("--sample-limit", type=float, default=10.0)
    parser.add_argument("--elapsed-regex", default=r"elapsed_s=([0-9]+(?:\.[0-9]+)?)\b",
                        help="regex with elapsed seconds in capture group 1")
    parser.add_argument("--success-regex", default=r"\bok=true\b")
    args = parser.parse_args()
    try:
        elapsed_pattern = re.compile(args.elapsed_regex)
        success_pattern = re.compile(args.success_regex)
        if elapsed_pattern.groups < 1:
            raise ValueError("elapsed regex must contain capture group 1")
        formal_runs, formal = elapsed_group(args.formal, elapsed_pattern, success_pattern)
        counter_runs, counter = elapsed_group(args.counter, elapsed_pattern, success_pattern)
        sample_runs, sample = elapsed_group(args.sample, elapsed_pattern, success_pattern)
    except (OSError, ValueError, re.error) as error:
        print(f"profile overhead compare: {error}", file=sys.stderr)
        return 1
    result = {
        "formal_seconds": formal,
        "counter_seconds": counter,
        "sample_seconds": sample,
        "formal_runs_seconds": formal_runs,
        "counter_runs_seconds": counter_runs,
        "sample_runs_seconds": sample_runs,
        "counter_overhead_pct": overhead(counter, formal),
        "sample_total_overhead_pct": overhead(sample, formal),
        "sample_incremental_overhead_pct": overhead(sample, counter),
        "counter_limit_pct": args.counter_limit,
        "sample_limit_pct": args.sample_limit,
    }
    result["counter_pass"] = result["counter_overhead_pct"] <= args.counter_limit
    result["sample_pass"] = result["sample_total_overhead_pct"] <= args.sample_limit
    result["valid"] = result["counter_pass"] and result["sample_pass"]
    rendered = json.dumps(result, indent=2)
    print(rendered)
    if args.output:
        args.output.write_text(rendered + "\n", encoding="utf-8")
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
