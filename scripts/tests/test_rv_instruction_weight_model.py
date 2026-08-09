"""RISC-V 逐指令耗时权重模型单元测试。"""

from __future__ import annotations

import json
import math
import random
import unittest

from scripts.rv_instruction_weight_model import (
    WeightModelError,
    fit_instruction_weight_model,
    instruction_family,
    moving_block_indices,
)


class InstructionFamilyTests(unittest.TestCase):
    """验证压缩编码、真实指令和常见 GNU 伪指令的稳定归类。"""

    def test_common_and_pseudo_mnemonics(self) -> None:
        expected = {
            "c.ld": "integer-load",
            "c.sdsp": "integer-store",
            "amoadd.d": "atomic",
            "lr.w": "atomic",
            "bgtu": "conditional-branch",
            "bltz": "conditional-branch",
            "c.bnez": "conditional-branch",
            "ret": "indirect-or-unconditional-branch",
            "sext.w": "integer-add-sub",
            "negw": "integer-add-sub",
            "not": "integer-logic",
            "rdtime": "csr",
            "frcsr": "csr",
            "fscsr": "csr",
            "fsqrt.d": "floating-arithmetic",
            "flt.d": "floating-compare",
            "fsd": "floating-store",
            "vsadd.vv": "vector-arithmetic",
            "vse64.v": "vector-store",
            "vle64.v": "vector-load",
        }
        for mnemonic, family in expected.items():
            with self.subTest(mnemonic=mnemonic):
                self.assertEqual(instruction_family(mnemonic), family)

    def test_empty_mnemonic_is_rejected(self) -> None:
        with self.assertRaises(WeightModelError):
            instruction_family("  ")


class InputAndResamplingTests(unittest.TestCase):
    """验证 vCPU 线程筛选契约和 moving-block 重采样。"""

    @staticmethod
    def thread_rows() -> list[dict[str, object]]:
        return [
            {
                "time_ns": index * 1_000_000_000,
                "counts": {"addi": 100 + index, "ld": 20 + index % 3},
                "task_clock_by_tid_ns": {
                    "101": 1_000 + 3 * index,
                    102: 500 + index,
                    "999": 1_000_000,
                },
            }
            for index in range(24)
        ]

    def test_thread_mapping_requires_explicit_jit_catalog_vcpu_tids(self) -> None:
        with self.assertRaisesRegex(WeightModelError, "vcpu_tids"):
            fit_instruction_weight_model(
                self.thread_rows(), bootstrap_replicates=0, cv_folds=0
            )

        result = fit_instruction_weight_model(
            self.thread_rows(),
            vcpu_tids=[101, "102"],
            bootstrap_replicates=0,
            cv_folds=0,
        )

        self.assertEqual(
            result["vcpu_tid_selection"], "explicit-jitdump-catalog-mapping"
        )
        self.assertEqual(result["epochs"][0]["vcpu_task_clock_ns"], 1_500.0)
        self.assertNotIn(1_000_000.0, [
            row["vcpu_task_clock_ns"] for row in result["epochs"]
        ])

    def test_moving_blocks_are_deterministic_and_preserve_local_order(self) -> None:
        first = moving_block_indices(25, 4, random.Random(17))
        second = moving_block_indices(25, 4, random.Random(17))

        self.assertEqual(first, second)
        self.assertEqual(len(first), 25)
        for begin in range(0, 24, 4):
            block = first[begin : begin + 4]
            self.assertEqual(block, list(range(block[0], block[0] + len(block))))


def synthetic_epochs(length: int = 180, seed: int = 41) -> list[dict[str, object]]:
    """构造含短程相关噪声、翻译开销和少量离群点的可辨识数据。"""

    rng = random.Random(seed)
    rows: list[dict[str, object]] = []
    correlated_noise = 0.0
    for index in range(length):
        addi = 35_000 + rng.randrange(30_000)
        load = 12_000 + rng.randrange(20_000)
        store = 8_000 + rng.randrange(16_000)
        atomic = 600 + rng.randrange(1_200)
        executed_tb = 2_000 + rng.randrange(3_000)
        translated_tb = rng.randrange(500) if index % 11 == 0 else rng.randrange(15)
        translated_insns = translated_tb * (8 + rng.randrange(20))
        duration_ns = 750_000_000 + rng.randrange(500_000_000)
        innovation = rng.gauss(0.0, 500.0)
        correlated_noise = 0.55 * correlated_noise + innovation
        task_clock = (
            0.55 * addi
            + 1.85 * load
            + 2.15 * store
            + 7.5 * atomic
            + 1.2 * executed_tb
            + 45.0 * translated_tb
            + 4.0 * translated_insns
            + 0.000004 * duration_ns
            + 8_000.0
            + correlated_noise
        )
        if index in {47, 133}:
            task_clock += 120_000.0
        rows.append(
            {
                "time_ns": index * 1_000_000_000,
                "duration_ns": duration_ns,
                "exact_counts": {
                    "addi": addi,
                    "ld": load,
                    "sd": store,
                    "amoadd.d": atomic,
                },
                "vcpu_task_clock_ns": task_clock,
                "tb_count": executed_tb,
                "translated_tb_delta": translated_tb,
                "translated_insns_delta": translated_insns,
            }
        )
    return rows


