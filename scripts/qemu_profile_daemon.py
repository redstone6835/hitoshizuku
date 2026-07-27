#!/usr/bin/env python3
"""QEMU 宿主机侧的低频栈采样与阶段计时守护进程。"""

from __future__ import annotations

import argparse
import bisect
import dataclasses
import datetime as dt
import errno
import hashlib
import json
import os
import re
import selectors
import shlex
import signal
import socket
import stat
import struct
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path
from typing import Any, Callable, Iterable, Sequence


SCHEMA = "mygo.qemu-profile.v1"
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
CARGO_PROGRESS_RE = re.compile(r"(?<![0-9])([0-9]{1,3})/446(?![0-9])")
CARGO_COMPILE_RE = re.compile(r"\bCompiling\s+([^\s]+)")
MARKER_RE = re.compile(r"@@([A-Z][A-Z0-9_]*)\b(.*)")
GDB_THREAD_RE = re.compile(r"^Thread\s+(\d+).*?\(CPU#([0-9]+)\s+\[[^]]+\]\).*:$")
GDB_FRAME_RE = re.compile(r"^#([0-9]+)\s+(.*)$")
GDB_ADDRESS_RE = re.compile(r"^(0x[0-9a-fA-F]+)\s+in\s+(.*)$")
VCPU_COMM_RE = re.compile(r"^CPU\s+([0-9]+)/(?:TCG|KVM)$")
QEMU_SYSTEM_BINARY = "qemu-system-loongarch64"
QEMU_SYSTEM_COMM = QEMU_SYSTEM_BINARY[:15]
PLUGIN_MAGIC = b"MYGOBS1\0"
PLUGIN_HEADER = struct.Struct("<8sHHIII12QII")
PLUGIN_VERSION = 1
PLUGIN_FLAG_KERNEL = 1 << 0
PLUGIN_FLAG_REGISTERS_VALID = 1 << 1
PLUGIN_FLAG_STACK_VALID = 1 << 2
PLUGIN_FLAG_STACK_TRUNCATED = 1 << 3
PLUGIN_FLAG_COUNTER_ONLY = 1 << 4


class ProfileError(RuntimeError):
    """表示 daemon 无法保证测量语义。"""


def utc_now() -> str:
    """返回可排序的 UTC 时间。"""

    return dt.datetime.now(dt.timezone.utc).isoformat()


def sha256_file(path: Path) -> str:
    """流式计算大符号文件的身份。"""

    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


@dataclasses.dataclass(frozen=True)
class KernelMapManifest:
    """同一次 production link 发布的 kernel/map 身份。"""

    target: str
    kernel_sha256: str
    symbol_map_sha256: str


def load_kernel_map_manifest(path: Path) -> KernelMapManifest:
    """严格解析构建器发布的四字段 kernel/map manifest。"""

    values: dict[str, str] = {}
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeError) as error:
        raise ProfileError(f"cannot read kernel/map manifest {path}: {error}") from error
    for line in lines:
        if "=" not in line:
            raise ProfileError(f"malformed kernel/map manifest line: {line!r}")
        name, value = line.split("=", 1)
        if not name or not value or name in values:
            raise ProfileError(f"malformed kernel/map manifest field: {line!r}")
        values[name] = value
    required = {"schema", "target", "kernel_sha256", "symbol_map_sha256"}
    if set(values) != required or values.get("schema") != "mygo.kernel-map-manifest.v1":
        raise ProfileError("invalid kernel/map manifest schema or fields")
    if not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", values["target"]):
        raise ProfileError("invalid kernel/map manifest target")
    for name in ("kernel_sha256", "symbol_map_sha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", values[name]):
            raise ProfileError(f"invalid kernel/map manifest {name}")
    return KernelMapManifest(
        target=values["target"],
        kernel_sha256=values["kernel_sha256"],
        symbol_map_sha256=values["symbol_map_sha256"],
    )


def parse_key_values(text: str) -> dict[str, str]:
    """解析 marker 中不含空格的 key=value 字段。"""

    values: dict[str, str] = {}
    for field in text.split():
        if "=" in field:
            key, value = field.split("=", 1)
            values[key] = value
    return values


def gdb_quote(value: str) -> str:
    """为 GDB CLI 参数生成不经过 shell 的双引号字符串。"""

    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


@dataclasses.dataclass(frozen=True)
class Frame:
    """一次 GDB backtrace 中的单帧。"""

    level: int
    address: str | None
    function: str
    raw: str

    @property
    def symbolized(self) -> bool:
        return self.function not in {"", "??"}


@dataclasses.dataclass(frozen=True)
class ThreadBacktrace:
    """一个 QEMU vCPU 对应的 backtrace。"""

    gdb_thread: int
    cpu: int
    frames: tuple[Frame, ...]


@dataclasses.dataclass(frozen=True)
class Symbol:
    """符号图中的一个地址边界。"""

    address: int
    name: str


class SymbolTable:
    """同时读取 Linux System.map 与 LLD GNU map。"""

    def __init__(
        self,
        symbols: Sequence[Symbol],
        text_start: int,
        text_stop: int,
        symbol_stops: dict[int, int] | None = None,
    ) -> None:
        if not symbols or text_start >= text_stop:
            raise ProfileError("symbol map has no executable text symbols")
        by_address: dict[int, str] = {}
        for symbol in symbols:
            if text_start <= symbol.address < text_stop:
                by_address[symbol.address] = symbol.name
        if not by_address:
            raise ProfileError("symbol map text range contains no symbols")
        self.symbols = tuple(Symbol(address, by_address[address]) for address in sorted(by_address))
        self.addresses = tuple(symbol.address for symbol in self.symbols)
        self.stops = tuple(
            (
                symbol_stops.get(symbol.address, symbol.address)
                if symbol_stops is not None
                else (
                    self.addresses[index + 1]
                    if index + 1 < len(self.addresses)
                    else text_stop
                )
            )
            for index, symbol in enumerate(self.symbols)
        )
        if any(stop <= symbol.address for symbol, stop in zip(self.symbols, self.stops)):
            raise ProfileError("symbol map contains an empty executable symbol range")
        self.text_start = text_start
        self.text_stop = text_stop

    @classmethod
    def load(cls, path: Path) -> "SymbolTable":
        candidates: list[Symbol] = []
        typed_text: list[Symbol] = []
        system_map_addresses: list[int] = []
        named: dict[str, int] = {}
        with path.open(encoding="utf-8", errors="replace") as source:
            for raw_line in source:
                fields = raw_line.split()
                if len(fields) >= 3 and re.fullmatch(r"[0-9A-Fa-f]{8,16}", fields[0]):
                    address = int(fields[0], 16)
                    if len(fields[1]) == 1 and fields[1].isalpha():
                        name = " ".join(fields[2:])
                        symbol = Symbol(address, name)
                        candidates.append(symbol)
                        system_map_addresses.append(address)
                        if fields[1] in {"t", "T"}:
                            typed_text.append(symbol)
                        named.setdefault(name, address)
                        continue
                if (
                    len(fields) >= 5
                    and all(re.fullmatch(r"[0-9A-Fa-f]+", field) for field in fields[:4])
                ):
                    address = int(fields[0], 16)
                    name = " ".join(fields[4:])
                    assignment = re.fullmatch(r"(stext|etext|_stext|_etext|_text) = \.", name)
                    if assignment:
                        named.setdefault(assignment.group(1), address)
                        continue
                    if (
                        name.startswith((".", "/", "<"))
                        or ":(" in name
                        or " = " in name
                        or name == "="
                    ):
                        continue
                    candidates.append(Symbol(address, name))
                    named.setdefault(name, address)

        if typed_text:
            boundaries = sorted(set(system_map_addresses))
            stops: dict[int, int] = {}
            for symbol in typed_text:
                next_index = bisect.bisect_right(boundaries, symbol.address)
                stops[symbol.address] = (
                    boundaries[next_index] if next_index < len(boundaries) else symbol.address + 4
                )
            return cls(typed_text, min(stops), max(stops.values()), stops)

        text_start = next(
            (named[name] for name in ("stext", "_stext", "_text") if name in named),
            None,
        )
        text_stop = next(
            (named[name] for name in ("etext", "_etext") if name in named),
            None,
        )
        if text_start is None or text_stop is None:
            raise ProfileError("symbol map is missing text boundaries")
        return cls(candidates, text_start, text_stop)

    def lookup(self, address: int, return_address: bool = False) -> tuple[Symbol, int] | None:
        probe = address - 4 if return_address and address >= 4 else address
        if probe < self.text_start or probe >= self.text_stop:
            return None
        index = bisect.bisect_right(self.addresses, probe) - 1
        if index < 0:
            return None
        symbol = self.symbols[index]
        if probe >= self.stops[index]:
            return None
        return symbol, probe - symbol.address


