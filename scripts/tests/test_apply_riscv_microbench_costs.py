"""RISC-V 微基准成本账本的行级闭合回归测试。"""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location(
    "apply_riscv_microbench_costs",
    SCRIPTS / "apply-riscv-microbench-costs.py",
)
assert SPEC is not None and SPEC.loader is not None
MAPPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MAPPER)


def estimate(*, bounded: bool) -> dict[str, object]:
    return {
        "point_ns": 2.0 if bounded else None,
        "low_ns": 1.0 if bounded else None,
        "high_ns": 3.0 if bounded else None,
        "diagnostic_context_center_ns": 2.5,
        "bounded": bounded,
        "strict": bounded,
        "context_count": 1,
        "assignment": "single-context" if bounded else "restricted",
        "quality": "high-confidence" if bounded else "restricted-context",
        "restrictions": [] if bounded else ["csr-context"],
        "missing_semantic_keys": [],
    }


class DescriptorCostRowTests(unittest.TestCase):
    def row(self, *, bounded: bool) -> dict[str, object]:
        return MAPPER.descriptor_cost_row(
            domain="user",
            domain_count=4,
            descriptor_total_count=5,
            total_count=10,
            descriptor={"descriptor_id": 7, "mnemonic": "addi", "size_bytes": 4},
            semantic_keys={"rv64:32:i:addi"},
            estimate=estimate(bounded=bounded),
        )

    def test_bounded_row_costs_close_from_count_and_weights(self) -> None:
        row = self.row(bounded=True)

        self.assertEqual(row["instruction_share"], 0.4)
        self.assertEqual(row["identified_cost_ns"], 8.0)
        self.assertEqual(row["bounded_cost_low_ns"], 4.0)
        self.assertEqual(row["bounded_cost_high_ns"], 12.0)
        self.assertEqual(row["diagnostic_context_center_cost_ns"], 10.0)

    def test_restricted_row_does_not_emit_aggregate_costs(self) -> None:
        row = self.row(bounded=False)

        self.assertIsNone(row["identified_cost_ns"])
        self.assertIsNone(row["bounded_cost_low_ns"])
        self.assertIsNone(row["bounded_cost_high_ns"])
        self.assertIsNone(row["diagnostic_context_center_cost_ns"])
        self.assertEqual(row["diagnostic_context_center_ns"], 2.5)


if __name__ == "__main__":
    unittest.main()
