"""RISC-V 指令权重样本合并与 catalog 映射回归测试。"""

from __future__ import annotations

import copy
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))

import rv_instruction_microbench_model as MICROBENCH_MODEL
from rv_instruction_microbench_model import fit_microbenchmark_weight_model
from riscv_weight_model_seal import (
    FWER_COVERAGE_CLAIM,
    FWER_METHOD,
    MONTE_CARLO_METHOD,
    REPLICATE_PARTITION_METHOD,
    seal_model_document,
)


def load_script(module_name: str, filename: str):
    spec = importlib.util.spec_from_file_location(module_name, SCRIPTS / filename)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MERGER = load_script(
    "merge_riscv_instruction_weight_samples",
    "merge-riscv-instruction-weight-samples.py",
)
MAPPER = load_script(
    "map_riscv_instruction_weights",
    "map-riscv-instruction-weights.py",
)


def guest_row(
    sequence: int,
    role: str,
    *,
    run_id: str = "run-1",
    pair_id: str = "pair-1",
    instruction: str = "mul",
    pattern: str = "independent",
    baseline_instruction: str = "addi",
) -> dict[str, str]:
    return {
        "run_id": run_id,
        "block_id": "block-1",
        "pair_id": pair_id,
        "sequence": str(sequence),
        "role": role,
        "order": "probe-first",
        "instruction": instruction,
        "encoding_bytes": "4",
        "pattern": pattern,
        "count_level": "0",
        "requested_count": "100",
        "blocks": "1",
        "slots_per_block": "100",
        "executed_instruction": instruction
        if role == "probe"
        else baseline_instruction,
        "guest_elapsed_ns": "1000",
        "rdtime_delta": "900",
        "timer_reads": "2",
        "baseline_instruction": baseline_instruction,
        "baseline_encoding_bytes": "4",
    }


def differential_guest_row(
    sequence: int,
    role: str,
    *,
    suite: str,
    contrast: str,
    variant: str,
    context: str,
    instruction: str,
    pattern: str,
    run_id: str = "run-1",
    pair_id: str = "pair-1",
) -> dict[str, str]:
    row = guest_row(sequence, role, run_id=run_id, pair_id=pair_id)
    calibration = suite == "differential-calibration-v2"
    baseline_instruction = "empty" if calibration else "nop"
    baseline_encoding_bytes = "0" if calibration else "4"
    row.update(
        {
            "version": "2",
            "probe_contract": MERGER._DIFFERENTIAL_PROBE_CONTRACT,
            "operand_set": MERGER._DIFFERENTIAL_OPERAND_SET,
            "calibration_profile": "standard-v2",
            "suite": suite,
            "contrast": contrast,
            "differential_variant": variant,
            "context": context,
            "instruction": instruction,
            "pattern": pattern,
            "executed_instruction": (
                instruction if role == "probe" else baseline_instruction
            ),
            "baseline_instruction": baseline_instruction,
            "baseline_encoding_bytes": baseline_encoding_bytes,
            "control_instruction": "empty-call",
            "requested_count": "1024",
            "slots_per_block": "1024",
            "anchor_position": (
                "body" if suite == "stability-anchor-v1" else "not-anchor"
            ),
        }
    )
    return row


def descriptor(raw_bytes: str, mnemonic: str, count: int) -> dict[str, object]:
    return {
        "size": 4,
        "bytes": raw_bytes,
        "mnemonic": mnemonic,
        "count": count,
    }


def timing_windows() -> dict[int, dict[str, object]]:
    return {
        1: {
            "instruction_count": 120,
            "counts": [
                descriptor("b3003102", "mul", 100),
                descriptor("63841000", "beq", 20),
            ],
        },
        2: {
            "instruction_count": 120,
            "counts": [
                descriptor("93801000", "addi", 100),
                descriptor("63841000", "beq", 20),
            ],
        },
    }


def differential_windows(*, alternating: bool = False) -> dict[int, dict[str, object]]:
    target_count = 1024
    common = [
        descriptor("13050500", "addi", 2 * target_count if alternating else target_count)
    ]
    if alternating:
        common.append(descriptor("b3e2b202", "rem", target_count))
    common.append(descriptor("63841000", "beq", 20))
    total = sum(int(item["count"]) for item in common) + target_count
    return {
        1: {
            "instruction_count": total,
            "counts": [descriptor("3345b502", "div", target_count), *common],
        },
        2: {
            "instruction_count": total,
            "counts": [descriptor("13000000", "nop", target_count), *common],
        },
    }


def calibration_windows() -> dict[int, dict[str, object]]:
    windows: dict[int, dict[str, object]] = {}
    sequence = 0
    for multiplier in (1, 4, 16):
        requested_count = multiplier * 1024
        sequence += 1
        windows[sequence] = {
            "instruction_count": requested_count + 20,
            "counts": [
                descriptor("13000000", "nop", requested_count),
                descriptor("63841000", "beq", 20),
            ],
        }
        sequence += 1
        windows[sequence] = {
            "instruction_count": 20,
            "counts": [descriptor("63841000", "beq", 20)],
        }
    return windows


def catalog_records(
    instructions: list[dict[str, object]],
    *,
    target: str = "riscv64",
    schema: str = MAPPER.CATALOG_SCHEMA,
    quality_updates: dict[str, object] | None = None,
) -> list[dict[str, object]]:
    quality: dict[str, object] = {
        "schema": schema,
        "type": "quality",
        "translated_blocks": 1,
        "records": 1,
        "write_errors": 0,
        "dropped_blocks": 0,
        "tracking_drops": 0,
    }
    if quality_updates:
        quality.update(quality_updates)
    return [
        {"schema": schema, "type": "header", "target": target},
        {
            "schema": schema,
            "type": "tb",
            "instruction_count": len(instructions),
            "descriptor_overflow": 0,
            "decode_errors": 0,
            "instructions": instructions,
        },
        quality,
    ]


def load_catalog_records(
    records: list[dict[str, object]], *, expected_key_count: int | None = None
):
    with tempfile.TemporaryDirectory() as directory:
        catalog_path = Path(directory) / "catalog.jsonl"
        catalog_path.write_text(
            "".join(json.dumps(record) + "\n" for record in records),
            encoding="utf-8",
        )
        return MAPPER.load_catalog(
            catalog_path, expected_key_count=expected_key_count
        )


def measured_context(
    encoding_key: str,
    *,
    pattern: str = "independent",
    value: float = 2.5,
    quality: str = "high-confidence",
) -> dict[str, object]:
    return {
        "key": {
            "mnemonic": encoding_key.rsplit(":", 1)[-1],
            "size": 4,
            "encoding_key": "raw:4:13000000",
            "semantic_encoding_key": encoding_key,
            "bytes": "13000000",
            "aq": False,
            "rl": False,
            "csr": None,
            "pattern": pattern,
        },
        "ns_per_instruction": value,
        "published_ns_per_instruction": (
            value if quality == "high-confidence" else None
        ),
        "anchor_adjusted": {
            "ns_per_instruction": value,
            "simultaneous_ci": [value * 0.9, value * 1.1],
        },
        "calibration_only": False,
        "relative_weight": 1.0,
        "simultaneous_ci": [value * 0.9, value * 1.1],
        "quality": quality,
        "quality_failures": [],
    }


