"""QEMU profiling daemon 的协议与控制面测试。"""

from __future__ import annotations

import argparse
import dataclasses
import errno
import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from scripts.qemu_profile_daemon import (
    CaptureState,
    PLUGIN_FLAG_KERNEL,
    PLUGIN_FLAG_REGISTERS_VALID,
    PLUGIN_FLAG_STACK_VALID,
    PLUGIN_HEADER,
    PLUGIN_MAGIC,
    PLUGIN_VERSION,
    SCHEMA,
    PluginRecord,
    ProcStat,
    ProfileError,
    ProfileDaemon,
    QemuProcessIdentity,
    QmpClient,
    SerialTimeline,
    Symbol,
    SymbolTable,
    VcpuThread,
    assess_vcpu_threads,
    build_parser,
    control_request,
    parse_gdb_backtrace,
    parse_proc_stat,
    plugin_frames,
    load_kernel_map_manifest,
    load_plugin_exit_summary,
    read_qemu_process_identity,
    reconcile_plugin_exit,
    validate_capture_args,
    validate_plugin_record_progress,
)


class ParserTests(unittest.TestCase):
    """验证宿主输入不会被静默误解。"""

    def test_proc_stat_handles_parentheses_in_comm(self) -> None:
        fields = ["S", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12"]
        fields.extend(["13", "14", "15", "16", "17", "18", "19", "20", "21"])
        value = parse_proc_stat("42 (qemu (worker)) " + " ".join(fields))
        self.assertEqual(value.pid, 42)
        self.assertEqual((value.utime_ticks, value.stime_ticks), (11, 12))
        self.assertEqual(value.start_ticks, 19)

    def test_qemu_identity_falls_back_on_proc_exe_eacces(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            process = Path(directory) / "42"
            process.mkdir()
            (process / "comm").write_text("qemu-system-loo\n")
            cmdline = b"/opt/qemu/bin/qemu-system-loongarch64\0-machine\0virt\0"
            (process / "cmdline").write_bytes(cmdline)
            denied = PermissionError(errno.EACCES, "permission denied")
            with mock.patch("scripts.qemu_profile_daemon.os.readlink", side_effect=denied):
                identity = read_qemu_process_identity(42, Path(directory))

        self.assertEqual(identity.method, "proc-comm-cmdline")
        self.assertEqual(identity.comm, "qemu-system-loo")
        self.assertEqual(identity.argv0, "/opt/qemu/bin/qemu-system-loongarch64")
        self.assertIsNone(identity.device)
        self.assertIsNone(identity.inode)

    def test_qemu_identity_accepts_the_configured_riscv_binary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            process = Path(directory) / "42"
            process.mkdir()
            (process / "comm").write_text("qemu-system-ris\n")
            cmdline = b"/opt/qemu/bin/qemu-system-riscv64\0-machine\0virt\0"
            (process / "cmdline").write_bytes(cmdline)
            denied = PermissionError(errno.EACCES, "permission denied")
            with mock.patch("scripts.qemu_profile_daemon.os.readlink", side_effect=denied):
                identity = read_qemu_process_identity(
                    42,
                    Path(directory),
                    "qemu-system-riscv64",
                )

        self.assertEqual(identity.method, "proc-comm-cmdline")
        self.assertEqual(identity.comm, "qemu-system-ris")
        self.assertEqual(identity.argv0, "/opt/qemu/bin/qemu-system-riscv64")

    def test_qemu_fallback_rejects_non_qemu_cmdline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            process = Path(directory) / "42"
            process.mkdir()
            (process / "comm").write_text("qemu-system-loo\n")
            (process / "cmdline").write_bytes(b"/usr/bin/sleep\0" b"10\0")
            denied = PermissionError(errno.EACCES, "permission denied")
            with mock.patch("scripts.qemu_profile_daemon.os.readlink", side_effect=denied):
                with self.assertRaisesRegex(ProfileError, "comm/cmdline is not"):
                    read_qemu_process_identity(42, Path(directory))

    def test_qemu_fallback_identity_and_start_ticks_are_rechecked(self) -> None:
        daemon = object.__new__(ProfileDaemon)
        daemon.args = SimpleNamespace(qemu_pid=42)
        daemon.qemu_identity = ProcStat(42, "R", 0, 0, 100, 0, 0)
        daemon.qemu_process_identity = QemuProcessIdentity(
            "proc-comm-cmdline",
            "/opt/qemu-system-loongarch64",
            None,
            None,
            "qemu-system-loo",
            "/opt/qemu-system-loongarch64",
            "original",
        )
        daemon._read_proc_stat = lambda _pid: daemon.qemu_identity  # type: ignore[method-assign]
        changed = QemuProcessIdentity(
            "proc-comm-cmdline",
            "/opt/qemu-system-loongarch64",
            None,
            None,
            "qemu-system-loo",
            "/opt/qemu-system-loongarch64",
            "changed",
        )
        with mock.patch(
            "scripts.qemu_profile_daemon.read_qemu_fallback_identity",
            return_value=changed,
        ):
            with self.assertRaisesRegex(ProfileError, "fallback process identity changed"):
                daemon._verify_qemu_identity()

        daemon._read_proc_stat = lambda _pid: ProcStat(42, "R", 0, 0, 101, 0, 0)  # type: ignore[method-assign]
        with self.assertRaisesRegex(ProfileError, "pid was reused"):
            daemon._verify_qemu_identity()

    def test_serial_timeline_splits_carriage_returns_and_deduplicates_progress(self) -> None:
        timeline = SerialTimeline()
        events = timeline.feed(
            b"\x1b[92m Compiling foo v1.0\x1b[0m\r[64/446]\r[64/446]\r[65/446]\n"
            b"@@PROFILE_STAGE name=aws token=p1\n",
            123,
        )
        self.assertEqual(
            [(event.kind, event.name) for event in events],
            [
                ("cargo_compile", "compile:foo"),
                ("cargo_progress", "cargo:64"),
                ("cargo_progress", "cargo:65"),
                ("marker", "PROFILE_STAGE:aws"),
            ],
        )

    def test_gdb_backtrace_preserves_each_vcpu(self) -> None:
        traces = parse_gdb_backtrace(
            "Thread 2 (Thread 1.2 (CPU#1 [running])):\n"
            "#0  0x90000010 in idle_loop () at idle.rs:1\n"
            "#1  schedule ()\n"
            "Thread 1 (Thread 1.1 (CPU#0 [running])):\n"
            "#0  0x90000020 in ?? ()\n"
        )
        self.assertEqual([(trace.cpu, len(trace.frames)) for trace in traces], [(1, 2), (0, 1)])
        self.assertEqual(traces[0].frames[0].function, "idle_loop")
        self.assertFalse(traces[1].frames[0].symbolized)


class SerialWriterTests(unittest.TestCase):
    """验证交互串口命令不会以整行突发方式写入。"""

    def test_serial_line_is_paced_one_byte_at_a_time(self) -> None:
        try:
            from scripts.serial_line_writer import write_serial_line
        except ImportError as error:
            self.fail(f"serial line writer is unavailable: {error}")

        class RecordingStream:
            def __init__(self) -> None:
                self.chunks: list[bytes] = []

            def write(self, chunk: bytes) -> int:
                self.chunks.append(chunk)
                return len(chunk)

        stream = RecordingStream()
        delays: list[float] = []
        write_serial_line(stream, "ab", delay_seconds=0.002, sleep=delays.append)

        self.assertEqual(stream.chunks, [b"a", b"b", b"\n"])
        self.assertEqual(delays, [0.002, 0.002, 0.002])


class SymbolTests(unittest.TestCase):
    """验证两种内核符号图与 guess unwind 的共同口径。"""

    def test_loads_lld_map(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "kernel.map"
            path.write_text(
                "VMA LMA Size Align Out In Symbol\n"
                "900000001000 900000001000 0 1 stext = .\n"
                "900000001000 900000001000 20 1 first::function\n"
                "900000001020 900000001020 20 1 second::function\n"
                "900000001040 900000001040 0 1 etext = .\n"
            )
            symbols = SymbolTable.load(path)
        resolved = symbols.lookup(0x900000001024)
        self.assertIsNotNone(resolved)
        assert resolved is not None
        self.assertEqual((resolved[0].name, resolved[1]), ("second::function", 4))

    def test_loads_linux_system_map(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "System.map"
            path.write_text(
                "900000001000 T _stext\n"
                "900000001010 t do_work\n"
                "900000001100 T _etext\n"
                "900000001110 D core_data\n"
                "900000002000 t init_only_work\n"
                "900000002020 d init_data\n"
            )
            symbols = SymbolTable.load(path)
        self.assertEqual(symbols.lookup(0x900000001014)[0].name, "do_work")  # type: ignore[index]
        self.assertEqual(
            symbols.lookup(0x900000002004)[0].name,  # type: ignore[index]
            "init_only_work",
        )
        self.assertIsNone(symbols.lookup(0x900000001800))

    def test_vcpu_snapshot_requires_complete_stable_unique_threads(self) -> None:
        threads = [
            VcpuThread(0, 100, 10, "R", 1, 2),
            VcpuThread(1, 101, 11, "S", 3, 4),
        ]
        complete, errors = assess_vcpu_threads(threads, 2)
        self.assertEqual(errors, ())
        expected = {cpu: thread.identity for cpu, thread in complete.items()}

        _snapshot, errors = assess_vcpu_threads(
            [threads[0], VcpuThread(0, 102, 12, "R", 0, 0)],
            2,
            expected,
        )
        self.assertTrue(any("duplicate vCPU 0" in error for error in errors))
        self.assertTrue(any("missing vCPUs 1" in error for error in errors))

        _snapshot, errors = assess_vcpu_threads(
            [threads[0], VcpuThread(1, 999, 99, "R", 0, 0)],
            2,
            expected,
        )
        self.assertTrue(any("vCPU 1 identity changed" in error for error in errors))

    def test_capture_parser_uses_one_plugin_period(self) -> None:
        args = build_parser().parse_args(
            [
                "capture",
                "--qemu-pid",
                "1",
                "--serial-log",
                "serial.log",
                "--output",
                "events.jsonl",
                "--summary",
                "summary.json",
                "--control-socket",
                "control.sock",
                "--system",
                "test",
                "--workload",
                "fixture",
                "--vcpu-count",
                "1",
                "--plugin-period-insns",
                "1234",
            ]
        )
        self.assertEqual(args.plugin_period_insns, 1234)
        self.assertFalse(hasattr(args, "plugin_counter_period_insns"))

    def test_counter_error_bound_uses_the_single_plugin_period(self) -> None:
        daemon = object.__new__(ProfileDaemon)
        daemon.args = SimpleNamespace(
            stack_interval_ms=0,
            plugin_socket=Path("plugin.sock"),
            vcpu_count=2,
            max_pause_ratio=0.05,
            plugin_period_insns=1234,
        )
        daemon.clock_ticks = 100
        daemon._metadata = lambda: {}  # type: ignore[method-assign]
        first = PluginRecord(0, 0, 1, 0, 100, 60, 40, 0, 0, 0, 0, 0, 0, 0, b"")
        last = PluginRecord(0, 0, 2, 0, 200, 120, 80, 0, 0, 0, 0, 0, 0, 0, b"")
        capture = CaptureState("unit", 1, 0)
        capture.proc_samples = 2
        capture.vcpu_thread_preflight_valid = True
        capture.vcpu_thread_complete_samples = 2
        capture.vcpu_thread_identity = {0: (10, 20), 1: (11, 21)}
        capture.plugin_samples = 1
        capture.plugin_top_symbolized = 1
        capture.plugin_first = {0: first}
        capture.plugin_last = {0: last}
        summary = daemon._build_summary(capture, 101, 10, "unit")
        self.assertFalse(summary["quality"]["valid"])
        self.assertTrue(summary["quality"]["plugin_preliminary_valid"])
        self.assertFalse(summary["quality"]["plugin_exit_reconciled"])
        self.assertEqual(summary["quality"]["plugin_observed_vcpus"], [0])
        self.assertEqual(summary["quality"]["plugin_unobserved_vcpus"], [1])
        self.assertEqual(summary["guest_instructions"]["counter_error_bound_insns"], 4936)

    def test_timeline_summary_does_not_wait_for_plugin_reconciliation(self) -> None:
        daemon = object.__new__(ProfileDaemon)
        daemon.args = SimpleNamespace(
            stack_interval_ms=0,
            plugin_socket=None,
            vcpu_count=1,
            max_pause_ratio=0.05,
            plugin_period_insns=1234,
        )
        daemon.clock_ticks = 100
        daemon._metadata = lambda: {}  # type: ignore[method-assign]
        capture = CaptureState("unit", 1, 0)
        capture.proc_samples = 2
        capture.vcpu_thread_preflight_valid = True
        capture.vcpu_thread_complete_samples = 2
        capture.vcpu_thread_identity = {0: (10, 20)}

        summary = daemon._build_summary(capture, 101, 10, "unit")

        self.assertTrue(summary["quality"]["valid"])
        self.assertTrue(summary["quality"]["plugin_preliminary_valid"])
        self.assertTrue(summary["quality"]["plugin_exit_reconciled"])
        self.assertIsNone(summary["quality"]["plugin_exit_reconciliation_error"])

    def test_plugin_record_and_guess_stack_keep_recursion(self) -> None:
        stack = struct.pack("<QQQ", 0xDEADBEEF, 0x1104, 0x1104)
        payload = PLUGIN_HEADER.pack(
            PLUGIN_MAGIC,
            PLUGIN_VERSION,
            PLUGIN_HEADER.size,
            PLUGIN_HEADER.size + len(stack),
            3,
            PLUGIN_FLAG_KERNEL | PLUGIN_FLAG_REGISTERS_VALID | PLUGIN_FLAG_STACK_VALID,
            7,
            100,
            1000,
            600,
            400,
            0,
            0x1008,
            0x8000,
            0x1204,
            0x9000,
            0xA000,
            0xB000,
            len(stack),
            0,
        ) + stack
        record = PluginRecord.parse(payload)
        symbols = SymbolTable([Symbol(0x1000, "leaf"), Symbol(0x1100, "recursive")], 0x1000, 0x2000)
        frames = plugin_frames(record, symbols, 8)
        self.assertEqual(record.vcpu, 3)
        self.assertEqual([frame.function for frame in frames], ["leaf", "recursive", "recursive"])
        self.assertEqual(frames[1].raw, "sp+0x8 recursive+0x0")

    def test_plugin_record_rejects_truncated_datagram(self) -> None:
        with self.assertRaisesRegex(ValueError, "short plugin record"):
            PluginRecord.parse(b"short")

    def test_plugin_record_rejects_inconsistent_instruction_counters(self) -> None:
        payload = PLUGIN_HEADER.pack(
            PLUGIN_MAGIC,
            PLUGIN_VERSION,
            PLUGIN_HEADER.size,
            PLUGIN_HEADER.size,
            0,
            0,
            1,
            100,
            1001,
            600,
            400,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        )
        with self.assertRaisesRegex(ValueError, "instruction counters do not add up"):
            PluginRecord.parse(payload)

    def test_plugin_record_progress_rejects_regressions(self) -> None:
        previous = PluginRecord(0, 0, 10, 100, 100, 60, 40, 1, 0, 0, 0, 0, 0, 0, b"")
        current = PluginRecord(0, 0, 11, 101, 110, 65, 45, 1, 0, 0, 0, 0, 0, 0, b"")
        validate_plugin_record_progress(previous, current)
        regressions = {
            "sequence": dataclasses.replace(current, sequence=10),
            "monotonic_ns": dataclasses.replace(current, monotonic_ns=99),
            "total_insns": dataclasses.replace(
                current, total_insns=99, user_insns=59, kernel_insns=40
            ),
            "user_insns": dataclasses.replace(current, user_insns=59, kernel_insns=51),
            "kernel_insns": dataclasses.replace(current, user_insns=71, kernel_insns=39),
            "dropped": dataclasses.replace(current, dropped=0),
        }
        for field, record in regressions.items():
            with self.subTest(field=field):
                with self.assertRaisesRegex(ValueError, field):
                    validate_plugin_record_progress(previous, record)

    def test_unsymbolized_plugin_sample_is_not_an_invalid_record(self) -> None:
        payload = PLUGIN_HEADER.pack(
            PLUGIN_MAGIC,
            PLUGIN_VERSION,
            PLUGIN_HEADER.size,
            PLUGIN_HEADER.size,
            0,
            PLUGIN_FLAG_REGISTERS_VALID | PLUGIN_FLAG_STACK_VALID,
            1,
            100,
            100,
            100,
            0,
            0,
            0x4000,
            0x8000,
            0,
            0,
            0,
            0,
            0,
            0,
        )

        class OneRecordSocket:
            def __init__(self) -> None:
                self.payload = payload

            def recv(self, _size: int) -> bytes:
                if self.payload is None:
                    raise BlockingIOError
                result = self.payload
                self.payload = None
                return result

        capture = CaptureState("unit", 1, 0)
        daemon = object.__new__(ProfileDaemon)
        daemon.args = SimpleNamespace(plugin_stack_bytes=0, vcpu_count=1, max_frames=8)
        daemon.plugin_socket = OneRecordSocket()
        daemon.plugin_latest = {}
        daemon.capture = capture
        daemon.clock = lambda: 200
        daemon.symbol_table = SymbolTable([Symbol(0x1000, "kernel")], 0x1000, 0x2000)
        daemon.writer = SimpleNamespace(write=lambda *_args, **_kwargs: None)

        daemon._drain_plugin()

        self.assertEqual(capture.plugin_invalid, 0)
        self.assertEqual(getattr(capture, "plugin_unsymbolized", 0), 1)


class PluginExitTests(unittest.TestCase):
    """验证 preliminary summary 只能由完整 atexit 累计量解锁。"""

    @staticmethod
    def exit_value(dropped: int = 0) -> dict[str, object]:
        return {
            "schema": "mygo.qemu-observer-plugin.v1",
            "counter_granularity": "translation-block",
            "period_insns": 100,
            "stack_bytes": 16,
            "vcpus": [
                {
                    "cpu": 0,
                    "total": 250,
                    "user": 150,
                    "kernel": 100,
                    "samples": 2,
                    "dropped": dropped,
                },
                {
                    "cpu": 1,
                    "total": 50,
                    "user": 20,
                    "kernel": 30,
                    "samples": 0,
                    "dropped": 0,
                },
            ],
        }

    @staticmethod
    def latest() -> dict[int, PluginRecord]:
        return {
            0: PluginRecord(0, 0, 2, 100, 200, 120, 80, 0, 0, 0, 0, 0, 0, 0, b"")
        }

    def test_exit_summary_parser_and_reconciliation_are_strict(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "plugin-summary.json"
            path.write_text(json.dumps(self.exit_value()))
            summary = load_plugin_exit_summary(path, 100, 16, 2)
        self.assertEqual(summary.counter_granularity, "translation-block")
        reconcile_plugin_exit(summary, self.latest())

        no_sample_at_period = dataclasses.replace(
            summary,
            vcpus=(
                summary.vcpus[0],
                dataclasses.replace(summary.vcpus[1], total=100, user=40, kernel=60),
            ),
        )
        with self.assertRaisesRegex(ProfileError, "no samples but reached one period"):
            reconcile_plugin_exit(no_sample_at_period, self.latest())

        counter_regression = dataclasses.replace(
            summary,
            vcpus=(
                dataclasses.replace(summary.vcpus[0], total=199, user=119, kernel=80),
                summary.vcpus[1],
            ),
        )
        with self.assertRaisesRegex(ProfileError, "exit total counter regressed"):
            reconcile_plugin_exit(counter_regression, self.latest())

        period_distance = dataclasses.replace(
            summary,
            vcpus=(
                dataclasses.replace(summary.vcpus[0], total=300, user=180, kernel=120),
                summary.vcpus[1],
            ),
        )
        with self.assertRaisesRegex(ProfileError, "distance reached one period"):
            reconcile_plugin_exit(period_distance, self.latest())

    def test_exit_summary_rejects_malformed_vcpu_rows(self) -> None:
        cases = {
            "cpu ids": self.exit_value(),
            "counters": self.exit_value(),
            "integer": self.exit_value(),
        }
        cases["cpu ids"]["vcpus"][1]["cpu"] = 0  # type: ignore[index]
        cases["counters"]["vcpus"][0]["total"] = 251  # type: ignore[index]
        cases["integer"]["vcpus"][0]["samples"] = True  # type: ignore[index]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "plugin-summary.json"
            for label, value in cases.items():
                with self.subTest(label=label):
                    path.write_text(json.dumps(value))
                    with self.assertRaises(ProfileError):
                        load_plugin_exit_summary(path, 100, 16, 2)

    def make_daemon(self, root: Path) -> ProfileDaemon:
        class EmptySocket:
            def recv(self, _size: int) -> bytes:
                raise BlockingIOError

            def close(self) -> None:
                pass

        daemon = object.__new__(ProfileDaemon)
        daemon.args = SimpleNamespace(
            plugin_summary=root / "plugin-summary.json",
            plugin_period_insns=100,
            plugin_stack_bytes=16,
            vcpu_count=2,
            summary=root / "summary.json",
        )
        daemon.plugin_socket = EmptySocket()
        daemon.plugin_latest = self.latest()
        daemon.plugin_exit_reconciliation_attempted = False
        daemon.capture = None
        daemon.clock = lambda: 123
        daemon.writer = SimpleNamespace(write=lambda *_args, **_kwargs: None)
        daemon.args.summary.write_text(
            json.dumps(
                {
                    "schema": SCHEMA,
                    "quality": {
                        "valid": False,
                        "plugin_preliminary_valid": True,
                        "plugin_exit_reconciled": False,
                    },
                }
            )
        )
        return daemon

    @staticmethod
    def prepare_shutdown(daemon: ProfileDaemon, root: Path) -> None:
        daemon.args.plugin_socket = root / "plugin.sock"
        daemon.args.control_socket = root / "control.sock"
        daemon.args.ready_file = None
        daemon.completed = True
        daemon.running = True
        daemon.qmp = None
        daemon.server = None
        daemon.selector = mock.Mock()
        daemon.writer = mock.Mock()

    def test_atomic_exit_reconciliation_unlocks_preliminary_summary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = self.make_daemon(root)
            daemon.args.plugin_summary.write_text(json.dumps(self.exit_value()))
            daemon._reconcile_plugin_exit_summary()
            summary = json.loads(daemon.args.summary.read_text())
        self.assertTrue(summary["quality"]["valid"])
        self.assertTrue(summary["quality"]["plugin_exit_reconciled"])
        self.assertIsNone(summary["quality"]["plugin_exit_reconciliation_error"])
        self.assertEqual(summary["quality"]["plugin_exit_counts"]["0"]["samples"], 2)

    def test_dropped_or_missing_exit_summary_forces_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = self.make_daemon(root)
            daemon.args.plugin_summary.write_text(json.dumps(self.exit_value(dropped=1)))
            daemon._reconcile_plugin_exit_summary()
            dropped = json.loads(daemon.args.summary.read_text())

            missing = self.make_daemon(root)
            missing.args.plugin_summary.unlink()
            missing._reconcile_plugin_exit_summary()
            absent = json.loads(missing.args.summary.read_text())

        self.assertFalse(dropped["quality"]["valid"])
        self.assertIn("dropped datagrams", dropped["quality"]["plugin_exit_reconciliation_error"])
        self.assertFalse(absent["quality"]["valid"])
        self.assertIn("cannot read plugin exit summary", absent["quality"]["plugin_exit_reconciliation_error"])

    def test_control_shutdown_reconciles_once(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = self.make_daemon(root)
            self.prepare_shutdown(daemon, root)
            daemon.args.plugin_summary.write_text(json.dumps(self.exit_value()))

            response = daemon._control({"command": "shutdown"})
            summary = json.loads(daemon.args.summary.read_text())
            daemon.plugin_socket = None
            daemon._reconcile_plugin_exit_summary = mock.Mock()  # type: ignore[method-assign]
            daemon._shutdown()

        self.assertTrue(response["ok"])
        self.assertFalse(daemon.running)
        self.assertTrue(summary["quality"]["valid"])
        daemon._reconcile_plugin_exit_summary.assert_not_called()

    def test_shutdown_fallback_reconciles_completed_capture(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            daemon = self.make_daemon(root)
            self.prepare_shutdown(daemon, root)
            daemon.args.plugin_summary.write_text(json.dumps(self.exit_value()))

            daemon._shutdown()
            summary = json.loads(daemon.args.summary.read_text())

        self.assertTrue(daemon.plugin_exit_reconciliation_attempted)
        self.assertTrue(summary["quality"]["valid"])
        self.assertTrue(summary["quality"]["plugin_exit_reconciled"])


class LifecycleTests(unittest.TestCase):
    """验证异常停止 capture 时仍执行 daemon 资源清理。"""

    def test_serve_shutdown_runs_when_implicit_stop_fails(self) -> None:
        daemon = object.__new__(ProfileDaemon)
        daemon.running = False
        daemon.capture = object()
        daemon.setup = mock.Mock()  # type: ignore[method-assign]
        daemon.stop_capture = mock.Mock(  # type: ignore[method-assign]
            side_effect=ProfileError("QEMU already exited")
        )
        daemon._shutdown = mock.Mock()  # type: ignore[method-assign]

        with self.assertRaisesRegex(ProfileError, "already exited"):
            daemon.serve()

        daemon._shutdown.assert_called_once_with()


class ManifestTests(unittest.TestCase):
    """验证 daemon 自己绑定 production kernel 与同链接 map。"""

    @staticmethod
    def capture_args(root: Path, *extra: str) -> argparse.Namespace:
        return build_parser().parse_args(
            [
                "capture",
                "--qemu-pid",
                "1",
                "--serial-log",
                str(root / "serial.log"),
                "--output",
                str(root / "events.jsonl"),
                "--summary",
                str(root / "summary.json"),
                "--control-socket",
                str(root / "control.sock"),
                "--system",
                "test",
                "--workload",
                "fixture",
                "--vcpu-count",
                "1",
                *extra,
            ]
        )

    def test_kernel_map_manifest_is_parsed_and_verified(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel-la"
            symbol_map = root / "kernel.map"
            manifest = root / "kernel.map.manifest"
            kernel.write_bytes(b"kernel")
            symbol_map.write_bytes(b"map")
            manifest.write_text(
                "schema=mygo.kernel-map-manifest.v1\n"
                "target=loongarch64-unknown-none\n"
                f"kernel_sha256={hashlib.sha256(b'kernel').hexdigest()}\n"
                f"symbol_map_sha256={hashlib.sha256(b'map').hexdigest()}\n"
            )
            manifest_sha256 = hashlib.sha256(manifest.read_bytes()).hexdigest()
            parsed = load_kernel_map_manifest(manifest)
            args = self.capture_args(
                root,
                "--symbol-map",
                str(symbol_map),
                "--kernel-image",
                str(kernel),
                "--symbol-manifest",
                str(manifest),
            )
            validate_capture_args(args)

        self.assertEqual(parsed.target, "loongarch64-unknown-none")
        self.assertEqual(args.kernel_sha256, hashlib.sha256(b"kernel").hexdigest())
        self.assertEqual(args.symbol_manifest_target, "loongarch64-unknown-none")
        self.assertEqual(args.symbol_manifest_sha256, manifest_sha256)

    def test_kernel_map_manifest_rejects_bad_kernel_hash(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel-la"
            symbol_map = root / "kernel.map"
            manifest = root / "kernel.map.manifest"
            kernel.write_bytes(b"kernel")
            symbol_map.write_bytes(b"map")
            manifest.write_text(
                "schema=mygo.kernel-map-manifest.v1\n"
                "target=loongarch64-unknown-none\n"
                f"kernel_sha256={'0' * 64}\n"
                f"symbol_map_sha256={hashlib.sha256(b'map').hexdigest()}\n"
            )
            args = self.capture_args(
                root,
                "--symbol-map",
                str(symbol_map),
                "--kernel-image",
                str(kernel),
                "--symbol-manifest",
                str(manifest),
            )
            with self.assertRaisesRegex(ProfileError, "kernel image hash"):
                validate_capture_args(args)

    def test_kernel_image_and_manifest_must_be_paired(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel-la"
            kernel.write_bytes(b"kernel")
            args = self.capture_args(root, "--kernel-image", str(kernel))
            with self.assertRaisesRegex(ProfileError, "must be provided together"):
                validate_capture_args(args)

    def test_plugin_mode_requires_a_fresh_exit_summary_path(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            symbol_map = root / "kernel.map"
            symbol_map.write_text("map")
            args = self.capture_args(
                root,
                "--plugin-socket",
                str(root / "plugin.sock"),
                "--symbol-map",
                str(symbol_map),
            )
            with self.assertRaisesRegex(ProfileError, "requires --plugin-summary"):
                validate_capture_args(args)

            plugin_summary = root / "plugin-summary.json"
            plugin_summary.write_text("stale")
            args = self.capture_args(
                root,
                "--plugin-socket",
                str(root / "plugin.sock"),
                "--plugin-summary",
                str(plugin_summary),
                "--symbol-map",
                str(symbol_map),
            )
            with self.assertRaisesRegex(ProfileError, "already exists"):
                validate_capture_args(args)


class QmpTests(unittest.TestCase):
    """验证异步 event 不会破坏 QMP 请求 ID 关联。"""

    def test_qmp_client_ignores_events_before_response(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "qmp.sock"
            ready = threading.Event()

            def server() -> None:
                listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                listener.bind(str(path))
                listener.listen(1)
                ready.set()
                connection, _ = listener.accept()
                with connection, connection.makefile("rwb", buffering=0) as stream:
                    stream.write(b'{"QMP":{"version":{},"capabilities":[]}}\n')
                    capabilities = json.loads(stream.readline())
                    stream.write(json.dumps({"return": {}, "id": capabilities["id"]}).encode() + b"\n")
                    status = json.loads(stream.readline())
                    stream.write(b'{"event":"STOP"}\n')
                    stream.write(
                        json.dumps({"return": {"status": "paused"}, "id": status["id"]}).encode()
                        + b"\n"
                    )
                listener.close()

            thread = threading.Thread(target=server)
            thread.start()
            self.assertTrue(ready.wait(2))
            client = QmpClient(path)
            client.connect()
            self.assertEqual(client.status(), "paused")
            client.close()
            thread.join(2)
            self.assertFalse(thread.is_alive())


class DaemonProcessTests(unittest.TestCase):
    """验证真实控制 socket 和原子 summary 交付。"""

    def test_rejects_a_non_qemu_pid(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            serial = root / "serial.log"
            serial.write_text("")
            result = subprocess.run(
                [
                    os.fspath(Path(__file__).parents[1] / "qemu_profile_daemon.py"),
                    "capture",
                    "--qemu-pid",
                    str(os.getpid()),
                    "--serial-log",
                    str(serial),
                    "--output",
                    str(root / "events.jsonl"),
                    "--summary",
                    str(root / "summary.json"),
                    "--control-socket",
                    str(root / "control.sock"),
                    "--system",
                    "test",
                    "--workload",
                    "fixture",
                    "--vcpu-count",
                    "1",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=3,
                check=False,
            )
        self.assertEqual(result.returncode, 1)
        self.assertIn("executable is not qemu-system", result.stderr)

    def test_timeline_only_capture(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_qemu = root / "qemu-system-loongarch64"
            shutil.copy2(shutil.which("sleep") or "/bin/sleep", fake_qemu)
            qemu = subprocess.Popen([fake_qemu, "10"])
            serial = root / "serial.log"
            serial.write_text("")
            output = root / "events.jsonl"
            summary = root / "summary.json"
            control = root / "control.sock"
            ready = root / "ready"
            command = [
                os.fspath(Path(__file__).parents[1] / "qemu_profile_daemon.py"),
                "capture",
                "--qemu-pid",
                str(qemu.pid),
                "--serial-log",
                str(serial),
                "--output",
                str(output),
                "--summary",
                str(summary),
                "--control-socket",
                str(control),
                "--ready-file",
                str(ready),
                "--system",
                "test",
                "--workload",
                "fixture",
                "--vcpu-count",
                "1",
                "--proc-interval-ms",
                "20",
            ]
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                deadline = time.monotonic() + 3
                while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(ready.exists())
                self.assertTrue(control_request(control, "start", "unit")["ok"])
                with serial.open("a") as stream:
                    stream.write("[64/446]\r[65/446]\n")
                time.sleep(0.15)
                self.assertTrue(control_request(control, "stop")["ok"])
                self.assertTrue(control_request(control, "shutdown")["ok"])
                stdout, stderr = process.communicate(timeout=3)
                self.assertEqual((process.returncode, stdout, stderr), (0, "", ""))
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                qemu.terminate()
                qemu.wait(timeout=3)
            result = json.loads(summary.read_text())
            self.assertFalse(result["quality"]["valid"])
            self.assertTrue(result["quality"]["qemu_process_identity_valid"])
            self.assertFalse(result["quality"]["vcpu_thread_preflight_valid"])
            self.assertGreater(result["quality"]["vcpu_thread_errors"], 0)
            self.assertEqual(result["cargo_milestones"].keys(), {"64", "65"})
            self.assertGreaterEqual(result["quality"]["proc_samples"], 2)


if __name__ == "__main__":
    unittest.main()