@dataclasses.dataclass(frozen=True)
class PluginRecord:
    """TCG plugin 发出的固定头加原始 guest 栈窗口。"""

    vcpu: int
    flags: int
    sequence: int
    monotonic_ns: int
    total_insns: int
    user_insns: int
    kernel_insns: int
    dropped: int
    pc: int
    sp: int
    ra: int
    fp: int
    tp: int
    percpu: int
    stack: bytes

    @classmethod
    def parse(cls, payload: bytes) -> "PluginRecord":
        if len(payload) < PLUGIN_HEADER.size:
            raise ValueError("short plugin record")
        values = PLUGIN_HEADER.unpack_from(payload)
        magic, version, header_bytes, record_bytes, vcpu, flags, *tail = values
        if magic != PLUGIN_MAGIC or version != PLUGIN_VERSION:
            raise ValueError("unsupported plugin record")
        if header_bytes != PLUGIN_HEADER.size or record_bytes != len(payload):
            raise ValueError("plugin record size mismatch")
        (
            sequence,
            monotonic_ns,
            total_insns,
            user_insns,
            kernel_insns,
            dropped,
            pc,
            sp,
            ra,
            fp,
            tp,
            percpu,
            stack_bytes,
            reserved,
        ) = tail
        if reserved != 0 or stack_bytes != len(payload) - header_bytes or stack_bytes % 8:
            raise ValueError("malformed plugin stack window")
        if total_insns != user_insns + kernel_insns:
            raise ValueError("plugin instruction counters do not add up")
        return cls(
            vcpu=vcpu,
            flags=flags,
            sequence=sequence,
            monotonic_ns=monotonic_ns,
            total_insns=total_insns,
            user_insns=user_insns,
            kernel_insns=kernel_insns,
            dropped=dropped,
            pc=pc,
            sp=sp,
            ra=ra,
            fp=fp,
            tp=tp,
            percpu=percpu,
            stack=payload[header_bytes:],
        )


def validate_plugin_record_progress(previous: PluginRecord, current: PluginRecord) -> None:
    """拒绝同一 vCPU 上乱序或累计量回退的 plugin 记录。"""

    errors: list[str] = []
    if current.sequence <= previous.sequence:
        errors.append(f"sequence {current.sequence} <= {previous.sequence}")
    for field in (
        "monotonic_ns",
        "total_insns",
        "user_insns",
        "kernel_insns",
        "dropped",
    ):
        before = getattr(previous, field)
        after = getattr(current, field)
        if after < before:
            errors.append(f"{field} {after} < {before}")
    if errors:
        raise ValueError("plugin record regressed: " + "; ".join(errors))


@dataclasses.dataclass(frozen=True)
class PluginExitVcpu:
    """QEMU plugin atexit 为一个 vCPU 发布的最终累计量。"""

    cpu: int
    total: int
    user: int
    kernel: int
    samples: int
    dropped: int


@dataclasses.dataclass(frozen=True)
class PluginExitSummary:
    """经过严格结构校验的 plugin atexit summary。"""

    period_insns: int
    stack_bytes: int
    vcpus: tuple[PluginExitVcpu, ...]


def _nonnegative_json_int(owner: str, value: Any) -> int:
    if type(value) is not int or value < 0:
        raise ProfileError(f"{owner} must be a non-negative integer")
    return value


def load_plugin_exit_summary(
    path: Path,
    expected_period_insns: int,
    expected_stack_bytes: int,
    expected_vcpus: int,
) -> PluginExitSummary:
    """严格解析并核对 plugin 退出时写出的最终累计量。"""

    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProfileError(f"cannot read plugin exit summary {path}: {error}") from error
    if not isinstance(value, dict) or set(value) != {
        "schema",
        "period_insns",
        "stack_bytes",
        "vcpus",
    }:
        raise ProfileError("plugin exit summary has invalid top-level fields")
    if value["schema"] != "mygo.qemu-observer-plugin.v1":
        raise ProfileError("plugin exit summary has invalid schema")
    period_insns = _nonnegative_json_int("plugin exit period_insns", value["period_insns"])
    stack_bytes = _nonnegative_json_int("plugin exit stack_bytes", value["stack_bytes"])
    if period_insns != expected_period_insns or period_insns == 0:
        raise ProfileError("plugin exit period does not match daemon configuration")
    if stack_bytes != expected_stack_bytes:
        raise ProfileError("plugin exit stack size does not match daemon configuration")
    rows = value["vcpus"]
    if not isinstance(rows, list) or len(rows) != expected_vcpus:
        raise ProfileError("plugin exit summary has wrong vCPU count")
    by_cpu: dict[int, PluginExitVcpu] = {}
    fields = {"cpu", "total", "user", "kernel", "samples", "dropped"}
    for index, row in enumerate(rows):
        if not isinstance(row, dict) or set(row) != fields:
            raise ProfileError(f"plugin exit vcpus[{index}] has invalid fields")
        parsed = {name: _nonnegative_json_int(f"plugin exit vcpus[{index}].{name}", row[name]) for name in fields}
        cpu = parsed["cpu"]
        if cpu >= expected_vcpus or cpu in by_cpu:
            raise ProfileError(f"plugin exit vcpus[{index}] has invalid cpu {cpu}")
        if parsed["total"] != parsed["user"] + parsed["kernel"]:
            raise ProfileError(f"plugin exit vcpus[{index}] counters do not add up")
        by_cpu[cpu] = PluginExitVcpu(
            cpu=cpu,
            total=parsed["total"],
            user=parsed["user"],
            kernel=parsed["kernel"],
            samples=parsed["samples"],
            dropped=parsed["dropped"],
        )
    if set(by_cpu) != set(range(expected_vcpus)):
        raise ProfileError("plugin exit summary does not contain exact vCPU ids")
    return PluginExitSummary(
        period_insns=period_insns,
        stack_bytes=stack_bytes,
        vcpus=tuple(by_cpu[cpu] for cpu in range(expected_vcpus)),
    )


def reconcile_plugin_exit(
    summary: PluginExitSummary,
    latest: dict[int, PluginRecord],
) -> None:
    """证明所有成功或失败的 plugin send 都被最终累计量覆盖。"""

    dropped = sum(vcpu.dropped for vcpu in summary.vcpus)
    if dropped != 0:
        raise ProfileError(f"plugin exit summary reports {dropped} dropped datagrams")
    for vcpu in summary.vcpus:
        record = latest.get(vcpu.cpu)
        if vcpu.samples == 0:
            if record is not None:
                raise ProfileError(f"plugin vCPU {vcpu.cpu} has daemon records but zero exit samples")
            if vcpu.total >= summary.period_insns:
                raise ProfileError(f"plugin vCPU {vcpu.cpu} has no samples but reached one period")
            continue
        if record is None:
            raise ProfileError(f"plugin vCPU {vcpu.cpu} exit samples have no daemon record")
        if record.sequence != vcpu.samples:
            raise ProfileError(
                f"plugin vCPU {vcpu.cpu} sequence mismatch {record.sequence} != {vcpu.samples}"
            )
        for exit_name, record_name in (
            ("total", "total_insns"),
            ("user", "user_insns"),
            ("kernel", "kernel_insns"),
            ("dropped", "dropped"),
        ):
            if getattr(vcpu, exit_name) < getattr(record, record_name):
                raise ProfileError(f"plugin vCPU {vcpu.cpu} exit {exit_name} counter regressed")
        if vcpu.total - record.total_insns >= summary.period_insns:
            raise ProfileError(f"plugin vCPU {vcpu.cpu} final counter distance reached one period")


def plugin_frames(
    record: PluginRecord,
    symbols: SymbolTable,
    max_frames: int,
) -> list[Frame]:
    """按 Linux UNWINDER_GUESS 口径扫描返回地址，不声称精确展开。"""

    if not record.flags & PLUGIN_FLAG_REGISTERS_VALID:
        return []
    candidates: list[tuple[int, bool, str]] = [(record.pc, False, "pc")]
    if record.flags & PLUGIN_FLAG_STACK_VALID:
        for offset in range(8, len(record.stack), 8):
            address = int.from_bytes(record.stack[offset : offset + 8], "little")
            candidates.append((address, True, f"sp+0x{offset:x}"))
    frames: list[Frame] = []
    for address, is_return, source in candidates:
        resolved = symbols.lookup(address, return_address=is_return)
        if resolved is None:
            continue
        symbol, offset = resolved
        function = symbol.name
        frames.append(
            Frame(
                level=len(frames),
                address=f"0x{address:016x}",
                function=function,
                raw=f"{source} {function}+0x{offset:x}",
            )
        )
        if len(frames) >= max_frames:
            break
    return frames


def _frame_function(body: str) -> tuple[str | None, str]:
    address: str | None = None
    match = GDB_ADDRESS_RE.match(body)
    if match:
        address = match.group(1)
        body = match.group(2)
    body = body.split(" at ", 1)[0].split(" from ", 1)[0].strip()
    if " (" in body:
        body = body.split(" (", 1)[0].strip()
    elif body.endswith("()"):
        body = body[:-2].strip()
    return address, body or "??"


def parse_gdb_backtrace(text: str) -> list[ThreadBacktrace]:
    """解析 `thread apply all bt`，保留无法符号化的原始帧。"""

    traces: list[ThreadBacktrace] = []
    current_thread: int | None = None
    current_cpu: int | None = None
    frames: list[Frame] = []

    def finish() -> None:
        nonlocal frames
        if current_thread is not None and current_cpu is not None:
            traces.append(ThreadBacktrace(current_thread, current_cpu, tuple(frames)))
        frames = []

    for raw_line in text.splitlines():
        line = raw_line.strip()
        thread_match = GDB_THREAD_RE.match(line)
        if thread_match:
            finish()
            current_thread = int(thread_match.group(1))
            current_cpu = int(thread_match.group(2))
            continue
        frame_match = GDB_FRAME_RE.match(line)
        if frame_match and current_thread is not None:
            address, function = _frame_function(frame_match.group(2))
            frames.append(
                Frame(
                    level=int(frame_match.group(1)),
                    address=address,
                    function=function,
                    raw=line,
                )
            )
    finish()
    return traces


@dataclasses.dataclass(frozen=True)
class ProcStat:
    """`/proc/<pid>/stat` 中用于性能归因的稳定字段。"""

    pid: int
    state: str
    utime_ticks: int
    stime_ticks: int
    start_ticks: int
    virtual_bytes: int
    rss_pages: int


