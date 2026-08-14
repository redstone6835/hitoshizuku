"""RISC-V 指令微基准权重模型单元测试。"""

from __future__ import annotations

import json
import math
import random
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import scripts.rv_instruction_microbench_model as MODEL
from scripts.rv_instruction_microbench_model import (
    MicrobenchmarkModelError,
    fit_microbenchmark_weight_model,
    load_samples,
    write_csv,
)


def synthetic_samples(seed: int = 19) -> list[dict[str, object]]:
    """生成三次独立 run、随机 AB/BA、三档 batch 和短程相关噪声。"""

    rng = random.Random(seed)
    variants = (
        ("nop", 4, "13000000", "throughput", 0.80, None),
        ("addi", 4, "13051500", "throughput", 1.70, ("nop", 4)),
        ("c.addi", 2, "0505", "throughput", 1.58, ("nop", 4)),
    )
    samples: list[dict[str, object]] = []
    sequence = 0
    for run_index in range(3):
        run = f"cold-{run_index}"
        run_shift = (-0.018, 0.0, 0.021)[run_index]
        correlated: dict[tuple[str, int], float] = {
            (name, size): 0.0
            for name, size, _encoding, _pattern, _weight, _control in variants
        }
        for round_index in range(36):
            shuffled = list(variants)
            rng.shuffle(shuffled)
            for name, size, encoding, pattern, absolute_weight, control in shuffled:
                batch = (12_000, 24_000, 48_000)[round_index % 3]
                control_weight = 0.0 if control is None else 0.80 + run_shift
                contrast = absolute_weight + run_shift - control_weight
                key = (name, size)
                innovation = rng.gauss(0.0, 65.0)
                correlated[key] = 0.35 * correlated[key] + innovation
                pair_noise = correlated[key]
                if name == "addi" and run_index == 1 and round_index == 17:
                    pair_noise += 22_000.0
                target_count = batch
                other = max(1, batch // 500)
                common = 2_000_000.0 + batch * 0.45 + round_index * 3.0
                baseline_cpu = common + rng.gauss(0.0, 25.0)
                probe_cpu = common + contrast * target_count + pair_noise
                baseline_guest = baseline_cpu * 1.10 + rng.gauss(0.0, 40.0)
                probe_guest = probe_cpu * 1.10 + rng.gauss(0.0, 40.0)
                probe_first = rng.random() < 0.5
                pair_id = f"{run}-{round_index}-{name}-{size}"
                common_fields: dict[str, object] = {
                    "run_id": run,
                    "block": round_index,
                    "segment_id": pair_id,
                    "instruction": name,
                    "encoding_bytes": size,
                    "pattern": pattern,
                    "batch": batch,
                    "timer_reads": 2,
                    "plugin_mode": "timing",
                    "translations_during_window": 0,
                    "target_descriptor": {
                        "size": size,
                        "bytes": encoding,
                        "mnemonic": name,
                    },
                }
                if control is None:
                    common_fields["baseline_kind"] = "empty"
                else:
                    common_fields["baseline_descriptor"] = {
                        "size": control[1],
                        "bytes": "13000000",
                        "mnemonic": control[0],
                    }
                    common_fields["control_pattern"] = "throughput"
                windows = (
                    (
                        "probe",
                        probe_cpu,
                        probe_guest,
                        target_count,
                        target_count + other,
                    ),
                    ("baseline", baseline_cpu, baseline_guest, 0, other),
                )
                if not probe_first:
                    windows = tuple(reversed(windows))
                for role, cpu_ns, guest_ns, exact_target, total in windows:
                    row = dict(common_fields)
                    row.update(
                        {
                            "role": role,
                            "sequence": sequence,
                            "plugin_thread_cpu_ns": cpu_ns,
                            "guest_ns": guest_ns,
                            "plugin_off_guest_ns": guest_ns,
                            "target_count": exact_target,
                            "total_instruction_count": total,
                        }
                    )
                    sequence += 1
                    samples.append(row)
    return samples


class RobustWeightModelTests(unittest.TestCase):
    """验证绝对 control 链、稳健斜率、编码拆分和统计元数据。"""

    @classmethod
    def setUpClass(cls) -> None:
        cls.result = fit_microbenchmark_weight_model(
            synthetic_samples(),
            bootstrap_replicates=79,
            seed=71,
            min_pairs=30,
            min_effective_pairs=20,
        )
        cls.items = {
            (
                item["key"]["mnemonic"],
                item["key"]["size"],
                item["key"]["pattern"],
            ): item
            for item in cls.result["instructions"]
        }

    def test_recovers_absolute_weights_through_control_chain(self) -> None:
        self.assertAlmostEqual(
            self.items[("nop", 4, "throughput")]["ns_per_instruction"],
            0.80,
            delta=0.04,
        )
        self.assertAlmostEqual(
            self.items[("addi", 4, "throughput")]["ns_per_instruction"],
            1.70,
            delta=0.06,
        )
        self.assertAlmostEqual(
            self.items[("c.addi", 2, "throughput")]["ns_per_instruction"],
            1.58,
            delta=0.06,
        )

    def test_reports_simultaneous_and_pointwise_intervals(self) -> None:
        for item in self.items.values():
            self.assertEqual(len(item["simultaneous_ci"]), 2)
            self.assertEqual(len(item["point_ci"]), 2)
            self.assertLessEqual(
                item["point_ci"][1] - item["point_ci"][0],
                item["simultaneous_ci"][1] - item["simultaneous_ci"][0],
            )
            self.assertGreater(item["ESS"], 0.0)
            self.assertEqual(item["runs"], 3)
            self.assertEqual(item["pairs"], 108)
            self.assertGreaterEqual(item["purity_q05"], 0.99)
            self.assertIn("insufficient-bootstrap-replicates", item["quality_failures"])

    def test_keeps_two_and_four_byte_variants_separate(self) -> None:
        self.assertIn(("addi", 4, "throughput"), self.items)
        self.assertIn(("c.addi", 2, "throughput"), self.items)
        self.assertEqual(
            self.result["instruction_key"],
            "raw-encoding+semantic-decoding+execution-pattern",
        )
        json.dumps(self.result, allow_nan=False)

    def test_primary_and_guest_responses_are_distinguished(self) -> None:
        addi = self.items[("addi", 4, "throughput")]
        self.assertEqual(
            addi["source"], "qemu-vcpu-thread-cpu-time-marker-only"
        )
        self.assertIsNotNone(addi["guest_time_check"])
        self.assertAlmostEqual(
            addi["guest_time_check"]["ratio_to_primary_absolute"],
            1.10,
            delta=0.08,
        )
        self.assertAlmostEqual(
            addi["plugin_off_check"]["timing_plugin_to_plugin_off_ratio"],
            1.0,
            delta=0.03,
        )

    def test_zero_cost_auxiliary_checks_preserve_within_pair_covariance(
        self,
    ) -> None:
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        for row in rows:
            common_noise = (int(row["sequence"]) % 17 - 8) * 10_000.0
            row["guest_ns"] = float(row["guest_ns"]) + common_noise
            row["plugin_off_guest_ns"] = (
                float(row["plugin_off_guest_ns"]) + common_noise
            )

        result = fit_microbenchmark_weight_model(
            rows,
            bootstrap_replicates=999,
            seed=1979,
            max_zero_cost_ci_upper_ns=1.0,
        )
        item = result["instructions"][0]
        interval = item["plugin_off_check"][
            "simultaneous_difference_ci"
        ]

        self.assertIsNotNone(interval)
        self.assertLess(interval[1] - interval[0], 0.05)
        self.assertEqual(
            result["auxiliary_consistency_inference"][
                "zero_cost_difference_method"
            ],
            "fit-within-pair-difference-of-responses-before-run-bootstrap",
        )

    def test_random_effects_and_dependence_diagnostics_are_present(self) -> None:
        addi = self.items[("addi", 4, "throughput")]
        meta = addi["cross_run_random_effects"]
        self.assertTrue(meta["identifiable"])
        self.assertIsNotNone(meta["i_squared"])
        self.assertEqual(meta["tau_squared_method"], "Paule-Mandel")
        self.assertEqual(
            meta["confidence_interval_method"],
            "modified-Hartung-Knapp-t(k-1)",
        )
        self.assertGreaterEqual(meta["modified_hartung_knapp_scale"], 1.0)
        self.assertEqual(len(meta["confidence_interval"]), 2)
        self.assertEqual(
            meta["estimand"],
            "absolute-instruction-cost-through-control-chain",
        )
        self.assertAlmostEqual(meta["random_effect_estimate"], 1.70, delta=0.03)
        contrast_meta = meta["local_contrast_only"]
        self.assertAlmostEqual(
            contrast_meta["random_effect_estimate"], 0.90, delta=0.03
        )
        self.assertFalse(
            contrast_meta["may_support_absolute_high_confidence"]
        )
        self.assertEqual(len(addi["autocorrelation"]), 3)
        self.assertIn("bootstrap_ci", addi["effects"])
        self.assertFalse(
            self.result["simultaneous_inference"]["run_is_highest_cluster"]
        )
        self.assertTrue(
            self.result["simultaneous_inference"]["super_run_is_highest_cluster"]
        )

    def test_batch_model_uses_actual_middle_level_as_reference(self) -> None:
        addi = self.items[("addi", 4, "throughput")]
        effects = addi["effects"]
        self.assertEqual(
            effects["batch_effect_model"], "categorical-reference-batch"
        )
        self.assertEqual(effects["batch_reference"], 24_000)
        self.assertEqual(effects["batch_levels"], [12_000, 24_000, 48_000])
        self.assertEqual(
            effects["per_log_batch_method"],
            "compatibility-endpoint-secant-not-used-for-gating",
        )
        self.assertEqual(
            effects["batch_level_effects_vs_reference"]["24000"], 0.0
        )
        self.assertEqual(
            set(effects["batch_pairwise_effects"]),
            {"12000:24000", "12000:48000", "24000:48000"},
        )
        self.assertEqual(
            set(effects["batch_pairwise_simultaneous_ci"]),
            {"12000:24000", "12000:48000", "24000:48000"},
        )
        self.assertEqual(
            effects["batch_pairwise_contrast_direction"], "right-minus-left"
        )

    def test_severe_outlier_diagnostic_declares_complete_run_as_unit(self) -> None:
        addi = self.items[("addi", 4, "throughput")]
        inference = addi["severe_outlier_run_cluster_inference"]
        self.assertEqual(inference["runs"], 3)
        self.assertEqual(len(inference["per_run"]), 3)
        self.assertEqual(
            addi["severe_outlier_fraction_wilson_upper"],
            addi["severe_outlier_fraction_run_cluster_upper"],
        )
        self.assertEqual(
            self.result["quality_thresholds"][
                "severe_outlier_independent_unit"
            ],
            "complete-crossover-super-run",
        )
        self.assertIn(
            "diagnostic-only",
            self.result["quality_thresholds"]["severe_outlier_fraction_gate"],
        )
        self.assertIn("estimator_sensitivity", addi)
        self.assertEqual(
            addi["estimator_sensitivity"]["estimand"],
            "classical-heteroscedastic-wls-minus-huber-absolute-cost",
        )

    def test_leave_one_super_run_out_influence_is_cluster_level(self) -> None:
        addi = self.items[("addi", 4, "throughput")]
        sensitivity = addi["leave_one_super_run_out_sensitivity"]

        self.assertEqual(sensitivity["runs"], 3)
        self.assertEqual(len(sensitivity["per_super_run"]), 3)
        self.assertTrue(sensitivity["complete"])
        self.assertEqual(
            {row["omitted_super_run"] for row in sensitivity["per_super_run"]},
            {"cold-0", "cold-1", "cold-2"},
        )
        self.assertIn("full Huber", sensitivity["method"])
        self.assertAlmostEqual(
            sensitivity["full_estimate_ns_per_instruction"],
            addi["unconstrained_ns_per_instruction"],
        )
        for row in sensitivity["per_super_run"]:
            self.assertAlmostEqual(
                row["shift_ns"],
                row["ns_per_instruction"]
                - sensitivity["full_estimate_ns_per_instruction"],
            )

    def test_leave_one_super_run_out_refits_the_full_estimator(self) -> None:
        rows = synthetic_samples()
        pairs, _assumed_empty = MODEL._pair_samples(rows)
        grouped: dict[object, list[MODEL._Pair]] = {}
        for pair in pairs:
            grouped.setdefault(pair.key, []).append(pair)
        fits = {
            key: MODEL._fit_variant(members, "plugin_delta_ns")
            for key, members in grouped.items()
        }
        controls = {
            key: next(
                (
                    candidate
                    for candidate in fits
                    if key != candidate
                    and key.mnemonic != "nop"
                    and candidate.mnemonic == "nop"
                ),
                None,
            )
            for key in fits
        }
        full, failures = MODEL._resolve_absolute(
            {key: fit.estimate for key, fit in fits.items()}, controls
        )
        self.assertEqual(failures, {})
        sensitivity = MODEL._leave_one_super_run_out_sensitivity(
            fits,
            {key: "plugin_delta_ns" for key in fits},
            controls,
            full,
        )

        for omitted in {pair.super_run for pair in pairs}:
            contrasts = {
                key: MODEL._fit_variant(
                    [
                        pair
                        for pair in fit.pairs
                        if pair.super_run != omitted
                    ],
                    "plugin_delta_ns",
                    compute_condition=False,
                    compute_standard_error=False,
                    batch_levels=fit.batch_levels,
                    batch_reference=fit.batch_reference,
                ).estimate
                for key, fit in fits.items()
            }
            expected, expected_failures = MODEL._resolve_absolute(
                contrasts, controls
            )
            self.assertEqual(expected_failures, {})
            for key in fits:
                row = next(
                    item
                    for item in sensitivity[key]["per_super_run"]
                    if item["omitted_super_run"] == omitted
                )
                self.assertAlmostEqual(
                    row["ns_per_instruction"], expected[key]
                )

    def test_comparable_nuisance_effects_sum_the_control_chain(self) -> None:
        nop = self.items[("nop", 4, "throughput")]
        addi = self.items[("addi", 4, "throughput")]
        self.assertEqual(
            addi["effects"]["estimand"],
            "absolute-instruction-cost-through-control-chain",
        )
        self.assertAlmostEqual(
            addi["effects"]["ab_ba_difference"],
            addi["effects"]["local_contrast_only"]["ab_ba_difference"]
            + nop["effects"]["ab_ba_difference"],
        )
        self.assertEqual(
            addi["effects"]["batch_pairwise_effects"],
            addi["effects"]["local_contrast_only"][
                "batch_pairwise_effects"
            ],
        )
        self.assertIn(
            "every control edge",
            addi["effects"]["batch_quality_estimand"],
        )

    def test_target_quality_requires_every_control_quality_gate(self) -> None:
        nop = self.items[("nop", 4, "throughput")]
        addi = self.items[("addi", 4, "throughput")]

        self.assertNotEqual(nop["quality"], "high-confidence")
        self.assertIn("control-quality-not-high", addi["quality_failures"])
        self.assertEqual(len(addi["control_quality_chain"]), 1)
        self.assertEqual(
            addi["control_quality_chain"][0]["key"]["mnemonic"], "nop"
        )
        self.assertEqual(
            addi["control_quality_chain"][0]["quality"], nop["quality"]
        )

    def test_parallel_bootstrap_is_deterministic(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        serial = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=11, bootstrap_jobs=1, seed=991
        )
        parallel = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=11, bootstrap_jobs=2, seed=991
        )
        self.assertEqual(serial["instructions"], parallel["instructions"])
        self.assertEqual(
            serial["simultaneous_inference"]["valid_replicates"],
            parallel["simultaneous_inference"]["valid_replicates"],
        )
        self.assertEqual(
            parallel["simultaneous_inference"]["worker_processes"], 2
        )

    def test_generation_configuration_records_all_replay_inputs(self) -> None:
        configuration = self.result["generation_configuration"]
        self.assertEqual(configuration["bootstrap_replicates"], 79)
        self.assertEqual(configuration["seed"], 71)
        self.assertEqual(configuration["minimum_pairs"], 30)
        self.assertEqual(configuration["minimum_effective_pairs"], 20)
        self.assertEqual(
            configuration["schema"],
            MODEL.GENERATION_CONFIGURATION_SCHEMA,
        )

    def test_low_confidence_items_do_not_define_normalization(self) -> None:
        self.assertTrue(
            all(
                item["quality"] != "high-confidence"
                for item in self.result["instructions"]
            )
        )
        self.assertIsNone(self.result["normalization_ns_per_instruction"])
        self.assertTrue(
            all(
                item["relative_weight"] is None
                for item in self.result["instructions"]
            )
        )


