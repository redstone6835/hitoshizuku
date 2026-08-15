"""RISC-V 指令权重宿主遥测门禁测试。"""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import tempfile
import unittest
import unittest.mock
from pathlib import Path
from types import SimpleNamespace
from typing import Callable


REPOSITORY = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "riscv_weight_host_telemetry",
    REPOSITORY / "scripts/riscv_weight_host_telemetry.py",
)
assert SPEC is not None and SPEC.loader is not None
TELEMETRY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TELEMETRY)


def run_design(design: str = "ABBA") -> list[dict[str, object]]:
    positions = (
        ((1, 2), (4, 3))
        if design == "ABBA"
        else ((2, 1), (3, 4))
    )
    return [
        {
            "run_id": f"run-{pair}",
            "run_order": pair - 1,
            "super_run_id": "super-1",
            "super_run_order": 0,
            "crossover_pair": pair,
            "crossover_design": design,
            "timing_launch_position": timing,
            "plugin_off_launch_position": plugin_off,
        }
        for pair, (timing, plugin_off) in enumerate(positions, start=1)
    ]


def snapshots(
    design_rows: list[dict[str, object]], *, sibling_busy: int = 0
) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    kernel_affinity = {
        path: {
            "syntax": syntax,
            "raw": "c" if syntax == "mask" else "2-3",
            "cpus": [2, 3],
        }
        for path, syntax in (
            ("/sys/devices/virtual/workqueue/cpumask", "mask"),
            ("/sys/devices/virtual/workqueue/writeback/cpumask", "mask"),
            ("/sys/devices/virtual/workqueue/test-wq/cpumask", "mask"),
            ("/proc/sys/kernel/watchdog_cpumask", "list"),
        )
    }
    launches: list[tuple[str, str, int]] = []
    for row in design_rows:
        launches.extend(
            (
                (str(row["run_id"]), "timing", int(row["timing_launch_position"])),
                (
                    str(row["run_id"]),
                    "plugin-off",
                    int(row["plugin_off_launch_position"]),
                ),
            )
        )
    for launch_index, (run_id, mode, position) in enumerate(launches):
        launch_id = f"super-1-{position}-{mode}"
        base_ns = (launch_index + 1) * 10_000_000_000
        for phase in ("before", "after"):
            before = phase == "before"
            selected_times = (
                [100, 0, 0, 100, 0]
                if before
                else [190, 0, 0, 110, 0]
            )
            sibling_times = (
                [10, 0, 0, 100, 0]
                if before
                else [10 + sibling_busy, 0, 0, 200 - sibling_busy, 0]
            )
            pressure_offset = 0 if before else 1_000
            result.append(
                {
                    "schema": TELEMETRY.TELEMETRY_SCHEMA,
                    "timestamp_ns": base_ns + (0 if before else 1_000_000_000),
                    "monotonic_ns": base_ns + (0 if before else 1_000_000_000),
                    "phase": phase,
                    "launch_id": launch_id,
                    "super_run_id": "super-1",
                    "run_id": run_id,
                    "mode": mode,
                    "launch_position": position,
                    "selected_cpus": [0],
                    "physical_core_cpus": [0, 1],
                    "selected_core_temperature_sensors": ["coretemp:Core 0"],
                    "kernel_affinity": copy.deepcopy(kernel_affinity),
                    "cpu": {
                        "0": {
                            "times": selected_times,
                            "schedstat": {
                                "run_ns": 1_000_000_000
                                + (0 if before else 900_000_000),
                                "wait_ns": 10_000_000
                                + (0 if before else 1_000_000),
                                "timeslices": 1_000
                                + (0 if before else 100),
                            },
                            "interrupts": {
                                "external": 100 + (0 if before else 10),
                                "local": 1_000 + (0 if before else 1_000),
                            },
                            "online": True,
                            "governor": "performance",
                            "mperf": 1_000_000 + (0 if before else 1_000_000),
                            "aperf": 2_000_000 + (0 if before else 1_000_000),
                            "scaling_cur_freq": 4_500_000,
                            "scaling_min_freq": 3_000_000,
                            "scaling_max_freq": 4_500_000,
                        },
                        "1": {
                            "times": sibling_times,
                            "schedstat": {
                                "run_ns": 100_000_000
                                + (0 if before else 10_000_000),
                                "wait_ns": 1_000_000
                                + (0 if before else 100_000),
                                "timeslices": 100 + (0 if before else 10),
                            },
                            "interrupts": {
                                "external": 50 + (0 if before else 1),
                                "local": 500 + (0 if before else 100),
                            },
                            "online": True,
                            "governor": "performance",
                            "mperf": 1_000_000 + (0 if before else 10_000),
                            "aperf": 2_000_000 + (0 if before else 10_000),
                            "scaling_cur_freq": 4_500_000,
                            "scaling_min_freq": 3_000_000,
                            "scaling_max_freq": 4_500_000,
                        },
                    },
                    "load_per_online_cpu": 0.05,
                    "pressure_cpu": (
                        "some avg10=0.00 avg60=0.00 avg300=0.00 "
                        f"total={10_000 + pressure_offset}"
                    ),
                    "pressure_memory": (
                        "some avg10=0.00 avg60=0.00 avg300=0.00 "
                        f"total={20_000 + pressure_offset // 10}\n"
                        "full avg10=0.00 avg60=0.00 avg300=0.00 "
                        f"total={2_000 + pressure_offset // 100}"
                    ),
                    "mem_available_kib": 8 * 1024 * 1024,
                    "temperatures_c": {
                        "coretemp:Core 0": 50.0 if before else 51.0
                    },
                }
            )
    return result


