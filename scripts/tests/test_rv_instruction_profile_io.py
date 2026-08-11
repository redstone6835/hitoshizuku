"""RISC-V 指令耗时剖析输入与时间映射测试。"""

from __future__ import annotations

import json
import struct
import tempfile
import unittest
from pathlib import Path

from scripts.rv_instruction_profile_io import (
    CATALOG_SCHEMA,
    JIT_HEADER_MAGIC,
    CatalogSource,
    JitCodeClose,
    JitCodeLoad,
    JitCodeMove,
    JitOtherRecord,
    MatchStatistics,
    MatchedJitLoad,
    PerfSample,
    ProfileIoError,
    RvTcgAttachFailure,
    RvTcgGate,
    RvTcgLost,
    RvTcgQuality,
    RvTcgThread,
    RvTcgTidStats,
    RvTcgUnknown,
    SampleLocation,
    TbCatalogRecord,
    TidNamespaceEntry,
    TidNamespaceMap,
    TimeAwareJitMap,
    iter_jitdump_records,
    iter_matched_jit_records,
    iter_rv_tcg_records,
    iter_tb_catalog,
    profile_quality_summary,
    read_rv_tcg_file_header,
    read_tid_namespace_tsv,
)


def rv_header() -> bytes:
    return struct.pack(
        "<8sHHIQQQQIIQQ",
        b"RVTCGT1\0",
        1,
        72,
        0x01020304,
        100,
        200,
        2_000_000,
        0x187,
        1,
        64,
        0,
        0,
    )


def rv_record(record_type: int, payload: bytes, flags: int = 0) -> bytes:
    return struct.pack("<HHI", record_type, len(payload) + 8, flags) + payload


def jit_header() -> bytes:
    return struct.pack("<IIIIIIQQ", JIT_HEADER_MAGIC, 1, 40, 243, 0, 7, 1, 0)


def jit_record(record_id: int, timestamp: int, payload: bytes = b"") -> bytes:
    return struct.pack("<IIQ", record_id, 16 + len(payload), timestamp) + payload


def jit_load(
    timestamp: int,
    *,
    tid: int,
    guest_pc: int,
    address: int,
    index: int,
    code: bytes = b"\x90\x90\x90\x90",
) -> bytes:
    fixed = struct.pack("<IIQQQQ", 7, tid, address, address, len(code), index)
    return jit_record(0, timestamp, fixed + f"guest-0x{guest_pc:x}".encode() + b"\0" + code)


def catalog_tb(
    timestamp: int,
    *,
    tid: int,
    guest_pc: int,
    index: int,
    mnemonic: str = "addi",
) -> dict[str, object]:
    return {
        "schema": CATALOG_SCHEMA,
        "type": "tb",
        "monotonic_ns": timestamp,
        "translation_begin_ns": timestamp - 5,
        "host_tid": tid,
        "translation_index": index,
        "guest_pc": f"0x{guest_pc:016x}",
        "mode": "kernel",
        "instruction_count": 1,
        "duplicate_pc": index != 1,
        "duplicate_exact": index != 1,
        "descriptor_overflow": 0,
        "decode_errors": 0,
        "instructions": [
            {
                "pc": f"0x{guest_pc:016x}",
                "size": 4,
                "bytes": "13000000",
                "bytes_complete": True,
                "descriptor_id": 4,
                "mnemonic": mnemonic,
            }
        ],
    }


def write_jsonl(path: Path, records: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records),
        encoding="utf-8",
    )


