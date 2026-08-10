"""RISC-V 指令画像质量校验器的边界与兼容性回归测试。"""

from __future__ import annotations

import importlib.util
import json
import struct
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
SCRIPTS = REPOSITORY / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location(
    "validate_riscv_instruction_profile",
    SCRIPTS / "validate-riscv-instruction-profile.py",
)
assert SPEC is not None and SPEC.loader is not None
VALIDATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATOR)


def write_jsonl(path: Path, records: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records),
        encoding="utf-8",
    )


def mix_records(
    skews: list[tuple[int, int]],
    *,
    canonical_user: int = 1_000_000,
    canonical_kernel: int = 0,
    emitted_descriptors: int = 1,
    registered_descriptors: int | None = None,
    configured_vcpus: int = 1,
    max_tb_instructions: int = 256,
) -> list[dict[str, object]]:
    start = 1_000_000_000
    registered = registered_descriptors or emitted_descriptors
    records: list[dict[str, object]] = [
        {
            "schema": VALIDATOR.MIX_SCHEMA,
            "type": "header",
            "monotonic_ns": start - emitted_descriptors - 2,
            "configured_vcpus": configured_vcpus,
        }
    ]
    for descriptor_id in range(emitted_descriptors):
        records.append(
            {
                "schema": VALIDATOR.MIX_SCHEMA,
                "type": "descriptor",
                "monotonic_ns": start - emitted_descriptors - 1 + descriptor_id,
                "id": descriptor_id,
                "mnemonic": "addi" if descriptor_id == 0 else f"synthetic-{descriptor_id}",
                "size": 4,
            }
        )
    records.append(
        {
            "schema": VALIDATOR.MIX_SCHEMA,
            "type": "window_start",
            "monotonic_ns": start,
            "window_id": 1,
        }
    )
    for index, (user_skew, kernel_skew) in enumerate(skews, 1):
        rows = [
            {
                "id": 0,
                "user": canonical_user,
                "kernel": canonical_kernel,
            }
        ]
        records.append(
            {
                "schema": VALIDATOR.MIX_SCHEMA,
                "type": "sample",
                "monotonic_ns": start + index * 1_000_000_000,
                "window_id": 1,
                "epoch": index,
                "counter_regression": False,
                "instruction_delta": {
                    "user": canonical_user + user_skew,
                    "kernel": canonical_kernel + kernel_skew,
                },
                "mix_instruction_delta": {
                    "user": canonical_user,
                    "kernel": canonical_kernel,
                },
                "mix": rows,
            }
        )
    stop = start + len(skews) * 1_000_000_000 + 1
    records.extend(
        [
            {
                "schema": VALIDATOR.MIX_SCHEMA,
                "type": "window_stop",
                "monotonic_ns": stop,
                "window_id": 1,
            },
            {
                "schema": VALIDATOR.MIX_SCHEMA,
                "type": "quality",
                "monotonic_ns": stop + 1,
                "complete": True,
                "configured_vcpus": configured_vcpus,
                "descriptor_count": registered,
                "windows": 1,
                "samples": len(skews),
                "start_detections": 1,
                "stop_detections": 1,
                "exit_stops": 0,
                "max_tb_instructions": max_tb_instructions,
                "translated_blocks": 1,
                "errors": {},
                "catalog": {
                    "enabled": True,
                    "records": 1,
                    "write_errors": 0,
                    "dropped_blocks": 0,
                    "allocation_failures": 0,
                    "tracking_drops": 0,
                },
            },
        ]
    )
    return records


def collector_record(record_type: int, payload: bytes) -> bytes:
    return VALIDATOR.TCG_RECORD.pack(
        record_type,
        VALIDATOR.TCG_RECORD.size + len(payload),
        0,
    ) + payload


