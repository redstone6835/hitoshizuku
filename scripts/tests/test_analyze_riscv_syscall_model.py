"""RISC-V syscall-path model pricing regression tests."""

from __future__ import annotations

import csv
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location(
    "analyze_riscv_syscall_model",
    SCRIPTS / "analyze-riscv-syscall-model.py",
)
assert SPEC is not None and SPEC.loader is not None
ANALYZER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYZER)


def weight(semantic: str, value: float, low: float, high: float) -> dict[str, object]:
    return {
        "key": {"semantic_encoding_key": semantic},
        "ns_per_instruction": value,
        "simultaneous_ci": [low, high],
        "quality": "high-confidence",
    }


def weights_document() -> dict[str, object]:
    return {
        "schema_version": 2,
        "instruction_key": ANALYZER.COST_MODEL.MODEL_KEY,
        "model": "fixture",
        "primary_response": "fixture-vcpu-time",
        "confidence": 0.95,
        "instructions": [
            weight("rv64:32:i:addi:form=nop", 2.0, 1.0, 3.0),
            weight("rv64:32:i:ecall", 10.0, 9.0, 11.0),
        ],
    }


def plugin_document() -> dict[str, object]:
    return {
        "schema": ANALYZER.INPUT_SCHEMA,
        "target": "riscv64",
        "vcpus": 1,
        "config": {"enter_pc": "0x1000", "exit_pc": "0x1004", "switch_pc": "0x1008"},
        "descriptors": [
            {"id": 1, "mnemonic": "addi", "size": 4, "encodings": ["13000000"]},
            {"id": 2, "mnemonic": "ecall", "size": 4, "encodings": ["73000000"]},
        ],
        "syscalls": [
            {
                "nr": 172,
                "entries": 2,
                "exits": 2,
                "blocks": 4,
                "instructions": 8,
                "descriptor_counts": [{"id": 1, "count": 6}, {"id": 2, "count": 2}],
            }
        ],
        "totals": {
            "entries": 2,
            "exits": 2,
            "blocks": 4,
            "instructions": 8,
            "descriptor_count_sum": 8,
        },
        "closure": {
            "entry_exit_delta": 0,
            "instructions_minus_accounted": 0,
            "closed": True,
        },
        "overflow": {"descriptors": 0},
        "errors": {"bad_marker": 0},
    }


