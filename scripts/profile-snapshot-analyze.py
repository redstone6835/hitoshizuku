#!/usr/bin/env python3
"""Parse and report a versioned MyGO profiling snapshot."""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import struct
import subprocess
import sys
from collections import defaultdict
from pathlib import Path


MAGIC = b"MYGOPRF\0"
HEADER_SIZE = 320
SCHEMA_HEADER_SIZES = {2: 256, 3: HEADER_SIZE}
HISTOGRAM_BUCKETS = 64
SECTION_NAMES = {
    1: "events",
    2: "metrics",
    3: "syscalls",
    4: "errnos",
    5: "tasks",
    6: "samples",
    7: "trace",
}
EVENT_NAMES = [
    "sys_send_copy", "sys_send_socket", "sys_recv_socket", "sys_recv_copy",
    "net_protocol_turn", "net_protocol_ingress", "net_tcp_output",
    "net_egress_backpressure", "net_worker_turn", "net_tx_materialize",
    "net_checksum", "net_virtio_submit", "net_virtio_reclaim",
    "sched_yield_delay", "sched_switch", "wait_socket_read",
    "wait_socket_write", "wait_poll", "wait_mutex", "wait_futex", "wait_timer",
    "wait_yield", "wait_other", "wakeup_latency", "syscall_dispatch",
    "syscall_invoke", "syscall_finalize", "syscall_handoff", "sys_udp_lookup",
    "sys_udp_wait", "sys_udp_pin", "sys_udp_consume", "vfs_read", "vfs_write",
    "page_fault", "irq_dispatch", "block_submit", "block_drain", "block_complete",
    "block_wait", "net_stack_local_turn", "net_peer_rx", "net_receiver_run",
    "net_tcp_sequence", "net_tcp_receive_sequence", "net_tcp_window",
    "net_tx_writable", "net_writer_run", "net_stack_request",
    "wait_process_exit", "wait_vfork", "wait_block_io", "page_fault_resident",
    "page_fault_prepare", "page_fault_commit", "page_fault_single",
    "page_fault_cache_fill", "page_fault_uncached_fill", "vfs_lookup", "vfs_open",
    "vfs_getdents", "vfs_stat", "mm_map", "mm_unmap", "mm_protect", "mm_brk",
    "page_fault_file", "page_fault_anon", "page_fault_cow", "process_clone",
    "process_exec", "process_wait", "runqueue_latency", "urgent_spin_check",
    "urgent_pending_hit", "urgent_service", "slab_cache_hit", "slab_cache_miss",
    "slab_refill", "slab_flush", "slab_slow_path", "mm_protect_noop",
    "mm_protect_batch", "page_fault_decode", "page_fault_task_lookup",
    "page_fault_vma_lookup", "page_fault_page_lookup", "page_fault_nonresident",
    "mem_zero_anon_page", "mem_zero_allocator_small", "mem_zero_allocator_large",
    "mem_copy_realloc", "mem_copy_cow", "alloc_registry_register",
    "alloc_registry_remove", "alloc_registry_lookup",
    "alloc_registry_register_kernel", "alloc_registry_register_owned",
    "alloc_owner_range_lookup",
]


class SnapshotError(RuntimeError):
    pass


def u16(data: bytes, offset: int) -> int:
    return struct.unpack_from("<H", data, offset)[0]


def u32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<I", data, offset)[0]


def u64(data: bytes, offset: int) -> int:
    return struct.unpack_from("<Q", data, offset)[0]


def i32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<i", data, offset)[0]


def percentile(histogram: list[int], percent: int) -> int:
    total = sum(histogram)
    if total == 0:
        return 0
    target = (total * percent + 99) // 100
    seen = 0
    for bucket, count in enumerate(histogram):
        seen += count
        if seen >= target:
            return 0 if bucket == 0 else 1 << (bucket - 1)
    return 1 << (len(histogram) - 2)


def parse_timing(record: bytes, base: int) -> dict[str, object]:
    names = [
        "calls", "cycles", "bytes", "packets", "max_cycles", "wall_ns",
        "on_cpu_ns", "off_cpu_ns", "max_latency_ns", "migrations",
    ]
    result: dict[str, object] = {
        name: u64(record, base + index * 8) for index, name in enumerate(names)
    }
    histogram = [u64(record, base + 80 + index * 8) for index in range(64)]
    result["p50_ns"] = percentile(histogram, 50)
    result["p95_ns"] = percentile(histogram, 95)
    result["p99_ns"] = percentile(histogram, 99)
    return result


