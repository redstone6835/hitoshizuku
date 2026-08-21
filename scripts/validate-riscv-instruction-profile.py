#!/usr/bin/env python3
"""Validate one completed RISC-V profile instruction profile."""

from __future__ import annotations

import argparse
import bisect
import collections
import csv
import json
import pathlib
import re
import struct
import sys
from typing import Any

from rv_instruction_profile_io import ProfileIoError, profile_quality_summary


MIX_SCHEMA = "mygo.riscv-instruction-mix.v1"
CATALOG_SCHEMA = "mygo.riscv-tb-catalog.v1"
REPORT_SCHEMA = "mygo.riscv-instruction-profile-quality.v1"
JIT_MAGIC = 0x4A695444
JIT_HEADER = struct.Struct("<IIIIIIQQ")
JIT_RECORD = struct.Struct("<IIQ")
TCG_HEADER = struct.Struct("<8sHHIQQQQIIQQ")
TCG_RECORD = struct.Struct("<HHI")
TCG_SAMPLE = struct.Struct("<QQQIIII")
TCG_LOST = struct.Struct("<QQQII")
TCG_THREAD = struct.Struct("<QIIIIiI32s")
TCG_TID_STATS = struct.Struct("<QQQQQQQQQQIiiI")
TCG_TID_STATS_LEGACY = struct.Struct("<QQQQQQQQIiiI")
TCG_ATTACH_FAILURE = struct.Struct("<QIiII")
TCG_GATE = struct.Struct("<QII")
TCG_QUALITY = struct.Struct("<QQQQQQQQQQQQQQIIIIII")
TCG_QUALITY_LEGACY = struct.Struct("<QQQQQQQQQQQQIIIIII")
CATALOG_INDEX = re.compile(br'"translation_index":([0-9]+)')
CATALOG_PC = re.compile(br'"guest_pc":"0x([0-9a-fA-F]{16})"')
CATALOG_TID = re.compile(br'"host_tid":([0-9]+)')
JIT_GUEST_NAME = re.compile(br"guest-0x([0-9a-fA-F]+)")
VCPU_COMM = re.compile(r"CPU ([0-9]+)/TCG")


class ProfileError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ProfileError(message)


def gate(condition: bool, message: str, errors: list[str]) -> None:
    if not condition:
        errors.append(message)


def uint(value: Any, owner: str) -> int:
    if type(value) is not int or value < 0:
        raise ProfileError(f"{owner} must be a non-negative integer")
    return value


def json_record(raw: bytes, path: pathlib.Path, line_number: int) -> dict[str, Any]:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProfileError(f"{path}:{line_number}: invalid JSON: {error}") from error
    if not isinstance(value, dict):
        raise ProfileError(f"{path}:{line_number}: record must be an object")
    return value


def parse_tid_maps(mapping_path: pathlib.Path, snapshots_path: pathlib.Path,
                   expected_vcpus: int, errors: list[str]) -> tuple[dict[str, Any], dict[str, Any]]:
    expected_phases = ("setup", "window-start", "window-stop")
    snapshots: dict[str, dict[str, int]] = {}
    with snapshots_path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        require(reader.fieldnames == ["monotonic_ns", "phase", "thread_count", "vcpu_count", "new_tid_count"],
                f"{snapshots_path}: invalid header")
        for row in reader:
            phase = row["phase"]
            require(phase in expected_phases and phase not in snapshots,
                    f"{snapshots_path}: invalid/duplicate snapshot")
            snapshots[phase] = {
                "monotonic_ns": int(row["monotonic_ns"]),
                "thread_count": int(row["thread_count"]),
                "vcpu_count": int(row["vcpu_count"]),
                "new_tid_count": int(row["new_tid_count"]),
            }
    gate(tuple(snapshots) == expected_phases,
         "QEMU TID namespace snapshots do not cover setup/window-start/window-stop", errors)

    host_to_container: dict[int, int] = {}
    container_to_host: dict[int, int] = {}
    container_tids: set[int] = set()
    with mapping_path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        require(reader.fieldnames == ["monotonic_ns", "host_tid", "container_tid", "nspid_chain", "comm"],
                f"{mapping_path}: invalid header")
        for row in reader:
            stamp = int(row["monotonic_ns"])
            host_tid = int(row["host_tid"])
            container_tid = int(row["container_tid"])
            require(stamp > 0 and host_tid > 0 and container_tid > 0,
                    f"{mapping_path}: invalid TID identity")
            chain = [int(value) for value in row["nspid_chain"].split(",")]
            require(chain and chain[0] == host_tid and chain[-1] == container_tid,
                    f"{mapping_path}: inconsistent NSpid chain for host TID {host_tid}")
            require(host_tid not in host_to_container and container_tid not in container_to_host,
                    f"{mapping_path}: duplicate host/container TID identity")
            host_to_container[host_tid] = container_tid
            container_to_host[container_tid] = host_tid
            container_tids.add(container_tid)

    phase_counts: dict[str, int] = {}
    vcpu_host_tids: set[int] = set()
    for phase in expected_phases:
        phase_path = mapping_path.with_name(f"tid-namespace-map-{phase}.tsv")
        require(phase_path.is_file(), f"missing QEMU TID namespace snapshot: {phase_path}")
        rows = []
        with phase_path.open(newline="", encoding="utf-8") as stream:
            reader = csv.DictReader(stream, delimiter="\t")
            require(reader.fieldnames == ["monotonic_ns", "host_tid", "container_tid", "nspid_chain", "comm"],
                    f"{phase_path}: invalid header")
            rows = list(reader)
        indices = set()
        for row in rows:
            host_tid = int(row["host_tid"])
            container_tid = int(row["container_tid"])
            chain = [int(value) for value in row["nspid_chain"].split(",")]
            require(chain and chain[0] == host_tid and chain[-1] == container_tid,
                    f"{phase_path}: inconsistent NSpid chain")
            require(host_to_container.get(host_tid) == container_tid,
                    f"{phase_path}: snapshot identity is absent from cumulative map")
            match = VCPU_COMM.fullmatch(row["comm"])
            if match:
                indices.add(int(match.group(1)))
                vcpu_host_tids.add(host_tid)
        gate(indices == set(range(expected_vcpus)),
             f"QEMU TID namespace snapshot {phase} has vCPUs {sorted(indices)}", errors)
        phase_counts[phase] = len(rows)
        gate(snapshots[phase]["thread_count"] == len(rows),
             f"QEMU TID namespace snapshot {phase} count disagrees", errors)
        gate(snapshots[phase]["vcpu_count"] == len(indices) == expected_vcpus,
             f"QEMU TID namespace snapshot {phase} vCPU count disagrees", errors)
    internal = {
        "host_to_container": host_to_container,
        "container_tids": container_tids,
        "vcpu_host_tids": vcpu_host_tids,
    }
    report = {
        "snapshots": phase_counts,
        "unique_host_tids": len(host_to_container),
        "unique_container_tids": len(container_tids),
        "vcpu_host_tids": sorted(vcpu_host_tids),
    }
    return report, internal


