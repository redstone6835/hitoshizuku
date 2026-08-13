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

    def test_random_effects_and_dependence_diagnostics_are_present(self) -> None:
        addi = self.items[("addi", 4, "throughput")]
        self.assertTrue(addi["cross_run_random_effects"]["identifiable"])
        self.assertIsNotNone(addi["cross_run_random_effects"]["i_squared"])
        self.assertEqual(len(addi["autocorrelation"]), 3)
        self.assertIn("bootstrap_ci", addi["effects"])
        self.assertTrue(
            self.result["simultaneous_inference"]["run_is_highest_cluster"]
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
    """验证污染、非物理解和 bootstrap 失败不会被发布为高置信权重。"""

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
            return ({key: -0.5 + jitter}, {key: (0.0, 0.0, 0.0, None)})

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

    def test_low_bootstrap_valid_fraction_is_not_high_confidence(self) -> None:
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
            if current < 100:
                return None
            key = state.keys[0]
            jitter = ((current % 21) - 10) * 1e-5
            return ({key: 0.8 + jitter}, {key: (0.0, 0.0, 0.0, None)})

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
        self.assertEqual(inference["valid_replicates"], 1000)
        self.assertAlmostEqual(inference["valid_fraction"], 10.0 / 11.0)
        self.assertNotIn(
            "insufficient-bootstrap-replicates", item["quality_failures"]
        )
        self.assertIn(
            "insufficient-bootstrap-valid-fraction",
            item["quality_failures"],
        )
        self.assertNotEqual(item["quality"], "high-confidence")


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

        self.assertEqual(
            MODEL._wls(matrix, response, weights), dense_reference()
        )


if __name__ == "__main__":
    unittest.main()