def parse_proc_stat(text: str) -> ProcStat:
    """处理 comm 中可能出现空格或右括号的 proc stat。"""

    left = text.find("(")
    right = text.rfind(")")
    if left <= 0 or right <= left:
        raise ValueError("malformed proc stat")
    pid = int(text[:left].strip())
    fields = text[right + 1 :].split()
    if len(fields) < 22:
        raise ValueError("short proc stat")
    return ProcStat(
        pid=pid,
        state=fields[0],
        utime_ticks=int(fields[11]),
        stime_ticks=int(fields[12]),
        start_ticks=int(fields[19]),
        virtual_bytes=int(fields[20]),
        rss_pages=int(fields[21]),
    )


@dataclasses.dataclass(frozen=True)
class QemuProcessIdentity:
    """用于阻止容器 init PID 或 exec 后进程冒充 QEMU 的宿主证据。"""

    method: str
    executable: str
    device: int | None
    inode: int | None
    comm: str | None
    argv0: str | None
    cmdline_sha256: str | None


def read_qemu_fallback_identity(pid: int, proc_root: Path = Path("/proc")) -> QemuProcessIdentity:
    """在 proc exe magic link 受限时，用严格的 comm/cmdline 证据确认 QEMU。"""

    process = proc_root / str(pid)
    try:
        comm = (process / "comm").read_text().rstrip("\n")
        cmdline = (process / "cmdline").read_bytes()
    except OSError as error:
        raise ProfileError(f"cannot read fallback identity for QEMU pid {pid}: {error}") from error
    arguments = cmdline.rstrip(b"\0").split(b"\0") if cmdline else []
    argv0 = arguments[0].decode("utf-8", errors="replace") if arguments else ""
    if comm != QEMU_SYSTEM_COMM or Path(argv0).name != QEMU_SYSTEM_BINARY:
        raise ProfileError(
            f"pid {pid} comm/cmdline is not {QEMU_SYSTEM_BINARY}: comm={comm!r} argv0={argv0!r}"
        )
    return QemuProcessIdentity(
        method="proc-comm-cmdline",
        executable=argv0,
        device=None,
        inode=None,
        comm=comm,
        argv0=argv0,
        cmdline_sha256=hashlib.sha256(cmdline).hexdigest(),
    )


def read_qemu_process_identity(pid: int, proc_root: Path = Path("/proc")) -> QemuProcessIdentity:
    """优先读取 exe inode；仅权限拒绝时退回可复核的 proc 文本证据。"""

    executable = proc_root / str(pid) / "exe"
    try:
        target = os.readlink(executable)
        identity = executable.stat()
    except OSError as error:
        if error.errno not in {errno.EACCES, errno.EPERM}:
            raise ProfileError(f"cannot identify QEMU pid {pid}: {error}") from error
        return read_qemu_fallback_identity(pid, proc_root)
    name = Path(target.removesuffix(" (deleted)")).name
    if name != QEMU_SYSTEM_BINARY:
        raise ProfileError(f"pid {pid} executable is not {QEMU_SYSTEM_BINARY}: {target}")
    return QemuProcessIdentity(
        method="proc-exe-dev-inode",
        executable=target,
        device=identity.st_dev,
        inode=identity.st_ino,
        comm=None,
        argv0=None,
        cmdline_sha256=None,
    )


@dataclasses.dataclass(frozen=True)
class VcpuThread:
    """一个由 QEMU 命名的宿主 vCPU 线程快照。"""

    cpu: int
    tid: int
    start_ticks: int
    state: str
    utime_ticks: int
    stime_ticks: int

    @property
    def identity(self) -> tuple[int, int]:
        return self.tid, self.start_ticks


def assess_vcpu_threads(
    threads: Sequence[VcpuThread],
    vcpu_count: int,
    expected: dict[int, tuple[int, int]] | None = None,
) -> tuple[dict[int, VcpuThread], tuple[str, ...]]:
    """校验 vCPU 编号完整、唯一，并在窗口内保持同一个宿主线程。"""

    by_cpu: dict[int, VcpuThread] = {}
    errors: list[str] = []
    for thread in threads:
        if thread.cpu < 0 or thread.cpu >= vcpu_count:
            errors.append(f"out-of-range vCPU {thread.cpu} tid={thread.tid}")
            continue
        previous = by_cpu.get(thread.cpu)
        if previous is not None:
            errors.append(
                f"duplicate vCPU {thread.cpu} tids={previous.tid},{thread.tid}"
            )
            continue
        by_cpu[thread.cpu] = thread

    missing = sorted(set(range(vcpu_count)) - by_cpu.keys())
    if missing:
        errors.append("missing vCPUs " + ",".join(str(cpu) for cpu in missing))
    if expected is not None:
        for cpu, thread in by_cpu.items():
            identity = expected.get(cpu)
            if identity is not None and thread.identity != identity:
                errors.append(
                    f"vCPU {cpu} identity changed from {identity[0]}/{identity[1]} "
                    f"to {thread.tid}/{thread.start_ticks}"
                )
    return by_cpu, tuple(errors)


@dataclasses.dataclass(frozen=True)
class StageEvent:
    """从串口观察到的一个阶段边界。"""

    monotonic_ns: int
    kind: str
    name: str
    line: str
    values: dict[str, str]


class SerialTimeline:
    """增量解析 CR/LF 串口流并去重 Cargo 进度。"""

    def __init__(self, custom_patterns: Sequence[tuple[str, re.Pattern[str]]] = ()) -> None:
        self._buffer = ""
        self._last_progress = -1
        self._patterns = tuple(custom_patterns)

    def reset(self) -> None:
        self._buffer = ""
        self._last_progress = -1

    def feed(self, data: bytes, timestamp_ns: int) -> list[StageEvent]:
        decoded = data.decode("utf-8", errors="replace")
        combined = self._buffer + decoded
        parts = re.split(r"[\r\n]", combined)
        self._buffer = parts.pop() if parts else ""
        events: list[StageEvent] = []
        for raw_line in parts:
            events.extend(self.parse_line(raw_line, timestamp_ns))
        return events

    def flush(self, timestamp_ns: int) -> list[StageEvent]:
        if not self._buffer:
            return []
        line = self._buffer
        self._buffer = ""
        return self.parse_line(line, timestamp_ns)

    def parse_line(self, raw_line: str, timestamp_ns: int) -> list[StageEvent]:
        line = ANSI_RE.sub("", raw_line).strip()
        if not line:
            return []
        events: list[StageEvent] = []
        marker = MARKER_RE.search(line)
        if marker:
            marker_name = marker.group(1)
            values = parse_key_values(marker.group(2))
            detail = values.get("name") or values.get("case") or marker_name
            events.append(
                StageEvent(timestamp_ns, "marker", f"{marker_name}:{detail}", line[:2048], values)
            )

        for progress_match in CARGO_PROGRESS_RE.finditer(line):
            progress = int(progress_match.group(1))
            if progress <= 446 and progress > self._last_progress:
                self._last_progress = progress
                events.append(
                    StageEvent(
                        timestamp_ns,
                        "cargo_progress",
                        f"cargo:{progress}",
                        line[:2048],
                        {"progress": str(progress), "total": "446"},
                    )
                )

        compile_match = CARGO_COMPILE_RE.search(line)
        if compile_match:
            crate = compile_match.group(1)
            events.append(
                StageEvent(timestamp_ns, "cargo_compile", f"compile:{crate}", line[:2048], {})
            )
        if "Finished" in line and ("dev" in line or "release" in line):
            events.append(StageEvent(timestamp_ns, "cargo_finished", "cargo:finished", line[:2048], {}))
        if "pre-build tg-xtask" in line:
            events.append(StageEvent(timestamp_ns, "buildstorm", "buildstorm:pre-build", line[:2048], {}))
        elif "build arceos-helloworld" in line:
            events.append(StageEvent(timestamp_ns, "buildstorm", "buildstorm:kernel-build", line[:2048], {}))
        elif "OS COMP TEST GROUP START buildstorm" in line:
            events.append(StageEvent(timestamp_ns, "buildstorm", "buildstorm:start", line[:2048], {}))
        elif "OS COMP TEST GROUP END buildstorm" in line:
            events.append(StageEvent(timestamp_ns, "buildstorm", "buildstorm:end", line[:2048], {}))

        for name, pattern in self._patterns:
            custom = pattern.search(line)
            if custom:
                values = {key: value for key, value in custom.groupdict().items() if value is not None}
                events.append(StageEvent(timestamp_ns, "custom", name, line[:2048], values))
        return events


