"""RISC-V 指令权重来源链的 fail-closed 回归测试。"""

from __future__ import annotations

import copy
import hashlib
import json
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path
from types import SimpleNamespace


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))

from riscv_weight_provenance import (
    MODEL_FIELD,
    ProvenanceError,
    artifact_identity,
    attach_model_provenance,
    create_manifest,
    finalize_model_provenance,
    verify_finalized_model,
    verify_manifest,
    write_manifest,
)
from riscv_weight_host_telemetry import (
    TELEMETRY_SCHEMA,
    audit as audit_host_telemetry,
    verify_binding as verify_host_audit_binding,
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")


def write_jsonl(path: Path, rows: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows),
        encoding="utf-8",
    )


def host_telemetry_rows(
    designs: list[dict[str, object]],
) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    launch_index = 0
    for design in designs:
        for mode, position in (
            ("timing", design["timing_launch_position"]),
            ("plugin-off", design["plugin_off_launch_position"]),
        ):
            launch_index += 1
            launch_id = f"super-1-{position}-{mode}"
            base_ns = launch_index * 10_000_000_000
            for phase in ("before", "after"):
                before = phase == "before"
                rows.append(
                    {
                        "schema": TELEMETRY_SCHEMA,
                        "timestamp_ns": base_ns + (0 if before else 1_000_000_000),
                        "monotonic_ns": base_ns + (0 if before else 1_000_000_000),
                        "phase": phase,
                        "launch_id": launch_id,
                        "super_run_id": "super-1",
                        "run_id": design["run_id"],
                        "mode": mode,
                        "launch_position": position,
                        "selected_cpus": [0],
                        "physical_core_cpus": [0],
                        "selected_core_temperature_sensors": ["cpu0"],
                        "kernel_affinity": {},
                        "cpu": {
                            "0": {
                                "times": (
                                    [100, 0, 0, 100, 0]
                                    if before
                                    else [190, 0, 0, 110, 0]
                                ),
                                "schedstat": None,
                                "interrupts": None,
                                "online": True,
                                "governor": "performance",
                                "mperf": None,
                                "aperf": None,
                                "scaling_cur_freq": 1_000_000,
                                "scaling_min_freq": 1_000_000,
                                "scaling_max_freq": 1_000_000,
                            }
                        },
                        "load_per_online_cpu": 0.05,
                        "pressure_cpu": None,
                        "pressure_memory": None,
                        "mem_available_kib": 8 * 1024 * 1024,
                        "temperatures_c": {"cpu0": 50.0 if before else 51.0},
                    }
                )
    return rows


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.paths = {
            "kernel": root / "kernel-rv",
            "probe": root / "probe.elf",
            "plugin": root / "riscv_instruction_weight.so",
            "qemu_version": root / "qemu-version.txt",
            "qemu_binary_checksum": root / "qemu-binary.sha256",
            "artifact_checksums": root / "artifacts.sha256",
            "samples": root / "samples.jsonl",
            "run_design": root / "run-design.jsonl",
            "host_telemetry": root / "host-telemetry.jsonl",
            "host_audit": root / "host-audit.json",
            "host_audit_binding": root / "host-audit-binding.json",
            "weights_pre_finalization": root / "weights.pre-final.json",
            "ml_validation": root / "ml-validation.json",
            "isolation_state": root / "isolation-state.json",
        }
        for role in ("kernel", "probe", "plugin"):
            self.paths[role].write_bytes((role + "\0binary").encode())
        self.paths["qemu_version"].write_text(
            "QEMU emulator version 10.0.0\nCopyright fixture\n", encoding="utf-8"
        )
        self.paths["qemu_binary_checksum"].write_text(
            f"{'4' * 64}  /usr/bin/qemu-system-riscv64\n", encoding="utf-8"
        )
        self.paths["artifact_checksums"].write_text(
            "".join(
                f"{sha256(self.paths[role])}  {self.paths[role]}\n"
                for role in ("kernel", "probe", "plugin")
            ),
            encoding="utf-8",
        )
        designs = [
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
        write_jsonl(self.paths["run_design"], designs)
        samples: list[dict[str, object]] = []
        for design in designs:
            for sequence, role in enumerate(("probe", "baseline"), 1):
                samples.append(
                    {
                        **design,
                        "sequence": sequence,
                        "pair_id": "instruction-pair",
                        "role": role,
                        "target_count": 100 if role == "probe" else 0,
                    }
                )
        write_jsonl(self.paths["samples"], samples)

        write_jsonl(self.paths["host_telemetry"], host_telemetry_rows(designs))
        write_json(self.paths["isolation_state"], {"schema": "unused-fixture"})
        audit_status = audit_host_telemetry(
            SimpleNamespace(
                input=str(self.paths["host_telemetry"]),
                run_design=str(self.paths["run_design"]),
                output=str(self.paths["host_audit"]),
                isolation_state=None,
                require_isolation_state=False,
                max_sibling_busy=0.10,
                max_load_per_cpu=0.75,
                min_frequency_ratio=0.90,
                require_frequency_floor=False,
                require_window_frequency=False,
                min_window_frequency_ratio=0.95,
                max_window_frequency_cv=0.03,
                max_interrupts_per_second=25.0,
                require_interrupts=False,
                require_schedstat=False,
                max_runqueue_wait_fraction=0.01,
                max_temperature_span=12.0,
                max_temperature=90.0,
                min_selected_busy=0.50,
                max_cpu_psi=0.10,
                max_memory_psi=0.02,
                require_psi=False,
                min_mem_available_kib=1024 * 1024,
            )
        )
        if audit_status != 0:
            raise AssertionError("provenance fixture host audit 必须通过")
        binding_status = verify_host_audit_binding(
            SimpleNamespace(
                audit=str(self.paths["host_audit"]),
                input=str(self.paths["host_telemetry"]),
                run_design=str(self.paths["run_design"]),
                source="current",
                output=str(self.paths["host_audit_binding"]),
            )
        )
        if binding_status != 0:
            raise AssertionError("provenance fixture host binding 必须通过")
        write_json(
            self.paths["weights_pre_finalization"],
            {"schema_version": 3, "publication_gate": {"passed": False}},
        )
        write_json(
            self.paths["ml_validation"],
            {
                "schema": "mygo.riscv-instruction-ml-validation.v3",
                "conclusion": {"status": "supported", "may_publish_weights": False},
                "input_bindings": {
                    "samples": artifact_identity(self.paths["samples"], root),
                    "statistical_weights_pre_finalization": artifact_identity(
                        self.paths["weights_pre_finalization"], root
                    ),
                },
            },
        )

    def create(self) -> dict[str, object]:
        return create_manifest(root=self.root, artifacts=self.paths)

    def final_document(self) -> dict[str, object]:
        audit = json.loads(self.paths["host_audit"].read_text())
        host_binding = json.loads(self.paths["host_audit_binding"].read_text())
        ml_identity = artifact_identity(self.paths["ml_validation"], self.root)
        input_bindings = {
            "samples": artifact_identity(self.paths["samples"], self.root),
            "statistical_weights_pre_finalization": artifact_identity(
                self.paths["weights_pre_finalization"], self.root
            ),
        }
        document: dict[str, object] = {
            "publication_gate": {"passed": True},
            "host_isolation_audit": audit,
            "host_isolation_audit_binding": host_binding,
            "host_isolation_audit_source": "current",
            "ml_validation_evidence": {
                "schema": "mygo.riscv-instruction-ml-validation.v3",
                "artifact": ml_identity,
                "input_bindings": input_bindings,
            },
            "ml_validation": {
                "schema": "mygo.riscv-instruction-ml-validation.v3",
                "conclusion": {"status": "supported", "may_publish_weights": False},
                "evidence_artifact": ml_identity,
                "input_bindings": input_bindings,
            },
        }
        return document


class ProvenanceTests(unittest.TestCase):
    def test_manifest_round_trip_rehashes_every_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            manifest = fixture.create()
            verified = verify_manifest(manifest, root=directory)
            self.assertEqual(verified["chain_sha256"], manifest["chain_sha256"])
            self.assertEqual(
                set(manifest["artifacts"]),  # type: ignore[arg-type]
                set(fixture.paths),
            )

    def test_each_artifact_replacement_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            manifest = fixture.create()
            original = {role: path.read_bytes() for role, path in fixture.paths.items()}
            for role, path in fixture.paths.items():
                with self.subTest(role=role):
                    path.write_bytes(original[role] + b"tamper")
                    with self.assertRaisesRegex(ProvenanceError, "不一致"):
                        verify_manifest(manifest, root=directory)
                    path.write_bytes(original[role])

    def test_manifest_edit_and_unknown_role_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            manifest = fixture.create()
            changed = copy.deepcopy(manifest)
            changed["relationships"]["collection"]["sample_count"] += 1  # type: ignore[index]
            with self.assertRaisesRegex(ProvenanceError, "chain_sha256"):
                verify_manifest(changed, root=directory)
            changed = copy.deepcopy(manifest)
            changed["artifacts"]["unknown"] = changed["artifacts"]["kernel"]  # type: ignore[index]
            with self.assertRaises(ProvenanceError):
                verify_manifest(changed, root=directory)

    def test_samples_run_design_splice_is_rejected_at_creation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            rows = [json.loads(line) for line in fixture.paths["samples"].read_text().splitlines()]
            rows[0]["timing_launch_position"] = 4
            write_jsonl(fixture.paths["samples"], rows)
            with self.assertRaisesRegex(ProvenanceError, "run-design"):
                fixture.create()

    def test_ml_and_host_binding_splices_are_rejected_at_creation(self) -> None:
        for role, key in (
            ("ml_validation", "input_bindings"),
            ("host_audit_binding", "inputs"),
        ):
            with self.subTest(role=role), tempfile.TemporaryDirectory() as directory:
                fixture = Fixture(Path(directory))
                document = json.loads(fixture.paths[role].read_text())
                if role == "ml_validation":
                    document[key]["samples"]["sha256"] = "0" * 64
                else:
                    document[key]["audit"]["sha256"] = "0" * 64
                write_json(fixture.paths[role], document)
                with self.assertRaisesRegex(ProvenanceError, "未绑定"):
                    fixture.create()

    def test_forged_accepted_audit_cannot_hide_invalid_telemetry(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = Fixture(root)
            telemetry = [
                json.loads(line)
                for line in fixture.paths["host_telemetry"].read_text().splitlines()
            ]
            telemetry[0]["schema"] = "forged-unsupported-telemetry"
            write_jsonl(fixture.paths["host_telemetry"], telemetry)

            forged_audit = json.loads(fixture.paths["host_audit"].read_text())
            forged_audit["inputs"]["telemetry"]["sha256"] = sha256(
                fixture.paths["host_telemetry"]
            )
            forged_audit["status"] = "accepted"
            forged_audit["failures"] = []
            write_json(fixture.paths["host_audit"], forged_audit)

            binding_status = verify_host_audit_binding(
                SimpleNamespace(
                    audit=str(fixture.paths["host_audit"]),
                    input=str(fixture.paths["host_telemetry"]),
                    run_design=str(fixture.paths["run_design"]),
                    source="current",
                    output=str(fixture.paths["host_audit_binding"]),
                )
            )
            self.assertEqual(binding_status, 0)
            self.assertTrue(
                json.loads(fixture.paths["host_audit_binding"].read_text())[
                    "publication_allowed"
                ]
            )
            with self.assertRaisesRegex(ProvenanceError, "重放后未通过"):
                fixture.create()

    def test_forged_accepted_audit_cannot_hide_invalid_isolation_state(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            forged_audit = json.loads(fixture.paths["host_audit"].read_text())
            forged_audit["inputs"]["isolation_state"] = {
                "path": str(fixture.paths["isolation_state"]),
                "sha256": sha256(fixture.paths["isolation_state"]),
            }
            forged_audit["isolation_state_checks_required"] = True
            forged_audit["status"] = "accepted"
            forged_audit["failures"] = []
            write_json(fixture.paths["host_audit"], forged_audit)

            binding_status = verify_host_audit_binding(
                SimpleNamespace(
                    audit=str(fixture.paths["host_audit"]),
                    input=str(fixture.paths["host_telemetry"]),
                    run_design=str(fixture.paths["run_design"]),
                    source="current",
                    output=str(fixture.paths["host_audit_binding"]),
                )
            )
            self.assertEqual(binding_status, 0)
            with self.assertRaisesRegex(ProvenanceError, "重放后未通过"):
                fixture.create()

    def test_host_audit_and_binding_derived_fields_must_match_replay(self) -> None:
        for role, mutate, message in (
            (
                "host_audit",
                lambda document: document.__setitem__(
                    "minimum_frequency_ratio", 0.5
                ),
                "host audit.*重放结果不一致",
            ),
            (
                "host_audit_binding",
                lambda document: document.__setitem__("expected_launches", []),
                "host audit binding.*重放结果不一致",
            ),
        ):
            with self.subTest(role=role), tempfile.TemporaryDirectory() as directory:
                fixture = Fixture(Path(directory))
                document = json.loads(fixture.paths[role].read_text())
                mutate(document)
                write_json(fixture.paths[role], document)
                if role == "host_audit":
                    verify_host_audit_binding(
                        SimpleNamespace(
                            audit=str(fixture.paths["host_audit"]),
                            input=str(fixture.paths["host_telemetry"]),
                            run_design=str(fixture.paths["run_design"]),
                            source="current",
                            output=str(fixture.paths["host_audit_binding"]),
                        )
                    )
                with self.assertRaisesRegex(ProvenanceError, message):
                    fixture.create()

    def test_final_model_closes_manifest_external_and_embedded_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = Fixture(root)
            manifest_path = root / "provenance.json"
            write_manifest(manifest_path, root=root, artifacts=fixture.paths)
            document = fixture.final_document()
            attach_model_provenance(document, manifest_path=manifest_path, root=root)
            weights_path = root / "weights.final.json"
            write_json(weights_path, document)
            with unittest.mock.patch(
                "riscv_weight_provenance.verify_model_document_seal"
            ):
                verified = verify_finalized_model(weights_path, root=root)
            self.assertEqual(
                verified[MODEL_FIELD]["manifest_chain_sha256"],  # type: ignore[index]
                json.loads(manifest_path.read_text())["chain_sha256"],
            )

            fixture.paths["kernel"].write_bytes(b"replacement")
            with unittest.mock.patch(
                "riscv_weight_provenance.verify_model_document_seal"
            ), self.assertRaises(ProvenanceError):
                verify_finalized_model(weights_path, root=root)

    def test_final_model_rejects_external_manifest_or_embedded_binding_drift(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = Fixture(root)
            manifest_path = root / "provenance.json"
            write_manifest(manifest_path, root=root, artifacts=fixture.paths)
            document = fixture.final_document()
            attach_model_provenance(document, manifest_path=manifest_path, root=root)
            weights_path = root / "weights.final.json"
            write_json(weights_path, document)

            original_manifest = manifest_path.read_bytes()
            manifest_path.write_bytes(original_manifest + b"\n")
            with unittest.mock.patch(
                "riscv_weight_provenance.verify_model_document_seal"
            ), self.assertRaisesRegex(ProvenanceError, "manifest"):
                verify_finalized_model(weights_path, root=root)
            manifest_path.write_bytes(original_manifest)

            changed = copy.deepcopy(document)
            changed[MODEL_FIELD]["manifest_chain_sha256"] = "0" * 64  # type: ignore[index]
            write_json(weights_path, changed)
            with unittest.mock.patch(
                "riscv_weight_provenance.verify_model_document_seal"
            ), self.assertRaisesRegex(ProvenanceError, "chain"):
                verify_finalized_model(weights_path, root=root)

    def test_finalize_attaches_provenance_before_resealing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = Fixture(root)
            manifest_path = root / "provenance.json"
            write_manifest(manifest_path, root=root, artifacts=fixture.paths)
            weights_path = root / "weights.final.json"
            write_json(weights_path, fixture.final_document())

            def fake_seal(document: dict[str, object]) -> None:
                self.assertIn(MODEL_FIELD, document)
                document["publication_seal"] = {"fixture": True}

            with unittest.mock.patch(
                "riscv_weight_model_seal.seal_model_document",
                side_effect=fake_seal,
            ):
                finalized = finalize_model_provenance(
                    weights_path, manifest_path=manifest_path, root=root
                )
            self.assertIn(MODEL_FIELD, finalized)
            self.assertEqual(finalized["publication_seal"], {"fixture": True})

    def test_runner_orders_pre_final_ml_manifest_reseal_and_verification(self) -> None:
        script = (SCRIPTS / "riscv-instruction-weight.sh").read_text(
            encoding="utf-8"
        )
        pre_final = script.index('cp "$output/weights.json" "$output/weights.pre-final.json"')
        ml = script.index("--finalize-weights", pre_final)
        manifest = script.index("riscv_weight_provenance.py\" create", ml)
        reseal = script.index("riscv_weight_provenance.py\" finalize", manifest)
        verify = script.index("riscv_weight_provenance.py\" verify", reseal)
        catalog = script.index("map-riscv-instruction-weights.py", verify)
        self.assertLess(pre_final, ml)
        self.assertLess(ml, manifest)
        self.assertLess(manifest, reseal)
        self.assertLess(reseal, verify)
        self.assertLess(verify, catalog)
        self.assertIn("qemu-binary.sha256", script)
        for role in (
            "kernel", "probe", "plugin", "qemu_version", "qemu_binary_checksum",
            "artifact_checksums", "samples", "run_design", "host_telemetry",
            "host_audit", "host_audit_binding", "weights_pre_finalization",
            "ml_validation",
        ):
            self.assertIn(f'--artifact "{role}=', script)

    def test_symlink_and_path_escape_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = Fixture(root)
            target = fixture.paths["kernel"]
            target.rename(root / "kernel.real")
            target.symlink_to(root / "kernel.real")
            with self.assertRaisesRegex(ProvenanceError, "符号链接"):
                fixture.create()


if __name__ == "__main__":
    unittest.main()
