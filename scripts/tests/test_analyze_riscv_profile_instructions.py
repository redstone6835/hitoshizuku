"""顶层 RISC-V profile 指令分析的合成端到端测试。"""

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
    "analyze_riscv_profile_instructions",
    SCRIPTS / "analyze-riscv-profile-instructions.py",
)
assert SPEC is not None and SPEC.loader is not None
ANALYZER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYZER)


def write_jsonl(path: Path, records: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records),
        encoding="utf-8",
    )


def rv_header(start: int) -> bytes:
    return struct.pack(
        "<8sHHIQQQQIIQQ",
        b"RVTCGT1\0",
        1,
        72,
        0x01020304,
        start,
        7,
        2_000_000,
        0x187,
        1,
        64,
        0,
        0,
    )


def rv_record(record_type: int, payload: bytes) -> bytes:
    return struct.pack("<HHI", record_type, 8 + len(payload), 0) + payload


def thread_record(timestamp: int, tid: int, comm: str) -> bytes:
    return rv_record(
        3,
        struct.pack(
            "<QIIIIiI32s",
            timestamp,
            7,
            tid,
            1000,
            1000,
            0,
            0,
            comm.encode() + b"\0" * (32 - len(comm)),
        ),
    )


def sample_record(timestamp: int, tid: int, ip: int, period: int) -> bytes:
    return rv_record(1, struct.pack("<QQQIIII", ip, timestamp, period, 7, tid, 0, 0))


def tid_stats_record(timestamp: int, tid: int, task_clock: int, samples: int) -> bytes:
    return rv_record(
        4,
        struct.pack(
            "<10QIiiI",
            timestamp,
            task_clock,
            task_clock,
            task_clock,
            samples,
            samples,
            0,
            0,
            0,
            0,
            tid,
            0,
            0,
            0,
        ),
    )


def jit_record(record_type: int, timestamp: int, payload: bytes = b"") -> bytes:
    return struct.pack("<IIQ", record_type, 16 + len(payload), timestamp) + payload