def parse_detection(control_path: pathlib.Path, detection_path: pathlib.Path,
                    mix: dict[str, Any], max_latency_ms: int,
                    errors: list[str]) -> dict[str, Any]:
    controls: dict[str, tuple[int, int]] = {}
    with control_path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        require(reader.fieldnames == ["monotonic_ns", "phase", "value"],
                f"{control_path}: invalid header")
        for row in reader:
            phase = row["phase"]
            require(phase in ("start", "stop") and phase not in controls,
                    f"{control_path}: invalid/duplicate transition")
            controls[phase] = (int(row["monotonic_ns"]), int(row["value"]))
    require(set(controls) == {"start", "stop"} and controls["start"][1] == 1 and controls["stop"][1] == 0,
            f"{control_path}: expected start=1 and stop=0")

    detections: dict[str, dict[str, int]] = {}
    with detection_path.open(newline="", encoding="utf-8") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        require(reader.fieldnames == ["phase", "request_monotonic_ns", "detected_monotonic_ns", "observed_monotonic_ns"],
                f"{detection_path}: invalid header")
        for row in reader:
            phase = row["phase"]
            require(phase in ("start", "stop") and phase not in detections,
                    f"{detection_path}: invalid/duplicate detection")
            detections[phase] = {key: int(value) for key, value in row.items() if key != "phase"}
    require(set(detections) == {"start", "stop"}, f"{detection_path}: incomplete transitions")

    result: dict[str, Any] = {}
    for phase, mix_key in (("start", "window_start_monotonic_ns"), ("stop", "window_stop_monotonic_ns")):
        row = detections[phase]
        request = row["request_monotonic_ns"]
        detected = row["detected_monotonic_ns"]
        observed = row["observed_monotonic_ns"]
        gate(request == controls[phase][0], f"instruction {phase} request timestamps disagree", errors)
        gate(detected == mix[mix_key], f"instruction {phase} detection does not match mix event", errors)
        latency_ns = detected - request
        observation_ns = observed - detected
        gate(0 <= latency_ns <= max_latency_ms * 1_000_000,
             f"instruction {phase} detection latency exceeds {max_latency_ms} ms", errors)
        gate(observation_ns >= 0, f"instruction {phase} host observation predates plugin detection", errors)
        result[phase] = {
            "request_monotonic_ns": request,
            "detected_monotonic_ns": detected,
            "observed_monotonic_ns": observed,
            "detection_latency_ms": latency_ns / 1_000_000,
            "observation_latency_ms": observation_ns / 1_000_000,
        }
    return result