def isolation_evidence() -> dict[str, object]:
    kernel_affinity_entries: list[dict[str, object]] = []
    for kind, path, syntax in (
        (
            "global-workqueue",
            "/sys/devices/virtual/workqueue/cpumask",
            "mask",
        ),
        (
            "writeback-workqueue",
            "/sys/devices/virtual/workqueue/writeback/cpumask",
            "mask",
        ),
        (
            "named-workqueue",
            "/sys/devices/virtual/workqueue/test-wq/cpumask",
            "mask",
        ),
        ("watchdog", "/proc/sys/kernel/watchdog_cpumask", "list"),
    ):
        kernel_affinity_entries.append(
            {
                "kind": kind,
                "path": path,
                "syntax": syntax,
                "initial_raw": "f" if syntax == "mask" else "0-3",
                "initial_cpus": [0, 1, 2, 3],
                "requested_raw": "c" if syntax == "mask" else "2-3",
                "requested_cpus": [2, 3],
                "readback_raw": "c" if syntax == "mask" else "2-3",
                "readback_cpus": [2, 3],
                "write_attempted": True,
                "write_failed": False,
                "write_error": None,
                "matches_requested": True,
                "excludes_physical_core": True,
            }
        )
    kernel_affinity_hash = hashlib.sha256(
        json.dumps(
            kernel_affinity_entries, sort_keys=True, separators=(",", ":")
        ).encode()
    ).hexdigest()
    entries: list[dict[str, object]] = [
        {
            "irq": 10,
            "path": "/proc/irq/10/smp_affinity_list",
            "appeared_after_plan": False,
            "migration_required": True,
            "write_attempted": True,
            "write_failed": False,
            "write_error": None,
            "actions": "test-migrated",
            "classification": "migrated_and_verified",
            "initial_effective_raw": "0",
            "initial_effective_cpus": [0],
            "requested_raw": "2-3",
            "requested_cpus": [2, 3],
            "effective_path": "/proc/irq/10/effective_affinity_list",
            "effective_raw": "2",
            "effective_cpus": [2],
        },
        {
            "irq": 11,
            "path": "/proc/irq/11/smp_affinity_list",
            "appeared_after_plan": False,
            "migration_required": False,
            "write_attempted": False,
            "write_failed": False,
            "write_error": None,
            "actions": "test-already-excluded",
            "classification": "already_excluded",
            "initial_effective_raw": "3",
            "initial_effective_cpus": [3],
            "requested_raw": "0-3",
            "requested_cpus": [0, 1, 2, 3],
            "effective_path": "/proc/irq/11/effective_affinity_list",
            "effective_raw": "3",
            "effective_cpus": [3],
        },
        {
            "irq": 12,
            "path": "/proc/irq/12/smp_affinity_list",
            "appeared_after_plan": False,
            "migration_required": False,
            "write_attempted": False,
            "write_failed": False,
            "write_error": None,
            "actions": "",
            "classification": "inactive_no_target",
            "initial_effective_raw": "",
            "initial_effective_cpus": [],
            "requested_raw": "0-3",
            "requested_cpus": [0, 1, 2, 3],
            "effective_path": "/proc/irq/12/effective_affinity_list",
            "effective_raw": "",
            "effective_cpus": [],
        },
    ]
    entries_hash = hashlib.sha256(
        json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    residuals: list[dict[str, object]] = []
    residuals_hash = hashlib.sha256(
        json.dumps(residuals, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    completed_ns = 9_900_000_000
    process_ns = 1_000_000_000
    delta_mperf = 3_300_000_000
    delta_aperf = 3_300_000_000
    frequency_preflight = {
        "schema": TELEMETRY.FREQUENCY_PREFLIGHT_SCHEMA,
        "selected_cpu": 0,
        "started_timestamp_ns": 8_900_000_000,
        "completed_timestamp_ns": 9_900_000_000,
        "started_monotonic_ns": 8_900_000_000,
        "completed_monotonic_ns": completed_ns,
        "requested_duration_ns": 1_000_000_000,
        "elapsed_ns": 1_000_000_000,
        "process_cpu_ns": process_ns,
        "iterations": 1_000_000,
        "state_checksum": "0123456789abcdef",
        "counters": {
            "mperf": {"before": 1, "after": 1 + delta_mperf, "delta": delta_mperf},
            "aperf": {"before": 2, "after": 2 + delta_aperf, "delta": delta_aperf},
        },
        "aperf_mperf_ratio": delta_aperf / delta_mperf,
        "process_busy_fraction": 1.0,
        "estimated_nominal_mhz": delta_mperf / process_ns * 1000.0,
        "estimated_actual_mhz": delta_aperf / process_ns * 1000.0,
        "thresholds": {
            "minimum_aperf_mperf_ratio": 0.95,
            "minimum_process_busy_fraction": 0.90,
        },
        "failures": [],
        "passed": True,
    }
    return {
        "schema": "mygo.riscv-weight-host-isolation.v5",
        "active_during_measurement": True,
        "selected_cpus": [0],
        "physical_core_cpus": [0, 1],
        "orchestrator_cpu": 2,
        "requested_background_cpus": [2, 3],
        "online_cpus": [0, 2, 3],
        "selected_cpu_online": True,
        "orchestrator_cpu_online": True,
        "smt_sibling_online_states": {"0": True, "1": False},
        "smt_siblings_offline": True,
        "measurement_slice_active": True,
        "measurement_slice_effective_cpus": "0,2-3",
        "measurement_slice_effective_cpu_list": [0, 2, 3],
        "background_slices": {
            name: {
                "effective_cpus": "2-3",
                "effective_cpu_list": [2, 3],
                "is_subset_of_requested_background": True,
                "excludes_physical_core": True,
            }
            for name in ("system.slice", "user.slice", "machine.slice")
        },
        "frequency": {
            "0": {
                "governor": "performance",
                "minimum": 4_500_000,
                "maximum": 4_500_000,
                "cpuinfo_maximum": 4_500_000,
            },
            "1": None,
        },
        "frequency_policy_applied": True,
        "frequency_preflight": frequency_preflight,
        "frequency_preflight_sha256": TELEMETRY._canonical_sha256(
            frequency_preflight
        ),
        "kernel_affinity_entries": kernel_affinity_entries,
        "kernel_affinity_entries_sha256": kernel_affinity_hash,
        "kernel_affinity_observed_count": len(kernel_affinity_entries),
        "kernel_affinity_write_failure_count": 0,
        "kernel_affinity_failed_paths": [],
        "kernel_affinity_required_kinds": [
            "global-workqueue",
            "watchdog",
            "writeback-workqueue",
        ],
        "kernel_affinity_policy_satisfied": True,
        "irq_affinity_attempt_failures": 0,
        "irq_affinity_default_write_failed": False,
        "irq_affinity_initial_read_errors": [],
        "irq_affinity_readback_failures": [],
        "irq_affinity_disappeared_after_plan": [],
        "irq_affinity_appeared_after_plan": [],
        "irq_affinity_violations": [],
        "irq_affinity_default_raw": "c",
        "irq_affinity_default_effective_cpus": [2, 3],
        "irq_affinity_default_matches_requested": True,
        "irq_affinity_observed_count": 3,
        "irq_affinity_planned_count": 3,
        "irq_affinity_migration_required_count": 1,
        "irq_affinity_write_attempt_count": 1,
        "irq_affinity_attempted_paths": [
            "/proc/irq/10/smp_affinity_list"
        ],
        "irq_affinity_write_failure_count": 0,
        "irq_affinity_failed_paths": [],
        "irq_affinity_skipped_safe_count": 2,
        "irq_affinity_readback_violation_count": 0,
        "irq_affinity_entries_sha256": entries_hash,
        "irq_affinity_entries": entries,
        "irq_affinity_migrated_and_verified_count": 1,
        "irq_affinity_already_excluded_count": 1,
        "irq_affinity_inactive_no_target_count": 1,
        "irq_affinity_residual_unmigratable_count": 0,
        "irq_affinity_residual_unmigratable": residuals,
        "irq_affinity_residual_unmigratable_sha256": residuals_hash,
        "irq_affinity_applied": True,
        "irq_isolation_policy_satisfied": True,
        "irq_residual_requires_zero_external_interrupts": False,
        "preflight_checks": {
            "selected_cpu_online": True,
            "orchestrator_cpu_online": True,
            "smt_siblings_offline": True,
            "measurement_slice_active": True,
            "background_slices_applied": True,
            "frequency_policy_applied": True,
            "hardware_frequency_preflight_passed": True,
            "irq_isolation_policy_satisfied": True,
            "kernel_affinity_policy_satisfied": True,
        },
        "restore_trap_armed": True,
    }


class HostTelemetryTests(unittest.TestCase):
    def test_cpu_list_accepts_kernel_and_systemd_separators(self) -> None:
        for value in ("0-2,4,6-7", "0-2 4 6-7", "0-2, 4\t6-7"):
            with self.subTest(value=value):
                self.assertEqual(TELEMETRY._cpu_list(value), [0, 1, 2, 4, 6, 7])

    def test_frequency_preflight_captures_hardware_counters_and_upgrades_state(
        self,
    ) -> None:
        evidence = isolation_evidence()
        evidence["schema"] = "mygo.riscv-weight-host-isolation.v4"
        evidence.pop("frequency_preflight")
        evidence.pop("frequency_preflight_sha256")
        checks = evidence["preflight_checks"]  # type: ignore[assignment]
        checks.pop("hardware_frequency_preflight_passed")
        counter_values = {
            TELEMETRY.MSR_MPERF: iter((1_000_000, 3_301_000_000)),
            TELEMETRY.MSR_APERF: iter((2_000_000, 3_302_000_000)),
        }
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "frequency-preflight.json"
            isolation_path = Path(directory) / "isolation-state.json"
            isolation_path.write_text(json.dumps(evidence) + "\n", encoding="utf-8")
            with (
                unittest.mock.patch.object(
                    TELEMETRY,
                    "_read_msr",
                    side_effect=lambda _cpu, register: next(counter_values[register]),
                ),
                unittest.mock.patch.object(
                    TELEMETRY.os, "sched_setaffinity", return_value=None
                ),
                unittest.mock.patch.object(
                    TELEMETRY.os, "sched_getaffinity", return_value={0}
                ),
            ):
                status = TELEMETRY.frequency_preflight(
                    SimpleNamespace(
                        cpu=0,
                        output=str(output),
                        isolation_state=str(isolation_path),
                        duration_seconds=0.1,
                        minimum_aperf_mperf_ratio=0.95,
                        minimum_process_busy_fraction=0.90,
                    )
                )
            result = json.loads(output.read_text(encoding="utf-8"))
            isolation = json.loads(isolation_path.read_text(encoding="utf-8"))

        self.assertEqual(status, 0)
        self.assertTrue(result["passed"])
        self.assertAlmostEqual(result["aperf_mperf_ratio"], 1.0)
        self.assertEqual(isolation["schema"], "mygo.riscv-weight-host-isolation.v5")
        self.assertEqual(isolation["frequency_preflight"], result)
        self.assertTrue(
            isolation["preflight_checks"]["hardware_frequency_preflight_passed"]
        )

    def test_isolation_accepts_systemd_space_separated_cpu_lists(self) -> None:
        evidence = isolation_evidence()
        evidence["measurement_slice_effective_cpus"] = "0 2-3"
        background_slices = evidence["background_slices"]  # type: ignore[assignment]
        for value in background_slices.values():
            value["effective_cpus"] = "2 3"

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )

        self.assertEqual(result["status"], "accepted")
        self.assertEqual(result["exit_status"], 0)

    def verify_binding(
        self, *, source: str = "current", mutate_audit=None, mutate_current=None
    ) -> dict[str, object]:
        design_rows = run_design("ABBA")
        telemetry_rows = snapshots(design_rows)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            telemetry_path = root / "host-telemetry.jsonl"
            design_path = root / "run-design.jsonl"
            audit_path = root / "host-audit.json"
            binding_path = root / "host-audit-binding.json"
            telemetry_path.write_text(
                "".join(json.dumps(row) + "\n" for row in telemetry_rows),
                encoding="utf-8",
            )
            design_path.write_text(
                "".join(json.dumps(row) + "\n" for row in design_rows),
                encoding="utf-8",
            )
            status = TELEMETRY.audit(
                SimpleNamespace(
                    input=str(telemetry_path),
                    run_design=str(design_path),
                    output=str(audit_path),
                    isolation_state=None,
                    require_isolation_state=False,
                    max_sibling_busy=0.10,
                    max_load_per_cpu=0.75,
                    min_frequency_ratio=0.90,
                    require_frequency_floor=True,
                    require_window_frequency=True,
                    require_frequency_preflight=False,
                    min_window_frequency_ratio=0.95,
                    min_window_to_preflight_ratio=0.95,
                    max_frequency_preflight_age_seconds=300.0,
                    max_window_frequency_cv=0.03,
                    max_interrupts_per_second=25.0,
                    require_interrupts=True,
                    max_temperature_span=12.0,
                    max_temperature=90.0,
                    min_selected_busy=0.50,
                    max_cpu_psi=0.10,
                    max_memory_psi=0.02,
                    require_psi=True,
                    min_mem_available_kib=1024 * 1024,
                )
            )
            self.assertEqual(status, 0)
            if mutate_audit is not None:
                document = json.loads(audit_path.read_text(encoding="utf-8"))
                mutate_audit(document)
                audit_path.write_text(json.dumps(document) + "\n", encoding="utf-8")
            if mutate_current is not None:
                mutate_current(telemetry_path, design_path)
            binding_status = TELEMETRY.verify_binding(
                SimpleNamespace(
                    audit=str(audit_path),
                    input=str(telemetry_path),
                    run_design=str(design_path),
                    source=source,
                    output=str(binding_path),
                )
            )
            result = json.loads(binding_path.read_text(encoding="utf-8"))
            result["exit_status"] = binding_status
            return result

    def audit(
        self,
        *,
        design: str = "ABBA",
        sibling_busy: int = 0,
        mutate_rows: Callable[[list[dict[str, object]]], None] | None = None,
        mutate_design: Callable[[list[dict[str, object]]], None] | None = None,
        require_psi: bool = True,
        require_frequency_floor: bool = True,
        require_window_frequency: bool = True,
        require_frequency_preflight: bool = False,
        require_schedstat: bool = True,
        require_isolation_state: bool = False,
        isolation_state: dict[str, object] | None = None,
    ) -> dict[str, object]:
        design_rows = run_design(design)
        rows = snapshots(design_rows, sibling_busy=sibling_busy)
        if mutate_rows is not None:
            mutate_rows(rows)
        if mutate_design is not None:
            mutate_design(design_rows)
        with tempfile.TemporaryDirectory() as directory:
            input_path = Path(directory) / "telemetry.jsonl"
            design_path = Path(directory) / "run-design.jsonl"
            output_path = Path(directory) / "audit.json"
            isolation_path = Path(directory) / "isolation-state.json"
            input_path.write_text(
                "".join(json.dumps(row) + "\n" for row in rows),
                encoding="utf-8",
            )
            design_path.write_text(
                "".join(json.dumps(row) + "\n" for row in design_rows),
                encoding="utf-8",
            )
            if isolation_state is not None:
                isolation_path.write_text(
                    json.dumps(isolation_state) + "\n", encoding="utf-8"
                )
            status = TELEMETRY.audit(
                SimpleNamespace(
                    input=str(input_path),
                    run_design=str(design_path),
                    output=str(output_path),
                    isolation_state=(
                        str(isolation_path)
                        if isolation_state is not None
                        else None
                    ),
                    require_isolation_state=require_isolation_state,
                    max_sibling_busy=0.10,
                    max_load_per_cpu=0.75,
                    min_frequency_ratio=0.90,
                    require_frequency_floor=require_frequency_floor,
                    require_window_frequency=require_window_frequency,
                    require_frequency_preflight=require_frequency_preflight,
                    min_window_frequency_ratio=0.95,
                    min_window_to_preflight_ratio=0.95,
                    max_frequency_preflight_age_seconds=300.0,
                    max_window_frequency_cv=0.03,
                    max_interrupts_per_second=25.0,
                    require_interrupts=True,
                    require_schedstat=require_schedstat,
                    max_runqueue_wait_fraction=0.01,
                    max_temperature_span=12.0,
                    max_temperature=90.0,
                    min_selected_busy=0.50,
                    max_cpu_psi=0.10,
                    max_memory_psi=0.02,
                    require_psi=require_psi,
                    min_mem_available_kib=1024 * 1024,
                )
            )
            result = json.loads(output_path.read_text(encoding="utf-8"))
            result["exit_status"] = status
            result["expected_telemetry_sha256"] = hashlib.sha256(
                input_path.read_bytes()
            ).hexdigest()
            result["expected_design_sha256"] = hashlib.sha256(
                design_path.read_bytes()
            ).hexdigest()
            return result

    @staticmethod
    def reasons(result: dict[str, object]) -> set[str]:
        return {
            str(failure["reason"])
            for failure in result["failures"]  # type: ignore[union-attr]
        }

    def test_complete_abba_and_baab_designs_are_accepted(self) -> None:
        for design in ("ABBA", "BAAB"):
            with self.subTest(design=design):
                result = self.audit(design=design)
                self.assertEqual(result["status"], "accepted")
                self.assertEqual(result["exit_status"], 0)
                self.assertEqual(result["planned_launches"], 4)
                self.assertEqual(result["observed_launches"], 4)
                self.assertEqual(result["complete_launches"], 4)
                inputs = result["inputs"]
                self.assertEqual(
                    inputs["telemetry"]["sha256"],  # type: ignore[index]
                    result["expected_telemetry_sha256"],
                )
                self.assertEqual(
                    inputs["run_design"]["sha256"],  # type: ignore[index]
                    result["expected_design_sha256"],
                )

    def test_binding_accepts_only_current_complete_evidence(self) -> None:
        result = self.verify_binding()
        self.assertEqual(result["exit_status"], 0)
        self.assertTrue(result["publication_allowed"])
        self.assertTrue(all(result["checks"].values()))  # type: ignore[union-attr]

    def test_binding_rejects_external_audit_even_when_self_consistent(self) -> None:
        result = self.verify_binding(source="external")
        self.assertEqual(result["exit_status"], 1)
        self.assertFalse(result["publication_allowed"])
        self.assertIn("external-host-audit-not-publishable", self.reasons(result))

    def test_binding_rejects_current_run_design_hash_or_launch_drift(self) -> None:
        def drift(_telemetry: Path, design: Path) -> None:
            rows = run_design("BAAB")
            design.write_text(
                "".join(json.dumps(row) + "\n" for row in rows),
                encoding="utf-8",
            )

        result = self.verify_binding(mutate_current=drift)
        self.assertEqual(result["exit_status"], 1)
        self.assertFalse(result["publication_allowed"])
        reasons = self.reasons(result)
        self.assertIn("host-audit-run-design-binding-mismatch", reasons)
        self.assertIn("current-telemetry-launch-set-incomplete", reasons)
        self.assertIn("host-audit-launch-manifest-mismatch", reasons)

    def test_binding_rejects_forged_audit_launch_manifest(self) -> None:
        def remove_launch(document: dict[str, object]) -> None:
            launches = document["launches"]  # type: ignore[assignment]
            launches.pop()

        result = self.verify_binding(mutate_audit=remove_launch)
        self.assertFalse(result["publication_allowed"])
        self.assertIn("host-audit-launch-manifest-mismatch", self.reasons(result))

    def test_formal_audit_binds_verified_isolation_state(self) -> None:
        evidence = isolation_evidence()

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )

        self.assertEqual(result["status"], "accepted")
        self.assertIsNotNone(result["inputs"]["isolation_state"])

        evidence["smt_siblings_offline"] = False
        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )
        self.assertIn("isolation-state-check-failed", self.reasons(result))

        result = self.audit(require_isolation_state=True)
        self.assertIn("isolation-state-unavailable", self.reasons(result))

    def test_inactive_irq_with_empty_effective_affinity_is_accepted(self) -> None:
        evidence = isolation_evidence()

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )

        self.assertEqual(result["status"], "accepted")

    def test_isolation_rejects_kernel_affinity_entry_tampering(self) -> None:
        evidence = isolation_evidence()
        entries = evidence["kernel_affinity_entries"]  # type: ignore[assignment]
        entries[0]["readback_raw"] = "d"
        entries[0]["readback_cpus"] = [0, 2, 3]
        entries[0]["matches_requested"] = False
        evidence["kernel_affinity_entries_sha256"] = hashlib.sha256(
            json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )

        self.assertEqual(result["status"], "rejected")
        checks = {
            failure.get("check")
            for failure in result["failures"]  # type: ignore[union-attr]
        }
        self.assertIn("kernel_affinity_entries_valid", checks)
        self.assertIn("kernel_affinity_policy_satisfied", checks)

    def test_isolation_rejects_kernel_affinity_hash_and_summary_tampering(
        self,
    ) -> None:
        for mutation, expected_check in (
            (
                lambda evidence: evidence.__setitem__(
                    "kernel_affinity_entries_sha256", "0" * 64
                ),
                "kernel_affinity_entries_bound",
            ),
            (
                lambda evidence: evidence.__setitem__(
                    "kernel_affinity_write_failure_count", 1
                ),
                "kernel_affinity_summary_consistent",
            ),
        ):
            with self.subTest(check=expected_check):
                evidence = isolation_evidence()
                mutation(evidence)
                result = self.audit(
                    require_isolation_state=True, isolation_state=evidence
                )
                checks = {
                    failure.get("check")
                    for failure in result["failures"]  # type: ignore[union-attr]
                }
                self.assertIn(expected_check, checks)

    def test_isolation_rejects_runtime_kernel_affinity_drift(self) -> None:
        evidence = isolation_evidence()

        def drift(rows: list[dict[str, object]]) -> None:
            for row in rows:
                if row["launch_id"] == rows[0]["launch_id"]:
                    item = row["kernel_affinity"][  # type: ignore[index]
                        "/sys/devices/virtual/workqueue/test-wq/cpumask"
                    ]
                    item["raw"] = "d"
                    item["cpus"] = [0, 2, 3]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=drift,
        )

        self.assertEqual(result["status"], "rejected")
        self.assertIn("kernel-affinity-runtime-drift", self.reasons(result))

    def test_isolation_rejects_required_irq_migration_failure(self) -> None:
        evidence = isolation_evidence()
        entry = evidence["irq_affinity_entries"][0]  # type: ignore[index]
        entry["write_failed"] = True
        entry["write_error"] = {"status": 1, "message": "ordinary failure"}
        entry["classification"] = "violation"
        evidence["irq_affinity_attempt_failures"] = 1
        evidence["irq_affinity_write_failure_count"] = 1
        evidence["irq_affinity_failed_paths"] = [entry["path"]]
        evidence["irq_affinity_entries_sha256"] = hashlib.sha256(
            json.dumps(
                evidence["irq_affinity_entries"],
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
        ).hexdigest()

        result = self.audit(
            require_isolation_state=True, isolation_state=evidence
        )
        self.assertEqual(result["status"], "rejected")
        self.assertIn("isolation-state-check-failed", self.reasons(result))

    @staticmethod
    def add_residual_irq(evidence: dict[str, object]) -> None:
        entries = evidence["irq_affinity_entries"]  # type: ignore[assignment]
        entry = entries[0]
        entry["write_failed"] = True
        entry["write_error"] = {"status": 1, "message": "固定中断不可迁移"}
        entry["classification"] = "residual_unmigratable"
        entry["effective_raw"] = "0"
        entry["effective_cpus"] = [0]
        evidence["irq_affinity_attempt_failures"] = 1
        evidence["irq_affinity_write_failure_count"] = 1
        evidence["irq_affinity_failed_paths"] = [entry["path"]]
        evidence["irq_affinity_migrated_and_verified_count"] = 0
        evidence["irq_affinity_applied"] = False
        residual = {
            "irq": entry["irq"],
            "path": entry["path"],
            "actions": entry["actions"],
            "effective_cpus": entry["effective_cpus"],
            "write_error": entry["write_error"],
        }
        evidence["irq_affinity_residual_unmigratable_count"] = 1
        evidence["irq_affinity_residual_unmigratable"] = [residual]
        evidence["irq_affinity_residual_unmigratable_sha256"] = hashlib.sha256(
            json.dumps([residual], sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
        evidence["irq_residual_requires_zero_external_interrupts"] = True
        evidence["irq_affinity_entries_sha256"] = hashlib.sha256(
            json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()

    def test_residual_irq_with_zero_window_interrupts_is_accepted(self) -> None:
        evidence = isolation_evidence()
        self.add_residual_irq(evidence)

        def zero_external(rows: list[dict[str, object]]) -> None:
            for row in rows:
                interrupts = row["cpu"]["0"]["interrupts"]  # type: ignore[index]
                interrupts["external"] = 100
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=zero_external,
        )
        self.assertEqual(result["status"], "accepted")

    def test_residual_irq_uses_readback_when_write_reports_success(self) -> None:
        evidence = isolation_evidence()
        self.add_residual_irq(evidence)
        entry = evidence["irq_affinity_entries"][0]  # type: ignore[index]
        entry["write_failed"] = False
        entry["write_error"] = None
        evidence["irq_affinity_attempt_failures"] = 0
        evidence["irq_affinity_write_failure_count"] = 0
        evidence["irq_affinity_failed_paths"] = []
        residuals = evidence["irq_affinity_residual_unmigratable"]
        residual = residuals[0]  # type: ignore[index]
        residual["write_error"] = None
        evidence["irq_affinity_entries_sha256"] = hashlib.sha256(
            json.dumps(
                evidence["irq_affinity_entries"],
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
        ).hexdigest()
        evidence["irq_affinity_residual_unmigratable_sha256"] = hashlib.sha256(
            json.dumps(
                evidence["irq_affinity_residual_unmigratable"],
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
        ).hexdigest()

        def zero_external(rows: list[dict[str, object]]) -> None:
            for row in rows:
                interrupts = row["cpu"]["0"]["interrupts"]  # type: ignore[index]
                interrupts["external"] = 100
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=zero_external,
        )
        self.assertEqual(result["status"], "accepted")

    def test_failed_irq_write_is_accepted_after_effective_migration(self) -> None:
        evidence = isolation_evidence()
        entry = evidence["irq_affinity_entries"][0]  # type: ignore[index]
        entry["write_failed"] = True
        entry["write_error"] = {"status": 1, "message": "fixed IRQ"}
        evidence["irq_affinity_attempt_failures"] = 1
        evidence["irq_affinity_write_failure_count"] = 1
        evidence["irq_affinity_failed_paths"] = [entry["path"]]
        evidence["irq_affinity_entries_sha256"] = hashlib.sha256(
            json.dumps(
                evidence["irq_affinity_entries"],
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
        ).hexdigest()

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )

        self.assertEqual(result["status"], "accepted")

    def test_isolator_pins_qemu_but_not_analysis_workers(self) -> None:
        script = (
            REPOSITORY / "scripts/run-riscv-weight-isolated.sh"
        ).read_text(encoding="utf-8")

        self.assertIn('-p "CPUAffinity=$background"', script)
        self.assertNotIn('-p "CPUAffinity=$orchestrator_cpu"', script)
        self.assertIn(
            '/bin/sh "$root/scripts/riscv-instruction-weight.sh" "$mode"',
            script,
        )

    def test_isolator_accepts_systemd_space_separated_cpu_lists(self) -> None:
        script = (
            REPOSITORY / "scripts/run-riscv-weight-isolated.sh"
        ).read_text(encoding="utf-8")

        self.assertGreaterEqual(
            script.count('spec.replace(",", " ").split()'), 4
        )

    def test_isolator_retries_transient_kernel_control_readback(self) -> None:
        script = (
            REPOSITORY / "scripts/run-riscv-weight-isolated.sh"
        ).read_text(encoding="utf-8")

        self.assertIn("error.errno not in {11, 16}", script)
        self.assertIn("time.sleep(0.02 * (attempt + 1))", script)

    def test_isolator_skips_cpufreq_readback_for_offline_sibling(self) -> None:
        script = (
            REPOSITORY / "scripts/run-riscv-weight-isolated.sh"
        ).read_text(encoding="utf-8")

        self.assertIn('if not sibling_states[str(cpu)]:', script)
        self.assertIn('frequency[str(cpu)] = None', script)
        self.assertIn('-E "RISCV_WEIGHT_CPUSET_MODE=taskset"', script)

    def test_isolator_requires_pinned_hardware_frequency_preflight(self) -> None:
        isolator = (
            REPOSITORY / "scripts/run-riscv-weight-isolated.sh"
        ).read_text(encoding="utf-8")
        runner = (
            REPOSITORY / "scripts/riscv-instruction-weight.sh"
        ).read_text(encoding="utf-8")

        self.assertIn('taskset -c "$cpuset" $host_telemetry_command', runner)
        self.assertIn("frequency-preflight --cpu", runner)
        self.assertIn("--minimum-aperf-mperf-ratio 0.95", runner)
        self.assertIn(
            "RISCV_WEIGHT_HOST_AUDIT_REQUIRE_FREQUENCY_PREFLIGHT=1", isolator
        )

    def test_isolator_classifies_residual_irq_from_affinity_readback(self) -> None:
        script = (
            REPOSITORY / "scripts/run-riscv-weight-isolated.sh"
        ).read_text(encoding="utf-8")
        residual_branch = script[
            script.index('elif entry["migration_required"]:') :
            script.index('elif entry["write_attempted"]:')
        ]

        self.assertIn("effective & physical_set", residual_branch)
        self.assertIn(
            'entry["classification"] = "residual_unmigratable"',
            residual_branch,
        )
        self.assertNotIn("Operation not permitted", residual_branch)
        self.assertNotIn(
            "migration-write-failure-without-active-residual", residual_branch
        )

    def test_residual_irq_with_one_window_interrupt_is_rejected(self) -> None:
        evidence = isolation_evidence()
        self.add_residual_irq(evidence)

        def one_external(rows: list[dict[str, object]]) -> None:
            launch = rows[0]["launch_id"]
            for row in rows:
                interrupts = row["cpu"]["0"]["interrupts"]  # type: ignore[index]
                interrupts["external"] = 100 + int(
                    row["launch_id"] == launch and row["phase"] == "after"
                )
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        result = self.audit(
            require_isolation_state=True,
            isolation_state=evidence,
            mutate_rows=one_external,
        )
        self.assertEqual(result["status"], "rejected")
        self.assertIn(
            "residual-irq-observed-on-selected-cpu", self.reasons(result)
        )

    def test_isolation_rejects_tampered_residual_irq_summary(self) -> None:
        evidence = isolation_evidence()
        self.add_residual_irq(evidence)
        residual = evidence["irq_affinity_residual_unmigratable"][0]  # type: ignore[index]
        residual["actions"] = "forged-action"

        result = self.audit(
            require_isolation_state=True, isolation_state=evidence
        )
        self.assertEqual(result["status"], "rejected")
        failures = result["failures"]
        self.assertTrue(
            any(
                failure.get("check") == "irq_affinity_residuals_bound"
                for failure in failures
            )
        )

    def test_isolation_rejects_effective_irq_on_measured_core(self) -> None:
        evidence = isolation_evidence()
        entry = evidence["irq_affinity_entries"][0]  # type: ignore[index]
        entry["effective_raw"] = "0,2"
        entry["effective_cpus"] = [0, 2]
        evidence["irq_affinity_violations"] = [
            {
                "path": entry["path"],
                "reasons": ["effective-includes-measured-physical-core"],
            }
        ]
        evidence["irq_affinity_readback_violation_count"] = 1
        evidence["irq_affinity_entries_sha256"] = hashlib.sha256(
            json.dumps(
                evidence["irq_affinity_entries"],
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
        ).hexdigest()

        result = self.audit(
            require_isolation_state=True, isolation_state=evidence
        )
        self.assertEqual(result["status"], "rejected")
        self.assertIn("isolation-state-check-failed", self.reasons(result))

    def test_isolation_rejects_tampered_irq_readback_hash(self) -> None:
        evidence = isolation_evidence()
        evidence["irq_affinity_entries_sha256"] = "0" * 64

        result = self.audit(
            require_isolation_state=True, isolation_state=evidence
        )
        self.assertEqual(result["status"], "rejected")
        failures = result["failures"]
        self.assertTrue(
            any(
                failure.get("check") == "irq_affinity_entries_bound"
                for failure in failures
            )
        )

    def test_launch_set_must_match_run_design_exactly(self) -> None:
        def remove_launch(rows: list[dict[str, object]]) -> None:
            launch = rows[0]["launch_id"]
            rows[:] = [row for row in rows if row["launch_id"] != launch]

        result = self.audit(mutate_rows=remove_launch)
        self.assertEqual(result["status"], "rejected")
        self.assertIn("missing-planned-launch", self.reasons(result))

        def add_launch(rows: list[dict[str, object]]) -> None:
            extra = copy.deepcopy(rows[:2])
            for row in extra:
                row["launch_id"] = "unplanned-launch"
            rows.extend(extra)

        result = self.audit(mutate_rows=add_launch)
        self.assertIn("unplanned-telemetry-launch", self.reasons(result))

    def test_run_design_must_encode_complete_crossover(self) -> None:
        def corrupt(rows: list[dict[str, object]]) -> None:
            rows[1]["timing_launch_position"] = 3

        with self.assertRaises(TELEMETRY.TelemetryError):
            self.audit(mutate_design=corrupt)

    def test_schema_and_metadata_must_remain_stable(self) -> None:
        def change_metadata(rows: list[dict[str, object]]) -> None:
            rows[1]["run_id"] = "changed-run"

        result = self.audit(mutate_rows=change_metadata)
        self.assertIn("snapshot-metadata-changed", self.reasons(result))

        def change_schema(rows: list[dict[str, object]]) -> None:
            rows[0]["schema"] = "unknown"
            rows[1]["schema"] = "unknown"

        result = self.audit(mutate_rows=change_schema)
        self.assertIn("unsupported-telemetry-schema", self.reasons(result))

    def test_exactly_one_selected_cpu_must_be_busy(self) -> None:
        def add_cpu(rows: list[dict[str, object]]) -> None:
            for row in rows[:2]:
                row["selected_cpus"] = [0, 1]

        result = self.audit(mutate_rows=add_cpu)
        self.assertIn("selected-cpu-count-not-one", self.reasons(result))

        def idle_selected(rows: list[dict[str, object]]) -> None:
            rows[1]["cpu"]["0"]["times"] = [100, 0, 0, 200, 0]  # type: ignore[index]

        result = self.audit(mutate_rows=idle_selected)
        self.assertIn("selected-cpu-not-busy", self.reasons(result))

    def test_busy_smt_sibling_rejects_measurement(self) -> None:
        result = self.audit(sibling_busy=30)
        self.assertEqual(result["status"], "rejected")
        self.assertIn("smt-sibling-interference", self.reasons(result))

    def test_offline_smt_sibling_is_accepted_as_zero_interference(self) -> None:
        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]
                row["cpu"]["1"]["times"] = None  # type: ignore[index]

        result = self.audit(mutate_rows=offline_sibling)

        self.assertEqual(result["status"], "accepted")
        self.assertTrue(
            all(
                launch["smt_sibling_busy_fraction"]["1"] == 0.0
                for launch in result["launches"]
            )
        )

    def test_frequency_and_governor_evidence_fail_closed(self) -> None:
        def remove_frequency(rows: list[dict[str, object]]) -> None:
            rows[0]["cpu"]["0"]["scaling_cur_freq"] = None  # type: ignore[index]

        result = self.audit(mutate_rows=remove_frequency)
        self.assertIn("cpu-frequency-unavailable", self.reasons(result))

        def remove_governor(rows: list[dict[str, object]]) -> None:
            rows[0]["cpu"]["0"]["governor"] = None  # type: ignore[index]
            rows[1]["cpu"]["0"]["governor"] = None  # type: ignore[index]

        result = self.audit(mutate_rows=remove_governor)
        self.assertIn("cpu-governor-unavailable", self.reasons(result))

        def change_sibling_metadata(rows: list[dict[str, object]]) -> None:
            rows[1]["cpu"]["1"]["scaling_max_freq"] = 4_000_000  # type: ignore[index]

        result = self.audit(mutate_rows=change_sibling_metadata)
        self.assertIn("cpu-frequency-metadata-changed", self.reasons(result))

    def test_boundary_frequency_floor_can_be_diagnostic_only(self) -> None:
        def low_boundary_frequency(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["0"]["scaling_cur_freq"] = 3_000_000  # type: ignore[index]

        rejected = self.audit(mutate_rows=low_boundary_frequency)
        self.assertIn("cpu-frequency-below-floor", self.reasons(rejected))
        diagnostic = self.audit(
            mutate_rows=low_boundary_frequency,
            require_frequency_floor=False,
        )
        self.assertEqual(diagnostic["status"], "accepted")
        self.assertLess(diagnostic["minimum_frequency_ratio"], 0.90)

    def test_window_frequency_uses_aperf_mperf_and_fails_closed(self) -> None:
        def slow_window(rows: list[dict[str, object]]) -> None:
            rows[1]["cpu"]["0"]["aperf"] = 2_500_000  # type: ignore[index]

        result = self.audit(mutate_rows=slow_window)
        self.assertIn("window-frequency-below-floor", self.reasons(result))

        def remove_counter(rows: list[dict[str, object]]) -> None:
            rows[0]["cpu"]["0"]["mperf"] = None  # type: ignore[index]

        result = self.audit(mutate_rows=remove_counter)
        reasons = self.reasons(result)
        self.assertIn("window-frequency-evidence-unavailable", reasons)
        self.assertIn("window-frequency-coverage-incomplete", reasons)

        diagnostic = self.audit(
            mutate_rows=remove_counter,
            require_window_frequency=False,
        )
        self.assertEqual(diagnostic["status"], "accepted")

    def test_frequency_preflight_is_bound_and_compared_to_every_window(self) -> None:
        evidence = isolation_evidence()

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        accepted = self.audit(
            require_isolation_state=True,
            require_frequency_preflight=True,
            isolation_state=evidence,
            mutate_rows=offline_sibling,
        )
        self.assertEqual(accepted["status"], "accepted")
        self.assertGreaterEqual(
            accepted["minimum_window_to_preflight_frequency_ratio"], 0.95
        )

        tampered = copy.deepcopy(evidence)
        preflight = tampered["frequency_preflight"]  # type: ignore[assignment]
        preflight["estimated_actual_mhz"] = 4500.0
        rejected = self.audit(
            require_isolation_state=True,
            require_frequency_preflight=True,
            isolation_state=tampered,
            mutate_rows=offline_sibling,
        )
        self.assertIn("isolation-state-check-failed", self.reasons(rejected))
        self.assertIn(
            "frequency-preflight-evidence-unavailable", self.reasons(rejected)
        )

    def test_frequency_preflight_rejects_stale_or_degraded_windows(self) -> None:
        evidence = isolation_evidence()

        def offline_sibling(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["cpu"]["1"]["online"] = False  # type: ignore[index]

        stale = copy.deepcopy(evidence)
        preflight = stale["frequency_preflight"]  # type: ignore[assignment]
        preflight["completed_monotonic_ns"] = 20_000_000_000
        preflight["started_monotonic_ns"] = 19_000_000_000
        stale["frequency_preflight_sha256"] = TELEMETRY._canonical_sha256(
            preflight
        )
        result = self.audit(
            require_isolation_state=True,
            require_frequency_preflight=True,
            isolation_state=stale,
            mutate_rows=offline_sibling,
        )
        self.assertIn("frequency-preflight-evidence-stale", self.reasons(result))

        degraded = copy.deepcopy(evidence)
        preflight = degraded["frequency_preflight"]  # type: ignore[assignment]
        preflight["counters"]["aperf"]["after"] = 3_960_000_002  # type: ignore[index]
        preflight["counters"]["aperf"]["delta"] = 3_960_000_000  # type: ignore[index]
        preflight["aperf_mperf_ratio"] = 1.2
        preflight["estimated_actual_mhz"] = 3960.0
        degraded["frequency_preflight_sha256"] = TELEMETRY._canonical_sha256(
            preflight
        )
        result = self.audit(
            require_isolation_state=True,
            require_frequency_preflight=True,
            isolation_state=degraded,
            mutate_rows=offline_sibling,
        )
        self.assertIn(
            "window-frequency-below-preflight-baseline", self.reasons(result)
        )

    def test_selected_cpu_interrupt_rate_is_a_window_gate(self) -> None:
        def interrupt_storm(rows: list[dict[str, object]]) -> None:
            rows[1]["cpu"]["0"]["interrupts"]["external"] = 200  # type: ignore[index]

        result = self.audit(mutate_rows=interrupt_storm)
        self.assertIn("interrupt-rate-too-high", self.reasons(result))

        def remove_interrupts(rows: list[dict[str, object]]) -> None:
            rows[0]["cpu"]["0"]["interrupts"] = None  # type: ignore[index]

        result = self.audit(mutate_rows=remove_interrupts)
        self.assertIn("interrupt-evidence-unavailable", self.reasons(result))

    def test_selected_cpu_runqueue_wait_is_a_window_gate(self) -> None:
        def excessive_wait(rows: list[dict[str, object]]) -> None:
            after = rows[1]["cpu"]["0"]["schedstat"]  # type: ignore[index]
            after["wait_ns"] = 110_000_000

        result = self.audit(mutate_rows=excessive_wait)
        self.assertIn("runqueue-wait-fraction-too-high", self.reasons(result))
        self.assertGreater(
            result["maximum_runqueue_wait_fraction"], 0.01  # type: ignore[operator]
        )

        def remove_schedstat(rows: list[dict[str, object]]) -> None:
            rows[0]["cpu"]["0"]["schedstat"] = None  # type: ignore[index]

        result = self.audit(mutate_rows=remove_schedstat)
        reasons = self.reasons(result)
        self.assertIn("schedstat-evidence-unavailable", reasons)
        self.assertIn("schedstat-coverage-incomplete", reasons)

        diagnostic = self.audit(
            mutate_rows=remove_schedstat,
            require_schedstat=False,
        )
        self.assertEqual(diagnostic["status"], "accepted")

    def test_psi_memory_and_temperature_thresholds_are_enforced(self) -> None:
        def overload(rows: list[dict[str, object]]) -> None:
            rows[1]["pressure_cpu"] = (
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=500000"
            )
            rows[1]["pressure_memory"] = (
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=500000"
            )
            rows[1]["mem_available_kib"] = 512 * 1024
            rows[1]["temperatures_c"] = {"coretemp:Core 0": 95.0}

        result = self.audit(mutate_rows=overload)
        reasons = self.reasons(result)
        self.assertIn("cpu-psi-too-high", reasons)
        self.assertIn("memory-psi-too-high", reasons)
        self.assertIn("memory-available-below-floor", reasons)
        self.assertIn("temperature-above-ceiling", reasons)
        self.assertIn("temperature-drift-too-high", reasons)

    def test_missing_psi_memory_and_temperature_evidence_is_rejected(self) -> None:
        def remove_evidence(rows: list[dict[str, object]]) -> None:
            rows[0]["pressure_cpu"] = None
            rows[0]["pressure_memory"] = None
            rows[0]["mem_available_kib"] = None
            rows[0]["temperatures_c"] = {}

        result = self.audit(mutate_rows=remove_evidence)
        reasons = self.reasons(result)
        self.assertIn("cpu-psi-unavailable", reasons)
        self.assertIn("memory-psi-unavailable", reasons)
        self.assertIn("memory-available-unavailable", reasons)
        self.assertIn("temperature-unavailable", reasons)

    def test_missing_optional_psi_is_recorded_without_rejection(self) -> None:
        def remove_psi(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["pressure_cpu"] = None
                row["pressure_memory"] = None

        result = self.audit(mutate_rows=remove_psi, require_psi=False)
        self.assertEqual(result["status"], "accepted")
        self.assertTrue(
            all(
                launch["cpu_psi_stall_fraction"] is None
                and launch["memory_psi_stall_fraction"] is None
                for launch in result["launches"]
            )
        )

    def test_temperature_drift_is_compared_within_each_sensor(self) -> None:
        def add_second_sensor(rows: list[dict[str, object]]) -> None:
            for row in rows:
                row["temperatures_c"] = {
                    "coretemp:Core 0": (
                        35.0 if row["phase"] == "before" else 36.0
                    ),
                    "coretemp:Package id 0": (
                        70.0 if row["phase"] == "before" else 71.0
                    ),
                }

        result = self.audit(mutate_rows=add_second_sensor)
        self.assertEqual(result["status"], "accepted")
        self.assertEqual(result["temperature_span_c"], 1.0)


if __name__ == "__main__":
    unittest.main()
