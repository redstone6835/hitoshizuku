#!/usr/bin/env python3
"""RISC-V TCG 指令耗时剖析文件的流式解析与时间关联。"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import enum
import json
import os
import re
import struct
from pathlib import Path
from typing import BinaryIO, Iterable, Iterator, Mapping, Sequence


RV_TCG_MAGIC = b"RVTCGT1\0"
RV_TCG_VERSION = 1
RV_TCG_ENDIAN_MARKER = 0x01020304
RV_TCG_HEADER = struct.Struct("<8sHHIQQQQIIQQ")
RV_TCG_RECORD_HEADER = struct.Struct("<HHI")

JIT_HEADER_MAGIC = 0x4A695444
JIT_HEADER = struct.Struct("<IIIIIIQQ")
JIT_RECORD_HEADER = struct.Struct("<IIQ")
JIT_CODE_LOAD = 0
JIT_CODE_MOVE = 1
JIT_CODE_CLOSE = 3
GUEST_CODE_NAME = re.compile(r"guest-0x([0-9a-fA-F]+)\Z")

CATALOG_SCHEMA = "mygo.riscv-tb-catalog.v1"


class ProfileIoError(RuntimeError):
    """输入文件不完整、格式错误或版本不受支持。"""


@dataclasses.dataclass(frozen=True)
class RvTcgFileHeader:
    start_monotonic_ns: int
    target_pid: int
    period_ns: int
    sample_type: int
    clock_id: int
    data_pages: int


@dataclasses.dataclass(frozen=True)
class PerfSample:
    ip: int
    time_ns: int
    period_ns: int
    pid: int
    tid: int
    cpu: int
    flags: int = 0

    @property
    def monotonic_ns(self) -> int:
        return self.time_ns

    @property
    def task_clock_ns(self) -> int:
        return self.period_ns


@dataclasses.dataclass(frozen=True)
class RvTcgLost:
    time_ns: int
    event_id: int
    lost: int
    tid: int
    flags: int = 0


@dataclasses.dataclass(frozen=True)
class RvTcgThread:
    time_ns: int
    pid: int
    tid: int
    real_uid: int
    effective_uid: int
    attach_errno: int
    comm: str
    flags: int = 0


@dataclasses.dataclass(frozen=True)
class RvTcgTidStats:
    time_ns: int
    task_clock_ns: int
    time_enabled_ns: int
    time_running_ns: int
    samples_seen: int
    samples_written: int
    samples_discarded: int
    lost: int
    throttle_records: int
    unthrottle_records: int
    tid: int
    attach_errno: int
    read_errno: int
    flags: int = 0


@dataclasses.dataclass(frozen=True)
class RvTcgAttachFailure:
    time_ns: int
    tid: int
    error: int
    effective_uid: int
    flags: int = 0


@dataclasses.dataclass(frozen=True)
class RvTcgGate:
    time_ns: int
    enabled: bool
    reason: int
    flags: int = 0


@dataclasses.dataclass(frozen=True)
class RvTcgQuality:
    time_ns: int
    runtime_ns: int
    gate_active_ns: int
    task_clock_ns: int
    time_enabled_ns: int
    time_running_ns: int
    samples_seen: int
    samples_written: int
    samples_discarded: int
    lost: int
    throttle_records: int
    unthrottle_records: int
    running_ratio_ppm: int
    loss_ratio_ppm: int
    tids_discovered: int
    tids_attached: int
    attach_failures: int
    gate_transitions: int
    malformed_records: int
    status: int
    flags: int = 0


@dataclasses.dataclass(frozen=True)
class RvTcgUnknown:
    record_type: int
    payload: bytes
    flags: int = 0


RvTcgRecord = (
    PerfSample
    | RvTcgLost
    | RvTcgThread
    | RvTcgTidStats
    | RvTcgAttachFailure
    | RvTcgGate
    | RvTcgQuality
    | RvTcgUnknown
)


def _read_exact(stream: BinaryIO, size: int, description: str) -> bytes:
    data = stream.read(size)
    if len(data) != size:
        raise ProfileIoError(
            f"{description} 被截断：需要 {size} 字节，实际 {len(data)} 字节"
        )
    return data


def read_rv_tcg_header(stream: BinaryIO) -> RvTcgFileHeader:
    """读取并验证 rv_tcg_time_collect.c 的小端文件头。"""

    raw = _read_exact(stream, RV_TCG_HEADER.size, "RV TCG 文件头")
    (
        magic,
        version,
        header_size,
        endian_marker,
        start_ns,
        target_pid,
        period_ns,
        sample_type,
        clock_id,
        data_pages,
        _reserved0,
        _reserved1,
    ) = RV_TCG_HEADER.unpack(raw)
    if magic != RV_TCG_MAGIC:
        raise ProfileIoError(f"错误的 RV TCG magic：{magic!r}")
    if version != RV_TCG_VERSION:
        raise ProfileIoError(f"不支持的 RV TCG 版本：{version}")
    if header_size != RV_TCG_HEADER.size:
        raise ProfileIoError(f"错误的 RV TCG 文件头长度：{header_size}")
    if endian_marker != RV_TCG_ENDIAN_MARKER:
        raise ProfileIoError("RV TCG 文件不是受支持的小端格式")
    return RvTcgFileHeader(
        start_monotonic_ns=start_ns,
        target_pid=target_pid,
        period_ns=period_ns,
        sample_type=sample_type,
        clock_id=clock_id,
        data_pages=data_pages,
    )


def _unpack_exact(payload: bytes, layout: struct.Struct, description: str) -> tuple:
    if len(payload) != layout.size:
        raise ProfileIoError(
            f"{description} 长度错误：需要 {layout.size} 字节，实际 {len(payload)} 字节"
        )
    return layout.unpack(payload)


def _parse_rv_tcg_record(record_type: int, flags: int, payload: bytes) -> RvTcgRecord:
    if record_type == 1:
        ip, time_ns, period_ns, pid, tid, cpu, _reserved = _unpack_exact(
            payload, struct.Struct("<QQQIIII"), "sample 记录"
        )
        return PerfSample(ip, time_ns, period_ns, pid, tid, cpu, flags)
    if record_type == 2:
        time_ns, event_id, lost, tid, _reserved = _unpack_exact(
            payload, struct.Struct("<QQQII"), "lost 记录"
        )
        return RvTcgLost(time_ns, event_id, lost, tid, flags)
    if record_type == 3:
        values = _unpack_exact(
            payload, struct.Struct("<QIIIIiI32s"), "thread 记录"
        )
        comm = values[7].split(b"\0", 1)[0].decode("utf-8", "replace")
        return RvTcgThread(*values[:6], comm, flags)
    if record_type == 4:
        # 采集器早期版本未输出 throttle/unthrottle，但沿用了 v1；按长度兼容。
        if len(payload) == struct.calcsize("<8QIiiI"):
            values = struct.unpack("<8QIiiI", payload)
            counters = (*values[:8], 0, 0)
            tail = values[8:11]
        elif len(payload) == struct.calcsize("<10QIiiI"):
            values = struct.unpack("<10QIiiI", payload)
            counters = values[:10]
            tail = values[10:13]
        else:
            raise ProfileIoError(f"tid-stats 记录长度错误：{len(payload)} 字节")
        return RvTcgTidStats(*counters, *tail, flags)
    if record_type == 5:
        time_ns, tid, error, effective_uid, _reserved = _unpack_exact(
            payload, struct.Struct("<QIiII"), "attach-failure 记录"
        )
        return RvTcgAttachFailure(time_ns, tid, error, effective_uid, flags)
    if record_type == 6:
        time_ns, enabled, reason = _unpack_exact(
            payload, struct.Struct("<QII"), "gate 记录"
        )
        if enabled not in (0, 1):
            raise ProfileIoError(f"gate.enabled 非布尔值：{enabled}")
        return RvTcgGate(time_ns, bool(enabled), reason, flags)
    if record_type == 7:
        # 同 tid-stats，兼容缺少两个 throttle 计数器的历史 v1 文件。
        if len(payload) == struct.calcsize("<12Q6I"):
            values = struct.unpack("<12Q6I", payload)
            prefix = values[:10]
            ratios = values[10:12]
            counters = (*prefix, 0, 0, *ratios)
            tail = values[12:]
        elif len(payload) == struct.calcsize("<14Q6I"):
            values = struct.unpack("<14Q6I", payload)
            counters = values[:14]
            tail = values[14:]
        else:
            raise ProfileIoError(f"quality 记录长度错误：{len(payload)} 字节")
        return RvTcgQuality(*counters, *tail, flags)
    return RvTcgUnknown(record_type, payload, flags)


def iter_rv_tcg_records(path: os.PathLike[str] | str) -> Iterator[RvTcgRecord]:
    """逐条读取全部采集记录；未知记录会原样保留。"""

    with Path(path).open("rb") as stream:
        read_rv_tcg_header(stream)
        while True:
            raw_header = stream.read(RV_TCG_RECORD_HEADER.size)
            if not raw_header:
                return
            if len(raw_header) != RV_TCG_RECORD_HEADER.size:
                raise ProfileIoError("RV TCG 记录头被截断")
            record_type, total_size, flags = RV_TCG_RECORD_HEADER.unpack(raw_header)
            if total_size < RV_TCG_RECORD_HEADER.size:
                raise ProfileIoError(f"RV TCG 记录长度非法：{total_size}")
            payload = _read_exact(
                stream, total_size - RV_TCG_RECORD_HEADER.size, "RV TCG 记录"
            )
            yield _parse_rv_tcg_record(record_type, flags, payload)


def read_rv_tcg_file_header(path: os.PathLike[str] | str) -> RvTcgFileHeader:
    with Path(path).open("rb") as stream:
        return read_rv_tcg_header(stream)


def sorted_perf_samples(path: os.PathLike[str] | str) -> list[PerfSample]:
    """只保留采样并按单调时钟排序；样本量远小于 10+GiB catalog。"""

    return sorted(
        (record for record in iter_rv_tcg_records(path) if isinstance(record, PerfSample)),
        key=lambda sample: (sample.time_ns, sample.tid, sample.ip),
    )


@dataclasses.dataclass(frozen=True)
class JitDumpHeader:
    elf_machine: int
    pid: int
    timestamp_ns: int
    flags: int


@dataclasses.dataclass(frozen=True)
class JitCodeLoad:
    timestamp_ns: int
    pid: int
    tid: int
    vma: int
    code_addr: int
    code_size: int
    code_index: int
    name: str
    code_bytes: bytes | None = None

    @property
    def guest_pc(self) -> int | None:
        match = GUEST_CODE_NAME.fullmatch(self.name)
        return int(match.group(1), 16) if match else None


@dataclasses.dataclass(frozen=True)
class JitCodeMove:
    timestamp_ns: int
    pid: int
    tid: int
    vma: int
    old_code_addr: int
    new_code_addr: int
    code_size: int
    code_index: int


@dataclasses.dataclass(frozen=True)
class JitCodeClose:
    timestamp_ns: int


@dataclasses.dataclass(frozen=True)
class JitOtherRecord:
    record_id: int
    total_size: int
    timestamp_ns: int


JitRecord = JitCodeLoad | JitCodeMove | JitCodeClose | JitOtherRecord


def _discard_exact(stream: BinaryIO, size: int, description: str) -> None:
    while size:
        chunk = stream.read(min(size, 1024 * 1024))
        if not chunk:
            raise ProfileIoError(f"{description} 被截断")
        size -= len(chunk)


def read_jitdump_header(stream: BinaryIO) -> JitDumpHeader:
    raw = _read_exact(stream, JIT_HEADER.size, "jitdump 文件头")
    magic, version, total_size, elf_machine, _pad, pid, timestamp, flags = (
        JIT_HEADER.unpack(raw)
    )
    if magic != JIT_HEADER_MAGIC:
        raise ProfileIoError(f"错误或非小端的 jitdump magic：0x{magic:08x}")
    if version != 1:
        raise ProfileIoError(f"不支持的 jitdump 版本：{version}")
    if total_size < JIT_HEADER.size:
        raise ProfileIoError(f"jitdump 文件头长度非法：{total_size}")
    _discard_exact(stream, total_size - JIT_HEADER.size, "jitdump 扩展文件头")
    return JitDumpHeader(elf_machine, pid, timestamp, flags)


def iter_jitdump_records(
    path: os.PathLike[str] | str, *, include_code: bool = False
) -> Iterator[JitRecord]:
    """流式读取 jitdump；默认跳过可能极大的主机机器码正文。"""

    load_layout = struct.Struct("<IIQQQQ")
    move_layout = struct.Struct("<IIQQQQQ")
    with Path(path).open("rb") as stream:
        read_jitdump_header(stream)
        previous_timestamp = -1
        while True:
            raw_header = stream.read(JIT_RECORD_HEADER.size)
            if not raw_header:
                return
            if len(raw_header) != JIT_RECORD_HEADER.size:
                raise ProfileIoError("jitdump 记录头被截断")
            record_id, total_size, timestamp = JIT_RECORD_HEADER.unpack(raw_header)
            if total_size < JIT_RECORD_HEADER.size:
                raise ProfileIoError(f"jitdump 记录长度非法：{total_size}")
            if timestamp < previous_timestamp:
                raise ProfileIoError("jitdump 记录时间戳发生倒退")
            previous_timestamp = timestamp
            payload_size = total_size - JIT_RECORD_HEADER.size
            if record_id == JIT_CODE_LOAD:
                if payload_size < load_layout.size + 1:
                    raise ProfileIoError("JIT_CODE_LOAD 记录过短")
                fixed = _read_exact(stream, load_layout.size, "JIT_CODE_LOAD 固定字段")
                pid, tid, vma, code_addr, code_size, code_index = load_layout.unpack(fixed)
                trailing = payload_size - load_layout.size
                if code_size > trailing - 1:
                    raise ProfileIoError("JIT_CODE_LOAD 的 code_size 超出记录")
                name_size = trailing - code_size
                raw_name = _read_exact(stream, name_size, "JIT_CODE_LOAD 名称")
                if raw_name[-1:] != b"\0" or b"\0" in raw_name[:-1]:
                    raise ProfileIoError("JIT_CODE_LOAD 名称没有唯一尾随 NUL")
                name = raw_name[:-1].decode("utf-8", "surrogateescape")
                if include_code:
                    code = _read_exact(stream, code_size, "JIT_CODE_LOAD 机器码")
                else:
                    _discard_exact(stream, code_size, "JIT_CODE_LOAD 机器码")
                    code = None
                yield JitCodeLoad(
                    timestamp,
                    pid,
                    tid,
                    vma,
                    code_addr,
                    code_size,
                    code_index,
                    name,
                    code,
                )
            elif record_id == JIT_CODE_MOVE:
                values = move_layout.unpack(
                    _read_exact(stream, move_layout.size, "JIT_CODE_MOVE")
                ) if payload_size == move_layout.size else None
                if values is None:
                    raise ProfileIoError(f"JIT_CODE_MOVE 长度错误：{total_size}")
                yield JitCodeMove(timestamp, *values)
            elif record_id == JIT_CODE_CLOSE:
                if payload_size:
                    raise ProfileIoError(f"JIT_CODE_CLOSE 长度错误：{total_size}")
                yield JitCodeClose(timestamp)
            else:
                _discard_exact(stream, payload_size, "jitdump 未使用记录")
                yield JitOtherRecord(record_id, total_size, timestamp)


@dataclasses.dataclass(frozen=True)
class CatalogInstruction:
    pc: int
    size: int
    raw_bytes: bytes
    bytes_complete: bool
    descriptor_id: int | None
    mnemonic: str


@dataclasses.dataclass(frozen=True)
class CatalogSource:
    path: Path
    offset: int
    length: int
    line_number: int


@dataclasses.dataclass(frozen=True)
class TbCatalogHeader:
    monotonic_ns: int
    target: str
    configured_vcpus: int
    seen_slots: int


@dataclasses.dataclass(frozen=True)
class TbCatalogRecord:
    monotonic_ns: int
    translation_begin_ns: int
    host_tid: int
    translation_index: int
    guest_pc: int
    mode: str
    instruction_count: int
    duplicate_pc: bool
    duplicate_exact: bool
    descriptor_overflow: int
    decode_errors: int
    instructions: tuple[CatalogInstruction, ...] | None
    source: CatalogSource

    def materialize(self) -> TbCatalogRecord:
        """按文件偏移延迟加载完整 2/4-byte 指令表。"""

        if self.instructions is not None:
            return self
        raw = _read_catalog_line(self.source)
        return _parse_catalog_tb(raw, self.source, include_instructions=True)


@dataclasses.dataclass(frozen=True)
class TbCatalogQuality:
    monotonic_ns: int
    translated_blocks: int
    records: int
    write_errors: int
    dropped_blocks: int
    duplicate_pc: int
    duplicate_exact: int
    tracking_drops: int


CatalogRecord = TbCatalogHeader | TbCatalogRecord | TbCatalogQuality


def _json_int(value: object, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ProfileIoError(f"catalog 字段 {name} 不是非负整数")
    return value


def _json_bool(value: object, name: str) -> bool:
    if not isinstance(value, bool):
        raise ProfileIoError(f"catalog 字段 {name} 不是布尔值")
    return value


def _json_str(value: object, name: str) -> str:
    if not isinstance(value, str):
        raise ProfileIoError(f"catalog 字段 {name} 不是字符串")
    return value


def _hex_int(value: object, name: str) -> int:
    text = _json_str(value, name)
    try:
        result = int(text, 0)
    except ValueError as error:
        raise ProfileIoError(f"catalog 字段 {name} 不是整数地址") from error
    if result < 0:
        raise ProfileIoError(f"catalog 字段 {name} 是负地址")
    return result


def _parse_catalog_tb(
    value: Mapping[str, object], source: CatalogSource, *, include_instructions: bool
) -> TbCatalogRecord:
    raw_instructions = value.get("instructions")
    if not isinstance(raw_instructions, list):
        raise ProfileIoError("catalog instructions 不是数组")
    instruction_count = _json_int(value.get("instruction_count"), "instruction_count")
    if instruction_count != len(raw_instructions):
        raise ProfileIoError("catalog instruction_count 与指令数组长度不符")
    instructions: list[CatalogInstruction] | None = [] if include_instructions else None
    if instructions is not None:
        for index, raw_instruction in enumerate(raw_instructions):
            if not isinstance(raw_instruction, dict):
                raise ProfileIoError(f"catalog instructions[{index}] 不是对象")
            size = _json_int(raw_instruction.get("size"), f"instructions[{index}].size")
            raw_hex = _json_str(raw_instruction.get("bytes"), f"instructions[{index}].bytes")
            try:
                raw_bytes = bytes.fromhex(raw_hex)
            except ValueError as error:
                raise ProfileIoError(f"catalog instructions[{index}].bytes 非法") from error
            complete = _json_bool(
                raw_instruction.get("bytes_complete"),
                f"instructions[{index}].bytes_complete",
            )
            if complete and len(raw_bytes) != size:
                raise ProfileIoError(f"catalog instructions[{index}] 完整字节长度错误")
            descriptor = raw_instruction.get("descriptor_id")
            if descriptor is not None:
                descriptor = _json_int(descriptor, f"instructions[{index}].descriptor_id")
            instructions.append(
                CatalogInstruction(
                    _hex_int(raw_instruction.get("pc"), f"instructions[{index}].pc"),
                    size,
                    raw_bytes,
                    complete,
                    descriptor,
                    _json_str(
                        raw_instruction.get("mnemonic"),
                        f"instructions[{index}].mnemonic",
                    ),
                )
            )
    mode = _json_str(value.get("mode"), "mode")
    if mode not in ("user", "kernel"):
        raise ProfileIoError(f"catalog mode 不受支持：{mode}")
    return TbCatalogRecord(
        monotonic_ns=_json_int(value.get("monotonic_ns"), "monotonic_ns"),
        translation_begin_ns=_json_int(
            value.get("translation_begin_ns"), "translation_begin_ns"
        ),
        host_tid=_json_int(value.get("host_tid"), "host_tid"),
        translation_index=_json_int(value.get("translation_index"), "translation_index"),
        guest_pc=_hex_int(value.get("guest_pc"), "guest_pc"),
        mode=mode,
        instruction_count=instruction_count,
        duplicate_pc=_json_bool(value.get("duplicate_pc"), "duplicate_pc"),
        duplicate_exact=_json_bool(value.get("duplicate_exact"), "duplicate_exact"),
        descriptor_overflow=_json_int(
            value.get("descriptor_overflow"), "descriptor_overflow"
        ),
        decode_errors=_json_int(value.get("decode_errors"), "decode_errors"),
        instructions=tuple(instructions) if instructions is not None else None,
        source=source,
    )


def _read_catalog_line(source: CatalogSource) -> Mapping[str, object]:
    with source.path.open("rb") as stream:
        stream.seek(source.offset)
        raw = _read_exact(stream, source.length, f"catalog 第 {source.line_number} 行")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProfileIoError(f"catalog 第 {source.line_number} 行 JSON 非法") from error
    if not isinstance(value, dict):
        raise ProfileIoError(f"catalog 第 {source.line_number} 行不是对象")
    return value


def iter_tb_catalog(
    path: os.PathLike[str] | str, *, include_instructions: bool = True
) -> Iterator[CatalogRecord]:
    """逐行读取 catalog；关闭指令加载可让 1GB+ 文件保持有界内存。"""

    catalog_path = Path(path)
    with catalog_path.open("rb") as stream:
        line_number = 0
        while True:
            offset = stream.tell()
            raw = stream.readline()
            if not raw:
                return
            line_number += 1
            source = CatalogSource(catalog_path, offset, len(raw), line_number)
            try:
                value = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise ProfileIoError(f"catalog 第 {line_number} 行 JSON 非法") from error
            if not isinstance(value, dict) or value.get("schema") != CATALOG_SCHEMA:
                raise ProfileIoError(f"catalog 第 {line_number} 行 schema 非法")
            record_type = value.get("type")
            if record_type == "header":
                yield TbCatalogHeader(
                    _json_int(value.get("monotonic_ns"), "monotonic_ns"),
                    _json_str(value.get("target"), "target"),
                    _json_int(value.get("configured_vcpus"), "configured_vcpus"),
                    _json_int(value.get("seen_slots"), "seen_slots"),
                )
            elif record_type == "tb":
                yield _parse_catalog_tb(
                    value, source, include_instructions=include_instructions
                )
            elif record_type == "quality":
                yield TbCatalogQuality(
                    *(
                        _json_int(value.get(name), name)
                        for name in (
                            "monotonic_ns",
                            "translated_blocks",
                            "records",
                            "write_errors",
                            "dropped_blocks",
                            "duplicate_pc",
                            "duplicate_exact",
                            "tracking_drops",
                        )
                    )
                )
            else:
                raise ProfileIoError(f"catalog 第 {line_number} 行 type 非法：{record_type!r}")


@dataclasses.dataclass(frozen=True)
class TidNamespaceEntry:
    monotonic_ns: int
    host_tid: int
    container_tid: int
    nspid_chain: tuple[int, ...]
    comm: str


@dataclasses.dataclass(frozen=True)
class TidNamespaceMap:
    entries: tuple[TidNamespaceEntry, ...]

    def by_host_tid(self) -> dict[int, TidNamespaceEntry]:
        return {entry.host_tid: entry for entry in self.entries}

    def by_container_tid(self) -> dict[int, TidNamespaceEntry]:
        return {entry.container_tid: entry for entry in self.entries}


def read_tid_namespace_tsv(path: os.PathLike[str] | str) -> TidNamespaceMap:
    """读取 harness 在 QEMU 存活期间固化的 host/container NSpid 映射。"""

    entries: list[TidNamespaceEntry] = []
    with Path(path).open("r", encoding="utf-8", newline="") as stream:
        expected = "monotonic_ns\thost_tid\tcontainer_tid\tnspid_chain\tcomm"
        header = stream.readline().rstrip("\r\n")
        if header != expected:
            raise ProfileIoError(f"TID namespace TSV 表头非法：{header!r}")
        for line_number, raw in enumerate(stream, 2):
            fields = raw.rstrip("\r\n").split("\t", 4)
            if len(fields) != 5:
                raise ProfileIoError(f"TID namespace TSV 第 {line_number} 行字段数错误")
            try:
                timestamp, host_tid, container_tid = map(int, fields[:3])
                chain = tuple(int(item) for item in fields[3].split(","))
            except ValueError as error:
                raise ProfileIoError(f"TID namespace TSV 第 {line_number} 行整数非法") from error
            if not chain or chain[0] != host_tid or chain[-1] != container_tid:
                raise ProfileIoError(f"TID namespace TSV 第 {line_number} 行 NSpid 链不一致")
            entries.append(
                TidNamespaceEntry(timestamp, host_tid, container_tid, chain, fields[4].strip())
            )
    if len({entry.host_tid for entry in entries}) != len(entries):
        raise ProfileIoError("TID namespace TSV 存在重复 host_tid")
    if len({entry.container_tid for entry in entries}) != len(entries):
        raise ProfileIoError("TID namespace TSV 存在重复 container_tid")
    return TidNamespaceMap(tuple(entries))


def read_live_tid_namespace_map(target_pid: int) -> TidNamespaceMap:
    """从存活进程的 /proc status 读取 NSpid；调用方应立即持久化。"""

    entries: list[TidNamespaceEntry] = []
    task_root = Path(f"/proc/{target_pid}/task")
    for task in sorted(task_root.iterdir(), key=lambda item: int(item.name)):
        status: dict[str, str] = {}
        for line in (task / "status").read_text(encoding="utf-8").splitlines():
            if ":" in line:
                name, value = line.split(":", 1)
                status[name] = value.strip()
        host_tid = int(task.name)
        try:
            chain = tuple(int(item) for item in status["NSpid"].split())
        except (KeyError, ValueError) as error:
            raise ProfileIoError(f"无法读取 host tid {host_tid} 的 NSpid") from error
        if not chain or chain[0] != host_tid:
            raise ProfileIoError(f"host tid {host_tid} 的 NSpid 链非法")
        entries.append(TidNamespaceEntry(0, host_tid, chain[-1], chain, status.get("Name", "")))
    return TidNamespaceMap(tuple(entries))


@dataclasses.dataclass(frozen=True)
class MatchedJitLoad:
    load: JitCodeLoad
    catalog: TbCatalogRecord | None

    @property
    def timestamp_ns(self) -> int:
        return self.load.timestamp_ns


MatchedJitRecord = MatchedJitLoad | JitCodeMove | JitCodeClose


@dataclasses.dataclass
class MatchStatistics:
    catalog_records: int = 0
    jit_loads: int = 0
    guest_jit_loads: int = 0
    matched_loads: int = 0
    unmatched_guest_loads: int = 0
    non_guest_loads: int = 0
    unmatched_catalog_records: int = 0
    catalog_container_tids: set[int] = dataclasses.field(default_factory=set)
    jit_container_tids: set[int] = dataclasses.field(default_factory=set)

    @property
    def catalog_match_ratio(self) -> float:
        return self.matched_loads / self.catalog_records if self.catalog_records else 0.0

    @property
    def guest_jit_match_ratio(self) -> float:
        return self.matched_loads / self.guest_jit_loads if self.guest_jit_loads else 0.0

    @property
    def observed_vcpu_container_tids(self) -> set[int]:
        return self.catalog_container_tids | self.jit_container_tids


def _catalog_tb_records(
    path: os.PathLike[str] | str, *, include_instructions: bool
) -> Iterator[TbCatalogRecord]:
    for record in iter_tb_catalog(path, include_instructions=include_instructions):
        if isinstance(record, TbCatalogRecord):
            yield record


def iter_matched_jit_records(
    catalog_path: os.PathLike[str] | str,
    jitdump_path: os.PathLike[str] | str,
    *,
    stats: MatchStatistics | None = None,
    include_instructions: bool = False,
) -> Iterator[MatchedJitRecord]:
    """按 ``(container_tid, guest_pc)`` 与时间 FIFO 一一关联翻译记录。"""

    result = stats if stats is not None else MatchStatistics()
    catalog_iter = _catalog_tb_records(
        catalog_path, include_instructions=include_instructions
    )
    pending: dict[tuple[int, int], collections.deque[TbCatalogRecord]] = (
        collections.defaultdict(collections.deque)
    )
    next_catalog = next(catalog_iter, None)

    def enqueue_until(timestamp_ns: int | None) -> None:
        nonlocal next_catalog
        while next_catalog is not None and (
            timestamp_ns is None or next_catalog.monotonic_ns <= timestamp_ns
        ):
            result.catalog_records += 1
            result.catalog_container_tids.add(next_catalog.host_tid)
            pending[(next_catalog.host_tid, next_catalog.guest_pc)].append(next_catalog)
            next_catalog = next(catalog_iter, None)

    for record in iter_jitdump_records(jitdump_path):
        if isinstance(record, JitOtherRecord):
            continue
        enqueue_until(record.timestamp_ns)
        if isinstance(record, JitCodeLoad):
            result.jit_loads += 1
            result.jit_container_tids.add(record.tid)
            guest_pc = record.guest_pc
            catalog = None
            if guest_pc is None:
                result.non_guest_loads += 1
            else:
                result.guest_jit_loads += 1
                queue = pending.get((record.tid, guest_pc))
                if queue:
                    catalog = queue.popleft()
                    result.matched_loads += 1
                else:
                    result.unmatched_guest_loads += 1
            yield MatchedJitLoad(record, catalog)
        else:
            yield record
    enqueue_until(None)
    result.unmatched_catalog_records = sum(len(queue) for queue in pending.values())


class SampleLocation(enum.StrEnum):
    MAPPED_TCG = "mapped-to-tcg"
    NATIVE_QEMU = "native-qemu"
    UNKNOWN = "unknown"


@dataclasses.dataclass(frozen=True)
class MappedPerfSample:
    sample: PerfSample
    location: SampleLocation
    load: JitCodeLoad | None
    catalog: TbCatalogRecord | None
    code_offset: int | None
    container_tid: int | None = None


@dataclasses.dataclass
class _ActiveMapping:
    token: int
    code_addr: int
    code_size: int
    load: JitCodeLoad
    catalog: TbCatalogRecord | None

    @property
    def end(self) -> int:
        return self.code_addr + self.code_size


class TimeAwareJitMap:
    """按 jitdump 时间推进的页索引，正确处理 MOVE、CLOSE 与地址复用。"""

    def __init__(self, records: Iterable[MatchedJitRecord], *, page_size: int = 4096):
        if page_size <= 0 or page_size & (page_size - 1):
            raise ValueError("page_size 必须是正的 2 次幂")
        self._records = iter(records)
        self._next = next(self._records, None)
        self._page_shift = page_size.bit_length() - 1
        self._pages: dict[int, dict[int, _ActiveMapping]] = {}
        self._by_token: dict[int, _ActiveMapping] = {}
        self._by_index: dict[int, _ActiveMapping] = {}
        self._token = 0
        self._last_event_ns = -1

    def _mapping_pages(self, mapping: _ActiveMapping) -> range:
        if mapping.code_size == 0:
            return range(0)
        return range(
            mapping.code_addr >> self._page_shift,
            ((mapping.end - 1) >> self._page_shift) + 1,
        )

    def _remove(self, mapping: _ActiveMapping) -> None:
        for page in self._mapping_pages(mapping):
            bucket = self._pages.get(page)
            if bucket is not None:
                bucket.pop(mapping.token, None)
                if not bucket:
                    self._pages.pop(page, None)
        self._by_token.pop(mapping.token, None)
        if self._by_index.get(mapping.load.code_index) is mapping:
            self._by_index.pop(mapping.load.code_index, None)

    def _install(
        self,
        load: JitCodeLoad,
        catalog: TbCatalogRecord | None,
        *,
        code_addr: int | None = None,
        code_size: int | None = None,
    ) -> None:
        start = load.code_addr if code_addr is None else code_addr
        size = load.code_size if code_size is None else code_size
        if size == 0:
            return
        end = start + size
        candidates: dict[int, _ActiveMapping] = {}
        first_page = start >> self._page_shift
        last_page = (end - 1) >> self._page_shift
        for page in range(first_page, last_page + 1):
            candidates.update(self._pages.get(page, {}))
        for active in candidates.values():
            if active.code_addr < end and start < active.end:
                self._remove(active)
        self._token += 1
        mapping = _ActiveMapping(self._token, start, size, load, catalog)
        self._by_token[mapping.token] = mapping
        self._by_index[load.code_index] = mapping
        for page in self._mapping_pages(mapping):
            self._pages.setdefault(page, {})[mapping.token] = mapping

    def _apply(self, record: MatchedJitRecord) -> None:
        if record.timestamp_ns < self._last_event_ns:
            raise ProfileIoError("匹配后的 jitdump 时间戳发生倒退")
        self._last_event_ns = record.timestamp_ns
        if isinstance(record, MatchedJitLoad):
            self._install(record.load, record.catalog)
        elif isinstance(record, JitCodeMove):
            active = self._by_index.get(record.code_index)
            if active is not None:
                self._remove(active)
                self._install(
                    active.load,
                    active.catalog,
                    code_addr=record.new_code_addr,
                    code_size=record.code_size,
                )
        else:
            for mapping in tuple(self._by_token.values()):
                self._remove(mapping)

    def _advance(self, timestamp_ns: int) -> None:
        while self._next is not None and self._next.timestamp_ns <= timestamp_ns:
            self._apply(self._next)
            self._next = next(self._records, None)

    def _lookup(self, ip: int) -> _ActiveMapping | None:
        candidates = self._pages.get(ip >> self._page_shift, {})
        matches = [mapping for mapping in candidates.values() if mapping.code_addr <= ip < mapping.end]
        return max(matches, key=lambda mapping: mapping.token) if matches else None

    def drain(self) -> None:
        """处理预取记录并耗尽底层流，使一一匹配统计完成最终结算。"""

        while self._next is not None:
            self._apply(self._next)
            self._next = next(self._records, None)

    def map_sorted_samples(
        self,
        samples: Iterable[PerfSample],
        *,
        tid_namespace: TidNamespaceMap | None = None,
    ) -> Iterator[MappedPerfSample]:
        """映射已按时间排序的样本，产出 TCG/native/unknown 三类质量标签。"""

        host_to_container = tid_namespace.by_host_tid() if tid_namespace else {}
        previous_time = -1
        for sample in samples:
            if sample.time_ns < previous_time:
                raise ProfileIoError("perf 样本没有按单调时钟排序")
            previous_time = sample.time_ns
            self._advance(sample.time_ns)
            mapping = self._lookup(sample.ip)
            namespace = host_to_container.get(sample.tid)
            container_tid = namespace.container_tid if namespace else None
            if mapping is None:
                yield MappedPerfSample(
                    sample, SampleLocation.NATIVE_QEMU, None, None, None, container_tid
                )
            elif mapping.catalog is None:
                yield MappedPerfSample(
                    sample,
                    SampleLocation.UNKNOWN,
                    mapping.load,
                    None,
                    sample.ip - mapping.code_addr,
                    container_tid,
                )
            else:
                yield MappedPerfSample(
                    sample,
                    SampleLocation.MAPPED_TCG,
                    mapping.load,
                    mapping.catalog,
                    sample.ip - mapping.code_addr,
                    container_tid,
                )


def profile_quality_summary(
    samples_path: os.PathLike[str] | str,
    jitdump_path: os.PathLike[str] | str,
    catalog_path: os.PathLike[str] | str,
    *,
    tid_namespace_path: os.PathLike[str] | str | None = None,
) -> dict[str, object]:
    """运行完整流式关联并返回不会误判 native 样本的质量摘要。"""

    samples = sorted_perf_samples(samples_path)
    namespace = read_tid_namespace_tsv(tid_namespace_path) if tid_namespace_path else None
    match = MatchStatistics()
    records = iter_matched_jit_records(catalog_path, jitdump_path, stats=match)
    mapper = TimeAwareJitMap(records)
    locations: collections.Counter[str] = collections.Counter()
    periods: collections.Counter[str] = collections.Counter()
    host_locations: collections.Counter[tuple[int, str]] = collections.Counter()
    host_periods: collections.Counter[tuple[int, str]] = collections.Counter()
    for mapped in mapper.map_sorted_samples(samples, tid_namespace=namespace):
        locations[mapped.location.value] += 1
        periods[mapped.location.value] += mapped.sample.period_ns
        host_locations[(mapped.sample.tid, mapped.location.value)] += 1
        host_periods[(mapped.sample.tid, mapped.location.value)] += mapped.sample.period_ns
    # 最后一个样本之后仍可能有翻译；包括 mapper 已经预取但尚未应用的一条。
    mapper.drain()
    total = sum(locations.values())
    total_period = sum(periods.values())
    vcpu_samples: dict[str, object] | None = None
    if namespace is not None:
        by_container = namespace.by_container_tid()
        vcpu_container_tids = match.observed_vcpu_container_tids
        vcpu_host_tids = {
            by_container[container_tid].host_tid
            for container_tid in vcpu_container_tids
            if container_tid in by_container
        }
        vcpu_locations: collections.Counter[str] = collections.Counter()
        vcpu_periods: collections.Counter[str] = collections.Counter()
        for (host_tid, location), count in host_locations.items():
            if host_tid in vcpu_host_tids:
                vcpu_locations[location] += count
        for (host_tid, location), period_ns in host_periods.items():
            if host_tid in vcpu_host_tids:
                vcpu_periods[location] += period_ns
        vcpu_total = sum(vcpu_locations.values())
        vcpu_total_period = sum(vcpu_periods.values())
        vcpu_samples = {
            "total": vcpu_total,
            "task_clock_ns": vcpu_total_period,
            "counts": dict(vcpu_locations),
            "task_clock_ns_by_location": dict(vcpu_periods),
            "mapped_tcg_ratio": vcpu_locations[SampleLocation.MAPPED_TCG.value]
            / vcpu_total
            if vcpu_total
            else 0.0,
            "host_tids": sorted(vcpu_host_tids),
            "container_tids": sorted(vcpu_container_tids),
        }
    return {
        "schema": "mygo.riscv-instruction-profile-io.v1",
        "samples": {
            "total": total,
            "task_clock_ns": total_period,
            "counts": dict(locations),
            "task_clock_ns_by_location": dict(periods),
            "mapped_tcg_ratio": locations[SampleLocation.MAPPED_TCG.value] / total
            if total
            else 0.0,
        },
        "vcpu_samples": vcpu_samples,
        "translation_match": {
            "catalog_records": match.catalog_records,
            "jit_loads": match.jit_loads,
            "guest_jit_loads": match.guest_jit_loads,
            "matched": match.matched_loads,
            "unmatched_guest_loads": match.unmatched_guest_loads,
            "non_guest_loads": match.non_guest_loads,
            "unmatched_catalog_records": match.unmatched_catalog_records,
            "catalog_match_ratio": match.catalog_match_ratio,
            "guest_jit_match_ratio": match.guest_jit_match_ratio,
            "catalog_container_tids": sorted(match.catalog_container_tids),
            "jit_container_tids": sorted(match.jit_container_tids),
            "observed_vcpu_container_tids": sorted(match.observed_vcpu_container_tids),
        },
        "tid_namespace": [dataclasses.asdict(entry) for entry in namespace.entries]
        if namespace
        else None,
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=Path, required=True)
    parser.add_argument("--jitdump", type=Path, required=True)
    parser.add_argument("--catalog", type=Path, required=True)
    parser.add_argument("--tid-namespace", type=Path)
    arguments = parser.parse_args(argv)
    try:
        summary = profile_quality_summary(
            arguments.samples,
            arguments.jitdump,
            arguments.catalog,
            tid_namespace_path=arguments.tid_namespace,
        )
    except (OSError, ProfileIoError) as error:
        parser.exit(2, f"rv_instruction_profile_io.py: {error}\n")
    json.dump(summary, fp=__import__("sys").stdout, ensure_ascii=False, indent=2)
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