def parse_profile_health(path: Path | None) -> dict[str, str]:
    if path is None or not path.exists():
        return {}
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    if len(lines) != 1:
        raise SnapshotError("profile health must contain exactly one line")
    return line_values("HEALTH " + lines[0])


def health_uint(health: dict[str, str], name: str) -> int:
    try:
        value = int(health[name], 0)
    except (KeyError, ValueError) as error:
        raise SnapshotError(f"invalid profile health field {name}") from error
    if value < 0:
        raise SnapshotError(f"invalid profile health field {name}={value}")
    return value


def parse_snapshot(path: Path, health_path: Path | None = None) -> dict[str, object]:
    data = path.read_bytes()
    if len(data) < min(SCHEMA_HEADER_SIZES.values()) or data[:8] != MAGIC:
        raise SnapshotError("invalid snapshot magic or truncated header")
    version = u16(data, 8)
    if version not in SCHEMA_HEADER_SIZES:
        raise SnapshotError(f"unsupported snapshot schema: {version}")
    header_size = u16(data, 10)
    if header_size != SCHEMA_HEADER_SIZES[version] or u32(data, 12) != 0x01020304:
        raise SnapshotError("unsupported snapshot byte order or header size")
    total_size = u64(data, 16)
    if total_size != len(data):
        raise SnapshotError(f"snapshot length mismatch: header={total_size} file={len(data)}")
    section_count = u16(data, 62)
    if section_count != 7:
        raise SnapshotError(f"unexpected section count: {section_count}")
    sections: dict[int, tuple[int, int, int]] = {}
    previous_end = header_size
    for index in range(section_count):
        base = 64 + index * 24
        kind = u16(data, base)
        size = u16(data, base + 2)
        offset = u64(data, base + 8)
        count = u64(data, base + 16)
        if version == 2 and index == section_count - 1 and kind == 7:
            if size == 0 or offset < previous_end or (len(data) - offset) % size != 0:
                raise SnapshotError("cannot recover legacy trace section geometry")
            count = (len(data) - offset) // size
        end = offset + count * size
        if (kind not in SECTION_NAMES or kind in sections or size == 0 or
                offset < previous_end or end > len(data)):
            raise SnapshotError(f"invalid section directory entry: kind={kind}")
        sections[kind] = (offset, count, size)
        previous_end = end
    if set(sections) != set(SECTION_NAMES):
        raise SnapshotError("snapshot section set is incomplete")
    if previous_end != len(data):
        raise SnapshotError("snapshot sections do not cover the complete file")

    if version == 2:
        event_mask_high = u64(data, 224)
        workload_root = u64(data, 232)
        sample_hz = u64(data, 240)
        legacy_dropped = u64(data, 248)
        dropped = {"samples": None, "trace": None, "errnos": None, "tasks": None}
    else:
        event_mask_high = u64(data, 232)
        workload_root = u64(data, 240)
        sample_hz = u64(data, 248)
        legacy_dropped = None
        dropped = {
            "samples": u64(data, 256),
            "trace": u64(data, 264),
            "errnos": u64(data, 272),
            "tasks": u64(data, 280),
        }

    health = parse_profile_health(health_path)
    if health:
        if health.get("state") != "frozen" or health_uint(health, "active_writers") != 0:
            raise SnapshotError("profile health reports an unstable snapshot")
        health_version = health_uint(health, "schema_version")
        health_bytes = health_uint(health, "snapshot_bytes")
        if health_version != version or health_bytes != total_size:
            raise SnapshotError("profile health does not match the snapshot")
        health_dropped = {
            "samples": health_uint(health, "dropped_samples"),
            "trace": health_uint(health, "dropped_trace"),
            "errnos": health_uint(health, "dropped_errno_records"),
            "tasks": health_uint(health, "dropped_task_records"),
        }
        if version == 3 and health_dropped != dropped:
            raise SnapshotError("profile health drop counters do not match the snapshot")
        dropped = health_dropped

    known_dropped = [value for value in dropped.values() if value is not None]
    dropped_records = (legacy_dropped if legacy_dropped is not None
                       else sum(known_dropped))
    section_complete = {
        name: value == 0 if value is not None else None
        for name, value in dropped.items()
    }
    header = {
        "version": version,
        "bytes": total_size,
        "session": u64(data, 24),
        "generation": u64(data, 32),
        "counter_hz": u64(data, 40),
        "event_mask": u64(data, 48),
        "event_mask_high": event_mask_high,
        "phase": u32(data, 56),
        "cpu_slots": u16(data, 60),
        "workload_root": workload_root,
        "sample_hz": sample_hz,
        "dropped_records": dropped_records,
        "dropped": dropped,
        "section_complete": section_complete,
        "complete": dropped_records == 0,
    }
    if header["counter_hz"] == 0:
        raise SnapshotError("counter frequency is zero")

    def records(kind: int):
        offset, count, size = sections[kind]
        for index in range(count):
            start = offset + index * size
            yield data[start:start + size]

    events_by_id: dict[int, dict[str, object]] = {}
    event_accumulator: dict[int, dict[str, object]] = {}
    for record in records(1):
        event_id = u16(record, 2)
        timing = parse_timing(record, 8)
        if not timing["calls"]:
            continue
        aggregate = event_accumulator.setdefault(event_id, {
            "calls": 0, "cycles": 0, "bytes": 0, "packets": 0,
            "max_cycles": 0, "wall_ns": 0, "on_cpu_ns": 0, "off_cpu_ns": 0,
            "max_latency_ns": 0, "migrations": 0,
        })
        for name in ("calls", "cycles", "bytes", "packets", "wall_ns", "on_cpu_ns",
                     "off_cpu_ns", "migrations"):
            aggregate[name] += timing[name]
        aggregate["max_cycles"] = max(aggregate["max_cycles"], timing["max_cycles"])
        aggregate["max_latency_ns"] = max(
            aggregate["max_latency_ns"], timing["max_latency_ns"]
        )
    for event_id, timing in event_accumulator.items():
        timing["id"] = event_id
        timing["name"] = EVENT_NAMES[event_id] if event_id < len(EVENT_NAMES) else f"event_{event_id}"
        events_by_id[event_id] = timing

    syscalls = []
    for record in records(3):
        timing = parse_timing(record, 24)
        if not timing["calls"]:
            continue
        success = u64(record, 8)
        errors = u64(record, 16)
        completed = success + errors
        syscalls.append({
            "phase": u16(record, 0),
            "nr": u16(record, 2),
            "completed": completed,
            "inflight": max(int(timing["calls"]) - completed, 0),
            "success": success,
            "errors": errors,
            **timing,
        })

    errnos = []
    for record in records(4):
        count = u64(record, 8)
        if count:
            errnos.append({
                "phase": u16(record, 0), "nr": u16(record, 2),
                "errno": u32(record, 4), "count": count,
            })

    tasks = []
    for record in records(5):
        session = u64(record, 0)
        if session == 0:
            continue
        tasks.append({
            "session": session, "pid": u32(record, 8), "tgid": u32(record, 12),
            "ppid": u32(record, 16), "exited": bool(u32(record, 20)),
            "runtime_ns": u64(record, 24), "voluntary_switches": u64(record, 32),
            "involuntary_switches": u64(record, 40), "migrations": u64(record, 48),
            "exit_code": i32(record, 56), "main_image_id": u64(record, 64),
            "main_image_base": u64(record, 72), "main_image_end": u64(record, 80),
            "interpreter_image_id": u64(record, 88),
            "interpreter_image_base": u64(record, 96),
            "interpreter_image_end": u64(record, 104),
        })

    samples = []
    for record in records(6):
        count = u64(record, 32)
        pc = u64(record, 8)
        if count and pc:
            samples.append({
                "cpu": u16(record, 0), "mode": "user" if u16(record, 2) else "kernel",
                "pc": pc, "image_id": u64(record, 16), "load_base": u64(record, 24),
                "samples": count,
            })

    return {
        "header": header,
        "events": list(events_by_id.values()),
        "syscalls": syscalls,
        "errnos": errnos,
        "tasks": tasks,
        "samples": samples,
    }