def model_document(
    instructions: list[dict[str, object]],
) -> dict[str, object]:
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
    document = {
        "schema_version": MAPPER.MODEL_SCHEMA_VERSION,
        "instruction_key": MAPPER.MODEL_INSTRUCTION_KEY,
        "confidence": 0.95,
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
        "simultaneous_inference": {
            "familywise_confidence": family_confidence,
            "requested_replicates": 4999,
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
            "familywise_confidence": family_confidence,
            "requested_replicates": 4999,
            "complete_replicates": 4999,
            "complete_family_replicates": 4999,
            "complete_max_statistic_replicates": 4000,
            "critical_value_monte_carlo": dict(finite_evidence),
        },
        "model": (
            "paired-huber-heteroscedastic-hierarchical-moving-block-"
            "max-standardized-deviation"
        ),
        "instructions": instructions,
        "publication_gate": {
            "passed": True,
            "failures": [],
            "components": {
                "statistical_core": True,
                "raw": True,
                "anchor_adjusted": True,
                "positive_anchor": True,
                "raw_adjusted_discrepancy": True,
                "estimator_sensitivity": True,
                "single_super_run_influence": True,
                "joint_bootstrap": True,
                "host_isolation": True,
                "ml_validation": True,
            },
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
        "ml_validation": {
            "schema": "mygo.riscv-instruction-ml-validation.v3",
            "conclusion": {
                "status": "supported",
                "high_confidence_status": "supported",
                "high_confidence_gate_passed": True,
                "may_publish_weights": False,
            },
        },
        "ml_validation_evidence": {
            "schema": "mygo.riscv-instruction-ml-validation.v3",
            "checks": {"all_subgates": True},
            "binding_checks": {
                "samples": True,
                "statistical_weights_pre_finalization": True,
            },
        },
    }
    seal_model_document(document)
    return document


def model_contract_samples() -> list[dict[str, object]]:
    samples: list[dict[str, object]] = []
    sequence = 0
    for run_index in range(3):
        for pair_index in range(6):
            batch = (10_000, 20_000, 40_000)[pair_index % 3]
            roles = (
                ("probe", "baseline")
                if pair_index % 2 == 0
                else ("baseline", "probe")
            )
            for role in roles:
                cpu_ns = 1_000_000.0 + (
                    1.25 * batch if role == "probe" else 0.0
                )
                samples.append(
                    {
                        "run_id": f"contract-{run_index}",
                        "block_id": pair_index,
                        "pair_id": f"contract-{run_index}-{pair_index}",
                        "sequence": sequence,
                        "role": role,
                        "instruction": "addi",
                        "encoding_bytes": 4,
                        "pattern": "independent",
                        "requested_count": batch,
                        "target_count": batch if role == "probe" else 0,
                        "total_instruction_count": (
                            batch + 1 if role == "probe" else 1
                        ),
                        "plugin_thread_cpu_ns": cpu_ns,
                        "guest_ns": cpu_ns,
                        "plugin_off_guest_ns": cpu_ns,
                        "timer_reads": 2,
                        "plugin_mode": "timing",
                        "translations_during_window": 0,
                        "baseline_kind": "empty",
                        "target_descriptor": {
                            "size": 4,
                            "bytes": "93801000",
                            "mnemonic": "addi",
                            "encoding_key": "rv64:32:i:addi",
                        },
                    }
                )
                sequence += 1
    return samples


class SampleMergeTests(unittest.TestCase):
    def test_paired_purity_requires_an_exact_canonical_contrast(self) -> None:
        rows = MERGER.merge_samples(
            [guest_row(1, "probe"), guest_row(2, "baseline")], timing_windows()
        )

        self.assertEqual(len(rows), 2)
        for row in rows:
            self.assertEqual(
                row["schema"], "mygo.riscv-instruction-weight-sample.v1"
            )
            self.assertEqual(row["paired_contrast_purity"], 1.0)
            self.assertNotIn("probe_version", row)
            self.assertNotIn("suite", row)
            self.assertNotIn("contrast", row)
            self.assertNotIn("context", row)
            self.assertEqual(
                row["target_descriptor"]["encoding_key"], "rv64:32:m:mul"
            )

    def test_versioned_differential_metadata_is_validated_and_preserved(
        self,
    ) -> None:
        rows_in = [
            differential_guest_row(
                1,
                "probe",
                suite="div-rem-dataflow-v2",
                contrast="div-dataflow",
                variant="independent",
                context="per-slot-reset-nondegenerate",
                instruction="div",
                pattern="independent-reset",
            ),
            differential_guest_row(
                2,
                "baseline",
                suite="div-rem-dataflow-v2",
                contrast="div-dataflow",
                variant="independent",
                context="per-slot-reset-nondegenerate",
                instruction="div",
                pattern="independent-reset",
            ),
        ]

        rows = MERGER.merge_samples(rows_in, differential_windows())

        for row in rows:
            self.assertEqual(
                row["schema"], "mygo.riscv-instruction-weight-sample.v2"
            )
            self.assertEqual(row["probe_version"], 2)
            self.assertEqual(
                row["probe_contract"], MERGER._DIFFERENTIAL_PROBE_CONTRACT
            )
            self.assertEqual(row["operand_set"], MERGER._DIFFERENTIAL_OPERAND_SET)
            self.assertEqual(row["suite"], "div-rem-dataflow-v2")
            self.assertEqual(row["contrast"], "div-dataflow")
            self.assertEqual(
                row["differential_variant"], "independent"
            )
            self.assertEqual(row["context"], "per-slot-reset-nondegenerate")

    def test_calibration_closes_nop_to_empty_and_resolves_control_chain(
        self,
    ) -> None:
        calibration = []
        sequence = 0
        for level, multiplier in enumerate((1, 4, 16)):
            for role in ("probe", "baseline"):
                sequence += 1
                row = differential_guest_row(
                    sequence,
                    role,
                    suite="differential-calibration-v2",
                    contrast="nop-reference",
                    variant="reference",
                    context="independent-nop",
                    instruction="nop",
                    pattern="independent",
                    pair_id=f"calibration-{level}",
                )
                row.update(
                    {
                        "count_level": str(level),
                        "blocks": str(multiplier),
                        "requested_count": str(multiplier * 1024),
                    }
                )
                calibration.append(row)
        calibration_rows = MERGER.merge_samples(
            calibration, calibration_windows()
        )

        for row in calibration_rows:
            self.assertEqual(row["baseline_kind"], "empty")
            self.assertNotIn("baseline_descriptor", row)
            self.assertEqual(
                row["target_descriptor"]["encoding_key"],
                "rv64:32:i:addi:form=nop",
            )

        target = [
            differential_guest_row(
                1,
                role,
                suite="div-rem-dataflow-v2",
                contrast="div-dataflow",
                variant="independent",
                context="per-slot-reset-nondegenerate",
                instruction="div",
                pattern="independent-reset",
                pair_id="target",
            )
            for role in ("probe", "baseline")
        ]
        target[1]["sequence"] = "2"
        target_rows = MERGER.merge_samples(target, differential_windows())

        pairs, assumed_empty = MICROBENCH_MODEL._pair_samples(
            [*calibration_rows, *target_rows]
        )
        keys = list({pair.key for pair in pairs})
        references = {pair.key: pair.control_reference for pair in pairs}
        controls, failures = MICROBENCH_MODEL._resolve_control_references(
            keys, references
        )

        self.assertFalse(assumed_empty)
        self.assertFalse(failures)
        by_mnemonic = {key.mnemonic: key for key in keys}
        nop_key = by_mnemonic["nop"]
        div_key = by_mnemonic["div"]
        self.assertIsNone(controls[nop_key])
        self.assertEqual(controls[div_key], nop_key)

        absolute, absolute_failures = MICROBENCH_MODEL._resolve_absolute(
            {nop_key: 0.75, div_key: 4.25}, controls
        )
        self.assertFalse(absolute_failures)
        self.assertEqual(absolute[nop_key], 0.75)
        self.assertEqual(absolute[div_key], 5.0)

    def test_long_calibration_profile_requires_long_batch_multiplier(self) -> None:
        row = differential_guest_row(
            1,
            "probe",
            suite="differential-calibration-v2",
            contrast="nop-reference",
            variant="reference",
            context="independent-nop",
            instruction="nop",
            pattern="independent",
        )
        row.update(
            {
                "calibration_profile": "long-window-v1",
                "count_level": "0",
                "blocks": "16",
                "requested_count": "16384",
            }
        )

        metadata = MERGER._guest_metadata(row)
        self.assertEqual(metadata["calibration_profile"], "long-window-v1")

        row["blocks"] = "1"
        row["requested_count"] = "1024"
        with self.assertRaisesRegex(MERGER.MergeError, "blocks 与 profile"):
            MERGER._guest_metadata(row)

    def test_calibration_profile_is_part_of_validation_signature(self) -> None:
        row = differential_guest_row(
            1,
            "probe",
            suite="differential-calibration-v2",
            contrast="nop-reference",
            variant="reference",
            context="independent-nop",
            instruction="nop",
            pattern="independent",
        )
        standard = MERGER._guest_signature(row)
        row["calibration_profile"] = "long-window-v1"
        row["blocks"] = "16"
        row["requested_count"] = "16384"
        long_window = MERGER._guest_signature(row)

        self.assertNotEqual(standard, long_window)

    def test_long_calibration_profile_keeps_non_calibration_batches_standard(
        self,
    ) -> None:
        row = differential_guest_row(
            1,
            "probe",
            suite="div-rem-dataflow-v2",
            contrast="div-dataflow",
            variant="independent",
            context="per-slot-reset-nondegenerate",
            instruction="div",
            pattern="independent-reset",
        )
        row["calibration_profile"] = "long-window-v1"

        metadata = MERGER._guest_metadata(row)
        self.assertEqual(metadata["calibration_profile"], "long-window-v1")

        row.update(
            {
                "count_level": "1",
                "blocks": "16",
                "requested_count": "16384",
            }
        )
        self.assertEqual(
            MERGER._guest_metadata(row)["calibration_profile"],
            "long-window-v1",
        )

        row.update({"blocks": "17", "requested_count": "17408"})
        with self.assertRaisesRegex(MERGER.MergeError, "standard profile"):
            MERGER._guest_metadata(row)

    def test_differential_input_rejects_mixed_calibration_profiles(self) -> None:
        calibration = [
            differential_guest_row(
                index,
                role,
                suite="differential-calibration-v2",
                contrast="nop-reference",
                variant="reference",
                context="independent-nop",
                instruction="nop",
                pattern="independent",
            )
            for index, role in enumerate(("probe", "baseline"), 1)
        ]
        calibration[1]["calibration_profile"] = "long-window-v1"
        calibration[1]["blocks"] = "16"
        calibration[1]["requested_count"] = "16384"

        with self.assertRaisesRegex(MERGER.MergeError, "元数据不一致"):
            MERGER._metadata_by_pair(calibration)

    def test_legacy_v2_metadata_defaults_to_standard_profile(self) -> None:
        row = differential_guest_row(
            1,
            "probe",
            suite="differential-calibration-v2",
            contrast="nop-reference",
            variant="reference",
            context="independent-nop",
            instruction="nop",
            pattern="independent",
        )
        del row["calibration_profile"]

        self.assertEqual(
            MERGER._guest_metadata(row)["calibration_profile"], "standard-v2"
        )

    def test_calibration_contract_is_strict(self) -> None:
        rows = {
            role: differential_guest_row(
                index,
                role,
                suite="differential-calibration-v2",
                contrast="nop-reference",
                variant="reference",
                context="independent-nop",
                instruction="nop",
                pattern="independent",
            )
            for index, role in enumerate(("probe", "baseline"), 1)
        }
        for row in rows.values():
            MERGER._guest_metadata(row)

        invalid = (
            ("probe", "baseline_instruction", "nop"),
            ("probe", "baseline_encoding_bytes", "4"),
            ("probe", "executed_instruction", "empty"),
            ("baseline", "executed_instruction", "nop"),
            ("probe", "contrast", "div-dataflow"),
            ("probe", "context", "evolving-dependency-chain"),
            ("probe", "instruction", "addi"),
            ("probe", "pattern", "dependency-chain"),
            ("probe", "differential_variant", "independent"),
        )
        for role, field, value in invalid:
            with self.subTest(role=role, field=field, value=value):
                forged = dict(rows[role])
                forged[field] = value
                with self.assertRaises(MERGER.MergeError):
                    MERGER._guest_metadata(forged)

    def test_alternating_context_closes_common_instruction_mix(self) -> None:
        probe = differential_guest_row(
            1,
            "probe",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="alternating",
            context="alternating-with-rem-reset",
            instruction="div",
            pattern="alternating-rem-div-reset",
        )
        baseline = differential_guest_row(
            2,
            "baseline",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="alternating",
            context="alternating-with-rem-reset",
            instruction="div",
            pattern="alternating-rem-div-reset",
        )

        rows = MERGER.merge_samples(
            [probe, baseline], differential_windows(alternating=True)
        )

        self.assertTrue(
            all(row["paired_contrast_purity"] == 1.0 for row in rows)
        )
        self.assertEqual(
            rows[0]["target_descriptor"]["encoding_key"], "rv64:32:m:div"
        )
        self.assertEqual(
            rows[0]["baseline_descriptor"]["encoding_key"],
            "rv64:32:i:addi:form=nop",
        )
        self.assertEqual(rows[0]["context"], "alternating-with-rem-reset")

    def test_versioned_differential_metadata_must_be_complete_and_pair_stable(
        self,
    ) -> None:
        probe = differential_guest_row(
            1,
            "probe",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="reference",
            context="homogeneous-div-reset",
            instruction="div",
            pattern="homogeneous-reset",
        )
        baseline = differential_guest_row(
            2,
            "baseline",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="reference",
            context="homogeneous-div-reset",
            instruction="div",
            pattern="homogeneous-reset",
        )
        del baseline["context"]
        with self.assertRaisesRegex(MERGER.MergeError, "缺少或含非法 context"):
            MERGER.merge_samples([probe, baseline], differential_windows())

        baseline["context"] = "alternating-with-rem-reset"
        baseline["differential_variant"] = "alternating"
        baseline["pattern"] = "alternating-rem-div-reset"
        with self.assertRaisesRegex(MERGER.MergeError, "不一致"):
            MERGER.merge_samples([probe, baseline], differential_windows())

    def test_versioned_pair_rejects_batch_metadata_drift(self) -> None:
        probe = differential_guest_row(
            1,
            "probe",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="reference",
            context="homogeneous-div-reset",
            instruction="div",
            pattern="homogeneous-reset",
        )
        baseline = dict(probe)
        baseline.update({"sequence": "2", "role": "baseline", "executed_instruction": "nop", "blocks": "2", "requested_count": "2048"})
        with self.assertRaisesRegex(MERGER.MergeError, "(blocks|requested_count).*不一致"):
            MERGER.merge_samples([probe, baseline], differential_windows())

    def test_guest_declared_target_count_must_match_plugin_counts(self) -> None:
        probe = differential_guest_row(
            1,
            "probe",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="reference",
            context="homogeneous-div-reset",
            instruction="div",
            pattern="homogeneous-reset",
        )
        baseline = differential_guest_row(
            2,
            "baseline",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="reference",
            context="homogeneous-div-reset",
            instruction="div",
            pattern="homogeneous-reset",
        )
        probe["target_count"] = "1023"
        with self.assertRaisesRegex(MERGER.MergeError, "target_count"):
            MERGER.merge_samples([probe, baseline], differential_windows())

    def test_differential_metadata_is_part_of_validation_signature(self) -> None:
        row = differential_guest_row(
            1,
            "probe",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="reference",
            context="homogeneous-div-reset",
            instruction="div",
            pattern="homogeneous-reset",
        )
        signature = MERGER._guest_signature(row)
        for name in MERGER._DIFFERENTIAL_METADATA_FIELDS:
            self.assertIn(f"{name}={row[name]}", signature)

        legacy = guest_row(1, "probe")
        self.assertEqual(
            MERGER._guest_signature(legacy),
            tuple(legacy[name] for name in MERGER._SIGNATURE_FIELDS),
        )

    def test_version_and_differential_semantics_are_strict(self) -> None:
        invalid_version = guest_row(1, "probe")
        invalid_version["version"] = 2  # type: ignore[assignment]
        with self.assertRaisesRegex(MERGER.MergeError, "version=2.*非法"):
            MERGER._guest_metadata(invalid_version)

        forged = differential_guest_row(
            1,
            "probe",
            suite="div-rem-dataflow-v2",
            contrast="div-dataflow",
            variant="independent",
            context="per-slot-reset-nondegenerate",
            instruction="div",
            pattern="independent-reset",
        )
        forged["instruction"] = "rem"
        forged["executed_instruction"] = "rem"
        with self.assertRaisesRegex(MERGER.MergeError, "contrast.*不一致"):
            MERGER._guest_metadata(forged)

        forged = differential_guest_row(
            1,
            "probe",
            suite="mixed-tb-interaction-v2",
            contrast="div-rem-alternation",
            variant="alternating",
            context="alternating-with-rem-reset",
            instruction="div",
            pattern="alternating-rem-div-reset",
        )
        forged["pattern"] = "homogeneous-reset"
        with self.assertRaisesRegex(MERGER.MergeError, "与差分上下文不一致"):
            MERGER._guest_metadata(forged)

    def test_differential_contexts_have_disjoint_model_group_keys(self) -> None:
        contexts = [
            ("div-dataflow", "evolving-dependency-chain", "reference", "dependency-chain"),
            ("div-dataflow", "per-slot-reset-nondegenerate", "independent", "independent-reset"),
            ("div-rem-alternation", "homogeneous-div-reset", "reference", "homogeneous-reset"),
            (
                "div-rem-alternation",
                "alternating-with-rem-reset",
                "alternating",
                "alternating-rem-div-reset",
            ),
        ]
        rows = [
            differential_guest_row(
                index + 1,
                "probe",
                suite=(
                    "div-rem-dataflow-v2"
                    if "dataflow" in contrast
                    else "mixed-tb-interaction-v2"
                ),
                contrast=contrast,
                variant=variant,
                context=context,
                instruction="div",
                pattern=pattern,
            )
            for index, (contrast, context, variant, pattern) in enumerate(contexts)
        ]
        for row in rows:
            MERGER._guest_metadata(row)
        model_keys = {
            (row["instruction"], row["encoding_bytes"], row["pattern"])
            for row in rows
        }
        self.assertEqual(len(model_keys), len(rows))

    def test_target_count_and_control_residual_must_close_exactly(self) -> None:
        target_mismatch = timing_windows()
        target_mismatch[1] = {
            "instruction_count": 119,
            "counts": [
                descriptor("b3003102", "mul", 99),
                descriptor("63841000", "beq", 20),
            ],
        }
        with self.assertRaisesRegex(MERGER.MergeError, "必须精确等于"):
            MERGER.merge_samples(
                [guest_row(1, "probe"), guest_row(2, "baseline")],
                target_mismatch,
            )

        residual = timing_windows()
        residual[2] = {
            "instruction_count": 113,
            "counts": [
                descriptor("93801000", "addi", 95),
                descriptor("63841000", "beq", 18),
            ],
        }
        with self.assertRaisesRegex(MERGER.MergeError, "对比计数|control 差"):
            MERGER.merge_samples(
                [guest_row(1, "probe"), guest_row(2, "baseline")], residual
            )

    def test_control_raw_count_may_exceed_requested_when_delta_closes(self) -> None:
        probe = guest_row(1, "probe")
        baseline = guest_row(2, "baseline")
        for row in (probe, baseline):
            row["instruction"] = "beq"
            row["baseline_instruction"] = "nop"
        windows = {
            1: {
                "instruction_count": 200,
                "counts": [
                    descriptor("63841000", "beq", 100),
                    descriptor("13000000", "nop", 100),
                ],
            },
            2: {
                "instruction_count": 200,
                "counts": [descriptor("13000000", "nop", 200)],
            },
        }

        rows = MERGER.merge_samples([probe, baseline], windows)

        self.assertTrue(
            all(row["paired_contrast_purity"] == 1.0 for row in rows)
        )
        self.assertEqual(rows[0]["baseline_descriptor"]["count"], 200)

    def test_descriptor_without_mnemonic_match_is_rejected(self) -> None:
        windows = timing_windows()
        windows[1] = {
            "instruction_count": 120,
            "counts": [
                descriptor("93801000", "addi", 100),
                descriptor("63841000", "beq", 20),
            ],
        }
        with self.assertRaisesRegex(MERGER.MergeError, "禁止按计数猜测"):
            MERGER.merge_samples(
                [guest_row(1, "probe"), guest_row(2, "baseline")], windows
            )

    def test_canonical_form_matches_a_pseudoinstruction_without_guessing(self) -> None:
        probe = guest_row(1, "probe")
        baseline = guest_row(2, "baseline")
        for row in (probe, baseline):
            row["instruction"] = "li"
            row["baseline_instruction"] = "nop"
        windows = {
            1: {
                "instruction_count": 120,
                "counts": [
                    descriptor("13051000", "addi", 100),
                    descriptor("63841000", "beq", 20),
                ],
            },
            2: {
                "instruction_count": 120,
                "counts": [
                    descriptor("13000000", "nop", 100),
                    descriptor("63841000", "beq", 20),
                ],
            },
        }

        rows = MERGER.merge_samples([probe, baseline], windows)

        self.assertEqual(
            rows[0]["target_descriptor"]["encoding_key"],
            "rv64:32:i:addi:form=li",
        )

    def test_multiple_runs_preserve_explicit_acquisition_order(self) -> None:
        run_one = [
            guest_row(1, "probe", run_id="run-1"),
            guest_row(2, "baseline", run_id="run-1"),
        ]
        run_two = [
            guest_row(1, "probe", run_id="run-2"),
            guest_row(2, "baseline", run_id="run-2"),
        ]
        shuffled_windows = timing_windows()
        for window in shuffled_windows.values():
            window["counts"] = list(reversed(window["counts"]))
        forward = MERGER.merge_timing_runs(
            [(run_one, timing_windows()), (run_two, timing_windows())]
        )
        reverse = MERGER.merge_timing_runs(
            [(run_two, shuffled_windows), (run_one, shuffled_windows)]
        )

        self.assertEqual(
            [row["run_id"] for row in forward],
            ["run-1"] * 2 + ["run-2"] * 2,
        )
        self.assertEqual(
            [row["run_id"] for row in reverse],
            ["run-2"] * 2 + ["run-1"] * 2,
        )
        self.assertEqual([row["run_order"] for row in forward], [0, 0, 1, 1])
        self.assertEqual([row["run_order"] for row in reverse], [0, 0, 1, 1])
        self.assertEqual([row["sequence"] for row in forward], [1, 2, 1, 2])

        with self.assertRaisesRegex(MERGER.MergeError, "重复使用 run_id"):
            MERGER.merge_timing_runs(
                [(run_one, timing_windows()), (run_one, timing_windows())]
            )

    def test_dual_mode_uses_validation_counts_and_marker_only_time(self) -> None:
        validation_guest = [
            guest_row(1, "probe", run_id="validation"),
            guest_row(2, "baseline", run_id="validation"),
        ]
        timing_guest = [
            guest_row(1, "probe", run_id="timing-1"),
            guest_row(2, "baseline", run_id="timing-1"),
        ]
        plugin_off_guest = [dict(row) for row in timing_guest]
        plugin_off_guest[0]["guest_elapsed_ns"] = "980"
        plugin_off_guest[1]["guest_elapsed_ns"] = "990"
        timing = {
            1: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 800,
                "plugin_monotonic_ns": 850,
                "translations_during_window": 0,
                "guest_trap_entries_during_window": 0,
            },
            2: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 700,
                "plugin_monotonic_ns": 760,
                "translations_during_window": 0,
                "guest_trap_entries_during_window": 0,
            },
        }

        rows = MERGER.merge_dual_mode_runs(
            validation_guest,
            timing_windows(),
            [(timing_guest, timing, plugin_off_guest)],
        )

        self.assertEqual(len(rows), 2)
        self.assertTrue(
            all(
                row["schema"] == "mygo.riscv-instruction-weight-sample.v2"
                for row in rows
            )
        )
        self.assertTrue(all(row["plugin_mode"] == "timing" for row in rows))
        self.assertTrue(
            all(row["translations_during_window"] == 0 for row in rows)
        )
        self.assertTrue(
            all(
                row["guest_trap_entries_during_window"] == 0
                for row in rows
            )
        )
        self.assertEqual(rows[0]["target_count"], 100)
        self.assertEqual(rows[0]["run_order"], 0)
        self.assertEqual(rows[0]["plugin_thread_cpu_ns"], 800)
        self.assertEqual(rows[0]["plugin_off_guest_ns"], 980)

    def test_dual_mode_uses_all_guest_translations_as_pollution(self) -> None:
        validation_guest = [
            guest_row(1, "probe", run_id="validation"),
            guest_row(2, "baseline", run_id="validation"),
        ]
        timing_guest = [
            guest_row(1, "probe", run_id="timing-1"),
            guest_row(2, "baseline", run_id="timing-1"),
        ]
        timing = {
            sequence: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 700 + sequence,
                "translations_during_window": 1 if sequence == 1 else 0,
                "scoped_translations_during_window": 0,
                "guest_trap_entries_during_window": 0,
            }
            for sequence in (1, 2)
        }

        rows = MERGER.merge_dual_mode_runs(
            validation_guest,
            timing_windows(),
            [(timing_guest, timing, None)],
        )

        self.assertEqual(rows[0]["translations_during_window"], 1)
        self.assertEqual(rows[0]["scoped_translations_during_window"], 0)

    def test_run_design_requires_complete_abba_super_run(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run-design.jsonl"
            rows = [
                {
                    "run_id": f"run-{pair}",
                    "run_order": pair - 1,
                    "super_run_id": "super-1",
                    "super_run_order": 0,
                    "crossover_pair": pair,
                    "crossover_design": "ABBA",
                    "timing_launch_position": 1 if pair == 1 else 4,
                    "plugin_off_launch_position": 2 if pair == 1 else 3,
                }
                for pair in (1, 2)
            ]
            path.write_text(
                "".join(json.dumps(row) + "\n" for row in rows),
                encoding="utf-8",
            )
            parsed = MERGER._parse_run_design(path)
            self.assertEqual(set(parsed), {"run-1", "run-2"})

            path.write_text(json.dumps(rows[0]) + "\n", encoding="utf-8")
            with self.assertRaisesRegex(MERGER.MergeError, "完整覆盖"):
                MERGER._parse_run_design(path)

    def test_dual_mode_preserves_differential_metadata(self) -> None:
        validation_guest = [
            differential_guest_row(
                1,
                "probe",
                suite="mixed-tb-interaction-v2",
                contrast="div-rem-alternation",
                variant="alternating",
                context="alternating-with-rem-reset",
                instruction="div",
                pattern="alternating-rem-div-reset",
                run_id="validation",
            ),
            differential_guest_row(
                2,
                "baseline",
                suite="mixed-tb-interaction-v2",
                contrast="div-rem-alternation",
                variant="alternating",
                context="alternating-with-rem-reset",
                instruction="div",
                pattern="alternating-rem-div-reset",
                run_id="validation",
            ),
        ]
        timing_guest = [
            differential_guest_row(
                1,
                "probe",
                suite="mixed-tb-interaction-v2",
                contrast="div-rem-alternation",
                variant="alternating",
                context="alternating-with-rem-reset",
                instruction="div",
                pattern="alternating-rem-div-reset",
                run_id="timing-1",
            ),
            differential_guest_row(
                2,
                "baseline",
                suite="mixed-tb-interaction-v2",
                contrast="div-rem-alternation",
                variant="alternating",
                context="alternating-with-rem-reset",
                instruction="div",
                pattern="alternating-rem-div-reset",
                run_id="timing-1",
            ),
        ]
        timing = {
            sequence: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 700 + sequence,
                "plugin_monotonic_ns": 800 + sequence,
                "translations_during_window": 0,
                "guest_trap_entries_during_window": 0,
            }
            for sequence in (1, 2)
        }

        rows = MERGER.merge_dual_mode_runs(
            validation_guest,
            differential_windows(alternating=True),
            [(timing_guest, timing, None)],
        )

        self.assertTrue(all(row["probe_version"] == 2 for row in rows))
        self.assertTrue(
            all(row["suite"] == "mixed-tb-interaction-v2" for row in rows)
        )
        self.assertTrue(
            all(row["contrast"] == "div-rem-alternation" for row in rows)
        )
        self.assertTrue(
            all(
                row["differential_variant"] == "alternating"
                for row in rows
            )
        )
        self.assertTrue(
            all(row["context"] == "alternating-with-rem-reset" for row in rows)
        )

    def test_dual_mode_rejects_signature_multiset_drift(self) -> None:
        validation_guest = [
            guest_row(1, "probe", run_id="validation"),
            guest_row(2, "baseline", run_id="validation"),
        ]
        timing_guest = [
            guest_row(1, "probe", run_id="timing-1"),
            guest_row(2, "baseline", run_id="timing-1"),
        ]
        timing_guest[1]["count_level"] = "1"
        timing = {
            1: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 700,
                "translations_during_window": 0,
                "guest_trap_entries_during_window": 0,
            },
            2: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 701,
                "translations_during_window": 0,
                "guest_trap_entries_during_window": 0,
            },
        }
        with self.assertRaisesRegex(MERGER.MergeError, "多重集"):
            MERGER.merge_dual_mode_runs(
                validation_guest,
                timing_windows(),
                [(timing_guest, timing, None)],
            )

    def test_dual_mode_rejects_guest_kernel_tb_execution_inside_window(
        self,
    ) -> None:
        validation_guest = [
            guest_row(1, "probe", run_id="validation"),
            guest_row(2, "baseline", run_id="validation"),
        ]
        timing_guest = [
            guest_row(1, "probe", run_id="timing-1"),
            guest_row(2, "baseline", run_id="timing-1"),
        ]
        timing = {
            sequence: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 700 + sequence,
                "translations_during_window": 0,
                "guest_trap_entries_during_window": (
                    1 if sequence == 1 else 0
                ),
            }
            for sequence in (1, 2)
        }

        with self.assertRaisesRegex(MERGER.MergeError, "客体 trap"):
            MERGER.merge_dual_mode_runs(
                validation_guest,
                timing_windows(),
                [(timing_guest, timing, None)],
            )