def parse_mix(path: pathlib.Path, expected_vcpus: int, errors: list[str]) -> dict[str, Any]:
    descriptors: dict[int, tuple[str, int]] = {}
    samples = 0
    instructions = {"user": 0, "kernel": 0}
    raw_instruction_counters = {"user": 0, "kernel": 0}
    counter_snapshot_skew = {
        "mismatched_domain_epochs": 0,
        "maximum_absolute": 0,
        "maximum_absolute_by_domain": {"user": 0, "kernel": 0},
        "maximum_relative_ppm": 0,
        "maximum_domain_relative_ppm": 0,
        "sum_absolute": {"user": 0, "kernel": 0},
        "sign_reversals": {"user": 0, "kernel": 0},
    }
    epoch_snapshot_skew: list[dict[str, Any]] = []
    previous_skew = {"user": 0, "kernel": 0}
    window_start: dict[str, Any] | None = None
    window_stop: dict[str, Any] | None = None
    quality: dict[str, Any] | None = None
    header: dict[str, Any] | None = None
    last_timestamp = 0
    expected_epoch = 1

    with path.open("rb") as stream:
        for line_number, raw in enumerate(stream, 1):
            require(raw.strip() != b"", f"{path}:{line_number}: empty record")
            record = json_record(raw, path, line_number)
            require(record.get("schema") == MIX_SCHEMA, f"{path}:{line_number}: bad schema")
            record_type = record.get("type")
            timestamp = uint(record.get("monotonic_ns"), f"{path}:{line_number}.monotonic_ns")
            require(timestamp >= last_timestamp, f"{path}:{line_number}: timestamp regressed")
            last_timestamp = timestamp
            require(quality is None, f"{path}:{line_number}: record after final quality")

            if record_type == "header":
                require(line_number == 1 and header is None, f"{path}:{line_number}: duplicate/misplaced header")
                header = record
            elif record_type == "descriptor":
                descriptor_id = uint(record.get("id"), f"{path}:{line_number}.id")
                mnemonic = record.get("mnemonic")
                size = uint(record.get("size"), f"{path}:{line_number}.size")
                require(isinstance(mnemonic, str) and mnemonic != "", f"{path}:{line_number}: bad mnemonic")
                require(size > 0 and descriptor_id not in descriptors, f"{path}:{line_number}: bad descriptor")
                descriptors[descriptor_id] = (mnemonic, size)
            elif record_type in ("window_start", "window_stop"):
                target = window_start if record_type == "window_start" else window_stop
                require(target is None, f"{path}:{line_number}: duplicate {record_type}")
                if record_type == "window_start":
                    window_start = record
                else:
                    window_stop = record
            elif record_type == "sample":
                samples += 1
                require(record.get("window_id") == 1,
                        f"{path}:{line_number}: sample belongs to the wrong window")
                epoch = uint(record.get("epoch"), f"{path}:{line_number}.epoch")
                require(epoch == expected_epoch, f"{path}:{line_number}: non-contiguous epoch")
                expected_epoch += 1
                require(record.get("counter_regression") is False, f"{path}:{line_number}: counter regression")
                mix_rows = record.get("mix")
                require(isinstance(mix_rows, list), f"{path}:{line_number}: mix must be a list")
                observed = {"user": 0, "kernel": 0}
                seen_ids: set[int] = set()
                for row in mix_rows:
                    require(isinstance(row, dict), f"{path}:{line_number}: invalid mix row")
                    descriptor_id = uint(row.get("id"), f"{path}:{line_number}.mix.id")
                    require(descriptor_id in descriptors and descriptor_id not in seen_ids,
                            f"{path}:{line_number}: unknown/duplicate descriptor {descriptor_id}")
                    seen_ids.add(descriptor_id)
                    for domain in observed:
                        observed[domain] += uint(row.get(domain), f"{path}:{line_number}.mix.{domain}")
                epoch_skew = {"user": 0, "kernel": 0}
                for domain in observed:
                    mix_delta = uint(record.get("mix_instruction_delta", {}).get(domain),
                                     f"{path}:{line_number}.mix_instruction_delta.{domain}")
                    instruction_delta = uint(record.get("instruction_delta", {}).get(domain),
                                             f"{path}:{line_number}.instruction_delta.{domain}")
                    require(observed[domain] == mix_delta,
                            f"{path}:{line_number}: {domain} descriptor/mix totals disagree")
                    # vCPU 计数器和逐 descriptor 数组由插件分别读取，
                    # SMP 上无法在同一条主机指令中完成事务快照。边界处
                    # 的小偏差会在相邻 epoch 反向抵消；分段以可分解的
                    # descriptor mix 为准，并在全窗口检查原始计数器闭包。
                    skew = instruction_delta - mix_delta
                    epoch_skew[domain] = skew
                    if skew:
                        counter_snapshot_skew["mismatched_domain_epochs"] += 1
                    counter_snapshot_skew["maximum_absolute"] = max(
                        counter_snapshot_skew["maximum_absolute"], abs(skew)
                    )
                    counter_snapshot_skew["maximum_absolute_by_domain"][domain] = max(
                        counter_snapshot_skew["maximum_absolute_by_domain"][domain],
                        abs(skew),
                    )
                    counter_snapshot_skew["sum_absolute"][domain] += abs(skew)
                    if skew and previous_skew[domain] and (skew > 0) != (previous_skew[domain] > 0):
                        counter_snapshot_skew["sign_reversals"][domain] += 1
                    if skew:
                        previous_skew[domain] = skew
                    relative_ppm = (
                        abs(skew) * 1_000_000
                        // max(instruction_delta, mix_delta, 1)
                    )
                    counter_snapshot_skew["maximum_domain_relative_ppm"] = max(
                        counter_snapshot_skew["maximum_domain_relative_ppm"],
                        relative_ppm,
                    )
                    instructions[domain] += mix_delta
                    raw_instruction_counters[domain] += instruction_delta
                epoch_relative_ppm = (
                    sum(abs(value) for value in epoch_skew.values()) * 1_000_000
                    // max(sum(observed.values()), 1)
                )
                counter_snapshot_skew["maximum_relative_ppm"] = max(
                    counter_snapshot_skew["maximum_relative_ppm"],
                    epoch_relative_ppm,
                )
                epoch_snapshot_skew.append({
                    "epoch": epoch,
                    "user": epoch_skew["user"],
                    "kernel": epoch_skew["kernel"],
                    "maximum_relative_ppm": epoch_relative_ppm,
                })
            elif record_type == "control_error":
                errors.append(f"{path}:{line_number}: plugin reported a control error")
            elif record_type == "quality":
                quality = record
            else:
                raise ProfileError(f"{path}:{line_number}: unsupported record type {record_type!r}")

    require(header is not None and quality is not None, f"{path}: missing header/final quality")
    require(window_start is not None and window_stop is not None, f"{path}: incomplete measurement window")
    configured_vcpus = uint(header.get("configured_vcpus"), f"{path}: header configured_vcpus")
    gate(configured_vcpus == expected_vcpus, f"instruction mix configured_vcpus={configured_vcpus}, expected {expected_vcpus}", errors)
    gate(uint(quality.get("configured_vcpus"), "instruction mix quality.configured_vcpus") == configured_vcpus,
         "instruction mix header/final vCPU counts disagree", errors)
    translated_descriptor_count = uint(
        quality.get("descriptor_count"),
        "instruction mix quality.descriptor_count",
    )
    gate(0 < len(descriptors) <= translated_descriptor_count,
         "instruction mix emitted descriptor total exceeds translated vocabulary",
         errors)
    gate(quality.get("complete") is True, "instruction mix final quality is incomplete", errors)
    gate(uint(quality.get("windows"), "instruction mix quality.windows") == 1, "instruction mix did not record exactly one window", errors)
    gate(uint(quality.get("samples"), "instruction mix quality.samples") == samples and samples > 0,
         "instruction mix sample count is empty or inconsistent", errors)
    gate(uint(quality.get("start_detections"), "instruction mix quality.start_detections") == 1,
         "instruction mix start detection count is not one", errors)
    gate(uint(quality.get("stop_detections"), "instruction mix quality.stop_detections") == 1,
         "instruction mix stop detection count is not one", errors)
    gate(uint(quality.get("exit_stops"), "instruction mix quality.exit_stops") == 0,
         "instruction mix stopped only during QEMU exit", errors)
    for group_name in ("errors",):
        group = quality.get(group_name)
        require(isinstance(group, dict), f"instruction mix quality.{group_name} must be an object")
        for name, value in group.items():
            gate(uint(value, f"instruction mix quality.{group_name}.{name}") == 0,
                 f"instruction mix {group_name}.{name} is non-zero", errors)
    catalog = quality.get("catalog")
    require(isinstance(catalog, dict), "instruction mix quality.catalog must be an object")
    gate(catalog.get("enabled") is True, "instruction catalog was not enabled", errors)
    for name in ("write_errors", "dropped_blocks", "allocation_failures", "tracking_drops"):
        gate(uint(catalog.get(name), f"instruction mix quality.catalog.{name}") == 0,
             f"instruction catalog {name} is non-zero", errors)
    gate(sum(instructions.values()) > 0, "instruction mix contains no executed instructions", errors)
    cumulative_skew = {
        domain: raw_instruction_counters[domain] - instructions[domain]
        for domain in instructions
    }
    maximum_tb_instructions = uint(
        quality.get("max_tb_instructions"),
        "instruction mix quality.max_tb_instructions",
    )
    boundary_allowance = {
        domain: max(
            counter_snapshot_skew["maximum_absolute_by_domain"][domain],
            configured_vcpus * maximum_tb_instructions,
        )
        for domain in instructions
    }
    cumulative_relative_ppm = (
        sum(abs(value) for value in cumulative_skew.values()) * 1_000_000
        // max(sum(instructions.values()), 1)
    )
    exact_window_closure = all(value == 0 for value in cumulative_skew.values())
    bounded_window_closure = (
        all(abs(cumulative_skew[domain]) <= boundary_allowance[domain]
            for domain in instructions)
        and cumulative_relative_ppm <= 100
    )
    gate(exact_window_closure or bounded_window_closure,
         "instruction counter/descriptor window skew exceeds boundary or 100 ppm allowance",
         errors)
    gate(counter_snapshot_skew["maximum_relative_ppm"] <= 1_000,
         "instruction counter/descriptor epoch snapshot skew exceeds 1000 ppm",
         errors)
    gate(window_start.get("window_id") == window_stop.get("window_id") == 1,
         "instruction mix window identifiers disagree", errors)
    return {
        "configured_vcpus": configured_vcpus,
        "descriptor_count": len(descriptors),
        "translated_descriptor_count": translated_descriptor_count,
        "samples": samples,
        "instructions": instructions,
        "raw_instruction_counter_totals": raw_instruction_counters,
        "counter_snapshot_skew": {
            **counter_snapshot_skew,
            "maximum_allowed_relative_ppm": 1_000,
            "epoch_skew": epoch_snapshot_skew,
            "cumulative": cumulative_skew,
            "cumulative_relative_ppm": cumulative_relative_ppm,
            "maximum_allowed_cumulative_relative_ppm": 100,
            "boundary_allowance": boundary_allowance,
            "window_totals_close": exact_window_closure,
            "window_closure_mode": (
                "exact" if exact_window_closure else
                "bounded-boundary" if bounded_window_closure else
                "invalid"
            ),
            "interpretation": "bounded SMP epoch-boundary snapshot skew; descriptor mix is canonical",
        },
        "window_start_monotonic_ns": window_start["monotonic_ns"],
        "window_stop_monotonic_ns": window_stop["monotonic_ns"],
        "translated_blocks": uint(quality.get("translated_blocks"), "instruction mix translated_blocks"),
        "catalog_records": uint(catalog.get("records"), "instruction mix catalog.records"),
        "complete": quality.get("complete") is True,
    }