class HierarchicalModelTests(unittest.TestCase):
    """验证非负稳健拟合、bootstrap、CV 和逐 epoch 归因。"""

    @classmethod
    def setUpClass(cls) -> None:
        cls.result = fit_instruction_weight_model(
            synthetic_epochs(),
            bootstrap_replicates=24,
            block_length=6,
            cv_folds=4,
            cv_purge_gap=5,
            max_irls_iterations=12,
            max_coordinate_sweeps=55,
            seed=73,
        )

    def test_recovers_nonnegative_instruction_weights_and_nuisance(self) -> None:
        weights = {
            item["instruction"]: item["ns_per_instruction"]
            for item in self.result["instructions"]
        }

        self.assertTrue(all(value >= 0.0 for value in weights.values()))
        self.assertAlmostEqual(weights["addi"], 0.55, delta=0.18)
        self.assertAlmostEqual(weights["ld"], 1.85, delta=0.25)
        self.assertAlmostEqual(weights["sd"], 2.15, delta=0.30)
        self.assertAlmostEqual(weights["amoadd.d"], 7.5, delta=1.5)
        coefficients = self.result["nuisance"]["coefficients"]
        self.assertIn("executed_tb_count", coefficients)
        self.assertIn("translated_tb_delta", coefficients)
        self.assertIn("translated_insns_delta", coefficients)
        self.assertTrue(all(value >= 0.0 for value in coefficients.values()))

    def test_reports_bootstrap_family_prior_exposure_and_identifiability(self) -> None:
        for item in self.result["instructions"]:
            interval = item["confidence_interval"]
            self.assertIsNotNone(interval)
            self.assertTrue(all(math.isfinite(value) for value in interval))
            self.assertLessEqual(interval[0], interval[1])
            self.assertGreater(item["total_exact_count"], 0.0)
            self.assertIn(
                item["identifiability"],
                {"not-identifiable", "weak", "moderate", "strong"},
            )
            self.assertNotEqual(item["source"], "constant-1.0-fallback")
        self.assertEqual(self.result["bootstrap"]["replicates"], 24)
        self.assertTrue(all(
            family["source"] == "empirical-vcpu-task-clock-family-prior"
            for family in self.result["families"]
        ))

    def test_fit_cv_and_epoch_attribution_are_consistent(self) -> None:
        self.assertIn(self.result["blocked_cv"]["quality"], {"good", "usable"})
        self.assertLess(
            self.result["blocked_cv"]["aggregate"]["relative_mae"], 0.20
        )
        self.assertGreater(self.result["fit"]["huber_downweighted_fraction"], 0.0)
        for row in self.result["epochs"]:
            self.assertAlmostEqual(
                row["predicted_ns"],
                row["attributed_instruction_ns"] + row["attributed_nuisance_ns"],
                places=6,
            )
            self.assertAlmostEqual(
                row["residual_ns"],
                row["vcpu_task_clock_ns"] - row["predicted_ns"],
                places=6,
            )
            self.assertGreaterEqual(row["unattributed_ns"], 0.0)
            self.assertGreaterEqual(row["overattributed_ns"], 0.0)
        json.dumps(self.result, allow_nan=False)

    def test_instruction_key_documents_shared_rvc_weight(self) -> None:
        self.assertEqual(
            self.result["instruction_key"],
            "normalized-mnemonic-shared-across-encoding-sizes",
        )


class IdentifiabilityTests(unittest.TestCase):
    """共线指令必须显式标记为不可辨识，而不是伪造单位权重。"""

    def test_perfectly_collinear_instructions_use_measured_family_prior(self) -> None:
        rows = []
        for index in range(60):
            shared = 100 + index
            addi = 500 + (index * 37) % 211
            rows.append(
                {
                    "time_ns": index * 1_000_000_000,
                    "counts": {"or": shared, "xor": shared, "addi": addi},
                    "vcpu_task_clock_ns": 3.0 * shared + 0.7 * addi,
                }
            )

        result = fit_instruction_weight_model(
            rows,
            bootstrap_replicates=12,
            block_length=4,
            cv_folds=3,
            seed=5,
        )
        items = {item["instruction"]: item for item in result["instructions"]}

        for name in ("or", "xor"):
            self.assertEqual(items[name]["identifiability"], "not-identifiable")
            self.assertEqual(
                items[name]["source"],
                "vcpu-task-clock-family-constrained-nonidentifiable",
            )
            self.assertAlmostEqual(items[name]["max_abs_predictor_correlation"], 1.0)
            self.assertGreaterEqual(items[name]["ns_per_instruction"], 0.0)
            self.assertNotEqual(items[name]["ns_per_instruction"], 1.0)


if __name__ == "__main__":
    unittest.main()