class CatalogMappingTests(unittest.TestCase):
    def test_canonical_encoding_and_restricted_contexts_cannot_assign(self) -> None:
        catalog = load_catalog_records(
            catalog_records(
                [
                {
                    "bytes_complete": True,
                    "size": 4,
                    "bytes": "0fa04900",
                    "mnemonic": "lq",
                },
                {
                    "bytes_complete": True,
                    "size": 4,
                    "bytes": "73002010",
                    "mnemonic": "sret",
                },
                {
                    "bytes_complete": True,
                    "size": 4,
                    "bytes": "732510c0",
                    "mnemonic": "csrrs",
                },
                {
                    "bytes_complete": True,
                    "size": 4,
                    "bytes": "73000000",
                    "mnemonic": "ecall",
                },
                ]
            ),
            expected_key_count=4,
        )

        cbo_key = "rv64:32:zicboz:cbo.zero"
        self.assertIn(cbo_key, catalog)
        self.assertEqual(catalog[cbo_key]["canonical_mnemonic"], "cbo.zero")
        self.assertEqual(catalog[cbo_key]["qemu_mnemonics"], {"lq"})

        model = model_document(
            [
                measured_context(cbo_key),
                measured_context("rv64:32:priv:sret"),
                measured_context("rv64:32:zicsr:csrrs:csr=0xc01:write=0"),
                measured_context("rv64:32:i:ecall"),
            ]
        )
        mapped = MAPPER.map_weights(catalog, model)
        by_key = {row["encoding_key"]: row for row in mapped["instructions"]}

        self.assertEqual(
            by_key[cbo_key]["assignment"],
            "cache-block-operation-is-context-dependent",
        )
        self.assertIsNone(by_key[cbo_key]["assigned_ns_per_instruction"])
        self.assertIsNone(
            by_key[cbo_key]["measured_estimate_ns_per_instruction"]
        )
        self.assertEqual(
            by_key[cbo_key]["estimate_quality"], "restricted-context"
        )
        self.assertEqual(
            by_key["rv64:32:priv:sret"]["assignment"],
            "requires-privileged-context-probe",
        )
        self.assertIsNone(
            by_key["rv64:32:priv:sret"]["assigned_ns_per_instruction"]
        )
        self.assertEqual(
            by_key["rv64:32:zicsr:csrrs:csr=0xc01:write=0"]["assignment"],
            "csr-is-not-safe-or-identifiable-in-user-mode",
        )
        self.assertEqual(
            by_key["rv64:32:i:ecall"]["assignment"],
            "trap-path-is-context-dependent",
        )
        self.assertTrue(
            all(row["restricted_contexts_ignored"] == 1 for row in by_key.values())
        )

    def test_catalog_requires_header_final_quality_and_zero_errors(self) -> None:
        instruction = {
            "bytes_complete": True,
            "size": 4,
            "bytes": "b3003102",
            "mnemonic": "mul",
        }
        valid = catalog_records([instruction])
        self.assertEqual(len(load_catalog_records(valid, expected_key_count=1)), 1)

        invalid_cases = {
            "缺少 final quality": valid[:-1],
            "final quality 后有记录": valid + [valid[1]],
            "target": catalog_records([instruction], target="loongarch64"),
            "schema": catalog_records([instruction], schema="wrong.schema"),
            "write_errors": catalog_records(
                [instruction], quality_updates={"write_errors": 1}
            ),
            "dropped_blocks": catalog_records(
                [instruction], quality_updates={"dropped_blocks": 1}
            ),
            "tracking_drops": catalog_records(
                [instruction], quality_updates={"tracking_drops": 1}
            ),
            "descriptor_overflow": [
                valid[0],
                {**valid[1], "descriptor_overflow": 1},
                valid[2],
            ],
            "incomplete bytes": [
                valid[0],
                {
                    **valid[1],
                    "instructions": [
                        {**instruction, "bytes_complete": False}
                    ],
                },
                valid[2],
            ],
        }
        for message, records in invalid_cases.items():
            with self.subTest(message=message):
                with self.assertRaises(MAPPER.MappingError):
                    load_catalog_records(records)

        with self.assertRaisesRegex(MAPPER.MappingError, "规范 key 数"):
            load_catalog_records(valid, expected_key_count=2)

    def test_393_key_mapping_reports_orphans_and_closes_status_counts(self) -> None:
        catalog = {
            f"rv64:32:i:synthetic-{index}": {
                "encoding_key": f"rv64:32:i:synthetic-{index}",
                "canonical_mnemonic": f"synthetic-{index}",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {f"{index:08x}"},
                "qemu_mnemonics": {f"synthetic-{index}"},
            }
            for index in range(393)
        }
        measured_key = "rv64:32:i:synthetic-0"
        result = MAPPER.map_weights(
            catalog,
            model_document(
                [
                    measured_context(measured_key),
                    measured_context("rv64:32:i:not-in-catalog"),
                ]
            ),
        )

        self.assertEqual(result["catalog_encoding_count"], 393)
        self.assertEqual(sum(result["status_counts"].values()), 393)
        self.assertEqual(result["orphan_model_encoding_count"], 1)
        self.assertEqual(
            result["orphan_model_encoding_keys"], ["rv64:32:i:not-in-catalog"]
        )

    def test_catalog_and_context_order_do_not_change_output(self) -> None:
        instructions = [
            {
                "bytes_complete": True,
                "size": 4,
                "bytes": "b3003102",
                "mnemonic": "mul",
            },
            {
                "bytes_complete": True,
                "size": 4,
                "bytes": "93801000",
                "mnemonic": "addi",
            },
        ]
        first_catalog = load_catalog_records(catalog_records(instructions))
        second_catalog = load_catalog_records(
            catalog_records(list(reversed(instructions)))
        )
        contexts = [
            measured_context("rv64:32:m:mul", pattern="dependency", value=3.0),
            measured_context("rv64:32:m:mul", pattern="independent", value=3.1),
            measured_context("rv64:32:i:addi", value=1.0),
        ]
        first = MAPPER.map_weights(
            first_catalog, model_document(contexts)
        )
        second = MAPPER.map_weights(
            second_catalog,
            model_document(list(reversed(contexts))),
        )
        self.assertEqual(first, second)
        self.assertEqual(
            json.dumps(first, separators=(",", ":")),
            json.dumps(second, separators=(",", ":")),
        )

    def test_current_model_contract_uses_semantic_encoding_key(self) -> None:
        semantic_key = "rv64:32:i:addi"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "addi",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"93801000"},
                "qemu_mnemonics": {"addi"},
            }
        }
        model = fit_microbenchmark_weight_model(
            model_contract_samples(), bootstrap_replicates=0
        )

        result = MAPPER.map_weights(catalog, model)

        self.assertEqual(
            result["model_schema_version"], MAPPER.MODEL_SCHEMA_VERSION
        )
        self.assertEqual(result["mapped_model_encoding_count"], 1)
        self.assertEqual(result["orphan_model_encoding_count"], 0)
        self.assertEqual(
            result["instructions"][0]["assignment"],
            "model-publication-gate-failed",
        )
        self.assertIsNone(
            result["instructions"][0][
                "measured_estimate_ns_per_instruction"
            ]
        )
        self.assertEqual(
            result["instructions"][0]["estimate_quality"],
            "not-identifiable",
        )
        self.assertEqual(
            result["instructions"][0]["contexts"][0]["raw_encoding_key"],
            "raw:4:93801000",
        )

    def test_single_low_confidence_context_retains_exploratory_estimate(
        self,
    ) -> None:
        semantic_key = "rv64:32:i:addi"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "addi",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"93801000"},
                "qemu_mnemonics": {"addi"},
            }
        }
        model = model_document(
            [
                measured_context(
                    semantic_key, value=1.75, quality="low-confidence"
                )
            ]
        )

        item = MAPPER.map_weights(catalog, model)["instructions"][0]

        self.assertIsNone(item["assigned_ns_per_instruction"])
        self.assertEqual(item["assignment"], "measured-but-confidence-gates-failed")
        self.assertEqual(item["measured_estimate_ns_per_instruction"], 1.75)
        self.assertEqual(item["estimate_quality"], "low-confidence")

    def test_mapper_uses_adjusted_value_for_low_confidence_exploration(
        self,
    ) -> None:
        semantic_key = "rv64:32:i:addi"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "addi",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"93801000"},
                "qemu_mnemonics": {"addi"},
            }
        }
        context = measured_context(
            semantic_key, value=100.0, quality="low-confidence"
        )
        context["anchor_adjusted"] = {
            "ns_per_instruction": 2.0,
            "simultaneous_ci": [1.0, 3.0],
        }

        item = MAPPER.map_weights(
            catalog, model_document([context])
        )["instructions"][0]

        self.assertEqual(item["measured_estimate_ns_per_instruction"], 2.0)
        self.assertEqual(item["contexts"][0]["ns_per_instruction"], 2.0)
        self.assertEqual(item["contexts"][0]["simultaneous_ci"], [1.0, 3.0])
        self.assertEqual(
            item["contexts"][0]["raw_diagnostic_ns_per_instruction"],
            100.0,
        )

    def test_mapper_rejects_tampered_published_adjusted_binding(self) -> None:
        semantic_key = "rv64:32:i:addi"
        context = measured_context(semantic_key, value=2.0)
        context["published_ns_per_instruction"] = 3.0

        with self.assertRaisesRegex(MAPPER.MappingError, "published"):
            MAPPER.map_weights({}, model_document([context]))

    def test_calibration_only_context_is_never_mapped(self) -> None:
        semantic_key = "rv64:32:m:div"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "div",
                "extension": "m",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"3345b502"},
                "qemu_mnemonics": {"div"},
            }
        }
        context = measured_context(semantic_key, value=4.0)
        context["calibration_only"] = True
        result = MAPPER.map_weights(catalog, model_document([context]))
        item = result["instructions"][0]
        self.assertIsNone(item["assigned_ns_per_instruction"])
        self.assertEqual(item["assignment"], "safe-probe-coverage-missing")

    def test_conflicting_contexts_do_not_publish_exploratory_estimate(
        self,
    ) -> None:
        semantic_key = "rv64:32:i:beq"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "beq",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"63841000"},
                "qemu_mnemonics": {"beq"},
            }
        }
        model = model_document(
            [
                measured_context(
                    semantic_key,
                    pattern="taken-branch",
                    value=3.2,
                    quality="low-confidence",
                ),
                measured_context(
                    semantic_key,
                    pattern="not-taken-branch",
                    value=2.1,
                    quality="low-confidence",
                ),
            ]
        )

        item = MAPPER.map_weights(catalog, model)["instructions"][0]

        self.assertIsNone(item["assigned_ns_per_instruction"])
        self.assertIsNone(item["measured_estimate_ns_per_instruction"])
        self.assertEqual(item["estimate_quality"], "context-dependent")

    def test_incompatible_model_contract_is_rejected(self) -> None:
        valid = model_document([])
        invalid_models = (
            {**valid, "schema_version": 1},
            {**valid, "schema_version": None},
            {**valid, "instruction_key": "mnemonic+size"},
            {**valid, "instruction_key": None},
        )
        for model in invalid_models:
            with self.subTest(model=model):
                with self.assertRaises(MAPPER.MappingError):
                    MAPPER.map_weights({}, model)

    def test_publication_requires_verified_ml_evidence_and_bindings(self) -> None:
        semantic_key = "rv64:32:i:addi"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "addi",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"93801000"},
                "qemu_mnemonics": {"addi"},
            }
        }
        base = model_document([measured_context(semantic_key)])
        for mutation in ("missing-evidence", "failed-check", "missing-binding"):
            model = copy.deepcopy(base)
            if mutation == "missing-evidence":
                model.pop("ml_validation_evidence")
            elif mutation == "failed-check":
                model["ml_validation_evidence"]["checks"]["all_subgates"] = False
            else:
                model["ml_validation_evidence"]["binding_checks"].pop("samples")

            with self.assertRaisesRegex(
                MAPPER.MappingError, "publication seal"
            ):
                MAPPER.map_weights(catalog, model)

    def test_mapper_rejects_post_finalization_point_and_interval_tampering(
        self,
    ) -> None:
        semantic_key = "rv64:32:i:addi"
        catalog = {
            semantic_key: {
                "encoding_key": semantic_key,
                "canonical_mnemonic": "addi",
                "extension": "i",
                "size": 4,
                "recognized": True,
                "modifiers": [],
                "raw_encodings": {"93801000"},
                "qemu_mnemonics": {"addi"},
            }
        }
        model = model_document([measured_context(semantic_key, value=2.0)])
        item = model["instructions"][0]
        item["published_ns_per_instruction"] = 9.0
        item["anchor_adjusted"]["ns_per_instruction"] = 9.0
        item["anchor_adjusted"]["simultaneous_ci"] = [8.0, 10.0]

        with self.assertRaisesRegex(MAPPER.MappingError, "publication seal"):
            MAPPER.map_weights(catalog, model)

    def test_mapper_rejects_missing_fwer_contract_even_if_gate_is_true(
        self,
    ) -> None:
        model = model_document([])
        del model["publication_familywise_error_control"]

        with self.assertRaisesRegex(MAPPER.MappingError, "FWER"):
            MAPPER.map_weights({}, model)

    def test_mapper_rejects_resealed_999_replicate_fwer_evidence(self) -> None:
        model = model_document([])
        inference_names = (
            "simultaneous_inference",
            "diagnostic_simultaneous_inference",
            "auxiliary_consistency_inference",
            "joint_raw_adjusted_inference",
        )
        for name in inference_names:
            inference = model[name]
            inference["requested_replicates"] = 999
            for field in (
                "complete_max_statistic_replicates",
                "complete_replicates",
                "complete_family_replicates",
                "valid_replicates",
            ):
                if field in inference:
                    inference[field] = 999
            evidence = inference["critical_value_monte_carlo"]
            evidence.update(
                {
                    "replicates": 800,
                    "required_rank": 999,
                    "selected_rank": 999,
                    "complete_family_replicates": 999,
                    "scale_replicates": 199,
                    "quantile_replicates": 800,
                }
            )
        with self.assertRaisesRegex(
            MAPPER.ModelSealError, "finite-bootstrap"
        ):
            seal_model_document(model)


