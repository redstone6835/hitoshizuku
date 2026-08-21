"""QEMU daemon profile 比较器单元测试。"""

from __future__ import annotations

import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

from scripts.qemu_profile_compare import (
    COMPARISON_SCHEMA,
    ComparisonError,
    REQUIRED_ENVIRONMENT_FIELDS,
    compare_summaries,
    load_summary,
    main,
    render_human,
)


def environment() -> dict[str, str]:
    """构造 Linux/Hitoshizuku 两侧必须完全相同的运行环境身份。"""

    return {
        "base_image_sha256": "1" * 64,
        "cold_target": "true",
        "container_image_id": "sha256:" + "2" * 64,
        "container_user": "1000:1000",
        "cpuset": "0-7",
        "guest_initramfs_sha256": "9" * 64,
        "memory_bytes": str(8 * 1024 * 1024 * 1024),
        "plugin_sha256": "3" * 64,
        "qemu_accel": "tcg,thread=multi",
        "qemu_cpu": "la464",
        "qemu_debug_threads": "on",
        "qemu_machine": "virt",
        "qemu_name": "profile",
        "qemu_version": "QEMU emulator version 10.0.2",
        "smp": "8",
        "target_tmpfs": "tmpfs:/work/tgoskits/target:size=5368709120",
        "toolchain": "default",
        "workload_plan_sha256": "4" * 64,
        "workload_script_sha256": "5" * 64,
    }


def summary(
    system: str,
    *,
    active_ns: int,
    cpu_ticks: int,
    clock_hz: int = 100,
) -> dict[str, object]:
    """构造包含所有比较字段的最小合法摘要。"""

    return {
        "schema": "mygo.qemu-profile.v1",
        "metadata": {
            "system": system,
            "workload": "tg-xtask",
            "vcpu_count": 8,
            "proc_interval_ms": 1000,
            "stack_interval_ms": 20,
            "stack_timeout_ms": 5000,
            "max_frames": 32,
            "max_pause_ratio": 0.05,
            "plugin_period_insns": 50_000_000,
            "plugin_stack_bytes": 1024,
            "unwind": "stack-scan-guess-v1",
            "kernel_sha256": "6" * 64,
            "symbol_map_sha256": "7" * 64,
            "symbol_manifest_sha256": "8" * 64,
            "symbol_manifest_target": "loongarch64-test",
            "environment": environment(),
        },
        "quality": {
            "valid": True,
            "plugin_exit_reconciled": True,
            "pause_ratio": 0.01,
            "stack_samples": 100,
            "stack_successes": 98,
            "symbolized_frame_ratio": 0.95,
        },
        "capture": {
            "wall_duration_ns": active_ns + 10,
            "paused_ns": 10,
            "active_duration_ns": active_ns,
            "qemu_cpu_ticks": cpu_ticks,
            "clock_ticks_per_second": clock_hz,
        },
        "cargo_milestones": {"0": 0, "64": 400, "128": 800},
        "stage_spans": [
            {"name": "compile", "active_duration_ns": 700},
            {"name": "link", "active_duration_ns": 300},
        ],
        "hotspots": [
            {"function": "fault", "samples": 60, "percent": 60.0},
            {"function": "walk", "samples": 40, "percent": 40.0},
        ],
    }