def parse_catalog(path: pathlib.Path, mix: dict[str, Any], tid_maps: dict[str, Any],
                  errors: list[str]) -> tuple[dict[str, Any], set[int]]:
    header: dict[str, Any] | None = None
    quality: dict[str, Any] | None = None
    guest_pcs: set[int] = set()
    translation_tids: set[int] = set()
    records = 0
    index_sum = 0
    index_square_sum = 0
    index_xor = 0

    with path.open("rb") as stream:
        for line_number, raw in enumerate(stream, 1):
            require(raw.strip() != b"", f"{path}:{line_number}: empty record")
            require(quality is None, f"{path}:{line_number}: record after final quality")
            if raw.startswith(b'{"schema":"mygo.riscv-tb-catalog.v1","type":"tb",'):
                index_match = CATALOG_INDEX.search(raw)
                pc_match = CATALOG_PC.search(raw)
                tid_match = CATALOG_TID.search(raw)
                require(index_match is not None and pc_match is not None and tid_match is not None,
                        f"{path}:{line_number}: malformed TB record")
                require(b'"descriptor_overflow":0,"decode_errors":0' in raw,
                        f"{path}:{line_number}: incomplete TB decoding")
                index = int(index_match.group(1))
                require(index > 0, f"{path}:{line_number}: invalid translation index")
                records += 1
                index_sum += index
                index_square_sum += index * index
                index_xor ^= index
                guest_pcs.add(int(pc_match.group(1), 16))
                translation_tids.add(int(tid_match.group(1)))
                continue

            record = json_record(raw, path, line_number)
            require(record.get("schema") == CATALOG_SCHEMA, f"{path}:{line_number}: bad schema")
            if record.get("type") == "header":
                require(line_number == 1 and header is None, f"{path}:{line_number}: duplicate/misplaced header")
                header = record
            elif record.get("type") == "quality":
                quality = record
            else:
                raise ProfileError(f"{path}:{line_number}: unsupported catalog record")

    require(header is not None and quality is not None, f"{path}: missing header/final quality")
    expected_sum = records * (records + 1) // 2
    expected_square_sum = records * (records + 1) * (2 * records + 1) // 6
    expected_xor = (records, 1, records + 1, 0)[records & 3]
    gate(index_sum == expected_sum and index_square_sum == expected_square_sum and index_xor == expected_xor,
         "instruction catalog translation indices are incomplete or duplicated", errors)
    translated_blocks = uint(quality.get("translated_blocks"), "catalog quality.translated_blocks")
    quality_records = uint(quality.get("records"), "catalog quality.records")
    gate(records > 0 and records == quality_records == translated_blocks,
         "instruction catalog record totals disagree", errors)
    gate(records == mix["catalog_records"] == mix["translated_blocks"],
         "instruction mix/catalog translated-block totals disagree", errors)
    for name in ("write_errors", "dropped_blocks", "tracking_drops"):
        gate(uint(quality.get(name), f"catalog quality.{name}") == 0,
             f"instruction catalog {name} is non-zero", errors)
    gate(translation_tids.issubset(tid_maps["container_tids"]),
         f"instruction catalog contains container TIDs absent from namespace snapshots: {sorted(translation_tids - tid_maps['container_tids'])}",
         errors)
    return {
        "records": records,
        "unique_guest_pcs": len(guest_pcs),
        "translated_blocks": translated_blocks,
        "translation_container_tids": sorted(translation_tids),
    }, guest_pcs