def parse_syscall_names(path: Path | None) -> dict[int, str]:
    if path is None:
        return {}
    result = {}
    pattern = re.compile(r"^pub const SYS_([A-Z0-9_]+): usize = ([0-9]+);")
    for line in path.read_text(encoding="utf-8").splitlines():
        match = pattern.match(line)
        if match:
            result[int(match.group(2))] = match.group(1).lower()
    return result


def parse_phase_names(path: Path | None) -> dict[int, str]:
    result = {0: "initial"}
    if path is None:
        return result
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) < 2 or not fields[0].isdigit() or not re.fullmatch(
            r"[A-Za-z0-9._-]+", fields[1]
        ):
            raise SnapshotError(f"invalid phase map at line {line_number}")
        result[int(fields[0])] = fields[1]
    return result


def fnv_path(path: str) -> int:
    value = 0xCBF29CE484222325
    for byte in path.encode("utf-8"):
        value ^= byte
        value = (value * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return max(value, 1)


def locate_user_images(root: Path | None, wanted: set[int]) -> dict[int, Path]:
    if root is None or not wanted:
        return {}
    found: dict[int, Path] = {}
    seen_real_directories: dict[str, int] = defaultdict(int)
    for directory, dirnames, filenames in os.walk(root, followlinks=True):
        real_directory = os.path.realpath(directory)
        seen_real_directories[real_directory] += 1
        if seen_real_directories[real_directory] > 4:
            dirnames[:] = []
            continue
        relative_dir = os.path.relpath(directory, root)
        for name in filenames:
            relative = name if relative_dir == "." else f"{relative_dir}/{name}"
            image_id = fnv_path("/" + relative)
            if image_id in wanted:
                candidate = Path(directory) / name
                if candidate.is_file():
                    found[image_id] = candidate
        if wanted <= set(found):
            break
    return found


def disk_path_identities(guest_path: str, mount_prefixes: list[str]) -> list[str]:
    identities = [guest_path]
    for prefix in mount_prefixes:
        normalized = "/" + prefix.strip("/")
        if normalized != "/":
            identities.append(normalized + guest_path)
    return identities


def locate_user_images_in_disk(disk: Path | None, wanted: set[int], cache: Path,
                               mount_prefixes: list[str]) -> dict[int, Path]:
    if disk is None or not wanted:
        return {}
    try:
        listing = subprocess.run(
            ["guestfish", "--ro", "-a", str(disk), "-m", "/dev/sda", "find", "/"],
            check=True, text=True, capture_output=True,
        ).stdout.splitlines()
    except (OSError, subprocess.CalledProcessError) as error:
        raise SnapshotError(f"cannot enumerate guest image for symbols: {error}") from error
    matches = {}
    for guest_path in listing:
        if not guest_path.startswith("/"):
            guest_path = "/" + guest_path
        for identity in disk_path_identities(guest_path, mount_prefixes):
            image_id = fnv_path(identity)
            if image_id in wanted:
                matches[image_id] = guest_path
    cache.mkdir(parents=True, exist_ok=True)
    found = {}
    for image_id, guest_path in matches.items():
        destination = cache / f"{image_id:016x}.elf"
        try:
            subprocess.run(
                ["guestfish", "--ro", "-a", str(disk), "-m", "/dev/sda",
                 "download", guest_path, str(destination)],
                check=True, text=True, capture_output=True,
            )
        except (OSError, subprocess.CalledProcessError):
            continue
        if destination.is_file() and destination.stat().st_size:
            found[image_id] = destination
    return found


def symbolize(binary: Path, addresses: list[int], addr2line: str) -> dict[int, str]:
    if not addresses:
        return {}
    result = {}
    # execve 参数受 ARG_MAX 限制；大型 BuildStorm 快照可能包含数万地址，
    # 分批调用避免一次传参过长导致 E2BIG，并保留已成功解析的批次。
    batch_size = 256
    for start in range(0, len(addresses), batch_size):
        batch = addresses[start : start + batch_size]
        command = [addr2line, "-f", "-C", "-e", str(binary)] + [hex(value) for value in batch]
        try:
            output = subprocess.run(
                command, check=True, text=True, capture_output=True
            ).stdout.splitlines()
        except (OSError, subprocess.CalledProcessError) as error:
            result.update({value: f"symbolization failed: {error}" for value in batch})
            continue
        for index, address in enumerate(batch):
            function = output[index * 2] if index * 2 < len(output) else "??"
            location = output[index * 2 + 1] if index * 2 + 1 < len(output) else "??:0"
            result[address] = f"{function} at {location}"
    return result


def add_symbols(profile: dict[str, object], kernel_elf: Path | None,
                image_root: Path | None, disk_image: Path | None,
                image_cache: Path, addr2line: str, tcg_profile: dict[str, object],
                disk_mount_prefixes: list[str] | None = None) -> None:
    samples: list[dict[str, object]] = profile["samples"]  # type: ignore[assignment]
    tasks: list[dict[str, object]] = profile["tasks"]  # type: ignore[assignment]
    tcg_hot: list[dict[str, object]] = tcg_profile["hot"]  # type: ignore[assignment]
    user_ranges = set()
    for task in tasks:
        for prefix in ("main", "interpreter"):
            image_id = int(task[f"{prefix}_image_id"])
            base = int(task[f"{prefix}_image_base"])
            end = int(task[f"{prefix}_image_end"])
            if image_id and base < end:
                user_ranges.add((image_id, base, end))

    def user_mapping_for_pc(pc: int) -> tuple[int, int] | None:
        matches = {(image_id, base) for image_id, base, end in user_ranges if base <= pc < end}
        return next(iter(matches)) if len(matches) == 1 else None

    wanted = {
        int(sample["image_id"])
        for sample in samples
        if sample["mode"] == "user" and sample["image_id"]
    }
    wanted.update(image_id for image_id, _, _ in user_ranges)
    images = locate_user_images(image_root, wanted)
    unresolved = wanted - set(images)
    if unresolved and disk_image is not None:
        images.update(locate_user_images_in_disk(
            disk_image, unresolved, image_cache, disk_mount_prefixes or []
        ))
    user_addresses: dict[int, set[int]] = defaultdict(set)
    for sample in samples:
        if sample["mode"] == "user" and sample["image_id"] in images:
            user_addresses[sample["image_id"]].add(sample["pc"] - sample["load_base"])
    kernel_addresses = {int(sample["pc"]) for sample in samples if sample["mode"] == "kernel"}
    for row in tcg_hot:
        mapping = user_mapping_for_pc(int(row["pc"]))
        if mapping is None:
            row["mode"] = "kernel_or_firmware"
            kernel_addresses.add(int(row["pc"]))
            continue
        image_id, base = mapping
        row["mode"] = "user"
        row["image_id"] = image_id
        row["load_base"] = base
        row["relative_pc"] = int(row["pc"]) - base
        if image_id in images:
            user_addresses[image_id].add(int(row["relative_pc"]))

    kernel_symbols = symbolize(kernel_elf, sorted(kernel_addresses), addr2line) if kernel_elf else {}
    user_symbols = {
        image_id: symbolize(images[image_id], sorted(addresses), addr2line)
        for image_id, addresses in user_addresses.items()
    }
    for sample in samples:
        if sample["mode"] == "kernel":
            sample["binary"] = str(kernel_elf) if kernel_elf else ""
            sample["symbol"] = kernel_symbols.get(sample["pc"], "kernel ELF not supplied")
        else:
            image_id = sample["image_id"]
            relative = sample["pc"] - sample["load_base"]
            sample["relative_pc"] = relative
            sample["binary"] = str(images.get(image_id, ""))
            sample["symbol"] = user_symbols.get(image_id, {}).get(
                relative, "user image unresolved"
            )
    for row in tcg_hot:
        if row["mode"] == "user":
            image_id = int(row["image_id"])
            relative = int(row["relative_pc"])
            row["binary"] = str(images.get(image_id, ""))
            row["symbol"] = user_symbols.get(image_id, {}).get(
                relative, "user image unresolved"
            )
        else:
            row["binary"] = str(kernel_elf) if kernel_elf else ""
            row["symbol"] = kernel_symbols.get(int(row["pc"]), "kernel ELF not supplied")


def parse_host_perf(path: Path | None) -> list[dict[str, str]]:
    if path is None or not path.exists():
        return []
    rows = []
    with path.open(newline="", encoding="utf-8") as stream:
        for row in csv.reader(stream):
            if len(row) >= 3 and row[0].strip() and not row[0].startswith("#"):
                rows.append({"value": row[0].strip(), "unit": row[1].strip(), "event": row[2].strip()})
    return rows


def line_values(line: str) -> dict[str, str]:
    values = {}
    for field in line.split()[1:]:
        if "=" not in field:
            raise SnapshotError(f"malformed profile field: {field}")
        key, value = field.split("=", 1)
        if not key or not value or key in values:
            raise SnapshotError(f"invalid profile field: {field}")
        values[key] = value
    return values


def tcg_uint(values: dict[str, str], name: str, minimum: int = 0) -> int:
    try:
        value = int(values[name], 0)
    except (KeyError, ValueError) as error:
        raise SnapshotError(f"invalid QEMU TCG field {name}") from error
    if value < minimum:
        raise SnapshotError(f"invalid QEMU TCG field {name}={value}")
    return value


def parse_tcg_profile(path: Path | None) -> dict[str, object]:
    if path is None or not path.exists():
        return {"header": {}, "vcpus": [], "hot": [], "complete": None}
    header = {}
    vcpus = []
    hot = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.startswith("MYGO_TCG_PROFILE "):
            if header:
                raise SnapshotError("duplicate QEMU TCG profile header")
            header = line_values(line)
        elif line.startswith("VCPU "):
            row = line_values(line)
            try:
                vcpus.append({key: int(value, 0) for key, value in row.items()})
            except ValueError as error:
                raise SnapshotError("invalid QEMU TCG VCPU row") from error
        elif line.startswith("HOT "):
            row = line_values(line)
            try:
                hot.append({key: int(value, 0) for key, value in row.items()})
            except ValueError as error:
                raise SnapshotError("invalid QEMU TCG hotspot row") from error
    if not header:
        raise SnapshotError("missing QEMU TCG profile header")
    required = {
        "version", "target", "configured_vcpus", "active_vcpus", "table_bits",
        "table_slots", "table_probes", "counter_bytes_per_vcpu", "translated_blocks",
        "occupied_slots", "dropped", "collision_probes", "max_probe", "total_blocks",
        "total_instructions", "reported_hotspots",
    }
    missing = sorted(required.difference(header))
    if missing:
        raise SnapshotError(f"QEMU TCG profile is missing fields: {', '.join(missing)}")
    if header["version"] != "2" or not header["target"]:
        raise SnapshotError("unsupported QEMU TCG profile schema")
    configured_vcpus = tcg_uint(header, "configured_vcpus", 1)
    active_vcpus = tcg_uint(header, "active_vcpus", 1)
    table_bits = tcg_uint(header, "table_bits", 12)
    table_slots = tcg_uint(header, "table_slots", 1)
    table_probes = tcg_uint(header, "table_probes", 1)
    counter_bytes = tcg_uint(header, "counter_bytes_per_vcpu", 17)
    translated_blocks = tcg_uint(header, "translated_blocks", 1)
    occupied_slots = tcg_uint(header, "occupied_slots", 1)
    dropped = tcg_uint(header, "dropped")
    collision_probes = tcg_uint(header, "collision_probes", translated_blocks)
    max_probe = tcg_uint(header, "max_probe", 1)
    total_blocks = tcg_uint(header, "total_blocks", 1)
    total_instructions = tcg_uint(header, "total_instructions", 1)
    reported_hotspots = tcg_uint(header, "reported_hotspots", 1)
    if table_bits > 23 or table_slots != 1 << table_bits:
        raise SnapshotError("QEMU TCG profile has invalid table geometry")
    if active_vcpus > configured_vcpus:
        raise SnapshotError("QEMU TCG profile has too many active VCPUs")
    if occupied_slots > table_slots or max_probe > table_probes:
        raise SnapshotError("QEMU TCG profile has invalid probe accounting")
    if counter_bytes <= 16 or reported_hotspots > 4096:
        raise SnapshotError("QEMU TCG profile has invalid counter capacity")
    seen_cpus = set()
    vcpu_blocks = 0
    vcpu_instructions = 0
    for row in vcpus:
        if set(row) != {"cpu", "blocks", "instructions"}:
            raise SnapshotError("QEMU TCG VCPU row has unexpected fields")
        cpu = row["cpu"]
        blocks = row["blocks"]
        instructions = row["instructions"]
        if cpu < 0 or cpu >= configured_vcpus or cpu in seen_cpus or blocks < 1 or instructions < 1:
            raise SnapshotError("QEMU TCG VCPU accounting is invalid")
        seen_cpus.add(cpu)
        vcpu_blocks += blocks
        vcpu_instructions += instructions
    if len(vcpus) != active_vcpus or vcpu_blocks != total_blocks or vcpu_instructions != total_instructions:
        raise SnapshotError("QEMU TCG VCPU totals do not match the header")

    seen_pcs = set()
    for index, row in enumerate(hot, start=1):
        if set(row) != {"rank", "pc", "blocks", "instructions"}:
            raise SnapshotError("QEMU TCG hotspot row has unexpected fields")
        if row["rank"] != index or row["pc"] in seen_pcs or row["blocks"] < 1 or row["instructions"] < 1:
            raise SnapshotError("QEMU TCG hotspot ordering is invalid")
        seen_pcs.add(row["pc"])
    if len(hot) != reported_hotspots:
        raise SnapshotError("QEMU TCG hotspot count does not match the header")
    return {"header": header, "vcpus": vcpus, "hot": hot, "complete": dropped == 0}


def write_tsv(path: Path, fields: list[str], rows: list[dict[str, object]]) -> None:
    with path.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields, delimiter="\t", extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)