class ComparisonTests(unittest.TestCase):
    """验证核心比值、里程碑选择和热点差异。"""

    def test_report_uses_active_time_and_normalized_cpu_seconds(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=500, clock_hz=100)
        candidate = summary("candidate", active_ns=800, cpu_ticks=360, clock_hz=120)
        candidate["cargo_milestones"] = {"0": 0, "64": 300, "128": 500, "256": 700}
        candidate["stage_spans"] = [
            {"name": "compile", "active_duration_ns": 350},
            {"name": "link", "active_duration_ns": 450},
            {"name": "candidate-only", "active_duration_ns": 20},
        ]
        candidate["hotspots"] = [
            {"function": "fault", "samples": 40, "percent": 40.0},
            {"function": "walk", "samples": 50, "percent": 50.0},
            {"function": "new", "samples": 10, "percent": 10.0},
        ]

        report = compare_summaries(baseline, candidate)

        self.assertEqual(report["schema"], COMPARISON_SCHEMA)
        self.assertEqual(report["environment"], environment())
        self.assertEqual(report["profiling"]["plugin_period_insns"], 50_000_000)
        self.assertAlmostEqual(report["active_speedup"], 1.25)
        self.assertAlmostEqual(report["cpu_speedup"], 5 / 3)
        self.assertEqual(report["common_milestone"]["progress"], "128")
        self.assertAlmostEqual(report["common_milestone_speedup"], 1.6)
        self.assertIsNone(report["accepted"])
        self.assertEqual([item["name"] for item in report["stage_speedups"]], ["compile", "link"])
        self.assertAlmostEqual(report["stage_speedups"][0]["speedup"], 2.0)

        hotspots = {item["function"]: item for item in report["hotspot_differences"]}
        self.assertEqual(hotspots["fault"]["percent_point_delta"], -20.0)
        self.assertEqual(hotspots["walk"]["sample_delta"], 10)
        self.assertEqual(hotspots["new"]["baseline_samples"], 0)

    def test_no_common_positive_milestone_is_reported_as_null(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=900, cpu_ticks=90)
        baseline["cargo_milestones"] = {"0": 0, "64": 500}
        candidate["cargo_milestones"] = {"0": 0, "128": 700}

        report = compare_summaries(baseline, candidate)

        self.assertIsNone(report["common_milestone"])
        self.assertIsNone(report["common_milestone_speedup"])

    def test_disabled_stack_sampling_and_zero_cpu_ticks_remain_comparable(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=0)
        candidate = summary("candidate", active_ns=900, cpu_ticks=0)
        for value in (baseline, candidate):
            value["metadata"]["stack_interval_ms"] = 0
            value["quality"]["stack_samples"] = 0
            value["quality"]["stack_successes"] = 0
            value["quality"]["symbolized_frame_ratio"] = 0

        report = compare_summaries(baseline, candidate)

        self.assertIsNone(report["cpu_speedup"])

    def test_repeated_stage_names_are_aggregated(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=900, cpu_ticks=90)
        baseline["stage_spans"] = [
            {"name": "compile", "active_duration_ns": 200},
            {"name": "compile", "active_duration_ns": 300},
        ]
        candidate["stage_spans"] = [{"name": "compile", "active_duration_ns": 250}]

        report = compare_summaries(baseline, candidate)

        self.assertEqual(report["stage_speedups"][0]["baseline_active_duration_ns"], 500)
        self.assertEqual(report["stage_speedups"][0]["speedup"], 2.0)

    def test_required_speedup_is_only_enforced_when_requested(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=800, cpu_ticks=80)
        candidate["cargo_milestones"] = {"0": 0, "64": 300, "128": 600}

        self.assertTrue(compare_summaries(baseline, candidate, required_speedup=1.2)["accepted"])
        self.assertFalse(compare_summaries(baseline, candidate, required_speedup=1.4)["accepted"])

    def test_gate_prefers_largest_common_milestone_over_active_duration(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=500, cpu_ticks=80)
        candidate["cargo_milestones"] = {"0": 0, "64": 390, "128": 790, "256": 900}

        report = compare_summaries(baseline, candidate, required_speedup=1.5)

        self.assertEqual(report["gate_metric"], "milestone:128")
        self.assertAlmostEqual(report["gate_speedup"], 800 / 790)
        self.assertFalse(report["accepted"])

    def test_gate_falls_back_to_active_duration_without_common_milestone(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=500, cpu_ticks=80)
        baseline["cargo_milestones"] = {"0": 0, "64": 400}
        candidate["cargo_milestones"] = {"0": 0, "128": 500}

        report = compare_summaries(baseline, candidate, required_speedup=1.5)

        self.assertEqual(report["gate_metric"], "active_duration")
        self.assertEqual(report["gate_speedup"], 2.0)
        self.assertTrue(report["accepted"])