class EventWriter:
    """逐行落盘，确保长任务中断后仍保留完整证据。"""

    def __init__(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self.path = path
        self._file = path.open("x", encoding="utf-8")
        self._sequence = 0

    def write(self, kind: str, timestamp_ns: int, **data: Any) -> dict[str, Any]:
        record = {
            "schema": SCHEMA,
            "seq": self._sequence,
            "kind": kind,
            "monotonic_ns": timestamp_ns,
            **data,
        }
        self._sequence += 1
        json.dump(record, self._file, ensure_ascii=True, sort_keys=True, separators=(",", ":"))
        self._file.write("\n")
        self._file.flush()
        return record

    def close(self) -> None:
        self._file.close()


class QmpClient:
    """只实现 daemon 所需的严格 QMP 请求/响应子集。"""

    def __init__(self, path: Path, timeout: float = 3.0) -> None:
        self.path = path
        self.timeout = timeout
        self._socket: socket.socket | None = None
        self._stream: Any = None
        self._buffered_events: list[dict[str, Any]] = []
        self._next_id = 1

    def connect(self) -> None:
        self.close()
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.settimeout(self.timeout)
        connection.connect(str(self.path))
        self._socket = connection
        self._stream = connection.makefile("rwb", buffering=0)
        greeting = self._read_object()
        if "QMP" not in greeting:
            self.close()
            raise ProfileError("QMP greeting is missing")
        self.execute("qmp_capabilities")

    def close(self) -> None:
        if self._stream is not None:
            self._stream.close()
        if self._socket is not None:
            self._socket.close()
        self._stream = None
        self._socket = None

    def _read_object(self) -> dict[str, Any]:
        if self._stream is None:
            raise ProfileError("QMP is disconnected")
        raw = self._stream.readline()
        if not raw:
            raise ProfileError("QMP connection closed")
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as error:
            raise ProfileError(f"malformed QMP response: {error}") from error
        if not isinstance(value, dict):
            raise ProfileError("QMP response is not an object")
        return value

    def execute(self, command: str, arguments: dict[str, Any] | None = None) -> Any:
        if self._stream is None:
            raise ProfileError("QMP is disconnected")
        request_id = self._next_id
        self._next_id += 1
        request: dict[str, Any] = {"execute": command, "id": request_id}
        if arguments:
            request["arguments"] = arguments
        payload = (json.dumps(request, separators=(",", ":")) + "\n").encode()
        self._stream.write(payload)
        while True:
            response = self._read_object()
            if "event" in response:
                self._buffered_events.append(response)
                continue
            if response.get("id") != request_id:
                raise ProfileError("QMP response id mismatch")
            if "error" in response:
                description = response["error"].get("desc", response["error"])
                raise ProfileError(f"QMP {command} failed: {description}")
            if "return" not in response:
                raise ProfileError(f"QMP {command} returned no result")
            return response["return"]

    def status(self) -> str:
        result = self.execute("query-status")
        if not isinstance(result, dict) or not isinstance(result.get("status"), str):
            raise ProfileError("QMP query-status returned malformed data")
        return result["status"]


@dataclasses.dataclass
class CaptureState:
    """单次测量窗口的宿主机证据。"""

    label: str
    start_ns: int
    start_cpu_ticks: int
    stages: list[StageEvent] = dataclasses.field(default_factory=list)
    pauses: list[tuple[int, int]] = dataclasses.field(default_factory=list)
    top_functions: Counter[str] = dataclasses.field(default_factory=Counter)
    stack_attempts: int = 0
    stack_successes: int = 0
    frames: int = 0
    symbolized_frames: int = 0
    proc_samples: int = 0
    qmp_errors: int = 0
    plugin_samples: int = 0
    plugin_records: int = 0
    plugin_invalid: int = 0
    plugin_sequence_gaps: int = 0
    plugin_top_symbolized: int = 0
    plugin_first: dict[int, PluginRecord] = dataclasses.field(default_factory=dict)
    plugin_last: dict[int, PluginRecord] = dataclasses.field(default_factory=dict)
    call_paths: Counter[str] = dataclasses.field(default_factory=Counter)
    pc_hist: Counter[int] = dataclasses.field(default_factory=Counter)
    vcpu_thread_identity: dict[int, tuple[int, int]] = dataclasses.field(default_factory=dict)
    vcpu_thread_preflight_valid: bool = False
    vcpu_thread_complete_samples: int = 0
    vcpu_thread_errors: int = 0
    vcpu_thread_failure_reasons: list[str] = dataclasses.field(default_factory=list)


def overlap_ns(start: int, stop: int, intervals: Iterable[tuple[int, int]]) -> int:
    """计算闭开区间与暂停区间的交集。"""

    return sum(max(0, min(stop, right) - max(start, left)) for left, right in intervals)


class ProfileDaemon:
    """串行化 QMP 停机与采样，避免多个调试器互相竞争。"""

    def __init__(self, args: argparse.Namespace, clock: Callable[[], int] = time.monotonic_ns) -> None:
        self.args = args
        self.clock = clock
        self.writer = EventWriter(args.output)
        self.qmp = QmpClient(args.qmp_socket, args.qmp_timeout_ms / 1000) if args.qmp_socket else None
        self.selector = selectors.DefaultSelector()
        self.server: socket.socket | None = None
        self.plugin_socket: socket.socket | None = None
        self.plugin_latest: dict[int, PluginRecord] = {}
        self.plugin_exit_reconciliation_attempted = False
        self.running = True
        self.capture: CaptureState | None = None
        self.completed = False
        self.next_proc_ns = 0
        self.next_stack_ns = 0
        self.serial_offset = 0
        self.timeline = SerialTimeline(args.stage_patterns)
        self.qemu_identity = self._read_proc_stat(args.qemu_pid)
        self.qemu_process_identity = read_qemu_process_identity(args.qemu_pid)
        self.clock_ticks = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
        self.page_size = os.sysconf("SC_PAGE_SIZE")
        self.symbol_sha256 = sha256_file(args.symbol_file) if args.symbol_file else None
        self.symbol_map_sha256 = sha256_file(args.symbol_map) if args.symbol_map else None
        self.kernel_sha256 = getattr(args, "kernel_sha256", None)
        self.symbol_manifest_sha256 = getattr(args, "symbol_manifest_sha256", None)
        self.symbol_manifest_target = getattr(args, "symbol_manifest_target", None)
        self.symbol_table = SymbolTable.load(args.symbol_map) if args.symbol_map else None

    def _read_proc_stat(self, pid: int) -> ProcStat:
        try:
            return parse_proc_stat(Path(f"/proc/{pid}/stat").read_text())
        except (OSError, ValueError) as error:
            raise ProfileError(f"cannot read QEMU pid {pid}: {error}") from error

    def _verify_qemu_identity(self) -> ProcStat:
        current = self._read_proc_stat(self.args.qemu_pid)
        if current.start_ticks != self.qemu_identity.start_ticks:
            raise ProfileError("QEMU pid was reused")
        initial = self.qemu_process_identity
        if initial.method == "proc-comm-cmdline":
            identity = read_qemu_fallback_identity(self.args.qemu_pid)
            if (
                identity.comm,
                identity.argv0,
                identity.cmdline_sha256,
            ) != (initial.comm, initial.argv0, initial.cmdline_sha256):
                raise ProfileError("QEMU fallback process identity changed")
        else:
            identity = read_qemu_process_identity(self.args.qemu_pid)
            if identity.method != initial.method or (identity.device, identity.inode) != (
                initial.device,
                initial.inode,
            ):
                raise ProfileError("QEMU process changed executable")
        return current

    def _read_vcpu_threads(self) -> tuple[list[VcpuThread], list[str]]:
        threads: list[VcpuThread] = []
        errors: list[str] = []
        task_root = Path(f"/proc/{self.args.qemu_pid}/task")
        try:
            tasks = sorted(
                (path for path in task_root.iterdir() if path.name.isdigit()),
                key=lambda path: int(path.name),
            )
        except OSError as error:
            raise ProfileError(f"cannot enumerate QEMU threads: {error}") from error
        for task in tasks:
            try:
                comm = (task / "comm").read_text().rstrip("\n")
            except OSError:
                continue
            match = VCPU_COMM_RE.fullmatch(comm)
            if match is None:
                continue
            try:
                value = parse_proc_stat((task / "stat").read_text())
            except (OSError, ValueError) as error:
                errors.append(f"cannot read vCPU thread {task.name}: {error}")
                continue
            expected_tid = int(task.name)
            if value.pid != expected_tid:
                errors.append(f"vCPU thread path/stat mismatch {expected_tid}/{value.pid}")
                continue
            threads.append(
                VcpuThread(
                    cpu=int(match.group(1)),
                    tid=value.pid,
                    start_ticks=value.start_ticks,
                    state=value.state,
                    utime_ticks=value.utime_ticks,
                    stime_ticks=value.stime_ticks,
                )
            )
        return threads, errors

    @staticmethod
    def _record_vcpu_errors(capture: CaptureState, errors: Sequence[str]) -> None:
        capture.vcpu_thread_errors += len(errors)
        remaining = max(0, 16 - len(capture.vcpu_thread_failure_reasons))
        capture.vcpu_thread_failure_reasons.extend(errors[:remaining])

    def setup(self) -> None:
        control = self.args.control_socket
        control.parent.mkdir(parents=True, exist_ok=True)
        if control.exists() or control.is_symlink():
            mode = control.lstat().st_mode
            if not stat.S_ISSOCK(mode):
                raise ProfileError(f"refusing to replace non-socket control path: {control}")
            control.unlink()
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(control))
        os.chmod(control, 0o600)
        server.listen(8)
        server.setblocking(False)
        self.server = server
        self.selector.register(server, selectors.EVENT_READ)
        if self.args.plugin_socket:
            plugin_path = self.args.plugin_socket
            plugin_path.parent.mkdir(parents=True, exist_ok=True)
            if plugin_path.exists() or plugin_path.is_symlink():
                mode = plugin_path.lstat().st_mode
                if not stat.S_ISSOCK(mode):
                    raise ProfileError(f"refusing to replace non-socket plugin path: {plugin_path}")
                plugin_path.unlink()
            plugin_socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            plugin_socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
            plugin_socket.bind(str(plugin_path))
            os.chmod(plugin_path, 0o600)
            plugin_socket.setblocking(False)
            self.plugin_socket = plugin_socket
            self.selector.register(plugin_socket, selectors.EVENT_READ)
        if self.qmp is not None:
            self.qmp.connect()
        now = self.clock()
        self.writer.write(
            "session_start",
            now,
            wall_time=utc_now(),
            metadata=self._metadata(),
        )
        if self.args.ready_file:
            self.args.ready_file.write_text(f"{os.getpid()}\n", encoding="ascii")

    def _metadata(self) -> dict[str, Any]:
        return {
            "system": self.args.system,
            "workload": self.args.workload,
            "qemu_pid": self.args.qemu_pid,
            "qemu_start_ticks": self.qemu_identity.start_ticks,
            "qemu_identity_method": self.qemu_process_identity.method,
            "qemu_identity_evidence": {
                "executable": self.qemu_process_identity.executable,
                "device": self.qemu_process_identity.device,
                "inode": self.qemu_process_identity.inode,
                "comm": self.qemu_process_identity.comm,
                "argv0": self.qemu_process_identity.argv0,
                "cmdline_sha256": self.qemu_process_identity.cmdline_sha256,
            },
            "qemu_executable": self.qemu_process_identity.executable,
            "qemu_executable_device": self.qemu_process_identity.device,
            "qemu_executable_inode": self.qemu_process_identity.inode,
            "vcpu_count": self.args.vcpu_count,
            "clock_ticks_per_second": self.clock_ticks,
            "proc_interval_ms": self.args.proc_interval_ms,
            "stack_interval_ms": self.args.stack_interval_ms,
            "stack_timeout_ms": self.args.stack_timeout_ms,
            "max_frames": self.args.max_frames,
            "max_pause_ratio": self.args.max_pause_ratio,
            "symbol_file": str(self.args.symbol_file) if self.args.symbol_file else None,
            "symbol_sha256": self.symbol_sha256,
            "symbol_map": str(self.args.symbol_map) if self.args.symbol_map else None,
            "symbol_map_sha256": self.symbol_map_sha256,
            "kernel_image": str(self.args.kernel_image) if self.args.kernel_image else None,
            "kernel_sha256": self.kernel_sha256,
            "symbol_manifest": (
                str(self.args.symbol_manifest) if self.args.symbol_manifest else None
            ),
            "symbol_manifest_sha256": self.symbol_manifest_sha256,
            "symbol_manifest_target": self.symbol_manifest_target,
            "plugin_period_insns": self.args.plugin_period_insns
            if self.args.plugin_socket
            else None,
            "plugin_stack_bytes": self.args.plugin_stack_bytes
            if self.args.plugin_socket
            else None,
            "plugin_summary": str(self.args.plugin_summary) if self.args.plugin_summary else None,
            "unwind": "stack-scan-guess-v1" if self.args.plugin_socket else "gdb",
            "environment": self.args.environment,
            "gdb_architecture": self.args.gdb_architecture if self.args.stack_interval_ms else None,
        }

    def serve(self) -> int:
        self.setup()
        try:
            while self.running:
                now = self.clock()
                self._poll_serial(now)
                if self.capture is not None:
                    if now >= self.next_proc_ns:
                        self._sample_proc(now)
                        self.next_proc_ns = now + self.args.proc_interval_ms * 1_000_000
                    if self.args.stack_interval_ms and now >= self.next_stack_ns:
                        self._sample_stack()
                        self.next_stack_ns = self.clock() + self.args.stack_interval_ms * 1_000_000
                events = self.selector.select(0.05)
                for key, _mask in events:
                    if key.fileobj is self.server:
                        self._accept_control()
                    elif key.fileobj is self.plugin_socket:
                        self._drain_plugin()
        finally:
            try:
                if self.capture is not None:
                    self.stop_capture("daemon-exit")
            finally:
                self._shutdown()
        return 0

    def _accept_control(self) -> None:
        assert self.server is not None
        connection, _address = self.server.accept()
        connection.settimeout(2)
        with connection:
            try:
                raw = b""
                while b"\n" not in raw and len(raw) <= 65536:
                    chunk = connection.recv(4096)
                    if not chunk:
                        break
                    raw += chunk
                if len(raw) > 65536:
                    raise ProfileError("control request is too large")
                request = json.loads(raw.split(b"\n", 1)[0])
                if not isinstance(request, dict):
                    raise ProfileError("control request is not an object")
                response = self._control(request)
            except (ProfileError, ValueError, json.JSONDecodeError) as error:
                response = {"ok": False, "error": str(error)}
            connection.sendall((json.dumps(response, separators=(",", ":")) + "\n").encode())

    def _control(self, request: dict[str, Any]) -> dict[str, Any]:
        command = request.get("command")
        if command == "start":
            label = request.get("label", "default")
            if not isinstance(label, str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,64}", label):
                raise ProfileError("invalid capture label")
            self.start_capture(label)
        elif command == "stop":
            self.stop_capture("control")
        elif command == "shutdown":
            if self.capture is not None:
                self.stop_capture("shutdown")
            self.running = False
            if self.args.plugin_socket is not None and self.completed:
                self._reconcile_plugin_exit_summary()
        elif command == "status":
            pass
        else:
            raise ProfileError("unknown control command")
        return {
            "ok": True,
            "active": self.capture is not None,
            "completed": self.completed,
            "pid": os.getpid(),
        }

    def start_capture(self, label: str) -> None:
        if self.capture is not None:
            raise ProfileError("capture is already active")
        if self.completed:
            raise ProfileError("this daemon already completed a capture")
        current = self._verify_qemu_identity()
        now = self.clock()
        capture = CaptureState(label, now, current.utime_ticks + current.stime_ticks)
        thread_values, read_errors = self._read_vcpu_threads()
        threads, validation_errors = assess_vcpu_threads(thread_values, self.args.vcpu_count)
        preflight_errors = [*read_errors, *validation_errors]
        capture.vcpu_thread_identity = {
            cpu: thread.identity for cpu, thread in threads.items()
        }
        capture.vcpu_thread_preflight_valid = not preflight_errors
        self._record_vcpu_errors(capture, preflight_errors)
        self.capture = capture
        capture.plugin_first.update(self.plugin_latest)
        capture.plugin_last.update(self.plugin_latest)
        self.timeline.reset()
        try:
            self.serial_offset = self.args.serial_log.stat().st_size
        except FileNotFoundError:
            self.serial_offset = 0
        self.next_proc_ns = now
        self.next_stack_ns = now + self.args.stack_interval_ms * 1_000_000
        self.writer.write("capture_start", now, label=label, serial_offset=self.serial_offset)

    def stop_capture(self, reason: str) -> None:
        capture = self.capture
        if capture is None:
            raise ProfileError("capture is not active")
        if self.plugin_socket is not None:
            self._drain_plugin()
        stop_ns = self.clock()
        self._poll_serial(stop_ns, flush=True)
        self._sample_proc(stop_ns)
        current = self._verify_qemu_identity()
        stop_cpu_ticks = current.utime_ticks + current.stime_ticks
        summary = self._build_summary(capture, stop_ns, stop_cpu_ticks, reason)
        self.writer.write("capture_stop", stop_ns, reason=reason, summary=summary)
        self._write_summary_atomic(summary)
        self.capture = None
        self.completed = True

    def _write_summary_atomic(self, summary: dict[str, Any]) -> None:
        temporary = self.args.summary.with_suffix(self.args.summary.suffix + ".tmp")
        temporary.write_text(json.dumps(summary, ensure_ascii=True, indent=2, sort_keys=True) + "\n")
        os.replace(temporary, self.args.summary)

    def _reconcile_plugin_exit_summary(self) -> None:
        if self.plugin_exit_reconciliation_attempted:
            return
        self.plugin_exit_reconciliation_attempted = True
        exit_summary: PluginExitSummary | None = None
        reconciliation_error: str | None = None
        try:
            self._drain_plugin()
            exit_summary = load_plugin_exit_summary(
                self.args.plugin_summary,
                self.args.plugin_period_insns,
                self.args.plugin_stack_bytes,
                self.args.vcpu_count,
            )
            reconcile_plugin_exit(exit_summary, self.plugin_latest)
        except (OSError, ProfileError, ValueError) as error:
            reconciliation_error = str(error)

        try:
            document = json.loads(self.args.summary.read_text(encoding="utf-8"))
            if not isinstance(document, dict) or document.get("schema") != SCHEMA:
                raise ProfileError("preliminary daemon summary has invalid schema")
            quality = document.get("quality")
            if not isinstance(quality, dict):
                raise ProfileError("preliminary daemon summary has invalid quality")
        except (OSError, UnicodeError, json.JSONDecodeError, ProfileError) as error:
            document = {"schema": SCHEMA, "quality": {}}
            quality = document["quality"]
            message = f"cannot load preliminary daemon summary: {error}"
            reconciliation_error = (
                message if reconciliation_error is None else f"{reconciliation_error}; {message}"
            )

        preliminary_valid = quality.get("plugin_preliminary_valid")
        if type(preliminary_valid) is not bool:
            message = "preliminary daemon summary is missing plugin_preliminary_valid"
            reconciliation_error = (
                message if reconciliation_error is None else f"{reconciliation_error}; {message}"
            )
            preliminary_valid = False
        reconciled = reconciliation_error is None
        quality["valid"] = bool(preliminary_valid and reconciled)
        quality["plugin_exit_reconciled"] = reconciled
        quality["plugin_exit_reconciliation_error"] = reconciliation_error
        quality["plugin_exit_counts"] = (
            {
                str(vcpu.cpu): {
                    "total": vcpu.total,
                    "user": vcpu.user,
                    "kernel": vcpu.kernel,
                    "samples": vcpu.samples,
                    "dropped": vcpu.dropped,
                }
                for vcpu in exit_summary.vcpus
            }
            if exit_summary is not None
            else None
        )
        self._write_summary_atomic(document)
        self.writer.write(
            "plugin_exit_reconciliation",
            self.clock(),
            reconciled=reconciled,
            error=reconciliation_error,
            exit_counts=quality["plugin_exit_counts"],
        )

    def _poll_serial(self, timestamp_ns: int, flush: bool = False) -> None:
        if self.capture is None:
            return
        try:
            size = self.args.serial_log.stat().st_size
        except FileNotFoundError:
            return
        if size < self.serial_offset:
            self.writer.write("warning", timestamp_ns, message="serial log was truncated")
            self.serial_offset = 0
            self.timeline.reset()
        if size > self.serial_offset:
            with self.args.serial_log.open("rb") as serial:
                serial.seek(self.serial_offset)
                data = serial.read(size - self.serial_offset)
            self.serial_offset = size
            for event in self.timeline.feed(data, timestamp_ns):
                self._record_stage(event)
        if flush:
            for event in self.timeline.flush(timestamp_ns):
                self._record_stage(event)

    def _record_stage(self, event: StageEvent) -> None:
        assert self.capture is not None
        self.capture.stages.append(event)
        self.writer.write(
            "stage",
            event.monotonic_ns,
            stage_kind=event.kind,
            name=event.name,
            line=event.line,
            values=event.values,
        )

    def _sample_proc(self, timestamp_ns: int) -> None:
        capture = self.capture
        if capture is None:
            return
        aggregate = self._verify_qemu_identity()
        thread_values, read_errors = self._read_vcpu_threads()
        by_cpu, validation_errors = assess_vcpu_threads(
            thread_values,
            self.args.vcpu_count,
            capture.vcpu_thread_identity,
        )
        thread_errors = [*read_errors, *validation_errors]
        self._record_vcpu_errors(capture, thread_errors)
        if not thread_errors:
            capture.vcpu_thread_complete_samples += 1
        capture.proc_samples += 1
        self.writer.write(
            "proc_sample",
            timestamp_ns,
            qemu={
                "state": aggregate.state,
                "utime_ticks": aggregate.utime_ticks,
                "stime_ticks": aggregate.stime_ticks,
                "rss_bytes": aggregate.rss_pages * self.page_size,
                "virtual_bytes": aggregate.virtual_bytes,
            },
            vcpus=[
                {
                    "cpu": thread.cpu,
                    "tid": thread.tid,
                    "start_ticks": thread.start_ticks,
                    "state": thread.state,
                    "utime_ticks": thread.utime_ticks,
                    "stime_ticks": thread.stime_ticks,
                }
                for thread in by_cpu.values()
            ],
            vcpu_thread_errors=thread_errors,
        )

    def _drain_plugin(self) -> None:
        assert self.plugin_socket is not None
        while True:
            try:
                payload = self.plugin_socket.recv(PLUGIN_HEADER.size + self.args.plugin_stack_bytes)
            except BlockingIOError:
                return
            received_ns = self.clock()
            try:
                record = PluginRecord.parse(payload)
            except ValueError as error:
                if self.capture is not None:
                    self.capture.plugin_invalid += 1
                self.writer.write("warning", received_ns, message=f"invalid plugin record: {error}")
                continue
            if record.vcpu >= self.args.vcpu_count:
                if self.capture is not None:
                    self.capture.plugin_invalid += 1
                self.writer.write(
                    "warning",
                    received_ns,
                    message=f"plugin record has invalid vcpu {record.vcpu}",
                )
                continue
            previous = self.plugin_latest.get(record.vcpu)
            if previous is not None:
                try:
                    validate_plugin_record_progress(previous, record)
                except ValueError as error:
                    if self.capture is not None:
                        self.capture.plugin_invalid += 1
                    self.writer.write(
                        "warning",
                        received_ns,
                        message=f"invalid plugin record: {error}",
                    )
                    continue
            self.plugin_latest[record.vcpu] = record
            capture = self.capture
            if capture is None:
                continue
            if previous is not None and record.sequence > previous.sequence + 1:
                capture.plugin_sequence_gaps += record.sequence - previous.sequence - 1
            capture.plugin_first.setdefault(record.vcpu, record)
            capture.plugin_last[record.vcpu] = record
            capture.plugin_records += 1
            if record.flags & PLUGIN_FLAG_COUNTER_ONLY:
                self.writer.write(
                    "plugin_counter_sample",
                    received_ns,
                    plugin_monotonic_ns=record.monotonic_ns,
                    receive_latency_ns=received_ns - record.monotonic_ns,
                    vcpu=record.vcpu,
                    sequence=record.sequence,
                    counters={
                        "total_insns": record.total_insns,
                        "user_insns": record.user_insns,
                        "kernel_insns": record.kernel_insns,
                        "dropped": record.dropped,
                    },
                )
                continue
            capture.plugin_samples += 1
            if record.flags & PLUGIN_FLAG_REGISTERS_VALID:
                capture.pc_hist[record.pc] += 1
            frames = (
                plugin_frames(record, self.symbol_table, self.args.max_frames)
                if self.symbol_table is not None
                else []
            )
            if frames:
                capture.plugin_top_symbolized += 1
                capture.top_functions[frames[0].function] += 1
                capture.call_paths[";".join(frame.function for frame in reversed(frames))] += 1
                capture.frames += len(frames)
                capture.symbolized_frames += len(frames)
            else:
                capture.plugin_invalid += 1
            self.writer.write(
                "plugin_stack_sample",
                received_ns,
                plugin_monotonic_ns=record.monotonic_ns,
                receive_latency_ns=received_ns - record.monotonic_ns,
                vcpu=record.vcpu,
                sequence=record.sequence,
                flags=record.flags,
                truncated=bool(record.flags & PLUGIN_FLAG_STACK_TRUNCATED),
                counters={
                    "total_insns": record.total_insns,
                    "user_insns": record.user_insns,
                    "kernel_insns": record.kernel_insns,
                    "dropped": record.dropped,
                },
                registers={
                    "pc": record.pc,
                    "sp": record.sp,
                    "ra": record.ra,
                    "fp": record.fp,
                    "tp": record.tp,
                    "percpu": record.percpu,
                },
                frames=[dataclasses.asdict(frame) for frame in frames],
            )

    def _ensure_qmp(self) -> QmpClient:
        if self.qmp is None:
            raise ProfileError("stack sampling requires QMP")
        try:
            self.qmp.status()
        except (OSError, ProfileError):
            self.qmp.connect()
        return self.qmp

    def _wait_qmp_status(self, qmp: QmpClient, expected: set[str], deadline_ns: int) -> str:
        while True:
            status_value = qmp.status()
            if status_value in expected:
                return status_value
            if self.clock() >= deadline_ns:
                raise ProfileError(f"QMP did not reach {sorted(expected)} (status={status_value})")
            time.sleep(0.005)

    def _resume_qemu(self, qmp: QmpClient) -> None:
        try:
            status_value = qmp.status()
            if status_value != "running":
                qmp.execute("cont")
                self._wait_qmp_status(qmp, {"running"}, self.clock() + 3_000_000_000)
        except (OSError, ProfileError):
            qmp.connect()
            if qmp.status() != "running":
                qmp.execute("cont")
                self._wait_qmp_status(qmp, {"running"}, self.clock() + 3_000_000_000)

    def _debugger_command(self) -> list[str]:
        assert self.args.debugger_symbol_file is not None
        assert self.args.debugger_gdb_socket is not None
        return [
            *self.args.debugger_command,
            "-q",
            "-nx",
            "-batch",
            "-ex",
            "set pagination off",
            "-ex",
            "set confirm off",
            "-ex",
            f"set architecture {self.args.gdb_architecture}",
            "-ex",
            f"file {gdb_quote(self.args.debugger_symbol_file)}",
            "-ex",
            f"target remote {gdb_quote(self.args.debugger_gdb_socket)}",
            "-ex",
            "info threads",
            "-ex",
            f"thread apply all bt {self.args.max_frames}",
            "-ex",
            "disconnect",
        ]

    def _sample_stack(self) -> None:
        capture = self.capture
        if capture is None:
            return
        capture.stack_attempts += 1
        sample_id = capture.stack_attempts
        qmp = self._ensure_qmp()
        pause_start = self.clock()
        debugger_start = pause_start
        result: subprocess.CompletedProcess[str] | None = None
        error: str | None = None
        traces: list[ThreadBacktrace] = []
        should_resume = False
        try:
            initial = qmp.status()
            if initial != "running":
                raise ProfileError(f"refusing stack sample while QEMU status is {initial}")
            qmp.execute("stop")
            should_resume = True
            self._wait_qmp_status(qmp, {"paused"}, self.clock() + 3_000_000_000)
            debugger_start = self.clock()
            result = subprocess.run(
                self._debugger_command(),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=self.args.stack_timeout_ms / 1000,
                check=False,
            )
            traces = parse_gdb_backtrace(result.stdout)
            if result.returncode != 0:
                raise ProfileError(f"debugger exited with status {result.returncode}")
            if not traces or not any(trace.frames for trace in traces):
                raise ProfileError("debugger returned no stack frames")
            capture.stack_successes += 1
        except (OSError, ProfileError, subprocess.TimeoutExpired) as caught:
            error = str(caught)
        finally:
            try:
                if should_resume:
                    self._resume_qemu(qmp)
            except (OSError, ProfileError) as resume_error:
                capture.qmp_errors += 1
                raise ProfileError(f"failed to resume QEMU after stack sample: {resume_error}") from resume_error
        pause_stop = self.clock()
        capture.pauses.append((pause_start, pause_stop))
        frames = [frame for trace in traces for frame in trace.frames]
        capture.frames += len(frames)
        capture.symbolized_frames += sum(frame.symbolized for frame in frames)
        for trace in traces:
            if trace.frames:
                capture.top_functions[trace.frames[0].function] += 1
        raw_path = self.args.output.parent / f"{self.args.output.stem}.stack-{sample_id:06d}.txt"
        raw_path.write_text(result.stdout if result is not None else error or "", encoding="utf-8")
        self.writer.write(
            "stack_sample",
            pause_stop,
            sample_id=sample_id,
            ok=error is None,
            error=error,
            pause_start_ns=pause_start,
            pause_duration_ns=pause_stop - pause_start,
            debugger_duration_ns=pause_stop - debugger_start,
            raw_path=str(raw_path),
            threads=[
                {
                    "gdb_thread": trace.gdb_thread,
                    "cpu": trace.cpu,
                    "frames": [dataclasses.asdict(frame) for frame in trace.frames],
                }
                for trace in traces
            ],
        )

    def _active_elapsed(self, capture: CaptureState, timestamp_ns: int) -> int:
        return timestamp_ns - capture.start_ns - overlap_ns(capture.start_ns, timestamp_ns, capture.pauses)

    def _build_summary(
        self,
        capture: CaptureState,
        stop_ns: int,
        stop_cpu_ticks: int,
        reason: str,
    ) -> dict[str, Any]:
        wall_duration = stop_ns - capture.start_ns
        paused = overlap_ns(capture.start_ns, stop_ns, capture.pauses)
        active = wall_duration - paused
        symbol_ratio = capture.symbolized_frames / capture.frames if capture.frames else 0.0
        pause_ratio = paused / wall_duration if wall_duration else 1.0
        stack_required = self.args.stack_interval_ms > 0
        enough_gdb_stacks = not stack_required or (
            capture.stack_attempts > 0
            and capture.stack_successes / capture.stack_attempts >= 0.8
            and symbol_ratio >= 0.5
        )
        plugin_required = self.args.plugin_socket is not None
        plugin_cpus: dict[str, dict[str, int]] = {}
        observed_plugin_cpus: list[int] = []
        plugin_drop_delta = 0
        guest_total = 0
        guest_user = 0
        guest_kernel = 0
        for cpu in range(self.args.vcpu_count):
            first = capture.plugin_first.get(cpu)
            last = capture.plugin_last.get(cpu)
            if first is None or last is None:
                continue
            total_delta = last.total_insns - first.total_insns
            user_delta = last.user_insns - first.user_insns
            kernel_delta = last.kernel_insns - first.kernel_insns
            dropped_delta = last.dropped - first.dropped
            if last.sequence > first.sequence:
                observed_plugin_cpus.append(cpu)
            guest_total += total_delta
            guest_user += user_delta
            guest_kernel += kernel_delta
            plugin_drop_delta += dropped_delta
            plugin_cpus[str(cpu)] = {
                "samples": last.sequence - first.sequence,
                "total": total_delta,
                "user": user_delta,
                "kernel": kernel_delta,
                "dropped": dropped_delta,
            }
        unobserved_plugin_cpus = sorted(
            set(range(self.args.vcpu_count)) - set(observed_plugin_cpus)
        )
        active_plugin_cpus = len(observed_plugin_cpus)
        plugin_symbol_ratio = (
            capture.plugin_top_symbolized / capture.plugin_samples
            if capture.plugin_samples
            else 0.0
        )
        enough_plugin_stacks = not plugin_required or (
            active_plugin_cpus > 0
            and capture.plugin_samples > 0
            and capture.plugin_invalid == 0
            and capture.plugin_sequence_gaps == 0
            and plugin_drop_delta == 0
            and plugin_symbol_ratio >= 0.8
        )
        vcpu_threads_valid = (
            capture.vcpu_thread_preflight_valid
            and capture.vcpu_thread_complete_samples == capture.proc_samples
            and capture.vcpu_thread_errors == 0
        )
        quality_valid = (
            active > 0
            and capture.proc_samples >= 2
            and vcpu_threads_valid
            and capture.qmp_errors == 0
            and pause_ratio <= self.args.max_pause_ratio
            and enough_gdb_stacks
            and enough_plugin_stacks
        )

        cargo_milestones: dict[str, int] = {}
        for stage in capture.stages:
            if stage.kind == "cargo_progress":
                progress = stage.values["progress"]
                cargo_milestones.setdefault(progress, self._active_elapsed(capture, stage.monotonic_ns))

        stage_spans: list[dict[str, Any]] = []
        previous_ns = capture.start_ns
        previous_name = "capture:start"
        for stage in capture.stages:
            if stage.monotonic_ns < previous_ns:
                continue
            stage_spans.append(
                {
                    "name": f"{previous_name}->{stage.name}",
                    "start_active_ns": self._active_elapsed(capture, previous_ns),
                    "stop_active_ns": self._active_elapsed(capture, stage.monotonic_ns),
                    "active_duration_ns": stage.monotonic_ns
                    - previous_ns
                    - overlap_ns(previous_ns, stage.monotonic_ns, capture.pauses),
                }
            )
            previous_ns = stage.monotonic_ns
            previous_name = stage.name
        stage_spans.append(
            {
                "name": f"{previous_name}->capture:stop",
                "start_active_ns": self._active_elapsed(capture, previous_ns),
                "stop_active_ns": active,
                "active_duration_ns": stop_ns
                - previous_ns
                - overlap_ns(previous_ns, stop_ns, capture.pauses),
            }
        )

        top_total = sum(capture.top_functions.values())
        hotspots = [
            {
                "function": function,
                "samples": samples,
                "percent": samples * 100 / top_total if top_total else 0.0,
                "sample_kind": "plugin-leaf" if plugin_required else "gdb-leaf",
            }
            for function, samples in capture.top_functions.most_common(50)
        ]
        call_path_total = sum(capture.call_paths.values())
        call_paths = [
            {
                "path": path,
                "samples": samples,
                "percent": samples * 100 / call_path_total if call_path_total else 0.0,
                "unwind": "stack-scan-guess-v1",
            }
            for path, samples in capture.call_paths.most_common(50)
        ]
        # Sub-function PC offset histogram
        hotspot_offsets: list[dict] = []
        if capture.pc_hist and self.symbol_table is not None:
            pc_total = sum(capture.pc_hist.values())
            offset_counts: Counter[tuple[str, int]] = Counter()
            for pc, count in capture.pc_hist.items():
                result = self.symbol_table.lookup(pc, return_address=False)
                if result is not None:
                    symbol, offset = result
                    offset_counts[(symbol.name, offset)] += count
            for (fn, offset), samples in offset_counts.most_common(200):
                hotspot_offsets.append({
                    "function": fn,
                    "offset": offset,
                    "samples": samples,
                    "percent": samples * 100 / pc_total if pc_total else 0.0,
                })
        stack_samples = capture.stack_attempts + capture.plugin_samples
        stack_successes = (
            capture.stack_successes + capture.plugin_samples - capture.plugin_invalid
        )
        return {
            "schema": SCHEMA,
            "metadata": self._metadata(),
            "quality": {
                "valid": quality_valid and not plugin_required,
                "plugin_preliminary_valid": quality_valid,
                "plugin_exit_reconciled": not plugin_required,
                "plugin_exit_reconciliation_error": (
                    "pending plugin exit reconciliation" if plugin_required else None
                ),
                "plugin_exit_counts": None,
                "qemu_process_identity_valid": True,
                "pause_ratio": pause_ratio,
                "stack_samples": stack_samples,
                "stack_successes": max(0, stack_successes),
                "symbolized_frame_ratio": symbol_ratio,
                "proc_samples": capture.proc_samples,
                "vcpu_thread_preflight_valid": capture.vcpu_thread_preflight_valid,
                "vcpu_thread_complete_samples": capture.vcpu_thread_complete_samples,
                "vcpu_thread_errors": capture.vcpu_thread_errors,
                "vcpu_thread_failure_reasons": capture.vcpu_thread_failure_reasons,
                "qmp_errors": capture.qmp_errors,
                "plugin_samples": capture.plugin_samples,
                "plugin_records": capture.plugin_records,
                "plugin_invalid": capture.plugin_invalid,
                "plugin_active_vcpus": active_plugin_cpus,
                "plugin_observed_vcpus": observed_plugin_cpus,
                "plugin_unobserved_vcpus": unobserved_plugin_cpus,
                "plugin_sequence_gaps": capture.plugin_sequence_gaps,
                "plugin_dropped": plugin_drop_delta,
                "plugin_symbolized_top_ratio": plugin_symbol_ratio,
            },
            "capture": {
                "label": capture.label,
                "reason": reason,
                "start_monotonic_ns": capture.start_ns,
                "stop_monotonic_ns": stop_ns,
                "wall_duration_ns": wall_duration,
                "paused_ns": paused,
                "active_duration_ns": active,
                "qemu_cpu_ticks": stop_cpu_ticks - capture.start_cpu_ticks,
                "clock_ticks_per_second": self.clock_ticks,
                "vcpu_threads": {
                    str(cpu): {"tid": identity[0], "start_ticks": identity[1]}
                    for cpu, identity in sorted(capture.vcpu_thread_identity.items())
                },
            },
            "cargo_milestones": cargo_milestones,
            "stage_spans": stage_spans,
            "hotspots": hotspots,
            "call_paths": call_paths,
            "hotspot_offsets": hotspot_offsets,
            "guest_instructions": {
                "total": guest_total,
                "user": guest_user,
                "kernel": guest_kernel,
                "counter_error_bound_insns": (
                    2
                    * self.args.plugin_period_insns
                    * self.args.vcpu_count
                    if plugin_required
                    else 0
                ),
                "vcpus": plugin_cpus,
            },
        }

    def _shutdown(self) -> None:
        try:
            if (
                self.args.plugin_socket is not None
                and self.completed
                and not self.plugin_exit_reconciliation_attempted
            ):
                self._reconcile_plugin_exit_summary()
            now = self.clock()
            self.writer.write("session_stop", now, wall_time=utc_now(), completed=self.completed)
        finally:
            self.writer.close()
            if self.qmp is not None:
                self.qmp.close()
            if self.server is not None:
                self.selector.unregister(self.server)
                self.server.close()
            if self.plugin_socket is not None:
                self.selector.unregister(self.plugin_socket)
                self.plugin_socket.close()
            self.selector.close()
            try:
                self.args.control_socket.unlink()
            except FileNotFoundError:
                pass
            if self.args.plugin_socket:
                try:
                    self.args.plugin_socket.unlink()
                except FileNotFoundError:
                    pass
            if self.args.ready_file:
                try:
                    self.args.ready_file.unlink()
                except FileNotFoundError:
                    pass