class SyscallModelTests(unittest.TestCase):
    def test_model_costs_reuse_restriction_and_envelope_semantics(self) -> None:
        plugin = ANALYZER.parse_plugin_document(plugin_document())
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "weights.json"
            path.write_text(json.dumps(weights_document()), encoding="utf-8")
            estimates, _ = ANALYZER.descriptor_estimates(plugin, path)

        rows = ANALYZER.analyze_syscalls(plugin, estimates)
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row["instruction_count"], 8)
        self.assertEqual(row["bounded_instruction_count"], 6)
        self.assertEqual(row["unpriced_instruction_count"], 2)
        self.assertEqual(row["restricted_instruction_count"], 2)
        self.assertEqual(row["bounded_instruction_ratio"], 0.75)
        self.assertEqual(row["model_cost_center_ns"], 12.0)
        self.assertEqual(row["model_cost_low_ns"], 6.0)
        self.assertEqual(row["model_cost_high_ns"], 18.0)
        self.assertEqual(row["per_entry_cost_center_ns"], 6.0)
        self.assertEqual(row["per_exit_cost_low_ns"], 3.0)
        self.assertEqual(row["per_exit_cost_high_ns"], 9.0)

    def test_runtime_rows_are_aggregated_and_multiplied_by_per_exit_cost(self) -> None:
        model_rows = [
            {
                "nr": 172,
                "entries": 2,
                "exits": 2,
                "bounded_instruction_ratio": 0.75,
                "per_entry_cost_center_ns": 6.0,
                "per_entry_cost_low_ns": 3.0,
                "per_entry_cost_high_ns": 9.0,
                "per_exit_cost_center_ns": 6.0,
                "per_exit_cost_low_ns": 3.0,
                "per_exit_cost_high_ns": 9.0,
            }
        ]
        runtime = [
            {
                "nr": 172,
                "name": "getpid",
                "calls": 5,
                "completed": 4,
                "inflight": 1,
                "success": 4,
                "errors": 0,
            },
            {"nr": 999, "name": "unknown", "calls": 7},
        ]

        rows, summary = ANALYZER.analyze_runtime(runtime, model_rows, denominator="exit")
        by_nr = {row["nr"]: row for row in rows}
        self.assertEqual(by_nr[172]["runtime_model_cost_center_ns"], 24.0)
        self.assertEqual(by_nr[172]["runtime_model_cost_low_ns"], 12.0)
        self.assertEqual(by_nr[172]["runtime_model_cost_high_ns"], 36.0)
        self.assertEqual(by_nr[172]["runtime_priced_instances"], 4)
        self.assertEqual(by_nr[172]["runtime_model_cost_share"], 1.0)
        self.assertFalse(by_nr[999]["model_available"])
        self.assertIsNone(by_nr[999]["runtime_model_cost_center_ns"])
        self.assertEqual(summary["runtime_call_count"], 12)
        self.assertEqual(summary["model_available_call_count"], 5)
        self.assertEqual(summary["priced_instance_count"], 4)
        self.assertEqual(summary["unmodeled_call_count"], 7)

    def test_cli_writes_model_and_runtime_reports(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            model = root / "model.json"
            weights = root / "weights.json"
            runtime = root / "syscalls.tsv"
            output = root / "output"
            model.write_text(json.dumps(plugin_document()), encoding="utf-8")
            weights.write_text(json.dumps(weights_document()), encoding="utf-8")
            runtime.write_text(
                "phase\tnr\tname\tcalls\n"
                "1\t172\tgetpid\t3\n"
                "2\t172\tgetpid\t2\n"
                "2\t999\tunknown\t7\n",
                encoding="utf-8",
            )

            result = ANALYZER.main(
                [
                    str(model),
                    "--weights",
                    str(weights),
                    "--runtime-syscalls",
                    str(runtime),
                    "--output-dir",
                    str(output),
                ]
            )

            self.assertEqual(result, 0)
            with (output / "syscall-model-costs.csv").open(newline="", encoding="utf-8") as stream:
                model_rows = list(csv.DictReader(stream))
            with (output / "syscall-runtime-costs.csv").open(newline="", encoding="utf-8") as stream:
                runtime_rows = list(csv.DictReader(stream))
            summary = json.loads((output / "summary.json").read_text(encoding="utf-8"))
            self.assertEqual(model_rows[0]["nr"], "172")
            self.assertEqual(runtime_rows[0]["runtime_calls"], "5")
            self.assertEqual(summary["aggregate"]["bounded_instruction_ratio"], 0.75)
            self.assertEqual(summary["runtime"]["model_cost_center_ns"], 30.0)

    def test_parser_rejects_bad_encoding_size(self) -> None:
        document = plugin_document()
        document["descriptors"][0]["encodings"] = ["1300"]  # type: ignore[index]
        with self.assertRaisesRegex(ANALYZER.AnalysisError, "length disagrees"):
            ANALYZER.parse_plugin_document(document)

    def test_parser_rejects_unknown_descriptor_reference(self) -> None:
        document = plugin_document()
        document["syscalls"][0]["descriptor_counts"] = [  # type: ignore[index]
            {"id": 99, "count": 8}
        ]
        with self.assertRaisesRegex(ANALYZER.AnalysisError, "unknown descriptors"):
            ANALYZER.parse_plugin_document(document)

    def test_parser_rejects_nonclosing_totals(self) -> None:
        document = plugin_document()
        document["totals"]["instructions"] = 9  # type: ignore[index]
        with self.assertRaisesRegex(ANALYZER.AnalysisError, "does not close"):
            ANALYZER.parse_plugin_document(document)

    def test_runtime_parser_aggregates_phases(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "syscalls.tsv"
            path.write_text(
                "phase\tnr\tname\tcalls\n1\t172\tgetpid\t3\n2\t172\tgetpid\t4\n",
                encoding="utf-8",
            )
            rows = ANALYZER.parse_runtime_syscalls(path)
        self.assertEqual(
            rows,
            [{
                "nr": 172,
                "calls": 7,
                "completed": 7,
                "inflight": 0,
                "success": 7,
                "errors": 0,
                "name": "getpid",
                "source_rows": 2,
            }],
        )

    def test_runtime_parser_preserves_inflight_calls(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "syscalls.tsv"
            path.write_text(
                "nr\tname\tcalls\tcompleted\tinflight\tsuccess\terrors\n"
                "63\tread\t9\t7\t2\t6\t1\n",
                encoding="utf-8",
            )
            rows = ANALYZER.parse_runtime_syscalls(path)
        self.assertEqual(rows[0]["completed"], 7)
        self.assertEqual(rows[0]["inflight"], 2)


if __name__ == "__main__":
    unittest.main()