class StatisticalGuardrailTests(unittest.TestCase):
    @staticmethod
    def _crossover_samples(super_runs: int = 12) -> list[dict[str, object]]:
        base = [
            dict(row)
            for row in synthetic_samples()
            if row["instruction"] == "nop" and row["run_id"] == "cold-0"
        ]
        output: list[dict[str, object]] = []
        sequence = 0
        for super_index in range(super_runs):
            design = "ABBA" if super_index % 2 == 0 else "BAAB"
            launches = (
                {1: (1, 2), 2: (4, 3)}
                if design == "ABBA"
                else {1: (2, 1), 2: (3, 4)}
            )
            for crossover_pair in (1, 2):
                run = f"crossover-{super_index}-{crossover_pair}"
                timing, plugin_off = launches[crossover_pair]
                for source in base:
                    row = dict(source)
                    row["run_id"] = run
                    row["run_order"] = super_index * 2 + crossover_pair - 1
                    row["super_run_id"] = f"crossover-{super_index}"
                    row["super_run_order"] = super_index
                    row["crossover_pair"] = crossover_pair
                    row["crossover_design"] = design
                    row["timing_launch_position"] = timing
                    row["plugin_off_launch_position"] = plugin_off
                    row["segment_id"] = f"{run}-{source['segment_id']}"
                    row["sequence"] = sequence
                    sequence += 1
                    output.append(row)
        return output

    def test_max_stat_uses_conservative_finite_bootstrap_rank(self) -> None:
        values = [float(value) for value in range(999)]
        critical, evidence = MODEL._conservative_bootstrap_quantile(
            values, 0.95, 0.95
        )

        self.assertEqual(evidence["required_rank"], 961)
        self.assertEqual(evidence["selected_rank"], 961)
        self.assertEqual(critical, 960.0)
        self.assertTrue(evidence["finite_rank_supported"])

        _critical, insufficient = MODEL._conservative_bootstrap_quantile(
            list(range(19)), 0.95, 0.95
        )
        self.assertFalse(insufficient["finite_rank_supported"])

        publication_values = [float(value) for value in range(4999)]
        publication_critical, publication_evidence = (
            MODEL._conservative_bootstrap_quantile(
                publication_values, 0.99375, 0.99375
            )
        )
        self.assertEqual(publication_evidence["required_rank"], 4982)
        self.assertEqual(publication_evidence["selected_rank"], 4982)
        self.assertEqual(publication_critical, 4981.0)

        rows = [{"x": float(value)} for value in range(4999)]
        _intervals, _critical, valid, split_evidence = (
            MODEL._simultaneous_intervals(
                {"x": 0.0}, rows, 0.99375, 0.99375
            )
        )
        self.assertEqual(valid, 4000)
        self.assertEqual(split_evidence["complete_family_replicates"], 4999)
        self.assertEqual(split_evidence["scale_replicates"], 999)
        self.assertEqual(split_evidence["quantile_replicates"], 4000)
        self.assertEqual(split_evidence["required_rank"], 3988)

    def test_overall_confidence_is_allocated_across_all_publication_families(
        self,
    ) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        result = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=19, confidence=0.95
        )
        control = result["publication_familywise_error_control"]

        self.assertEqual(control["family_count"], 4)
        self.assertAlmostEqual(control["sampling_alpha_budget"], 0.025)
        self.assertAlmostEqual(control["monte_carlo_alpha_budget"], 0.025)
        self.assertAlmostEqual(
            control["sampling_alpha_per_family"], 0.00625
        )
        self.assertAlmostEqual(
            control["sampling_confidence_per_family"], 0.99375
        )
        self.assertAlmostEqual(
            control["monte_carlo_alpha_per_family"], 0.00625
        )
        self.assertAlmostEqual(
            control["monte_carlo_confidence_per_family"], 0.99375
        )
        self.assertAlmostEqual(
            control["sampling_alpha_budget"]
            + control["monte_carlo_alpha_budget"],
            control["overall_alpha"],
        )
        for name in (
            "simultaneous_inference",
            "diagnostic_simultaneous_inference",
            "auxiliary_consistency_inference",
            "joint_raw_adjusted_inference",
        ):
            self.assertAlmostEqual(
                result[name]["familywise_confidence"], 0.99375
            )
            evidence = result[name]["critical_value_monte_carlo"]
            self.assertAlmostEqual(
                evidence["target_probability"], 0.99375
            )
            self.assertAlmostEqual(
                evidence["monte_carlo_confidence"], 0.99375
            )

    def test_simultaneous_intervals_use_only_complete_family_replicates(
        self,
    ) -> None:
        points = {"left": 0.0, "right": 0.0}
        complete = [
            {"left": float(index), "right": float(-index)}
            for index in range(1, 40)
        ]
        partial = [
            {"left": 1_000_000.0 + float(index)} for index in range(400)
        ]

        actual = MODEL._simultaneous_intervals(
            points, complete + partial, 0.95
        )
        reference = MODEL._simultaneous_intervals(points, complete, 0.95)

        self.assertEqual(actual, reference)
        self.assertEqual(actual[2], len(complete))

    def test_diagnostic_family_requires_complete_replicates(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]

        def incomplete_diagnostics(
            state: MODEL._BootstrapState, replicate_seed: int
        ) -> tuple[dict[object, float], dict[object, dict[str, float]]]:
            key = state.keys[0]
            jitter = ((replicate_seed % 101) - 50) * 1.0e-6
            return ({key: 0.8 + jitter}, {key: {"order": 0.0}})

        with mock.patch.object(
            MODEL, "_run_bootstrap_replicate", side_effect=incomplete_diagnostics
        ):
            result = fit_microbenchmark_weight_model(
                rows, bootstrap_replicates=999, seed=31337
            )
        item = result["instructions"][0]
        inference = result["diagnostic_simultaneous_inference"]

        self.assertEqual(inference["complete_family_replicates"], 0)
        self.assertIn(
            "insufficient-diagnostic-bootstrap-replicates",
            item["quality_failures"],
        )
        self.assertIn(
            "insufficient-diagnostic-bootstrap-valid-fraction",
            item["quality_failures"],
        )

    def test_translation_exclusion_fraction_fails_even_with_enough_pairs(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        contaminated: set[tuple[str, str]] = set()
        for row in rows:
            pair = (str(row["run_id"]), str(row["segment_id"]))
            if int(str(row["segment_id"]).split("-")[1]) % 10 == 0:
                contaminated.add(pair)
        for row in rows:
            if (str(row["run_id"]), str(row["segment_id"])) in contaminated:
                row["translations_during_window"] = 1

        result = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=0, min_pairs=30
        )
        item = result["instructions"][0]

        self.assertGreater(item["pairs"], 30)
        self.assertGreater(
            item["translation_exclusion_fraction_run_cluster_upper"],
            item["maximum_translation_excluded_pair_fraction"],
        )
        self.assertIn(
            "translation-exclusion-fraction-too-high", item["quality_failures"]
        )

    def test_process_crossover_effects_detect_design_period_and_carryover(self) -> None:
        rows = self._crossover_samples()
        for row in rows:
            design = str(row["crossover_design"])
            pair = int(row["crossover_pair"])
            # Orthogonal injected effects: design=+/-0.3, period=+0.4,
            # preceded-by-plugin-off=+0.5 ns/instruction.
            design_effect = 0.3 if design == "ABBA" else -0.3
            period_effect = 0.2 if pair == 2 else -0.2
            preceded = (design == "ABBA" and pair == 2) or (
                design == "BAAB" and pair == 1
            )
            carryover_effect = 0.25 if preceded else -0.25
            if row["role"] == "probe":
                delta = (
                    design_effect + period_effect + carryover_effect
                ) * int(row["target_count"])
                row["plugin_thread_cpu_ns"] = float(
                    row["plugin_thread_cpu_ns"]
                ) + delta
        pairs, _ = MODEL._pair_samples(rows)
        fit = MODEL._fit_variant(pairs, "plugin_delta_ns")
        effects = MODEL._process_crossover_effects(fit)

        self.assertTrue(effects["available"])
        self.assertAlmostEqual(
            effects["design_abba_minus_baab"], 0.6, delta=0.03
        )
        self.assertAlmostEqual(
            effects["second_pair_minus_first_pair"], 0.4, delta=0.03
        )
        self.assertAlmostEqual(
            effects["preceded_by_plugin_off_minus_other_timing"],
            0.5,
            delta=0.03,
        )

    def test_crossover_effects_enter_quality_gate_and_threshold_output(
        self,
    ) -> None:
        rows = self._crossover_samples()
        result = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=19, seed=7331
        )
        item = result["instructions"][0]

        self.assertIn(
            "minimum_crossover_design_fraction", result["quality_thresholds"]
        )
        self.assertIn(
            "process_launch_crossover", item["effects"]
        )
        self.assertEqual(
            set(item["effects"]["process_launch_crossover"]["simultaneous_ci"]),
            {
                "design_abba_minus_baab",
                "second_pair_minus_first_pair",
                "preceded_by_plugin_off_minus_other_timing",
            },
        )

    """验证污染、非物理解和 bootstrap 失败不会被发布为高置信权重。"""

    def test_run_block_resampling_preserves_acquisition_order(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        renamed = {"cold-0": "run-1", "cold-1": "run-10", "cold-2": "run-2"}
        explicit = {"run-1": 0, "run-10": 1, "run-2": 2}
        for row in rows:
            row["run_id"] = renamed[str(row["run_id"])]
            row["run_order"] = explicit[str(row["run_id"])]
        pairs, _ = MODEL._pair_samples(rows)

        class FixedRandom:
            def randrange(self, _length: int) -> int:
                return 0

        resampled = MODEL._hierarchical_resample(
            pairs, block_length=1, rng=FixedRandom(), run_block_length=3
        )
        first_pair_by_copy = [
            next(
                pair
                for pair in resampled
                if pair.run == f"bootstrap-super-run-{index}-qemu-{index}"
            )
            for index in range(3)
        ]
        self.assertEqual(
            [pair.run_order for pair in first_pair_by_copy], [0, 1, 2]
        )
        # 采集轴是 1,10,2；不能被字典序 1,2,10 重排。
        source_runs = MODEL._ordered_runs(pairs)
        self.assertEqual(source_runs, ["run-1", "run-10", "run-2"])

    def test_plugin_off_difference_keeps_timing_minus_uninstrumented_sign(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        for row in rows:
            row["plugin_off_guest_ns"] = float(row["guest_ns"])
            if row["role"] == "probe":
                row["plugin_off_guest_ns"] -= 0.25 * int(row["target_count"])
        pairs, _ = MODEL._pair_samples(rows)
        values = [
            pair.plugin_off_difference_ns / pair.target_count
            for pair in pairs
            if pair.plugin_off_difference_ns is not None
        ]
        self.assertTrue(values)
        self.assertTrue(all(abs(value - 0.25) < 1.0e-12 for value in values))

    def test_translation_contaminated_pairs_are_excluded(self) -> None:
        rows = synthetic_samples()
        nop_pairs_by_run: dict[str, list[str]] = {}
        for row in rows:
            if row["instruction"] != "nop":
                continue
            run = str(row["run_id"])
            pair = str(row["segment_id"])
            pairs = nop_pairs_by_run.setdefault(run, [])
            if pair not in pairs:
                pairs.append(pair)
        contaminated = {
            (run, pair)
            for run, pairs in nop_pairs_by_run.items()
            for pair in pairs[:30]
        }
        for row in rows:
            if (str(row["run_id"]), str(row["segment_id"])) in contaminated:
                row["translations_during_window"] = 1

        result = fit_microbenchmark_weight_model(
            rows,
            bootstrap_replicates=0,
            min_pairs=30,
            min_effective_pairs=20,
        )
        nop = next(
            item
            for item in result["instructions"]
            if item["key"]["mnemonic"] == "nop"
        )

        self.assertEqual(len(contaminated), 90)
        self.assertEqual(
            result["sample_filtering"][
                "translation_contaminated_pairs_excluded"
            ],
            90,
        )
        self.assertEqual(
            result["sample_filtering"]["translation_unknown_pairs_retained"],
            0,
        )
        excluded_by_instruction = result["sample_filtering"][
            "translation_contaminated_pairs_excluded_by_instruction"
        ]
        self.assertEqual(len(excluded_by_instruction), 1)
        self.assertEqual(
            excluded_by_instruction[0]["key"]["mnemonic"], "nop"
        )
        self.assertEqual(excluded_by_instruction[0]["pairs"], 90)
        self.assertEqual(nop["translation_contaminated_pairs_excluded"], 90)
        self.assertEqual(nop["pairs"], 18)
        self.assertIn("insufficient-pairs", nop["quality_failures"])
        self.assertNotEqual(nop["quality"], "high-confidence")

    def test_significantly_negative_estimate_is_not_published_as_zero(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        pairs: dict[tuple[str, str], dict[str, dict[str, object]]] = {}
        for row in rows:
            key = (str(row["run_id"]), str(row["segment_id"]))
            pairs.setdefault(key, {})[str(row["role"])] = row
        for pair in pairs.values():
            probe = pair["probe"]
            baseline = pair["baseline"]
            count = int(probe["target_count"])
            for metric in (
                "plugin_thread_cpu_ns",
                "guest_ns",
                "plugin_off_guest_ns",
            ):
                probe[metric] = float(baseline[metric]) - 0.5 * count

        def negative_replicate(
            state: MODEL._BootstrapState, replicate_seed: int
        ) -> tuple[dict[object, float], dict[object, tuple[float, float, float, None]]]:
            key = state.keys[0]
            jitter = ((replicate_seed % 2001) - 1000) * 1e-6
            # joint max-stat family 还包含 estimator-sensitivity；mock 必须
            # 返回完整 family，避免测试绕过新的 complete-replicate 门禁。
            return (
                {
                    key: -0.5 + jitter,
                    ("estimator-sensitivity", key): 0.0,
                },
                {key: (0.0, 0.0, 0.0, None)},
            )

        with mock.patch.object(
            MODEL, "_run_bootstrap_replicate", side_effect=negative_replicate
        ):
            result = fit_microbenchmark_weight_model(
                rows,
                bootstrap_replicates=999,
                seed=1927,
            )
        item = result["instructions"][0]

        self.assertLess(item["unconstrained_ns_per_instruction"], -0.49)
        self.assertLess(item["unconstrained_simultaneous_ci"][1], 0.0)
        self.assertIsNone(item["ns_per_instruction"])
        self.assertFalse(item["zero_cost_equivalent"])
        self.assertIn(
            "negative-unconstrained-weight", item["quality_failures"]
        )
        self.assertNotEqual(item["quality"], "high-confidence")

    def test_any_missing_bootstrap_replicate_is_not_high_confidence(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        calls = 0

        def partially_valid_replicate(
            state: MODEL._BootstrapState, _replicate_seed: int
        ) -> tuple[
            dict[object, float],
            dict[object, tuple[float, float, float, None]],
        ] | None:
            nonlocal calls
            current = calls
            calls += 1
            if current == 0:
                return None
            key = state.keys[0]
            jitter = ((current % 21) - 10) * 1e-5
            return (
                {
                    key: 0.8 + jitter,
                    ("estimator-sensitivity", key): 0.0,
                },
                {key: (0.0, 0.0, 0.0, None)},
            )

        with mock.patch.object(
            MODEL,
            "_run_bootstrap_replicate",
            side_effect=partially_valid_replicate,
        ):
            result = fit_microbenchmark_weight_model(
                rows,
                bootstrap_replicates=1100,
                seed=2293,
            )
        item = result["instructions"][0]
        inference = result["simultaneous_inference"]

        self.assertEqual(calls, 1100)
        self.assertEqual(inference["valid_replicates"], 1099)
        self.assertAlmostEqual(inference["valid_fraction"], 1099.0 / 1100.0)
        self.assertEqual(inference["minimum_valid_fraction"], 1.0)
        self.assertIn(
            "insufficient-bootstrap-replicates", item["quality_failures"]
        )
        self.assertIn(
            "insufficient-bootstrap-valid-fraction",
            item["quality_failures"],
        )
        self.assertNotEqual(item["quality"], "high-confidence")

    def test_run_cluster_outlier_bound_is_invariant_to_pair_duplication(self) -> None:
        outcomes = [False, True, False, False, False, False]
        runs = ["run-a", "run-a", "run-b", "run-b", "run-c", "run-c"]
        original = MODEL._run_cluster_proportion_upper_bound(
            outcomes, runs, 0.95
        )
        duplicated = MODEL._run_cluster_proportion_upper_bound(
            [outcome for outcome in outcomes for _ in range(50)],
            [run for run in runs for _ in range(50)],
            0.95,
        )

        self.assertEqual(original["runs"], 3)
        self.assertAlmostEqual(
            original["mean_run_fraction"],
            duplicated["mean_run_fraction"],
        )
        self.assertAlmostEqual(original["upper"], duplicated["upper"])
        pair_wilson = MODEL._wilson_upper_bound(
            sum(outcomes) * 50, len(outcomes) * 50, 0.95
        )
        self.assertLess(pair_wilson, duplicated["upper"])

    def test_paule_mandel_and_modified_hartung_knapp_metadata(self) -> None:
        # 不等方差数据的 PM 根明显不同于 DL（后者约为 1.1667）。
        meta = MODEL._paule_mandel_tau_squared(
            [0.0, 1.0, 4.0], [0.1, 1.0, 4.0]
        )

        self.assertTrue(meta["converged"])
        self.assertAlmostEqual(meta["tau_squared"], 1.9502808722, places=8)
        self.assertAlmostEqual(meta["q_at_tau"], 2.0, places=8)
        self.assertAlmostEqual(
            MODEL._student_t_critical(0.95, 1), 12.706204736, places=8
        )
        self.assertAlmostEqual(
            MODEL._student_t_critical(0.95, 2), 4.30265273, places=8
        )

    def test_categorical_batch_model_detects_u_shape_hidden_from_log_slope(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        batch_offsets = {12_000: 0.20, 24_000: 0.0, 48_000: 0.20}
        for row in rows:
            if row["role"] != "probe":
                continue
            offset = batch_offsets[int(row["batch"])] * int(row["target_count"])
            for metric in (
                "plugin_thread_cpu_ns",
                "guest_ns",
                "plugin_off_guest_ns",
            ):
                row[metric] = float(row[metric]) + offset
        pairs, _assumed_empty = MODEL._pair_samples(rows)
        fit = MODEL._fit_variant(pairs, "plugin_delta_ns")

        self.assertEqual(fit.batch_reference, 24_000)
        self.assertAlmostEqual(fit.batch_level_effects[24_000], 0.0)
        self.assertAlmostEqual(
            fit.batch_level_effects[12_000], 0.20, delta=0.01
        )
        self.assertAlmostEqual(
            fit.batch_level_effects[48_000], 0.20, delta=0.01
        )
        # 旧单斜率兼容值只看两端，几乎为零；逐档模型保留了 U 形。
        self.assertAlmostEqual(fit.batch_effect, 0.0, delta=0.01)
        self.assertGreater(fit.batch_peak_to_peak, 0.18)

    def test_batch_gate_checks_low_to_high_pairwise_contrast(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        batch_offsets = {12_000: -0.09, 24_000: 0.0, 48_000: 0.09}
        for row in rows:
            if row["role"] != "probe":
                continue
            offset = batch_offsets[int(row["batch"])] * int(row["target_count"])
            for metric in (
                "plugin_thread_cpu_ns",
                "guest_ns",
                "plugin_off_guest_ns",
            ):
                row[metric] = float(row[metric]) + offset
        result = fit_microbenchmark_weight_model(
            rows,
            bootstrap_replicates=199,
            seed=881,
            equivalence_margin=0.14,
        )
        item = result["instructions"][0]
        effects = item["effects"]
        margin = 0.14 * item["unconstrained_ns_per_instruction"]

        for level in ("12000", "48000"):
            interval = effects["batch_level_simultaneous_ci"][level]
            self.assertGreaterEqual(interval[0], -margin)
            self.assertLessEqual(interval[1], margin)
        low_high = effects["batch_pairwise_simultaneous_ci"][
            "12000:48000"
        ]
        self.assertGreater(low_high[0], margin)
        self.assertIn("batch-size-nonlinearity", item["quality_failures"])

    def test_batch_gate_uses_each_control_edges_physical_grid(self) -> None:
        rows = synthetic_samples()
        for row in rows:
            if row["instruction"] != "nop":
                continue
            rank = {12_000: 0, 24_000: 1, 48_000: 2}[int(row["batch"])]
            long_batch = (192_000, 768_000, 3_072_000)[rank]
            row["batch"] = long_batch
        result = fit_microbenchmark_weight_model(
            rows,
            bootstrap_replicates=79,
            seed=313,
            min_pairs=30,
            min_effective_pairs=20,
        )
        items = {
            (item["key"]["mnemonic"], item["key"]["size"]): item
            for item in result["instructions"]
        }
        nop = items[("nop", 4)]
        addi = items[("addi", 4)]

        self.assertEqual(
            nop["effects"]["batch_levels"],
            [192_000, 768_000, 3_072_000],
        )
        self.assertEqual(
            addi["effects"]["batch_levels"], [12_000, 24_000, 48_000]
        )
        self.assertNotIn("batch-size-nonlinearity", addi["quality_failures"])
        self.assertTrue(
            all(
                value is not None
                for value in addi["effects"][
                    "batch_pairwise_simultaneous_ci"
                ].values()
            )
        )

    def test_irls_cycle_damping_converges_to_a_fixed_point(self) -> None:
        matrix = [
            [1.0, 0.5, -0.5, 0.0, 1.0],
            [1.0, -0.5, -0.4368932038834952, 1.0, 0.0],
            [1.0, 0.5, -0.42071197411003236, 0.0, 0.0],
            [1.0, 0.5, -0.4077669902912621, 1.0, 0.0],
            [1.0, 0.5, -0.3948220064724919, 0.0, 0.0],
            [1.0, 0.5, -0.3932038834951456, 0.0, 1.0],
            [1.0, -0.5, -0.27346278317152106, 0.0, 0.0],
            [1.0, -0.5, -0.27184466019417475, 1.0, 0.0],
            [1.0, -0.5, -0.26860841423948223, 0.0, 1.0],
            [1.0, 0.5, -0.1844660194174757, 0.0, 0.0],
            [1.0, 0.5, -0.1326860841423948, 1.0, 0.0],
            [1.0, -0.5, -0.13106796116504854, 0.0, 1.0],
            [1.0, 0.5, -0.10032362459546923, 0.0, 0.0],
            [1.0, -0.5, -0.09385113268608414, 1.0, 0.0],
            [1.0, 0.5, -0.05987055016181231, 0.0, 1.0],
            [1.0, 0.5, 0.02588996763754048, 0.0, 1.0],
            [1.0, -0.5, 0.03074433656957931, 0.0, 0.0],
            [1.0, -0.5, 0.03398058252427183, 1.0, 0.0],
            [1.0, 0.5, 0.10841423948220064, 1.0, 0.0],
            [1.0, 0.5, 0.1181229773462783, 0.0, 0.0],
            [1.0, -0.5, 0.15372168284789645, 0.0, 1.0],
            [1.0, 0.5, 0.22653721682847894, 1.0, 0.0],
            [1.0, 0.5, 0.24110032362459544, 0.0, 1.0],
            [1.0, -0.5, 0.25889967637540456, 0.0, 0.0],
            [1.0, -0.5, 0.3203883495145631, 0.0, 0.0],
            [1.0, 0.5, 0.3527508090614887, 1.0, 0.0],
            [1.0, 0.5, 0.36084142394822005, 0.0, 1.0],
            [1.0, 0.5, 0.4255663430420712, 1.0, 0.0],
            [1.0, -0.5, 0.488673139158576, 0.0, 1.0],
            [1.0, -0.5, 0.5, 0.0, 0.0],
        ]
        response = [
            2.5807342529296875, 2.746826171875, 2.8519287109375,
            2.712646484375, 3.06109619140625, 3.03277587890625,
            2.90631103515625, 2.7392578125, 2.987823486328125,
            3.0047607421875, 3.147705078125, 2.9222564697265625,
            2.933837890625, 3.292236328125, 2.807891845703125,
            2.9395294189453125, 2.72601318359375, 2.883544921875,
            3.218994140625, 3.348388671875, 2.987060546875,
            2.86181640625, 2.863861083984375, 2.92962646484375,
            2.90692138671875, 2.776123046875, 2.8403167724609375,
            2.873779296875, 2.946258544921875, 3.072021484375,
        ]
        hetero = [
            (1.0, 0.4612479734897932, 1.8564841988273533)[
                int(row[3] != 0.0) + 2 * int(row[4] != 0.0)
            ]
            for row in matrix
        ]
        sparse = [
            [(index, value) for index, value in enumerate(row) if value]
            for row in matrix
        ]
        fit = MODEL._robust_fit(
            matrix, response, hetero, sparse_rows=sparse
        )

        self.assertTrue(fit[4])
        self.assertLessEqual(fit[5], 120)
        self.assertTrue(fit[6])

    def test_skipping_standard_error_preserves_fit_outputs(self) -> None:
        pairs, _assumed_empty = MODEL._pair_samples(
            [
                row
                for row in synthetic_samples()
                if row["instruction"] == "nop"
            ]
        )
        full = MODEL._fit_variant(pairs, "plugin_delta_ns")
        bootstrap = MODEL._fit_variant(
            pairs,
            "plugin_delta_ns",
            compute_condition=False,
            compute_standard_error=False,
        )

        self.assertAlmostEqual(full.estimate, bootstrap.estimate, delta=1e-12)
        for level in full.batch_level_effects:
            self.assertAlmostEqual(
                full.batch_level_effects[level],
                bootstrap.batch_level_effects[level],
                delta=1e-12,
            )
        for actual, expected in zip(
            full.robust_weights, bootstrap.robust_weights, strict=True
        ):
            self.assertAlmostEqual(actual, expected, delta=1e-12)
        self.assertIsNone(bootstrap.standard_error)

    def test_wide_future_run_interval_fails_even_when_i_squared_is_low(self) -> None:
        rows = [
            row for row in synthetic_samples() if row["instruction"] == "nop"
        ]
        for row in rows:
            if row["role"] != "probe":
                continue
            round_index = int(str(row["segment_id"]).split("-")[2])
            noise = (
                0.5 if round_index % 2 else -0.5
            ) * int(row["target_count"])
            for metric in (
                "plugin_thread_cpu_ns",
                "guest_ns",
                "plugin_off_guest_ns",
            ):
                row[metric] = float(row[metric]) + noise
        result = fit_microbenchmark_weight_model(rows, bootstrap_replicates=0)
        item = result["instructions"][0]
        meta = item["cross_run_random_effects"]

        self.assertLessEqual(
            meta["i_squared"],
            result["quality_thresholds"]["maximum_i_squared"],
        )
        self.assertGreater(meta["prediction_interval_half_width"], 0.15)
        self.assertIn(
            "cross-run-heterogeneity-high", item["quality_failures"]
        )
        self.assertEqual(
            result["quality_thresholds"]["i_squared_role"],
            "diagnostic-only-not-a-prediction-interval-gate",
        )


class QualityAndInputTests(unittest.TestCase):
    """验证不可辨识输入、guest fallback 以及文件格式。"""

    def test_unpaired_window_is_rejected(self) -> None:
        rows = synthetic_samples()[:1]
        with self.assertRaisesRegex(MicrobenchmarkModelError, "恰好包含"):
            fit_microbenchmark_weight_model(rows, bootstrap_replicates=0)

    def test_unknown_control_is_explicitly_unidentifiable(self) -> None:
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] == "addi"
        ]
        result = fit_microbenchmark_weight_model(rows, bootstrap_replicates=9)
        item = result["instructions"][0]
        self.assertIsNone(item["ns_per_instruction"])
        self.assertEqual(item["quality"], "not-identifiable")
        self.assertIn("absolute-reference-unresolved", item["quality_failures"])

    def test_guest_only_data_is_degraded_not_silently_promoted(self) -> None:
        rows = [
            {key: value for key, value in row.items() if key != "plugin_thread_cpu_ns"}
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        result = fit_microbenchmark_weight_model(rows, bootstrap_replicates=9)
        item = result["instructions"][0]
        self.assertEqual(item["source"], "guest-time-fallback")
        self.assertIn("guest-time-primary-response", item["quality_failures"])

    def test_run_order_uses_explicit_acquisition_order_not_run_id(self) -> None:
        rows = [
            dict(row)
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        mapping = {
            "cold-0": ("run-10", 2),
            "cold-1": ("run-2", 1),
            "cold-2": ("run-1", 0),
        }
        for row in rows:
            run, order = mapping[str(row["run_id"])]
            row["run_id"] = run
            row["run_order"] = order

        pairs, _assumed_empty = MODEL._pair_samples(rows)

        self.assertEqual(MODEL._ordered_runs(pairs), ["run-1", "run-2", "run-10"])
        fit = MODEL._fit_variant(pairs, "plugin_delta_ns")
        self.assertEqual(
            [pair.run for pair in fit.pairs[:3]],
            ["run-1", "run-1", "run-1"],
        )
        result = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=0
        )
        self.assertEqual(
            result["simultaneous_inference"]["run_order"],
            ["run-1", "run-2", "run-10"],
        )
        self.assertEqual(
            result["simultaneous_inference"]["run_order_source"],
            "explicit-run-order",
        )

    def test_run_order_recovers_strict_contiguous_numeric_suffix(self) -> None:
        rows = [
            dict(row)
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        mapping = {"cold-0": "run-1", "cold-1": "run-10", "cold-2": "run-2"}
        for row in rows:
            row["run_id"] = mapping[str(row["run_id"])]

        pairs, _assumed_empty = MODEL._pair_samples(rows)

        # 此 synthetic 只有三个 run；run-10 不是连续 1..N，故不能推断。
        self.assertEqual(MODEL._ordered_runs(pairs), ["run-1", "run-10", "run-2"])
        self.assertEqual(
            {pair.run_order_source for pair in pairs},
            {"input-first-appearance"},
        )

        contiguous = {"cold-0": "run-1", "cold-1": "run-3", "cold-2": "run-2"}
        for row in rows:
            inverse = {value: key for key, value in mapping.items()}
            row["run_id"] = contiguous[inverse[str(row["run_id"])]]
        pairs, _assumed_empty = MODEL._pair_samples(rows)
        self.assertEqual(MODEL._ordered_runs(pairs), ["run-1", "run-2", "run-3"])
        self.assertEqual(
            {pair.run_order_source for pair in pairs},
            {"strict-common-prefix-contiguous-suffix"},
        )

    def test_auxiliary_bootstrap_reuses_primary_run_block_indices(self) -> None:
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        pairs, _assumed_empty = MODEL._pair_samples(rows)
        fit = MODEL._fit_variant(pairs, "plugin_delta_ns")
        key = fit.pairs[0].key
        seed = 1771
        expected = MODEL._run_resample_positions(
            3, 2, random.Random(seed)
        )
        observed: list[list[int]] = []

        def record_positions(
            length: int, block: int, rng: random.Random
        ) -> list[int]:
            result = MODEL._moving_block_positions(length, block, rng)
            observed.append(result)
            return result

        with mock.patch.object(
            MODEL,
            "_run_resample_positions",
            side_effect=record_positions,
        ) as positions:
            MODEL._auxiliary_run_cluster_inference(
                {key: fit},
                {key: "plugin_delta_ns"},
                {key: None},
                {key: "ratio"},
                [seed],
                0.95,
                2,
            )

        self.assertIn(
            mock.call(3, 2, mock.ANY),
            positions.call_args_list,
        )
        self.assertTrue(observed)
        self.assertTrue(all(indices == expected for indices in observed))

    def test_primary_bootstrap_uses_shared_run_block_indices(self) -> None:
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        pairs, _assumed_empty = MODEL._pair_samples(rows)
        fit = MODEL._fit_variant(pairs, "plugin_delta_ns")
        key = fit.pairs[0].key
        seed = 991
        expected = MODEL._run_resample_positions(
            3, 2, random.Random(seed)
        )
        state = MODEL._BootstrapState(
            pairs=tuple(pairs),
            keys=(key,),
            response_names={key: "plugin_delta_ns"},
            controls={key: None},
            batch_levels={key: fit.batch_levels},
            batch_references={key: fit.batch_reference},
            block_length=1,
            run_block_length=2,
            linear_algebra_backend="python",
        )
        captured: list[list[int]] = []
        original = MODEL._hierarchical_resample

        def record_resample(*args: object, **kwargs: object) -> list[MODEL._Pair]:
            captured.append(list(kwargs["run_positions"]))
            return original(*args, **kwargs)

        with mock.patch.object(
            MODEL, "_hierarchical_resample", side_effect=record_resample
        ):
            MODEL._run_bootstrap_replicate(state, seed)

        self.assertEqual(captured, [expected])

    def test_classical_estimate_keeps_weights_bound_when_sorting_pairs(
        self,
    ) -> None:
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]
        pairs, _assumed_empty = MODEL._pair_samples(rows)
        fit = MODEL._fit_variant(pairs, "plugin_delta_ns")
        weights = [1.0 + (index % 11) for index in range(len(fit.pairs))]
        expected = MODEL._classical_variant_estimate(
            fit.pairs,
            "plugin_delta_ns",
            batch_levels=fit.batch_levels,
            batch_reference=fit.batch_reference,
            heteroscedastic_weights=weights,
        )
        order = list(range(len(fit.pairs)))
        random.Random(1771).shuffle(order)

        actual = MODEL._classical_variant_estimate(
            [fit.pairs[index] for index in order],
            "plugin_delta_ns",
            batch_levels=fit.batch_levels,
            batch_reference=fit.batch_reference,
            heteroscedastic_weights=[weights[index] for index in order],
        )

        self.assertAlmostEqual(actual, expected, delta=1e-12)

    def test_super_run_is_the_bootstrap_cluster(self) -> None:
        rows = [dict(row) for row in synthetic_samples() if row["instruction"] == "nop"]
        for row in rows:
            source = int(str(row["run_id"]).rsplit("-", 1)[1])
            row["run_order"] = source * 2
            row["super_run_id"] = f"super-{source}"
            row["super_run_order"] = source
            row["crossover_pair"] = 1
            row["crossover_design"] = "ABBA"
            row["timing_launch_position"] = 1
            row["plugin_off_launch_position"] = 2
        copies = []
        for row in rows:
            duplicate = dict(row)
            duplicate["run_id"] = f"{row['run_id']}-copy"
            duplicate["run_order"] = int(row["run_order"]) + 1
            duplicate["crossover_pair"] = 2
            duplicate["timing_launch_position"] = 4
            duplicate["plugin_off_launch_position"] = 3
            copies.append(duplicate)
        pairs, _ = MODEL._pair_samples(rows + copies)

        self.assertEqual(len(MODEL._ordered_runs(pairs)), 6)
        self.assertEqual(len(MODEL._ordered_super_runs(pairs)), 3)
        resampled = MODEL._hierarchical_resample(
            pairs,
            1,
            random.Random(13),
            run_block_length=1,
            run_positions=[0, 0, 0],
        )
        self.assertEqual(len(MODEL._ordered_super_runs(resampled)), 3)
        self.assertEqual(len(MODEL._ordered_runs(resampled)), 6)

    def test_crossover_launch_positions_are_validated_at_model_boundary(
        self,
    ) -> None:
        rows = [dict(row) for row in synthetic_samples()]
        copies = []
        for row in rows:
            source = int(str(row["run_id"]).rsplit("-", 1)[1])
            row["run_order"] = source * 2
            row["super_run_id"] = f"super-{source}"
            row["super_run_order"] = source
            row["crossover_pair"] = 1
            row["crossover_design"] = "ABBA"
            row["timing_launch_position"] = 1
            row["plugin_off_launch_position"] = 2
            duplicate = dict(row)
            duplicate["run_id"] = f"{row['run_id']}-copy"
            duplicate["run_order"] = source * 2 + 1
            duplicate["crossover_pair"] = 2
            duplicate["timing_launch_position"] = 4
            duplicate["plugin_off_launch_position"] = 3
            copies.append(duplicate)
        MODEL._pair_samples(rows + copies)

        copies[0]["timing_launch_position"] = 3
        with self.assertRaisesRegex(
            MicrobenchmarkModelError, "pair 元数据不一致|启动位置"
        ):
            MODEL._pair_samples(rows + copies)

        missing = [dict(row) for row in rows + copies]
        for row in missing:
            row.pop("plugin_off_launch_position", None)
        with self.assertRaisesRegex(
            MicrobenchmarkModelError, "启动位置不完整"
        ):
            MODEL._pair_samples(missing)

    def test_positive_anchor_fails_closed_when_missing(self) -> None:
        rows = [row for row in synthetic_samples() if row["instruction"] == "nop"]
        pairs, _ = MODEL._pair_samples(rows)
        inference = MODEL._anchor_super_run_calibration(pairs)
        self.assertEqual(inference["status"], "unavailable")

    def test_publication_fails_closed_without_adjusted_anchor(self) -> None:
        result = fit_microbenchmark_weight_model(
            synthetic_samples(), bootstrap_replicates=0
        )
        self.assertFalse(result["publication_gate"]["passed"])
        self.assertIn(
            "positive-anchor-scale-inconclusive",
            result["publication_gate"]["failures"],
        )
        self.assertTrue(
            all(
                item["published_ns_per_instruction"] is None
                for item in result["instructions"]
            )
        )

    def test_joint_anchor_adjustment_recovers_scale_and_marks_anchor_calibration_only(
        self,
    ) -> None:
        rows = [dict(row) for row in synthetic_samples()]
        for row in rows:
            row["super_run_id"] = row["run_id"]
            row["super_run_order"] = int(str(row["run_id"]).rsplit("-", 1)[1])
            row["anchor_position"] = "not-anchor"
        sequence = max(int(row["sequence"]) for row in rows) + 1
        anchor_descriptor = {
            "size": 4,
            "bytes": "3345b502",
            "mnemonic": "div",
        }
        for run_index in range(3):
            run = f"cold-{run_index}"
            for pair_index, position in enumerate(
                ("head", "body", "body", "body", "tail"), 1
            ):
                count = (24_000, 12_000, 24_000, 48_000, 24_000)[
                    pair_index - 1
                ]
                for role in ("probe", "baseline"):
                    baseline = 1_000_000.0
                    target = 4.0 * count if role == "probe" else 0.0
                    rows.append(
                        {
                            "run_id": run,
                            "run_order": run_index,
                            "super_run_id": run,
                            "super_run_order": run_index,
                            "block_id": pair_index,
                            "pair_id": f"anchor-{run_index}-{pair_index}",
                            "sequence": sequence,
                            "role": role,
                            "instruction": "div",
                            "encoding_bytes": 4,
                            "pattern": MODEL.STABILITY_ANCHOR_PATTERN,
                            "anchor_position": position,
                            "requested_count": count,
                            "target_count": count if role == "probe" else 0,
                            "total_instruction_count": count + 1,
                            "plugin_thread_cpu_ns": baseline + target,
                            "guest_ns": baseline + target * 1.25,
                            "plugin_off_guest_ns": baseline + target * 2.0,
                            "timer_reads": 2,
                            "plugin_mode": "timing",
                            "translations_during_window": 0,
                            "baseline_kind": "empty",
                            "target_descriptor": anchor_descriptor,
                        }
                    )
                    sequence += 1
        pairs, _ = MODEL._pair_samples(rows)
        grouped: dict[object, list[MODEL._Pair]] = {}
        for pair in pairs:
            grouped.setdefault(pair.key, []).append(pair)
        fits = {
            key: MODEL._fit_variant(members, "plugin_delta_ns")
            for key, members in grouped.items()
        }
        adjusted, calibration = MODEL._anchor_adjusted_absolute_estimates(
            pairs,
            tuple(fits),
            {key: None for key in fits},
            {key: fit.batch_levels for key, fit in fits.items()},
            {key: fit.batch_reference for key, fit in fits.items()},
        )
        anchor_key = next(
            key for key in adjusted if key.pattern == MODEL.STABILITY_ANCHOR_PATTERN
        )
        self.assertEqual(calibration["status"], "available")
        self.assertAlmostEqual(
            calibration["metrics"]["plugin_off_to_primary_scale"], 0.5
        )
        self.assertAlmostEqual(adjusted[anchor_key], 4.0, places=6)
        self.assertEqual(
            calibration["metrics"]["position_log_scale:head"], 0.0
        )
        self.assertEqual(
            calibration["metrics"]["position_log_scale:tail"], 0.0
        )

    def test_anchor_position_and_batch_nuisance_are_explicit(self) -> None:
        rows = [dict(row) for row in synthetic_samples()]
        for row in rows:
            row["super_run_id"] = row["run_id"]
            row["super_run_order"] = int(str(row["run_id"]).rsplit("-", 1)[1])
            row["anchor_position"] = "not-anchor"
        sequence = max(int(row["sequence"]) for row in rows) + 1
        descriptor = {"size": 4, "bytes": "3345b502", "mnemonic": "div"}
        for run_index in range(3):
            run = f"cold-{run_index}"
            strata = (
                ("head", 24_000, 0.60),
                ("body", 12_000, 0.50),
                ("body", 24_000, 0.50),
                ("body", 48_000, 0.55),
                ("tail", 24_000, 0.45),
            )
            for pair_index, (position, count, scale) in enumerate(strata, 1):
                for role in ("probe", "baseline"):
                    primary_delta = 4.0 * count if role == "probe" else 0.0
                    rows.append(
                        {
                            "run_id": run,
                            "run_order": run_index,
                            "super_run_id": run,
                            "super_run_order": run_index,
                            "block_id": pair_index,
                            "pair_id": f"anchor-{run_index}-{pair_index}",
                            "sequence": sequence,
                            "role": role,
                            "instruction": "div",
                            "encoding_bytes": 4,
                            "pattern": MODEL.STABILITY_ANCHOR_PATTERN,
                            "anchor_position": position,
                            "requested_count": count,
                            "target_count": count if role == "probe" else 0,
                            "total_instruction_count": count + 1,
                            "plugin_thread_cpu_ns": 1_000_000.0 + primary_delta,
                            "guest_ns": 1_000_000.0 + primary_delta * 1.25,
                            "plugin_off_guest_ns": (
                                1_000_000.0 + primary_delta / scale
                            ),
                            "timer_reads": 2,
                            "plugin_mode": "timing",
                            "translations_during_window": 0,
                            "baseline_kind": "empty",
                            "target_descriptor": descriptor,
                        }
                    )
                    sequence += 1
        pairs, _ = MODEL._pair_samples(rows)
        calibration = MODEL._anchor_super_run_calibration(pairs)

        self.assertEqual(calibration["status"], "available")
        self.assertAlmostEqual(
            calibration["metrics"]["plugin_off_to_primary_scale"], 0.50
        )
        self.assertAlmostEqual(
            calibration["metrics"]["position_log_scale:head"],
            math.log(1.2),
        )
        self.assertAlmostEqual(
            calibration["metrics"]["position_log_scale:tail"],
            math.log(0.9),
        )
        self.assertAlmostEqual(
            calibration["metrics"]["batch_log_scale:48000"],
            math.log(1.1),
        )

    def test_raw_bytes_aq_rl_and_csr_are_part_of_the_key(self) -> None:
        amo_base = (2 << 20) | (1 << 15) | (3 << 12) | 0x2F
        csr_base = (1 << 15) | (1 << 12) | (1 << 7) | 0x73
        variants = (
            ("amoadd.d", amo_base, 1.4),
            ("amoadd.d", amo_base | (1 << 26) | (1 << 25), 1.7),
            ("csrrw", csr_base | (0x100 << 20), 2.1),
            ("csrrw", csr_base | (0xC00 << 20), 2.4),
        )
        rows: list[dict[str, object]] = []
        sequence = 0
        for variant_index, (mnemonic, word, weight) in enumerate(variants):
            descriptor = {
                "size": 4,
                "bytes": word.to_bytes(4, "little").hex(),
                "mnemonic": mnemonic,
            }
            for pair_index in range(8):
                count = (10_000, 20_000, 40_000)[pair_index % 3]
                pair = f"{variant_index}-{pair_index}"
                probe_first = pair_index % 4 in {0, 3}
                roles = ("probe", "baseline") if probe_first else ("baseline", "probe")
                for role in roles:
                    cpu = 1_000_000.0 + (weight * count if role == "probe" else 0.0)
                    rows.append(
                        {
                            "run_id": "encoding-run",
                            "block_id": pair_index,
                            "pair_id": pair,
                            "sequence": sequence,
                            "role": role,
                            "instruction": mnemonic,
                            "encoding_bytes": 4,
                            "pattern": "dependency",
                            "requested_count": count,
                            "target_count": count if role == "probe" else 0,
                            "total_instruction_count": count + 1,
                            "plugin_thread_cpu_ns": cpu,
                            "guest_ns": cpu,
                            "timer_reads": 2,
                            "baseline_kind": "empty",
                            "target_descriptor": descriptor,
                        }
                    )
                    sequence += 1
        result = fit_microbenchmark_weight_model(rows, bootstrap_replicates=0)
        keys = [item["key"] for item in result["instructions"]]
        amo = [key for key in keys if key["mnemonic"] == "amoadd.d"]
        csr = [key for key in keys if key["mnemonic"] == "csrrw"]
        self.assertEqual({(key["aq"], key["rl"]) for key in amo}, {(False, False), (True, True)})
        self.assertEqual({key["csr"] for key in csr}, {0x100, 0xC00})
        self.assertEqual(len({key["bytes"] for key in keys}), 4)

    def test_same_semantic_class_keeps_distinct_raw_encodings(self) -> None:
        rows: list[dict[str, object]] = []
        sequence = 0
        for variant, (encoding, weight) in enumerate(
            (("13051500", 1.2), ("13052500", 1.5))
        ):
            for pair_index in range(4):
                count = (10_000, 20_000, 40_000, 20_000)[pair_index]
                for role in ("probe", "baseline"):
                    rows.append(
                        {
                            "run_id": "raw-encoding-run",
                            "block_id": pair_index,
                            "pair_id": f"{variant}-{pair_index}",
                            "sequence": sequence,
                            "role": role,
                            "instruction": "addi",
                            "encoding_bytes": 4,
                            "pattern": "dependency",
                            "requested_count": count,
                            "target_count": count if role == "probe" else 0,
                            "total_instruction_count": count + 1,
                            "plugin_thread_cpu_ns": (
                                1_000_000.0
                                + (weight * count if role == "probe" else 0.0)
                            ),
                            "guest_ns": (
                                1_000_000.0
                                + (weight * count if role == "probe" else 0.0)
                            ),
                            "timer_reads": 2,
                            "plugin_mode": "timing",
                            "translations_during_window": 0,
                            "baseline_kind": "empty",
                            "target_descriptor": {
                                "size": 4,
                                "bytes": encoding,
                                "mnemonic": "addi",
                                "encoding_key": "rv64:32:i:addi",
                            },
                        }
                    )
                    sequence += 1
        result = fit_microbenchmark_weight_model(rows, bootstrap_replicates=0)
        keys = [item["key"] for item in result["instructions"]]
        self.assertEqual(len(keys), 2)
        self.assertEqual(
            {key["encoding_key"] for key in keys},
            {"raw:4:13051500", "raw:4:13052500"},
        )
        self.assertEqual(
            {key["semantic_encoding_key"] for key in keys},
            {"rv64:32:i:addi"},
        )

    def test_merged_plugin_jsonl_descriptor_schema_is_accepted(self) -> None:
        rows = synthetic_samples()[:2]
        descriptor = {"size": 4, "bytes": "13000000", "mnemonic": "nop"}
        for row in rows:
            row.pop("encoding_hex", None)
            row["schema"] = "mygo.riscv-instruction-weight-sample.v1"
            row["target_descriptor"] = descriptor
            row["exact_counts"] = {
                "4:13000000:nop": row["target_count"],
                "4:67800000:ret": row["total_instruction_count"] - row["target_count"],
            }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "merged.jsonl"
            path.write_text(
                "".join(json.dumps(row) + "\n" for row in rows),
                encoding="utf-8",
            )
            loaded = load_samples(path)
        self.assertEqual(loaded[0]["target_descriptor"]["bytes"], "13000000")
        # 两窗口构成完整 pair；解析阶段会验证 descriptor/raw bytes，而拟合至少
        # 需要四对，因此这里只调用 loader，不伪造统计结论。
        self.assertEqual(len(loaded), 2)

    def test_jsonl_tsv_and_csv_io(self) -> None:
        rows = synthetic_samples()[:2]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            jsonl = root / "samples.jsonl"
            jsonl.write_text(
                "".join(
                    "RV_WEIGHT_SAMPLE " + json.dumps(row) + "\n" for row in rows
                ),
                encoding="utf-8",
            )
            self.assertEqual(load_samples(jsonl), rows)

            tsv = root / "samples.tsv"
            fields = list(rows[0])
            with tsv.open("w", encoding="utf-8", newline="") as stream:
                writer = __import__("csv").DictWriter(
                    stream, fieldnames=fields, delimiter="\t"
                )
                writer.writeheader()
                writer.writerows(rows)
            loaded = load_samples(tsv)
            self.assertEqual(loaded[0]["sequence"], rows[0]["sequence"])
            self.assertEqual(
                loaded[0]["plugin_thread_cpu_ns"],
                rows[0]["plugin_thread_cpu_ns"],
            )

            result = fit_microbenchmark_weight_model(
                synthetic_samples(), bootstrap_replicates=9
            )
            output = root / "weights.csv"
            write_csv(result, output)
            header = output.read_text(encoding="utf-8").splitlines()[0]
            self.assertIn("simultaneous_ci_low", header)
            self.assertIn("quality_failures", header)

    def test_sparse_wls_matches_dense_reference_bit_for_bit(self) -> None:
        matrix = [
            [1.0, 0.0, -1.0, -0.50],
            [1.0, 1.0, 1.0, -0.25],
            [1.0, 0.0, 1.0, 0.00],
            [1.0, 1.0, -1.0, 0.25],
            [1.0, 0.0, 1.0, 0.50],
            [1.0, 1.0, -1.0, 0.75],
        ]
        response = [0.75, 1.60, 1.15, 0.90, 1.80, 1.05]
        weights = [1.0, 0.75, 1.25, 0.50, 1.50, 0.90]

        def dense_reference() -> tuple[list[float], list[list[float]]]:
            width = len(matrix[0])
            gram = [[0.0] * width for _ in range(width)]
            rhs = [0.0] * width
            for row, value, weight in zip(matrix, response, weights):
                for left in range(width):
                    rhs[left] += weight * row[left] * value
                    for right in range(left + 1):
                        gram[left][right] += weight * row[left] * row[right]
            for left in range(width):
                for right in range(left):
                    gram[right][left] = gram[left][right]
            trace = sum(gram[index][index] for index in range(width))
            ridge = max(1e-14, trace * 1e-13 / max(1, width))
            for index in range(1, width):
                gram[index][index] += ridge
            inverse = MODEL._invert(gram)
            coefficients = [
                math.fsum(
                    inverse[row][column] * rhs[column]
                    for column in range(width)
                )
                for row in range(width)
            ]
            return coefficients, inverse

        actual_coefficients, actual_inverse = MODEL._wls(
            matrix, response, weights
        )
        expected_coefficients, expected_inverse = dense_reference()
        for actual, expected in zip(
            actual_coefficients, expected_coefficients, strict=True
        ):
            self.assertAlmostEqual(actual, expected, delta=1e-12)
        for actual_row, expected_row in zip(
            actual_inverse, expected_inverse, strict=True
        ):
            for actual, expected in zip(
                actual_row, expected_row, strict=True
            ):
                self.assertAlmostEqual(actual, expected, delta=1e-12)

    def test_numpy_backend_matches_python_on_synthetic_model(self) -> None:
        try:
            MODEL._numpy_module()
        except MicrobenchmarkModelError:
            self.skipTest("NumPy backend is not installed")
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] in {"nop", "addi"}
        ]
        python_result = fit_microbenchmark_weight_model(
            rows,
            bootstrap_replicates=19,
            seed=1771,
            linear_algebra_backend="python",
        )
        numpy_result = fit_microbenchmark_weight_model(
            rows,
            bootstrap_replicates=19,
            seed=1771,
            linear_algebra_backend="numpy",
        )

        self.assertEqual(numpy_result["linear_algebra_backend"], "numpy")
        by_key = {
            (item["key"]["mnemonic"], item["key"]["pattern"]): item
            for item in python_result["instructions"]
        }
        for item in numpy_result["instructions"]:
            reference = by_key[
                (item["key"]["mnemonic"], item["key"]["pattern"])
            ]
            self.assertAlmostEqual(
                item["unconstrained_ns_per_instruction"],
                reference["unconstrained_ns_per_instruction"],
                delta=1e-8,
            )
            for actual, expected in zip(
                item["simultaneous_ci"], reference["simultaneous_ci"]
            ):
                self.assertAlmostEqual(actual, expected, delta=1e-7)
            self.assertEqual(
                item["quality_failures"], reference["quality_failures"]
            )

    def test_auto_backend_uses_numpy_when_available(self) -> None:
        try:
            MODEL._numpy_module()
        except MicrobenchmarkModelError:
            self.skipTest("NumPy backend is not installed")
        rows = [
            row
            for row in synthetic_samples()
            if row["instruction"] == "nop"
        ]

        result = fit_microbenchmark_weight_model(
            rows, bootstrap_replicates=0
        )

        self.assertEqual(result["linear_algebra_backend"], "numpy")

    def test_default_cli_jobs_are_bounded_by_available_cpus(self) -> None:
        with mock.patch.object(MODEL.os, "cpu_count", return_value=64):
            self.assertEqual(MODEL._default_cli_jobs(), 16)
        with mock.patch.object(MODEL.os, "cpu_count", return_value=4):
            self.assertEqual(MODEL._default_cli_jobs(), 4)
        with mock.patch.object(MODEL.os, "cpu_count", return_value=None):
            self.assertEqual(MODEL._default_cli_jobs(), 1)


if __name__ == "__main__":
    unittest.main()
