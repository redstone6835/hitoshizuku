#!/usr/bin/env python3
"""严格校验并逐条比较 MyGO/Linux 的单次陷入动态指令。"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Iterable, NoReturn, TextIO


FIELD_NAME = re.compile(r"^[A-Za-z0-9_]+$")
UINT = re.compile(r"^[0-9]+$")
HEX = re.compile(r"^0x[0-9a-f]+$")
BYTE_STRING = re.compile(r"^(?:[0-9a-f]{2})+$")
TARGET_NAME = re.compile(r"^[A-Za-z0-9_.+-]+$")
LABEL_LINE = re.compile(r"^\s*([0-9a-fA-F]+)\s+<(.+)>:$")
STATIC_INSTRUCTION_LINE = re.compile(
    r"^\s*([0-9a-fA-F]+):\s+([0-9a-fA-F]+)\s+(\S.*)$"
)
UINT64_MAX = (1 << 64) - 1
ELF_MACHINE_RISCV = 243
ELF_SECTION_ALLOCATED = 0x2
ELF_SECTION_NOBITS = 8
ELF_SECTION_PROGBITS = 1
ELF_SECTION_INDEX_EXTENDED = 0xFFFF
ELF64_HEADER = struct.Struct("<16sHHIQQQIHHHHHH")
ELF64_SECTION_HEADER = struct.Struct("<IIQQQQIIQQ")
RISCV_ALTERNATIVE_ENTRY = struct.Struct("<iiHHI")

HEADER_FIELDS = {
    "version",
    "target",
    "configured_vcpus",
    "start_pc",
    "stop_pc",
    "max_instructions",
}
INSTRUCTION_FIELDS = {"sequence", "cpu", "pc", "size", "bytes", "disas_hex"}
FOOTER_FIELDS = {
    "instructions",
    "dropped",
    "translation_failures",
    "start_events",
    "stop_events",
    "active_at_exit",
}


class ChineseArgumentParser(argparse.ArgumentParser):
    """让命令行参数错误也保持中文。"""

    def format_usage(self) -> str:
        return super().format_usage().replace("usage:", "用法:", 1)

    def format_help(self) -> str:
        return super().format_help().replace("usage:", "用法:", 1)

    def error(self, message: str) -> NoReturn:
        required = "the following arguments are required: "
        unrecognized = "unrecognized arguments: "
        if message.startswith(required):
            message = "缺少必需参数：" + message.removeprefix(required)
        elif message.startswith(unrecognized):
            message = "无法识别的参数：" + message.removeprefix(unrecognized)
        elif message.startswith("argument "):
            message = "参数 " + message.removeprefix("argument ")
        self.print_usage(sys.stderr)
        self.exit(2, f"{self.prog}: 参数错误：{message}\n")


@dataclass(frozen=True)
class TraceInstruction:
    sequence: int
    pc: int
    size: int
    raw_bytes: str
    qemu_assembly: str


@dataclass(frozen=True)
class Trace:
    path: Path
    target: str
    start_pc: int
    stop_pc: int
    instructions: tuple[TraceInstruction, ...]


@dataclass(frozen=True)
class DisassembledInstruction:
    pc: int
    function: str
    function_start: int
    static_bytes: str
    static_assembly: str


@dataclass(frozen=True)
class ResolvedInstruction:
    trace: TraceInstruction
    space: str
    decoded: DisassembledInstruction

    @property
    def static_match(self) -> bool:
        return self.trace.raw_bytes == self.decoded.static_bytes

    @property
    def mnemonic(self) -> str:
        return self.trace.qemu_assembly.split(None, 1)[0]


@dataclass(frozen=True)
class ElfSection:
    name: str
    section_type: int
    flags: int
    address: int
    file_offset: int
    size: int


@dataclass(frozen=True)
class LinuxAlternative:
    entry_address: int
    old_address: int
    alt_address: int
    length: int
    vendor_id: int
    patch_id: int
    runtime_bytes: bytes


@dataclass(frozen=True)
class LinuxAlternativeValidation:
    dynamic_mismatches: int
    unique_pcs: int
    sites: tuple[LinuxAlternative, ...]


def parse_fields(line: str, record: str, required: set[str]) -> dict[str, str]:
    words = line.split()
    if not words or words[0] != record:
        raise ValueError(f"{record} 记录格式错误")
    values: dict[str, str] = {}
    for word in words[1:]:
        if word.count("=") != 1:
            raise ValueError(f"{record} 字段格式错误：{word!r}")
        name, value = word.split("=", 1)
        if not FIELD_NAME.fullmatch(name) or not value:
            raise ValueError(f"{record} 字段格式错误：{word!r}")
        if name in values:
            raise ValueError(f"{record} 字段重复：{name}")
        values[name] = value
    missing = required - values.keys()
    extra = values.keys() - required
    if missing or extra:
        raise ValueError(
            f"{record} 字段集合错误：缺少={sorted(missing)} 多余={sorted(extra)}"
        )
    return values


def as_uint(values: dict[str, str], name: str, record: str) -> int:
    value = values[name]
    if not UINT.fullmatch(value):
        raise ValueError(f"{record}.{name} 不是无符号整数：{value!r}")
    parsed = int(value)
    if parsed > UINT64_MAX:
        raise ValueError(f"{record}.{name} 超出 64 位范围")
    return parsed


def as_pc(values: dict[str, str], name: str, record: str) -> int:
    value = values[name]
    if not HEX.fullmatch(value):
        raise ValueError(f"{record}.{name} 不是规范的小写十六进制 PC：{value!r}")
    parsed = int(value, 16)
    if parsed > UINT64_MAX:
        raise ValueError(f"{record}.{name} 超出 64 位范围")
    return parsed


def parse_trace(path: Path) -> Trace:
    if not path.is_file():
        raise ValueError(f"跟踪文件不存在：{path}")

    records: list[tuple[int, str]] = []
    try:
        with path.open("r", encoding="utf-8", errors="strict", newline=None) as stream:
            for line_number, raw_line in enumerate(stream, 1):
                line = raw_line.strip()
                if line:
                    records.append((line_number, line))
    except UnicodeError as error:
        raise ValueError(f"跟踪文件不是合法 UTF-8：{path}") from error
    except OSError as error:
        raise ValueError(f"无法读取跟踪文件：{path}：{error.strerror}") from error

    if not records:
        raise ValueError(f"跟踪文件为空：{path}")
    if not records[0][1].startswith("MYGO_INSN_TRACE "):
        raise ValueError(f"{path}:{records[0][0]}：首条记录不是 MYGO_INSN_TRACE")
    if not records[-1][1].startswith("TRACE_DONE "):
        raise ValueError(f"{path}:{records[-1][0]}：末条记录不是 TRACE_DONE")

    headers: list[tuple[int, dict[str, str]]] = []
    footers: list[tuple[int, dict[str, str]]] = []
    instructions: list[TraceInstruction] = []
    for line_number, line in records:
        try:
            if line.startswith("MYGO_INSN_TRACE "):
                headers.append(
                    (line_number, parse_fields(line, "MYGO_INSN_TRACE", HEADER_FIELDS))
                )
            elif line.startswith("INSN "):
                values = parse_fields(line, "INSN", INSTRUCTION_FIELDS)
                sequence = as_uint(values, "sequence", "INSN")
                cpu = as_uint(values, "cpu", "INSN")
                pc = as_pc(values, "pc", "INSN")
                size = as_uint(values, "size", "INSN")
                raw_bytes = values["bytes"]
                disassembly_hex = values["disas_hex"]
                if cpu != 0:
                    raise ValueError(f"INSN.cpu={cpu}，单 vCPU 跟踪只允许 cpu=0")
                if size not in (2, 4):
                    raise ValueError(f"INSN.size={size}，RISC-V64 跟踪只允许 2 或 4")
                if pc & 1:
                    raise ValueError(f"INSN.pc=0x{pc:x} 未按 2 字节对齐")
                if not BYTE_STRING.fullmatch(raw_bytes) or len(raw_bytes) != size * 2:
                    raise ValueError(
                        f"INSN.bytes 长度或格式与 size={size} 不一致：{raw_bytes!r}"
                    )
                if not BYTE_STRING.fullmatch(disassembly_hex):
                    raise ValueError(
                        f"INSN.disas_hex 不是非空的小写十六进制字节串：{disassembly_hex!r}"
                    )
                try:
                    qemu_assembly = bytes.fromhex(disassembly_hex).decode(
                        "utf-8", errors="strict"
                    )
                except (ValueError, UnicodeError) as error:
                    raise ValueError("INSN.disas_hex 不是合法 UTF-8") from error
                if (
                    not qemu_assembly.strip()
                    or "\x00" in qemu_assembly
                    or "\r" in qemu_assembly
                    or "\n" in qemu_assembly
                ):
                    raise ValueError("INSN.disas_hex 解码后为空或包含非法控制字符")
                instructions.append(
                    TraceInstruction(
                        sequence,
                        pc,
                        size,
                        raw_bytes,
                        normalize_assembly(qemu_assembly),
                    )
                )
            elif line.startswith("TRACE_DONE "):
                footers.append(
                    (line_number, parse_fields(line, "TRACE_DONE", FOOTER_FIELDS))
                )
            else:
                raise ValueError(f"未知记录：{line.split()[0]!r}")
        except ValueError as error:
            raise ValueError(f"{path}:{line_number}：{error}") from error

    if len(headers) != 1:
        raise ValueError(f"{path}：MYGO_INSN_TRACE 数量为 {len(headers)}，预期为 1")
    if len(footers) != 1:
        raise ValueError(f"{path}：TRACE_DONE 数量为 {len(footers)}，预期为 1")
    if headers[0][0] != records[0][0] or footers[0][0] != records[-1][0]:
        raise ValueError(f"{path}：header/footer 位置非法")

    header = headers[0][1]
    footer = footers[0][1]
    if header["version"] != "1":
        raise ValueError(f"{path}：不支持的跟踪版本：{header['version']!r}")
    if not TARGET_NAME.fullmatch(header["target"]) or header["target"] != "riscv64":
        raise ValueError(f"{path}：目标架构不是 riscv64：{header['target']!r}")
    if as_uint(header, "configured_vcpus", "MYGO_INSN_TRACE") != 1:
        raise ValueError(f"{path}：configured_vcpus 必须为 1")
    start_pc = as_pc(header, "start_pc", "MYGO_INSN_TRACE")
    stop_pc = as_pc(header, "stop_pc", "MYGO_INSN_TRACE")
    maximum = as_uint(header, "max_instructions", "MYGO_INSN_TRACE")
    if start_pc == stop_pc or maximum == 0:
        raise ValueError(f"{path}：marker 地址必须不同且 max_instructions 必须非零")

    for expected, instruction in enumerate(instructions):
        if instruction.sequence != expected:
            raise ValueError(
                f"{path}：INSN sequence 在记录 {expected} 处为 {instruction.sequence}，"
                f"预期为 {expected}"
            )

    reported = as_uint(footer, "instructions", "TRACE_DONE")
    dropped = as_uint(footer, "dropped", "TRACE_DONE")
    failures = as_uint(footer, "translation_failures", "TRACE_DONE")
    starts = as_uint(footer, "start_events", "TRACE_DONE")
    stops = as_uint(footer, "stop_events", "TRACE_DONE")
    active = as_uint(footer, "active_at_exit", "TRACE_DONE")
    if reported != len(instructions):
        raise ValueError(
            f"{path}：footer instructions={reported}，实际 INSN 数量={len(instructions)}"
        )
    if reported == 0 or reported > maximum:
        raise ValueError(
            f"{path}：有效指令数 {reported} 不在 1..max_instructions({maximum}) 范围内"
        )
    if dropped != 0:
        raise ValueError(f"{path}：跟踪截断，dropped={dropped}")
    if failures != 0:
        raise ValueError(f"{path}：指令翻译失败，translation_failures={failures}")
    if starts != 1 or stops != 1 or active != 0:
        raise ValueError(
            f"{path}：marker 窗口不完整：start_events={starts} "
            f"stop_events={stops} active_at_exit={active}"
        )

    return Trace(path, header["target"], start_pc, stop_pc, tuple(instructions))


def is_kernel_pc(pc: int) -> bool:
    return bool(pc & (1 << 63))


def collect_instruction_sizes(
    traces: Iterable[Trace], kernel: bool
) -> dict[int, int]:
    sizes: dict[int, int] = {}
    for trace in traces:
        for instruction in trace.instructions:
            if is_kernel_pc(instruction.pc) != kernel:
                continue
            sizes[instruction.pc] = max(sizes.get(instruction.pc, 0), instruction.size)
    return sizes


def normalize_assembly(value: str) -> str:
    return " ".join(value.split())


def encoding_to_memory_bytes(encoding: str) -> str:
    """objdump 按数值打印编码，跟踪插件按客机内存顺序打印字节。"""

    octets = [encoding[index : index + 2] for index in range(0, len(encoding), 2)]
    return "".join(reversed(octets)).lower()


def is_local_label(name: str) -> bool:
    return name.startswith((".L", "$"))


def disassemble(
    objdump: str, artifact: Path, requested: dict[int, int], description: str
) -> dict[int, DisassembledInstruction]:
    if not requested:
        return {}
    if not artifact.is_file():
        raise ValueError(f"{description} 不存在：{artifact}")

    requested_pcs = set(requested)
    needed_addresses = {
        pc + offset for pc, size in requested.items() for offset in range(size)
    }
    command = [objdump, "-d", "-C", str(artifact)]
    environment = os.environ.copy()
    environment["LC_ALL"] = "C"
    try:
        with tempfile.TemporaryFile() as error_stream:
            process = subprocess.Popen(
                command,
                stdout=subprocess.PIPE,
                stderr=error_stream,
                text=True,
                encoding="utf-8",
                errors="strict",
                env=environment,
            )
            assert process.stdout is not None
            static_octets: dict[int, str] = {}
            owners: dict[int, tuple[str, int, str]] = {}
            current_function = "<未知函数>"
            current_function_start = 0
            try:
                for raw_line in process.stdout:
                    line = raw_line.rstrip("\r\n")
                    if line.startswith("Disassembly of section "):
                        current_function = "<未知函数>"
                        current_function_start = 0
                        continue
                    label = LABEL_LINE.match(line)
                    if label:
                        candidate = label.group(2)
                        if not is_local_label(candidate):
                            current_function_start = int(label.group(1), 16)
                            current_function = candidate
                        continue
                    instruction = STATIC_INSTRUCTION_LINE.match(line)
                    if not instruction:
                        continue
                    pc = int(instruction.group(1), 16)
                    encoding = instruction.group(2)
                    if len(encoding) < 4 or len(encoding) > 32 or len(encoding) % 2:
                        overlaps = any(
                            pc <= address < pc + len(encoding) // 2
                            for address in needed_addresses
                        )
                        if overlaps:
                            raise ValueError(
                                f"{description} 的 PC 0x{pc:x} 静态指令编码长度非法：{encoding}"
                            )
                        continue
                    static_bytes = encoding_to_memory_bytes(encoding)
                    assembly = normalize_assembly(instruction.group(3))
                    octets = [
                        static_bytes[index : index + 2]
                        for index in range(0, len(static_bytes), 2)
                    ]
                    for offset, octet in enumerate(octets):
                        address = pc + offset
                        if address in needed_addresses:
                            previous_octet = static_octets.get(address)
                            if previous_octet is not None and previous_octet != octet:
                                raise ValueError(
                                    f"{description} 的静态字节 0x{address:x} 出现冲突"
                                )
                            static_octets[address] = octet
                        if address in requested_pcs:
                            owner = (current_function, current_function_start, assembly)
                            previous_owner = owners.get(address)
                            if previous_owner is not None and previous_owner != owner:
                                raise ValueError(
                                    f"{description} 的 PC 0x{address:x} 函数归属出现冲突"
                                )
                            owners[address] = owner
            except UnicodeError as error:
                process.kill()
                process.wait()
                raise ValueError(f"{description} 的 objdump 输出不是合法 UTF-8") from error
            except Exception:
                process.kill()
                process.wait()
                raise
            return_code = process.wait()
            if return_code != 0:
                raise ValueError(
                    f"反汇编 {description} 失败：objdump 退出状态为 {return_code}"
                )
    except FileNotFoundError as error:
        raise ValueError(f"找不到 RISC-V objdump：{objdump}") from error
    except OSError as error:
        raise ValueError(f"无法运行 objdump：{error.strerror}") from error

    missing_pcs = {
        pc
        for pc, size in requested.items()
        if pc not in owners
        or any(pc + offset not in static_octets for offset in range(size))
    }
    if missing_pcs:
        examples = " ".join(f"0x{pc:x}" for pc in sorted(missing_pcs)[:8])
        suffix = " ..." if len(missing_pcs) > 8 else ""
        raise ValueError(
            f"{description} 无法取得 {len(missing_pcs)} 个动态 PC 的函数或静态字节："
            f"{examples}{suffix}"
        )
    found: dict[int, DisassembledInstruction] = {}
    for pc, size in requested.items():
        function, function_start, static_assembly = owners[pc]
        static_bytes = "".join(static_octets[pc + offset] for offset in range(size))
        found[pc] = DisassembledInstruction(
            pc, function, function_start, static_bytes, static_assembly
        )
    return found


def read_exact(
    stream: BinaryIO, file_size: int, offset: int, size: int, description: str
) -> bytes:
    if offset < 0 or size < 0 or offset > file_size or size > file_size - offset:
        raise ValueError(f"Linux vmlinux 中 {description} 超出文件范围")
    try:
        stream.seek(offset)
        data = stream.read(size)
    except OSError as error:
        raise ValueError(
            f"无法读取 Linux vmlinux 中 {description}：{error.strerror}"
        ) from error
    if len(data) != size:
        raise ValueError(f"Linux vmlinux 中 {description} 被截断")
    return data


def section_name(string_table: bytes, offset: int) -> str:
    if offset >= len(string_table):
        raise ValueError("Linux vmlinux 节名称偏移超出字符串表")
    end = string_table.find(b"\0", offset)
    if end < 0:
        raise ValueError("Linux vmlinux 节名称没有 NUL 结尾")
    try:
        return string_table[offset:end].decode("ascii", errors="strict")
    except UnicodeError as error:
        raise ValueError("Linux vmlinux 节名称不是 ASCII") from error


def parse_elf_sections(
    stream: BinaryIO, file_size: int
) -> tuple[ElfSection, ...]:
    header_data = read_exact(
        stream, file_size, 0, ELF64_HEADER.size, "ELF64 文件头"
    )
    header = ELF64_HEADER.unpack(header_data)
    identity = header[0]
    if identity[:4] != b"\x7fELF":
        raise ValueError("Linux vmlinux 不是 ELF 文件")
    if identity[4:7] != b"\x02\x01\x01":
        raise ValueError("Linux vmlinux 不是小端 ELF64 version 1")
    if header[2] != ELF_MACHINE_RISCV:
        raise ValueError(f"Linux vmlinux ELF e_machine={header[2]}，不是 RISC-V")

    section_offset = header[6]
    elf_header_size = header[8]
    section_entry_size = header[11]
    section_count = header[12]
    string_section_index = header[13]
    if elf_header_size != ELF64_HEADER.size:
        raise ValueError(
            f"Linux vmlinux ELF 文件头大小为 {elf_header_size}，预期为 {ELF64_HEADER.size}"
        )
    if section_entry_size != ELF64_SECTION_HEADER.size:
        raise ValueError(
            "Linux vmlinux ELF 节头大小为 "
            f"{section_entry_size}，预期为 {ELF64_SECTION_HEADER.size}"
        )
    if section_offset == 0:
        raise ValueError("Linux vmlinux 没有 ELF 节头表")

    first_data = read_exact(
        stream,
        file_size,
        section_offset,
        ELF64_SECTION_HEADER.size,
        "ELF 第 0 节头",
    )
    first = ELF64_SECTION_HEADER.unpack(first_data)
    if section_count == 0:
        section_count = first[5]
    if string_section_index == ELF_SECTION_INDEX_EXTENDED:
        string_section_index = first[6]
    if section_count == 0 or section_count > 1_000_000:
        raise ValueError(f"Linux vmlinux ELF 节数量非法：{section_count}")
    if string_section_index >= section_count:
        raise ValueError(
            "Linux vmlinux 节名称字符串表索引超出节头表："
            f"{string_section_index}/{section_count}"
        )

    table_size = section_count * ELF64_SECTION_HEADER.size
    section_data = read_exact(
        stream, file_size, section_offset, table_size, "ELF 节头表"
    )
    raw_sections = tuple(
        ELF64_SECTION_HEADER.unpack_from(
            section_data, index * ELF64_SECTION_HEADER.size
        )
        for index in range(section_count)
    )
    strings_header = raw_sections[string_section_index]
    string_table = read_exact(
        stream,
        file_size,
        strings_header[4],
        strings_header[5],
        "ELF 节名称字符串表",
    )

    sections: list[ElfSection] = []
    for raw in raw_sections:
        name = section_name(string_table, raw[0])
        section_type = raw[1]
        file_offset = raw[4]
        size = raw[5]
        if section_type != ELF_SECTION_NOBITS and (
            file_offset > file_size or size > file_size - file_offset
        ):
            raise ValueError(f"Linux vmlinux 中 ELF 节 {name!r} 超出文件范围")
        sections.append(
            ElfSection(name, section_type, raw[2], raw[3], file_offset, size)
        )
    return tuple(sections)


def read_virtual_bytes(
    stream: BinaryIO,
    file_size: int,
    sections: tuple[ElfSection, ...],
    address: int,
    size: int,
    description: str,
) -> bytes:
    if address < 0 or address > UINT64_MAX or size <= 0 or size > UINT64_MAX - address:
        raise ValueError(f"Linux alternatives {description} 地址或长度非法")
    end = address + size
    owners = [
        section
        for section in sections
        if section.flags & ELF_SECTION_ALLOCATED
        and section.section_type != ELF_SECTION_NOBITS
        and section.address <= address
        and end <= section.address + section.size
    ]
    if len(owners) != 1:
        raise ValueError(
            f"Linux alternatives {description} 0x{address:x}..0x{end:x} "
            f"对应 {len(owners)} 个可加载 ELF 节"
        )
    section = owners[0]
    offset = section.file_offset + address - section.address
    return read_exact(stream, file_size, offset, size, description)


def signed(value: int, bits: int) -> int:
    mask = (1 << bits) - 1
    value &= mask
    sign = 1 << (bits - 1)
    return value - (1 << bits) if value & sign else value


def extract_jal_immediate(instruction: int) -> int:
    immediate = (
        ((instruction >> 21) & 0x3FF) << 1
        | ((instruction >> 20) & 0x1) << 11
        | ((instruction >> 12) & 0xFF) << 12
        | ((instruction >> 31) & 0x1) << 20
    )
    return signed(immediate, 21)


def insert_jal_immediate(instruction: int, immediate: int) -> int:
    value = immediate & 0x1FFFFF
    instruction &= 0xFFF
    instruction |= (
        ((value >> 1) & 0x3FF) << 21
        | ((value >> 11) & 0x1) << 20
        | ((value >> 12) & 0xFF) << 12
        | ((value >> 20) & 0x1) << 31
    )
    return instruction


def fix_linux_alternative_offsets(
    replacement: bytes, old_address: int, alt_address: int
) -> bytes:
    if len(replacement) % 4:
        raise ValueError(
            f"Linux alternative replacement 长度 {len(replacement)} 不是 4 的倍数"
        )
    patch_offset = old_address - alt_address
    if not -(1 << 31) <= patch_offset < (1 << 31):
        raise ValueError(
            f"Linux alternative patch_offset={patch_offset} 超出内核 int 范围"
        )

    fixed = bytearray(replacement)
    instruction_count = len(fixed) // 4
    index = 0
    while index < instruction_count:
        offset = index * 4
        instruction = struct.unpack_from("<I", fixed, offset)[0]
        if instruction & 0x7F == 0x17 and index + 1 < instruction_count:
            next_offset = offset + 4
            next_instruction = struct.unpack_from("<I", fixed, next_offset)[0]
            if next_instruction & 0x707F == 0x67 and (instruction >> 7) & 0x1F == 1:
                upper = signed(instruction & 0xFFFFF000, 32)
                lower = signed(next_instruction >> 20, 12)
                immediate = signed(upper + lower - patch_offset, 32)
                instruction &= 0xFFF
                instruction |= (immediate & 0xFFFFF000) + (
                    (immediate & 0x800) << 1
                )
                next_instruction &= ~(0xFFF << 20)
                next_instruction |= (immediate & 0xFFF) << 20
                struct.pack_into("<I", fixed, offset, instruction & 0xFFFFFFFF)
                struct.pack_into(
                    "<I", fixed, next_offset, next_instruction & 0xFFFFFFFF
                )
                index += 2
                continue

        if instruction & 0x7F == 0x6F:
            immediate = extract_jal_immediate(instruction)
            target = old_address + offset + immediate
            if not old_address <= target < old_address + len(fixed):
                immediate -= patch_offset
                instruction = insert_jal_immediate(instruction, immediate)
                struct.pack_into("<I", fixed, offset, instruction & 0xFFFFFFFF)
        index += 1
    return bytes(fixed)


def load_linux_alternatives(
    path: Path, mismatch_rows: tuple[ResolvedInstruction, ...]
) -> tuple[LinuxAlternative, ...]:
    try:
        with path.open("rb") as stream:
            file_size = os.fstat(stream.fileno()).st_size
            sections = parse_elf_sections(stream, file_size)
            alternative_sections = [
                section for section in sections if section.name == ".alternative"
            ]
            if len(alternative_sections) != 1:
                raise ValueError(
                    "Linux vmlinux 的 .alternative 节数量为 "
                    f"{len(alternative_sections)}，预期为 1"
                )
            alternative_section = alternative_sections[0]
            if (
                alternative_section.section_type != ELF_SECTION_PROGBITS
                or not alternative_section.flags & ELF_SECTION_ALLOCATED
                or alternative_section.size == 0
                or alternative_section.size % RISCV_ALTERNATIVE_ENTRY.size
            ):
                raise ValueError("Linux vmlinux 的 .alternative 节属性或长度非法")
            contents = read_exact(
                stream,
                file_size,
                alternative_section.file_offset,
                alternative_section.size,
                "ELF .alternative 节",
            )

            alternatives: list[LinuxAlternative] = []
            for offset in range(0, len(contents), RISCV_ALTERNATIVE_ENTRY.size):
                old_offset, alt_offset, vendor_id, length, patch_id = (
                    RISCV_ALTERNATIVE_ENTRY.unpack_from(contents, offset)
                )
                entry_address = alternative_section.address + offset
                old_address = entry_address + old_offset
                alt_address = entry_address + 4 + alt_offset
                if (
                    length == 0
                    or length % 4
                    or old_address < 0
                    or alt_address < 0
                    or old_address > UINT64_MAX
                    or alt_address > UINT64_MAX
                    or old_address & 1
                    or alt_address & 1
                ):
                    raise ValueError(
                        f"Linux alt_entry@0x{entry_address:x} 地址或长度非法"
                    )
                relevant = any(
                    old_address <= row.trace.pc
                    and row.trace.pc + row.trace.size <= old_address + length
                    for row in mismatch_rows
                )
                if not relevant:
                    continue
                read_virtual_bytes(
                    stream,
                    file_size,
                    sections,
                    old_address,
                    length,
                    f"alt_entry@0x{entry_address:x} 原始代码",
                )
                replacement = read_virtual_bytes(
                    stream,
                    file_size,
                    sections,
                    alt_address,
                    length,
                    f"alt_entry@0x{entry_address:x} 替换代码",
                )
                alternatives.append(
                    LinuxAlternative(
                        entry_address,
                        old_address,
                        alt_address,
                        length,
                        vendor_id,
                        patch_id,
                        fix_linux_alternative_offsets(
                            replacement, old_address, alt_address
                        ),
                    )
                )
            return tuple(alternatives)
    except OSError as error:
        raise ValueError(f"无法读取 Linux vmlinux：{path}：{error.strerror}") from error


def validate_linux_alternative_mismatches(
    path: Path,
    rows: tuple[ResolvedInstruction, ...],
) -> LinuxAlternativeValidation:
    mismatch_rows = tuple(row for row in rows if not row.static_match)
    if any(row.space != "kernel" for row in mismatch_rows):
        pcs = " ".join(
            f"0x{row.trace.pc:x}" for row in mismatch_rows if row.space != "kernel"
        )
        raise ValueError(f"Linux 用户态动态/静态字节不一致，不能由 alternatives 解释：{pcs}")
    alternatives = load_linux_alternatives(path, mismatch_rows)

    groups: dict[tuple[int, int], list[LinuxAlternative]] = {}
    for alternative in alternatives:
        groups.setdefault(
            (alternative.old_address, alternative.length), []
        ).append(alternative)

    observations = {
        (row.trace.pc, row.trace.size, bytes.fromhex(row.trace.raw_bytes))
        for row in rows
        if row.space == "kernel"
    }
    mismatches = {
        (row.trace.pc, row.trace.size, bytes.fromhex(row.trace.raw_bytes))
        for row in mismatch_rows
    }
    validated: set[tuple[int, int, bytes]] = set()
    selected: list[LinuxAlternative] = []
    for (old_address, length), candidates in groups.items():
        site_mismatches = {
            observation
            for observation in mismatches
            if old_address <= observation[0]
            and observation[0] + observation[1] <= old_address + length
        }
        if not site_mismatches:
            continue
        site_observations = {
            observation
            for observation in observations
            if old_address <= observation[0]
            and observation[0] + observation[1] <= old_address + length
        }
        matching = [
            candidate
            for candidate in candidates
            if all(
                candidate.runtime_bytes[
                    pc - old_address : pc - old_address + size
                ]
                == raw_bytes
                for pc, size, raw_bytes in site_observations
            )
        ]
        if matching:
            validated.update(site_mismatches)
            selected.append(matching[0])

    uncovered = sorted(mismatches - validated)
    if uncovered:
        examples = " ".join(
            f"0x{pc:x}/{raw_bytes.hex()}" for pc, _, raw_bytes in uncovered[:8]
        )
        suffix = " ..." if len(uncovered) > 8 else ""
        raise ValueError(
            f"Linux 有 {len(uncovered)} 个动态字节差异无法由 .alternative "
            f"元数据及替换字节精确解释：{examples}{suffix}"
        )
    return LinuxAlternativeValidation(
        len(mismatch_rows),
        len({row.trace.pc for row in mismatch_rows}),
        tuple(sorted(selected, key=lambda alternative: alternative.old_address)),
    )


def resolve_trace(
    trace: Trace,
    user_disassembly: dict[int, DisassembledInstruction],
    kernel_disassembly: dict[int, DisassembledInstruction],
) -> tuple[ResolvedInstruction, ...]:
    resolved: list[ResolvedInstruction] = []
    for instruction in trace.instructions:
        space = "kernel" if is_kernel_pc(instruction.pc) else "user"
        source = kernel_disassembly if space == "kernel" else user_disassembly
        decoded = source.get(instruction.pc)
        if decoded is None:
            raise ValueError(
                f"{trace.path}：PC 0x{instruction.pc:x} 缺少 {space} 反汇编结果"
            )
        resolved.append(ResolvedInstruction(instruction, space, decoded))
    return tuple(resolved)


def validate_single_syscall_path(
    rows: tuple[ResolvedInstruction, ...], system: str
) -> None:
    spaces: list[str] = []
    for row in rows:
        if not spaces or spaces[-1] != row.space:
            spaces.append(row.space)
    if spaces != ["user", "kernel", "user"]:
        raise ValueError(f"{system} 权限态轨迹不是 user -> kernel -> user：{spaces}")
    if sum(row.mnemonic == "ecall" for row in rows) != 1:
        raise ValueError(f"{system} 轨迹必须恰好包含一个 ecall")
    if sum(row.mnemonic == "sret" for row in rows) != 1:
        raise ValueError(f"{system} 轨迹必须恰好包含一个 sret")

    first_kernel = next(index for index, row in enumerate(rows) if row.space == "kernel")
    last_kernel = len(rows) - 1 - next(
        index for index, row in enumerate(reversed(rows)) if row.space == "kernel"
    )
    if rows[first_kernel - 1].mnemonic != "ecall" or rows[last_kernel].mnemonic != "sret":
        raise ValueError(f"{system} ecall/sret 不在唯一内核区段的边界")
    entry_pc = rows[first_kernel].trace.pc
    if sum(row.trace.pc == entry_pc for row in rows) != 1:
        raise ValueError(f"{system} 内核入口 PC 重复执行，窗口内疑似发生嵌套 trap")


def is_faulting_memory_instruction(mnemonic: str) -> bool:
    base = mnemonic.removeprefix("c.")
    return base in {
        "lb",
        "lbu",
        "lh",
        "lhu",
        "lw",
        "lwu",
        "ld",
        "sb",
        "sh",
        "sw",
        "sd",
        "flw",
        "fld",
        "fsw",
        "fsd",
    }


def kernel_segments(
    rows: tuple[ResolvedInstruction, ...],
) -> tuple[tuple[int, int], ...]:
    segments: list[tuple[int, int]] = []
    index = 0
    while index < len(rows):
        if rows[index].space != "kernel":
            index += 1
            continue
        start = index
        while index + 1 < len(rows) and rows[index + 1].space == "kernel":
            index += 1
        segments.append((start, index))
        index += 1
    return tuple(segments)


def validate_page_fault_path(
    rows: tuple[ResolvedInstruction, ...], system: str
) -> None:
    if not rows or rows[0].space != "user" or rows[-1].space != "user":
        raise ValueError(f"{system} 缺页窗口没有完整的用户态起止边界")
    if any(row.mnemonic == "ecall" for row in rows):
        raise ValueError(f"{system} 缺页窗口内意外出现 ecall")
    segments = kernel_segments(rows)
    if not segments:
        raise ValueError(f"{system} 缺页窗口没有进入内核")
    sret_count = sum(row.mnemonic == "sret" for row in rows)
    if sret_count != len(segments):
        raise ValueError(
            f"{system} 内核区段={len(segments)}，sret={sret_count}，无法逐段配对"
        )

    for number, (first_kernel, last_kernel) in enumerate(segments, start=1):
        if first_kernel == 0 or last_kernel + 1 >= len(rows):
            raise ValueError(f"{system} 第 {number} 个缺页没有完整的故障/重放边界")
        if rows[last_kernel].mnemonic != "sret":
            raise ValueError(f"{system} 第 {number} 个内核区段不是以 sret 结束")

        fault = rows[first_kernel - 1]
        replay = rows[last_kernel + 1]
        if not is_faulting_memory_instruction(fault.mnemonic):
            raise ValueError(
                f"{system} 第 {number} 次陷入前不是可识别的 load/store："
                f"{fault.mnemonic}"
            )
        if (
            fault.trace.pc,
            fault.trace.size,
            fault.trace.raw_bytes,
            fault.trace.qemu_assembly,
        ) != (
            replay.trace.pc,
            replay.trace.size,
            replay.trace.raw_bytes,
            replay.trace.qemu_assembly,
        ):
            raise ValueError(
                f"{system} 第 {number} 次 sret 后没有重执行故障指令："
                f"before=0x{fault.trace.pc:x}/{fault.trace.qemu_assembly} "
                f"after=0x{replay.trace.pc:x}/{replay.trace.qemu_assembly}"
            )
        entry_pc = rows[first_kernel].trace.pc
        if sum(
            row.trace.pc == entry_pc for row in rows[first_kernel : last_kernel + 1]
        ) != 1:
            raise ValueError(
                f"{system} 第 {number} 个内核区段重复执行入口 PC，疑似嵌套 trap"
            )


def clean_tsv(value: str) -> str:
    return " ".join(value.replace("\t", " ").replace("\r", " ").split("\n"))


def write_tsv(path: Path, rows: Iterable[ResolvedInstruction]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_name: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w",
            encoding="utf-8",
            newline="\n",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            temporary_name = stream.name
            os.fchmod(stream.fileno(), 0o644)
            stream.write(
                "sequence\tspace\tpc\tsize\tbytes\tfunction\toffset\tassembly\tstatic_match\n"
            )
            for row in rows:
                offset = max(row.trace.pc - row.decoded.function_start, 0)
                fields = (
                    str(row.trace.sequence),
                    row.space,
                    f"0x{row.trace.pc:x}",
                    str(row.trace.size),
                    row.trace.raw_bytes,
                    clean_tsv(row.decoded.function),
                    f"0x{offset:x}",
                    clean_tsv(row.trace.qemu_assembly),
                    "true" if row.static_match else "false",
                )
                stream.write("\t".join(fields) + "\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_name, path)
        temporary_name = None
    except OSError as error:
        raise ValueError(f"无法写入 TSV：{path}：{error.strerror}") from error
    finally:
        if temporary_name is not None:
            try:
                Path(temporary_name).unlink()
            except FileNotFoundError:
                pass


def percentage(part: int, total: int) -> str:
    return f"{part * 100.0 / total:.2f}%" if total else "0.00%"


def ratio(mygo: int, linux: int) -> str:
    if linux == 0:
        return "不可比" if mygo else "1.000x"
    return f"{mygo / linux:.3f}x"


def counts(rows: tuple[ResolvedInstruction, ...]) -> dict[str, int]:
    return {
        "总指令": len(rows),
        "用户态": sum(row.space == "user" for row in rows),
        "内核态": sum(row.space == "kernel" for row in rows),
        "2 字节": sum(row.trace.size == 2 for row in rows),
        "4 字节": sum(row.trace.size == 4 for row in rows),
        "静态一致": sum(row.static_match for row in rows),
    }


def print_counter(
    title: str, counter: Counter[str], total: int, top: int, stream: TextIO
) -> None:
    print(title, file=stream)
    print(f"  {'项目':<46} {'指令数':>10} {'占比':>9}", file=stream)
    for name, count in counter.most_common(top):
        display = name if len(name) <= 46 else name[:43] + "..."
        print(f"  {display:<46} {count:>10} {percentage(count, total):>9}", file=stream)


def print_summary(
    mygo: tuple[ResolvedInstruction, ...],
    linux: tuple[ResolvedInstruction, ...],
    mygo_output: Path,
    linux_output: Path,
    top: int,
    path_kind: str,
) -> None:
    mygo_counts = counts(mygo)
    linux_counts = counts(linux)
    title = "单次系统调用" if path_kind == "syscall" else "匿名页首次访问窗口"
    print(f"{title}动态指令比较")
    if path_kind == "syscall":
        integrity = "唯一 ecall/sret"
    else:
        integrity = (
            "零 ecall、每段唯一 sret、故障 load/store 返回后原 PC 重放；"
            f"缺页段 MyGO={len(kernel_segments(mygo))} "
            f"Linux={len(kernel_segments(linux))}"
        )
    print(f"  路径完整性：{integrity}，无嵌套 trap")
    print(f"  {'维度':<12} {'MyGO':>10} {'Linux':>10} {'差值':>11} {'MyGO/Linux':>12}")
    for name in ("总指令", "用户态", "内核态", "2 字节", "4 字节", "静态一致"):
        left_count = mygo_counts[name]
        right_count = linux_counts[name]
        print(
            f"  {name:<12} {left_count:>10} {right_count:>10} "
            f"{left_count - right_count:>+11} {ratio(left_count, right_count):>12}"
        )

    for system, rows in (("MyGO", mygo), ("Linux", linux)):
        mnemonic_counts = Counter(row.mnemonic for row in rows)
        function_counts = Counter(
            f"{row.space}:{row.decoded.function}" for row in rows
        )
        print()
        print_counter(f"{system} 主要助记符（前 {top} 项）", mnemonic_counts, len(rows), top, sys.stdout)
        print()
        print_counter(f"{system} 主要函数（前 {top} 项）", function_counts, len(rows), top, sys.stdout)

    common = 0
    for left_row, right_row in zip(mygo, linux):
        left_key = (
            left_row.space,
            left_row.trace.pc,
            left_row.trace.size,
            left_row.trace.raw_bytes,
            left_row.trace.qemu_assembly,
        )
        right_key = (
            right_row.space,
            right_row.trace.pc,
            right_row.trace.size,
            right_row.trace.raw_bytes,
            right_row.trace.qemu_assembly,
        )
        if left_key != right_key:
            break
        common += 1
    print()
    print(f"两侧完全相同的动态指令前缀：{common} 条")
    if common < min(len(mygo), len(linux)):
        left_row = mygo[common]
        right_row = linux[common]
        print(
            "  首个分歧 MyGO："
            f"sequence={left_row.trace.sequence} {left_row.space} "
            f"pc=0x{left_row.trace.pc:x} {left_row.decoded.function}"
            f"+0x{left_row.trace.pc - left_row.decoded.function_start:x} "
            f"{left_row.trace.qemu_assembly}"
        )
        print(
            "  首个分歧 Linux："
            f"sequence={right_row.trace.sequence} {right_row.space} "
            f"pc=0x{right_row.trace.pc:x} {right_row.decoded.function}"
            f"+0x{right_row.trace.pc - right_row.decoded.function_start:x} "
            f"{right_row.trace.qemu_assembly}"
        )
    elif len(mygo) != len(linux):
        longer = "MyGO" if len(mygo) > len(linux) else "Linux"
        print(f"  公共前缀后仅 {longer} 仍有动态指令。")

    print()
    print(f"MyGO 完整动态顺序 TSV：{mygo_output}")
    print(f"Linux 完整动态顺序 TSV：{linux_output}")


def validate_paths(args: argparse.Namespace) -> None:
    inputs = {
        args.mygo_trace.resolve(),
        args.linux_trace.resolve(),
        args.benchmark_elf.resolve(),
        args.mygo_kernel.resolve(),
        args.linux_vmlinux.resolve(),
    }
    mygo_output = args.mygo_output.resolve()
    linux_output = args.linux_output.resolve()
    if mygo_output == linux_output:
        raise ValueError("MyGO/Linux TSV 输出路径不能相同")
    if mygo_output in inputs or linux_output in inputs:
        raise ValueError("TSV 输出路径不能覆盖输入产物")
    if not 1 <= args.top <= 100:
        raise ValueError("--top 必须位于 1..100")


def parse_integer(value: str) -> int:
    try:
        return int(value, 10)
    except ValueError as error:
        raise argparse.ArgumentTypeError(f"必须是十进制整数：{value!r}") from error


def build_parser() -> ChineseArgumentParser:
    parser = ChineseArgumentParser(
        description="严格校验、符号化并比较 MyGO/Linux 单次陷入动态指令跟踪。",
        add_help=False,
    )
    parser.add_argument("--mygo-trace", required=True, type=Path, help="MyGO 原始指令跟踪")
    parser.add_argument("--linux-trace", required=True, type=Path, help="Linux 原始指令跟踪")
    parser.add_argument("--benchmark-elf", required=True, type=Path, help="共同的 benchmark ELF")
    parser.add_argument("--mygo-kernel", required=True, type=Path, help="MyGO 可符号化内核 ELF")
    parser.add_argument(
        "--linux-vmlinux",
        "--linux-kernel",
        dest="linux_vmlinux",
        required=True,
        type=Path,
        help="Linux vmlinux",
    )
    parser.add_argument("--mygo-output", required=True, type=Path, help="MyGO 完整顺序 TSV")
    parser.add_argument("--linux-output", required=True, type=Path, help="Linux 完整顺序 TSV")
    parser.add_argument(
        "--objdump",
        default=os.environ.get("RISCV64_OBJDUMP", "riscv64-linux-gnu-objdump"),
        help="RISC-V objdump 命令",
    )
    parser.add_argument(
        "--top", type=parse_integer, default=12, help="摘要中显示的热点数量（默认 12）"
    )
    parser.add_argument(
        "--path-kind",
        choices=("syscall", "page-fault"),
        default="syscall",
        help="被测陷入类型（默认 syscall）",
    )
    parser.add_argument(
        "--allow-linux-alternatives",
        action="store_true",
        help="仅允许被 Linux .alternative 元数据和替换字节精确证明的差异",
    )
    parser.add_argument("-h", "--help", action="help", help="显示本帮助并退出")
    parser._optionals.title = "选项"
    return parser


def run(args: argparse.Namespace) -> int:
    validate_paths(args)
    objdump = args.objdump
    if os.sep not in objdump and shutil.which(objdump) is None:
        raise ValueError(f"找不到 RISC-V objdump：{objdump}")

    mygo_trace = parse_trace(args.mygo_trace)
    linux_trace = parse_trace(args.linux_trace)
    if (mygo_trace.start_pc, mygo_trace.stop_pc) != (
        linux_trace.start_pc,
        linux_trace.stop_pc,
    ):
        raise ValueError(
            "MyGO/Linux marker 地址不一致："
            f"0x{mygo_trace.start_pc:x}/0x{mygo_trace.stop_pc:x} 与 "
            f"0x{linux_trace.start_pc:x}/0x{linux_trace.stop_pc:x}"
        )

    user_requests = collect_instruction_sizes((mygo_trace, linux_trace), kernel=False)
    mygo_kernel_requests = collect_instruction_sizes((mygo_trace,), kernel=True)
    linux_kernel_requests = collect_instruction_sizes((linux_trace,), kernel=True)

    user_disassembly = disassemble(
        objdump, args.benchmark_elf, user_requests, "benchmark ELF"
    )
    mygo_disassembly = disassemble(
        objdump, args.mygo_kernel, mygo_kernel_requests, "MyGO 内核 ELF"
    )
    linux_disassembly = disassemble(
        objdump, args.linux_vmlinux, linux_kernel_requests, "Linux vmlinux"
    )
    mygo_rows = resolve_trace(mygo_trace, user_disassembly, mygo_disassembly)
    linux_rows = resolve_trace(linux_trace, user_disassembly, linux_disassembly)
    mygo_mismatches = sum(not row.static_match for row in mygo_rows)
    linux_mismatches = sum(not row.static_match for row in linux_rows)
    if mygo_mismatches:
        raise ValueError(
            f"MyGO 有 {mygo_mismatches} 条动态字节与 ELF 静态字节不一致；"
            "Linux alternatives 许可绝不适用于 MyGO"
        )
    if linux_mismatches and not args.allow_linux_alternatives:
        raise ValueError(
            f"Linux 有 {linux_mismatches} 条动态字节与 ELF 静态字节不一致；"
            "可使用 --allow-linux-alternatives 进行严格 alternatives 验证"
        )
    if linux_mismatches:
        validation = validate_linux_alternative_mismatches(
            args.linux_vmlinux, linux_rows
        )
        print(
            "Linux alternatives 严格验证通过："
            f"动态差异={validation.dynamic_mismatches} "
            f"唯一PC={validation.unique_pcs} 补丁位点={len(validation.sites)}；"
            "动态字节与 replacement（含 JAL/AUIPC+JALR 重定位）完全一致",
            file=sys.stderr,
        )
    validator = (
        validate_single_syscall_path
        if args.path_kind == "syscall"
        else validate_page_fault_path
    )
    validator(mygo_rows, "MyGO")
    validator(linux_rows, "Linux")

    write_tsv(args.mygo_output, mygo_rows)
    write_tsv(args.linux_output, linux_rows)
    print_summary(
        mygo_rows,
        linux_rows,
        args.mygo_output,
        args.linux_output,
        args.top,
        args.path_kind,
    )
    return 0


def main() -> int:
    try:
        return run(build_parser().parse_args())
    except ValueError as error:
        print(f"陷入指令比较失败：{error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
