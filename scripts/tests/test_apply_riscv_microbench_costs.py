"""RISC-V 微基准成本账本的行级闭合回归测试。"""

from __future__ import annotations

import importlib.util
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
from riscv_weight_model_seal import (
    FWER_COVERAGE_CLAIM,
    FWER_METHOD,
    MONTE_CARLO_METHOD,
    REPLICATE_PARTITION_METHOD,
    ModelSealError,
    seal_model_document,
    verify_model_document_seal,
)
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


def published_model_item(
    *, raw: float = 200.0, adjusted: float = 2.0
) -> dict[str, object]:
    return {
        "key": {"semantic_encoding_key": "rv64:32:i:addi"},
        "ns_per_instruction": raw,
        "simultaneous_ci": [raw - 10.0, raw + 10.0],
        "published_ns_per_instruction": adjusted,
        "anchor_adjusted": {
            "ns_per_instruction": adjusted,
            "simultaneous_ci": [adjusted - 1.0, adjusted + 1.0],
        },
        "quality": "high-confidence",
        "calibration_only": False,
    }


def published_model_document(item: dict[str, object]) -> dict[str, object]:
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
        "schema_version": 3,
        "instruction_key": MAPPER.MODEL_KEY,
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
        "instructions": [item],
        "publication_gate": {
            "passed": True,
            "components": {
                name: True for name in MAPPER.REQUIRED_PUBLICATION_COMPONENTS
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
            "checks": {"held_out_gate": True},
            "binding_checks": {
                "samples": True,
                "statistical_weights_pre_finalization": True,
            },
        },
    }
    seal_model_document(document)
    return document


def low_confidence_model_item() -> dict[str, object]:
    return {
        "key": {"semantic_encoding_key": "rv64:32:i:sub"},
        "ns_per_instruction": 9.0,
        "simultaneous_ci": [1.0, 17.0],
        "published_ns_per_instruction": None,
        "anchor_adjusted": {
            "ns_per_instruction": 8.0,
            "simultaneous_ci": [0.5, 15.5],
        },
        "quality": "low-confidence",
        "calibration_only": False,
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


class PublishedEstimatorBoundaryTests(unittest.TestCase):
    def load(self, item: dict[str, object]):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "weights.json"
            path.write_text(
                json.dumps(published_model_document(item)), encoding="utf-8"
            )
            return MAPPER.load_model(path)

    def test_cost_estimate_uses_only_published_anchor_adjusted_values(self) -> None:
        by_semantic, _metadata = self.load(published_model_item())
        decoded = SimpleNamespace(
            mnemonic="addi", extension="i", recognized=True
        )

        result = MAPPER.descriptor_estimate(
            {"rv64:32:i:addi"},
            {"rv64:32:i:addi": decoded},
            by_semantic,
        )

        self.assertEqual(result["point_ns"], 2.0)
        self.assertEqual(result["low_ns"], 1.0)
        self.assertEqual(result["high_ns"], 3.0)
        self.assertEqual(result["diagnostic_context_center_ns"], 2.0)
        aggregate = MAPPER.aggregate_costs(
            {7: {"user": 4, "kernel": 0}},
            {(7, "user"): result},
        )
        self.assertEqual(aggregate["identified_point_cost_ns"], 8.0)
        self.assertEqual(aggregate["bounded_cost_envelope_low_ns"], 4.0)
        self.assertEqual(aggregate["bounded_cost_envelope_high_ns"], 12.0)

    def test_restricted_diagnostic_center_does_not_leak_raw_value(self) -> None:
        by_semantic, _metadata = self.load(published_model_item())
        decoded = SimpleNamespace(
            mnemonic="ecall", extension="i", recognized=True
        )

        result = MAPPER.descriptor_estimate(
            {"rv64:32:i:addi"},
            {"rv64:32:i:addi": decoded},
            by_semantic,
        )

        self.assertEqual(result["quality"], "restricted-context")
        self.assertEqual(result["diagnostic_context_center_ns"], 2.0)

    def test_loader_rejects_published_and_adjusted_point_disagreement(self) -> None:
        item = published_model_item(adjusted=2.0)
        item["published_ns_per_instruction"] = 3.0

        with self.assertRaisesRegex(
            MAPPER.CostError, "disagrees with anchor-adjusted"
        ):
            self.load(item)

    def test_loader_rejects_post_finalization_point_and_interval_tampering(
        self,
    ) -> None:
        document = published_model_document(published_model_item())
        item = document["instructions"][0]
        item["published_ns_per_instruction"] = 7.0
        item["anchor_adjusted"]["ns_per_instruction"] = 7.0
        item["anchor_adjusted"]["simultaneous_ci"] = [6.0, 8.0]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "weights.json"
            path.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaisesRegex(MAPPER.CostError, "publication seal"):
                MAPPER.load_model(path)

    def test_low_confidence_context_is_unpriced_without_rejecting_model(
        self,
    ) -> None:
        document = published_model_document(published_model_item())
        document["instructions"].append(low_confidence_model_item())
        seal_model_document(document)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "weights.json"
            path.write_text(json.dumps(document), encoding="utf-8")

            by_semantic, _metadata = MAPPER.load_model(path)

        self.assertIn("rv64:32:i:addi", by_semantic)
        self.assertNotIn("rv64:32:i:sub", by_semantic)

    def test_resealed_model_with_invalid_fwer_contract_is_rejected(self) -> None:
        document = published_model_document(published_model_item())
        document["publication_familywise_error_control"]["overall_alpha"] = 0.20
        # seal helper 本身也执行语义门禁，不能被用作无密钥重签工具。
        with self.assertRaisesRegex(
            MAPPER.ModelSealError, "FWER"
        ):
            seal_model_document(document)

    def test_resealed_model_with_forged_finite_bootstrap_rank_is_rejected(
        self,
    ) -> None:
        document = published_model_document(published_model_item())
        evidence = document["simultaneous_inference"][
            "critical_value_monte_carlo"
        ]
        evidence["required_rank"] = 3987
        evidence["selected_rank"] = 3987
        with self.assertRaisesRegex(
            MAPPER.ModelSealError, "finite-bootstrap rank"
        ):
            seal_model_document(document)

    def test_seal_rejects_even_one_missing_complete_family_replicate(self) -> None:
        document = published_model_document(published_model_item())
        document["simultaneous_inference"]["requested_replicates"] = 5000

        with self.assertRaisesRegex(ModelSealError, "finite-bootstrap"):
            seal_model_document(document)

    def test_verify_rejects_matching_digest_when_publication_gate_failed(self) -> None:
        document = published_model_document(published_model_item())
        document["publication_gate"]["passed"] = False
        payload = json.dumps(
            {
                key: value
                for key, value in document.items()
                if key != "publication_seal"
            },
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        ).encode("utf-8")
        document["publication_seal"]["payload_sha256"] = hashlib.sha256(
            payload
        ).hexdigest()
        document["publication_seal"]["payload_size"] = len(payload)

        with self.assertRaisesRegex(ModelSealError, "gate 未通过"):
            verify_model_document_seal(document)


if __name__ == "__main__":
    unittest.main()