def parse_stage_patterns(values: Sequence[str]) -> tuple[tuple[str, re.Pattern[str]], ...]:
    """解析 `NAME=REGEX`，名称保持稳定以便跨内核对比。"""

    patterns: list[tuple[str, re.Pattern[str]]] = []
    for value in values:
        if "=" not in value:
            raise argparse.ArgumentTypeError("stage regex must be NAME=REGEX")
        name, expression = value.split("=", 1)
        if not re.fullmatch(r"[A-Za-z0-9_.:-]{1,64}", name):
            raise argparse.ArgumentTypeError(f"invalid stage name: {name}")
        try:
            patterns.append((name, re.compile(expression)))
        except re.error as error:
            raise argparse.ArgumentTypeError(f"invalid stage regex {name}: {error}") from error
    return tuple(patterns)


def parse_environment(values: Sequence[str]) -> dict[str, str]:
    """记录必须跨 MyGO/Linux 保持一致的外部测量条件。"""

    environment: dict[str, str] = {}
    for value in values:
        if "=" not in value:
            raise argparse.ArgumentTypeError("environment must be NAME=VALUE")
        name, contents = value.split("=", 1)
        if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_.-]{0,63}", name) or not contents:
            raise argparse.ArgumentTypeError(f"invalid environment field: {value}")
        if name in environment:
            raise argparse.ArgumentTypeError(f"duplicate environment field: {name}")
        environment[name] = contents
    return environment


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def nonnegative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be non-negative")
    return parsed


