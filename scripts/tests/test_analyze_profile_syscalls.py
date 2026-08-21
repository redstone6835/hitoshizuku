from __future__ import annotations

import csv
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "analyze_profile_syscalls", ROOT / "scripts/analyze-profile-syscalls.py"
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def histogram(*entries: tuple[int, int]) -> str:
    values = [0] * 64
    for bucket, count in entries:
        values[bucket] = count
    return ",".join(str(value) for value in values)


class ProfileSyscallAnalysisTests(unittest.TestCase):
    def test_extracts_entries_completions_and_aggregates_phases(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            serial = root / "serial.log"
            serial.write_text(
                "@@PROFILE_WORKLOAD_ROOT pid=83 token=fixture\n"
                "@@PROFILE_STATS_BEGIN phase=before case=fixture\n"
                "state=frozen enabled=0 session=1 generation=2 workload_root=0 active_writers=0\n"
                "@@PROFILE_STATS_END phase=before case=fixture\n"
                "@@PROFILE_STATS_BEGIN phase=after case=fixture\n"
                "state=frozen enabled=0 session=7 generation=9 active_writers=0\n"
                f"phase=0 syscall=63 calls=3 completed=2 inflight=1 success=1 errors=1 "
                f"cycles=40 max_cycles=25 wall_ns=30 on_cpu_ns=20 off_cpu_ns=10 "
                f"max_latency_ns=16 migrations=0 p50_ns=8 p95_ns=16 p99_ns=16 "
                f"hist={histogram((4, 1), (5, 1))}\n"
                f"phase=1 syscall=63 calls=1 completed=1 inflight=0 success=1 errors=0 "
                f"cycles=10 max_cycles=10 wall_ns=9 on_cpu_ns=9 off_cpu_ns=0 "
                f"max_latency_ns=8 migrations=0 p50_ns=8 p95_ns=8 p99_ns=8 "
                f"hist={histogram((4, 1))}\n"
                "phase=0 syscall=63 errno=2 count=1\n"
                "@@PROFILE_STATS_END phase=after case=fixture\n",
                encoding="utf-8",
            )
            output = root / "analysis"
            capture = MODULE.parse_serial(serial)
            self.assertEqual(capture["workload_root"], 83)
            rows = MODULE.aggregate(capture["rows"], {63: "read"})
            self.assertEqual(len(rows), 1)
            row = rows[0]
            self.assertEqual(row["calls"], 4)
            self.assertEqual(row["completed"], 3)
            self.assertEqual(row["inflight"], 1)
            self.assertEqual(row["on_cpu_ns"], 29)
            self.assertEqual(row["phases"], "0,1")
            self.assertEqual(row["p50_ns"], 8)
            self.assertEqual(row["p95_ns"], 16)

            MODULE.atomic_tsv(output / "syscalls.tsv", list(row), rows)
            with (output / "syscalls.tsv").open(newline="", encoding="utf-8") as stream:
                written = list(csv.DictReader(stream, delimiter="\t"))
            self.assertEqual(written[0]["name"], "read")
            MODULE.atomic_text(output / "summary.json", json.dumps({"calls": row["calls"]}))
            self.assertEqual(json.loads((output / "summary.json").read_text())["calls"], 4)

    def test_rejects_unscoped_capture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            serial = Path(temporary) / "serial.log"
            serial.write_text(
                "@@PROFILE_STATS_BEGIN phase=after case=fixture\n"
                "state=frozen enabled=0 session=1 generation=1 workload_root=0 active_writers=0\n"
                "@@PROFILE_STATS_END phase=after case=fixture\n",
                encoding="utf-8",
            )
            with self.assertRaises(MODULE.CaptureError):
                MODULE.parse_serial(serial)


if __name__ == "__main__":
    unittest.main()