def parse_collector(path: pathlib.Path, expected_vcpus: int, tid_maps: dict[str, Any],
                    errors: list[str]) -> tuple[dict[str, Any], collections.Counter[int], collections.Counter[int]]:
    all_ips: collections.Counter[int] = collections.Counter()
    samples_by_tid: collections.Counter[int] = collections.Counter()
    thread_names: dict[int, str] = {}
    thread_attach_errors: dict[int, int] = {}
    tid_stats: list[tuple[int, ...]] = []
    gate_records: list[tuple[int, int, int]] = []
    quality: tuple[int, ...] | None = None
    actual_samples = 0
    actual_lost = 0
    last_record_type = 0

    with path.open("rb") as stream:
        raw_header = stream.read(TCG_HEADER.size)
        require(len(raw_header) == TCG_HEADER.size, f"{path}: truncated collector header")
        header = TCG_HEADER.unpack(raw_header)
        magic, version, header_size, endian_marker = header[:4]
        require(magic == b"RVTCGT1\0" and version == 1 and header_size == TCG_HEADER.size,
                f"{path}: unsupported collector header")
        require(endian_marker == 0x01020304, f"{path}: bad collector endian marker")
        while True:
            offset = stream.tell()
            raw_record = stream.read(TCG_RECORD.size)
            if raw_record == b"":
                break
            require(len(raw_record) == TCG_RECORD.size, f"{path}: truncated record header at {offset}")
            record_type, size, flags = TCG_RECORD.unpack(raw_record)
            require(size >= TCG_RECORD.size, f"{path}: invalid record size at {offset}")
            payload = stream.read(size - TCG_RECORD.size)
            require(len(payload) == size - TCG_RECORD.size, f"{path}: truncated record at {offset}")
            require(quality is None, f"{path}: record after final quality")
            last_record_type = record_type

            if record_type == 1:
                require(size == TCG_RECORD.size + TCG_SAMPLE.size, f"{path}: bad sample size")
                ip, timestamp, period, pid, tid, cpu, reserved = TCG_SAMPLE.unpack(payload)
                require(period == header[6] and reserved == 0,
                        f"{path}: malformed/adaptive sample period")
                all_ips[ip] += 1
                samples_by_tid[tid] += 1
                actual_samples += 1
            elif record_type == 2:
                require(size == TCG_RECORD.size + TCG_LOST.size, f"{path}: bad lost-record size")
                actual_lost += TCG_LOST.unpack(payload)[2]
            elif record_type == 3:
                require(size == TCG_RECORD.size + TCG_THREAD.size, f"{path}: bad thread-record size")
                values = TCG_THREAD.unpack(payload)
                tid = values[2]
                name = values[7].split(b"\0", 1)[0].decode("utf-8", "replace")
                require(tid not in thread_names, f"{path}: duplicate thread record for {tid}")
                thread_names[tid] = name
                thread_attach_errors[tid] = values[5]
            elif record_type == 4:
                if len(payload) == TCG_TID_STATS.size:
                    tid_stats.append(TCG_TID_STATS.unpack(payload))
                elif len(payload) == TCG_TID_STATS_LEGACY.size:
                    legacy = TCG_TID_STATS_LEGACY.unpack(payload)
                    tid_stats.append((*legacy[:8], 0, 0, *legacy[8:]))
                else:
                    raise ProfileError(f"{path}: bad TID-stats size")
            elif record_type == 5:
                require(size == TCG_RECORD.size + TCG_ATTACH_FAILURE.size, f"{path}: bad attach-failure size")
            elif record_type == 6:
                require(size == TCG_RECORD.size + TCG_GATE.size, f"{path}: bad gate-record size")
                gate_records.append(TCG_GATE.unpack(payload))
            elif record_type == 7:
                if len(payload) == TCG_QUALITY.size:
                    quality = TCG_QUALITY.unpack(payload)
                elif len(payload) == TCG_QUALITY_LEGACY.size:
                    legacy = TCG_QUALITY_LEGACY.unpack(payload)
                    quality = (*legacy[:10], 0, 0, *legacy[10:])
                else:
                    raise ProfileError(
                        f"{path}: incompatible quality-record size {size}"
                    )
            else:
                raise ProfileError(f"{path}: unsupported collector record type {record_type} at {offset}")

    require(quality is not None and last_record_type == 7, f"{path}: missing final collector quality")
    quality_names = (
        "time_ns", "runtime_ns", "gate_active_ns", "task_clock_ns",
        "time_enabled_ns", "time_running_ns", "samples_seen", "samples_written",
        "samples_discarded", "lost", "throttle_records", "unthrottle_records",
        "running_ratio_ppm", "loss_ratio_ppm", "tids_discovered", "tids_attached",
        "attach_failures", "gate_transitions", "malformed_records", "status",
    )
    q = dict(zip(quality_names, quality, strict=True))
    gate(q["status"] == 0, "TCG time collector final status is not good", errors)
    gate(q["samples_seen"] == q["samples_written"] == actual_samples and actual_samples > 0,
         "TCG time collector sample totals are empty or inconsistent", errors)
    gate(q["lost"] == actual_lost == 0, "TCG time collector lost samples", errors)
    gate(q["throttle_records"] == 0 and q["unthrottle_records"] == 0,
         "TCG time collector was throttled", errors)
    gate(q["attach_failures"] == 0 and q["tids_attached"] == q["tids_discovered"] > 0,
         "TCG time collector did not attach every discovered QEMU thread", errors)
    gate(q["malformed_records"] == 0, "TCG time collector saw malformed perf records", errors)
    gate(q["running_ratio_ppm"] >= 990_000, "TCG time collector perf running ratio is below 99%", errors)
    gate(q["gate_active_ns"] > 0, "TCG time collector gate was never active", errors)
    gate(q["gate_transitions"] == len(gate_records) and [row[1] for row in gate_records] == [0, 1, 0],
         "TCG time collector gate sequence is not exactly 0 -> 1 -> 0", errors)
    gate(all(error == 0 for error in thread_attach_errors.values()),
         "TCG time collector thread metadata contains attach failures", errors)
    gate(len(tid_stats) == q["tids_discovered"], "TCG time collector TID statistics are incomplete", errors)
    stats_tids = [row[10] for row in tid_stats]
    gate(len(set(stats_tids)) == len(stats_tids),
         "TCG time collector contains duplicate TID statistics", errors)
    for index, quality_name in (
        (1, "task_clock_ns"), (2, "time_enabled_ns"), (3, "time_running_ns"),
        (4, "samples_seen"), (5, "samples_written"),
        (6, "samples_discarded"), (7, "lost"),
        (8, "throttle_records"), (9, "unthrottle_records"),
    ):
        gate(sum(row[index] for row in tid_stats) == q[quality_name],
             f"TCG time collector per-TID {quality_name} total disagrees", errors)
    gate(all(row[11] == 0 and row[12] == 0 and row[13] == 0 for row in tid_stats),
         "TCG time collector per-TID status contains errors", errors)
    target_pid = header[5]
    gate(target_pid in tid_maps["host_to_container"],
         "QEMU leader is missing from TID namespace snapshots", errors)

    vcpu_tids: set[int] = set()
    vcpu_indices: set[int] = set()
    for tid, name in thread_names.items():
        match = VCPU_COMM.fullmatch(name)
        if match:
            vcpu_tids.add(tid)
            vcpu_indices.add(int(match.group(1)))
    gate(vcpu_indices == set(range(expected_vcpus)),
         f"TCG collector identified vCPU threads {sorted(vcpu_indices)}, expected 0..{expected_vcpus - 1}", errors)
    gate(vcpu_tids == tid_maps["vcpu_host_tids"],
         "collector and namespace snapshots disagree on QEMU vCPU host TIDs", errors)
    vcpu_ips: collections.Counter[int] = collections.Counter()
    for tid in vcpu_tids:
        # Keep the compact per-IP counters; individual timestamps are not needed
        # for the conservative union-of-generated-code coverage gate.
        if samples_by_tid[tid] == 0:
            continue
    if vcpu_tids:
        # Re-read samples once to avoid retaining millions of (IP,TID) tuples.
        with path.open("rb") as stream:
            stream.seek(TCG_HEADER.size)
            while True:
                raw_record = stream.read(TCG_RECORD.size)
                if not raw_record:
                    break
                record_type, size, flags = TCG_RECORD.unpack(raw_record)
                payload = stream.read(size - TCG_RECORD.size)
                if record_type == 1:
                    ip, timestamp, period, pid, tid, cpu, reserved = TCG_SAMPLE.unpack(payload)
                    if tid in vcpu_tids:
                        vcpu_ips[ip] += 1
    gate(sum(vcpu_ips.values()) > 0, "TCG time collector recorded no vCPU samples", errors)
    return {
        "target_pid": target_pid,
        "target_container_pid": tid_maps["host_to_container"].get(target_pid),
        "period_ns": header[6],
        "samples": actual_samples,
        "vcpu_samples": sum(vcpu_ips.values()),
        "threads_discovered": q["tids_discovered"],
        "threads_attached": q["tids_attached"],
        "vcpu_threads": sorted(vcpu_indices),
        "lost": q["lost"],
        "throttle_records": q["throttle_records"],
        "running_ratio_ppm": q["running_ratio_ppm"],
        "gate_active_ns": q["gate_active_ns"],
        "status": q["status"],
    }, all_ips, vcpu_ips