def ratio(value: str) -> float:
    parsed = float(value)
    if not 0 <= parsed < 1:
        raise argparse.ArgumentTypeError("ratio must be in [0, 1)")
    return parsed


def validate_capture_args(args: argparse.Namespace) -> None:
    args.debugger_command = shlex.split(args.debugger_command)
    if not args.debugger_command and args.stack_interval_ms:
        raise ProfileError("debugger command is empty")
    args.stage_patterns = parse_stage_patterns(args.stage_regex)
    args.environment = parse_environment(args.environment)
    if args.stack_interval_ms:
        missing = [
            name
            for name in ("qmp_socket", "gdb_socket", "symbol_file", "debugger_symbol_file")
            if getattr(args, name) is None
        ]
        if missing:
            raise ProfileError("stack sampling requires: " + ", ".join(missing))
        args.debugger_gdb_socket = args.debugger_gdb_socket or str(args.gdb_socket)
    elif args.qmp_socket is None:
        args.debugger_gdb_socket = None
    if args.symbol_file and not args.symbol_file.is_file():
        raise ProfileError(f"symbol file is unreadable: {args.symbol_file}")
    if (args.kernel_image is None) != (args.symbol_manifest is None):
        raise ProfileError("--kernel-image and --symbol-manifest must be provided together")
    if args.kernel_image and not args.kernel_image.is_file():
        raise ProfileError(f"kernel image is unreadable: {args.kernel_image}")
    if args.symbol_manifest and not args.symbol_manifest.is_file():
        raise ProfileError(f"symbol manifest is unreadable: {args.symbol_manifest}")
    if args.plugin_socket:
        if args.symbol_map is None:
            raise ProfileError("plugin sampling requires --symbol-map")
        if args.plugin_summary is None:
            raise ProfileError("plugin sampling requires --plugin-summary")
        if args.plugin_stack_bytes > 4096 or args.plugin_stack_bytes % 8:
            raise ProfileError("plugin stack bytes must be a multiple of 8 up to 4096")
        if args.plugin_summary.exists():
            raise ProfileError("plugin summary path already exists")
    elif args.plugin_summary is not None:
        raise ProfileError("--plugin-summary requires --plugin-socket")
    if args.symbol_map and not args.symbol_map.is_file():
        raise ProfileError(f"symbol map is unreadable: {args.symbol_map}")
    args.kernel_sha256 = None
    args.symbol_manifest_sha256 = None
    args.symbol_manifest_target = None
    if args.symbol_manifest:
        if args.symbol_map is None:
            raise ProfileError("--symbol-manifest requires --symbol-map")
        manifest = load_kernel_map_manifest(args.symbol_manifest)
        kernel_sha256 = sha256_file(args.kernel_image)
        symbol_map_sha256 = sha256_file(args.symbol_map)
        if kernel_sha256 != manifest.kernel_sha256:
            raise ProfileError("kernel image hash does not match symbol manifest")
        if symbol_map_sha256 != manifest.symbol_map_sha256:
            raise ProfileError("symbol map hash does not match symbol manifest")
        args.kernel_sha256 = kernel_sha256
        args.symbol_manifest_sha256 = sha256_file(args.symbol_manifest)
        args.symbol_manifest_target = manifest.target
    if args.output.exists() or args.summary.exists():
        raise ProfileError("output and summary paths must not already exist")