class AnalyzerEndToEndTests(unittest.TestCase):
    def make_run(self, root: Path, *, corrupt_count: bool = False) -> Path:
        run = root / "run"
        run.mkdir()
        start = 10_000_000_000
        descriptors = [
            (0, "addi", 4),
            (1, "addi", 2),
            (2, "ld", 4),
            (3, "sd", 4),
            (4, "mul", 4),
        ]
        mix: list[dict[str, object]] = [
            {
                "schema": ANALYZER.MIX_SCHEMA,
                "type": "header",
                "monotonic_ns": start - 20,
                "target": "riscv64",
                "configured_vcpus": 1,
                "max_supported_vcpus": 64,
                "epoch_ms": 1000,
                "descriptor_limit": 256,
                "mode_rule": "guest_pc_bit_63",
                "catalog_enabled": True,
            }
        ]
        for descriptor_id, mnemonic, size in descriptors:
            mix.append(
                {
                    "schema": ANALYZER.MIX_SCHEMA,
                    "type": "descriptor",
                    "monotonic_ns": start - 10 + descriptor_id,
                    "id": descriptor_id,
                    "mnemonic": mnemonic,
                    "size": size,
                }
            )
        mix.append(
            {
                "schema": ANALYZER.MIX_SCHEMA,
                "type": "window_start",
                "monotonic_ns": start,
                "window_id": 1,
                "detected_from_control": True,
            }
        )
        epoch_periods: list[int] = []
        for index in range(120):
            if index < 40:
                counts = {0: 900, 1: 120, 2: 180, 3: 20, 4: 10}
                kernel_fraction = 0
            elif index < 80:
                counts = {0: 100, 1: 20, 2: 40, 3: 160, 4: 950}
                kernel_fraction = 1
            else:
                counts = {0: 80, 1: 350, 2: 620, 3: 590, 4: 30}
                kernel_fraction = 0
            rows = []
            user_total = 0
            kernel_total = 0
            for descriptor_id, count in counts.items():
                user = 0 if kernel_fraction else count
                kernel = count if kernel_fraction else 0
                rows.append({"id": descriptor_id, "user": user, "kernel": kernel})
                user_total += user
                kernel_total += kernel
            canonical_user = user_total
            canonical_kernel = kernel_total
            mix_user = canonical_user + (1 if corrupt_count and index == 0 else 0)
            raw_user = canonical_user + (1 if index == 10 else -1 if index == 11 else 0)
            raw_kernel = canonical_kernel + (1 if index == 50 else -1 if index == 51 else 0)
            total = canonical_user + canonical_kernel
            translated_tb = 3 + index % 5
            translated_insns = 12 + index % 7
            executed_tb = 100 + index % 11
            mix.append(
                {
                    "schema": ANALYZER.MIX_SCHEMA,
                    "type": "sample",
                    "monotonic_ns": start + (index + 1) * 1_000_000_000,
                    "window_id": 1,
                    "epoch": index + 1,
                    "reason": "interval" if index < 119 else "window_stop",
                    "tb_delta": {
                        "user": 0 if kernel_fraction else executed_tb,
                        "kernel": executed_tb if kernel_fraction else 0,
                    },
                    "instruction_delta": {"user": raw_user, "kernel": raw_kernel},
                    "mix_instruction_delta": {"user": mix_user, "kernel": canonical_kernel},
                    "translated": {
                        "tb": translated_tb,
                        "instructions": translated_insns,
                        "tb_delta": translated_tb,
                        "instruction_delta": translated_insns,
                        "max_tb_instructions": 8,
                    },
                    "counter_regression": False,
                    "mix": rows,
                }
            )
            epoch_periods.append(total * 20 + executed_tb * 9 + translated_insns * 13)
        stop = start + 120 * 1_000_000_000
        mix.extend(
            [
                {
                    "schema": ANALYZER.MIX_SCHEMA,
                    "type": "window_stop",
                    "monotonic_ns": stop + 1,
                    "window_id": 1,
                    "detected_from_control": True,
                },
                {
                    "schema": ANALYZER.MIX_SCHEMA,
                    "type": "quality",
                    "monotonic_ns": stop + 2,
                    "complete": True,
                    "configured_vcpus": 1,
                    "max_supported_vcpus": 64,
                    "descriptor_count": 5,
                    "descriptor_limit": 256,
                    "translated_blocks": 1,
                    "translated_instructions": 1,
                    "max_tb_instructions": 8,
                    "windows": 1,
                    "samples": 120,
                    "start_detections": 1,
                    "stop_detections": 1,
                    "exit_stops": 0,
                    "errors": {
                        "output_write": 0,
                        "control_read": 0,
                        "counter_regression": 0,
                        "descriptor_overflow_instructions": 0,
                        "descriptor_overflow_blocks": 0,
                        "disassembly": 0,
                        "mnemonic_truncation": 0,
                        "invalid_instruction_size": 0,
                        "instruction_data": 0,
                        "unsupported_vcpu": 0,
                        "late_translation_drop": 0,
                        "sampler_wait": 0,
                    },
                    "catalog": {
                        "enabled": True,
                        "records": 1,
                        "write_errors": 0,
                        "dropped_blocks": 0,
                        "allocation_failures": 0,
                        "duplicate_pc": 0,
                        "duplicate_exact": 0,
                        "tracking_drops": 0,
                    },
                },
            ]
        )
        write_jsonl(run / "instruction-mix.jsonl", mix)

        catalog = [
            {
                "schema": "mygo.riscv-tb-catalog.v1",
                "type": "header",
                "monotonic_ns": start - 100,
                "target": "riscv64",
                "configured_vcpus": 1,
                "seen_slots": 1024,
            },
            {
                "schema": "mygo.riscv-tb-catalog.v1",
                "type": "tb",
                "monotonic_ns": start - 80,
                "translation_begin_ns": start - 90,
                "host_tid": 12,
                "translation_index": 1,
                "guest_pc": "0x0000000000001000",
                "mode": "user",
                "instruction_count": 1,
                "duplicate_pc": False,
                "duplicate_exact": False,
                "descriptor_overflow": 0,
                "decode_errors": 0,
                "instructions": [
                    {
                        "pc": "0x0000000000001000",
                        "size": 4,
                        "bytes": "13000000",
                        "bytes_complete": True,
                        "descriptor_id": 0,
                        "mnemonic": "addi",
                    }
                ],
            },
            {
                "schema": "mygo.riscv-tb-catalog.v1",
                "type": "quality",
                "monotonic_ns": stop + 10,
                "translated_blocks": 1,
                "records": 1,
                "write_errors": 0,
                "dropped_blocks": 0,
                "duplicate_pc": 0,
                "duplicate_exact": 0,
                "tracking_drops": 0,
            },
        ]
        write_jsonl(run / "instruction-catalog.jsonl", catalog)

        code = b"\x90" * 16
        load = struct.pack("<IIQQQQ", 7, 12, 0x100000, 0x100000, len(code), 1)
        jitdump = struct.pack("<IIIIIIQQ", 0x4A695444, 1, 40, 62, 0, 7, start - 100, 0)
        jitdump += jit_record(0, start - 70, load + b"guest-0x1000\0" + code)
        # QEMU 10 不保证写 JIT_CODE_CLOSE；完整 EOF 本身即是有效结束。
        (run / "jit-7.dump").write_bytes(jitdump)

        records = bytearray(rv_header(start - 100))
        records += thread_record(start - 60, 1000, "qemu-system-ris")
        records += thread_record(start - 60, 1001, "CPU 0/TCG")
        records += rv_record(6, struct.pack("<QII", start - 50, 0, 1))
        records += rv_record(6, struct.pack("<QII", start - 40, 1, 2))
        vcpu_period_sum = 0
        for index, period in enumerate(epoch_periods):
            timestamp = start + index * 1_000_000_000 + 500_000_000
            records += sample_record(timestamp, 1001, 0x100004, period)
            records += sample_record(timestamp + 1, 1001, 0x400000, 1000)
            vcpu_period_sum += period + 1000
        records += sample_record(start + 500_000_100, 1000, 0x500000, 1234)
        records += rv_record(6, struct.pack("<QII", stop + 20, 0, 3))
        # final read 比 sample period 多一个可审计的尾部残量。
        exact_vcpu = vcpu_period_sum + 777
        records += tid_stats_record(stop + 30, 1000, 1234, 1)
        records += tid_stats_record(stop + 30, 1001, exact_vcpu, 240)
        sample_count = 241
        quality = struct.pack(
            "<14Q6I",
            stop + 40,
            120_000_000_000,
            120_000_000_000,
            exact_vcpu + 1234,
            exact_vcpu + 1234,
            exact_vcpu + 1234,
            sample_count,
            sample_count,
            0,
            0,
            0,
            0,
            1_000_000,
            0,
            2,
            2,
            0,
            3,
            0,
            0,
        )
        records += rv_record(7, quality)
        (run / "tcg-time-samples.bin").write_bytes(records)

        (run / "tid-namespace-map.tsv").write_text(
            "monotonic_ns\thost_tid\tcontainer_tid\tnspid_chain\tcomm\n"
            f"{start - 30}\t1000\t7\t1000,7\tqemu-system-ris\n"
            f"{start - 30}\t1001\t12\t1001,12\tCPU 0/TCG\n",
            encoding="utf-8",
        )
        (run / "progress.tsv").write_text(
            "milestone\tmonotonic_ns\n"
            f"0\t{start + 1_000_000_000}\n"
            f"64\t{start + 35_000_000_000}\n"
            f"128\t{start + 50_000_000_000}\n"
            f"256\t{start + 82_000_000_000}\n"
            f"384\t{start + 105_000_000_000}\n",
            encoding="utf-8",
        )
        return run

    def test_complete_analysis_outputs_and_recovers_from_cache(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            run = self.make_run(Path(directory))
            arguments = [
                str(run),
                "--min-segment-seconds",
                "10",
                "--boundary-bootstrap-replicates",
                "5",
                "--global-permutation-replicates",
                "19",
                "--permutation-replicates",
                "19",
                "--weight-bootstrap-replicates",
                "5",
                "--distribution-bootstrap-replicates",
                "19",
            ]
            self.assertEqual(ANALYZER.main(arguments), 0)
            analysis = run / "analysis"
            for name in (
                "instruction-list.csv",
                "epoch-timeline.csv",
                "epoch-instruction-counts.csv",
                "segmentation.json",
                "native-ip-distribution.csv",
                "stage-weighted-instructions.csv",
                "quality.json",
                "analysis-report.md",
                "analysis-state.json",
            ):
                self.assertTrue((analysis / name).is_file(), name)
            instruction_rows = (analysis / "instruction-list.csv").read_text().splitlines()
            self.assertEqual(len(instruction_rows), 6)
            self.assertTrue(any("addi,2," in row for row in instruction_rows))
            self.assertTrue(any("addi,4," in row for row in instruction_rows))
            segmentation = json.loads((analysis / "segmentation.json").read_text())
            self.assertEqual(len(segmentation["sensitivity"]["configurations"]), 12)
            self.assertGreaterEqual(len(segmentation["stages"]), 2)
            self.assertTrue(segmentation["global_change_point_test"]["selection_corrected"])
            self.assertEqual(
                len(segmentation["global_change_point_block_sensitivity"]["tests"]),
                2,
            )
            self.assertEqual(
                segmentation["boundary_bootstrap"]["block_length"],
                segmentation["serial_dependence"]["primary_block_length"],
            )
            self.assertGreaterEqual(
                segmentation["serial_dependence"]["long_block_length"],
                segmentation["serial_dependence"]["primary_block_length"],
            )
            self.assertEqual(segmentation["progress_annotation"]["source"], "progress.tsv")
            weighted = json.loads(
                (analysis / "stage-weighted-distributions.json").read_text()
            )
            for stage, definition in zip(weighted, segmentation["stages"], strict=True):
                items = [
                    item
                    for item in stage["distribution"]["items"]
                    if item["instruction"] != "OTHER"
                ]
                self.assertEqual(len(items), 5)
                self.assertEqual(
                    sum(item["exact_count"] for item in items),
                    definition["instruction_count"],
                )
                self.assertTrue(
                    all(
                        item["confidence_interval_scope"]
                        == "conditional-on-point-estimated-weights"
                        for item in items
                    )
                )
                self.assertTrue(stage["weight_uncertainty"]["available"])
                self.assertEqual(
                    stage["dependence_support"]["source"],
                    "stage-weighted-acf-variogram-iat-with-shared-minimum",
                )
                self.assertTrue(
                    all(item["weight_ci_share_envelope"] is not None for item in items)
                )
            quality = json.loads((analysis / "quality.json").read_text())
            self.assertTrue(quality["valid"])
            self.assertEqual(quality["count_closure"]["skewed_epoch_domains"], 4)
            self.assertEqual(
                quality["count_closure"]["cumulative_snapshot_skew"],
                {"kernel": 0, "user": 0},
            )
            self.assertAlmostEqual(
                quality["measurement"]["catalog_coverage_ratio"], 1.0
            )
            perf_cache = json.loads((analysis / ".perf-cache.json").read_text())["payload"]
            self.assertEqual(perf_cache["translation_match"]["close_records"], 0)
            self.assertLess(abs(perf_cache["gate_alignment"]["start_skew_ns"]), 100)
            accounting = perf_cache["vcpu"]["task_clock_accounting"]["1001"]
            self.assertEqual(accounting["unlocated_tail_task_clock_ns"], 777)
            self.assertEqual(
                accounting["located_task_clock_ns"]
                + accounting["unlocated_tail_task_clock_ns"],
                accounting["exact_task_clock_ns"],
            )
            # 第二次运行必须能够复用完整缓存且仍生成一致报告。
            report_before = (analysis / "analysis-report.md").read_text()
            self.assertEqual(ANALYZER.main(arguments), 0)
            self.assertEqual(report_before, (analysis / "analysis-report.md").read_text())

    def test_rejects_epoch_count_that_does_not_close(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            run = self.make_run(Path(directory), corrupt_count=True)
            with self.assertRaises(ANALYZER.AnalysisError):
                ANALYZER.parse_instruction_mix(run / "instruction-mix.jsonl")


if __name__ == "__main__":
    unittest.main()