class RvTcgParserTests(unittest.TestCase):
    def test_parses_every_current_record_and_preserves_unknown(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "samples.bin"
            current_tid = struct.pack(
                "<10QIiiI", 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, -2, -3, 0
            )
            current_quality = struct.pack(
                "<14Q6I", *range(100, 114), 8, 7, 1, 2, 3, 0
            )
            path.write_bytes(
                rv_header()
                + rv_record(1, struct.pack("<QQQIIII", 1, 2, 3, 4, 5, 6, 0), 9)
                + rv_record(2, struct.pack("<QQQII", 7, 8, 9, 10, 0))
                + rv_record(
                    3,
                    struct.pack(
                        "<QIIIIiI32s", 11, 12, 13, 14, 15, -16, 0, b"CPU 0/TCG\0"
                    ),
                )
                + rv_record(4, current_tid)
                + rv_record(5, struct.pack("<QIiII", 21, 22, -23, 24, 0))
                + rv_record(6, struct.pack("<QII", 25, 1, 1))
                + rv_record(7, current_quality)
                + rv_record(99, b"future", 7)
            )

            header = read_rv_tcg_file_header(path)
            records = list(iter_rv_tcg_records(path))

            self.assertEqual(header.target_pid, 200)
            self.assertEqual(len(records), 8)
            self.assertIsInstance(records[0], PerfSample)
            self.assertEqual(records[0].flags, 9)
            self.assertIsInstance(records[1], RvTcgLost)
            self.assertIsInstance(records[2], RvTcgThread)
            self.assertEqual(records[2].comm, "CPU 0/TCG")
            self.assertIsInstance(records[3], RvTcgTidStats)
            self.assertEqual(records[3].throttle_records, 18)
            self.assertIsInstance(records[4], RvTcgAttachFailure)
            self.assertIsInstance(records[5], RvTcgGate)
            self.assertIsInstance(records[6], RvTcgQuality)
            self.assertEqual(records[6].running_ratio_ppm, 112)
            self.assertEqual(records[6].status, 0)
            self.assertEqual(records[7], RvTcgUnknown(99, b"future", 7))

    def test_accepts_legacy_v1_statistics_layout(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "legacy.bin"
            legacy_tid = struct.pack("<8QIiiI", *range(1, 9), 9, 0, 0, 0)
            legacy_quality = struct.pack("<12Q6I", *range(20, 32), 8, 8, 0, 2, 0, 1)
            path.write_bytes(
                rv_header() + rv_record(4, legacy_tid) + rv_record(7, legacy_quality)
            )

            tid_stats, quality = list(iter_rv_tcg_records(path))

            self.assertEqual(tid_stats.throttle_records, 0)
            self.assertEqual(tid_stats.unthrottle_records, 0)
            self.assertEqual(quality.running_ratio_ppm, 30)
            self.assertEqual(quality.loss_ratio_ppm, 31)
            self.assertEqual(quality.status, 1)

    def test_rejects_truncated_record(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.bin"
            path.write_bytes(rv_header() + struct.pack("<HHI", 1, 48, 0) + b"short")
            with self.assertRaisesRegex(ProfileIoError, "截断"):
                list(iter_rv_tcg_records(path))


class JitDumpParserTests(unittest.TestCase):
    def test_streams_load_move_close_and_skips_debug_payload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "jit.dump"
            move = struct.pack("<IIQQQQQ", 7, 12, 0x1000, 0x1000, 0x2000, 4, 5)
            path.write_bytes(
                jit_header()
                + jit_record(2, 10, b"debug")
                + jit_load(20, tid=12, guest_pc=0x8000, address=0x1000, index=5)
                + jit_record(1, 30, move)
                + jit_record(3, 40)
            )

            records = list(iter_jitdump_records(path, include_code=True))

            self.assertIsInstance(records[0], JitOtherRecord)
            self.assertIsInstance(records[1], JitCodeLoad)
            self.assertEqual(records[1].guest_pc, 0x8000)
            self.assertEqual(records[1].code_bytes, b"\x90" * 4)
            self.assertIsInstance(records[2], JitCodeMove)
            self.assertEqual(records[2].new_code_addr, 0x2000)
            self.assertIsInstance(records[3], JitCodeClose)


class CatalogAndMatchTests(unittest.TestCase):
    def test_lazy_catalog_materialization_retains_real_instruction_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "catalog.jsonl"
            write_jsonl(
                path,
                [
                    {
                        "schema": CATALOG_SCHEMA,
                        "type": "header",
                        "monotonic_ns": 1,
                        "target": "riscv64",
                        "configured_vcpus": 8,
                        "seen_slots": 1024,
                    },
                    catalog_tb(100, tid=12, guest_pc=0x8000, index=1),
                    {
                        "schema": CATALOG_SCHEMA,
                        "type": "quality",
                        "monotonic_ns": 200,
                        "translated_blocks": 1,
                        "records": 1,
                        "write_errors": 0,
                        "dropped_blocks": 0,
                        "duplicate_pc": 0,
                        "duplicate_exact": 0,
                        "tracking_drops": 0,
                    },
                ],
            )

            record = [
                item
                for item in iter_tb_catalog(path, include_instructions=False)
                if isinstance(item, TbCatalogRecord)
            ][0]
            materialized = record.materialize()

            self.assertIsNone(record.instructions)
            self.assertEqual(materialized.instructions[0].size, 4)
            self.assertEqual(materialized.instructions[0].raw_bytes, b"\x13\0\0\0")

    def test_matches_each_translation_by_container_tid_pc_and_fifo(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            catalog = root / "catalog.jsonl"
            jitdump = root / "jit.dump"
            write_jsonl(
                catalog,
                [
                    catalog_tb(100, tid=12, guest_pc=0x8000, index=1),
                    catalog_tb(200, tid=12, guest_pc=0x8000, index=2),
                    catalog_tb(300, tid=13, guest_pc=0x9000, index=3),
                ],
            )
            jitdump.write_bytes(
                jit_header()
                + jit_load(150, tid=12, guest_pc=0x8000, address=0x1000, index=10)
                + jit_load(250, tid=12, guest_pc=0x8000, address=0x2000, index=11)
            )
            stats = MatchStatistics()

            records = list(iter_matched_jit_records(catalog, jitdump, stats=stats))

            self.assertEqual(
                [record.catalog.translation_index for record in records], [1, 2]
            )
            self.assertEqual(stats.matched_loads, 2)
            self.assertEqual(stats.unmatched_catalog_records, 1)
            self.assertEqual(stats.catalog_container_tids, {12, 13})
            self.assertEqual(stats.jit_container_tids, {12})
            self.assertAlmostEqual(stats.catalog_match_ratio, 2 / 3)


class NamespaceAndTimeMapTests(unittest.TestCase):
    @staticmethod
    def tb(index: int) -> TbCatalogRecord:
        return TbCatalogRecord(
            1,
            1,
            12,
            index,
            0x8000,
            "kernel",
            1,
            False,
            False,
            0,
            0,
            (),
            CatalogSource(Path("unused"), 0, 0, 0),
        )

    def test_address_reuse_move_close_and_namespace_mapping(self) -> None:
        first = JitCodeLoad(10, 7, 12, 0x1000, 0x1000, 16, 1, "guest-0x8000")
        replacement = JitCodeLoad(20, 7, 12, 0x1000, 0x1000, 16, 2, "guest-0x9000")
        timeline = [
            MatchedJitLoad(first, self.tb(1)),
            MatchedJitLoad(replacement, None),
            JitCodeMove(30, 7, 12, 0x1000, 0x1000, 0x2000, 16, 2),
            JitCodeClose(40),
        ]
        samples = [
            PerfSample(0x1004, 5, 1, 7, 100, 0),
            PerfSample(0x1004, 11, 1, 7, 100, 0),
            PerfSample(0x1004, 21, 1, 7, 100, 0),
            PerfSample(0x1004, 31, 1, 7, 100, 0),
            PerfSample(0x2004, 32, 1, 7, 100, 0),
            PerfSample(0x2004, 41, 1, 7, 100, 0),
        ]
        namespace = TidNamespaceMap((TidNamespaceEntry(1, 100, 12, (100, 12), "CPU 0/TCG"),))

        mapped = list(TimeAwareJitMap(timeline).map_sorted_samples(samples, tid_namespace=namespace))

        self.assertEqual(
            [item.location for item in mapped],
            [
                SampleLocation.NATIVE_QEMU,
                SampleLocation.MAPPED_TCG,
                SampleLocation.UNKNOWN,
                SampleLocation.NATIVE_QEMU,
                SampleLocation.UNKNOWN,
                SampleLocation.NATIVE_QEMU,
            ],
        )
        self.assertEqual(mapped[1].catalog.translation_index, 1)
        self.assertEqual(mapped[4].code_offset, 4)
        self.assertTrue(all(item.container_tid == 12 for item in mapped))

    def test_reads_namespace_tsv_without_using_comm_as_identity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tid.tsv"
            path.write_text(
                "monotonic_ns\thost_tid\tcontainer_tid\tnspid_chain\tcomm\n"
                "100\t2612336\t12\t2612336,12\tqemu-system-ris \n"
                "100\t2612337\t13\t2612337,13\tCPU 1/TCG \n",
                encoding="utf-8",
            )

            mapping = read_tid_namespace_tsv(path)

            self.assertEqual(mapping.by_host_tid()[2612336].container_tid, 12)
            self.assertEqual(mapping.by_container_tid()[13].host_tid, 2612337)
            self.assertEqual(mapping.entries[0].comm, "qemu-system-ris")


class EndToEndSummaryTests(unittest.TestCase):
    def test_summary_keeps_native_samples_out_of_translation_failure_rate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            samples = root / "samples.bin"
            jitdump = root / "jit.dump"
            catalog = root / "catalog.jsonl"
            namespace = root / "tid.tsv"
            samples.write_bytes(
                rv_header()
                + rv_record(1, struct.pack("<QQQIIII", 0x5000, 30, 2, 7, 101, 0, 0))
                + rv_record(1, struct.pack("<QQQIIII", 0x1001, 31, 3, 7, 100, 0, 0))
            )
            jitdump.write_bytes(
                jit_header()
                + jit_load(20, tid=12, guest_pc=0x8000, address=0x1000, index=1)
            )
            write_jsonl(catalog, [catalog_tb(10, tid=12, guest_pc=0x8000, index=1)])
            namespace.write_text(
                "monotonic_ns\thost_tid\tcontainer_tid\tnspid_chain\tcomm\n"
                "1\t100\t12\t100,12\tCPU 0/TCG\n"
                "1\t101\t20\t101,20\tworker\n",
                encoding="utf-8",
            )

            summary = profile_quality_summary(
                samples, jitdump, catalog, tid_namespace_path=namespace
            )

            self.assertEqual(summary["samples"]["counts"]["native-qemu"], 1)
            self.assertEqual(summary["samples"]["counts"]["mapped-to-tcg"], 1)
            self.assertEqual(summary["translation_match"]["matched"], 1)
            self.assertEqual(summary["translation_match"]["guest_jit_match_ratio"], 1.0)
            self.assertEqual(summary["vcpu_samples"]["total"], 1)
            self.assertEqual(summary["vcpu_samples"]["host_tids"], [100])
            self.assertEqual(summary["vcpu_samples"]["counts"]["mapped-to-tcg"], 1)


if __name__ == "__main__":
    unittest.main()