def control_request(path: Path, command: str, label: str | None = None) -> dict[str, Any]:
    request: dict[str, Any] = {"command": command}
    if label is not None:
        request["label"] = label
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.settimeout(5)
    try:
        connection.connect(str(path))
        connection.sendall((json.dumps(request, separators=(",", ":")) + "\n").encode())
        response = b""
        while b"\n" not in response and len(response) <= 65536:
            chunk = connection.recv(4096)
            if not chunk:
                break
            response += chunk
    finally:
        connection.close()
    value = json.loads(response.split(b"\n", 1)[0])
    if not isinstance(value, dict):
        raise ProfileError("daemon returned a non-object response")
    return value


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    capture = subparsers.add_parser("capture", help="run the profiling daemon")
    capture.add_argument("--qemu-pid", type=positive_int, required=True)
    capture.add_argument("--qmp-socket", type=Path)
    capture.add_argument("--gdb-socket", type=Path)
    capture.add_argument("--plugin-socket", type=Path)
    capture.add_argument("--plugin-summary", type=Path)
    capture.add_argument("--serial-log", type=Path, required=True)
    capture.add_argument("--output", type=Path, required=True)
    capture.add_argument("--summary", type=Path, required=True)
    capture.add_argument("--control-socket", type=Path, required=True)
    capture.add_argument("--ready-file", type=Path)
    capture.add_argument("--system", required=True)
    capture.add_argument("--workload", required=True)
    capture.add_argument("--vcpu-count", type=positive_int, required=True)
    capture.add_argument("--proc-interval-ms", type=positive_int, default=1000)
    capture.add_argument("--stack-interval-ms", type=nonnegative_int, default=0)
    capture.add_argument("--stack-timeout-ms", type=positive_int, default=5000)
    capture.add_argument("--qmp-timeout-ms", type=positive_int, default=3000)
    capture.add_argument("--max-frames", type=positive_int, default=32)
    capture.add_argument("--max-pause-ratio", type=ratio, default=0.05)
    capture.add_argument("--symbol-file", type=Path)
    capture.add_argument("--symbol-map", type=Path)
    capture.add_argument("--kernel-image", type=Path)
    capture.add_argument("--symbol-manifest", type=Path)
    capture.add_argument("--plugin-period-insns", type=positive_int, default=50_000_000)
    capture.add_argument("--plugin-stack-bytes", type=nonnegative_int, default=1024)
    capture.add_argument("--debugger-symbol-file")
    capture.add_argument("--debugger-gdb-socket")
    capture.add_argument("--debugger-command", default="gdb-multiarch")
    capture.add_argument("--gdb-architecture", default="Loongarch64")
    capture.add_argument("--stage-regex", action="append", default=[])
    capture.add_argument("--environment", action="append", default=[])

    control = subparsers.add_parser("ctl", help="control a running daemon")
    control.add_argument("--socket", type=Path, required=True)
    control.add_argument("command", choices=("start", "stop", "status", "shutdown"))
    control.add_argument("--label", default="default")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.subcommand == "ctl":
        label = args.label if args.command == "start" else None
        response = control_request(args.socket, args.command, label)
        print(json.dumps(response, sort_keys=True))
        return 0 if response.get("ok") else 1

    try:
        validate_capture_args(args)
        daemon = ProfileDaemon(args)

        def request_shutdown(_signum: int, _frame: Any) -> None:
            daemon.running = False

        signal.signal(signal.SIGINT, request_shutdown)
        signal.signal(signal.SIGTERM, request_shutdown)
        return daemon.serve()
    except (OSError, ProfileError, ValueError, argparse.ArgumentTypeError) as error:
        print(f"qemu profile daemon: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