class ValidationTests(unittest.TestCase):
    """验证质量门禁和可比性约束。"""

    def test_incompatible_metadata_is_rejected(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        for field, value in (
            ("workload", "other"),
            ("vcpu_count", 4),
            ("proc_interval_ms", 500),
            ("stack_interval_ms", 10),
            ("plugin_period_insns", 10_000_000),
            ("plugin_stack_bytes", 2048),
            ("unwind", "other"),
        ):
            with self.subTest(field=field):
                candidate = summary("candidate", active_ns=900, cpu_ticks=90)
                candidate["metadata"][field] = value
                if field == "vcpu_count":
                    candidate["metadata"]["environment"]["smp"] = str(value)
                with self.assertRaisesRegex(ComparisonError, field):
                    compare_summaries(baseline, candidate)

    def test_missing_required_environment_fields_are_rejected(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        for field in sorted(REQUIRED_ENVIRONMENT_FIELDS):
            with self.subTest(field=field):
                candidate = summary("candidate", active_ns=900, cpu_ticks=90)
                del candidate["metadata"]["environment"][field]
                with self.assertRaisesRegex(ComparisonError, field):
                    compare_summaries(baseline, candidate)

    def test_verified_symbol_snapshot_is_required(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        for field in (
            "kernel_sha256",
            "symbol_map_sha256",
            "symbol_manifest_sha256",
            "symbol_manifest_target",
        ):
            with self.subTest(field=field):
                candidate = summary("candidate", active_ns=900, cpu_ticks=90)
                candidate["metadata"][field] = None
                with self.assertRaisesRegex(ComparisonError, field):
                    compare_summaries(baseline, candidate)

    def test_incompatible_environment_is_rejected_field_by_field(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        for field, value in (
            ("base_image_sha256", "a" * 64),
            ("cpuset", "8-15"),
            ("qemu_cpu", "max"),
            ("target_tmpfs", "ext4:/work/tgoskits/target"),
            ("toolchain", "nightly-other"),
            ("workload_plan_sha256", "b" * 64),
        ):
            with self.subTest(field=field):
                candidate = summary("candidate", active_ns=900, cpu_ticks=90)
                candidate["metadata"]["environment"][field] = value
                with self.assertRaisesRegex(ComparisonError, rf"environment\.{field}"):
                    compare_summaries(baseline, candidate)

    def test_environment_identity_values_are_canonical(self) -> None:
        invalid_values = (
            ("base_image_sha256", "A" * 64),
            ("container_image_id", "2" * 64),
            ("container_user", "root:root"),
            ("memory_bytes", "0"),
            ("smp", "4"),
            ("cold_target", "false"),
        )
        for field, value in invalid_values:
            with self.subTest(field=field):
                candidate = summary("candidate", active_ns=900, cpu_ticks=90)
                candidate["metadata"]["environment"][field] = value
                with self.assertRaisesRegex(ComparisonError, field):
                    compare_summaries(
                        summary("baseline", active_ns=1_000, cpu_ticks=100), candidate
                    )

    def test_extension_environment_fields_are_also_compared(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=900, cpu_ticks=90)
        candidate["metadata"]["environment"]["host_governor"] = "performance"

        with self.assertRaisesRegex(ComparisonError, "host_governor"):
            compare_summaries(baseline, candidate)

    def test_invalid_quality_is_rejected_on_either_side(self) -> None:
        baseline = summary("baseline", active_ns=1_000, cpu_ticks=100)
        candidate = summary("candidate", active_ns=900, cpu_ticks=90)
        candidate["quality"]["valid"] = False

        with self.assertRaisesRegex(ComparisonError, "quality.valid"):
            compare_summaries(baseline, candidate)

    def test_internal_quality_and_capture_ranges_are_checked(self) -> None:
        for field, value in (
            ("pause_ratio", 1.1),
            ("symbolized_frame_ratio", -0.1),
            ("stack_successes", 101),
        ):
            with self.subTest(field=field):
                candidate = summary("candidate", active_ns=900, cpu_ticks=90)
                candidate["quality"][field] = value
                with self.assertRaises(ComparisonError):
                    compare_summaries(
                        summary("baseline", active_ns=1_000, cpu_ticks=100), candidate
                    )

        candidate = summary("candidate", active_ns=900, cpu_ticks=90)
        candidate["capture"]["paused_ns"] = candidate["capture"]["wall_duration_ns"] + 1
        with self.assertRaisesRegex(ComparisonError, "paused_ns"):
            compare_summaries(summary("baseline", active_ns=1_000, cpu_ticks=100), candidate)


class CommandLineTests(unittest.TestCase):
    """验证目录输入、两种输出格式和退出码。"""

    def write_summary(self, directory: Path, value: dict[str, object]) -> Path:
        directory.mkdir()
        path = directory / "summary.json"
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def test_load_summary_accepts_run_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            run_dir = Path(temporary) / "run"
            path = self.write_summary(run_dir, summary("baseline", active_ns=1_000, cpu_ticks=100))

            loaded = load_summary(run_dir)

            self.assertEqual(loaded["_path"], str(path))

    def test_load_summary_prefers_runner_observer_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            run_dir = Path(temporary) / "run"
            run_dir.mkdir()
            (run_dir / "summary.json").write_text(
                json.dumps({"schema": "mygo.profile", "schema_version": 2}),
                encoding="utf-8",
            )
            observer = run_dir / "qemu-profile-summary.json"
            observer.write_text(
                json.dumps(summary("baseline", active_ns=1_000, cpu_ticks=100)),
                encoding="utf-8",
            )

            loaded = load_summary(run_dir)

            self.assertEqual(loaded["_path"], str(observer))

    def test_json_mode_and_failed_gate_exit_code(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = self.write_summary(
                root / "baseline", summary("baseline", active_ns=1_000, cpu_ticks=100)
            )
            candidate = self.write_summary(
                root / "candidate", summary("candidate", active_ns=900, cpu_ticks=90)
            )
            output = io.StringIO()
            with redirect_stdout(output):
                status = main([str(baseline), str(candidate), "--required-speedup", "2", "--json"])

            self.assertEqual(status, 1)
            report = json.loads(output.getvalue())
            self.assertFalse(report["accepted"])

    def test_default_output_is_human_readable_and_analysis_only(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = self.write_summary(
                root / "baseline", summary("baseline", active_ns=1_000, cpu_ticks=100)
            )
            candidate = self.write_summary(
                root / "candidate", summary("candidate", active_ns=800, cpu_ticks=80)
            )
            output = io.StringIO()
            with redirect_stdout(output):
                status = main([str(baseline), str(candidate)])

            self.assertEqual(status, 0)
            self.assertIn("活动时长", output.getvalue())
            self.assertIn("仅分析", output.getvalue())

    def test_bad_schema_returns_usage_error(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            baseline = self.write_summary(
                root / "baseline", summary("baseline", active_ns=1_000, cpu_ticks=100)
            )
            bad = summary("candidate", active_ns=900, cpu_ticks=90)
            bad["schema"] = "other"
            candidate = self.write_summary(root / "candidate", bad)
            errors = io.StringIO()
            with redirect_stderr(errors):
                status = main([str(baseline), str(candidate), "--json"])

            self.assertEqual(status, 2)
            self.assertIn("schema", errors.getvalue())

    def test_human_renderer_includes_common_milestone(self) -> None:
        report = compare_summaries(
            summary("baseline", active_ns=1_000, cpu_ticks=100),
            summary("candidate", active_ns=800, cpu_ticks=80),
        )
        self.assertIn("里程碑 128", render_human(report))


if __name__ == "__main__":
    unittest.main()
