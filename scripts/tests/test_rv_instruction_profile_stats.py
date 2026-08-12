"""RISC-V 指令时序画像统计模块单元测试。"""

from __future__ import annotations

import math
import random
import unittest

from scripts.rv_instruction_profile_stats import (
    NANOSECONDS_PER_SECOND,
    StatisticsError,
    adjacent_segment_block_permutation_js,
    aggregate_epoch_rows,
    assess_distribution_confidence,
    diagnose_serial_dependence,
    detect_change_points,
    global_change_point_block_permutation_test,
    global_change_point_block_sensitivity_test,
    holm_adjust,
    moving_block_bootstrap_boundary_stability,
    prepare_feature_matrix,
    run_segmentation_sensitivity,
    standardize_matrix,
    weighted_distribution_block_bootstrap,
    weighted_stage_distributions,
)


def epoch(
    index: int,
    counts: dict[str, int | float],
    *,
    rate: float | None = None,
    kernel_share: float = 0.8,
) -> dict[str, object]:
    """构造一秒 epoch。"""

    return {
        "time_ns": index * NANOSECONDS_PER_SECOND,
        "counts": counts,
        "rate": sum(counts.values()) if rate is None else rate,
        "kernel_share": kernel_share,
    }


def phased_epochs(length: int = 90, seed: int = 3) -> list[dict[str, object]]:
    """构造三个具有轻微短程噪声、边界清晰的阶段。"""

    rng = random.Random(seed)
    rows: list[dict[str, object]] = []
    previous_noise = 0.0
    for index in range(length):
        innovation = rng.gauss(0.0, 2.0)
        previous_noise = 0.45 * previous_noise + innovation
        if index < length // 3:
            a, b, c, rate, share = 90.0, 7.0, 3.0, 100.0, 0.92
        elif index < 2 * length // 3:
            a, b, c, rate, share = 12.0, 82.0, 6.0, 145.0, 0.73
        else:
            a, b, c, rate, share = 8.0, 12.0, 80.0, 75.0, 0.55
        rows.append(
            epoch(
                index,
                {
                    "addi": max(0.0, a + previous_noise),
                    "ld": max(0.0, b - 0.3 * previous_noise),
                    "sd": max(0.0, c + 0.2 * innovation),
                    "rare": 0.1,
                },
                rate=rate + innovation,
                kernel_share=share,
            )
        )
    return rows


class FeatureTests(unittest.TestCase):
    """验证组成特征、桶化和输入约束。"""

    def test_top_coverage_jeffreys_clr_and_standardization(self) -> None:
        rows = [
            epoch(0, {"addi": 950, "ld": 45, "rare": 5}, kernel_share=0.9),
            epoch(1, {"addi": 800, "ld": 195, "rare": 5}, kernel_share=0.7),
            epoch(2, {"addi": 600, "ld": 395, "rare": 5}, kernel_share=0.5),
        ]

        result = prepare_feature_matrix(rows, coverage=0.995)

        self.assertEqual(result["vocabulary"], ["addi", "ld"])
        self.assertEqual(result["components"], ["addi", "ld", "OTHER"])
        self.assertEqual(len(result["matrix"]), 3)
        for raw in result["raw_matrix"]:
            self.assertAlmostEqual(sum(raw[:3]), 0.0)
            self.assertTrue(all(math.isfinite(value) for value in raw))
        for column in range(len(result["feature_names"])):
            values = [row[column] for row in result["matrix"]]
            self.assertAlmostEqual(sum(values), 0.0, places=10)
            if result["feature_names"][column] not in result["constant_features"]:
                self.assertAlmostEqual(sum(value * value for value in values) / 3, 1.0)

    def test_bucket_aggregation_weights_kernel_share_by_instruction_rate(self) -> None:
        rows = [
            epoch(0, {"addi": 100}, rate=100.0, kernel_share=1.0),
            epoch(1, {"addi": 300}, rate=300.0, kernel_share=0.0),
            epoch(2, {"ld": 50}, rate=50.0, kernel_share=0.4),
        ]

        buckets = aggregate_epoch_rows(rows, 2)

        self.assertEqual(len(buckets), 2)
        self.assertEqual(buckets[0]["counts"], {"addi": 400.0})
        self.assertEqual(buckets[0]["rate"], 200.0)
        self.assertEqual(buckets[0]["kernel_share"], 0.25)
        self.assertEqual(buckets[0]["source_epoch_end"], 2)
        self.assertEqual(buckets[1]["duration_ns"], NANOSECONDS_PER_SECOND)

    def test_invalid_time_order_and_reserved_component_are_rejected(self) -> None:
        with self.assertRaisesRegex(StatisticsError, "严格递增"):
            prepare_feature_matrix([epoch(0, {"addi": 1}), epoch(0, {"ld": 1})])
        with self.assertRaisesRegex(StatisticsError, "保留名称"):
            prepare_feature_matrix([epoch(0, {"OTHER": 1})])