class RunnerGateTests(unittest.TestCase):
    def test_qemu_status_and_success_marker_are_both_strict(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('[ "$status" -ne 0 ]', script)
        self.assertIn("tr -d '\\r'", script)
        self.assertIn(
            "grep -qx 'RISCV_WEIGHT_GUEST_DONE status=0'", script
        )

    def test_runner_embeds_host_audit_into_publication_gate(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('document["host_isolation_audit"] = audit', script)
        self.assertIn('components["host_isolation"]', script)
        self.assertIn("verify-binding --audit", script)
        self.assertIn('source == "current"', script)
        self.assertIn('binding.get("publication_allowed") is True', script)
        self.assertIn("external-host-audit-not-publishable", script)
        self.assertIn("host-audit-binding.json", script)
        self.assertIn("formal measurement rejects RISCV_WEIGHT_HOST_AUDIT", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_MAX_INTERRUPTS_PER_SECOND", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_REQUIRE_PSI", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_REQUIRE_FREQUENCY_FLOOR", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_REQUIRE_WINDOW_FREQUENCY", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_REQUIRE_INTERRUPTS", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_REQUIRE_SCHEDSTAT", script)
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_MAX_RUNQUEUE_WAIT_FRACTION", script)
        self.assertIn("RISCV_WEIGHT_HOST_TELEMETRY_SUDO", script)
        self.assertIn("RISCV_WEIGHT_ISOLATION_STATE", script)
        self.assertIn("RISCV_WEIGHT_REQUIRE_ISOLATION_STATE", script)
        self.assertIn('--expected-key-count "$expected_catalog_keys"', script)

    def test_runner_finalizes_ml_before_catalog_mapping(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )

        ml = script.index("--finalize-weights")
        mapper = script.index(
            "python3 scripts/map-riscv-instruction-weights.py", ml
        )
        self.assertLess(ml, mapper)
        self.assertIn('--weights "$output/weights.json"', script)

    def test_plugin_on_off_launch_order_uses_complete_abba_baab_super_runs(
        self,
    ) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("RISCV_WEIGHT_LAUNCH_SEED", script)
        self.assertIn("launch-order.tsv", script)
        self.assertIn("run-design.jsonl", script)
        self.assertIn("super-run-design.tsv", script)
        self.assertIn('block = ["ABBA", "BAAB"]', script)
        self.assertIn("generator.shuffle(block)", script)
        self.assertIn("generator = random.Random(seed)", script)
        self.assertIn("while [ \"$position\" -le 4 ]", script)
        self.assertIn("ABBA:1|ABBA:4|BAAB:2|BAAB:3", script)
        self.assertIn('--run-design \"/work/${run_design_log#', script)

    def test_probe_balances_pair_order_and_resets_state_after_warmup(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )
        source = (
            REPOSITORY / "userland/tests/riscv_instruction_weight_probe.c"
        ).read_text(encoding="utf-8")

        self.assertIn("unsigned char *order_schedule", source)
        self.assertIn("(round + offset) & 1U", source)
        self.assertIn("schedule[index - 1] = schedule[other]", source)
        warmup = source.index("预热可能改变 FCSR/fflags")
        measurement = source.index("run_profiled_window(kernel", warmup)
        self.assertIn(
            "prepare_window_state(entry, data, 1);",
            source[warmup:measurement],
        )
        self.assertEqual(script.count('timer_hz=0 riscv_weight_'), 4)
        self.assertIn("trap_entry_pc", script)

    def test_formal_calibration_uses_two_family_publication_split(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("default_runs=205", script)
        self.assertIn("conformal_train_runs=${conformal_train_runs:-20}", script)
        self.assertIn(
            "conformal_calibration_runs=${conformal_calibration_runs:-39}",
            script,
        )
        self.assertIn('ml_confidence=${RISCV_WEIGHT_ML_CONFIDENCE:-0.975}', script)
        self.assertIn('--confidence "$ml_confidence"', script)

    def test_model_uses_explicit_numpy_backend_without_blas_oversubscription(
        self,
    ) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            "RISCV_WEIGHT_LINEAR_ALGEBRA_BACKEND:-numpy", script
        )
        self.assertIn("RISCV_WEIGHT_BOOTSTRAP_JOBS:-16", script)
        self.assertIn(
            '--linear-algebra-backend "$linear_algebra_backend"', script
        )
        for variable in (
            "OPENBLAS_NUM_THREADS",
            "OMP_NUM_THREADS",
            "MKL_NUM_THREADS",
            "BLIS_NUM_THREADS",
            "NUMEXPR_NUM_THREADS",
        ):
            self.assertIn(f"{variable}=1", script)

    def test_runner_supports_selinux_and_inherited_cgroup_containers(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("RISCV_WEIGHT_CONTAINER_RUNTIME", script)
        self.assertIn("RISCV_WEIGHT_CONTAINER_MOUNT_SUFFIX", script)
        self.assertIn("RISCV_WEIGHT_CONTAINER_RUN_ARGUMENTS", script)
        self.assertIn("container_run()", script)
        self.assertIn('$root:/work$container_mount_suffix', script)

    def test_isolator_restores_host_state_and_binds_audit_evidence(self) -> None:
        script = (SCRIPTS / "run-riscv-weight-isolated.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("trap restore_host EXIT HUP INT TERM", script)
        self.assertIn("isolation-state.json", script)
        self.assertIn("isolation-restore.json", script)
        self.assertIn("thread_siblings_list", script)
        self.assertIn("AllowedCPUs=$background", script)
        self.assertIn("RISCV_WEIGHT_PHYSICAL_CORE_CPUSET", script)
        self.assertIn("RISCV_WEIGHT_ISOLATION_STATE", script)
        self.assertIn("RISCV_WEIGHT_HOST_TELEMETRY_SUDO=1", script)
        self.assertIn(
            "RISCV_WEIGHT_HOST_AUDIT_MAX_INTERRUPTS_PER_SECOND=0", script
        )
        self.assertIn("RISCV_WEIGHT_HOST_AUDIT_REQUIRE_SCHEDSTAT=1", script)
        self.assertIn(
            "RISCV_WEIGHT_HOST_AUDIT_MAX_RUNQUEUE_WAIT_FRACTION=0.01", script
        )
        self.assertIn("effective_affinity_list", script)
        self.assertIn("residual_unmigratable", script)
        self.assertIn("irq_isolation_policy_satisfied", script)
        self.assertIn("kernel-affinity-plan.tsv", script)
        self.assertIn("/sys/devices/virtual/workqueue", script)
        self.assertIn("writeback/cpumask", script)
        self.assertIn("/proc/sys/kernel/watchdog_cpumask", script)
        self.assertIn("kernel_affinity_restore_verified", script)
        self.assertIn("kernel_affinity_entries_sha256", script)
        self.assertIn("kernel_affinity_policy_satisfied", script)

    def test_plugin_smoke_creates_build_directory_before_mktemp(self) -> None:
        script = (
            REPOSITORY
            / "tools/qemu-plugins/test-riscv-instruction-weight-plugin.sh"
        ).read_text(encoding="utf-8")
        self.assertLess(
            script.index('mkdir -p "$root/build"'),
            script.index('work=$(mktemp -d "$root/build/'),
        )


if __name__ == "__main__":
    unittest.main()