def parse_jitdump(path: pathlib.Path, catalog_pcs: set[int], all_ips: collections.Counter[int],
                  vcpu_ips: collections.Counter[int], tid_maps: dict[str, Any],
                  collector: dict[str, Any], errors: list[str]) -> dict[str, Any]:
    sorted_all_ips = sorted(all_ips)
    sorted_vcpu_ips = sorted(vcpu_ips)
    mapped_all: set[int] = set()
    mapped_vcpu: set[int] = set()
    records = 0
    loads = 0
    named_loads = 0
    catalog_mapped_loads = 0
    close_records = 0
    unknown_container_tids: set[int] = set()
    last_record_type: int | None = None

    with path.open("rb") as stream:
        raw_header = stream.read(JIT_HEADER.size)
        require(len(raw_header) == JIT_HEADER.size, f"{path}: truncated jitdump header")
        magic, version, header_size, elf_machine, pad, jit_pid, timestamp, flags = JIT_HEADER.unpack(raw_header)
        require(magic == JIT_MAGIC and version == 1 and header_size == JIT_HEADER.size,
                f"{path}: unsupported jitdump header")
        require(elf_machine == 62 and pad == 0, f"{path}: jitdump does not contain x86-64 TCG code")
        while True:
            offset = stream.tell()
            raw_record = stream.read(JIT_RECORD.size)
            if raw_record == b"":
                break
            require(len(raw_record) == JIT_RECORD.size, f"{path}: truncated jitdump record header at {offset}")
            record_type, size, record_timestamp = JIT_RECORD.unpack(raw_record)
            require(size >= JIT_RECORD.size, f"{path}: invalid jitdump record size at {offset}")
            payload = stream.read(size - JIT_RECORD.size)
            require(len(payload) == size - JIT_RECORD.size, f"{path}: truncated jitdump record at {offset}")
            require(record_type in (0, 1, 2, 3, 4), f"{path}: unsupported jitdump record type {record_type}")
            records += 1
            last_record_type = record_type
            if record_type == 3:
                close_records += 1
            if record_type != 0:
                continue
            require(len(payload) >= 41, f"{path}: truncated JIT_CODE_LOAD at {offset}")
            pid, tid, vma, code_address, code_size, code_index = struct.unpack_from("<IIQQQQ", payload)
            require(pid == jit_pid and code_size > 0 and code_size <= len(payload) - 41,
                    f"{path}: malformed JIT_CODE_LOAD at {offset}")
            name_end_limit = len(payload) - code_size
            name_end = payload.find(b"\0", 40, name_end_limit + 1)
            require(name_end == name_end_limit - 1, f"{path}: malformed JIT code name at {offset}")
            name = payload[40:name_end]
            loads += 1
            if tid not in tid_maps["container_tids"]:
                unknown_container_tids.add(tid)
            match = JIT_GUEST_NAME.fullmatch(name)
            if match:
                named_loads += 1
                if int(match.group(1), 16) in catalog_pcs:
                    catalog_mapped_loads += 1

            code_end = code_address + code_size
            require(code_end > code_address, f"{path}: overflowing JIT code range at {offset}")
            for sorted_ips, mapped in ((sorted_all_ips, mapped_all), (sorted_vcpu_ips, mapped_vcpu)):
                index = bisect.bisect_left(sorted_ips, code_address)
                while index < len(sorted_ips) and sorted_ips[index] < code_end:
                    mapped.add(sorted_ips[index])
                    index += 1

    require(records > 0 and loads > 0 and last_record_type is not None,
            f"{path}: jitdump contains no complete code records")
    gate(jit_pid == collector["target_container_pid"],
         "jitdump PID does not match the QEMU container PID namespace", errors)
    gate(not unknown_container_tids,
         f"jitdump contains container TIDs absent from namespace snapshots: {sorted(unknown_container_tids)}",
         errors)
    all_samples = sum(all_ips.values())
    vcpu_samples = sum(vcpu_ips.values())
    mapped_all_samples = sum(all_ips[ip] for ip in mapped_all)
    mapped_vcpu_samples = sum(vcpu_ips[ip] for ip in mapped_vcpu)
    return {
        "pid_namespace_pid": jit_pid,
        "records": records,
        "last_record_type": last_record_type,
        "close_records": close_records,
        "unknown_container_tids": sorted(unknown_container_tids),
        "code_loads": loads,
        "guest_named_loads": named_loads,
        "catalog_mapped_loads": catalog_mapped_loads,
        "catalog_mapping_ppm": catalog_mapped_loads * 1_000_000 // max(named_loads, 1),
        "all_samples": all_samples,
        "all_jit_mapped_samples": mapped_all_samples,
        "all_sample_mapping_ppm": mapped_all_samples * 1_000_000 // max(all_samples, 1),
        "vcpu_samples": vcpu_samples,
        "vcpu_jit_mapped_samples": mapped_vcpu_samples,
        "vcpu_sample_mapping_ppm": mapped_vcpu_samples * 1_000_000 // max(vcpu_samples, 1),
        # QEMU 10 在正常退出时不保证写 JIT_CODE_CLOSE。读到物理
        # EOF 且最后一条 record 完整，才是该版本可审计的完整性条件。
        "eof_record_boundary_complete": True,
        "code_close_required": False,
        "termination_model": "qemu10-complete-record-eof; JIT_CODE_CLOSE optional",
        "tail_complete": True,
    }


