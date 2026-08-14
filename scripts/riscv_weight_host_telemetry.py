#!/usr/bin/env python3
"""采集并审计 RISC-V 指令权重实验的宿主干扰。"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import statistics
import time
from collections import Counter, defaultdict
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any


class TelemetryError(ValueError):
    pass


TELEMETRY_SCHEMA = "mygo.riscv-weight-host-telemetry.v1"
AUDIT_SCHEMA = "mygo.riscv-weight-host-audit.v1"
FREQUENCY_PREFLIGHT_SCHEMA = "mygo.riscv-weight-frequency-preflight.v1"
MSR_MPERF = 0xE7
MSR_APERF = 0xE8


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _jsonl(path: Path, owner: str) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise TelemetryError(
                f"{owner} 第 {line_number} 行不是合法 JSON"
            ) from error
        if not isinstance(row, dict):
            raise TelemetryError(f"{owner} 第 {line_number} 行必须是 object")
        rows.append(row)
    if not rows:
        raise TelemetryError(f"{owner} 不能为空")
    return rows


def _json_object(path: Path, owner: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise TelemetryError(f"{owner} 不是合法 JSON") from error
    if not isinstance(value, dict):
        raise TelemetryError(f"{owner} 必须是 object")
    return value


def _integer(value: Any, owner: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise TelemetryError(f"{owner} 必须是大于等于 {minimum} 的整数")
    return value


def _identifier(value: Any, owner: str) -> str:
    if not isinstance(value, str) or not value:
        raise TelemetryError(f"{owner} 必须是非空字符串")
    return value


def _finite(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    result = float(value)
    return result if math.isfinite(result) else None


def _frequency_preflight_evidence(
    value: Any, selected_cpus: set[int] | None
) -> dict[str, float | int] | None:
    """校验隔离 runner 生成的固定 CPU 满载 APERF/MPERF 基线。"""

    expected_fields = {
        "schema",
        "selected_cpu",
        "started_timestamp_ns",
        "completed_timestamp_ns",
        "started_monotonic_ns",
        "completed_monotonic_ns",
        "requested_duration_ns",
        "elapsed_ns",
        "process_cpu_ns",
        "iterations",
        "state_checksum",
        "counters",
        "aperf_mperf_ratio",
        "process_busy_fraction",
        "estimated_nominal_mhz",
        "estimated_actual_mhz",
        "thresholds",
        "failures",
        "passed",
    }
    if (
        not isinstance(value, Mapping)
        or set(value) != expected_fields
        or value.get("schema") != FREQUENCY_PREFLIGHT_SCHEMA
    ):
        return None
    if selected_cpus is None or len(selected_cpus) != 1:
        return None
    selected_cpu = next(iter(selected_cpus))
    if value.get("selected_cpu") != selected_cpu:
        return None
    checksum = value.get("state_checksum")
    if (
        not isinstance(checksum, str)
        or len(checksum) != 16
        or any(character not in "0123456789abcdef" for character in checksum)
    ):
        return None
    integer_fields = (
        "started_timestamp_ns",
        "completed_timestamp_ns",
        "started_monotonic_ns",
        "completed_monotonic_ns",
        "requested_duration_ns",
        "elapsed_ns",
        "process_cpu_ns",
        "iterations",
    )
    if any(
        isinstance(value.get(name), bool)
        or not isinstance(value.get(name), int)
        or value[name] <= 0
        for name in integer_fields
    ):
        return None
    if (
        value["completed_timestamp_ns"] <= value["started_timestamp_ns"]
        or value["completed_monotonic_ns"] <= value["started_monotonic_ns"]
        or value["elapsed_ns"]
        != value["completed_monotonic_ns"] - value["started_monotonic_ns"]
        or value["elapsed_ns"] < value["requested_duration_ns"]
    ):
        return None
    counters = value.get("counters")
    if not isinstance(counters, Mapping) or set(counters) != {"aperf", "mperf"}:
        return None
    deltas: dict[str, int] = {}
    for name in ("aperf", "mperf"):
        counter = counters.get(name)
        if not isinstance(counter, Mapping) or set(counter) != {
            "before",
            "after",
            "delta",
        }:
            return None
        if any(
            isinstance(counter.get(field), bool)
            or not isinstance(counter.get(field), int)
            or counter[field] < 0
            for field in ("before", "after", "delta")
        ):
            return None
        expected_delta = counter["after"] - counter["before"]
        if counter["delta"] != expected_delta or expected_delta <= 0:
            return None
        deltas[name] = expected_delta
    ratio = _finite(value.get("aperf_mperf_ratio"))
    busy = _finite(value.get("process_busy_fraction"))
    nominal_mhz = _finite(value.get("estimated_nominal_mhz"))
    actual_mhz = _finite(value.get("estimated_actual_mhz"))
    expected_ratio = deltas["aperf"] / deltas["mperf"]
    expected_busy = value["process_cpu_ns"] / value["elapsed_ns"]
    expected_nominal_mhz = deltas["mperf"] / value["process_cpu_ns"] * 1000.0
    expected_actual_mhz = deltas["aperf"] / value["process_cpu_ns"] * 1000.0
    if any(item is None or item <= 0.0 for item in (ratio, busy, nominal_mhz, actual_mhz)):
        return None
    if not all(
        math.isclose(observed, expected, rel_tol=1e-12, abs_tol=1e-12)
        for observed, expected in (
            (ratio, expected_ratio),
            (busy, expected_busy),
            (nominal_mhz, expected_nominal_mhz),
            (actual_mhz, expected_actual_mhz),
        )
    ):
        return None
    thresholds = value.get("thresholds")
    if thresholds != {
        "minimum_aperf_mperf_ratio": 0.95,
        "minimum_process_busy_fraction": 0.90,
    }:
        return None
    failures = value.get("failures")
    passed = (
        ratio >= 0.95
        and busy >= 0.90
        and busy <= 1.05
        and isinstance(failures, list)
        and not failures
        and value.get("passed") is True
    )
    if not passed:
        return None
    return {
        "selected_cpu": selected_cpu,
        "completed_timestamp_ns": value["completed_timestamp_ns"],
        "completed_monotonic_ns": value["completed_monotonic_ns"],
        "aperf_mperf_ratio": ratio,
        "process_busy_fraction": busy,
        "estimated_nominal_mhz": nominal_mhz,
        "estimated_actual_mhz": actual_mhz,
    }


def frequency_preflight(arguments: argparse.Namespace) -> int:
    """在所选 CPU 上运行短满载，采集正式测量前的硬件频率基线。"""

    cpu = _integer(arguments.cpu, "cpu")
    duration_seconds = _finite(arguments.duration_seconds)
    minimum_ratio = _finite(arguments.minimum_aperf_mperf_ratio)
    minimum_busy = _finite(arguments.minimum_process_busy_fraction)
    if duration_seconds is None or not 0.1 <= duration_seconds <= 10.0:
        raise TelemetryError("duration_seconds 必须位于 [0.1, 10.0]")
    if minimum_ratio is None or not 0.0 < minimum_ratio <= 1.0:
        raise TelemetryError("minimum_aperf_mperf_ratio 必须位于 (0, 1]")
    if minimum_busy is None or not 0.0 < minimum_busy <= 1.0:
        raise TelemetryError("minimum_process_busy_fraction 必须位于 (0, 1]")
    try:
        os.sched_setaffinity(0, {cpu})
    except (AttributeError, OSError) as error:
        raise TelemetryError(
            f"无法把频率预检固定到 CPU {cpu}: {error}"
        ) from error
    if os.sched_getaffinity(0) != {cpu}:
        raise TelemetryError("频率预检 CPU affinity 读回不精确")

    before_mperf = _read_msr(cpu, MSR_MPERF)
    before_aperf = _read_msr(cpu, MSR_APERF)
    started_timestamp_ns = time.time_ns()
    started_monotonic_ns = time.monotonic_ns()
    started_process_ns = time.process_time_ns()
    requested_duration_ns = int(duration_seconds * 1_000_000_000)
    deadline = started_monotonic_ns + requested_duration_ns
    iterations = 0
    state = 0x9E3779B97F4A7C15
    while time.monotonic_ns() < deadline:
        for _ in range(4096):
            state = (state * 6364136223846793005 + 1442695040888963407) & (
                (1 << 64) - 1
            )
        iterations += 4096
    completed_process_ns = time.process_time_ns()
    completed_monotonic_ns = time.monotonic_ns()
    completed_timestamp_ns = time.time_ns()
    after_mperf = _read_msr(cpu, MSR_MPERF)
    after_aperf = _read_msr(cpu, MSR_APERF)

    elapsed_ns = completed_monotonic_ns - started_monotonic_ns
    process_cpu_ns = completed_process_ns - started_process_ns
    failures: list[str] = []
    if None in {before_mperf, after_mperf, before_aperf, after_aperf}:
        failures.append("aperf-mperf-unavailable")
        delta_mperf = None
        delta_aperf = None
    else:
        assert before_mperf is not None and after_mperf is not None
        assert before_aperf is not None and after_aperf is not None
        # 64-bit APERF/MPERF 在一秒窗口内不会回绕；计数倒退视为失效，
        # 避免把 CPU reset 或错误读数按模运算放大为可信样本。
        delta_mperf = after_mperf - before_mperf
        delta_aperf = after_aperf - before_aperf
        if delta_mperf <= 0 or delta_aperf <= 0:
            failures.append("aperf-mperf-nonpositive-delta")
    ratio = (
        None
        if delta_mperf is None or delta_aperf is None or delta_mperf <= 0
        else delta_aperf / delta_mperf
    )
    busy_fraction = process_cpu_ns / elapsed_ns if elapsed_ns > 0 else None
    if ratio is None or ratio < minimum_ratio:
        failures.append("aperf-mperf-ratio-below-floor")
    if (
        busy_fraction is None
        or busy_fraction < minimum_busy
        or busy_fraction > 1.05
    ):
        failures.append("process-busy-fraction-out-of-range")
    estimated_nominal_mhz = (
        None
        if delta_mperf is None or process_cpu_ns <= 0
        else delta_mperf / process_cpu_ns * 1000.0
    )
    estimated_actual_mhz = (
        None
        if delta_aperf is None or process_cpu_ns <= 0
        else delta_aperf / process_cpu_ns * 1000.0
    )
    document = {
        "schema": FREQUENCY_PREFLIGHT_SCHEMA,
        "selected_cpu": cpu,
        "started_timestamp_ns": started_timestamp_ns,
        "completed_timestamp_ns": completed_timestamp_ns,
        "started_monotonic_ns": started_monotonic_ns,
        "completed_monotonic_ns": completed_monotonic_ns,
        "requested_duration_ns": requested_duration_ns,
        "elapsed_ns": elapsed_ns,
        "process_cpu_ns": process_cpu_ns,
        "iterations": iterations,
        "state_checksum": f"{state:016x}",
        "counters": {
            "mperf": {
                "before": before_mperf,
                "after": after_mperf,
                "delta": delta_mperf,
            },
            "aperf": {
                "before": before_aperf,
                "after": after_aperf,
                "delta": delta_aperf,
            },
        },
        "aperf_mperf_ratio": ratio,
        "process_busy_fraction": busy_fraction,
        "estimated_nominal_mhz": estimated_nominal_mhz,
        "estimated_actual_mhz": estimated_actual_mhz,
        "thresholds": {
            "minimum_aperf_mperf_ratio": minimum_ratio,
            "minimum_process_busy_fraction": minimum_busy,
        },
        "failures": failures,
        "passed": not failures,
    }
    output = Path(arguments.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    isolation_path_raw = getattr(arguments, "isolation_state", None)
    if not failures and isolation_path_raw:
        isolation_path = Path(str(isolation_path_raw))
        isolation = _json_object(isolation_path, "isolation-state")
        checks = isolation.get("preflight_checks")
        expected_checks = {
            "selected_cpu_online",
            "orchestrator_cpu_online",
            "smt_siblings_offline",
            "measurement_slice_active",
            "background_slices_applied",
            "frequency_policy_applied",
            "irq_isolation_policy_satisfied",
            "kernel_affinity_policy_satisfied",
        }
        if (
            isolation.get("schema") != "mygo.riscv-weight-host-isolation.v4"
            or isolation.get("selected_cpus") != [cpu]
            or not isinstance(checks, dict)
            or set(checks) != expected_checks
            or not all(value is True for value in checks.values())
        ):
            raise TelemetryError("频率预检不能附加到不完整的 v4 isolation-state")
        isolation["schema"] = "mygo.riscv-weight-host-isolation.v5"
        isolation["frequency_preflight"] = document
        isolation["frequency_preflight_sha256"] = _canonical_sha256(document)
        checks["hardware_frequency_preflight_passed"] = True
        isolation["active_during_measurement"] = all(checks.values())
        temporary = isolation_path.with_name(f".{isolation_path.name}.tmp")
        temporary.write_text(
            json.dumps(isolation, indent=2, sort_keys=True, allow_nan=False)
            + "\n",
            encoding="utf-8",
        )
        os.replace(temporary, isolation_path)
    return 0 if not failures else 1


def _threshold(arguments: argparse.Namespace, name: str, default: float) -> float:
    value = _finite(getattr(arguments, name, default))
    if value is None or value < 0.0:
        raise TelemetryError(f"{name} 必须是非负有限数")
    return value


def _read(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip()
    except (FileNotFoundError, PermissionError, OSError):
        return None


def _read_msr(cpu: int, register: int) -> int | None:
    """读取 x86 per-CPU MSR；非 x86 或权限不足时返回不可用。"""

    try:
        descriptor = os.open(f"/dev/cpu/{cpu}/msr", os.O_RDONLY)
    except OSError:
        return None
    try:
        raw = os.pread(descriptor, 8, register)
    except OSError:
        return None
    finally:
        os.close(descriptor)
    return int.from_bytes(raw, "little") if len(raw) == 8 else None


def _cpu_list(value: str) -> list[int]:
    result: set[int] = set()
    for part in value.replace(",", " ").split():
        bounds = part.split("-", 1)
        try:
            first = int(bounds[0])
            last = int(bounds[-1])
        except ValueError as error:
            raise TelemetryError(f"非法 CPU list: {value!r}") from error
        if first < 0 or last < first:
            raise TelemetryError(f"非法 CPU list: {value!r}")
        result.update(range(first, last + 1))
    if not result:
        raise TelemetryError("CPU list 不能为空")
    return sorted(result)


def _cpu_evidence_set(value: Any) -> set[int] | None:
    """解析审计证据中的展开 CPU 列表，不接受重复或隐式布尔值。"""

    if (
        not isinstance(value, list)
        or not value
        or any(
            isinstance(cpu, bool) or not isinstance(cpu, int) or cpu < 0
            for cpu in value
        )
    ):
        return None
    result = set(value)
    return result if len(result) == len(value) else None


def _cpu_evidence_set_allow_empty(value: Any) -> set[int] | None:
    """解析 IRQ effective CPU 证据；无活动目标时允许空列表。"""

    if value == []:
        return set()
    return _cpu_evidence_set(value)


def _cpu_spec_set(value: Any) -> set[int] | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        return set(_cpu_list(value))
    except TelemetryError:
        return None


def _cpu_spec_set_allow_empty(value: Any) -> set[int] | None:
    """解析 IRQ effective affinity；inactive IRQ 的内核读回可以为空。"""

    if value == "":
        return set()
    return _cpu_spec_set(value)


def _cpu_mask_set(value: Any) -> set[int] | None:
    if not isinstance(value, str) or not value:
        return None
    chunks = value.split(",")
    if any(not chunk or len(chunk) > 8 for chunk in chunks):
        return None
    result: set[int] = set()
    try:
        for chunk_index, chunk in enumerate(reversed(chunks)):
            bits = int(chunk, 16)
            if bits < 0:
                return None
            for bit in range(32):
                if bits & (1 << bit):
                    result.add(chunk_index * 32 + bit)
    except ValueError:
        return None
    return result or None


def _cpu_times() -> dict[str, list[int]]:
    text = _read(Path("/proc/stat"))
    if text is None:
        raise TelemetryError("无法读取 /proc/stat")
    result: dict[str, list[int]] = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 5 and fields[0].startswith("cpu") and fields[0][3:].isdigit():
            result[fields[0]] = [int(value) for value in fields[1:]]
    return result


def _schedstat() -> dict[str, dict[str, int]]:
    """读取每 CPU 调度运行/等待时间；字段定义来自 schedstat v15+。"""

    text = _read(Path("/proc/schedstat"))
    if not text:
        return {}
    result: dict[str, dict[str, int]] = {}
    for line in text.splitlines():
        fields = line.split()
        if (
            len(fields) < 10
            or not fields[0].startswith("cpu")
            or not fields[0][3:].isdigit()
        ):
            continue
        try:
            run_ns, wait_ns, timeslices = map(int, fields[7:10])
        except ValueError:
            continue
        if min(run_ns, wait_ns, timeslices) < 0:
            continue
        result[fields[0]] = {
            "run_ns": run_ns,
            "wait_ns": wait_ns,
            "timeslices": timeslices,
        }
    return result


def _interrupt_counts() -> dict[int, dict[str, int]]:
    text = _read(Path("/proc/interrupts"))
    if not text:
        return {}
    lines = text.splitlines()
    if not lines:
        return {}
    cpus = []
    for token in lines[0].split():
        if not token.startswith("CPU") or not token[3:].isdigit():
            return {}
        cpus.append(int(token[3:]))
    totals = {cpu: {"external": 0, "local": 0} for cpu in cpus}
    for line in lines[1:]:
        fields = line.split()
        if len(fields) < len(cpus) + 1 or not fields[0].endswith(":"):
            continue
        try:
            counts = [int(value) for value in fields[1 : len(cpus) + 1]]
        except ValueError:
            continue
        category = "external" if fields[0][:-1].isdigit() else "local"
        for cpu, count in zip(cpus, counts, strict=True):
            totals[cpu][category] += count
    return totals


def _temperatures(selected_cpus: Sequence[int]) -> dict[str, float]:
    result: dict[str, float] = {}
    thermal = Path("/sys/class/thermal")
    for path in sorted(thermal.glob("thermal_zone*/temp")):
        raw = _read(path)
        if raw is None:
            continue
        try:
            value = float(raw)
        except ValueError:
            continue
        if abs(value) > 1000.0:
            value /= 1000.0
        if math.isfinite(value):
            result[path.parent.name] = value
    for path in sorted(Path("/sys/class/hwmon").glob("hwmon*/temp*_input")):
        device_name = _read(path.parent / "name")
        if device_name not in {"coretemp", "k10temp", "zenpower", "cpu_thermal"}:
            continue
        raw = _read(path)
        if raw is None:
            continue
        try:
            value = float(raw)
        except ValueError:
            continue
        if abs(value) > 1000.0:
            value /= 1000.0
        if math.isfinite(value):
            label = _read(path.with_name(path.name.replace("_input", "_label")))
            allowed_labels = {"Package id 0"}
            for cpu in selected_cpus:
                core_id = _read(
                    Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/core_id")
                )
                if core_id is not None:
                    allowed_labels.add(f"Core {core_id}")
            if label is not None and label not in allowed_labels:
                continue
            sensor = label or path.stem.removesuffix("_input")
            result[f"{device_name}:{sensor}"] = value
    return result


def _kernel_affinity_snapshot() -> dict[str, dict[str, Any]]:
    root = Path("/sys/devices/virtual/workqueue")
    paths = set(root.glob("**/cpumask")) | {
        Path("/proc/sys/kernel/watchdog_cpumask")
    }
    result: dict[str, dict[str, Any]] = {}
    for path in sorted(paths, key=str):
        raw = _read(path)
        if raw is None:
            continue
        syntax = "list" if path == Path("/proc/sys/kernel/watchdog_cpumask") else "mask"
        parsed = _cpu_spec_set(raw) if syntax == "list" else _cpu_mask_set(raw)
        result[str(path)] = {
            "syntax": syntax,
            "raw": raw,
            "cpus": None if parsed is None else sorted(parsed),
        }
    return result


def snapshot(arguments: argparse.Namespace) -> int:
    selected = _cpu_list(arguments.cpuset)
    physical_override = getattr(arguments, "physical_core_cpuset", None)
    siblings: set[int] = set(
        selected
        if not physical_override
        else _cpu_list(str(physical_override))
    )
    if not set(selected).issubset(siblings):
        raise TelemetryError("physical-core-cpuset 必须包含 selected cpuset")
    cpu_metadata: dict[str, dict[str, Any]] = {}
    for cpu in selected:
        sibling_raw = _read(
            Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
        )
        if sibling_raw:
            siblings.update(_cpu_list(sibling_raw))
    selected_core_labels: set[str] = set()
    for cpu in selected:
        core_id = _read(
            Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/core_id")
        )
        if core_id is not None:
            selected_core_labels.add(f"coretemp:Core {core_id}")
    times = _cpu_times()
    schedstat = _schedstat()
    interrupts = _interrupt_counts()
    for cpu in sorted(siblings):
        base = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
        metadata: dict[str, Any] = {
            "times": times.get(f"cpu{cpu}"),
            "schedstat": schedstat.get(f"cpu{cpu}"),
            "interrupts": interrupts.get(cpu),
            "online": (
                True
                if cpu == 0
                else _read(Path(f"/sys/devices/system/cpu/cpu{cpu}/online"))
                == "1"
            ),
            "governor": _read(base / "scaling_governor"),
            "mperf": _read_msr(cpu, MSR_MPERF),
            "aperf": _read_msr(cpu, MSR_APERF),
        }
        for name in ("scaling_cur_freq", "scaling_min_freq", "scaling_max_freq"):
            raw = _read(base / name)
            try:
                metadata[name] = None if raw is None else int(raw)
            except ValueError:
                metadata[name] = None
        cpu_metadata[str(cpu)] = metadata
    load = os.getloadavg()
    document = {
        "schema": TELEMETRY_SCHEMA,
        "timestamp_ns": time.time_ns(),
        "monotonic_ns": time.monotonic_ns(),
        "phase": arguments.phase,
        "launch_id": arguments.launch_id,
        "super_run_id": arguments.super_run_id,
        "run_id": arguments.run_id,
        "mode": arguments.mode,
        "launch_position": arguments.launch_position,
        "selected_cpus": selected,
        "physical_core_cpus": sorted(siblings),
        "selected_core_temperature_sensors": sorted(selected_core_labels),
        "kernel_affinity": _kernel_affinity_snapshot(),
        "cpu": cpu_metadata,
        "load_average": list(load),
        "load_per_online_cpu": load[0] / max(1, os.cpu_count() or 1),
        "pressure_cpu": _read(Path("/proc/pressure/cpu")),
        "pressure_memory": _read(Path("/proc/pressure/memory")),
        "mem_available_kib": next(
            (
                int(line.split()[1])
                for line in (_read(Path("/proc/meminfo")) or "").splitlines()
                if line.startswith("MemAvailable:")
            ),
            None,
        ),
        "temperatures_c": _temperatures(selected),
    }
    output = Path(arguments.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(document, sort_keys=True, separators=(",", ":")) + "\n")
    return 0


def _busy_fraction(before: list[int], after: list[int]) -> float | None:
    if len(before) < 4 or len(after) < 4:
        return None
    width = min(len(before), len(after))
    deltas = [after[index] - before[index] for index in range(width)]
    if any(delta < 0 for delta in deltas):
        return None
    total = sum(deltas)
    if total <= 0:
        return None
    idle = deltas[3] + (deltas[4] if width > 4 else 0)
    return max(0.0, min(1.0, (total - idle) / total))


def _pressure_total(value: Any) -> int | None:
    if not isinstance(value, str) or not value.strip():
        return None
    totals: list[int] = []
    for line in value.splitlines():
        fields = line.split()
        if not fields or fields[0] not in {"some", "full"}:
            return None
        values = dict(
            field.split("=", 1)
            for field in fields[1:]
            if "=" in field
        )
        try:
            total = int(values["total"])
        except (KeyError, ValueError):
            return None
        if total < 0:
            return None
        totals.append(total)
    return max(totals) if totals else None


def _expected_launches(
    rows: Sequence[Mapping[str, Any]],
) -> dict[str, dict[str, Any]]:
    expected: dict[str, dict[str, Any]] = {}
    by_super: dict[str, list[tuple[int, int, str, str, int, int]]] = defaultdict(list)
    run_ids: set[str] = set()
    run_orders: set[int] = set()
    for index, row in enumerate(rows, start=1):
        owner = f"run-design 第 {index} 行"
        run_id = _identifier(row.get("run_id"), f"{owner}.run_id")
        super_run_id = _identifier(
            row.get("super_run_id"), f"{owner}.super_run_id"
        )
        run_order = _integer(row.get("run_order"), f"{owner}.run_order")
        super_order = _integer(
            row.get("super_run_order"), f"{owner}.super_run_order"
        )
        pair = _integer(row.get("crossover_pair"), f"{owner}.crossover_pair", minimum=1)
        design = row.get("crossover_design")
        if design not in {"ABBA", "BAAB"}:
            raise TelemetryError(f"{owner}.crossover_design 必须是 ABBA 或 BAAB")
        timing = _integer(
            row.get("timing_launch_position"),
            f"{owner}.timing_launch_position",
            minimum=1,
        )
        off = _integer(
            row.get("plugin_off_launch_position"),
            f"{owner}.plugin_off_launch_position",
            minimum=1,
        )
        if run_id in run_ids or run_order in run_orders:
            raise TelemetryError("run-design 的 run_id/run_order 必须唯一")
        run_ids.add(run_id)
        run_orders.add(run_order)
        by_super[super_run_id].append(
            (super_order, pair, design, run_id, timing, off)
        )
        for mode, position in (("timing", timing), ("plugin-off", off)):
            launch_id = f"{super_run_id}-{position}-{mode}"
            if launch_id in expected:
                raise TelemetryError(f"run-design 重复 launch_id={launch_id!r}")
            expected[launch_id] = {
                "launch_id": launch_id,
                "super_run_id": super_run_id,
                "run_id": run_id,
                "mode": mode,
                "launch_position": position,
                "crossover_pair": pair,
                "crossover_design": design,
                "super_run_order": super_order,
                "run_order": run_order,
            }
    if sorted(run_orders) != list(range(len(run_orders))):
        raise TelemetryError("run-design 的 run_order 必须从 0 连续递增")
    super_orders: dict[str, int] = {}
    for super_run_id, members in by_super.items():
        orders = {member[0] for member in members}
        designs = {member[2] for member in members}
        pairs = {member[1] for member in members}
        if len(orders) != 1 or len(designs) != 1 or pairs != {1, 2}:
            raise TelemetryError(
                f"super-run={super_run_id!r} 必须恰含同设计的 crossover pair 1/2"
            )
        super_orders[super_run_id] = next(iter(orders))
        design = next(iter(designs))
        positions = Counter(
            (mode, position)
            for _order, _pair, _design, _run, timing, off in members
            for mode, position in (("timing", timing), ("plugin-off", off))
        )
        expected_modes = (
            {("timing", 1), ("plugin-off", 2), ("plugin-off", 3), ("timing", 4)}
            if design == "ABBA"
            else {("plugin-off", 1), ("timing", 2), ("timing", 3), ("plugin-off", 4)}
        )
        if set(positions) != expected_modes or any(count != 1 for count in positions.values()):
            raise TelemetryError(
                f"super-run={super_run_id!r} 的 launch 不符合 {design} 设计"
            )
    if sorted(super_orders.values()) != list(range(len(super_orders))):
        raise TelemetryError("run-design 的 super_run_order 必须从 0 连续递增")
    return expected


def _failure(
    failures: list[dict[str, Any]], reason: str, launch_id: str | None = None, **details: Any
) -> None:
    item: dict[str, Any] = {"reason": reason}
    if launch_id is not None:
        item["launch_id"] = launch_id
    item.update(details)
    failures.append(item)


def _canonical_sha256(value: Any) -> str:
    payload = json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def verify_binding(arguments: argparse.Namespace) -> int:
    """验证 audit 只为本次 run-design/telemetry 提供发布证据。"""

    audit_path = Path(arguments.audit)
    input_path = Path(arguments.input)
    design_path = Path(arguments.run_design)
    output_path = Path(arguments.output)
    source = str(arguments.source)
    if source not in {"current", "external"}:
        raise TelemetryError("binding source 必须是 current 或 external")

    audit_document = _json_object(audit_path, "host audit")
    design_rows = _jsonl(design_path, "current run-design")
    expected = _expected_launches(design_rows)
    audit_inputs = audit_document.get("inputs")
    audit_inputs = audit_inputs if isinstance(audit_inputs, Mapping) else {}

    def identity_matches(name: str, path: Path) -> bool:
        identity = audit_inputs.get(name)
        return (
            isinstance(identity, Mapping)
            and identity.get("sha256") == _sha256(path)
            and isinstance(identity.get("path"), str)
            and Path(str(identity["path"])).resolve() == path.resolve()
        )

    telemetry_launches: dict[str, set[str]] = defaultdict(set)
    telemetry_unique = True
    if source == "current":
        for index, row in enumerate(_jsonl(input_path, "current host telemetry")):
            launch_id = row.get("launch_id")
            phase = row.get("phase")
            if not isinstance(launch_id, str) or phase not in {"before", "after"}:
                raise TelemetryError(
                    f"current host telemetry[{index}] 缺少合法 launch_id/phase"
                )
            if phase in telemetry_launches[launch_id]:
                telemetry_unique = False
            telemetry_launches[launch_id].add(phase)
    telemetry_complete = (
        source == "current"
        and telemetry_unique
        and set(telemetry_launches) == set(expected)
        and all(phases == {"before", "after"} for phases in telemetry_launches.values())
    )

    audit_launch_rows = audit_document.get("launches")
    audit_launches: dict[str, Mapping[str, Any]] = {}
    audit_launches_unique = isinstance(audit_launch_rows, list)
    if isinstance(audit_launch_rows, list):
        for row in audit_launch_rows:
            if not isinstance(row, Mapping) or not isinstance(
                row.get("launch_id"), str
            ):
                audit_launches_unique = False
                continue
            launch_id = str(row["launch_id"])
            if launch_id in audit_launches:
                audit_launches_unique = False
            audit_launches[launch_id] = row
    audit_manifest_matches = (
        audit_launches_unique
        and set(audit_launches) == set(expected)
        and all(
            audit_launches[launch_id].get("run_design") == plan
            for launch_id, plan in expected.items()
        )
    )
    expected_count = len(expected)
    audit_counts_match = all(
        audit_document.get(name) == expected_count
        for name in ("planned_launches", "observed_launches", "complete_launches")
    )
    audit_failures = audit_document.get("failures")
    checks = {
        "source_is_current": source == "current",
        "audit_schema_supported": audit_document.get("schema") == AUDIT_SCHEMA,
        "audit_status_accepted": audit_document.get("status") == "accepted",
        "audit_failures_empty": isinstance(audit_failures, list)
        and not audit_failures,
        "telemetry_identity_matches_current": identity_matches(
            "telemetry", input_path
        ),
        "run_design_identity_matches_current": identity_matches(
            "run_design", design_path
        ),
        "current_telemetry_launch_set_complete": telemetry_complete,
        "audit_launch_manifest_matches_current": audit_manifest_matches,
        "audit_launch_counts_match_current": audit_counts_match,
    }
    reason_by_check = {
        "source_is_current": "external-host-audit-not-publishable",
        "audit_schema_supported": "host-audit-schema-mismatch",
        "audit_status_accepted": "host-audit-not-accepted",
        "audit_failures_empty": "host-audit-failures-not-empty",
        "telemetry_identity_matches_current": "host-audit-telemetry-binding-mismatch",
        "run_design_identity_matches_current": "host-audit-run-design-binding-mismatch",
        "current_telemetry_launch_set_complete": "current-telemetry-launch-set-incomplete",
        "audit_launch_manifest_matches_current": "host-audit-launch-manifest-mismatch",
        "audit_launch_counts_match_current": "host-audit-launch-count-mismatch",
    }
    failures = [
        {"reason": reason_by_check[name], "check": name}
        for name, passed in checks.items()
        if not passed
    ]
    publication_allowed = all(checks.values())
    result = {
        "schema": "mygo.riscv-weight-host-audit-binding.v1",
        "source": source,
        "publication_allowed": publication_allowed,
        "inputs": {
            "audit": {"path": str(audit_path), "sha256": _sha256(audit_path)},
            "telemetry": {"path": str(input_path), "sha256": _sha256(input_path)},
            "run_design": {"path": str(design_path), "sha256": _sha256(design_path)},
        },
        "expected_launches": sorted(expected),
        "launch_manifest_sha256": _canonical_sha256(expected),
        "checks": checks,
        "failures": failures,
    }
    output_path.write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0 if publication_allowed else 1


def audit(arguments: argparse.Namespace) -> int:
    input_path = Path(arguments.input)
    design_path = Path(arguments.run_design)
    rows = _jsonl(input_path, "host telemetry")
    design_rows = _jsonl(design_path, "run-design")
    expected = _expected_launches(design_rows)
    isolation_path_raw = getattr(arguments, "isolation_state", None)
    isolation_path = (
        None if not isolation_path_raw else Path(str(isolation_path_raw))
    )
    isolation_state = (
        None
        if isolation_path is None
        else _json_object(isolation_path, "isolation-state")
    )
    require_isolation_state = bool(
        getattr(arguments, "require_isolation_state", False)
    )
    max_sibling_busy = _threshold(arguments, "max_sibling_busy", 0.10)
    max_load_per_cpu = _threshold(arguments, "max_load_per_cpu", 0.75)
    min_frequency_ratio = _threshold(arguments, "min_frequency_ratio", 0.90)
    max_temperature_span = _threshold(arguments, "max_temperature_span", 12.0)
    max_temperature = _threshold(arguments, "max_temperature", 90.0)
    min_selected_busy = _threshold(arguments, "min_selected_busy", 0.50)
    require_frequency_floor = bool(
        getattr(arguments, "require_frequency_floor", False)
    )
    require_window_frequency = bool(
        getattr(arguments, "require_window_frequency", False)
    )
    require_frequency_preflight = bool(
        getattr(arguments, "require_frequency_preflight", False)
    )
    if require_frequency_preflight and not require_isolation_state:
        raise TelemetryError("频率预检发布证据要求 isolation-state")
    min_window_frequency_ratio = _threshold(
        arguments, "min_window_frequency_ratio", 0.95
    )
    min_window_to_preflight_ratio = _threshold(
        arguments, "min_window_to_preflight_ratio", 0.95
    )
    max_frequency_preflight_age_seconds = _threshold(
        arguments, "max_frequency_preflight_age_seconds", 300.0
    )
    max_window_frequency_cv = _threshold(
        arguments, "max_window_frequency_cv", 0.03
    )
    max_interrupts_per_second = _threshold(
        arguments, "max_interrupts_per_second", 25.0
    )
    require_interrupts = bool(getattr(arguments, "require_interrupts", False))
    require_schedstat = bool(getattr(arguments, "require_schedstat", False))
    max_runqueue_wait_fraction = _threshold(
        arguments, "max_runqueue_wait_fraction", 0.01
    )
    max_cpu_psi = _threshold(arguments, "max_cpu_psi", 0.10)
    max_memory_psi = _threshold(arguments, "max_memory_psi", 0.02)
    require_psi = bool(getattr(arguments, "require_psi", False))
    min_mem_available_kib = _threshold(
        arguments, "min_mem_available_kib", 1_048_576.0
    )
    if min_frequency_ratio > 1.0:
        raise TelemetryError("min_frequency_ratio 不能大于 1")
    for name, value in (
        ("max_sibling_busy", max_sibling_busy),
        ("max_load_per_cpu", max_load_per_cpu),
        ("min_selected_busy", min_selected_busy),
        ("max_cpu_psi", max_cpu_psi),
        ("max_memory_psi", max_memory_psi),
        ("min_window_frequency_ratio", min_window_frequency_ratio),
        ("min_window_to_preflight_ratio", min_window_to_preflight_ratio),
        ("max_window_frequency_cv", max_window_frequency_cv),
        ("max_runqueue_wait_fraction", max_runqueue_wait_fraction),
    ):
        if value > 1.0:
            raise TelemetryError(f"{name} 不能大于 1")
    grouped: dict[str, dict[str, dict[str, Any]]] = {}
    for row in rows:
        launch_id = row.get("launch_id")
        phase = row.get("phase")
        if not isinstance(launch_id, str) or phase not in {"before", "after"}:
            raise TelemetryError("遥测行缺少合法 launch_id/phase")
        if phase in grouped.setdefault(launch_id, {}):
            raise TelemetryError(f"launch={launch_id!r} 重复 phase={phase}")
        grouped[launch_id][phase] = row
    failures: list[dict[str, Any]] = []
    launches: list[dict[str, Any]] = []
    frequencies: list[float] = []
    temperatures_by_sensor: dict[str, list[float]] = defaultdict(list)
    window_frequency_ratios: list[float] = []
    window_to_preflight_ratios: list[float] = []
    runqueue_wait_fractions: list[float] = []
    frequency_preflight_summary: dict[str, float | int] | None = None
    frequency_preflight_fresh = False
    isolation_online_states: dict[str, bool] | None = None
    residual_requires_zero_external_interrupts = False
    residual_irq_numbers: list[int] = []
    missing_launches = sorted(set(expected) - set(grouped))
    extra_launches = sorted(set(grouped) - set(expected))
    if require_isolation_state and isolation_state is None:
        _failure(failures, "isolation-state-unavailable")
    elif isolation_state is not None:
        selected_sets = {
            tuple(row.get("selected_cpus", [])) for row in rows
        }
        physical_sets = {
            tuple(row.get("physical_core_cpus", [])) for row in rows
        }
        expected_selected = isolation_state.get("selected_cpus")
        expected_physical = isolation_state.get("physical_core_cpus")
        background_slices = isolation_state.get("background_slices")
        requested_background = isolation_state.get("requested_background_cpus")
        kernel_affinity_entries = isolation_state.get("kernel_affinity_entries")
        kernel_affinity_canonical = (
            json.dumps(
                kernel_affinity_entries, sort_keys=True, separators=(",", ":")
            )
            if isinstance(kernel_affinity_entries, list)
            else None
        )
        kernel_affinity_hash = (
            hashlib.sha256(kernel_affinity_canonical.encode()).hexdigest()
            if kernel_affinity_canonical is not None
            else None
        )
        irq_entries = isolation_state.get("irq_affinity_entries")
        irq_entries_canonical = (
            json.dumps(irq_entries, sort_keys=True, separators=(",", ":"))
            if isinstance(irq_entries, list)
            else None
        )
        irq_entries_hash = (
            hashlib.sha256(irq_entries_canonical.encode()).hexdigest()
            if irq_entries_canonical is not None
            else None
        )
        residual_entries = isolation_state.get(
            "irq_affinity_residual_unmigratable"
        )
        residual_canonical = (
            json.dumps(residual_entries, sort_keys=True, separators=(",", ":"))
            if isinstance(residual_entries, list)
            else None
        )
        residual_hash = (
            hashlib.sha256(residual_canonical.encode()).hexdigest()
            if residual_canonical is not None
            else None
        )
        selected_set = _cpu_evidence_set(expected_selected)
        physical_set = _cpu_evidence_set(expected_physical)
        background_set = _cpu_evidence_set(requested_background)
        measurement_effective = _cpu_evidence_set(
            isolation_state.get("measurement_slice_effective_cpu_list")
        )
        measurement_effective_raw = _cpu_spec_set(
            isolation_state.get("measurement_slice_effective_cpus")
        )
        orchestrator = isolation_state.get("orchestrator_cpu")
        orchestrator_valid = (
            isinstance(orchestrator, int)
            and not isinstance(orchestrator, bool)
            and orchestrator >= 0
        )
        online_set = _cpu_evidence_set(isolation_state.get("online_cpus"))
        frequency_preflight_raw = isolation_state.get("frequency_preflight")
        frequency_preflight_summary = _frequency_preflight_evidence(
            frequency_preflight_raw, selected_set
        )
        frequency_preflight_hash = (
            _canonical_sha256(frequency_preflight_raw)
            if isinstance(frequency_preflight_raw, Mapping)
            else None
        )
        first_snapshot_monotonic_ns = min(
            (
                row["monotonic_ns"]
                for row in rows
                if isinstance(row.get("monotonic_ns"), int)
                and not isinstance(row.get("monotonic_ns"), bool)
            ),
            default=None,
        )
        if frequency_preflight_summary is not None:
            completed_ns = int(
                frequency_preflight_summary["completed_monotonic_ns"]
            )
            frequency_preflight_fresh = (
                first_snapshot_monotonic_ns is not None
                and completed_ns <= first_snapshot_monotonic_ns
                and first_snapshot_monotonic_ns - completed_ns
                <= max_frequency_preflight_age_seconds * 1_000_000_000
            )
        frequency_preflight_valid = (
            frequency_preflight_summary is not None
            and frequency_preflight_fresh
            and frequency_preflight_hash is not None
            and isolation_state.get("frequency_preflight_sha256")
            == frequency_preflight_hash
        )
        sibling_states_raw = isolation_state.get("smt_sibling_online_states")
        isolation_online_states = (
            {
                str(cpu): sibling_states_raw.get(str(cpu))
                for cpu in expected_physical
            }
            if isinstance(expected_physical, list)
            and isinstance(sibling_states_raw, Mapping)
            and set(sibling_states_raw) == {str(cpu) for cpu in expected_physical}
            and all(
                isinstance(sibling_states_raw.get(str(cpu)), bool)
                for cpu in expected_physical
            )
            else None
        )
        sibling_states_consistent = (
            isolation_online_states is not None
            and online_set is not None
            and all(
                isolation_online_states[str(cpu)] is (cpu in online_set)
                for cpu in expected_physical
            )
        )
        background_slices_valid = (
            isinstance(background_slices, Mapping)
            and set(background_slices)
            == {"system.slice", "user.slice", "machine.slice"}
            and background_set is not None
            and physical_set is not None
        )
        if background_slices_valid:
            for value in background_slices.values():
                if not isinstance(value, Mapping):
                    background_slices_valid = False
                    break
                effective = _cpu_evidence_set(value.get("effective_cpu_list"))
                effective_raw = _cpu_spec_set(value.get("effective_cpus"))
                valid = (
                    effective is not None
                    and effective_raw == effective
                    and effective.issubset(background_set)
                    and not bool(effective & physical_set)
                )
                if (
                    not valid
                    or value.get("is_subset_of_requested_background") is not valid
                    or value.get("excludes_physical_core") is not valid
                ):
                    background_slices_valid = False
                    break
        required_kernel_affinity = {
            "global-workqueue": "/sys/devices/virtual/workqueue/cpumask",
            "writeback-workqueue": (
                "/sys/devices/virtual/workqueue/writeback/cpumask"
            ),
            "watchdog": "/proc/sys/kernel/watchdog_cpumask",
        }
        kernel_affinity_paths: list[str] = []
        kernel_affinity_kinds: list[str] = []
        kernel_affinity_entries_valid = (
            isinstance(kernel_affinity_entries, list)
            and bool(kernel_affinity_entries)
            and background_set is not None
            and physical_set is not None
        )
        if kernel_affinity_entries_valid:
            for entry in kernel_affinity_entries:
                if not isinstance(entry, Mapping):
                    kernel_affinity_entries_valid = False
                    break
                kind = entry.get("kind")
                path = entry.get("path")
                syntax = entry.get("syntax")
                parser = _cpu_mask_set if syntax == "mask" else _cpu_spec_set
                initial = parser(entry.get("initial_raw"))
                requested = parser(entry.get("requested_raw"))
                readback = parser(entry.get("readback_raw"))
                valid_path = (
                    isinstance(path, str)
                    and (
                        (kind == "global-workqueue" and path == required_kernel_affinity[kind])
                        or (kind == "writeback-workqueue" and path == required_kernel_affinity[kind])
                        or (kind == "watchdog" and path == required_kernel_affinity[kind])
                        or (
                            kind == "named-workqueue"
                            and path.startswith("/sys/devices/virtual/workqueue/")
                            and path.endswith("/cpumask")
                            and path not in required_kernel_affinity.values()
                        )
                    )
                )
                valid = (
                    kind
                    in {
                        "global-workqueue",
                        "writeback-workqueue",
                        "watchdog",
                        "named-workqueue",
                    }
                    and syntax == ("list" if kind == "watchdog" else "mask")
                    and valid_path
                    and initial is not None
                    and requested == background_set
                    and readback == background_set
                    and _cpu_evidence_set(entry.get("initial_cpus")) == initial
                    and _cpu_evidence_set(entry.get("requested_cpus")) == requested
                    and _cpu_evidence_set(entry.get("readback_cpus")) == readback
                    and entry.get("write_attempted") is True
                    and entry.get("write_failed") is False
                    and entry.get("write_error") is None
                    and entry.get("matches_requested") is True
                    and entry.get("excludes_physical_core") is True
                    and not bool(readback & physical_set)
                    and "readback_error" not in entry
                )
                if not valid:
                    kernel_affinity_entries_valid = False
                    break
                kernel_affinity_paths.append(path)
                kernel_affinity_kinds.append(str(kind))
        required_kinds_present = (
            set(required_kernel_affinity).issubset(kernel_affinity_kinds)
        )
        kernel_affinity_summary_consistent = (
            kernel_affinity_entries_valid
            and len(kernel_affinity_paths) == len(set(kernel_affinity_paths))
            and len(kernel_affinity_kinds)
            == isolation_state.get("kernel_affinity_observed_count")
            and isolation_state.get("kernel_affinity_write_failure_count") == 0
            and isolation_state.get("kernel_affinity_failed_paths") == []
            and isolation_state.get("kernel_affinity_required_kinds")
            == sorted(required_kernel_affinity)
            and required_kinds_present
        )
        kernel_affinity_policy_satisfied = (
            kernel_affinity_summary_consistent
            and kernel_affinity_hash is not None
            and isolation_state.get("kernel_affinity_entries_sha256")
            == kernel_affinity_hash
        )
        frequency = isolation_state.get("frequency")
        frequency_valid = (
            isinstance(frequency, Mapping)
            and physical_set is not None
            and isolation_online_states is not None
            and set(frequency) == {str(cpu) for cpu in physical_set}
        )
        if frequency_valid:
            for cpu in physical_set:
                if not isolation_online_states[str(cpu)]:
                    continue
                row = frequency.get(str(cpu))
                if not isinstance(row, Mapping):
                    frequency_valid = False
                    break
                maximum = row.get("cpuinfo_maximum")
                if (
                    row.get("governor") != "performance"
                    or isinstance(maximum, bool)
                    or not isinstance(maximum, int)
                    or maximum <= 0
                    or row.get("minimum") != maximum
                    or row.get("maximum") != maximum
                ):
                    frequency_valid = False
                    break
        irq_entry_sets = []
        if isinstance(irq_entries, list):
            for entry in irq_entries:
                if isinstance(entry, Mapping):
                    irq_entry_sets.append(
                        (
                            _cpu_evidence_set_allow_empty(
                                entry.get("initial_effective_cpus")
                            ),
                            _cpu_evidence_set_allow_empty(
                                entry.get("effective_cpus")
                            ),
                        )
                    )
                else:
                    irq_entry_sets.append((None, None))
        irq_numbers = [
            entry.get("irq")
            for entry in irq_entries or []
            if isinstance(entry, Mapping)
        ]
        attempted_paths_raw = isolation_state.get("irq_affinity_attempted_paths")
        failed_paths_raw = isolation_state.get("irq_affinity_failed_paths")
        attempted_paths_valid = (
            isinstance(attempted_paths_raw, list)
            and all(isinstance(path, str) and path for path in attempted_paths_raw)
            and len(set(attempted_paths_raw)) == len(attempted_paths_raw)
        )
        failed_paths_valid = (
            isinstance(failed_paths_raw, list)
            and all(isinstance(path, str) and path for path in failed_paths_raw)
            and len(set(failed_paths_raw)) == len(failed_paths_raw)
            and attempted_paths_valid
            and set(failed_paths_raw).issubset(set(attempted_paths_raw))
        )
        attempted_paths = attempted_paths_raw if attempted_paths_valid else []
        failed_paths = failed_paths_raw if failed_paths_valid else []
        irq_entries_valid = (
            isinstance(irq_entries, list)
            and bool(irq_entries)
            and physical_set is not None
            and background_set is not None
            and len(irq_numbers) == len(irq_entries)
            and len(set(irq_numbers)) == len(irq_numbers)
            and all(
                isinstance(entry, Mapping)
                and isinstance(entry.get("irq"), int)
                and not isinstance(entry.get("irq"), bool)
                and entry.get("irq") >= 0
                and entry.get("path")
                == f"/proc/irq/{entry.get('irq')}/smp_affinity_list"
                and entry.get("effective_path")
                == f"/proc/irq/{entry.get('irq')}/effective_affinity_list"
                and _cpu_spec_set(entry.get("requested_raw"))
                == _cpu_evidence_set(entry.get("requested_cpus"))
                and isinstance(entry.get("appeared_after_plan"), bool)
                and isinstance(entry.get("migration_required"), bool)
                and isinstance(entry.get("write_attempted"), bool)
                and isinstance(entry.get("write_failed"), bool)
                and (
                    (
                        entry.get("appeared_after_plan") is True
                        and initial is None
                        and entry.get("migration_required") is False
                    )
                    or (
                        entry.get("appeared_after_plan") is False
                        and initial is not None
                        and entry.get("migration_required")
                        is bool(initial & physical_set)
                    )
                )
                and (
                    entry.get("write_attempted")
                    is entry.get("migration_required")
                )
                and effective is not None
                and "readback_error" not in entry
                and (
                    (
                        entry.get("classification") == "already_excluded"
                        and entry.get("migration_required") is False
                        and entry.get("write_attempted") is False
                        and entry.get("write_failed") is False
                        and not bool(effective & physical_set)
                        and _cpu_spec_set_allow_empty(
                            entry.get("initial_effective_raw")
                        )
                        == initial
                        and _cpu_spec_set_allow_empty(entry.get("effective_raw"))
                        == effective
                    )
                    or (
                        entry.get("classification") == "migrated_and_verified"
                        and entry.get("migration_required") is True
                        and entry.get("write_attempted") is True
                        and not bool(effective & physical_set)
                        and (
                            (
                                entry.get("write_failed") is False
                                and entry.get("write_error") is None
                            )
                            or (
                                entry.get("write_failed") is True
                                and isinstance(entry.get("write_error"), Mapping)
                                and isinstance(
                                    entry.get("write_error", {}).get("status"),
                                    int,
                                )
                                and not isinstance(
                                    entry.get("write_error", {}).get("status"),
                                    bool,
                                )
                                and entry.get("write_error", {}).get("status") > 0
                                and isinstance(
                                    entry.get("write_error", {}).get("message"),
                                    str,
                                )
                            )
                        )
                        and _cpu_spec_set_allow_empty(
                            entry.get("initial_effective_raw")
                        )
                        == initial
                        and _cpu_spec_set_allow_empty(entry.get("effective_raw"))
                        == effective
                    )
                    or (
                        entry.get("classification") == "residual_unmigratable"
                        and entry.get("migration_required") is True
                        and entry.get("write_attempted") is True
                        and bool(effective & physical_set)
                        and (
                            (
                                entry.get("write_failed") is True
                                and isinstance(entry.get("write_error"), Mapping)
                                and isinstance(
                                    entry.get("write_error", {}).get("status"),
                                    int,
                                )
                                and not isinstance(
                                    entry.get("write_error", {}).get("status"),
                                    bool,
                                )
                                and entry.get("write_error", {}).get("status")
                                > 0
                                and isinstance(
                                    entry.get("write_error", {}).get("message"),
                                    str,
                                )
                            )
                            or (
                                entry.get("write_failed") is False
                                and entry.get("write_error") is None
                            )
                        )
                        and isinstance(entry.get("actions"), str)
                        and bool(entry.get("actions"))
                        and _cpu_spec_set_allow_empty(
                            entry.get("initial_effective_raw")
                        )
                        == initial
                        and _cpu_spec_set_allow_empty(entry.get("effective_raw"))
                        == effective
                    )
                    or (
                        entry.get("classification") == "inactive_no_target"
                        and entry.get("migration_required") is False
                        and entry.get("write_attempted") is False
                        and entry.get("write_failed") is False
                        and entry.get("write_error") is None
                        and entry.get("actions") == ""
                        and initial == set()
                        and effective == set()
                        and _cpu_spec_set_allow_empty(
                            entry.get("initial_effective_raw")
                        )
                        == initial
                        and _cpu_spec_set_allow_empty(entry.get("effective_raw"))
                        == effective
                    )
                )
                for entry, (initial, effective) in zip(
                    irq_entries, irq_entry_sets, strict=True
                )
            )
        )
        derived_residuals = [
            {
                "irq": entry.get("irq"),
                "path": entry.get("path"),
                "actions": entry.get("actions"),
                "effective_cpus": entry.get("effective_cpus"),
                "write_error": entry.get("write_error"),
            }
            for entry in irq_entries or []
            if isinstance(entry, Mapping)
            and entry.get("classification") == "residual_unmigratable"
        ]
        residuals_valid = (
            isinstance(residual_entries, list)
            and residual_entries == derived_residuals
            and isolation_state.get("irq_affinity_residual_unmigratable_count")
            == len(derived_residuals)
            and residual_hash is not None
            and isolation_state.get(
                "irq_affinity_residual_unmigratable_sha256"
            )
            == residual_hash
        )
        residual_irq_numbers = [
            int(entry["irq"])
            for entry in derived_residuals
            if isinstance(entry.get("irq"), int)
            and not isinstance(entry.get("irq"), bool)
        ]
        residual_requires_zero_external_interrupts = bool(derived_residuals)
        expected_preflight_checks = {
            "selected_cpu_online",
            "orchestrator_cpu_online",
            "smt_siblings_offline",
            "measurement_slice_active",
            "background_slices_applied",
            "frequency_policy_applied",
            "hardware_frequency_preflight_passed",
            "irq_isolation_policy_satisfied",
            "kernel_affinity_policy_satisfied",
        }
        preflight_checks = isolation_state.get("preflight_checks")
        selected_online = (
            selected_set is not None
            and len(selected_set) == 1
            and online_set is not None
            and selected_set.issubset(online_set)
        )
        orchestrator_online = (
            orchestrator_valid
            and online_set is not None
            and orchestrator in online_set
        )
        siblings_offline = (
            isolation_online_states is not None
            and selected_set is not None
            and all(
                cpu in selected_set or isolation_online_states[str(cpu)] is False
                for cpu in physical_set or set()
            )
        )
        measurement_slice_exact = (
            selected_set is not None
            and len(selected_set) == 1
            and measurement_effective is not None
            and measurement_effective_raw == measurement_effective
            and orchestrator_valid
            and background_set is not None
            and orchestrator in background_set
            and measurement_effective == selected_set | background_set
        )
        irq_write_evidence_consistent = (
            isinstance(irq_entries, list)
            and isolation_state.get("irq_affinity_attempt_failures")
            == len(failed_paths)
            + int(isolation_state.get("irq_affinity_default_write_failed") is True)
            and isolation_state.get("irq_affinity_write_failure_count")
            == len(failed_paths)
            and set(failed_paths)
            == {
                str(entry.get("path"))
                for entry in irq_entries
                if isinstance(entry, Mapping) and entry.get("write_failed") is True
            }
        )
        irq_readback_complete = (
            isinstance(irq_entries, list)
            and isolation_state.get("irq_affinity_initial_read_errors") == []
            and isolation_state.get("irq_affinity_readback_failures") == []
            and isolation_state.get("irq_affinity_violations") == []
            and isolation_state.get("irq_affinity_observed_count")
            == len(irq_entries)
            and isolation_state.get("irq_affinity_disappeared_after_plan") == []
            and isolation_state.get("irq_affinity_appeared_after_plan") == []
        )
        irq_policy_satisfied = (
            irq_write_evidence_consistent
            and isolation_state.get("irq_affinity_default_write_failed") is False
            and irq_readback_complete
            and irq_entries_valid
            and residuals_valid
            and isolation_state.get("irq_affinity_default_matches_requested")
            is True
        )
        recomputed_preflight_checks = {
            "selected_cpu_online": selected_online,
            "orchestrator_cpu_online": orchestrator_online,
            "smt_siblings_offline": siblings_offline,
            "measurement_slice_active": measurement_slice_exact,
            "background_slices_applied": background_slices_valid,
            "frequency_policy_applied": frequency_valid,
            "hardware_frequency_preflight_passed": frequency_preflight_valid,
            "irq_isolation_policy_satisfied": irq_policy_satisfied,
            "kernel_affinity_policy_satisfied": (
                kernel_affinity_policy_satisfied
            ),
        }
        checks = {
            "schema": isolation_state.get("schema")
            == "mygo.riscv-weight-host-isolation.v5",
            "active_during_measurement": isolation_state.get(
                "active_during_measurement"
            )
            is True,
            "selected_cpu_matches": isinstance(expected_selected, list)
            and selected_sets == {tuple(expected_selected)},
            "physical_core_matches": isinstance(expected_physical, list)
            and physical_sets == {tuple(expected_physical)},
            "online_state_readback_consistent": sibling_states_consistent,
            "smt_siblings_offline": siblings_offline
            and isolation_state.get("smt_siblings_offline") is True,
            "measurement_slice_active": isolation_state.get(
                "measurement_slice_active"
            )
            is True,
            "selected_cpu_online": selected_online
            and isolation_state.get("selected_cpu_online") is True,
            "orchestrator_cpu_online": isolation_state.get(
                "orchestrator_cpu_online"
            )
            is True
            and orchestrator_online,
            "measurement_slice_exact": measurement_slice_exact,
            "background_slices_exclude_physical_core": background_slices_valid,
            "frequency_policy_applied": isolation_state.get(
                "frequency_policy_applied"
            )
            is True
            and frequency_valid,
            "frequency_preflight_valid": frequency_preflight_valid,
            "kernel_affinity_entries_valid": kernel_affinity_entries_valid,
            "kernel_affinity_entries_bound": kernel_affinity_hash is not None
            and isolation_state.get("kernel_affinity_entries_sha256")
            == kernel_affinity_hash,
            "kernel_affinity_summary_consistent": (
                kernel_affinity_summary_consistent
            ),
            "kernel_affinity_policy_satisfied": isolation_state.get(
                "kernel_affinity_policy_satisfied"
            )
            is True
            and kernel_affinity_policy_satisfied,
            "irq_affinity_write_evidence_consistent": irq_write_evidence_consistent,
            "irq_affinity_readback_complete": irq_readback_complete,
            "irq_affinity_counts_consistent": isinstance(irq_entries, list)
            and attempted_paths_valid
            and failed_paths_valid
            and isolation_state.get("irq_affinity_planned_count")
            == sum(
                entry.get("appeared_after_plan") is False
                for entry in irq_entries
                if isinstance(entry, Mapping)
            )
            + len(isolation_state.get("irq_affinity_disappeared_after_plan", []))
            and isolation_state.get("irq_affinity_migration_required_count")
            == sum(
                entry.get("migration_required") is True
                for entry in irq_entries
                if isinstance(entry, Mapping)
            )
            + sum(
                path in set(attempted_paths)
                for path in isolation_state.get(
                    "irq_affinity_disappeared_after_plan", []
                )
            )
            and isolation_state.get("irq_affinity_write_attempt_count")
            == len(attempted_paths)
            and isolation_state.get("irq_affinity_write_failure_count")
            == len(failed_paths)
            and isolation_state.get("irq_affinity_readback_violation_count")
            == len(isolation_state.get("irq_affinity_violations", []))
            and isolation_state.get("irq_affinity_migrated_and_verified_count")
            == sum(
                entry.get("classification") == "migrated_and_verified"
                for entry in irq_entries
                if isinstance(entry, Mapping)
            )
            and isolation_state.get("irq_affinity_already_excluded_count")
            == sum(
                entry.get("classification") == "already_excluded"
                for entry in irq_entries
                if isinstance(entry, Mapping)
            )
            and isolation_state.get("irq_affinity_inactive_no_target_count")
            == sum(
                entry.get("classification") == "inactive_no_target"
                for entry in irq_entries
                if isinstance(entry, Mapping)
            )
            and isolation_state.get("irq_affinity_skipped_safe_count")
            == sum(
                entry.get("classification")
                in {"already_excluded", "inactive_no_target"}
                for entry in irq_entries
                if isinstance(entry, Mapping)
            ),
            "irq_affinity_entries_valid": irq_entries_valid,
            "irq_affinity_entries_bound": irq_entries_hash is not None
            and isolation_state.get("irq_affinity_entries_sha256")
            == irq_entries_hash,
            "irq_affinity_residuals_bound": residuals_valid,
            "irq_affinity_default_matches_requested": isolation_state.get(
                "irq_affinity_default_matches_requested"
            )
            is True
            and background_set is not None
            and _cpu_evidence_set(
                isolation_state.get("irq_affinity_default_effective_cpus")
            )
            == background_set
            and _cpu_mask_set(isolation_state.get("irq_affinity_default_raw"))
            == background_set,
            "irq_affinity_applied_consistent": isolation_state.get(
                "irq_affinity_applied"
            )
            is (irq_policy_satisfied and not residual_requires_zero_external_interrupts),
            "irq_isolation_policy_satisfied": isolation_state.get(
                "irq_isolation_policy_satisfied"
            )
            is True
            and irq_policy_satisfied,
            "irq_residual_policy_declared": isolation_state.get(
                "irq_residual_requires_zero_external_interrupts"
            )
            is residual_requires_zero_external_interrupts,
            "preflight_checks_passed": isinstance(preflight_checks, Mapping)
            and set(preflight_checks) == expected_preflight_checks
            and preflight_checks == recomputed_preflight_checks
            and all(recomputed_preflight_checks.values()),
            "restore_trap_armed": isolation_state.get("restore_trap_armed")
            is True,
        }
        for name, passed in checks.items():
            if not passed:
                _failure(
                    failures,
                    "isolation-state-check-failed",
                    check=name,
                )
    for launch_id in missing_launches:
        _failure(failures, "missing-planned-launch", launch_id)
    for launch_id in extra_launches:
        _failure(failures, "unplanned-telemetry-launch", launch_id)
    for launch_id, phases in sorted(grouped.items()):
        if set(phases) != {"before", "after"}:
            _failure(failures, "incomplete-snapshot-pair", launch_id)
            continue
        before, after = phases["before"], phases["after"]
        if before.get("schema") != TELEMETRY_SCHEMA or after.get("schema") != TELEMETRY_SCHEMA:
            _failure(failures, "unsupported-telemetry-schema", launch_id)
            continue
        immutable_fields = (
            "schema",
            "launch_id",
            "super_run_id",
            "run_id",
            "mode",
            "launch_position",
            "selected_cpus",
            "physical_core_cpus",
            "selected_core_temperature_sensors",
            "kernel_affinity",
        )
        changed = [
            field for field in immutable_fields if before.get(field) != after.get(field)
        ]
        if changed:
            _failure(
                failures,
                "snapshot-metadata-changed",
                launch_id,
                fields=changed,
            )
            continue
        plan = expected.get(launch_id)
        if plan is None:
            continue
        mismatches = [
            field
            for field in ("launch_id", "super_run_id", "run_id", "mode", "launch_position")
            if before.get(field) != plan[field]
        ]
        if mismatches:
            _failure(
                failures,
                "telemetry-run-design-mismatch",
                launch_id,
                fields=mismatches,
            )
            continue
        if isolation_state is not None:
            expected_kernel_paths = {
                str(entry.get("path"))
                for entry in isolation_state.get("kernel_affinity_entries", [])
                if isinstance(entry, Mapping)
            }
            observed_kernel = before.get("kernel_affinity")
            observed_kernel_valid = (
                isinstance(observed_kernel, Mapping)
                and set(observed_kernel) == expected_kernel_paths
                and background_set is not None
                and all(
                    isinstance(item, Mapping)
                    and _cpu_evidence_set(item.get("cpus")) == background_set
                    and (
                        _cpu_spec_set(item.get("raw"))
                        if item.get("syntax") == "list"
                        else _cpu_mask_set(item.get("raw"))
                    )
                    == background_set
                    for item in observed_kernel.values()
                )
            )
            if not observed_kernel_valid:
                _failure(
                    failures,
                    "kernel-affinity-runtime-drift",
                    launch_id,
                )
        selected_raw = before.get("selected_cpus")
        physical_raw = before.get("physical_core_cpus")
        if (
            not isinstance(selected_raw, list)
            or len(selected_raw) != 1
            or isinstance(selected_raw[0], bool)
            or not isinstance(selected_raw[0], int)
        ):
            _failure(failures, "selected-cpu-count-not-one", launch_id)
            continue
        if (
            not isinstance(physical_raw, list)
            or not physical_raw
            or any(isinstance(cpu, bool) or not isinstance(cpu, int) for cpu in physical_raw)
            or selected_raw[0] not in physical_raw
        ):
            _failure(failures, "invalid-physical-core-cpus", launch_id)
            continue
        selected = set(selected_raw)
        before_cpu = before.get("cpu")
        after_cpu = after.get("cpu")
        if not isinstance(before_cpu, dict) or not isinstance(after_cpu, dict):
            _failure(failures, "cpu-evidence-missing", launch_id)
            continue
        expected_cpu_keys = {str(cpu) for cpu in physical_raw}
        if set(before_cpu) != expected_cpu_keys or set(after_cpu) != expected_cpu_keys:
            _failure(
                failures,
                "cpu-metadata-set-changed",
                launch_id,
                expected=sorted(expected_cpu_keys),
                before=sorted(before_cpu),
                after=sorted(after_cpu),
            )
            continue
        sibling_busy: dict[str, float | None] = {}
        selected_busy: float | None = None
        selected_window_frequency_ratio: float | None = None
        selected_interrupt_deltas: dict[str, int] | None = None
        selected_schedstat_delta: dict[str, int] | None = None
        selected_runqueue_wait_fraction: float | None = None
        launch_frequency_ratios: list[float] = []
        for cpu in physical_raw:
            left_metadata = before_cpu.get(str(cpu))
            right_metadata = after_cpu.get(str(cpu))
            if not isinstance(left_metadata, dict) or not isinstance(right_metadata, dict):
                _failure(failures, "cpu-evidence-missing", launch_id, cpu=cpu)
                continue
            left = left_metadata.get("times")
            right = right_metadata.get("times")
            busy = (
                _busy_fraction(left, right)
                if isinstance(left, list)
                and isinstance(right, list)
                and all(isinstance(value, int) and not isinstance(value, bool) for value in left + right)
                else None
            )
            for field in ("governor", "scaling_min_freq", "scaling_max_freq"):
                if left_metadata.get(field) != right_metadata.get(field):
                    _failure(
                        failures,
                        "cpu-frequency-metadata-changed",
                        launch_id,
                        cpu=cpu,
                        field=field,
                    )
            if isolation_online_states is not None:
                expected_online = isolation_online_states.get(str(cpu))
                for phase_name, metadata in (
                    ("before", left_metadata),
                    ("after", right_metadata),
                ):
                    if metadata.get("online") is not expected_online:
                        _failure(
                            failures,
                            "cpu-online-state-drifted-from-isolation",
                            launch_id,
                            cpu=cpu,
                            phase=phase_name,
                            expected=expected_online,
                            observed=metadata.get("online"),
                        )
            if cpu not in selected:
                online_values = (
                    left_metadata.get("online"),
                    right_metadata.get("online"),
                )
                if online_values == (False, False):
                    sibling_busy[str(cpu)] = 0.0
                elif online_values != (True, True):
                    sibling_busy[str(cpu)] = None
                    _failure(
                        failures,
                        "smt-sibling-online-state-changed",
                        launch_id,
                        cpu=cpu,
                    )
                else:
                    sibling_busy[str(cpu)] = busy
                if online_values != (False, False) and (
                    busy is None or busy > max_sibling_busy
                ):
                    _failure(
                        failures,
                        "smt-sibling-interference",
                        launch_id,
                        cpu=cpu,
                        busy_fraction=busy,
                    )
            else:
                selected_busy = busy
                left_schedstat = left_metadata.get("schedstat")
                right_schedstat = right_metadata.get("schedstat")
                schedstat_fields = {"run_ns", "wait_ns", "timeslices"}
                if (
                    isinstance(left_schedstat, Mapping)
                    and isinstance(right_schedstat, Mapping)
                    and set(left_schedstat) == schedstat_fields
                    and set(right_schedstat) == schedstat_fields
                    and all(
                        isinstance(left_schedstat[name], int)
                        and not isinstance(left_schedstat[name], bool)
                        and isinstance(right_schedstat[name], int)
                        and not isinstance(right_schedstat[name], bool)
                        and left_schedstat[name] >= 0
                        and right_schedstat[name] >= left_schedstat[name]
                        for name in schedstat_fields
                    )
                ):
                    selected_schedstat_delta = {
                        name: right_schedstat[name] - left_schedstat[name]
                        for name in schedstat_fields
                    }
                    scheduled_ns = (
                        selected_schedstat_delta["run_ns"]
                        + selected_schedstat_delta["wait_ns"]
                    )
                    if scheduled_ns > 0:
                        selected_runqueue_wait_fraction = (
                            selected_schedstat_delta["wait_ns"] / scheduled_ns
                        )
                        runqueue_wait_fractions.append(
                            selected_runqueue_wait_fraction
                        )
                if require_schedstat and selected_runqueue_wait_fraction is None:
                    _failure(
                        failures,
                        "schedstat-evidence-unavailable",
                        launch_id,
                        cpu=cpu,
                    )
                elif (
                    require_schedstat
                    and selected_runqueue_wait_fraction
                    > max_runqueue_wait_fraction
                ):
                    _failure(
                        failures,
                        "runqueue-wait-fraction-too-high",
                        launch_id,
                        cpu=cpu,
                        wait_fraction=selected_runqueue_wait_fraction,
                    )
                left_interrupts = left_metadata.get("interrupts")
                right_interrupts = right_metadata.get("interrupts")
                if (
                    isinstance(left_interrupts, Mapping)
                    and isinstance(right_interrupts, Mapping)
                    and set(left_interrupts) == {"external", "local"}
                    and set(right_interrupts) == {"external", "local"}
                    and all(
                        isinstance(left_interrupts[name], int)
                        and not isinstance(left_interrupts[name], bool)
                        and isinstance(right_interrupts[name], int)
                        and not isinstance(right_interrupts[name], bool)
                        and right_interrupts[name] >= left_interrupts[name]
                        for name in ("external", "local")
                    )
                ):
                    selected_interrupt_deltas = {
                        name: right_interrupts[name] - left_interrupts[name]
                        for name in ("external", "local")
                    }
                if busy is None or busy < min_selected_busy:
                    _failure(
                        failures,
                        "selected-cpu-not-busy",
                        launch_id,
                        cpu=cpu,
                        busy_fraction=busy,
                    )
                for phase_name, metadata in (
                    ("before", left_metadata),
                    ("after", right_metadata),
                ):
                    governor = metadata.get("governor")
                    if not isinstance(governor, str) or not governor:
                        _failure(
                            failures,
                            "cpu-governor-unavailable",
                            launch_id,
                            cpu=cpu,
                            phase=phase_name,
                        )
                    elif governor != "performance":
                        _failure(
                            failures,
                            "non-performance-governor",
                            launch_id,
                            cpu=cpu,
                            phase=phase_name,
                            governor=governor,
                        )
                    current = _finite(metadata.get("scaling_cur_freq"))
                    minimum = _finite(metadata.get("scaling_min_freq"))
                    maximum = _finite(metadata.get("scaling_max_freq"))
                    if (
                        current is None
                        or minimum is None
                        or maximum is None
                        or current <= 0.0
                        or minimum <= 0.0
                        or maximum <= 0.0
                        or minimum > maximum
                    ):
                        _failure(
                            failures,
                            "cpu-frequency-unavailable",
                            launch_id,
                            cpu=cpu,
                            phase=phase_name,
                        )
                    else:
                        ratio = current / maximum
                        launch_frequency_ratios.append(ratio)
                        frequencies.append(ratio)
                left_mperf = left_metadata.get("mperf")
                right_mperf = right_metadata.get("mperf")
                left_aperf = left_metadata.get("aperf")
                right_aperf = right_metadata.get("aperf")
                counters = (left_mperf, right_mperf, left_aperf, right_aperf)
                if all(
                    isinstance(value, int)
                    and not isinstance(value, bool)
                    and value >= 0
                    for value in counters
                ):
                    delta_mperf = right_mperf - left_mperf
                    delta_aperf = right_aperf - left_aperf
                    if delta_mperf > 0 and delta_aperf > 0:
                        selected_window_frequency_ratio = (
                            delta_aperf / delta_mperf
                        )
                        window_frequency_ratios.append(
                            selected_window_frequency_ratio
                        )
                if (
                    require_window_frequency
                    and selected_window_frequency_ratio is None
                ):
                    _failure(
                        failures,
                        "window-frequency-evidence-unavailable",
                        launch_id,
                        cpu=cpu,
                    )
                elif (
                    require_window_frequency
                    and selected_window_frequency_ratio
                    < min_window_frequency_ratio
                ):
                    _failure(
                        failures,
                        "window-frequency-below-floor",
                        launch_id,
                        cpu=cpu,
                        aperf_mperf_ratio=selected_window_frequency_ratio,
                    )
                if (
                    selected_window_frequency_ratio is not None
                    and frequency_preflight_summary is not None
                ):
                    preflight_ratio = float(
                        frequency_preflight_summary["aperf_mperf_ratio"]
                    )
                    relative_ratio = selected_window_frequency_ratio / preflight_ratio
                    window_to_preflight_ratios.append(relative_ratio)
                    if (
                        require_frequency_preflight
                        and relative_ratio < min_window_to_preflight_ratio
                    ):
                        _failure(
                            failures,
                            "window-frequency-below-preflight-baseline",
                            launch_id,
                            cpu=cpu,
                            aperf_mperf_ratio=selected_window_frequency_ratio,
                            preflight_aperf_mperf_ratio=preflight_ratio,
                            ratio_to_preflight=relative_ratio,
                        )
        load_values = [
            _finite(before.get("load_per_online_cpu")),
            _finite(after.get("load_per_online_cpu")),
        ]
        maximum_load = None if None in load_values else max(load_values)  # type: ignore[arg-type]
        if maximum_load is None:
            _failure(failures, "host-load-unavailable", launch_id)
        elif maximum_load > max_load_per_cpu:
            _failure(
                failures,
                "host-load-too-high",
                launch_id,
                load_per_online_cpu=maximum_load,
            )
        before_ns = before.get("monotonic_ns")
        after_ns = after.get("monotonic_ns")
        before_wall = before.get("timestamp_ns")
        after_wall = after.get("timestamp_ns")
        timing_valid = all(
            isinstance(value, int) and not isinstance(value, bool) and value >= 0
            for value in (before_ns, after_ns, before_wall, after_wall)
        )
        duration_ns = (
            after_ns - before_ns  # type: ignore[operator]
            if timing_valid
            else None
        )
        if (
            duration_ns is None
            or duration_ns <= 0
            or after_wall < before_wall  # type: ignore[operator]
        ):
            _failure(failures, "invalid-snapshot-timestamps", launch_id)
        external_interrupt_rate = (
            selected_interrupt_deltas["external"] * 1_000_000_000.0 / duration_ns
            if selected_interrupt_deltas is not None
            and duration_ns is not None
            and duration_ns > 0
            else None
        )
        local_interrupt_rate = (
            selected_interrupt_deltas["local"] * 1_000_000_000.0 / duration_ns
            if selected_interrupt_deltas is not None
            and duration_ns is not None
            and duration_ns > 0
            else None
        )
        if (
            require_interrupts or residual_requires_zero_external_interrupts
        ) and external_interrupt_rate is None:
            _failure(failures, "interrupt-evidence-unavailable", launch_id)
        elif (
            residual_requires_zero_external_interrupts
            and selected_interrupt_deltas is not None
            and selected_interrupt_deltas["external"] != 0
        ):
            _failure(
                failures,
                "residual-irq-observed-on-selected-cpu",
                launch_id,
                residual_irqs=residual_irq_numbers,
                external_interrupt_delta=selected_interrupt_deltas["external"],
            )
        elif (
            external_interrupt_rate is not None
            and external_interrupt_rate > max_interrupts_per_second
        ):
            _failure(
                failures,
                "interrupt-rate-too-high",
                launch_id,
                external_interrupts_per_second=external_interrupt_rate,
            )
        pressure_fractions: dict[str, float | None] = {}
        for name, limit in (("cpu", max_cpu_psi), ("memory", max_memory_psi)):
            left_total = _pressure_total(before.get(f"pressure_{name}"))
            right_total = _pressure_total(after.get(f"pressure_{name}"))
            fraction = (
                None
                if left_total is None
                or right_total is None
                or right_total < left_total
                or duration_ns is None
                or duration_ns <= 0
                else (right_total - left_total) * 1000.0 / duration_ns
            )
            pressure_fractions[name] = fraction
            if fraction is None:
                if require_psi:
                    _failure(failures, f"{name}-psi-unavailable", launch_id)
            elif fraction > limit:
                _failure(
                    failures,
                    f"{name}-psi-too-high",
                    launch_id,
                    stall_fraction=fraction,
                )
        memory_values = [
            _finite(before.get("mem_available_kib")),
            _finite(after.get("mem_available_kib")),
        ]
        minimum_memory = None if None in memory_values else min(memory_values)  # type: ignore[arg-type]
        if minimum_memory is None:
            _failure(failures, "memory-available-unavailable", launch_id)
        elif minimum_memory < min_mem_available_kib:
            _failure(
                failures,
                "memory-available-below-floor",
                launch_id,
                minimum_kib=minimum_memory,
            )
        launch_temperatures: list[float] = []
        before_temperatures = before.get("temperatures_c")
        after_temperatures = after.get("temperatures_c")
        if (
            not isinstance(before_temperatures, dict)
            or not isinstance(after_temperatures, dict)
            or set(before_temperatures) != set(after_temperatures)
        ):
            _failure(failures, "temperature-sensor-set-changed", launch_id)
        for phase_name, phase in (("before", before), ("after", after)):
            values = phase.get("temperatures_c")
            finite_values = (
                [_finite(value) for value in values.values()]
                if isinstance(values, dict) and values
                else []
            )
            if not finite_values or any(value is None for value in finite_values):
                _failure(
                    failures,
                    "temperature-unavailable",
                    launch_id,
                    phase=phase_name,
                )
            else:
                launch_temperatures.extend(finite_values)  # type: ignore[arg-type]
                selected_temperature_sensors = set(
                    phase.get("selected_core_temperature_sensors", [])
                )
                for sensor, value in values.items():
                    if (
                        selected_temperature_sensors
                        and sensor not in selected_temperature_sensors
                    ):
                        continue
                    finite = _finite(value)
                    if finite is not None:
                        temperatures_by_sensor[str(sensor)].append(finite)
        if launch_temperatures and max(launch_temperatures) > max_temperature:
            _failure(
                failures,
                "temperature-above-ceiling",
                launch_id,
                maximum_c=max(launch_temperatures),
            )
        launches.append(
            {
                "launch_id": launch_id,
                "run_design": plan,
                "duration_ns": duration_ns,
                "selected_cpu_busy_fraction": selected_busy,
                "smt_sibling_busy_fraction": sibling_busy,
                "maximum_load_per_online_cpu": maximum_load,
                "minimum_frequency_ratio": min(launch_frequency_ratios, default=None),
                "window_aperf_mperf_ratio": selected_window_frequency_ratio,
                "window_to_preflight_frequency_ratio": (
                    None
                    if selected_window_frequency_ratio is None
                    or frequency_preflight_summary is None
                    else selected_window_frequency_ratio
                    / float(frequency_preflight_summary["aperf_mperf_ratio"])
                ),
                "selected_cpu_interrupt_delta": selected_interrupt_deltas,
                "selected_cpu_schedstat_delta": selected_schedstat_delta,
                "selected_cpu_runqueue_wait_fraction": (
                    selected_runqueue_wait_fraction
                ),
                "selected_cpu_external_interrupts_per_second": external_interrupt_rate,
                "selected_cpu_local_interrupts_per_second": local_interrupt_rate,
                "cpu_psi_stall_fraction": pressure_fractions["cpu"],
                "memory_psi_stall_fraction": pressure_fractions["memory"],
                "minimum_mem_available_kib": minimum_memory,
            }
        )
    minimum_frequency_ratio = min(frequencies, default=None)
    window_frequency_cv = (
        statistics.stdev(window_frequency_ratios)
        / statistics.mean(window_frequency_ratios)
        if len(window_frequency_ratios) >= 2
        and statistics.mean(window_frequency_ratios) > 0.0
        else 0.0
        if len(window_frequency_ratios) == 1
        else None
    )
    if require_window_frequency and len(window_frequency_ratios) != len(launches):
        _failure(
            failures,
            "window-frequency-coverage-incomplete",
            observed=len(window_frequency_ratios),
            expected=len(launches),
        )
    elif (
        require_window_frequency
        and window_frequency_cv is not None
        and window_frequency_cv > max_window_frequency_cv
    ):
        _failure(
            failures,
            "window-frequency-variation-too-high",
            coefficient_of_variation=window_frequency_cv,
        )
    if require_frequency_preflight and frequency_preflight_summary is None:
        _failure(failures, "frequency-preflight-evidence-unavailable")
    elif require_frequency_preflight and not frequency_preflight_fresh:
        _failure(failures, "frequency-preflight-evidence-stale")
    if (
        require_frequency_preflight
        and len(window_to_preflight_ratios) != len(launches)
    ):
        _failure(
            failures,
            "frequency-preflight-window-coverage-incomplete",
            observed=len(window_to_preflight_ratios),
            expected=len(launches),
        )
    if require_schedstat and len(runqueue_wait_fractions) != len(launches):
        _failure(
            failures,
            "schedstat-coverage-incomplete",
            observed=len(runqueue_wait_fractions),
            expected=len(launches),
        )
    if minimum_frequency_ratio is None:
        _failure(failures, "cpu-frequency-unavailable")
    elif require_frequency_floor and minimum_frequency_ratio < min_frequency_ratio:
        _failure(
            failures,
            "cpu-frequency-below-floor",
            minimum_ratio_to_scaling_max=minimum_frequency_ratio,
        )
    sensor_temperature_spans = {
        sensor: max(values) - min(values)
        for sensor, values in temperatures_by_sensor.items()
        if values
    }
    temperature_span = max(sensor_temperature_spans.values(), default=None)
    if temperature_span is None:
        _failure(failures, "temperature-unavailable")
    elif temperature_span > max_temperature_span:
        _failure(
            failures,
            "temperature-drift-too-high",
            span_c=temperature_span,
        )
    result = {
        "schema": AUDIT_SCHEMA,
        "status": "accepted" if not failures else "rejected",
        "inputs": {
            "telemetry": {"path": str(input_path), "sha256": _sha256(input_path)},
            "run_design": {"path": str(design_path), "sha256": _sha256(design_path)},
            "isolation_state": (
                None
                if isolation_path is None
                else {
                    "path": str(isolation_path),
                    "sha256": _sha256(isolation_path),
                }
            ),
        },
        "planned_launches": len(expected),
        "observed_launches": len(grouped),
        "complete_launches": len(launches),
        "thresholds": {
            "max_sibling_busy": max_sibling_busy,
            "max_load_per_cpu": max_load_per_cpu,
            "min_frequency_ratio": min_frequency_ratio,
            "require_frequency_floor": require_frequency_floor,
            "require_window_frequency": require_window_frequency,
            "min_window_aperf_mperf_ratio": min_window_frequency_ratio,
            "require_frequency_preflight": require_frequency_preflight,
            "min_window_to_preflight_frequency_ratio": (
                min_window_to_preflight_ratio
            ),
            "max_frequency_preflight_age_seconds": (
                max_frequency_preflight_age_seconds
            ),
            "max_window_frequency_coefficient_of_variation": max_window_frequency_cv,
            "max_selected_cpu_interrupts_per_second": max_interrupts_per_second,
            "require_interrupt_evidence": require_interrupts,
            "require_schedstat": require_schedstat,
            "max_runqueue_wait_fraction": max_runqueue_wait_fraction,
            "max_temperature_span_c": max_temperature_span,
            "max_temperature_c": max_temperature,
            "min_selected_cpu_busy": min_selected_busy,
            "max_cpu_psi_stall_fraction": max_cpu_psi,
            "max_memory_psi_stall_fraction": max_memory_psi,
            "require_psi": require_psi,
            "min_mem_available_kib": min_mem_available_kib,
        },
        "minimum_frequency_ratio": minimum_frequency_ratio,
        "minimum_window_aperf_mperf_ratio": min(
            window_frequency_ratios, default=None
        ),
        "frequency_preflight": frequency_preflight_summary,
        "minimum_window_to_preflight_frequency_ratio": min(
            window_to_preflight_ratios, default=None
        ),
        "window_frequency_coefficient_of_variation": window_frequency_cv,
        "maximum_runqueue_wait_fraction": max(
            runqueue_wait_fractions, default=None
        ),
        "temperature_span_c": temperature_span,
        "temperature_available": bool(temperatures_by_sensor),
        "temperature_span_by_sensor_c": sensor_temperature_spans,
        "launches": launches,
        "failures": failures,
        "isolation_state_checks_required": require_isolation_state,
    }
    Path(arguments.output).write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0 if not failures else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    capture = subparsers.add_parser("snapshot")
    capture.add_argument("--output", required=True)
    capture.add_argument("--phase", choices=("before", "after"), required=True)
    capture.add_argument("--launch-id", required=True)
    capture.add_argument("--super-run-id", required=True)
    capture.add_argument("--run-id", required=True)
    capture.add_argument("--mode", choices=("timing", "plugin-off"), required=True)
    capture.add_argument("--launch-position", type=int, required=True)
    capture.add_argument("--cpuset", required=True)
    capture.add_argument("--physical-core-cpuset")
    capture.set_defaults(function=snapshot)
    check = subparsers.add_parser("audit")
    check.add_argument("--input", required=True)
    check.add_argument("--run-design", required=True)
    check.add_argument("--output", required=True)
    check.add_argument("--isolation-state")
    check.add_argument("--require-isolation-state", action="store_true")
    check.add_argument("--max-sibling-busy", type=float, default=0.10)
    check.add_argument("--max-load-per-cpu", type=float, default=0.75)
    check.add_argument("--min-frequency-ratio", type=float, default=0.90)
    check.add_argument(
        "--require-frequency-floor",
        action="store_true",
        help="把窗口边界的 scaling_cur_freq 当作硬门禁；默认仅诊断",
    )
    check.add_argument(
        "--require-window-frequency",
        action="store_true",
        help="要求每个 launch 具备相对 nominal 频率的 APERF/MPERF 窗口证据",
    )
    check.add_argument(
        "--min-window-frequency-ratio", type=float, default=0.95
    )
    check.add_argument("--require-frequency-preflight", action="store_true")
    check.add_argument(
        "--min-window-to-preflight-ratio", type=float, default=0.95
    )
    check.add_argument(
        "--max-frequency-preflight-age-seconds", type=float, default=300.0
    )
    check.add_argument("--max-window-frequency-cv", type=float, default=0.03)
    check.add_argument("--max-interrupts-per-second", type=float, default=25.0)
    check.add_argument("--require-interrupts", action="store_true")
    check.add_argument(
        "--require-schedstat",
        action="store_true",
        help="要求每个 launch 具备所选 CPU 的 runqueue wait 证据",
    )
    check.add_argument("--max-runqueue-wait-fraction", type=float, default=0.01)
    check.add_argument("--max-temperature-span", type=float, default=12.0)
    check.add_argument("--max-temperature", type=float, default=90.0)
    check.add_argument("--min-selected-busy", type=float, default=0.50)
    check.add_argument("--max-cpu-psi", type=float, default=0.10)
    check.add_argument("--max-memory-psi", type=float, default=0.02)
    check.add_argument(
        "--require-psi",
        action="store_true",
        help="要求宿主内核提供 PSI；默认在缺失时记录为不可用而不拒绝",
    )
    check.add_argument(
        "--min-mem-available-kib", type=float, default=1_048_576.0
    )
    check.set_defaults(function=audit)
    binding = subparsers.add_parser("verify-binding")
    binding.add_argument("--audit", required=True)
    binding.add_argument("--input", required=True)
    binding.add_argument("--run-design", required=True)
    binding.add_argument("--source", choices=("current", "external"), required=True)
    binding.add_argument("--output", required=True)
    binding.set_defaults(function=verify_binding)
    preflight = subparsers.add_parser("frequency-preflight")
    preflight.add_argument("--cpu", type=int, required=True)
    preflight.add_argument("--output", required=True)
    preflight.add_argument("--isolation-state")
    preflight.add_argument("--duration-seconds", type=float, default=1.0)
    preflight.add_argument(
        "--minimum-aperf-mperf-ratio", type=float, default=0.95
    )
    preflight.add_argument(
        "--minimum-process-busy-fraction", type=float, default=0.90
    )
    preflight.set_defaults(function=frequency_preflight)
    arguments = parser.parse_args()
    return int(arguments.function(arguments))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (TelemetryError, OSError, ValueError) as error:
        raise SystemExit(f"riscv weight telemetry: {error}") from error
