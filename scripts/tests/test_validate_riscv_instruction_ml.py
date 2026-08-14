"""RISC-V 指令机器学习结论校验器回归测试。"""

from __future__ import annotations

import dataclasses
import importlib.util
import json
import math
import statistics
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
from riscv_weight_model_seal import (
    FWER_COVERAGE_CLAIM,
    FWER_METHOD,
    MONTE_CARLO_METHOD,
    REPLICATE_PARTITION_METHOD,
    verify_model_document_seal,
)
SPEC = importlib.util.spec_from_file_location(
    "validate_riscv_instruction_ml",
    SCRIPTS / "validate-riscv-instruction-ml.py",
)
assert SPEC is not None and SPEC.loader is not None
VALIDATOR = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = VALIDATOR
SPEC.loader.exec_module(VALIDATOR)


def synthetic_samples(runs: int = 8) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    sequence_by_run = {index: 0 for index in range(runs)}
    for run in range(runs):
        run_shift = (run - runs / 2.0) * 0.015
        pair = 0
        for round_index in range(4):
            for batch in (4096, 16384, 65536):
                for variant, base_weight in (
                    ("reference", 0.55),
                    ("independent", 2.15),
                ):
                    pair += 1
                    weight = (
                        base_weight
                        + run_shift
                        + 0.025 * math.log2(batch / 16384)
                        + (round_index - 1.5) * 0.004
                    )
                    probe_first = (pair + run) % 2 == 0
                    roles = (
                        ("probe", "baseline")
                        if probe_first
                        else ("baseline", "probe")
                    )
                    for role in roles:
                        sequence_by_run[run] += 1
                        base = 250_000.0 + round_index * 17.0
                        cpu = base + (weight * batch if role == "probe" else 0.0)
                        rows.append(
                            {
                                "run_id": f"run-{run}",
                                "block_id": str(round_index + 1),
                                "pair_id": str(pair),
                                "sequence": sequence_by_run[run],
                                "role": role,
                                "order": "AB" if probe_first else "BA",
                                "instruction": "add",
                                "encoding_bytes": 4,
                                "pattern": f"diff:add:{variant}",
                                "requested_count": batch,
                                "target_count": batch if role == "probe" else 0,
                                "plugin_thread_cpu_ns": cpu,
                                "translations_during_window": 0,
                                "target_descriptor": {
                                    "encoding_key": "rv64:32:i:add",
                                    "bytes": "3305b500"
                                    if variant == "reference"
                                    else "b306b500",
                                    "extension": "i",
                                },
                            }
                        )
    return rows


def versioned_differential_samples(runs: int = 8) -> list[dict[str, object]]:
    rows = synthetic_samples(runs)
    for row in rows:
        variant = str(row["pattern"]).rsplit(":", 1)[-1]
        row.update(
            {
                "probe_version": 2,
                "suite": "integer-dataflow-v2",
                "contrast": "add-dataflow",
                "differential_variant": variant,
                "context": (
                    "evolving-dependency-chain"
                    if variant == "reference"
                    else "independent-destination"
                ),
                "pattern": (
                    "dependency-chain"
                    if variant == "reference"
                    else "independent-destination"
                ),
            }
        )
    return rows