class InstructionMixValidationTests(unittest.TestCase):
    def parse(self, records: list[dict[str, object]], expected_vcpus: int = 1):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "instruction-mix.jsonl"
            write_jsonl(path, records)
            errors: list[str] = []
            result = VALIDATOR.parse_mix(path, expected_vcpus, errors)
        return result, errors

    def test_adjacent_inverse_snapshot_skew_closes_exactly(self) -> None:
        result, errors = self.parse(
            mix_records(
                [(2_175, 0), (-2_175, 0)],
                canonical_user=10_000_000,
            )
        )

        self.assertEqual(errors, [])
        skew = result["counter_snapshot_skew"]
        self.assertEqual(skew["cumulative"], {"user": 0, "kernel": 0})
        self.assertEqual(skew["sign_reversals"]["user"], 1)
        self.assertEqual(skew["window_closure_mode"], "exact")

    def test_bounded_window_residual_at_most_100_ppm_passes(self) -> None:
        result, errors = self.parse(mix_records([(90, 0), (90, 0)]))

        self.assertEqual(errors, [])
        skew = result["counter_snapshot_skew"]
        self.assertEqual(skew["cumulative_relative_ppm"], 90)
        self.assertEqual(skew["window_closure_mode"], "bounded-boundary")

    def test_epoch_skew_over_1000_ppm_is_rejected(self) -> None:
        _, errors = self.parse(mix_records([(1_001, 0)]))

        self.assertIn(
            "instruction counter/descriptor epoch snapshot skew exceeds 1000 ppm",
            errors,
        )

    def test_cumulative_skew_over_100_ppm_is_rejected(self) -> None:
        _, errors = self.parse(mix_records([(150, 0), (150, 0)]))

        self.assertIn(
            "instruction counter/descriptor window skew exceeds boundary or 100 ppm allowance",
            errors,
        )

    def test_real_smoke_lazy_registry_and_smp_skew_shape_passes(self) -> None:
        records = mix_records(
            [
                (2_175, 0),
                (-2_175, 0),
                (0, 1_599),
                (0, -1_589),
                (0, 6_029),
                (0, -6_039),
                (0, 8_488),
                (0, -8_488),
            ],
            canonical_user=20_000_000,
            canonical_kernel=10_000_000,
            emitted_descriptors=149,
            registered_descriptors=157,
            configured_vcpus=8,
        )

        result, errors = self.parse(records, expected_vcpus=8)

        self.assertEqual(errors, [])
        self.assertEqual(result["descriptor_count"], 149)
        self.assertEqual(result["translated_descriptor_count"], 157)
        self.assertEqual(
            result["counter_snapshot_skew"]["cumulative"],
            {"user": 0, "kernel": 0},
        )


class LegacyCollectorValidationTests(unittest.TestCase):
    def test_legacy_tid_stats_and_quality_records_are_normalized(self) -> None:
        period_ns = 2_000_000
        leader_tid = 7
        vcpu_tid = 8
        header = VALIDATOR.TCG_HEADER.pack(
            b"RVTCGT1\0",
            1,
            VALIDATOR.TCG_HEADER.size,
            0x01020304,
            1_000,
            leader_tid,
            period_ns,
            0,
            1,
            64,
            0,
            0,
        )
        comm = b"CPU 0/TCG"
        thread = VALIDATOR.TCG_THREAD.pack(
            1_100,
            leader_tid,
            vcpu_tid,
            1_000,
            1_000,
            0,
            0,
            comm + b"\0" * (32 - len(comm)),
        )
        sample = VALIDATOR.TCG_SAMPLE.pack(
            0x1000,
            1_200,
            period_ns,
            leader_tid,
            vcpu_tid,
            0,
            0,
        )
        tid_stats = VALIDATOR.TCG_TID_STATS_LEGACY.pack(
            1_300,
            100,
            100,
            100,
            1,
            1,
            0,
            0,
            vcpu_tid,
            0,
            0,
            0,
        )
        gates = b"".join(
            collector_record(6, VALIDATOR.TCG_GATE.pack(1_050 + index, active, 0))
            for index, active in enumerate((0, 1, 0))
        )
        quality = VALIDATOR.TCG_QUALITY_LEGACY.pack(
            1_400,
            1_000,
            900,
            100,
            100,
            100,
            1,
            1,
            0,
            0,
            1_000_000,
            0,
            1,
            1,
            0,
            3,
            0,
            0,
        )
        payload = b"".join(
            [
                header,
                collector_record(3, thread),
                gates,
                collector_record(1, sample),
                collector_record(4, tid_stats),
                collector_record(7, quality),
            ]
        )

        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "tcg-time-samples.bin"
            path.write_bytes(payload)
            errors: list[str] = []
            result, all_ips, vcpu_ips = VALIDATOR.parse_collector(
                path,
                1,
                {
                    "host_to_container": {leader_tid: 70, vcpu_tid: 80},
                    "vcpu_host_tids": {vcpu_tid},
                },
                errors,
            )

        self.assertEqual(errors, [])
        self.assertEqual(result["samples"], 1)
        self.assertEqual(result["vcpu_samples"], 1)
        self.assertEqual(all_ips, {0x1000: 1})
        self.assertEqual(vcpu_ips, {0x1000: 1})


if __name__ == "__main__":
    unittest.main()
