"""RISC-V 指令权重样本合并与 catalog 映射回归测试。"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))

from rv_instruction_microbench_model import fit_microbenchmark_weight_model


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
) -> dict[str, str]:
    return {
        "run_id": run_id,
        "block_id": "block-1",
        "pair_id": pair_id,
        "sequence": str(sequence),
        "role": role,
        "order": "probe-first",
        "instruction": "mul",
        "encoding_bytes": "4",
        "pattern": "independent",
        "count_level": "0",
        "requested_count": "100",
        "blocks": "1",
        "slots_per_block": "100",
        "executed_instruction": "mul" if role == "probe" else "addi",
        "guest_elapsed_ns": "1000",
        "rdtime_delta": "900",
        "timer_reads": "2",
        "baseline_instruction": "addi",
        "baseline_encoding_bytes": "4",
    }


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
        "relative_weight": 1.0,
        "simultaneous_ci": [value * 0.9, value * 1.1],
        "quality": quality,
        "quality_failures": [],
    }


def model_document(
    instructions: list[dict[str, object]],
) -> dict[str, object]:
    return {
        "schema_version": MAPPER.MODEL_SCHEMA_VERSION,
        "instruction_key": MAPPER.MODEL_INSTRUCTION_KEY,
        "model": (
            "paired-huber-heteroscedastic-hierarchical-moving-block-"
            "max-standardized-deviation"
        ),
        "instructions": instructions,
    }


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
            self.assertEqual(row["paired_contrast_purity"], 1.0)
            self.assertEqual(
                row["target_descriptor"]["encoding_key"], "rv64:32:m:mul"
            )

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

    def test_multiple_runs_are_unique_and_input_order_independent(self) -> None:
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

        self.assertEqual(forward, reverse)
        self.assertEqual(
            json.dumps(forward, separators=(",", ":")),
            json.dumps(reverse, separators=(",", ":")),
        )
        self.assertEqual(
            [row["run_id"] for row in forward],
            ["run-1"] * 2 + ["run-2"] * 2,
        )
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
            },
            2: {
                "mode": "timing",
                "plugin_thread_cpu_ns": 700,
                "plugin_monotonic_ns": 760,
                "translations_during_window": 0,
            },
        }

        rows = MERGER.merge_dual_mode_runs(
            validation_guest,
            timing_windows(),
            [(timing_guest, timing, plugin_off_guest)],
        )

        self.assertEqual(len(rows), 2)
        self.assertTrue(all(row["plugin_mode"] == "timing" for row in rows))
        self.assertTrue(
            all(row["translations_during_window"] == 0 for row in rows)
        )
        self.assertEqual(rows[0]["target_count"], 100)
        self.assertEqual(rows[0]["plugin_thread_cpu_ns"], 800)
        self.assertEqual(rows[0]["plugin_off_guest_ns"], 980)


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

        self.assertEqual(result["model_schema_version"], 2)
        self.assertEqual(result["mapped_model_encoding_count"], 1)
        self.assertEqual(result["orphan_model_encoding_count"], 0)
        self.assertEqual(
            result["instructions"][0]["assignment"],
            "measured-but-confidence-gates-failed",
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
        self.assertIn('--expected-key-count "$expected_catalog_keys"', script)

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