class PairObservationTests(unittest.TestCase):
    def test_pairing_and_translation_filter(self) -> None:
        samples = synthetic_samples(runs=2)
        samples[0]["translations_during_window"] = 1

        rows = VALIDATOR.pair_observations(samples)

        self.assertEqual(len(rows), len(samples) // 2 - 1)
        self.assertTrue(all(math.isfinite(row.response_ns) for row in rows))
        self.assertEqual({row.order for row in rows}, {"probe-first", "baseline-first"})

    def test_pair_structure_mismatch_is_rejected(self) -> None:
        samples = synthetic_samples(runs=1)
        samples[1]["pattern"] = "forged-context"

        with self.assertRaisesRegex(VALIDATOR.MlValidationError, "pattern 结构不一致"):
            VALIDATOR.pair_observations(samples)

    def test_response_uses_exact_probe_minus_baseline_target_count(self) -> None:
        samples = synthetic_samples(runs=1)
        first_pair = [row for row in samples if row["pair_id"] == "1"]
        probe = next(row for row in first_pair if row["role"] == "probe")
        baseline = next(row for row in first_pair if row["role"] == "baseline")
        probe["requested_count"] = baseline["requested_count"] = 4096
        probe["target_count"] = 4_125
        baseline["target_count"] = 125
        baseline["plugin_thread_cpu_ns"] = 250_000.0
        probe["plugin_thread_cpu_ns"] = 258_000.0

        observation = next(
            row for row in VALIDATOR.pair_observations(samples) if row.pair_id == "1"
        )

        self.assertEqual(observation.batch, 4096)
        self.assertAlmostEqual(observation.response_ns, 2.0)

    def test_nonpositive_exact_target_delta_is_rejected(self) -> None:
        samples = synthetic_samples(runs=1)
        first_pair = [row for row in samples if row["pair_id"] == "1"]
        for row in first_pair:
            row["target_count"] = 100
        with self.assertRaisesRegex(
            VALIDATOR.MlValidationError, "probe target_count 必须大于 baseline"
        ):
            VALIDATOR.pair_observations(samples)

    def test_calibration_anchor_is_not_a_differential_target(self) -> None:
        rows = VALIDATOR.pair_observations(versioned_differential_samples(2))
        anchor = dataclasses.replace(
            rows[0],
            suite="stability-anchor-v1",
            contrast="positive-div-anchor",
            differential_variant="anchor",
            context="repeated-positive-anchor",
            pattern="stability-anchor-positive-div",
        )
        self.assertIsNone(VALIDATOR._differential_identity(anchor))

    def test_explicit_run_order_must_be_complete_unique_and_consistent(self) -> None:
        incomplete = synthetic_samples(runs=2)
        for row in incomplete:
            if row["run_id"] == "run-0":
                row["run_order"] = 0
        with self.assertRaisesRegex(VALIDATOR.MlValidationError, "覆盖所有"):
            VALIDATOR.pair_observations(incomplete)

        duplicate = synthetic_samples(runs=2)
        for row in duplicate:
            row["run_order"] = 0
        with self.assertRaisesRegex(VALIDATOR.MlValidationError, "复用"):
            VALIDATOR.pair_observations(duplicate)

        inconsistent = synthetic_samples(runs=2)
        for row in inconsistent:
            row["run_order"] = int(str(row["run_id"]).rsplit("-", 1)[1])
        inconsistent[0]["run_order"] = 1
        with self.assertRaisesRegex(VALIDATOR.MlValidationError, "不一致"):
            VALIDATOR.pair_observations(inconsistent)

    def test_legacy_contiguous_run_suffix_recovers_time_order(self) -> None:
        samples = synthetic_samples(runs=3)
        remap = {"run-0": "capture-1", "run-1": "capture-3", "run-2": "capture-2"}
        for row in samples:
            row["run_id"] = remap[str(row["run_id"])]
        observations = VALIDATOR.pair_observations(samples)

        self.assertEqual(
            VALIDATOR._ordered_runs(observations),
            ["capture-1", "capture-2", "capture-3"],
        )

    def test_crossover_pairs_share_one_ml_group(self) -> None:
        samples = synthetic_samples(runs=4)
        duplicated: list[dict[str, object]] = []
        for row in samples:
            source = int(str(row["run_id"]).rsplit("-", 1)[1])
            row["run_order"] = source * 2
            row["super_run_id"] = f"super-{source}"
            row["super_run_order"] = source
            row["crossover_pair"] = 1
            row["crossover_design"] = "ABBA"
            row["timing_launch_position"] = 1
            row["plugin_off_launch_position"] = 2
            duplicate = dict(row)
            duplicate["run_id"] = f"run-{source}-pair-2"
            duplicate["run_order"] = source * 2 + 1
            duplicate["crossover_pair"] = 2
            duplicate["timing_launch_position"] = 4
            duplicate["plugin_off_launch_position"] = 3
            duplicated.append(duplicate)
        observations = VALIDATOR.pair_observations(samples + duplicated)

        self.assertEqual(len(VALIDATOR._ordered_runs(observations)), 8)
        self.assertEqual(len(VALIDATOR._ordered_super_runs(observations)), 4)
        try:
            VALIDATOR._load_sklearn()
        except VALIDATOR.MlValidationError:
            self.skipTest("isolated ML venv is not active")
        _predicted, _dummy, _baseline, folds, _versions = (
            VALIDATOR._cross_validated_predictions(
                observations, folds=2, seed=31, max_iter=10
            )
        )
        for fold in folds:
            self.assertFalse(set(fold["train_runs"]) & set(fold["test_runs"]))
        self.assertEqual(
            {row.run_order_source for row in observations},
            {"explicit-run-order"},
        )

    def test_crossover_launch_positions_are_validated_at_ml_boundary(
        self,
    ) -> None:
        samples = synthetic_samples(runs=2)
        duplicated: list[dict[str, object]] = []
        for row in samples:
            source = int(str(row["run_id"]).rsplit("-", 1)[1])
            row["run_order"] = source * 2
            row["super_run_id"] = f"super-{source}"
            row["super_run_order"] = source
            row["crossover_pair"] = 1
            row["crossover_design"] = "ABBA"
            row["timing_launch_position"] = 1
            row["plugin_off_launch_position"] = 2
            duplicate = dict(row)
            duplicate["run_id"] = f"run-{source}-pair-2"
            duplicate["run_order"] = source * 2 + 1
            duplicate["crossover_pair"] = 2
            duplicate["timing_launch_position"] = 4
            duplicate["plugin_off_launch_position"] = 3
            duplicated.append(duplicate)
        VALIDATOR.pair_observations(samples + duplicated)

        duplicated[0]["timing_launch_position"] = 3
        with self.assertRaisesRegex(
            VALIDATOR.MlValidationError, "元数据不一致|启动位置"
        ):
            VALIDATOR.pair_observations(samples + duplicated)

        missing = [dict(row) for row in samples + duplicated]
        for row in missing:
            row.pop("plugin_off_launch_position", None)
        with self.assertRaisesRegex(
            VALIDATOR.MlValidationError, "启动位置不完整"
        ):
            VALIDATOR.pair_observations(missing)


class PredictionValidationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        try:
            import sklearn  # noqa: F401
        except ImportError as error:
            raise unittest.SkipTest(
                "需要通过 setup-riscv-instruction-ml-venv.sh 安装 scikit-learn"
            ) from error

    def test_grouped_prediction_detects_context_effect(self) -> None:
        result, predictions = VALIDATOR.validate_predictions(
            synthetic_samples(),
            folds=4,
            max_iter=80,
            bootstrap_replicates=199,
            minimum_runs=4,
            minimum_skill_improvement=0.0,
            seed=71,
        )

        self.assertEqual(result["data"]["runs"], 8)
        self.assertEqual(len(predictions), result["data"]["pairs"])
        for fold in result["cross_validation"]["folds"]:
            self.assertFalse(set(fold["train_runs"]) & set(fold["test_runs"]))
        self.assertGreater(
            result["cross_validation"]["mae_skill_improvement_over_global_median"],
            0.5,
        )
        self.assertLess(
            result["cross_validation"]["mae_skill_improvement_over_context_batch"],
            0.1,
        )
        self.assertIsNone(
            result["cross_validation"]["incremental_value"]["gate_passed"]
        )
        self.assertTrue(
            result["cross_validation"]["incremental_value"][
                "diagnostic_equivalence_passed"
            ]
        )
        self.assertFalse(
            result["cross_validation"]["incremental_value"][
                "training_uncertainty_included"
            ]
        )
        self.assertEqual(
            result["cross_validation"]["incremental_value"]["interpretation"],
            "no-practically-material-omitted-structure",
        )
        self.assertIsNotNone(
            result["cross_validation"]["incremental_value"][
                "mae_improvement_run_cluster_ci"
            ]
        )
        self.assertEqual(
            result["cross_validation"]["incremental_value"]["cluster_unit"],
            "complete ABBA/BAAB crossover super-run",
        )
        self.assertEqual(len(result["differential_checks"]), 1)
        check = result["differential_checks"][0]
        self.assertEqual(check["observed_conclusion"], "context-dependent")
        self.assertEqual(check["ml_conclusion_check"], "supported")
        self.assertEqual(result["conclusion"]["status"], "supported")
        self.assertFalse(result["conclusion"]["may_publish_weights"])

    def test_context_batch_baseline_fallbacks_are_train_only(self) -> None:
        observations = VALIDATOR.pair_observations(synthetic_samples(runs=2))
        train = [
            index for index, row in enumerate(observations) if row.run_id == "run-0"
        ]
        known_context_test = next(
            index
            for index, row in enumerate(observations)
            if row.run_id == "run-1" and row.pattern.endswith(":reference")
        )
        unknown_context_test = next(
            index
            for index, row in enumerate(observations)
            if row.run_id == "run-1" and row.pattern.endswith(":independent")
        )
        modified = list(observations)
        modified[known_context_test] = dataclasses.replace(
            modified[known_context_test], batch=12_345
        )
        modified[unknown_context_test] = dataclasses.replace(
            modified[unknown_context_test], pattern="unseen-context"
        )
        target = [row.response_ns for row in modified]
        estimates = VALIDATOR._context_batch_median_predictions(
            modified,
            target,
            train,
            [known_context_test, unknown_context_test],
        )
        known_identity = VALIDATOR._context_identity(modified[known_context_test])
        expected_context = statistics.median(
            target[index]
            for index in train
            if VALIDATOR._context_identity(modified[index]) == known_identity
        )
        expected_global = statistics.median(target[index] for index in train)
        self.assertEqual(estimates, [expected_context, expected_global])

        contaminated_target = list(target)
        contaminated_target[known_context_test] += 10_000.0
        contaminated_target[unknown_context_test] -= 10_000.0
        self.assertEqual(
            VALIDATOR._context_batch_median_predictions(
                modified,
                contaminated_target,
                train,
                [known_context_test, unknown_context_test],
            ),
            estimates,
        )

    def test_omitted_structure_equivalence_gate_has_four_outcomes(self) -> None:
        all_rows = VALIDATOR.pair_observations(synthetic_samples(runs=8))
        observations = []
        seen = set()
        for row in all_rows:
            if row.run_id not in seen:
                observations.append(row)
                seen.add(row.run_id)
        actual = [0.0] * len(observations)

        def check(predicted, baseline, seed):
            return VALIDATOR._incremental_prediction_value(
                observations,
                actual,
                predicted,
                baseline,
                confidence=0.95,
                bootstrap_replicates=999,
                minimum_relative_improvement=0.10,
                practical_equivalence_ns=0.15,
                seed=seed,
            )

        equivalent = check(
            [0.0] * len(observations),
            [0.05] * len(observations),
            181,
        )
        self.assertIsNone(equivalent["gate_passed"])
        self.assertTrue(equivalent["diagnostic_equivalence_passed"])
        self.assertEqual(
            equivalent["interpretation"],
            "no-practically-material-omitted-structure",
        )

        omitted = check(
            [0.0] * len(observations),
            [1.0] * len(observations),
            191,
        )
        self.assertIsNone(omitted["gate_passed"])
        self.assertFalse(omitted["diagnostic_equivalence_passed"])
        self.assertEqual(
            omitted["interpretation"],
            "practically-material-omitted-structure-detected",
        )

        worse = check(
            [1.0] * len(observations),
            [0.0] * len(observations),
            193,
        )
        self.assertIsNone(worse["gate_passed"])
        self.assertFalse(worse["diagnostic_equivalence_passed"])
        self.assertEqual(
            worse["interpretation"],
            "flexible-model-materially-worse-than-structured-baseline",
        )

        predicted = [
            0.6 if index % 4 in {2, 3} else 0.0
            for index in range(len(observations))
        ]
        baseline = [
            0.0 if index % 4 in {2, 3} else 0.6
            for index in range(len(observations))
        ]
        inconclusive = check(predicted, baseline, 197)
        self.assertIsNone(inconclusive["gate_passed"])
        self.assertFalse(inconclusive["diagnostic_equivalence_passed"])
        self.assertEqual(
            inconclusive["interpretation"],
            "inconclusive-against-practical-equivalence-band",
        )

    def test_independent_run_gate_cannot_be_bypassed_by_pair_count(self) -> None:
        result, _ = VALIDATOR.validate_predictions(
            synthetic_samples(runs=4),
            folds=2,
            max_iter=50,
            bootstrap_replicates=49,
            minimum_runs=20,
            seed=91,
        )

        self.assertEqual(
            result["conclusion"]["status"],
            "inconclusive-insufficient-independent-runs",
        )
        self.assertEqual(
            result["cross_validation"]["group_conformal"]["status"],
            "insufficient-calibration-runs",
        )
        conformal = result["cross_validation"]["split_conformal"]
        self.assertFalse(conformal["finite_sample"]["gate_passed"])
        self.assertFalse(conformal["high_confidence_gate"]["passed"])
        self.assertEqual(
            result["conclusion"]["high_confidence_status"],
            "inconclusive-ml-high-confidence-gate",
        )

    def test_versioned_contrast_metadata_drives_differential_matching(self) -> None:
        result, _ = VALIDATOR.validate_predictions(
            versioned_differential_samples(),
            folds=4,
            max_iter=80,
            bootstrap_replicates=99,
            minimum_runs=4,
            minimum_skill_improvement=0.0,
            seed=113,
        )

        self.assertEqual(len(result["differential_checks"]), 1)
        check = result["differential_checks"][0]
        self.assertEqual(check["group"], "add-dataflow")
        self.assertEqual(check["variant"], "independent")
        self.assertEqual(check["observed_conclusion"], "context-dependent")

    def test_differential_effects_match_within_the_same_block(self) -> None:
        samples = synthetic_samples(runs=4)
        for row in samples:
            if str(row["pattern"]).endswith(":independent"):
                row["block_id"] = f"treatment-{row['block_id']}"

        with self.assertRaisesRegex(
            VALIDATOR.MlValidationError, "没有配对 run/batch/block"
        ):
            VALIDATOR.validate_predictions(
                samples,
                folds=2,
                max_iter=20,
                bootstrap_replicates=9,
                minimum_runs=4,
                minimum_skill_improvement=0.0,
                seed=127,
            )

    def test_95_percent_finite_sample_rank_requires_19_calibration_runs(self) -> None:
        self.assertEqual(VALIDATOR._minimum_calibration_runs(0.95), 19)
        self.assertEqual(VALIDATOR._conformal_rank(18, 0.95), 19)
        self.assertEqual(VALIDATOR._conformal_rank(19, 0.95), 19)

    def test_publication_ml_fwer_does_not_claim_combined_weight_coverage(
        self,
    ) -> None:
        contract = VALIDATOR._publication_fwer_document()

        self.assertEqual(contract["overall_confidence"], 0.95)
        self.assertEqual(contract["confidence_per_family"], 0.975)
        self.assertEqual(
            contract["scope"], "independent-ml-falsification-diagnostic-only"
        )
        self.assertEqual(
            contract["statistical_weight_coverage"],
            "not-proven-or-upgraded-by-ml",
        )
        self.assertIsNone(contract["combined_overall_confidence_claim"])

        diagnostic = VALIDATOR._diagnostic_fwer_document(0.95)
        self.assertEqual(
            diagnostic["scope"],
            "independent-ml-falsification-diagnostic-only",
        )
        self.assertIsNone(diagnostic["combined_overall_confidence_claim"])

    def test_differential_classification_is_independent_of_numeric_coverage(
        self,
    ) -> None:
        observed, predicted, covered, check = (
            VALIDATOR._evaluate_conformal_conclusion(
                actual_effect=0.09290,
                predicted_effect=0.03276,
                half_width=0.04296,
                margin=0.15,
            )
        )

        self.assertEqual(observed, "equivalent")
        self.assertEqual(predicted, "equivalent")
        self.assertFalse(covered)
        self.assertEqual(check, "supported")

    def test_differential_opposite_classifications_are_contradicted(self) -> None:
        self.assertEqual(
            VALIDATOR._evaluate_conformal_conclusion(
                actual_effect=0.0,
                predicted_effect=0.30,
                half_width=0.01,
                margin=0.15,
            )[-1],
            "contradicted",
        )

    def test_differential_overlapping_classification_is_inconclusive(self) -> None:
        self.assertEqual(
            VALIDATOR._evaluate_conformal_conclusion(
                actual_effect=0.0,
                predicted_effect=0.15,
                half_width=0.10,
                margin=0.15,
            )[-1],
            "inconclusive",
        )

    def test_automatic_split_reports_40_run_test_evidence_as_weak(self) -> None:
        observations = VALIDATOR.pair_observations(synthetic_samples(runs=40))

        conformal, predictions = VALIDATOR._split_group_conformal(
            observations,
            confidence=0.95,
            seed=139,
            max_iter=40,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )

        split = conformal["split"]
        self.assertEqual(len(split["train_runs"]), 20)
        self.assertEqual(len(split["calibration_runs"]), 19)
        self.assertEqual(len(split["test_runs"]), 1)
        self.assertTrue(split["leakage_check_passed"])
        self.assertFalse(
            set(split["train_runs"])
            & set(split["calibration_runs"])
        )
        self.assertFalse(set(split["train_runs"]) & set(split["test_runs"]))
        self.assertFalse(
            set(split["calibration_runs"]) & set(split["test_runs"])
        )
        finite = conformal["finite_sample"]
        self.assertEqual(finite["rank"], 19)
        self.assertAlmostEqual(finite["maximum_achievable_finite_coverage"], 0.95)
        self.assertAlmostEqual(finite["guaranteed_coverage_lower_bound"], 0.95)
        self.assertTrue(finite["gate_passed"])
        self.assertIsNotNone(conformal["test"]["run_coverage"])
        self.assertIsNotNone(conformal["test"]["interval_width_ns"])
        self.assertFalse(conformal["test"]["evidence_gate_passed"])
        self.assertFalse(conformal["high_confidence_gate"]["passed"])
        self.assertEqual(len(conformal["structural"]["centers"]), 2)
        self.assertEqual(len(conformal["differential_effects"]["centers"]), 1)
        self.assertEqual(
            conformal["differential_effects"]["conclusion_validation"][
                "comparisons_per_run"
            ],
            1,
        )
        self.assertFalse(
            conformal["differential_effects"]["conclusion_validation"][
                "gate_passed"
            ]
        )
        self.assertIn(
            "practical equivalence margin",
            conformal["structural"]["scale"],
        )
        for row in conformal["structural"]["centers"]:
            self.assertEqual(
                row["conformal_normalizer_ns"],
                row["equivalence_margin_ns"],
            )
        self.assertIn(
            "not a structural weight center",
            conformal["pair_level_diagnostic"]["estimand"],
        )
        self.assertEqual(len(predictions), len(observations))

    def test_60_run_split_has_20_independent_runs_in_each_role(self) -> None:
        observations = VALIDATOR.pair_observations(synthetic_samples(runs=60))

        conformal, _ = VALIDATOR._split_group_conformal(
            observations,
            confidence=0.95,
            seed=149,
            max_iter=30,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )

        split = conformal["split"]
        self.assertEqual(
            (
                len(split["train_runs"]),
                len(split["calibration_runs"]),
                len(split["test_runs"]),
            ),
            (20, 20, 20),
        )
        self.assertTrue(conformal["finite_sample"]["gate_passed"])
        self.assertEqual(conformal["finite_sample"]["quantile_tail_depth"], 1)
        self.assertTrue(
            conformal["finite_sample"]["quantile_is_calibration_maximum"]
        )
        self.assertEqual(conformal["test"]["runs"], 20)
        self.assertIsNotNone(conformal["test"]["run_coverage_wilson_interval"])

    def test_120_run_automatic_split_reserves_80_future_runs(self) -> None:
        observations = VALIDATOR.pair_observations(synthetic_samples(runs=120))

        conformal, _ = VALIDATOR._split_group_conformal(
            observations,
            confidence=0.95,
            seed=151,
            max_iter=20,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )

        self.assertEqual(
            (
                len(conformal["split"]["train_runs"]),
                len(conformal["split"]["calibration_runs"]),
                len(conformal["split"]["test_runs"]),
            ),
            (20, 20, 80),
        )
        self.assertEqual(conformal["finite_sample"]["rank"], 20)
        self.assertTrue(
            conformal["finite_sample"]["quantile_is_calibration_maximum"]
        )

    def test_chronological_split_holds_out_the_latest_runs(self) -> None:
        samples = synthetic_samples(runs=120)
        for row in samples:
            row["run_order"] = int(str(row["run_id"]).rsplit("-", 1)[1])
        observations = VALIDATOR.pair_observations(samples)

        conformal, _ = VALIDATOR._split_group_conformal(
            observations,
            confidence=0.95,
            seed=181,
            max_iter=20,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=(40, 40, 40),
            minimum_train_runs=20,
            minimum_test_runs=20,
            split_strategy="chronological",
        )

        split = conformal["split"]
        self.assertEqual(split["strategy"], "chronological")
        self.assertIsNone(split["seed"])
        self.assertEqual(split["run_order_source"], "explicit-run-order")
        self.assertEqual(split["train_runs"], [f"run-{index}" for index in range(40)])
        self.assertEqual(
            split["calibration_runs"], [f"run-{index}" for index in range(40, 80)]
        )
        self.assertEqual(split["test_runs"], [f"run-{index}" for index in range(80, 120)])

    def test_exact_coverage_gate_rejects_perfect_40_run_coverage(self) -> None:
        interval = VALIDATOR._wilson_interval(40, 40, confidence=0.95)
        self.assertIsNotNone(interval)
        self.assertLess(interval[0], 0.95)
        self.assertFalse(
            VALIDATOR._coverage_evidence_gate(
                40, 40, confidence=0.95, minimum_test_runs=20
            )
        )
        self.assertTrue(
            VALIDATOR._coverage_evidence_gate(
                73, 73, confidence=0.95, minimum_test_runs=20
            )
        )
        self.assertAlmostEqual(
            VALIDATOR._clopper_pearson_lower_bound(
                80, 80, confidence=0.95
            ),
            0.05 ** (1.0 / 80.0),
        )
        self.assertLess(
            VALIDATOR._clopper_pearson_lower_bound(
                79, 80, confidence=0.95
            ),
            0.95,
        )

    def test_complete_validation_requires_random_and_forward_gates(self) -> None:
        result, _ = VALIDATOR.validate_predictions(
            synthetic_samples(runs=60),
            folds=3,
            max_iter=20,
            bootstrap_replicates=49,
            minimum_runs=20,
            conformal_train_runs=20,
            conformal_calibration_runs=20,
            conformal_test_runs=20,
            seed=191,
        )

        components = result["conclusion"]["high_confidence_gate_components"]
        self.assertIn("random_joint_conformal_family", components)
        self.assertIn("chronological_joint_conformal_family", components)
        forward = result["cross_validation"]["chronological_split_conformal"]
        self.assertEqual(forward["split"]["strategy"], "chronological")

    def test_temporal_diagnostics_detect_monotone_drift(self) -> None:
        identity = ("context",)
        centers = {
            f"run-{index}": {identity: float(index)}
            for index in range(20)
        }
        diagnostic = VALIDATOR._temporal_diagnostics(centers)

        self.assertFalse(diagnostic["stable"])
        self.assertGreater(
            diagnostic["contexts"][0]["spearman_run_order"], 0.99
        )
        self.assertGreater(diagnostic["contexts"][0]["lag1_pearson"], 0.99)

    def test_honest_test_residuals_do_not_set_the_conformal_width(self) -> None:
        samples = synthetic_samples(runs=40)
        observations = VALIDATOR.pair_observations(samples)
        conformal, _ = VALIDATOR._split_group_conformal(
            observations,
            confidence=0.95,
            seed=157,
            max_iter=35,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )
        test_run = conformal["split"]["test_runs"][0]
        original_half_width = conformal["calibration"]["half_width_ns"]
        original_test_score = conformal["test"]["run_scores"][test_run]
        original_scales = [
            row["scale_ns"] for row in conformal["structural"]["centers"]
        ]

        contaminated = [dict(row) for row in samples]
        for row in contaminated:
            if row["run_id"] == test_run and row["role"] == "probe":
                row["plugin_thread_cpu_ns"] = float(row["plugin_thread_cpu_ns"]) + (
                    100.0 * int(row["requested_count"])
                )
        contaminated_observations = VALIDATOR.pair_observations(contaminated)
        contaminated_conformal, _ = VALIDATOR._split_group_conformal(
            contaminated_observations,
            confidence=0.95,
            seed=157,
            max_iter=35,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )

        self.assertEqual(
            contaminated_conformal["calibration"]["half_width_ns"],
            original_half_width,
        )
        self.assertEqual(
            [
                row["scale_ns"]
                for row in contaminated_conformal["structural"]["centers"]
            ],
            original_scales,
        )
        self.assertGreater(
            contaminated_conformal["test"]["run_scores"][test_run],
            original_test_score + 90.0,
        )
        self.assertEqual(contaminated_conformal["test"]["run_coverage"], 0.0)

    def test_single_pair_tail_only_expands_pair_diagnostic(self) -> None:
        samples = synthetic_samples(runs=40)
        observations = VALIDATOR.pair_observations(samples)
        conformal, _ = VALIDATOR._split_group_conformal(
            observations,
            confidence=0.95,
            seed=173,
            max_iter=35,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )
        calibration_run = conformal["split"]["calibration_runs"][0]
        original_structural_width = conformal["calibration"]["interval_width_ns"]
        original_pair_width = conformal["pair_level_diagnostic"]["calibration"][
            "interval_width_ns"
        ]

        contaminated = [dict(row) for row in samples]
        for row in contaminated:
            if row["run_id"] == calibration_run and row["role"] == "probe":
                row["plugin_thread_cpu_ns"] = float(row["plugin_thread_cpu_ns"]) + (
                    1_000.0 * int(row["requested_count"])
                )
                break
        contaminated_conformal, _ = VALIDATOR._split_group_conformal(
            VALIDATOR.pair_observations(contaminated),
            confidence=0.95,
            seed=173,
            max_iter=35,
            equivalence_absolute_ns=0.15,
            equivalence_relative=0.10,
            explicit_counts=None,
            minimum_train_runs=20,
            minimum_test_runs=20,
        )
        contaminated_pair_width = contaminated_conformal[
            "pair_level_diagnostic"
        ]["calibration"]["interval_width_ns"]
        contaminated_structural_width = contaminated_conformal["calibration"][
            "interval_width_ns"
        ]

        self.assertGreater(contaminated_pair_width, original_pair_width + 1_900.0)
        self.assertLess(contaminated_structural_width, 2.0)
        self.assertLess(
            abs(contaminated_structural_width - original_structural_width),
            1.0,
        )

    def test_explicit_split_counts_must_be_complete(self) -> None:
        with self.assertRaisesRegex(
            VALIDATOR.MlValidationError, "必须同时给出"
        ):
            VALIDATOR.validate_predictions(
                synthetic_samples(runs=4),
                folds=2,
                max_iter=20,
                bootstrap_replicates=9,
                minimum_runs=4,
                conformal_train_runs=2,
                seed=163,
            )


class PublicationFinalizationTests(unittest.TestCase):
    RUNS = VALIDATOR.PUBLICATION_SUPER_RUNS
    TRAIN_RUNS = VALIDATOR.PUBLICATION_TRAIN_SUPER_RUNS
    CALIBRATION_RUNS = VALIDATOR.PUBLICATION_CALIBRATION_SUPER_RUNS
    TEST_RUNS = VALIDATOR.PUBLICATION_TEST_SUPER_RUNS
    CONFIDENCE = VALIDATOR.PUBLICATION_FAMILY_CONFIDENCE
    PAIRS_PER_RUN = 24

    def setUp(self) -> None:
        self._real_publication_replay = (
            VALIDATOR._replay_publication_validation
        )
        self._replay_validation: dict[str, object] | None = None
        self._replay_predictions: list[dict[str, object]] = []
        self._replay_calls: list[dict[str, object]] = []
        self._statistical_replay_document: dict[str, object] | None = None
        self._statistical_replay_calls: list[dict[str, object]] = []
        self._replay_patcher = unittest.mock.patch.object(
            VALIDATOR,
            "_replay_publication_validation",
            side_effect=self._replay,
        )
        self._replay_patcher.start()
        self.addCleanup(self._replay_patcher.stop)
        self._statistical_replay_patcher = unittest.mock.patch.object(
            VALIDATOR,
            "_replay_publication_statistical_model",
            side_effect=self._replay_statistical,
        )
        self._statistical_replay_patcher.start()
        self.addCleanup(self._statistical_replay_patcher.stop)

    def _replay_statistical(self, samples, *, worker_processes):
        self._statistical_replay_calls.append(
            {"sample_rows": len(samples), "worker_processes": worker_processes}
        )
        if self._statistical_replay_document is None:
            raise AssertionError("fixture did not install statistical replay document")
        return json.loads(json.dumps(self._statistical_replay_document))

    def _replay(self, samples, statistical, *, input_bindings):
        self._replay_calls.append(
            {
                "sample_rows": len(samples),
                "statistical_keys": set(statistical),
                "input_bindings": dict(input_bindings),
            }
        )
        if self._replay_validation is None:
            raise AssertionError("fixture did not install replay validation")
        return (
            json.loads(json.dumps(self._replay_validation)),
            json.loads(json.dumps(self._replay_predictions)),
        )

    def _samples(self) -> list[dict[str, object]]:
        return versioned_differential_samples(self.RUNS)

    def _weights(self) -> dict[str, object]:
        key = {
            "mnemonic": "add",
            "size": 4,
            "semantic_encoding_key": "rv64:32:i:add",
            "encoding_key": "raw:4:3305b500",
            "bytes": "3305b500",
            "pattern": "dependency-chain",
        }
        components = {
            name: True for name in VALIDATOR.REQUIRED_PUBLICATION_COMPONENTS
        }
        components["ml_validation"] = False
        influence_rows = [
            {
                "omitted_super_run": f"run-{index}",
                "ns_per_instruction": 0.55,
                "full_estimate_ns_per_instruction": 0.55,
                "shift_ns": 0.0,
            }
            for index in range(self.RUNS)
        ]
        family_confidence = 0.99375
        finite_evidence = {
            "method": MONTE_CARLO_METHOD,
            "target_probability": family_confidence,
            "monte_carlo_confidence": family_confidence,
            "replicates": 4000,
            "required_rank": 3988,
            "selected_rank": 3988,
            "finite_rank_supported": True,
            "replicate_partition_method": REPLICATE_PARTITION_METHOD,
            "complete_family_replicates": 4999,
            "scale_replicates": 999,
            "quantile_replicates": 4000,
        }
        return {
            "confidence": 0.95,
            "generation_configuration": (
                VALIDATOR.publication_generation_configuration()
            ),
            "publication_familywise_error_control": {
                "method": FWER_METHOD,
                "overall_confidence": 0.95,
                "overall_alpha": 0.05,
                "sampling_alpha_budget": 0.025,
                "monte_carlo_alpha_budget": 0.025,
                "families": [
                    "raw-absolute-costs",
                    "diagnostic-nuisance-effects",
                    "auxiliary-clock-consistency",
                    "joint-adjusted-anchor-sensitivity",
                ],
                "family_count": 4,
                "sampling_alpha_per_family": 0.00625,
                "sampling_confidence_per_family": family_confidence,
                "monte_carlo_alpha_per_family": 0.00625,
                "monte_carlo_confidence_per_family": family_confidence,
                "coverage_claim": FWER_COVERAGE_CLAIM,
            },
            "instructions": [
                {
                    "key": key,
                    "quality": "high-confidence",
                    "calibration_only": False,
                    "ns_per_instruction": 0.55,
                    "simultaneous_ci": [0.50, 0.60],
                    "published_ns_per_instruction": 0.55,
                    "anchor_adjusted": {
                        "ns_per_instruction": 0.55,
                        "simultaneous_ci": [0.50, 0.60],
                    },
                    "raw_adjusted_discrepancy": {
                        "simultaneous_ci": [-0.01, 0.01],
                        "equivalence_margin_ns": 0.15,
                        "equivalent": True,
                    },
                    "estimator_sensitivity": {
                        "simultaneous_ci": [-0.01, 0.01],
                        "equivalence_margin_ns": 0.15,
                        "equivalent": True,
                    },
                    "leave_one_super_run_out_sensitivity": {
                        "complete": True,
                        "runs": self.RUNS,
                        "maximum_absolute_shift_ns": 0.0,
                        "equivalence_margin_ns": 0.15,
                        "failed_super_runs": [],
                        "per_super_run": influence_rows,
                        "stable": True,
                    },
                }
            ],
            "positive_anchor_scale_inference": {
                "status": "accepted",
                "method": "fixture-joint-anchor-scale-inference",
                "anchor_key": {
                    "semantic_encoding_key": "rv64:32:m:div",
                    "encoding_key": "raw:4:3305b502",
                    "pattern": "stability-anchor-positive-div",
                },
                "complete_super_runs": self.RUNS,
                "primary_anchor_ns_per_instruction": 3.6,
                "plugin_off_to_primary_scale": 1.0,
                "plugin_off_to_primary_scale_simultaneous_ci": [0.99, 1.01],
                "scale_interval_ratio": 1.0202020202020203,
                "maximum_scale_interval_ratio": 1.10,
                "nuisance_interval_gate_passed": True,
            },
            "simultaneous_inference": {
                "requested_replicates": 4999,
                "worker_processes": 16,
                "familywise_confidence": family_confidence,
                "complete_family_replicates": 4999,
                "complete_max_statistic_replicates": 4000,
                "critical_value_monte_carlo": dict(finite_evidence),
            },
            "diagnostic_simultaneous_inference": {
                "familywise_confidence": family_confidence,
                "requested_replicates": 4999,
                "complete_replicates": 4000,
                "complete_family_replicates": 4999,
                "critical_value_monte_carlo": dict(finite_evidence),
            },
            "auxiliary_consistency_inference": {
                "familywise_confidence": family_confidence,
                "requested_replicates": 4999,
                "valid_replicates": 4000,
                "complete_family_replicates": 4999,
                "critical_value_monte_carlo": dict(finite_evidence),
            },
            "joint_raw_adjusted_inference": {
                "requested_replicates": 4999,
                "complete_replicates": 4999,
                "complete_family_replicates": 4999,
                "familywise_confidence": family_confidence,
                "complete_max_statistic_replicates": 4000,
                "critical_value_monte_carlo": dict(finite_evidence),
            },
            "host_isolation_audit": {
                "schema": "mygo.riscv-weight-host-audit.v1",
                "status": "accepted",
            },
            "host_isolation_audit_source": "current",
            "host_isolation_audit_binding": {
                "schema": "mygo.riscv-weight-host-audit-binding.v1",
                "source": "current",
                "publication_allowed": True,
            },
            "publication_gate": {
                "passed": False,
                "failures": ["ml-validation-missing"],
                "publishable_contexts": 1,
                "components": components,
                "statistical_core_passed": True,
            }
        }

    def _temporal(self) -> dict[str, object]:
        return {
            "contexts": [
                {
                    "identity": ["context"],
                    "spearman_run_order": 0.0,
                    "lag1_pearson": 0.0,
                    "trend_threshold": 0.30,
                    "dependence_threshold": 0.30,
                    "stable": True,
                }
            ],
            "stable": True,
            "failed_contexts": [],
        }

    def _layer(
        self, calibration_runs: list[str], test_runs: list[str], *, differential: bool
    ) -> dict[str, object]:
        calibration_scores = {run: 0.05 for run in calibration_runs}
        test_scores = {run: 0.05 for run in test_runs}
        rank = VALIDATOR._conformal_rank(
            len(calibration_runs), self.CONFIDENCE
        )
        quantile = 0.05
        layer: dict[str, object] = {
            "status": "calibrated",
            "finite_sample": {
                "calibration_runs": len(calibration_runs),
                "rank": rank,
                "gate_passed": True,
            },
            "calibration": {
                "run_scores": calibration_scores,
                "standardized_quantile": quantile,
                "sharpness_gate_passed": True,
            },
            "centers": [
                {
                    "identity": ["context"],
                    "conformal_normalizer_ns": 0.15,
                    "equivalence_margin_ns": 0.15,
                    "half_width_ns": quantile * 0.15,
                }
            ],
            "temporal_diagnostics": self._temporal(),
            "test": {
                "runs": len(test_runs),
                "covered_runs": len(test_runs),
                "run_coverage": 1.0,
                "run_scores": test_scores,
                "evidence_gate_passed": True,
            },
        }
        if differential:
            details = [
                {
                    "run_id": run,
                    "actual_effect_ns": 0.0,
                    "predicted_effect_ns": 0.0,
                    "half_width_ns": 0.01,
                    "equivalence_margin_ns": 0.15,
                    "observed_conclusion": "equivalent",
                    "conformal_interval_conclusion": "equivalent",
                    "actual_covered": True,
                    "conclusion_check": "supported",
                }
                for run in test_runs
            ]
            layer["conclusion_validation"] = {
                "status": "supported",
                "test_runs": len(test_runs),
                "comparisons_per_run": 1,
                "supported": len(details),
                "inconclusive": 0,
                "contradicted": 0,
                "gate_passed": True,
                "details": details,
            }
        return layer

    def _conformal(self, runs: list[str], *, chronological: bool) -> dict[str, object]:
        train = runs[: self.TRAIN_RUNS]
        calibration = runs[
            self.TRAIN_RUNS : self.TRAIN_RUNS + self.CALIBRATION_RUNS
        ]
        test = runs[self.TRAIN_RUNS + self.CALIBRATION_RUNS :]
        pairs_per_run = self.PAIRS_PER_RUN
        checks = {
            name: True
            for name in (
                VALIDATOR.REQUIRED_CHRONOLOGICAL_CHECKS
                if chronological
                else VALIDATOR.REQUIRED_CONFORMAL_CHECKS
            )
        }
        structural = self._layer(calibration, test, differential=False)
        differential = self._layer(calibration, test, differential=True)
        joint_calibration_scores = {run: 0.05 for run in calibration}
        joint_test_scores = {run: 0.05 for run in test}
        quantile = 0.05
        for layer in (structural, differential):
            for center in layer["centers"]:
                center["joint_family_half_width_ns"] = (
                    quantile * center["conformal_normalizer_ns"]
                )
        joint_details = [dict(row) for row in differential["conclusion_validation"]["details"]]
        for row in joint_details:
            row["half_width_ns"] = quantile * row["equivalence_margin_ns"]
        joint_family = {
            "schema": "mygo.riscv-instruction-ml-joint-conformal-family.v1",
            "family": VALIDATOR.PUBLICATION_CONFORMAL_FAMILIES[chronological],
            "combination": (
                "per-super-run maximum standardized nonconformity across layers"
            ),
            "included_layers": ["structural", "differential"],
            "target_coverage": self.CONFIDENCE,
            "alpha": 1.0 - self.CONFIDENCE,
            "finite_sample": {
                "calibration_runs": len(calibration),
                "rank": VALIDATOR._conformal_rank(
                    len(calibration), self.CONFIDENCE
                ),
                "maximum_achievable_finite_coverage": (
                    len(calibration) / (len(calibration) + 1)
                ),
                "guaranteed_coverage_lower_bound": self.CONFIDENCE,
                "gate_passed": True,
            },
            "calibration": {
                "layer_run_scores": {
                    "structural": dict(joint_calibration_scores),
                    "differential": dict(joint_calibration_scores),
                },
                "run_scores": dict(joint_calibration_scores),
                "standardized_quantile": quantile,
                "sharpness_gate_passed": True,
                "maximum_interval_width_ns": 2.0 * quantile * 0.15,
            },
            "test": {
                "runs": len(test),
                "layer_run_scores": {
                    "structural": dict(joint_test_scores),
                    "differential": dict(joint_test_scores),
                },
                "run_scores": dict(joint_test_scores),
                "covered_runs": len(test),
                "run_coverage": 1.0,
                "run_coverage_clopper_pearson_one_sided_lower": (
                    VALIDATOR._clopper_pearson_lower_bound(
                        len(test), len(test), confidence=self.CONFIDENCE
                    )
                ),
                "evidence_gate_passed": True,
            },
            "differential_conclusion_validation": {
                "status": "supported",
                "test_runs": len(test),
                "comparisons_per_run": 1,
                "supported": len(joint_details),
                "inconclusive": 0,
                "contradicted": 0,
                "gate_passed": True,
                "details": joint_details,
            },
        }
        return {
            "split_strategy": "chronological" if chronological else "random",
            "target_coverage": self.CONFIDENCE,
            "required_minimum_train_runs_for_high_confidence": (
                VALIDATOR.PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
            ),
            "required_minimum_test_runs_for_high_confidence": (
                VALIDATOR.PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS
            ),
            "split": {
                "strategy": "chronological" if chronological else "random",
                "train_runs": train,
                "calibration_runs": calibration,
                "test_runs": test,
                "train_pairs": len(train) * pairs_per_run,
                "calibration_pairs": len(calibration) * pairs_per_run,
                "test_pairs": len(test) * pairs_per_run,
                "leakage_check_passed": True,
            },
            "finite_sample": {
                "calibration_runs": len(calibration),
                "gate_passed": True,
            },
            "calibration": {
                "run_scores": {run: 0.05 for run in calibration},
            },
            "test": {
                "runs": len(test),
                "covered_runs": len(test),
                "run_coverage": 1.0,
                "run_scores": {run: 0.05 for run in test},
                "evidence_gate_passed": True,
            },
            "structural": structural,
            "differential_effects": differential,
            "joint_family": joint_family,
            "high_confidence_gate": {
                "checks": checks,
                "failed_checks": [],
                "passed": True,
            },
        }

    def _validation(
        self, samples_path: Path, weights_path: Path
    ) -> dict[str, object]:
        runs = [f"run-{index}" for index in range(self.RUNS)]
        folds = []
        for index in range(VALIDATOR.PUBLICATION_FOLDS):
            test = runs[index:: VALIDATOR.PUBLICATION_FOLDS]
            train = [run for run in runs if run not in set(test)]
            folds.append(
                {
                    "fold": index + 1,
                    "train_runs": train,
                    "test_runs": test,
                    "train_pairs": len(train) * self.PAIRS_PER_RUN,
                    "test_pairs": len(test) * self.PAIRS_PER_RUN,
                }
            )
        components = {
            "random_joint_conformal_family": True,
            "chronological_joint_conformal_family": True,
        }
        self._replay_predictions = [
            {"pair_index": index}
            for index in range(self.RUNS * self.PAIRS_PER_RUN)
        ]
        return {
            "schema": VALIDATOR.OUTPUT_SCHEMA,
            "data": {
                "runs": self.RUNS,
                "super_run_ids": runs,
                "pairs": self.RUNS * self.PAIRS_PER_RUN,
            },
            "configuration": {
                "folds_requested": VALIDATOR.PUBLICATION_FOLDS,
                "max_iter": VALIDATOR.PUBLICATION_MAX_ITER,
                "confidence": self.CONFIDENCE,
                "bootstrap_replicates": (
                    VALIDATOR.PUBLICATION_BOOTSTRAP_REPLICATES
                ),
                "minimum_independent_super_runs": (
                    VALIDATOR.PUBLICATION_MINIMUM_RUNS
                ),
                "minimum_skill_improvement_over_context_batch": (
                    VALIDATOR.PUBLICATION_MINIMUM_SKILL_IMPROVEMENT
                ),
                "omitted_structure_equivalence_ns": (
                    VALIDATOR.PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS
                ),
                "equivalence_absolute_ns": (
                    VALIDATOR.PUBLICATION_EQUIVALENCE_ABSOLUTE_NS
                ),
                "equivalence_relative": (
                    VALIDATOR.PUBLICATION_EQUIVALENCE_RELATIVE
                ),
                "conformal_explicit_run_counts": {
                    "train": self.TRAIN_RUNS,
                    "calibration": self.CALIBRATION_RUNS,
                    "test": self.TEST_RUNS,
                },
                "conformal_minimum_train_runs": (
                    VALIDATOR.PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
                ),
                "conformal_minimum_test_runs": (
                    VALIDATOR.PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS
                ),
                "seed": VALIDATOR.PUBLICATION_SEED,
            },
            "publication_policy": VALIDATOR._publication_policy_document(),
            "publication_familywise_error_control": (
                VALIDATOR._publication_fwer_document()
            ),
            "prediction_evidence": VALIDATOR._prediction_evidence(
                self._replay_predictions
            ),
            "input_bindings": {
                "samples": VALIDATOR._artifact_identity(samples_path),
                "statistical_weights_pre_finalization": (
                    VALIDATOR._artifact_identity(weights_path)
                ),
            },
            "cross_validation": {
                "available": True,
                "folds": folds,
                "incremental_value": {
                    "status": "available",
                    "role": "diagnostic-only",
                    "formal_gate": False,
                    "mae_improvement_run_cluster_ci": [-0.05, 0.05],
                    "practical_equivalence_ns": 0.15,
                    "gate_passed": None,
                    "diagnostic_equivalence_passed": True,
                    "training_uncertainty_included": False,
                    "interpretation": "no-practically-material-omitted-structure",
                },
                "split_conformal": self._conformal(runs, chronological=False),
                "chronological_split_conformal": self._conformal(
                    runs, chronological=True
                ),
            },
            "contexts": [
                {
                    "semantic_key": "rv64:32:i:add",
                    "raw_key": "raw:4:3305b500",
                    "pattern": "dependency-chain",
                    "runs": self.RUNS,
                    "ml_bias_cluster_ci": [-0.01, 0.01],
                    "equivalence_margin_ns": 0.15,
                    "conclusion_check": "consistent",
                }
            ],
            "differential_checks": [
                {
                    "observed_effect_ns": 1.6,
                    "observed_effect_cluster_ci": [1.5, 1.7],
                    "observed_conclusion": "context-dependent",
                    "ml_oof_effect_ns": 1.6,
                    "equivalence_margin_ns": 0.15,
                    "ml_conclusion_check": "supported",
                }
            ],
            "conclusion": {
                "status": "supported",
                "high_confidence_status": "supported",
                "high_confidence_gate_passed": True,
                "high_confidence_gate_components": components,
                "diagnostic_status": "supported",
                "may_publish_weights": False,
                "context_checks": ["consistent"],
                "differential_checks": ["supported"],
            },
        }

    def _fixture(self, directory: str):
        root = Path(directory)
        samples_path = root / "samples.jsonl"
        weights_path = root / "weights.json"
        validation_path = root / "ml-validation.json"
        samples_path.write_text(
            "\n".join(json.dumps(row) for row in self._samples()) + "\n",
            encoding="utf-8",
        )
        weights_document = self._weights()
        self._statistical_replay_document = json.loads(
            json.dumps(weights_document)
        )
        weights_path.write_text(
            json.dumps(weights_document, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        validation_document = self._validation(samples_path, weights_path)
        self._replay_validation = json.loads(json.dumps(validation_document))
        validation_path.write_text(
            json.dumps(validation_document, sort_keys=True)
            + "\n",
            encoding="utf-8",
        )
        return samples_path, weights_path, validation_path

    def test_matching_supported_ml_closes_the_final_gate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)

            self.assertTrue(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )

            document = json.loads(weights.read_text(encoding="utf-8"))
            gate = document["publication_gate"]
            self.assertTrue(gate["passed"])
            self.assertTrue(gate["components"]["ml_validation"])
            self.assertEqual(gate["failures"], [])
            self.assertTrue(
                document["ml_validation_evidence"]["checks"][
                    "input_bindings"
                ]
            )
            self.assertTrue(
                document["ml_validation_evidence"]["checks"][
                    "deterministic_full_replay"
                ]
            )
            self.assertEqual(len(self._replay_calls), 1)
            self.assertEqual(len(self._statistical_replay_calls), 1)
            self.assertTrue(
                document["ml_validation_evidence"]["checks"][
                    "statistical_full_replay"
                ]
            )
            verify_model_document_seal(document)

    def test_incremental_oof_diagnostic_is_excluded_from_formal_gate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            assert self._replay_validation is not None
            document = json.loads(json.dumps(self._replay_validation))
            incremental = document["cross_validation"]["incremental_value"]
            incremental["mae_improvement_run_cluster_ci"] = [0.30, 0.40]
            incremental["diagnostic_equivalence_passed"] = False
            incremental["interpretation"] = (
                "practically-material-omitted-structure-detected"
            )
            conclusion = document["conclusion"]
            conclusion["diagnostic_status"] = (
                "contradicted-practically-material-omitted-structure"
            )
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")
            self._replay_validation = json.loads(json.dumps(document))

            self.assertTrue(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            evidence = result["ml_validation_evidence"]
            self.assertTrue(result["publication_gate"]["components"]["ml_validation"])
            self.assertFalse(
                evidence["recomputed"]["diagnostics"]["incremental_equivalence"]
            )
            self.assertEqual(
                set(evidence["recomputed"]["components"]),
                VALIDATOR.REQUIRED_ML_GATE_COMPONENTS,
            )

    def test_finalizer_rejects_old_unsplit_fwer_contract(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(weights.read_text(encoding="utf-8"))
            document["publication_familywise_error_control"] = {
                "method": (
                    "bonferroni-across-pre-registered-max-stat-families"
                ),
                "overall_confidence": 0.95,
                "overall_alpha": 0.05,
                "families": [
                    "raw-absolute-costs",
                    "diagnostic-nuisance-effects",
                    "auxiliary-clock-consistency",
                    "joint-adjusted-anchor-sensitivity",
                ],
                "family_count": 4,
                "alpha_per_family": 0.0125,
                "confidence_per_family": 0.9875,
                "coverage_claim": "conditional old claim",
            }
            weights.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertFalse(result["publication_gate"]["passed"])
            self.assertNotIn("publication_seal", result)

    def test_publication_replay_uses_only_registered_parameters(self) -> None:
        samples = self._samples()
        with unittest.mock.patch.object(
            VALIDATOR,
            "validate_predictions",
            return_value=({"schema": VALIDATOR.OUTPUT_SCHEMA}, []),
        ) as replay:
            result, predictions = self._real_publication_replay(
                samples,
                {},
                input_bindings={"samples": {}, "statistical_weights_pre_finalization": {}},
            )

        self.assertEqual(predictions, [])
        self.assertEqual(
            result["publication_policy"], VALIDATOR._publication_policy_document()
        )
        keyword = replay.call_args.kwargs
        self.assertEqual(
            keyword,
            {
                "statistical_weights": {},
                "input_bindings": {
                    "samples": {},
                    "statistical_weights_pre_finalization": {},
                },
                "folds": VALIDATOR.PUBLICATION_FOLDS,
                "max_iter": VALIDATOR.PUBLICATION_MAX_ITER,
                "confidence": VALIDATOR.PUBLICATION_FAMILY_CONFIDENCE,
                "bootstrap_replicates": (
                    VALIDATOR.PUBLICATION_BOOTSTRAP_REPLICATES
                ),
                "minimum_runs": VALIDATOR.PUBLICATION_MINIMUM_RUNS,
                "minimum_skill_improvement": (
                    VALIDATOR.PUBLICATION_MINIMUM_SKILL_IMPROVEMENT
                ),
                "omitted_structure_equivalence_ns": (
                    VALIDATOR.PUBLICATION_OMITTED_STRUCTURE_EQUIVALENCE_NS
                ),
                "equivalence_absolute_ns": (
                    VALIDATOR.PUBLICATION_EQUIVALENCE_ABSOLUTE_NS
                ),
                "equivalence_relative": (
                    VALIDATOR.PUBLICATION_EQUIVALENCE_RELATIVE
                ),
                "conformal_train_runs": VALIDATOR.PUBLICATION_TRAIN_SUPER_RUNS,
                "conformal_calibration_runs": (
                    VALIDATOR.PUBLICATION_CALIBRATION_SUPER_RUNS
                ),
                "conformal_test_runs": VALIDATOR.PUBLICATION_TEST_SUPER_RUNS,
                "conformal_minimum_train_runs": (
                    VALIDATOR.PUBLICATION_CONFORMAL_MINIMUM_TRAIN_RUNS
                ),
                "conformal_minimum_test_runs": (
                    VALIDATOR.PUBLICATION_CONFORMAL_MINIMUM_TEST_RUNS
                ),
                "seed": VALIDATOR.PUBLICATION_SEED,
            },
        )

    def test_internally_consistent_fabricated_interval_fails_replay(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(validation.read_text(encoding="utf-8"))
            incremental = document["cross_validation"]["incremental_value"]
            # 仍完整落在等价带内，所有缓存 gate/结论依然内部自洽；旧 finalizer
            # 会接受它，但它不是绑定 samples 在固定政策下的重放结果。
            incremental["mae_improvement_run_cluster_ci"] = [-0.04, 0.04]
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertFalse(
                result["ml_validation_evidence"]["checks"][
                    "deterministic_full_replay"
                ]
            )

    def test_publication_rejects_bootstrap_below_999(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(validation.read_text(encoding="utf-8"))
            document["configuration"]["bootstrap_replicates"] = 998
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertFalse(
                result["ml_validation_evidence"]["checks"][
                    "fixed_publication_policy"
                ]
            )

    def test_changed_samples_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            rows = VALIDATOR.load_samples(samples)
            rows[0]["plugin_thread_cpu_ns"] = float(
                rows[0]["plugin_thread_cpu_ns"]
            ) + 1.0
            samples.write_text(
                "\n".join(json.dumps(row) for row in rows) + "\n",
                encoding="utf-8",
            )

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )

            gate = json.loads(weights.read_text(encoding="utf-8"))[
                "publication_gate"
            ]
            self.assertFalse(gate["components"]["ml_validation"])
            self.assertIn("ml-validation-rejected", gate["failures"])

    def test_changed_weights_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(weights.read_text(encoding="utf-8"))
            document["extra"] = "changed-after-validation"
            weights.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )

    def test_inconsistent_ml_subgate_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(validation.read_text(encoding="utf-8"))
            document["conclusion"]["high_confidence_gate_components"][
                "chronological_split_conformal"
            ] = False
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )

    def test_fabricated_conclusion_booleans_cannot_override_nested_failure(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(validation.read_text(encoding="utf-8"))
            split = document["cross_validation"]["split_conformal"]
            split["structural"]["calibration"][
                "sharpness_gate_passed"
            ] = False
            # 保留所有外层布尔为 True，模拟只篡改/伪造 summary 的攻击。
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertFalse(result["publication_gate"]["components"]["ml_validation"])
            self.assertFalse(
                result["ml_validation_evidence"]["checks"]["detailed_evidence"]
            )

    def test_missing_cross_validation_detail_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(validation.read_text(encoding="utf-8"))
            del document["cross_validation"]["chronological_split_conformal"]
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )

    def test_empty_instruction_set_is_rejected_even_with_all_true_components(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(weights.read_text(encoding="utf-8"))
            document["instructions"] = []
            document["publication_gate"]["publishable_contexts"] = 0
            weights.write_text(json.dumps(document) + "\n", encoding="utf-8")
            bound = json.loads(validation.read_text(encoding="utf-8"))
            bound["input_bindings"]["statistical_weights_pre_finalization"] = (
                VALIDATOR._artifact_identity(weights)
            )
            validation.write_text(json.dumps(bound) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertIn(
                "statistical-detail-rejected",
                result["publication_gate"]["failures"],
            )

    def test_tampered_instruction_component_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(weights.read_text(encoding="utf-8"))
            document["instructions"][0]["estimator_sensitivity"][
                "simultaneous_ci"
            ] = [0.20, 0.30]
            weights.write_text(json.dumps(document) + "\n", encoding="utf-8")
            bound = json.loads(validation.read_text(encoding="utf-8"))
            bound["input_bindings"]["statistical_weights_pre_finalization"] = (
                VALIDATOR._artifact_identity(weights)
            )
            validation.write_text(json.dumps(bound) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertIn(
                "statistical-detail-rejected",
                result["publication_gate"]["failures"],
            )

    def _assert_statistical_tamper_is_rejected(self, mutate) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(weights.read_text(encoding="utf-8"))
            mutate(document)
            weights.write_text(json.dumps(document) + "\n", encoding="utf-8")
            bound = json.loads(validation.read_text(encoding="utf-8"))
            bound["input_bindings"]["statistical_weights_pre_finalization"] = (
                VALIDATOR._artifact_identity(weights)
            )
            validation.write_text(json.dumps(bound) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            result = json.loads(weights.read_text(encoding="utf-8"))
            self.assertIn(
                "statistical-detail-rejected",
                result["publication_gate"]["failures"],
            )
            self.assertFalse(
                result["ml_validation_evidence"]["recomputed"][
                    "statistical_full_replay"
                ]["matched"]
            )

    def test_tampered_raw_point_outside_interval_is_rejected_by_full_replay(
        self,
    ) -> None:
        self._assert_statistical_tamper_is_rejected(
            lambda document: document["instructions"][0].__setitem__(
                "ns_per_instruction", 1.0e9
            )
        )

    def test_tampered_adjusted_interval_is_rejected_by_full_replay(self) -> None:
        self._assert_statistical_tamper_is_rejected(
            lambda document: document["instructions"][0]["anchor_adjusted"].__setitem__(
                "simultaneous_ci", [100.0, 101.0]
            )
        )

    def test_status_only_positive_anchor_is_rejected_by_full_replay(self) -> None:
        def mutate(document):
            document["positive_anchor_scale_inference"] = {"status": "accepted"}

        self._assert_statistical_tamper_is_rejected(mutate)

    def test_tampered_leave_one_run_shift_is_rejected_by_full_replay(self) -> None:
        self._assert_statistical_tamper_is_rejected(
            lambda document: document["instructions"][0][
                "leave_one_super_run_out_sensitivity"
            ]["per_super_run"][0].__setitem__("shift_ns", 1.0e9)
        )

    def test_non_registered_statistical_configuration_is_rejected(self) -> None:
        def mutate(document):
            document["generation_configuration"]["seed"] += 1

        self._assert_statistical_tamper_is_rejected(mutate)

    def test_conflicting_statistical_core_fields_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(weights.read_text(encoding="utf-8"))
            document["publication_gate"]["components"][
                "statistical_core"
            ] = False
            weights.write_text(json.dumps(document) + "\n", encoding="utf-8")
            bound = json.loads(validation.read_text(encoding="utf-8"))
            bound["input_bindings"]["statistical_weights_pre_finalization"] = (
                VALIDATOR._artifact_identity(weights)
            )
            validation.write_text(json.dumps(bound) + "\n", encoding="utf-8")

            self.assertFalse(
                VALIDATOR.finalize_publication_gate(
                    weights_path=weights,
                    samples_path=samples,
                    validation_path=validation,
                )
            )
            gate = json.loads(weights.read_text(encoding="utf-8"))[
                "publication_gate"
            ]
            self.assertIn("statistical-detail-rejected", gate["failures"])

    def test_finalize_cli_returns_nonzero_for_rejected_publication(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            samples, weights, validation = self._fixture(directory)
            document = json.loads(validation.read_text(encoding="utf-8"))
            document["conclusion"]["high_confidence_status"] = "inconclusive"
            validation.write_text(json.dumps(document) + "\n", encoding="utf-8")

            with unittest.mock.patch.object(
                VALIDATOR,
                "validate_predictions",
                return_value=(document, []),
            ):
                self.assertEqual(
                    VALIDATOR.main(
                        [
                            str(samples),
                            "--weights",
                            str(weights),
                            "--output",
                            str(validation),
                            "--finalize-weights",
                        ]
                    ),
                    1,
                )


if __name__ == "__main__":
    unittest.main()