class SegmentationTests(unittest.TestCase):
    """验证最短段长、敏感性组合和边界 bootstrap。"""

    def test_pelt_objective_matches_exhaustive_dynamic_programming(self) -> None:
        rng = random.Random(29)
        matrix = [
            [rng.gauss(index // 7, 0.8), rng.gauss(index // 9, 0.5)]
            for index in range(24)
        ]
        penalty = 3.7
        minimum = 3

        detected = detect_change_points(
            matrix, penalty=penalty, min_segment_length=minimum
        )

        def segment_sse(begin: int, end: int) -> float:
            means = [
                sum(matrix[row][column] for row in range(begin, end)) / (end - begin)
                for column in range(2)
            ]
            return sum(
                (matrix[row][column] - means[column]) ** 2
                for row in range(begin, end)
                for column in range(2)
            )

        objective = [float("inf")] * (len(matrix) + 1)
        objective[0] = 0.0
        for end in range(minimum, len(matrix) + 1):
            objective[end] = min(
                (
                    objective[begin] + segment_sse(begin, end) + penalty
                    for begin in range(end - minimum + 1)
                    if math.isfinite(objective[begin])
                ),
                default=float("inf"),
            )

        self.assertAlmostEqual(detected["objective"], objective[-1], places=9)

    def test_pelt_matches_known_three_stage_signal(self) -> None:
        matrix = [[0.0]] * 20 + [[5.0]] * 20 + [[-4.0]] * 20

        result = detect_change_points(
            matrix, penalty=10.0, min_segment_length=8
        )

        self.assertEqual(result["boundaries"], [0, 20, 40, 60])
        self.assertTrue(all(
            right - left >= 8
            for left, right in zip(result["boundaries"], result["boundaries"][1:])
        ))

    def test_sensitivity_runs_all_bucket_and_penalty_combinations(self) -> None:
        result = run_segmentation_sensitivity(
            phased_epochs(120), min_segment_seconds=10
        )

        self.assertEqual(len(result["configurations"]), 12)
        self.assertEqual(
            {row["bucket_seconds"] for row in result["configurations"]},
            {1, 2, 5, 10},
        )
        self.assertEqual(
            {row["penalty_multiplier"] for row in result["configurations"]},
            {0.8, 1.0, 1.2},
        )
        stable = [
            cluster
            for cluster in result["boundary_clusters"]
            if cluster["support_fraction"] >= 0.75
        ]
        self.assertEqual(len(stable), 2)
        self.assertAlmostEqual(
            stable[0]["median_time_ns"] / NANOSECONDS_PER_SECOND, 40, delta=5
        )
        self.assertAlmostEqual(
            stable[1]["median_time_ns"] / NANOSECONDS_PER_SECOND, 80, delta=5
        )

    def test_piecewise_residual_bootstrap_recovers_strong_boundary(self) -> None:
        rng = random.Random(11)
        matrix = [
            [rng.gauss(-2.0 if index < 40 else 2.0, 0.18)]
            for index in range(80)
        ]

        result = moving_block_bootstrap_boundary_stability(
            matrix,
            [0, 40, 80],
            penalty=8.0,
            min_segment_length=10,
            replicates=60,
            block_length=4,
            match_tolerance=3,
            seed=19,
        )

        boundary = result["boundaries"][0]
        self.assertGreaterEqual(boundary["stability_probability"], 0.95)
        self.assertAlmostEqual(boundary["conditional_median"], 40, delta=1)
        self.assertGreaterEqual(result["exact_change_count_probability"], 0.9)


class HypothesisTests(unittest.TestCase):
    """验证选择校正全局检验、JS 描述与 Holm 校正。"""

    def test_global_test_reselects_boundaries_and_rejects_clear_phases(self) -> None:
        features = prepare_feature_matrix(phased_epochs(90))
        penalty = features["effective_dimension"] * math.log(90)

        result = global_change_point_block_permutation_test(
            features["matrix"],
            penalty=penalty,
            min_segment_length=10,
            block_length=5,
            permutations=199,
            seed=31,
        )

        self.assertTrue(result["selection_corrected"])
        self.assertEqual(result["observed"]["boundaries"], [0, 30, 60, 90])
        self.assertTrue(result["reject_single_segment"])
        self.assertLessEqual(result["p_value"], 0.05)
        self.assertEqual(result["minimum_resolvable_p"], 0.005)

    def test_global_test_does_not_reject_constant_single_segment(self) -> None:
        result = global_change_point_block_permutation_test(
            [[0.0, 1.0] for _ in range(40)],
            penalty=5.0,
            min_segment_length=8,
            block_length=4,
            permutations=39,
            seed=5,
        )

        self.assertEqual(result["observed"]["boundaries"], [0, 40])
        self.assertEqual(result["observed"]["penalized_sse_gain"], 0.0)
        self.assertEqual(result["p_value"], 1.0)
        self.assertFalse(result["reject_single_segment"])

    def test_long_correlated_null_uses_longer_blocks_and_is_not_high_confidence_change(self) -> None:
        rng = random.Random(211)
        value = 0.0
        matrix = []
        for _ in range(180):
            value = 0.9 * value + rng.gauss(0.0, math.sqrt(1.0 - 0.9**2))
            matrix.append([value])
        matrix = standardize_matrix(matrix)["matrix"]
        penalty = math.log(len(matrix))
        selected = detect_change_points(
            matrix, penalty=penalty, min_segment_length=20
        )
        dependence = diagnose_serial_dependence(
            matrix,
            boundaries=selected["boundaries"],
            feature_names=["ar09"],
        )

        result = global_change_point_block_sensitivity_test(
            matrix,
            penalty=penalty,
            min_segment_length=20,
            dependence=dependence,
            primary_permutations=99,
            long_permutations=99,
            seed=77,
        )

        self.assertGreater(
            dependence["primary_block_length"],
            dependence["cubic_root_baseline"],
        )
        self.assertGreater(
            dependence["long_block_length"],
            dependence["primary_block_length"],
        )
        self.assertTrue(result["conclusions_agree"])
        self.assertTrue(result["all_fail_to_reject_single_segment"])
        self.assertFalse(result["all_reject_single_segment"])

    def test_clear_change_survives_both_conservative_block_lengths(self) -> None:
        rng = random.Random(313)
        matrix = []
        for index in range(180):
            mean = -4.0 if index < 60 else 4.0 if index < 120 else 0.0
            matrix.append([mean + rng.gauss(0.0, 0.25)])
        matrix = standardize_matrix(matrix)["matrix"]
        penalty = math.log(len(matrix))
        selected = detect_change_points(
            matrix, penalty=penalty, min_segment_length=20
        )
        dependence = diagnose_serial_dependence(
            matrix,
            boundaries=selected["boundaries"],
            feature_names=["clear-phase"],
        )

        result = global_change_point_block_sensitivity_test(
            matrix,
            penalty=penalty,
            min_segment_length=20,
            dependence=dependence,
            primary_permutations=99,
            long_permutations=99,
            seed=88,
        )

        self.assertEqual(selected["boundaries"], [0, 60, 120, 180])
        self.assertTrue(dependence["adequate_for_high_confidence"])
        self.assertTrue(result["conclusions_agree"])
        self.assertTrue(result["all_reject_single_segment"])

    def test_adjacent_segments_are_distinct_after_holm_correction(self) -> None:
        rows = phased_epochs(90)

        result = adjacent_segment_block_permutation_js(
            rows,
            [0, 30, 60, 90],
            block_length=5,
            permutations=199,
            seed=7,
        )

        self.assertEqual(len(result["tests"]), 2)
        self.assertTrue(result["all_adjacent_pairs_significant"])
        self.assertTrue(
            all(test["holm_adjusted_p"] <= 0.05 for test in result["tests"])
        )
        self.assertEqual(result["tests"][0]["minimum_resolvable_p"], 0.005)

    def test_holm_adjustment_is_returned_in_original_order(self) -> None:
        self.assertEqual(holm_adjust([0.04, 0.01, 0.03]), [0.06, 0.03, 0.06])


class WeightedDistributionTests(unittest.TestCase):
    """验证带权分布、归因元数据、ESS 和 top-k 稳定性。"""

    def weighted_rows(self, count: int = 60) -> list[dict[str, object]]:
        rows: list[dict[str, object]] = []
        for index in range(count):
            slow = 70.0 + 5.0 * math.sin(index / 5)
            fast = 30.0 - 2.0 * math.sin(index / 5)
            rows.append(
                {
                    "time_ns": index * NANOSECONDS_PER_SECOND,
                    "values": {"amoadd.d": slow, "addi": fast, "rare": 0.1},
                    "exact_count": {
                        "amoadd.d": 10,
                        "addi": 100,
                        "rare": 1,
                        "unmapped": 4,
                    },
                    "attributed_task_clock_ns": {
                        "amoadd.d": slow,
                        "addi": fast,
                        "rare": 0.1,
                    },
                    "weight_ns_per_instruction": {
                        "amoadd.d": slow / 10,
                        "addi": fast / 100,
                        "rare": 0.1,
                    },
                    "shrinkage": {"amoadd.d": 0.2, "addi": 0.5, "rare": 0.8},
                    "source": {
                        "amoadd.d": "perf-jitdump-family-shrink",
                        "addi": "perf-jitdump-exact",
                        "rare": "perf-jitdump-family-shrink",
                        "unmapped": "no-reliable-time-attribution",
                    },
                }
            )
        return rows

    def test_block_bootstrap_reports_ci_ess_topk_and_unattributed(self) -> None:
        result = weighted_distribution_block_bootstrap(
            self.weighted_rows(),
            coverage=0.999,
            block_length=5,
            replicates=200,
            confidence=0.95,
            top_k=1,
            seed=23,
        )
        items = {item["instruction"]: item for item in result["items"]}

        self.assertGreater(items["amoadd.d"]["share"], 0.68)
        self.assertEqual(items["amoadd.d"]["top_k_probability"], 1.0)
        self.assertGreater(items["amoadd.d"]["effective_sample_size"], 5.0)
        self.assertLess(
            items["amoadd.d"]["confidence_interval"][0],
            items["amoadd.d"]["share"],
        )
        self.assertGreater(
            items["amoadd.d"]["confidence_interval"][1],
            items["amoadd.d"]["share"],
        )
        self.assertEqual(items["amoadd.d"]["exact_count"], 600.0)
        self.assertEqual(
            items["amoadd.d"]["sources"], ["perf-jitdump-family-shrink"]
        )
        self.assertEqual(result["unattributed_exact_count"], 240.0)
        self.assertEqual(result["unattributed"][0]["instruction"], "unmapped")

    def test_stage_wrapper_and_confidence_gate_are_json_friendly(self) -> None:
        rows = self.weighted_rows(40)
        stages = weighted_stage_distributions(
            rows,
            [0, 20, 40],
            coverage=0.999,
            block_length=4,
            replicates=40,
            top_k=1,
            seed=2,
        )
        sensitivity = {"boundary_clusters": [{"support_fraction": 1.0}]}
        stability = {"boundaries": [{"stability_probability": 0.95}]}
        tests = {"tests": [{"reject_equal_distribution": True}]}

        assessment = assess_distribution_confidence(
            sensitivity,
            stability,
            tests,
            stages,
            minimum_ess=1.0,
        )

        self.assertEqual(len(stages), 2)
        self.assertTrue(assessment["high_confidence"])
        self.assertEqual(assessment["reasons"], [])

    def test_missing_all_attributed_values_is_rejected_without_unit_fallback(self) -> None:
        with self.assertRaisesRegex(StatisticsError, "已归因成本"):
            weighted_distribution_block_bootstrap(
                [
                    {
                        "time_ns": 0,
                        "values": {},
                        "exact_count": {"addi": 100},
                    }
                ],
                replicates=5,
            )


if __name__ == "__main__":
    unittest.main()
