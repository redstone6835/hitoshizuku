#!/usr/bin/env python3

import importlib.util
import struct
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "profile_snapshot_analyze", ROOT / "scripts/profile-snapshot-analyze.py"
)
assert SPEC and SPEC.loader
ANALYZER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYZER)


def put(record: bytearray, offset: int, fmt: str, value: int) -> None:
    struct.pack_into("<" + fmt, record, offset, value)


def fixture(version: int = 3) -> bytes:
    header_size = 320 if version == 3 else 256
    sections = []

    event = bytearray(608)
    put(event, 2, "H", 58)
    put(event, 8, "Q", 3)
    put(event, 48, "Q", 1_500_000)
    put(event, 56, "Q", 1_000_000)
    put(event, 64, "Q", 500_000)
    sections.append((1, event))

    sections.append((2, bytearray(544)))

    syscall = bytearray(624)
    put(syscall, 0, "H", 4)
    put(syscall, 2, "H", 56)
    put(syscall, 8, "Q", 3)
    put(syscall, 16, "Q", 1)
    put(syscall, 24, "Q", 4)
    put(syscall, 64, "Q", 2_000_000)
    put(syscall, 72, "Q", 1_500_000)
    put(syscall, 80, "Q", 500_000)
    put(syscall, 24 + 80 + 10 * 8, "Q", 4)
    sections.append((3, syscall))

    errno = bytearray(32)
    put(errno, 0, "H", 4)
    put(errno, 2, "H", 56)
    put(errno, 4, "I", 2)
    put(errno, 8, "Q", 1)
    sections.append((4, errno))

    task = bytearray(128)
    put(task, 0, "Q", 1)
    put(task, 8, "I", 7)
    put(task, 12, "I", 7)
    put(task, 16, "I", 1)
    put(task, 24, "Q", 3_000_000)
    sections.append((5, task))

    sample = bytearray(40)
    put(sample, 8, "Q", 0x80200000)
    put(sample, 32, "Q", 10)
    sections.append((6, sample))
    sections.append((7, bytearray(80)))

    header = bytearray(header_size)
    header[:8] = b"MYGOPRF\0"
    put(header, 8, "H", version)
    put(header, 10, "H", header_size)
    put(header, 12, "I", 0x01020304)
    put(header, 24, "Q", 1)
    put(header, 32, "Q", 2)
    put(header, 40, "Q", 1_000_000_000)
    put(header, 48, "Q", 0xFFFFFFFFFFFFFFFF)
    put(header, 56, "I", 5)
    put(header, 60, "H", 8)
    put(header, 62, "H", 7)
    if version == 3:
        put(header, 232, "Q", 0x1FF)
        put(header, 240, "Q", 7)
        put(header, 248, "Q", 250)
    else:
        put(header, 224, "Q", 0x1FF)
        put(header, 232, "Q", 7)
        put(header, 240, "Q", 250)
    offset = len(header)
    for index, (kind, record) in enumerate(sections):
        base = 64 + index * 24
        put(header, base, "H", kind)
        put(header, base + 2, "H", len(record))
        put(header, base + 8, "Q", offset)
        put(header, base + 16, "Q", 1)
        offset += len(record)
    if version == 2:
        # 旧格式会以扩展字段覆盖最后一个目录项的 count；分析器必须恢复它。
        put(header, 224, "Q", 0x1FF)
    put(header, 16, "Q", offset)
    return bytes(header) + b"".join(bytes(record) for _, record in sections)


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        snapshot = root / "profile.bin"
        snapshot.write_bytes(fixture())
        profile = ANALYZER.parse_snapshot(snapshot)
        assert ANALYZER.disk_path_identities("/musl/tool", ["/mnt"]) == [
            "/musl/tool", "/mnt/musl/tool"
        ]
        assert profile["header"]["sample_hz"] == 250
        assert profile["header"]["complete"]
        assert ANALYZER.EVENT_NAMES[49:58] == [
            "wait_process_exit", "wait_vfork", "wait_block_io", "page_fault_resident",
            "page_fault_prepare", "page_fault_commit", "page_fault_single",
            "page_fault_cache_fill", "page_fault_uncached_fill",
        ]
        assert ANALYZER.EVENT_NAMES[64] == "mm_protect"
        assert ANALYZER.EVENT_NAMES[73:81] == [
            "urgent_spin_check", "urgent_pending_hit", "urgent_service",
            "slab_cache_hit", "slab_cache_miss", "slab_refill", "slab_flush",
            "slab_slow_path",
        ]
        assert ANALYZER.EVENT_NAMES[81] == "mm_protect_noop"
        assert ANALYZER.EVENT_NAMES[82] == "mm_protect_batch"
        assert len(ANALYZER.EVENT_NAMES) == 83
        assert profile["events"][0]["name"] == "vfs_lookup"
        assert profile["syscalls"][0]["phase"] == 4
        assert profile["samples"][0]["samples"] == 10
        output = root / "report"
        tcg = root / "tcg.txt"
        tcg.write_text(
            "MYGO_TCG_PROFILE version=2 target=riscv64 configured_vcpus=2 active_vcpus=1 "
            "table_bits=18 table_slots=262144 table_probes=128 "
            "counter_bytes_per_vcpu=4194320 translated_blocks=1 occupied_slots=1 dropped=0 "
            "collision_probes=1 max_probe=1 total_blocks=2 total_instructions=8 "
            "reported_hotspots=1\n"
            "VCPU cpu=0 blocks=2 instructions=8\n"
            "HOT rank=1 pc=0x80200000 blocks=2 instructions=8\n"
        )
        phases = root / "phases.tsv"
        phases.write_text("4\texecute\t^run workload\n")
        tcg_profile = ANALYZER.parse_tcg_profile(tcg)
        ANALYZER.add_symbols(
            profile, None, None, None, root / "images", "addr2line", tcg_profile
        )
        ANALYZER.write_reports(
            profile,
            output,
            ANALYZER.parse_syscall_names(ROOT / "kernel/src/syscalls/nr.rs"),
            [],
            tcg_profile,
            ANALYZER.parse_phase_names(phases),
        )
        assert "4\texecute\t56\topenat" in (output / "syscalls.tsv").read_text()
        assert "vfs_lookup" in (output / "events.tsv").read_text()
        report = (output / "report.md").read_text()
        assert "QEMU TCG guest-PC hotspots" in report
        assert "kernel_or_firmware" in report
        damaged = bytearray(snapshot.read_bytes())
        put(damaged, 256, "Q", 1)
        snapshot.write_bytes(damaged)
        incomplete = ANALYZER.parse_snapshot(snapshot)
        assert not incomplete["header"]["complete"]
        assert not incomplete["header"]["section_complete"]["samples"]
        snapshot.write_bytes(fixture(2))
        legacy = ANALYZER.parse_snapshot(snapshot)
        assert legacy["header"]["version"] == 2
        assert legacy["header"]["event_mask_high"] == 0x1FF
        tcg.write_text(tcg.read_text().replace("reported_hotspots=1", "reported_hotspots=2"))
        try:
            ANALYZER.parse_tcg_profile(tcg)
        except ANALYZER.SnapshotError:
            pass
        else:
            raise AssertionError("TCG report with mismatched hotspot count was accepted")
        tcg.write_text(tcg.read_text().replace("reported_hotspots=2", "reported_hotspots=1").replace(
            "dropped=0", "dropped=1"
        ))
        assert not ANALYZER.parse_tcg_profile(tcg)["complete"]
    print("profile-snapshot-analyze fixture: ok")


if __name__ == "__main__":
    main()
