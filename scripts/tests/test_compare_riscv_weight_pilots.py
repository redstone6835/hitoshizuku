"""12-super-run 指令权重 pilot 比较器测试。"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts.compare_riscv_weight_pilots import (
    PilotComparisonError,
    bind_refit_artifacts,
    compare_pilots,
    load_pilot,
    main,
    render_refit_commands,
)


def pilot(*, improved: bool) -> dict[str, object]:
    factor = 0.50 if improved else 1.0
    instructions = []
    for index, mnemonic in enumerate(("addi", "div", "rem")):
        width = (1.0 + index * 0.1) * factor
        instructions.append(
            {
                "key": {"mnemonic": mnemonic, "size": 4, "pattern": "dependency"},
                "runs": 12,
                "simultaneous_ci": [5.0 - width / 2.0, 5.0 + width / 2.0],
                "anchor_adjusted": {"simultaneous_ci": [5.0 - width / 2.0, 5.0 + width / 2.0]},
                "cross_run_random_effects": {"tau_squared": (0.4 + index * 0.1) * factor},
                "guest_time_check": {
                    "status": "accepted" if improved or index else "divergent",
                    "accepted_ratio_range": [0.85, 1.15],
                    "simultaneous_ratio_ci": [1.0 - 0.2 * factor, 1.0 + 0.2 * factor],
                    "simultaneous_difference_ci": None,
                    "zero_cost_absolute_margin_ns": 0.15,
                },
                "plugin_off_check": {
                    "status": "accepted" if improved or index < 2 else "divergent",
                    "accepted_ratio_range": [0.85, 1.15],
                    "simultaneous_ratio_ci": None,
                    "simultaneous_difference_ci": [-0.3 * factor, 0.3 * factor],
                    "zero_cost_absolute_margin_ns": 0.15,
                },
                "raw_adjusted_discrepancy": {"equivalent": True},
                "estimator_sensitivity": {"equivalent": improved or index > 0},
                "leave_one_super_run_out_sensitivity": {
                    "complete": True,
                    "stable": True,
                    "maximum_absolute_shift_ns": 0.02,
                    "equivalence_margin_ns": 0.10,
                },
            }
        )
    weights = {
        "schema_version": 3,
        "model": "current-test-model",
        "confidence": 0.95,
        "primary_response": "marker-only",
        "instruction_key": "full-key",
        "linear_algebra_backend": "numpy",
        "quality_thresholds": {"minimum_bootstrap_replicates": 4999},
        "instructions": instructions,
        "simultaneous_inference": {
            "automatic_block_length": 4,
            "automatic_run_block_length_rule": "cube-root",
            "block_length": 4,
            "block_length_unit": "probe-round-blocks",
            "familywise_confidence": 0.95,
            "method": "super-run bootstrap",
            "minimum_valid_fraction": 0.99,
            "requested_replicates": 4999,
            "complete_max_statistic_replicates": 4999,
            "run_block_length": 2,
            "run_resampling": "moving-block",
            "super_run_is_highest_cluster": True,
        },
        "joint_raw_adjusted_inference": {
            "familywise_confidence": 0.95,
            "method": "joint bootstrap",
            "point_family_size": 20,
            "requested_replicates": 4999,
            "complete_replicates": 4999,
            "complete_max_statistic_replicates": 4999,
        },
        "positive_anchor_scale_inference": {
            "status": "accepted",
            "nuisance_interval_gate_passed": True,
            "per_super_run": [
                {"super_run": f"super-{index}", "plugin_off_to_primary_scale": 1.0}
                for index in range(12)
            ],
            "simultaneous_intervals": {
                "plugin_off_to_primary_scale": [1.0 - 0.1 * factor, 1.0 + 0.1 * factor]
            },
            "nuisance_log_scale_intervals": {
                "position_log_scale:head": [-0.1 * factor, 0.1 * factor],
                "position_log_scale:tail": [-0.08 * factor, 0.08 * factor],
            },
        },
        "publication_gate": {
            "components": {
                "anchor_adjusted": True,
                "estimator_sensitivity": True,
                "joint_bootstrap": True,
                "positive_anchor": True,
                "raw": True,
                "raw_adjusted_discrepancy": True,
                "single_super_run_influence": True,
                "statistical_core": True,
            }
        },
    }
    launches = [
        {
            "window_aperf_mperf_ratio": 1.2,
            "selected_cpu_external_interrupts_per_second": 0.0,
        }
        for _ in range(48)
    ]
    audit = {
        "schema": "mygo.riscv-weight-host-audit.v1",
        "status": "accepted" if improved else "rejected",
        "inputs": {"isolation_state": {"path": "isolation-state.json", "sha256": "fixture"}},
        "isolation_state_checks_required": True,
        "thresholds": {
            "require_window_frequency": True,
            "require_interrupt_evidence": True,
        },
        "launches": launches,
        "failures": [],
        "minimum_window_aperf_mperf_ratio": 1.2,
        "window_frequency_coefficient_of_variation": 0.001,
        "temperature_span_c": 1.0,
    }
    weights["host_isolation_audit"] = audit
    sample_identity = "candidate-fixture" if improved else "baseline-fixture"
    weights["pilot_comparison_input_bindings"] = {
        "samples": {"path": "samples.jsonl", "sha256": sample_identity, "size": 1}
    }
    return {
        "root": "/fixture",
        "weights": weights,
        "host_audit": audit,
        "artifact_bindings": {
            "samples": {
                "path": "samples.jsonl",
                "sha256": sample_identity,
                "size": 1,
                "binding_available": True,
                "all_available_bindings_match": True,
            }
        },
    }


class PilotComparisonTests(unittest.TestCase):
    def test_accepts_broad_preregistered_improvement(self) -> None:
        report = compare_pilots(pilot(improved=False), pilot(improved=True))

        self.assertTrue(report["accepted_for_formal_run"])
        self.assertEqual(report["failed_gates"], [])
        self.assertAlmostEqual(
            report["metrics"]["raw_interval_width"]["median_paired_ratio"],
            0.5,
        )
        self.assertEqual(
            report["decision_scope"],
            "engineering pilot gate only; not a high-confidence statistical claim",
        )
        self.assertIn("before collecting", report["preregistration_provenance"])
        self.assertEqual(
            report["formal_follow_up"],
            "passing authorizes only the preregistered 205-super-run experiment",
        )

    def test_rejects_candidate_with_failed_host_audit(self) -> None:
        candidate = pilot(improved=True)
        candidate["host_audit"]["status"] = "rejected"  # type: ignore[index]

        report = compare_pilots(pilot(improved=False), candidate)

        self.assertFalse(report["accepted_for_formal_run"])
        self.assertIn("candidate_host_audit_accepted", report["failed_gates"])

    def test_rejects_candidate_without_strict_isolation_evidence(self) -> None:
        candidate = pilot(improved=True)
        candidate["host_audit"]["isolation_state_checks_required"] = False  # type: ignore[index]
        candidate["weights"]["host_isolation_audit"] = candidate["host_audit"]  # type: ignore[index]

        with self.assertRaisesRegex(PilotComparisonError, "isolation-state"):
            compare_pilots(pilot(improved=False), candidate)

    def test_requires_candidate_robustness_and_discrepancy_gates(self) -> None:
        candidate = pilot(improved=True)
        candidate["weights"]["instructions"][0]["estimator_sensitivity"]["equivalent"] = False  # type: ignore[index]
        candidate["weights"]["instructions"][1]["raw_adjusted_discrepancy"]["equivalent"] = False  # type: ignore[index]

        report = compare_pilots(pilot(improved=False), candidate)

        self.assertFalse(report["accepted_for_formal_run"])
        self.assertIn("all_estimator_sensitivity_checks_equivalent", report["failed_gates"])
        self.assertIn("all_raw_adjusted_discrepancies_equivalent", report["failed_gates"])

    def test_rejects_q90_regression_hidden_by_median(self) -> None:
        candidate = pilot(improved=True)
        candidate["weights"]["instructions"][2]["simultaneous_ci"] = [0.0, 20.0]  # type: ignore[index]

        report = compare_pilots(pilot(improved=False), candidate)

        self.assertFalse(report["accepted_for_formal_run"])
        self.assertIn("raw_interval_q90_not_wider", report["failed_gates"])

    def test_requires_same_current_analysis_contract(self) -> None:
        candidate = pilot(improved=True)
        candidate["weights"]["quality_thresholds"]["minimum_bootstrap_replicates"] = 999  # type: ignore[index]

        with self.assertRaisesRegex(PilotComparisonError, "分析合同不一致"):
            compare_pilots(pilot(improved=False), candidate)

    def test_rejects_same_sample_identity_on_both_sides(self) -> None:
        baseline = pilot(improved=False)
        candidate = pilot(improved=True)
        candidate["artifact_bindings"]["samples"]["sha256"] = baseline["artifact_bindings"]["samples"]["sha256"]  # type: ignore[index]

        with self.assertRaisesRegex(PilotComparisonError, "同一个 samples.jsonl"):
            compare_pilots(baseline, candidate)

    def test_requires_new_sensitivity_components(self) -> None:
        baseline = pilot(improved=False)
        del baseline["weights"]["instructions"][0]["estimator_sensitivity"]  # type: ignore[index]

        with self.assertRaisesRegex(PilotComparisonError, "estimator_sensitivity"):
            compare_pilots(baseline, pilot(improved=True))

    def test_loads_directories_and_cli_returns_gate_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, data in (("old", pilot(improved=False)), ("new", pilot(improved=True))):
                target = root / name
                target.mkdir()
                samples = target / "samples.jsonl"
                telemetry = target / "host-telemetry.jsonl"
                design = target / "run-design.jsonl"
                samples.write_text(f'{{"sample":"{name}"}}\n', encoding="utf-8")
                telemetry.write_text('{"phase":"before"}\n', encoding="utf-8")
                design.write_text('{"design":"ABBA"}\n', encoding="utf-8")
                isolation = target / "isolation-state.json"
                isolation.write_text('{"selected_cpus":[7]}\n', encoding="utf-8")
                import hashlib

                data["host_audit"]["inputs"] = {  # type: ignore[index]
                    "telemetry": {"path": str(telemetry), "sha256": hashlib.sha256(telemetry.read_bytes()).hexdigest()},
                    "run_design": {"path": str(design), "sha256": hashlib.sha256(design.read_bytes()).hexdigest()},
                    "isolation_state": {"path": str(isolation), "sha256": hashlib.sha256(isolation.read_bytes()).hexdigest()},
                }
                data["weights"]["host_isolation_audit"] = data["host_audit"]  # type: ignore[index]
                data["weights"].pop("pilot_comparison_input_bindings", None)  # type: ignore[index]
                (target / "weights.json").write_text(json.dumps(data["weights"]), encoding="utf-8")
                (target / "host-audit.json").write_text(json.dumps(data["host_audit"]), encoding="utf-8")

                bind_refit_artifacts(target)

            loaded = load_pilot(root / "old")
            self.assertEqual(loaded["weights"]["schema_version"], 3)
            self.assertTrue(
                loaded["artifact_bindings"]["samples"]["binding_available"]
            )
            self.assertEqual(main([str(root / "old"), str(root / "new")]), 0)

            (root / "new" / "host-telemetry.jsonl").write_text("tampered\n", encoding="utf-8")
            with self.assertRaisesRegex(PilotComparisonError, "SHA-256"):
                load_pilot(root / "new")

    def test_refit_template_fixes_current_model_seed_and_replicates(self) -> None:
        commands = render_refit_commands("old-pilot", "new-pilot")

        self.assertEqual(commands.count("--bootstrap 4999"), 2)
        self.assertEqual(commands.count("--seed 5396035"), 2)
        self.assertIn("cp old-pilot/samples.jsonl", commands)
        self.assertIn("cp new-pilot/samples.jsonl", commands)
        self.assertEqual(commands.count("--bind-refit-artifacts"), 2)
        self.assertEqual(
            main(["old-pilot", "new-pilot", "--print-refit-commands"]),
            0,
        )


if __name__ == "__main__":
    unittest.main()