def write_report(path: pathlib.Path, report: dict[str, Any]) -> None:
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mix", required=True, type=pathlib.Path)
    parser.add_argument("--catalog", required=True, type=pathlib.Path)
    parser.add_argument("--samples", required=True, type=pathlib.Path)
    parser.add_argument("--jitdump", required=True, type=pathlib.Path)
    parser.add_argument("--tid-map", required=True, type=pathlib.Path)
    parser.add_argument("--tid-map-snapshots", required=True, type=pathlib.Path)
    parser.add_argument("--control", required=True, type=pathlib.Path)
    parser.add_argument("--detections", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--expected-vcpus", required=True, type=int)
    parser.add_argument("--max-transition-latency-ms", required=True, type=int)
    parser.add_argument("--min-jit-sample-mapping-ppm", required=True, type=int)
    parser.add_argument("--min-jit-catalog-mapping-ppm", required=True, type=int)
    parser.add_argument("--min-catalog-jit-coverage-ppm", type=int, default=999_000)
    arguments = parser.parse_args()
    report: dict[str, Any] = {"schema": REPORT_SCHEMA, "valid": False, "errors": []}
    try:
        require(arguments.expected_vcpus > 0, "expected vCPU count must be positive")
        require(arguments.max_transition_latency_ms > 0,
                "max transition latency must be positive")
        for name, value in (
            ("min-jit-sample-mapping-ppm", arguments.min_jit_sample_mapping_ppm),
            ("min-jit-catalog-mapping-ppm", arguments.min_jit_catalog_mapping_ppm),
            ("min-catalog-jit-coverage-ppm", arguments.min_catalog_jit_coverage_ppm),
        ):
            require(0 <= value <= 1_000_000, f"{name} must be in 0..1000000")
        for path in (
            arguments.mix, arguments.catalog, arguments.samples, arguments.jitdump,
            arguments.tid_map, arguments.tid_map_snapshots, arguments.control,
            arguments.detections,
        ):
            require(path.is_file() and path.stat().st_size > 0, f"missing or empty profile artifact: {path}")

        errors: list[str] = []
        tid_map_report, tid_maps = parse_tid_maps(
            arguments.tid_map, arguments.tid_map_snapshots,
            arguments.expected_vcpus, errors
        )
        mix = parse_mix(arguments.mix, arguments.expected_vcpus, errors)
        transitions = parse_detection(
            arguments.control, arguments.detections, mix,
            arguments.max_transition_latency_ms, errors
        )
        catalog, catalog_pcs = parse_catalog(arguments.catalog, mix, tid_maps, errors)
        collector, all_ips, vcpu_ips = parse_collector(
            arguments.samples, arguments.expected_vcpus, tid_maps, errors
        )
        jitdump = parse_jitdump(
            arguments.jitdump, catalog_pcs, all_ips, vcpu_ips, tid_maps,
            collector, errors
        )
        time_aware_mapping = profile_quality_summary(
            arguments.samples, arguments.jitdump, arguments.catalog,
            tid_namespace_path=arguments.tid_map,
        )
        translation_match = time_aware_mapping["translation_match"]
        vcpu_mapping = time_aware_mapping["vcpu_samples"]
        require(isinstance(vcpu_mapping, dict),
                "time-aware mapper did not identify vCPU samples")
        catalog_mapping_ppm = int(
            float(translation_match["guest_jit_match_ratio"]) * 1_000_000
        )
        catalog_records = uint(
            translation_match.get("catalog_records"),
            "time-aware catalog_records",
        )
        matched_catalog_records = uint(
            translation_match.get("matched"),
            "time-aware matched catalog records",
        )
        unmatched_catalog_records = uint(
            translation_match.get("unmatched_catalog_records"),
            "time-aware unmatched catalog records",
        )
        catalog_coverage_ppm = (
            matched_catalog_records * 1_000_000 // max(catalog_records, 1)
        )
        vcpu_task_clock_ns = uint(vcpu_mapping.get("task_clock_ns"),
                                  "time-aware vCPU task_clock_ns")
        task_clock_by_location = vcpu_mapping.get("task_clock_ns_by_location")
        require(isinstance(task_clock_by_location, dict),
                "time-aware mapper omitted task-clock locations")
        mapped_task_clock_ns = uint(task_clock_by_location.get("mapped-to-tcg", 0),
                                    "time-aware mapped TCG task_clock_ns")
        sample_mapping_ppm = mapped_task_clock_ns * 1_000_000 // max(vcpu_task_clock_ns, 1)
        gate(catalog_mapping_ppm >= arguments.min_jit_catalog_mapping_ppm,
             f"time-aware JIT guest-PC to catalog mapping is {catalog_mapping_ppm} ppm, below {arguments.min_jit_catalog_mapping_ppm}",
             errors)
        gate(catalog_coverage_ppm >= arguments.min_catalog_jit_coverage_ppm,
             f"time-aware catalog to JIT coverage is {catalog_coverage_ppm} ppm, below {arguments.min_catalog_jit_coverage_ppm}",
             errors)
        gate(sample_mapping_ppm >= arguments.min_jit_sample_mapping_ppm,
             f"time-aware vCPU task-clock to catalog mapping is {sample_mapping_ppm} ppm, below {arguments.min_jit_sample_mapping_ppm}",
             errors)
        report.update({
            "valid": not errors,
            "errors": errors,
            "thresholds": {
                "min_jit_sample_mapping_ppm": arguments.min_jit_sample_mapping_ppm,
                "min_jit_catalog_mapping_ppm": arguments.min_jit_catalog_mapping_ppm,
                "min_catalog_jit_coverage_ppm": arguments.min_catalog_jit_coverage_ppm,
            },
            "instruction_mix": mix,
            "transitions": transitions,
            "tid_namespace_map": tid_map_report,
            "catalog": catalog,
            "collector": collector,
            "jitdump": jitdump,
            "time_aware_mapping": time_aware_mapping,
            "mapping_quality": {
                "jit_guest_catalog_ppm": catalog_mapping_ppm,
                "catalog_jit_coverage_ppm": catalog_coverage_ppm,
                "unmatched_catalog_records": unmatched_catalog_records,
                "vcpu_task_clock_catalog_ppm": sample_mapping_ppm,
            },
        })
    except (OSError, ProfileError, ProfileIoError, struct.error, ValueError, KeyError) as error:
        report["errors"] = [str(error)]
    write_report(arguments.output, report)
    if not report["valid"]:
        for error in report["errors"]:
            print(f"RISC-V instruction profile quality: {error}", file=sys.stderr)
        return 1
    print(
        "RISC-V instruction profile quality: valid "
        f"samples={report['collector']['samples']} "
        f"vcpu_jit_mapping_ppm={report['mapping_quality']['vcpu_task_clock_catalog_ppm']} "
        f"jit_catalog_mapping_ppm={report['mapping_quality']['jit_guest_catalog_ppm']}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