def write_reports(profile: dict[str, object], output: Path,
                  syscall_names: dict[int, str], host_perf: list[dict[str, str]],
                  tcg_profile: dict[str, object], phase_names: dict[int, str]) -> None:
    output.mkdir(parents=True, exist_ok=True)
    syscalls: list[dict[str, object]] = profile["syscalls"]  # type: ignore[assignment]
    for row in syscalls:
        phase_names.setdefault(row["phase"], f"phase_{row['phase']}")
        row["phase_name"] = phase_names[row["phase"]]
        row["name"] = syscall_names.get(row["nr"], f"syscall_{row['nr']}")
    syscalls.sort(key=lambda row: int(row["wall_ns"]), reverse=True)
    events: list[dict[str, object]] = profile["events"]  # type: ignore[assignment]
    events.sort(key=lambda row: int(row["wall_ns"]), reverse=True)
    tasks: list[dict[str, object]] = profile["tasks"]  # type: ignore[assignment]
    tasks.sort(key=lambda row: int(row["runtime_ns"]), reverse=True)
    samples: list[dict[str, object]] = profile["samples"]  # type: ignore[assignment]
    samples.sort(key=lambda row: int(row["samples"]), reverse=True)
    errnos: list[dict[str, object]] = profile["errnos"]  # type: ignore[assignment]
    for row in errnos:
        row["phase_name"] = phase_names.get(row["phase"], f"phase_{row['phase']}")
        row["name"] = syscall_names.get(row["nr"], f"syscall_{row['nr']}")

    write_tsv(output / "events.tsv", ["id", "name", "calls", "wall_ns", "on_cpu_ns",
              "off_cpu_ns", "bytes", "packets", "max_latency_ns", "migrations"], events)
    write_tsv(output / "syscalls.tsv", ["phase", "phase_name", "nr", "name", "calls",
              "completed", "inflight", "success", "errors", "wall_ns", "on_cpu_ns",
              "off_cpu_ns", "p50_ns", "p95_ns", "p99_ns", "max_latency_ns",
              "migrations"], syscalls)
    write_tsv(output / "errnos.tsv", ["phase", "phase_name", "nr", "name", "errno", "count"], errnos)
    write_tsv(output / "tasks.tsv", ["pid", "tgid", "ppid", "runtime_ns", "voluntary_switches",
              "involuntary_switches", "migrations", "exited", "exit_code", "main_image_id",
              "main_image_base", "main_image_end", "interpreter_image_id",
              "interpreter_image_base", "interpreter_image_end"], tasks)
    write_tsv(output / "samples.tsv", ["cpu", "mode", "pc", "image_id", "load_base",
              "relative_pc", "samples", "binary", "symbol"], samples)

    document = {**profile, "host_perf": host_perf, "tcg_profile": tcg_profile}
    (output / "profile.json").write_text(json.dumps(document, indent=2), encoding="utf-8")
    phase_totals: dict[str, dict[str, int]] = defaultdict(lambda: {"calls": 0, "wall_ns": 0})
    for row in syscalls:
        phase = str(row["phase_name"])
        phase_totals[phase]["calls"] += int(row["calls"])
        phase_totals[phase]["wall_ns"] += int(row["wall_ns"])
    dropped: dict[str, int | None] = profile["header"]["dropped"]  # type: ignore[assignment]

    def completeness(value: object) -> str:
        if value is None:
            return "unknown"
        return "complete" if value else "incomplete"

    lines = [
        "# Workload profiling report", "", "## Capture health", "",
        f"- Session: {profile['header']['session']}",
        f"- Snapshot bytes: {profile['header']['bytes']}",
        f"- Sample frequency: {profile['header']['sample_hz']} Hz",
        f"- Guest snapshot: {completeness(profile['header']['complete'])}",
        f"- Dropped records: {profile['header']['dropped_records']}",
        f"- PC samples: {completeness(profile['header']['section_complete']['samples'])} "
        f"(dropped={dropped['samples'] if dropped['samples'] is not None else 'unknown'})",
        f"- Trace: {completeness(profile['header']['section_complete']['trace'])} "
        f"(dropped={dropped['trace'] if dropped['trace'] is not None else 'unknown'})",
        f"- Errno records: {completeness(profile['header']['section_complete']['errnos'])} "
        f"(dropped={dropped['errnos'] if dropped['errnos'] is not None else 'unknown'})",
        f"- Task records: {completeness(profile['header']['section_complete']['tasks'])} "
        f"(dropped={dropped['tasks'] if dropped['tasks'] is not None else 'unknown'})",
        f"- QEMU TCG hotspots: {completeness(tcg_profile['complete'])}", "",
        "## Phase syscall totals", "", "| Phase | Calls | Wall time (s) |",
        "| --- | ---: | ---: |",
    ]
    for phase_id in sorted(phase_names):
        phase = phase_names[phase_id]
        total = phase_totals[phase]
        lines.append(f"| {phase} | {total['calls']} | {total['wall_ns'] / 1e9:.6f} |")
    lines += ["", "## Top syscalls", "", "| Phase | Syscall | Calls | Wall time (s) | Off-CPU (s) | p99 (us) |",
              "| --- | --- | ---: | ---: | ---: | ---: |"]
    for row in syscalls[:20]:
        lines.append(f"| {row['phase_name']} | {row['name']} | {row['calls']} | "
                     f"{int(row['wall_ns']) / 1e9:.6f} | {int(row['off_cpu_ns']) / 1e9:.6f} | "
                     f"{int(row['p99_ns']) / 1e3:.3f} |")
    lines += ["", "## Top PC samples", "", "| Mode | Samples | PC | Binary | Symbol |",
              "| --- | ---: | --- | --- | --- |"]
    for row in samples[:30]:
        lines.append(f"| {row['mode']} | {row['samples']} | 0x{int(row['pc']):x} | "
                     f"{row.get('binary', '')} | {row.get('symbol', '')} |")
    if host_perf:
        lines += ["", "## Host QEMU counters", "", "| Event | Value | Unit |", "| --- | ---: | --- |"]
        for row in host_perf:
            lines.append(f"| {row['event']} | {row['value']} | {row['unit']} |")
    tcg_hot: list[dict[str, object]] = tcg_profile["hot"]  # type: ignore[assignment]
    if tcg_hot:
        tcg_header: dict[str, str] = tcg_profile["header"]  # type: ignore[assignment]
        lines += ["", "## QEMU TCG guest-PC hotspots", "",
                  f"Table drops: {tcg_header.get('dropped', 'unknown')}. "
                  "Rows remain useful for hotspot discovery; shares are exact only when complete.", "",
                  "| Rank | Guest PC | Kind | Binary | Symbol | Blocks | Instructions |",
                  "| ---: | --- | --- | --- | --- | ---: | ---: |"]
        for row in tcg_hot[:30]:
            lines.append(
                f"| {row['rank']} | 0x{row['pc']:x} | {row.get('mode', '')} | "
                f"{row.get('binary', '')} | {row.get('symbol', '')} | "
                f"{row['blocks']} | {row['instructions']} |"
            )
    (output / "report.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--kernel-elf", type=Path)
    parser.add_argument("--image-root", type=Path)
    parser.add_argument("--disk-image", type=Path)
    parser.add_argument("--disk-mount-prefix", action="append", default=["/mnt"])
    parser.add_argument("--syscall-table", type=Path,
                        default=Path(__file__).resolve().parents[1] / "kernel/src/syscalls/nr.rs")
    parser.add_argument("--phase-map", type=Path)
    parser.add_argument("--addr2line", default="addr2line")
    parser.add_argument("--host-perf", type=Path)
    parser.add_argument("--tcg-profile", type=Path)
    parser.add_argument("--health", type=Path)
    args = parser.parse_args()
    try:
        profile = parse_snapshot(args.snapshot, args.health)
        tcg_profile = parse_tcg_profile(args.tcg_profile)
        add_symbols(profile, args.kernel_elf, args.image_root, args.disk_image,
                    args.output / "user-images", args.addr2line, tcg_profile,
                    args.disk_mount_prefix)
        write_reports(profile, args.output, parse_syscall_names(args.syscall_table),
                      parse_host_perf(args.host_perf), tcg_profile,
                      parse_phase_names(args.phase_map))
    except (OSError, SnapshotError, ValueError) as error:
        print(f"profile snapshot analyze: {error}", file=sys.stderr)
        return 1
    print(f"profile snapshot analyze: reports written to {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
