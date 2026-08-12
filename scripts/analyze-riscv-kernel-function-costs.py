#!/usr/bin/env python3
"""将 RISC-V BuildStorm 内核指令和微基准成本近似归因到 ELF 函数。"""

from __future__ import annotations

import argparse
import bisect
import collections
import csv
import dataclasses
import hashlib
import io
import json
import math
import os
import re
import shutil
import statistics
import subprocess
import tempfile
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any, BinaryIO

from rv_instruction_profile_io import (
    MatchStatistics,
    SampleLocation,
    TbCatalogRecord,
    TimeAwareJitMap,
    iter_matched_jit_records,
    read_tid_namespace_tsv,
    sorted_perf_samples,
)


OUTPUT_SCHEMA = "mygo.riscv-kernel-function-costs.v3"
ELM_INTERFACE_SCHEMA = "ELM-KERNEL-INTERFACE-V1"
ELM_API_PREFIX = "__elm_kernel_api_"
VCPU_COMM = re.compile(r"CPU ([0-9]+)/TCG\Z")
READELF_SYMBOL_LINE = re.compile(
    r"^\s*\d+:\s+([0-9a-fA-F]+)\s+(\d+)\s+FUNC\s+(\S+)\s+\S+\s+(\S+)\s+(\S+)\s*$"
)
READELF_TEXT_LINE = re.compile(
    r"\[\s*\d+\]\s+\.text\s+\S+\s+([0-9a-fA-F]+)\s+\S+\s+([0-9a-fA-F]+)"
)


class FunctionCostError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise FunctionCostError(message)


def atomic_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def atomic_json(path: Path, value: Any) -> None:
    atomic_text(path, json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def atomic_csv(
    path: Path, fields: Sequence[str], rows: Iterable[Mapping[str, Any]]
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as stream:
            writer = csv.DictWriter(stream, fieldnames=fields, lineterminator="\n")
            writer.writeheader()
            writer.writerows(rows)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def optional_float(value: str | None) -> float | None:
    if value is None or value == "":
        return None
    result = float(value)
    require(math.isfinite(result), "成本表包含非有限数")
    return result


def csv_bool(value: str) -> bool:
    require(value in {"True", "False"}, f"非法布尔字段：{value!r}")
    return value == "True"


@dataclasses.dataclass(frozen=True)
class DescriptorCost:
    descriptor_id: int
    mnemonic: str
    size_bytes: int
    exact_kernel_count: int
    assignment: str
    quality: str
    bounded: bool
    strict: bool
    point_ns: float | None
    low_ns: float | None
    high_ns: float | None
    center_ns: float | None
    allocation_weight_ns: float
    allocation_weight_imputed: bool


def load_kernel_descriptor_costs(
    path: Path,
) -> tuple[dict[int, DescriptorCost], dict[str, Any]]:
    raw: list[dict[str, Any]] = []
    with path.open(newline="", encoding="utf-8") as stream:
        for row in csv.DictReader(stream):
            if row["domain"] != "kernel":
                continue
            raw.append(
                {
                    "descriptor_id": int(row["descriptor_id"]),
                    "mnemonic": row["mnemonic"],
                    "size_bytes": int(row["size_bytes"]),
                    "exact_kernel_count": int(row["domain_count"]),
                    "assignment": row["assignment"],
                    "quality": row["quality"],
                    "bounded": csv_bool(row["bounded"]),
                    "strict": csv_bool(row["strict"]),
                    "point_ns": optional_float(row["identified_weight_ns"]),
                    "low_ns": optional_float(row["weight_envelope_low_ns"]),
                    "high_ns": optional_float(row["weight_envelope_high_ns"]),
                    "center_ns": optional_float(row["diagnostic_context_center_ns"]),
                }
            )
    require(raw, "逐指令成本表没有内核行")
    positive_centers = [
        row["center_ns"]
        for row in raw
        if row["bounded"] and row["center_ns"] is not None and row["center_ns"] > 0.0
    ]
    require(positive_centers, "逐指令成本表没有正的有限中心权重")
    fallback = statistics.median(positive_centers)
    result: dict[int, DescriptorCost] = {}
    for row in raw:
        descriptor_id = row["descriptor_id"]
        require(descriptor_id not in result, f"重复内核 descriptor {descriptor_id}")
        center = row["center_ns"]
        imputed = not row["bounded"] or center is None or center <= 0.0
        allocation_weight = fallback if imputed else center
        result[descriptor_id] = DescriptorCost(
            **row,
            allocation_weight_ns=allocation_weight,
            allocation_weight_imputed=imputed,
        )
    return result, {
        "descriptor_count": len(result),
        "exact_kernel_instruction_count": sum(
            row.exact_kernel_count for row in result.values()
        ),
        "allocation_fallback_ns": fallback,
        "imputed_descriptor_count": sum(
            row.allocation_weight_imputed for row in result.values()
        ),
    }


@dataclasses.dataclass(frozen=True, order=True)
class FunctionSymbol:
    address: int
    size: int
    name: str
    kind: str
    aliases: tuple[str, ...] = ()
    elm_api_names: tuple[str, ...] = ()
    elm_api_rust_names: tuple[str, ...] = ()
    elm_api_contracts: tuple[str, ...] = ()

    @property
    def end(self) -> int:
        return self.address + self.size


UNRESOLVED_SYMBOL = FunctionSymbol(-1, 0, "[unresolved-static-kernel-symbol]", "?")
UNSAMPLED_SYMBOL = FunctionSymbol(-2, 0, "[descriptor-without-sampled-tb]", "?")
DYNAMIC_CODE_SYMBOL = FunctionSymbol(-3, 0, "[dynamic-kernel-code-outside-ELF]", "?")


@dataclasses.dataclass(frozen=True)
class ElmApiMetadata:
    name: str
    rust_name: str
    linker_symbol: str
    contract: str


@dataclasses.dataclass(frozen=True)
class RiscvDisassembledInstruction:
    """objdump 中的一条指令以及所在的 ELF 函数地址。"""

    address: int
    mnemonic: str
    operands: str
    function_address: int | None


@dataclasses.dataclass(frozen=True)
class DirectCallGraph:
    """静态可解析的 direct-call 图（边方向为 caller -> callee）。"""

    edges: dict[FunctionSymbol, set[FunctionSymbol]]
    instruction_count: int
    call_site_count: int
    resolved_call_site_count: int
    unresolved_call_site_count: int
    resolved_tail_transfer_site_count: int
    tail_edges: frozenset[tuple[FunctionSymbol, FunctionSymbol]]


@dataclasses.dataclass(frozen=True)
class InclusiveClosure:
    """SCC 压缩后的可达闭包，成员集合不包含重复路径。"""

    members: dict[FunctionSymbol, frozenset[FunctionSymbol]]
    component_count: int
    recursive_component_count: int


OBJDUMP_FUNCTION_LINE = re.compile(
    r"^\s*([0-9a-fA-F]+)\s+<[^>]+>:\s*$"
)
OBJDUMP_INSTRUCTION_LINE = re.compile(
    r"^\s*([0-9a-fA-F]+):\s*(.*?)\s*$"
)
REGISTER_ALIASES = {
    "x1": "ra",
    "x5": "t0",
    "x6": "t1",
    "x7": "t2",
    "x28": "t3",
    "x29": "t4",
    "x30": "t5",
    "x31": "t6",
    "x0": "zero",
}
HEX_NUMBER = re.compile(r"(?<![A-Za-z0-9_])(?:0x)?[0-9a-fA-F]+")
TARGET_WITH_SYMBOL = re.compile(r"((?:0x)?[0-9a-fA-F]+)\s*<[^>]+>")
JALR_OPERANDS = re.compile(
    r"^(?:(?P<rd>[A-Za-z0-9]+)\s*,\s*)?"
    r"(?P<offset>[+-]?(?:0x)?[0-9a-fA-F]+)\((?P<base>[A-Za-z0-9]+)\)"
)


def normalize_register(value: str) -> str:
    register = value.strip().lower()
    return REGISTER_ALIASES.get(register, register)


def parse_integer(value: str) -> int | None:
    token = value.strip().lower()
    try:
        return int(token, 0)
    except ValueError:
        if token.startswith("-"):
            try:
                return -int(token[1:], 16)
            except ValueError:
                return None
        try:
            return int(token, 16)
        except ValueError:
            return None


def parse_target_integer(value: str) -> int | None:
    token = value.strip().lower()
    try:
        return (
            int(token, 0)
            if token.startswith(("0x", "+0x", "-0x"))
            else int(token, 16)
        )
    except ValueError:
        return None


def parse_riscv_objdump(content: str) -> list[RiscvDisassembledInstruction]:
    """解析 GNU/LLVM RISC-V objdump 的函数头和指令行。

    调用目标在后续阶段解析，因为 ``auipc``+``jalr`` 需要查看相邻指令。
    ``--no-show-raw-insn`` 的输出不含机器码；若调用方没有该选项，函数也会
    跳过前面的十六进制机器码后读取助记符。
    """

    result: list[RiscvDisassembledInstruction] = []
    function_address: int | None = None
    for line in content.splitlines():
        function_match = OBJDUMP_FUNCTION_LINE.match(line)
        if function_match is not None:
            function_address = int(function_match.group(1), 16)
            continue
        instruction_match = OBJDUMP_INSTRUCTION_LINE.match(line)
        if instruction_match is None:
            continue
        address = int(instruction_match.group(1), 16)
        body = instruction_match.group(2).strip()
        if not body:
            continue
        tokens = body.split()
        mnemonic_index = next(
            (
                token_index
                for token_index, token in enumerate(tokens)
                if re.match(r"^[A-Za-z.][A-Za-z0-9_.]*$", token)
                and not (
                    re.fullmatch(r"[0-9a-fA-F]+", token)
                    and len(token) in {2, 4, 8}
                )
            ),
            None,
        )
        if mnemonic_index is None:
            continue
        mnemonic = tokens[mnemonic_index].lower()
        operands = " ".join(tokens[mnemonic_index + 1 :])
        result.append(
            RiscvDisassembledInstruction(
                address, mnemonic, operands, function_address
            )
        )
    return result


def _target_from_operands(operands: str) -> int | None:
    """读取 objdump 在 ``# address <symbol>`` 中给出的绝对目标地址。"""

    comment = operands.split("#", 1)[1] if "#" in operands else ""
    match = TARGET_WITH_SYMBOL.search(comment)
    if match is not None:
        return parse_target_integer(match.group(1))
    if comment:
        match = HEX_NUMBER.search(comment)
        if match is not None:
            return parse_target_integer(match.group(0))
    match = TARGET_WITH_SYMBOL.search(operands)
    if match is not None:
        return parse_target_integer(match.group(1))
    return None


def _split_operands(operands: str) -> list[str]:
    return [part.strip() for part in operands.split(",") if part.strip()]


def _sign_extend(value: int, bits: int) -> int:
    sign = 1 << (bits - 1)
    return (value ^ sign) - sign


def _direct_call_target(
    instructions: Sequence[RiscvDisassembledInstruction], index: int
) -> int | None:
    instruction = instructions[index]
    mnemonic = instruction.mnemonic
    operands = instruction.operands
    parts = _split_operands(operands)
    if mnemonic in {"call", "call.t0"}:
        return _target_from_operands(operands)
    if mnemonic == "jal":
        # ``jal target`` is the ra pseudo-instruction; ``jal zero,target`` is
        # an unconditional jump and must not contribute a call edge.
        if len(parts) == 1:
            target = _target_from_operands(operands)
            return target if target is not None else parse_target_integer(parts[0])
        if not parts or normalize_register(parts[0]) != "ra":
            return None
        target = _target_from_operands(operands)
        if target is not None:
            return target
        return parse_target_integer(parts[1]) if len(parts) >= 2 else None
    if mnemonic != "jalr":
        return None

    match = JALR_OPERANDS.match(operands)
    if match is None:
        return None
    rd = normalize_register(match.group("rd") or "ra")
    if rd != "ra":
        return None
    target = _target_from_operands(operands)
    if target is not None:
        return target

    # Linker-relaxed calls retain an AUIPC immediately before JALR.  Decode the
    # pair when objdump did not print a relocation target comment.
    if index == 0:
        return None
    previous = instructions[index - 1]
    if previous.function_address != instruction.function_address:
        return None
    if (
        instruction.address <= previous.address
        or instruction.address - previous.address > 8
    ):
        return None
    if previous.mnemonic != "auipc":
        return None
    auipc_parts = _split_operands(previous.operands)
    if len(auipc_parts) != 2:
        return None
    auipc_register = normalize_register(auipc_parts[0])
    if normalize_register(match.group("base")) != auipc_register:
        return None
    immediate = parse_integer(auipc_parts[1])
    offset = parse_integer(match.group("offset"))
    if immediate is None or offset is None:
        return None
    # GNU objdump prints the 20-bit AUIPC immediate before the implicit shift.
    return previous.address + (_sign_extend(immediate, 20) << 12) + offset


def _is_call_site(instruction: RiscvDisassembledInstruction) -> bool:
    if instruction.mnemonic in {"call", "call.t0"}:
        return True
    parts = _split_operands(instruction.operands)
    if instruction.mnemonic == "jal":
        return len(parts) == 1 or (
            len(parts) >= 2 and normalize_register(parts[0]) == "ra"
        )
    if instruction.mnemonic != "jalr":
        return False
    match = JALR_OPERANDS.match(instruction.operands)
    if match is not None:
        return normalize_register(match.group("rd") or "ra") == "ra"
    # GNU objdump uses ``jalr register`` for the indirect ra-link pseudo-op.
    return len(parts) == 1 and re.fullmatch(r"[A-Za-z0-9]+", parts[0]) is not None


def build_direct_call_graph(
    instructions: Sequence[RiscvDisassembledInstruction],
    symbols: Sequence[FunctionSymbol],
    resolver: SymbolResolver,
) -> DirectCallGraph:
    """从反汇编构建可解析 direct-call 图。

    只有目标能定位到正式 ELF 函数符号的调用才形成图边；未知的 ELF 内部
    目标和动态 ELM 目标仍计入 ``unresolved_call_site_count``，避免静默夸大
    inclusive 成本。
    """

    edges: dict[FunctionSymbol, set[FunctionSymbol]] = {
        symbol: set() for symbol in symbols if symbol.address >= 0
    }
    call_sites = 0
    resolved = 0
    unresolved = 0
    tail_edges: set[tuple[FunctionSymbol, FunctionSymbol]] = set()
    for index, instruction in enumerate(instructions):
        if not _is_call_site(instruction):
            if instruction.mnemonic not in {"j", "tail"}:
                continue
            target = _target_from_operands(instruction.operands)
            if target is None:
                continue
            caller = resolver.resolve(instruction.address)
            callee = resolver.resolve(target)
            if caller.address < 0 or callee.address < 0 or caller == callee:
                continue
            edges.setdefault(caller, set()).add(callee)
            tail_edges.add((caller, callee))
            continue
        call_sites += 1
        target = _direct_call_target(instructions, index)
        if target is None:
            unresolved += 1
            continue
        caller = resolver.resolve(instruction.address)
        callee = resolver.resolve(target)
        if caller.address < 0 or callee.address < 0:
            unresolved += 1
            continue
        edges.setdefault(caller, set()).add(callee)
        resolved += 1
    return DirectCallGraph(
        edges,
        len(instructions),
        call_sites,
        resolved,
        unresolved,
        len(tail_edges),
        frozenset(tail_edges),
    )


def load_direct_call_graph(
    kernel: Path,
    objdump: str,
    symbols: Sequence[FunctionSymbol],
    resolver: SymbolResolver,
) -> DirectCallGraph:
    """运行交叉 objdump 并构建静态 direct-call 图。"""

    process = subprocess.run(
        [objdump, "-d", "--wide", "--no-show-raw-insn", str(kernel)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    require(
        process.returncode == 0,
        f"objdump 反汇编失败（{objdump}）：{process.stderr.strip()}",
    )
    instructions = parse_riscv_objdump(process.stdout)
    require(instructions, "objdump 没有产生可解析的 RISC-V 指令")
    return build_direct_call_graph(instructions, symbols, resolver)


def select_objdump(requested: str | None) -> str:
    if requested is not None:
        return requested
    for candidate in (
        "riscv64-linux-gnu-objdump",
        "riscv64-unknown-elf-objdump",
        "llvm-objdump",
        "objdump",
    ):
        if shutil.which(candidate) is not None:
            return candidate
    raise FunctionCostError("没有找到可用的 RISC-V objdump")


def _strongly_connected_components(
    graph: Mapping[FunctionSymbol, Iterable[FunctionSymbol]],
) -> tuple[list[tuple[FunctionSymbol, ...]], dict[FunctionSymbol, int]]:
    """Kosaraju SCC，使用显式栈以覆盖较深的内核调用链。"""

    nodes = set(graph)
    for targets in graph.values():
        nodes.update(targets)
    adjacency: dict[FunctionSymbol, tuple[FunctionSymbol, ...]] = {}
    for node in nodes:
        adjacency[node] = tuple(
            sorted(
                set(graph.get(node, ())),
                key=lambda item: (item.address, item.name),
            )
        )
    for node in nodes:
        adjacency.setdefault(node, tuple())
    reverse: dict[FunctionSymbol, list[FunctionSymbol]] = {
        node: [] for node in nodes
    }
    for caller, targets in adjacency.items():
        for callee in targets:
            reverse[callee].append(caller)
    for node in reverse:
        reverse[node].sort(key=lambda item: (item.address, item.name))

    visited: set[FunctionSymbol] = set()
    finish_order: list[FunctionSymbol] = []
    for root in sorted(nodes, key=lambda item: (item.address, item.name)):
        if root in visited:
            continue
        visited.add(root)
        stack: list[tuple[FunctionSymbol, int]] = [(root, 0)]
        while stack:
            node, next_index = stack[-1]
            if next_index < len(adjacency[node]):
                target = adjacency[node][next_index]
                stack[-1] = (node, next_index + 1)
                if target not in visited:
                    visited.add(target)
                    stack.append((target, 0))
            else:
                finish_order.append(node)
                stack.pop()

    components: list[tuple[FunctionSymbol, ...]] = []
    assigned: set[FunctionSymbol] = set()
    for root in reversed(finish_order):
        if root in assigned:
            continue
        assigned.add(root)
        component: list[FunctionSymbol] = []
        stack = [root]
        while stack:
            node = stack.pop()
            component.append(node)
            for target in reverse[node]:
                if target not in assigned:
                    assigned.add(target)
                    stack.append(target)
        components.append(
            tuple(sorted(component, key=lambda item: (item.address, item.name)))
        )
    component_of = {
        member: component_id
        for component_id, members in enumerate(components)
        for member in members
    }
    return components, component_of


def compute_inclusive_closure(
    graph: Mapping[FunctionSymbol, Iterable[FunctionSymbol]],
) -> InclusiveClosure:
    """返回每个函数可达的唯一函数集合（含自身）。"""

    components, component_of = _strongly_connected_components(graph)
    outgoing: list[set[int]] = [set() for _ in components]
    for caller, targets in graph.items():
        caller_component = component_of[caller]
        for callee in targets:
            callee_component = component_of[callee]
            if caller_component != callee_component:
                outgoing[caller_component].add(callee_component)

    # Condensation graph is a DAG.  A reverse topological pass lets each
    # component reuse already-built descendant sets and counts a diamond's
    # shared child exactly once.
    indegree = [0] * len(components)
    for targets in outgoing:
        for target in targets:
            indegree[target] += 1
    queue = collections.deque(
        component_id for component_id, degree in enumerate(indegree) if degree == 0
    )
    topological: list[int] = []
    while queue:
        component_id = queue.popleft()
        topological.append(component_id)
        for target in sorted(outgoing[component_id]):
            indegree[target] -= 1
            if indegree[target] == 0:
                queue.append(target)
    require(len(topological) == len(components), "SCC condensation graph 不是 DAG")
    component_closure: list[set[int]] = [set() for _ in components]
    for component_id in reversed(topological):
        closure = component_closure[component_id]
        closure.add(component_id)
        for target in outgoing[component_id]:
            closure.update(component_closure[target])
    members = {
        member: frozenset(
            component_member
            for component_id in component_closure[component_of[member]]
            for component_member in components[component_id]
        )
        for member in component_of
    }
    recursive_count = sum(len(component) > 1 for component in components)
    recursive_count += sum(
        len(component) == 1
        and component[0] in graph
        and component[0] in set(graph.get(component[0], ()))
        for component in components
    )
    return InclusiveClosure(members, len(components), recursive_count)


def inclusive_metric(
    exclusive: Mapping[FunctionSymbol, float],
    closure: InclusiveClosure,
) -> dict[FunctionSymbol, float]:
    """按静态可达闭包对任意函数标量成本做 inclusive 求和。"""

    return {
        symbol: math.fsum(
            exclusive.get(callee, 0.0) for callee in reachable
        )
        for symbol, reachable in closure.members.items()
    }


def inclusive_descriptor_counts(
    exclusive: Mapping[FunctionSymbol, Mapping[int, float]],
    closure: InclusiveClosure,
) -> dict[FunctionSymbol, dict[int, float]]:
    """按闭包合并 descriptor 计数，供成本和区间字段共同使用。"""

    result: dict[FunctionSymbol, dict[int, float]] = {}
    for symbol, reachable in closure.members.items():
        counters: collections.Counter[int] = collections.Counter()
        for callee in reachable:
            counters.update(exclusive.get(callee, {}))
        result[symbol] = dict(counters)
    return result


def parse_key_value_manifest(content: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for line in content.splitlines():
        if not line or "=" not in line or line.startswith("symbol\t"):
            continue
        key, value = line.split("=", 1)
        result[key] = value
    return result


def parse_elm_manifest(
    content: str,
) -> tuple[dict[str, ElmApiMetadata], dict[str, str]]:
    lines = content.splitlines()
    require(lines and lines[0] == ELM_INTERFACE_SCHEMA, "非法 ELM interface manifest")
    header = parse_key_value_manifest(content)
    result: dict[str, ElmApiMetadata] = {}
    for line in lines[1:]:
        if not line.startswith("symbol\t"):
            continue
        fields = line.split("\t")
        require(len(fields) >= 12, "ELM interface manifest 的 symbol 行字段不足")
        metadata = ElmApiMetadata(
            name=fields[6],
            rust_name=fields[7],
            linker_symbol=fields[8],
            contract=fields[9],
        )
        require(
            metadata.linker_symbol.startswith(ELM_API_PREFIX),
            f"非法 ELM API 符号：{metadata.linker_symbol}",
        )
        require(
            metadata.linker_symbol not in result,
            f"重复 ELM API 符号：{metadata.linker_symbol}",
        )
        result[metadata.linker_symbol] = metadata
    require(result, "ELM interface manifest 没有 symbol 行")
    if "symbol_count" in header:
        require(int(header["symbol_count"]) == len(result), "ELM symbol_count 不闭合")
    return result, header


def validate_kernel_map_manifest(path: Path, kernel_sha256: str) -> dict[str, str]:
    content = path.read_text(encoding="utf-8")
    header = parse_key_value_manifest(content)
    require(
        header.get("schema") == "mygo.kernel-map-manifest.v1",
        "非法 kernel map manifest",
    )
    require(
        header.get("kernel_sha256") == kernel_sha256,
        "kernel map manifest 与正式 ELF 不匹配",
    )
    symbol_map = Path(str(path).removesuffix(".manifest"))
    require(symbol_map.is_file(), f"kernel map 不存在：{symbol_map}")
    require(
        header.get("symbol_map_sha256") == sha256_file(symbol_map),
        "kernel map SHA256 与 manifest 不匹配",
    )
    return header


def parse_readelf_symbols(content: str) -> list[tuple[int, int, str, str]]:
    symbols: list[tuple[int, int, str, str]] = []
    for line in content.splitlines():
        match = READELF_SYMBOL_LINE.match(line)
        if match is None:
            continue
        address = int(match.group(1), 16)
        size = int(match.group(2))
        binding = match.group(3)
        section = match.group(4)
        name = match.group(5)
        if size <= 0 or section == "UND":
            continue
        symbols.append((address, size, binding, name))
    return symbols


def parse_readelf_defined_symbol_names(content: str) -> set[str]:
    names: set[str] = set()
    for line in content.splitlines():
        fields = line.split(None, 7)
        if len(fields) != 8 or not fields[0].endswith(":"):
            continue
        if fields[6] == "UND":
            continue
        names.add(fields[7])
    return names


def demangle_names(names: Sequence[str], cxxfilt: str) -> dict[str, str]:
    unique = list(dict.fromkeys(names))
    process = subprocess.run(
        [cxxfilt, "--format=rust", "--no-recurse-limit"],
        check=False,
        input="\n".join(unique) + "\n",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    require(process.returncode == 0, f"c++filt 失败：{process.stderr.strip()}")
    demangled = process.stdout.splitlines()
    require(len(demangled) == len(unique), "c++filt 输出行数不闭合")
    return dict(zip(unique, demangled, strict=True))


def load_symbols(
    kernel: Path,
    readelf: str,
    cxxfilt: str,
    elm_apis: dict[str, ElmApiMetadata] | None = None,
) -> tuple[list[FunctionSymbol], int, int, set[str]]:
    symbols_process = subprocess.run(
        [readelf, "--wide", "--syms", str(kernel)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    require(
        symbols_process.returncode == 0,
        f"readelf --syms 失败：{symbols_process.stderr.strip()}",
    )
    raw_symbols = parse_readelf_symbols(symbols_process.stdout)
    defined_elm_api_symbols = {
        name
        for name in parse_readelf_defined_symbol_names(symbols_process.stdout)
        if name.startswith(ELM_API_PREFIX)
    }
    demangled = demangle_names([row[3] for row in raw_symbols], cxxfilt)
    grouped: dict[tuple[int, int], list[tuple[str, str, str]]] = (
        collections.defaultdict(list)
    )
    for address, size, binding, name in raw_symbols:
        grouped[(address, size)].append((name, demangled[name], binding))
    symbols: list[FunctionSymbol] = []
    for (address, size), rows in grouped.items():
        aliases = tuple(
            sorted(
                {demangled for _, demangled, _ in rows},
                key=lambda name: (len(name), name),
            )
        )
        bindings = {binding for _, _, binding in rows}
        kind = (
            "GLOBAL"
            if "GLOBAL" in bindings
            else ("WEAK" if "WEAK" in bindings else "LOCAL")
        )
        api_rows = [
            elm_apis[raw]
            for raw, _, _ in rows
            if elm_apis is not None and raw in elm_apis
        ]
        api_names = tuple(sorted({row.name for row in api_rows}))
        api_rust_names = tuple(sorted({row.rust_name for row in api_rows}))
        api_contracts = tuple(sorted({row.contract for row in api_rows}))
        display_name = f"elm-api::{api_names[0]}" if api_names else aliases[0]
        symbols.append(
            FunctionSymbol(
                address,
                size,
                display_name,
                kind,
                aliases,
                api_names,
                api_rust_names,
                api_contracts,
            )
        )
    symbols.sort(key=lambda row: (row.address, row.size, row.name, row.kind))

    sections_process = subprocess.run(
        [readelf, "--wide", "--sections", str(kernel)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    require(
        sections_process.returncode == 0,
        f"readelf --sections 失败：{sections_process.stderr.strip()}",
    )
    text_match = next(
        (
            match
            for line in sections_process.stdout.splitlines()
            if (match := READELF_TEXT_LINE.search(line)) is not None
        ),
        None,
    )
    require(text_match is not None, "readelf 没有找到 .text section")
    text_start = int(text_match.group(1), 16)
    text_size = int(text_match.group(2), 16)
    require(symbols, "内核 ELF 没有可用的函数符号")
    return symbols, text_start, text_start + text_size, defined_elm_api_symbols


class SymbolResolver:
    def __init__(
        self,
        symbols: Sequence[FunctionSymbol],
        text_start: int,
        text_end: int,
        dynamic_code_symbol: FunctionSymbol = DYNAMIC_CODE_SYMBOL,
    ):
        self.symbols = list(symbols)
        self.text_start = text_start
        self.text_end = text_end
        self.dynamic_code_symbol = dynamic_code_symbol
        self.starts = [row.address for row in self.symbols]
        self.prefix_max_end: list[int] = []
        maximum = -1
        for row in self.symbols:
            maximum = max(maximum, row.end)
            self.prefix_max_end.append(maximum)
        self.cache: dict[int, FunctionSymbol] = {}

    def resolve(self, pc: int) -> FunctionSymbol:
        cached = self.cache.get(pc)
        if cached is not None:
            return cached
        index = bisect.bisect_right(self.starts, pc) - 1
        candidates: list[FunctionSymbol] = []
        while index >= 0 and self.prefix_max_end[index] > pc:
            row = self.symbols[index]
            if row.address <= pc < row.end:
                candidates.append(row)
            index -= 1
        result = (
            min(
                candidates,
                key=lambda row: (
                    row.size,
                    0 if row.kind.islower() else 1,
                    row.name,
                ),
            )
            if candidates
            else (
                UNRESOLVED_SYMBOL
                if self.text_start <= pc < self.text_end
                else self.dynamic_code_symbol
            )
        )
        self.cache[pc] = result
        return result


class CatalogMaterializer:
    def __init__(self, path: Path):
        self.path = path
        self.stream: BinaryIO | None = None

    def __enter__(self) -> CatalogMaterializer:
        self.stream = self.path.open("rb")
        return self

    def __exit__(self, *_: object) -> None:
        if self.stream is not None:
            self.stream.close()
            self.stream = None

    def instructions(self, record: TbCatalogRecord) -> list[tuple[int, int]]:
        require(self.stream is not None, "catalog materializer 尚未打开")
        self.stream.seek(record.source.offset)
        raw = self.stream.read(record.source.length)
        require(len(raw) == record.source.length, "catalog 随机读取被截断")
        value = json.loads(raw)
        require(value.get("type") == "tb", "catalog 随机读取不是 TB")
        require(int(value["translation_index"]) == record.translation_index, "catalog TB index 漂移")
        instructions = value.get("instructions")
        require(isinstance(instructions, list), "catalog TB 缺少指令数组")
        require(len(instructions) == record.instruction_count, "catalog TB 指令数不闭合")
        result: list[tuple[int, int]] = []
        for item in instructions:
            require(isinstance(item, dict), "catalog 指令不是对象")
            descriptor = item.get("descriptor_id")
            require(isinstance(descriptor, int) and descriptor >= 0, "catalog 指令缺少 descriptor")
            result.append((int(str(item["pc"]), 16), descriptor))
        return result


@dataclasses.dataclass(frozen=True)
class TbSummary:
    occurrences: tuple[tuple[int, FunctionSymbol, int], ...]
    function_weights: tuple[tuple[FunctionSymbol, float], ...]
    allocation_cost_ns: float
    instruction_count: int
    symbolized_instruction_count: int
    imputed_instruction_count: int


def summarize_tb(
    record: TbCatalogRecord,
    materializer: CatalogMaterializer,
    resolver: SymbolResolver,
    descriptors: Mapping[int, DescriptorCost],
) -> TbSummary:
    occurrences: collections.Counter[tuple[int, FunctionSymbol]] = collections.Counter()
    function_weights: collections.Counter[FunctionSymbol] = collections.Counter()
    allocation_cost = 0.0
    symbolized = 0
    imputed = 0
    instructions = materializer.instructions(record)
    for pc, descriptor_id in instructions:
        descriptor = descriptors.get(descriptor_id)
        require(descriptor is not None, f"kernel TB 引用了无动态成本行的 descriptor {descriptor_id}")
        symbol = resolver.resolve(pc)
        weight = descriptor.allocation_weight_ns
        occurrences[(descriptor_id, symbol)] += 1
        function_weights[symbol] += weight
        allocation_cost += weight
        symbolized += int(symbol.address >= 0)
        imputed += int(descriptor.allocation_weight_imputed)
    require(allocation_cost > 0.0, "TB 分配成本不是正数")
    return TbSummary(
        tuple((descriptor, symbol, count) for (descriptor, symbol), count in occurrences.items()),
        tuple(function_weights.items()),
        allocation_cost,
        len(instructions),
        symbolized,
        imputed,
    )


def allocate_exact_counts(
    exposures: Mapping[int, Mapping[FunctionSymbol, float]],
    descriptors: Mapping[int, DescriptorCost],
) -> dict[tuple[FunctionSymbol, int], float]:
    result: dict[tuple[FunctionSymbol, int], float] = {}
    for descriptor_id, descriptor in descriptors.items():
        candidates = {
            symbol: float(value)
            for symbol, value in exposures.get(descriptor_id, {}).items()
            if value > 0.0 and math.isfinite(value)
        }
        if not candidates:
            result[(UNSAMPLED_SYMBOL, descriptor_id)] = float(
                descriptor.exact_kernel_count
            )
            continue
        total = math.fsum(candidates.values())
        allocated = {
            symbol: descriptor.exact_kernel_count * value / total
            for symbol, value in candidates.items()
        }
        largest = max(candidates, key=candidates.get)
        allocated[largest] += descriptor.exact_kernel_count - math.fsum(allocated.values())
        for symbol, count in allocated.items():
            result[(symbol, descriptor_id)] = count
    return result


def share_bounds(
    function_counts: Mapping[int, float],
    descriptors: Mapping[int, DescriptorCost],
) -> tuple[float | None, float | None]:
    bounded = [row for row in descriptors.values() if row.bounded]
    if not bounded:
        return None, None
    total_low = math.fsum(row.exact_kernel_count * float(row.low_ns) for row in bounded)
    total_high = math.fsum(row.exact_kernel_count * float(row.high_ns) for row in bounded)
    if total_high <= 0.0:
        return None, None

    def difference(ratio: float, *, maximize: bool) -> float:
        terms: list[float] = []
        for row in bounded:
            coefficient = function_counts.get(row.descriptor_id, 0.0) - (
                ratio * row.exact_kernel_count
            )
            low = float(row.low_ns)
            high = float(row.high_ns)
            if maximize:
                weight = high if coefficient >= 0.0 else low
            else:
                weight = low if coefficient >= 0.0 else high
            terms.append(coefficient * weight)
        return math.fsum(terms)

    def root(*, maximize: bool) -> float:
        left = 0.0
        right = 1.0
        while difference(right, maximize=maximize) >= 0.0:
            left = right
            right *= 2.0
            require(right <= 2.0**40, "成本占比求根没有收敛上界")
        for _ in range(80):
            middle = (left + right) / 2.0
            if difference(middle, maximize=maximize) >= 0.0:
                left = middle
            else:
                right = middle
        return (left + right) / 2.0

    return root(maximize=False), root(maximize=True)


def symbol_fields(symbol: FunctionSymbol) -> dict[str, Any]:
    aliases = symbol.aliases or (symbol.name,)
    return {
        "function": symbol.name,
        "function_aliases": ";".join(aliases),
        "function_alias_count": len(aliases),
        "elm_api_names": ";".join(symbol.elm_api_names),
        "elm_api_rust_names": ";".join(symbol.elm_api_rust_names),
        "elm_api_contracts": ";".join(symbol.elm_api_contracts),
        "symbol_address": f"0x{symbol.address:016x}" if symbol.address >= 0 else "",
        "symbol_size": symbol.size if symbol.address >= 0 else "",
        "symbol_kind": symbol.kind,
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--kernel-map-manifest", type=Path)
    parser.add_argument("--elm-manifest", type=Path)
    parser.add_argument("--instruction-costs", type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument(
        "--dynamic-code-label", default="[dynamic-kernel-code-outside-ELF]"
    )
    parser.add_argument("--readelf", default="readelf")
    parser.add_argument("--cxxfilt", default="c++filt")
    parser.add_argument("--objdump")
    arguments = parser.parse_args(argv)
    objdump = select_objdump(arguments.objdump)

    run_dir = arguments.run_dir.resolve()
    kernel = (arguments.kernel or run_dir / "kernel-rv").resolve()
    instruction_costs = (
        arguments.instruction_costs
        or run_dir / "microbench-costs" / "instruction-costs.csv"
    ).resolve()
    output_dir = (arguments.output_dir or run_dir / "kernel-function-costs").resolve()
    require(kernel.is_file(), f"内核 ELF 不存在：{kernel}")
    require(instruction_costs.is_file(), f"逐指令成本表不存在：{instruction_costs}")
    kernel_sha256 = sha256_file(kernel)
    run_summary = json.loads((run_dir / "summary.json").read_text(encoding="utf-8"))
    expected_kernel_sha256 = str(run_summary["metadata"]["kernel_sha256"])
    require(
        kernel_sha256 == expected_kernel_sha256,
        "内核 ELF SHA256 与正式采集 metadata 不一致",
    )
    kernel_map_metadata: dict[str, str] | None = None
    if arguments.kernel_map_manifest is not None:
        kernel_map_manifest = arguments.kernel_map_manifest.resolve()
        require(
            kernel_map_manifest.is_file(),
            f"kernel map manifest 不存在：{kernel_map_manifest}",
        )
        kernel_map_metadata = validate_kernel_map_manifest(
            kernel_map_manifest, kernel_sha256
        )

    elm_apis: dict[str, ElmApiMetadata] | None = None
    elm_manifest_header: dict[str, str] | None = None
    elm_manifest: Path | None = None
    if arguments.elm_manifest is not None:
        elm_manifest = arguments.elm_manifest.resolve()
        require(elm_manifest.is_file(), f"ELM manifest 不存在：{elm_manifest}")
        elm_apis, elm_manifest_header = parse_elm_manifest(
            elm_manifest.read_text(encoding="utf-8")
        )

    microbench_summary = json.loads(
        (instruction_costs.parent / "summary.json").read_text(encoding="utf-8")
    )
    window_start = int(microbench_summary["configuration"]["window_start_monotonic_ns"])
    window_stop = int(microbench_summary["configuration"]["window_stop_monotonic_ns"])
    descriptors, descriptor_quality = load_kernel_descriptor_costs(instruction_costs)
    profile_quality = json.loads(
        (run_dir / "riscv-instruction-profile-quality.json").read_text(encoding="utf-8")
    )
    expected_kernel_count = int(profile_quality["instruction_mix"]["instructions"]["kernel"])
    require(
        descriptor_quality["exact_kernel_instruction_count"] == expected_kernel_count,
        "逐指令成本表与 instruction mix 的内核总数不闭合",
    )

    print("kernel-functions: 读取并索引精确 ELF 函数符号", file=os.sys.stderr)
    symbols, text_start, text_end, elf_elm_api_symbols = load_symbols(
        kernel, arguments.readelf, arguments.cxxfilt, elm_apis
    )
    elf_elm_api_function_symbols = {
        alias
        for symbol in symbols
        for alias in symbol.aliases
        if alias.startswith(ELM_API_PREFIX)
    }
    if elm_apis is not None:
        require(
            elf_elm_api_symbols == set(elm_apis),
            "ELM manifest 与精确 ELF 的 API 符号集合不一致",
        )
    dynamic_code_symbol = FunctionSymbol(-3, 0, arguments.dynamic_code_label, "?")
    resolver = SymbolResolver(symbols, text_start, text_end, dynamic_code_symbol)
    print("kernel-functions: 解析 ELF 静态 direct-call 图", file=os.sys.stderr)
    call_graph_resolver = SymbolResolver(
        symbols, text_start, text_end, dynamic_code_symbol
    )
    call_graph = load_direct_call_graph(
        kernel, objdump, symbols, call_graph_resolver
    )
    inclusive_closure = compute_inclusive_closure(call_graph.edges)
    namespace = read_tid_namespace_tsv(run_dir / "tid-namespace-map.tsv")
    vcpu_host_tids = {
        entry.host_tid for entry in namespace.entries if VCPU_COMM.fullmatch(entry.comm)
    }
    require(vcpu_host_tids, "TID namespace 没有 vCPU 线程")

    samples = sorted_perf_samples(run_dir / "tcg-time-samples.bin")
    match = MatchStatistics()
    records = iter_matched_jit_records(
        run_dir / "instruction-catalog.jsonl",
        run_dir / "jit-7.dump",
        stats=match,
        include_instructions=False,
    )
    mapper = TimeAwareJitMap(records)
    primary_exposure: dict[int, collections.Counter[FunctionSymbol]] = (
        collections.defaultdict(collections.Counter)
    )
    time_exposure: dict[int, collections.Counter[FunctionSymbol]] = (
        collections.defaultdict(collections.Counter)
    )
    sampled_function_clock: collections.Counter[FunctionSymbol] = collections.Counter()
    tb_cache: dict[int, TbSummary] = {}
    mapped_kernel_samples = 0
    mapped_kernel_clock = 0
    symbolized_kernel_clock = 0.0
    sampled_tb_instructions = 0
    symbolized_tb_instructions = 0
    imputed_tb_instructions = 0
    window_vcpu_samples = 0
    window_vcpu_clock = 0

    print("kernel-functions: 映射 task-clock 样本到内核 TB", file=os.sys.stderr)
    with CatalogMaterializer(run_dir / "instruction-catalog.jsonl") as materializer:
        for sample_number, mapped in enumerate(
            mapper.map_sorted_samples(samples, tid_namespace=namespace), 1
        ):
            if sample_number % 100_000 == 0:
                print(
                    f"kernel-functions: 已映射 {sample_number:,} 个样本",
                    file=os.sys.stderr,
                )
            sample = mapped.sample
            if sample.tid not in vcpu_host_tids:
                continue
            if not window_start <= sample.time_ns < window_stop:
                continue
            window_vcpu_samples += 1
            window_vcpu_clock += sample.period_ns
            if (
                mapped.location is not SampleLocation.MAPPED_TCG
                or mapped.catalog is None
                or mapped.catalog.mode != "kernel"
            ):
                continue
            mapped_kernel_samples += 1
            mapped_kernel_clock += sample.period_ns
            translation_index = mapped.catalog.translation_index
            tb = tb_cache.get(translation_index)
            if tb is None:
                tb = summarize_tb(mapped.catalog, materializer, resolver, descriptors)
                tb_cache[translation_index] = tb
                sampled_tb_instructions += tb.instruction_count
                symbolized_tb_instructions += tb.symbolized_instruction_count
                imputed_tb_instructions += tb.imputed_instruction_count
            execution_mass = sample.period_ns / tb.allocation_cost_ns
            for descriptor_id, symbol, count in tb.occurrences:
                primary_exposure[descriptor_id][symbol] += execution_mass * count
                time_exposure[descriptor_id][symbol] += sample.period_ns * count
            for symbol, weight in tb.function_weights:
                clock = sample.period_ns * weight / tb.allocation_cost_ns
                sampled_function_clock[symbol] += clock
                if symbol.address >= 0:
                    symbolized_kernel_clock += clock
    mapper.drain()

    require(match.unmatched_guest_loads == 0, "存在无法匹配 catalog 的 guest JIT load")
    require(match.guest_jit_match_ratio == 1.0, "guest JIT 到 catalog 未完全匹配")
    require(mapped_kernel_samples > 0 and mapped_kernel_clock > 0, "没有映射到内核 TB 的样本")
    require(
        math.isclose(
            math.fsum(sampled_function_clock.values()),
            mapped_kernel_clock,
            rel_tol=1e-12,
            abs_tol=1e-3,
        ),
        "按函数拆分的内核 sampled task-clock 不闭合",
    )

    print("kernel-functions: 将 descriptor 暴露度归一化到精确动态计数", file=os.sys.stderr)
    primary_counts = allocate_exact_counts(primary_exposure, descriptors)
    alternative_counts = allocate_exact_counts(time_exposure, descriptors)
    primary_total = math.fsum(primary_counts.values())
    alternative_total = math.fsum(alternative_counts.values())
    require(
        math.isclose(primary_total, expected_kernel_count, rel_tol=1e-12, abs_tol=1e-3),
        "主分配器的内核动态计数不闭合",
    )
    require(
        math.isclose(alternative_total, expected_kernel_count, rel_tol=1e-12, abs_tol=1e-3),
        "敏感性分配器的内核动态计数不闭合",
    )

    instruction_rows: list[dict[str, Any]] = []
    function_descriptor_counts: dict[FunctionSymbol, dict[int, float]] = (
        collections.defaultdict(dict)
    )
    alternative_function_descriptor_counts: dict[FunctionSymbol, dict[int, float]] = (
        collections.defaultdict(dict)
    )
    keys = sorted(
        set(primary_counts) | set(alternative_counts),
        key=lambda item: (item[0].name, item[0].address, item[1]),
    )
    for symbol, descriptor_id in keys:
        descriptor = descriptors[descriptor_id]
        count = primary_counts.get((symbol, descriptor_id), 0.0)
        alternative = alternative_counts.get((symbol, descriptor_id), 0.0)
        function_descriptor_counts[symbol][descriptor_id] = count
        alternative_function_descriptor_counts[symbol][descriptor_id] = alternative
        instruction_rows.append(
            {
                **symbol_fields(symbol),
                "descriptor_id": descriptor_id,
                "mnemonic": descriptor.mnemonic,
                "size_bytes": descriptor.size_bytes,
                "exact_kernel_descriptor_count": descriptor.exact_kernel_count,
                "estimated_instruction_count": count,
                "descriptor_allocation_share": (
                    count / descriptor.exact_kernel_count
                    if descriptor.exact_kernel_count
                    else 0.0
                ),
                "alternative_estimated_instruction_count": alternative,
                "alternative_descriptor_allocation_share": (
                    alternative / descriptor.exact_kernel_count
                    if descriptor.exact_kernel_count
                    else 0.0
                ),
                "assignment": descriptor.assignment,
                "quality": descriptor.quality,
                "bounded": descriptor.bounded,
                "strict": descriptor.strict,
                "identified_weight_ns": descriptor.point_ns,
                "weight_envelope_low_ns": descriptor.low_ns,
                "weight_envelope_high_ns": descriptor.high_ns,
                "diagnostic_context_center_ns": descriptor.center_ns,
                "allocation_weight_ns": descriptor.allocation_weight_ns,
                "allocation_weight_imputed": descriptor.allocation_weight_imputed,
                "identified_cost_ns": (
                    count * descriptor.point_ns
                    if descriptor.point_ns is not None
                    else None
                ),
                "bounded_cost_low_ns": (
                    count * descriptor.low_ns if descriptor.bounded else None
                ),
                "bounded_cost_high_ns": (
                    count * descriptor.high_ns if descriptor.bounded else None
                ),
                "diagnostic_context_center_cost_ns": (
                    count * descriptor.center_ns if descriptor.bounded else None
                ),
                "alternative_diagnostic_center_cost_ns": (
                    alternative * descriptor.center_ns if descriptor.bounded else None
                ),
            }
        )

    # Inclusive 归因只沿静态 direct-call 闭包向下累加；特殊的未采样/未解析
    # 桶不在 ELF 图中，因此保留其自身的 exclusive 计数，不虚构调用边。
    inclusive_primary_counts = inclusive_descriptor_counts(
        function_descriptor_counts, inclusive_closure
    )
    inclusive_alternative_counts = inclusive_descriptor_counts(
        alternative_function_descriptor_counts, inclusive_closure
    )
    for symbol, counts in function_descriptor_counts.items():
        inclusive_primary_counts.setdefault(symbol, dict(counts))
    for symbol, counts in alternative_function_descriptor_counts.items():
        inclusive_alternative_counts.setdefault(symbol, dict(counts))

    total_bounded_low = math.fsum(
        row.exact_kernel_count * float(row.low_ns)
        for row in descriptors.values()
        if row.bounded
    )
    total_bounded_high = math.fsum(
        row.exact_kernel_count * float(row.high_ns)
        for row in descriptors.values()
        if row.bounded
    )
    total_center = math.fsum(
        row.exact_kernel_count * float(row.center_ns)
        for row in descriptors.values()
        if row.bounded
    )
    total_bounded_count = sum(
        row.exact_kernel_count for row in descriptors.values() if row.bounded
    )
    total_strict_count = sum(
        row.exact_kernel_count for row in descriptors.values() if row.strict
    )

    function_rows: list[dict[str, Any]] = []
    function_row_by_symbol: dict[FunctionSymbol, dict[str, Any]] = {}
    all_symbols = sorted(
        set(function_descriptor_counts)
        | set(alternative_function_descriptor_counts)
        | set(sampled_function_clock),
        key=lambda row: (row.name, row.address),
    )
    for symbol in all_symbols:
        counts = function_descriptor_counts.get(symbol, {})
        alternative_counts_by_descriptor = alternative_function_descriptor_counts.get(
            symbol, {}
        )
        instruction_count = math.fsum(counts.values())
        alternative_instruction_count = math.fsum(
            alternative_counts_by_descriptor.values()
        )
        bounded_count = math.fsum(
            count
            for descriptor_id, count in counts.items()
            if descriptors[descriptor_id].bounded
        )
        low_cost = math.fsum(
            count * float(descriptors[descriptor_id].low_ns)
            for descriptor_id, count in counts.items()
            if descriptors[descriptor_id].bounded
        )
        high_cost = math.fsum(
            count * float(descriptors[descriptor_id].high_ns)
            for descriptor_id, count in counts.items()
            if descriptors[descriptor_id].bounded
        )
        center_cost = math.fsum(
            count * float(descriptors[descriptor_id].center_ns)
            for descriptor_id, count in counts.items()
            if descriptors[descriptor_id].bounded
        )
        alternative_center = math.fsum(
            count * float(descriptors[descriptor_id].center_ns)
            for descriptor_id, count in alternative_counts_by_descriptor.items()
            if descriptors[descriptor_id].bounded
        )
        strict_cost = math.fsum(
            count * float(descriptors[descriptor_id].point_ns)
            for descriptor_id, count in counts.items()
            if descriptors[descriptor_id].strict
        )
        inclusive_counts = inclusive_primary_counts.get(symbol, counts)
        inclusive_alternative_by_descriptor = inclusive_alternative_counts.get(
            symbol, alternative_counts_by_descriptor
        )
        inclusive_instruction_count = math.fsum(inclusive_counts.values())
        inclusive_alternative_instruction_count = math.fsum(
            inclusive_alternative_by_descriptor.values()
        )
        inclusive_bounded_count = math.fsum(
            count
            for descriptor_id, count in inclusive_counts.items()
            if descriptors[descriptor_id].bounded
        )
        inclusive_low_cost = math.fsum(
            count * float(descriptors[descriptor_id].low_ns)
            for descriptor_id, count in inclusive_counts.items()
            if descriptors[descriptor_id].bounded
        )
        inclusive_high_cost = math.fsum(
            count * float(descriptors[descriptor_id].high_ns)
            for descriptor_id, count in inclusive_counts.items()
            if descriptors[descriptor_id].bounded
        )
        inclusive_center_cost = math.fsum(
            count * float(descriptors[descriptor_id].center_ns)
            for descriptor_id, count in inclusive_counts.items()
            if descriptors[descriptor_id].bounded
        )
        inclusive_alternative_center = math.fsum(
            count * float(descriptors[descriptor_id].center_ns)
            for descriptor_id, count in inclusive_alternative_by_descriptor.items()
            if descriptors[descriptor_id].bounded
        )
        inclusive_strict_cost = math.fsum(
            count * float(descriptors[descriptor_id].point_ns)
            for descriptor_id, count in inclusive_counts.items()
            if descriptors[descriptor_id].strict
        )
        share_low, share_high = share_bounds(counts, descriptors)
        inclusive_share_low, inclusive_share_high = share_bounds(
            inclusive_counts, descriptors
        )
        center_share = center_cost / total_center if total_center else 0.0
        alternative_center_share = (
            alternative_center / total_center if total_center else 0.0
        )
        sampled_clock = sampled_function_clock.get(symbol, 0.0)
        row = {
            **symbol_fields(symbol),
            "estimated_instruction_count": instruction_count,
            "instruction_share": instruction_count / expected_kernel_count,
            "alternative_estimated_instruction_count": alternative_instruction_count,
            "bounded_instruction_count": bounded_count,
            "unpriced_instruction_count": instruction_count - bounded_count,
            "bounded_cost_low_ns": low_cost,
            "bounded_cost_high_ns": high_cost,
            "diagnostic_context_center_cost_ns": center_cost,
            "diagnostic_context_center_cost_share": center_share,
            "conditional_model_share_low": share_low,
            "conditional_model_share_high": share_high,
            "alternative_diagnostic_center_cost_ns": alternative_center,
            "alternative_diagnostic_center_cost_share": alternative_center_share,
            "allocation_center_share_absolute_delta": abs(
                center_share - alternative_center_share
            ),
            "strict_cost_ns": strict_cost,
            "sampled_kernel_tcg_task_clock_ns": sampled_clock,
            "sampled_kernel_tcg_task_clock_share": sampled_clock
            / mapped_kernel_clock,
            "descriptor_count": len(counts),
            "inclusive_estimated_instruction_count": inclusive_instruction_count,
            "inclusive_instruction_share": inclusive_instruction_count
            / expected_kernel_count,
            "inclusive_alternative_estimated_instruction_count": inclusive_alternative_instruction_count,
            "inclusive_bounded_instruction_count": inclusive_bounded_count,
            "inclusive_unpriced_instruction_count": inclusive_instruction_count
            - inclusive_bounded_count,
            "inclusive_bounded_cost_low_ns": inclusive_low_cost,
            "inclusive_bounded_cost_high_ns": inclusive_high_cost,
            "inclusive_diagnostic_context_center_cost_ns": inclusive_center_cost,
            "inclusive_diagnostic_context_center_cost_share": (
                inclusive_center_cost / total_center if total_center else 0.0
            ),
            "inclusive_conditional_model_share_low": inclusive_share_low,
            "inclusive_conditional_model_share_high": inclusive_share_high,
            "inclusive_alternative_diagnostic_center_cost_ns": inclusive_alternative_center,
            "inclusive_alternative_diagnostic_center_cost_share": (
                inclusive_alternative_center / total_center if total_center else 0.0
            ),
            "inclusive_strict_cost_ns": inclusive_strict_cost,
            "inclusive_static_callee_count": max(
                0, len(inclusive_closure.members.get(symbol, frozenset({symbol}))) - 1
            ),
        }
        function_rows.append(row)
        function_row_by_symbol[symbol] = row
    function_rows.sort(
        key=lambda row: (
            row["diagnostic_context_center_cost_ns"],
            row["sampled_kernel_tcg_task_clock_ns"],
        ),
        reverse=True,
    )
    inclusive_function_rows = sorted(
        function_rows,
        key=lambda row: (
            row["inclusive_diagnostic_context_center_cost_ns"],
            row["diagnostic_context_center_cost_ns"],
        ),
        reverse=True,
    )
    instruction_rows.sort(
        key=lambda row: (
            row["diagnostic_context_center_cost_ns"] is not None,
            row["diagnostic_context_center_cost_ns"] or 0.0,
            row["estimated_instruction_count"],
        ),
        reverse=True,
    )

    logical_symbols: dict[tuple[str, tuple[str, ...]], list[FunctionSymbol]] = (
        collections.defaultdict(list)
    )
    logical_counts: dict[tuple[str, tuple[str, ...]], collections.Counter[int]] = (
        collections.defaultdict(collections.Counter)
    )
    for symbol, counts in function_descriptor_counts.items():
        aliases = symbol.aliases or (symbol.name,)
        key = (symbol.name, aliases)
        logical_symbols[key].append(symbol)
        logical_counts[key].update(counts)
    for symbol in sampled_function_clock:
        aliases = symbol.aliases or (symbol.name,)
        logical_symbols[(symbol.name, aliases)].append(symbol)
    logical_rows: list[dict[str, Any]] = []
    for key, raw_symbols in logical_symbols.items():
        name, aliases = key
        symbols_for_function = sorted(set(raw_symbols))
        source_rows = [function_row_by_symbol[symbol] for symbol in symbols_for_function]
        counts = logical_counts.get(key, {})
        share_low, share_high = share_bounds(counts, descriptors)
        center_cost = math.fsum(
            row["diagnostic_context_center_cost_ns"] for row in source_rows
        )
        alternative_center = math.fsum(
            row["alternative_diagnostic_center_cost_ns"] for row in source_rows
        )
        center_share = center_cost / total_center if total_center else 0.0
        alternative_share = alternative_center / total_center if total_center else 0.0
        sampled_clock = math.fsum(
            row["sampled_kernel_tcg_task_clock_ns"] for row in source_rows
        )
        addresses = [
            f"0x{symbol.address:016x}"
            for symbol in symbols_for_function
            if symbol.address >= 0
        ]
        logical_rows.append(
            {
                "function": name,
                "function_aliases": ";".join(aliases),
                "function_alias_count": len(aliases),
                "elm_api_names": ";".join(
                    sorted(
                        {
                            api_name
                            for symbol in symbols_for_function
                            for api_name in symbol.elm_api_names
                        }
                    )
                ),
                "elm_api_rust_names": ";".join(
                    sorted(
                        {
                            rust_name
                            for symbol in symbols_for_function
                            for rust_name in symbol.elm_api_rust_names
                        }
                    )
                ),
                "elm_api_contracts": ";".join(
                    sorted(
                        {
                            contract
                            for symbol in symbols_for_function
                            for contract in symbol.elm_api_contracts
                        }
                    )
                ),
                "symbol_addresses": ";".join(addresses),
                "symbol_copy_count": len(addresses) if addresses else 1,
                "total_symbol_size": sum(
                    symbol.size for symbol in symbols_for_function if symbol.address >= 0
                ),
                "symbol_kinds": ";".join(
                    sorted({symbol.kind for symbol in symbols_for_function})
                ),
                "estimated_instruction_count": math.fsum(
                    row["estimated_instruction_count"] for row in source_rows
                ),
                "instruction_share": math.fsum(
                    row["estimated_instruction_count"] for row in source_rows
                )
                / expected_kernel_count,
                "alternative_estimated_instruction_count": math.fsum(
                    row["alternative_estimated_instruction_count"]
                    for row in source_rows
                ),
                "bounded_instruction_count": math.fsum(
                    row["bounded_instruction_count"] for row in source_rows
                ),
                "unpriced_instruction_count": math.fsum(
                    row["unpriced_instruction_count"] for row in source_rows
                ),
                "bounded_cost_low_ns": math.fsum(
                    row["bounded_cost_low_ns"] for row in source_rows
                ),
                "bounded_cost_high_ns": math.fsum(
                    row["bounded_cost_high_ns"] for row in source_rows
                ),
                "diagnostic_context_center_cost_ns": center_cost,
                "diagnostic_context_center_cost_share": center_share,
                "conditional_model_share_low": share_low,
                "conditional_model_share_high": share_high,
                "alternative_diagnostic_center_cost_ns": alternative_center,
                "alternative_diagnostic_center_cost_share": alternative_share,
                "allocation_center_share_absolute_delta": abs(
                    center_share - alternative_share
                ),
                "strict_cost_ns": math.fsum(
                    row["strict_cost_ns"] for row in source_rows
                ),
                "sampled_kernel_tcg_task_clock_ns": sampled_clock,
                "sampled_kernel_tcg_task_clock_share": sampled_clock
                / mapped_kernel_clock,
                "descriptor_count": len(counts),
                "inclusive_estimated_instruction_count": math.fsum(
                    row["inclusive_estimated_instruction_count"]
                    for row in source_rows
                ),
                "inclusive_instruction_share": math.fsum(
                    row["inclusive_estimated_instruction_count"]
                    for row in source_rows
                )
                / expected_kernel_count,
                "inclusive_alternative_estimated_instruction_count": math.fsum(
                    row["inclusive_alternative_estimated_instruction_count"]
                    for row in source_rows
                ),
                "inclusive_bounded_instruction_count": math.fsum(
                    row["inclusive_bounded_instruction_count"]
                    for row in source_rows
                ),
                "inclusive_unpriced_instruction_count": math.fsum(
                    row["inclusive_unpriced_instruction_count"]
                    for row in source_rows
                ),
                "inclusive_bounded_cost_low_ns": math.fsum(
                    row["inclusive_bounded_cost_low_ns"] for row in source_rows
                ),
                "inclusive_bounded_cost_high_ns": math.fsum(
                    row["inclusive_bounded_cost_high_ns"] for row in source_rows
                ),
                "inclusive_diagnostic_context_center_cost_ns": math.fsum(
                    row["inclusive_diagnostic_context_center_cost_ns"]
                    for row in source_rows
                ),
                "inclusive_diagnostic_context_center_cost_share": math.fsum(
                    row["inclusive_diagnostic_context_center_cost_ns"]
                    for row in source_rows
                )
                / total_center,
                "inclusive_conditional_model_share_low": math.fsum(
                    row["inclusive_conditional_model_share_low"] or 0.0
                    for row in source_rows
                ),
                "inclusive_conditional_model_share_high": math.fsum(
                    row["inclusive_conditional_model_share_high"] or 0.0
                    for row in source_rows
                ),
                "inclusive_alternative_diagnostic_center_cost_ns": math.fsum(
                    row["inclusive_alternative_diagnostic_center_cost_ns"]
                    for row in source_rows
                ),
                "inclusive_alternative_diagnostic_center_cost_share": math.fsum(
                    row["inclusive_alternative_diagnostic_center_cost_ns"]
                    for row in source_rows
                )
                / total_center,
                "inclusive_strict_cost_ns": math.fsum(
                    row["inclusive_strict_cost_ns"] for row in source_rows
                ),
                "inclusive_static_callee_count": sum(
                    row["inclusive_static_callee_count"] for row in source_rows
                ),
            }
        )
    logical_rows.sort(
        key=lambda row: (
            row["diagnostic_context_center_cost_ns"],
            row["sampled_kernel_tcg_task_clock_ns"],
        ),
        reverse=True,
    )

    logical_instruction_groups: dict[
        tuple[str, str, int], list[dict[str, Any]]
    ] = collections.defaultdict(list)
    for row in instruction_rows:
        logical_instruction_groups[
            (row["function"], row["function_aliases"], row["descriptor_id"])
        ].append(row)
    logical_instruction_rows: list[dict[str, Any]] = []
    for rows in logical_instruction_groups.values():
        first = rows[0]
        count = math.fsum(row["estimated_instruction_count"] for row in rows)
        alternative = math.fsum(
            row["alternative_estimated_instruction_count"] for row in rows
        )
        exact_descriptor_count = first["exact_kernel_descriptor_count"]
        addresses = sorted(
            {row["symbol_address"] for row in rows if row["symbol_address"]}
        )
        logical_instruction_rows.append(
            {
                "function": first["function"],
                "function_aliases": first["function_aliases"],
                "function_alias_count": first["function_alias_count"],
                "elm_api_names": first["elm_api_names"],
                "elm_api_rust_names": first["elm_api_rust_names"],
                "elm_api_contracts": first["elm_api_contracts"],
                "symbol_addresses": ";".join(addresses),
                "symbol_copy_count": len(addresses) if addresses else 1,
                "descriptor_id": first["descriptor_id"],
                "mnemonic": first["mnemonic"],
                "size_bytes": first["size_bytes"],
                "exact_kernel_descriptor_count": exact_descriptor_count,
                "estimated_instruction_count": count,
                "descriptor_allocation_share": count / exact_descriptor_count,
                "alternative_estimated_instruction_count": alternative,
                "alternative_descriptor_allocation_share": alternative
                / exact_descriptor_count,
                "assignment": first["assignment"],
                "quality": first["quality"],
                "bounded": first["bounded"],
                "strict": first["strict"],
                "identified_weight_ns": first["identified_weight_ns"],
                "weight_envelope_low_ns": first["weight_envelope_low_ns"],
                "weight_envelope_high_ns": first["weight_envelope_high_ns"],
                "diagnostic_context_center_ns": first[
                    "diagnostic_context_center_ns"
                ],
                "allocation_weight_ns": first["allocation_weight_ns"],
                "allocation_weight_imputed": first[
                    "allocation_weight_imputed"
                ],
                "identified_cost_ns": (
                    math.fsum(row["identified_cost_ns"] for row in rows)
                    if first["identified_cost_ns"] is not None
                    else None
                ),
                "bounded_cost_low_ns": (
                    math.fsum(row["bounded_cost_low_ns"] for row in rows)
                    if first["bounded_cost_low_ns"] is not None
                    else None
                ),
                "bounded_cost_high_ns": (
                    math.fsum(row["bounded_cost_high_ns"] for row in rows)
                    if first["bounded_cost_high_ns"] is not None
                    else None
                ),
                "diagnostic_context_center_cost_ns": (
                    math.fsum(
                        row["diagnostic_context_center_cost_ns"] for row in rows
                    )
                    if first["diagnostic_context_center_cost_ns"] is not None
                    else None
                ),
                "alternative_diagnostic_center_cost_ns": (
                    math.fsum(
                        row["alternative_diagnostic_center_cost_ns"]
                        for row in rows
                    )
                    if first["alternative_diagnostic_center_cost_ns"] is not None
                    else None
                ),
            }
        )
    logical_instruction_rows.sort(
        key=lambda row: (
            row["diagnostic_context_center_cost_ns"] is not None,
            row["diagnostic_context_center_cost_ns"] or 0.0,
            row["estimated_instruction_count"],
        ),
        reverse=True,
    )
    inclusive_logical_rows = sorted(
        logical_rows,
        key=lambda row: (
            row["inclusive_diagnostic_context_center_cost_ns"],
            row["diagnostic_context_center_cost_ns"],
        ),
        reverse=True,
    )
    call_graph_rows = [
        {
            "caller_function": caller.name,
            "caller_address": f"0x{caller.address:016x}",
            "callee_function": callee.name,
            "callee_address": f"0x{callee.address:016x}",
            "call_kind": (
                "static-direct-tail"
                if (caller, callee) in call_graph.tail_edges
                else "static-direct-call"
            ),
        }
        for caller in sorted(call_graph.edges, key=lambda row: (row.address, row.name))
        for callee in sorted(
            call_graph.edges[caller], key=lambda row: (row.address, row.name)
        )
    ]

    primary_center_sum = math.fsum(
        row["diagnostic_context_center_cost_ns"] for row in function_rows
    )
    alternative_center_sum = math.fsum(
        row["alternative_diagnostic_center_cost_ns"] for row in function_rows
    )
    require(
        math.isclose(primary_center_sum, total_center, rel_tol=1e-12, abs_tol=1e-3),
        "函数中心成本不闭合",
    )
    require(
        math.isclose(alternative_center_sum, total_center, rel_tol=1e-12, abs_tol=1e-3),
        "敏感性函数中心成本不闭合",
    )

    summary = {
        "schema": OUTPUT_SCHEMA,
        "run_dir": str(run_dir),
        "kernel": str(kernel),
        "kernel_sha256": kernel_sha256,
        "kernel_map": (
            {
                "manifest": str(arguments.kernel_map_manifest.resolve()),
                **kernel_map_metadata,
            }
            if kernel_map_metadata is not None
            else None
        ),
        "elm_interface": (
            {
                "manifest": str(elm_manifest),
                "manifest_sha256": sha256_file(elm_manifest),
                "target": elm_manifest_header.get("target"),
                "manifest_api_symbol_count": len(elm_apis),
                "elf_api_symbol_count": len(elf_elm_api_symbols),
                "elf_api_function_symbol_count": len(
                    elf_elm_api_function_symbols
                ),
                "exact_api_symbol_set_match": True,
            }
            if elm_apis is not None
            and elm_manifest is not None
            and elm_manifest_header is not None
            else None
        ),
        "instruction_costs": str(instruction_costs),
        "weights": microbench_summary["weights"],
        "method": {
            "exact_counts": "kernel descriptor dynamic counters",
            "primary_allocation": "sampled-task-clock / imputed-model-TB-cost, normalized independently per descriptor",
            "sensitivity_allocation": "raw sampled-task-clock exposure, normalized independently per descriptor",
            "sampled_clock_split": "model-weighted instruction split within each sampled TB",
            "translation_counts_used_as_execution_counts": False,
            "dynamic_pc_counts_available": False,
            "function_allocation_is_estimated": True,
            "cost_scope": "bounded microbenchmark-priced kernel instruction subset",
            "inclusive_allocation": "unique transitive closure of statically resolved direct calls after SCC condensation",
            "unprefixed_function_cost_fields_are_exclusive": True,
            "inclusive_counts_shared_descendant_once": True,
            "inclusive_global_sum_is_not_a_closure_check": True,
            "inclusive_rank_scope": "functions with nonzero exclusive exposure in this trace",
        },
        "configuration": {
            "window_start_monotonic_ns": window_start,
            "window_stop_monotonic_ns": window_stop,
            "vcpu_count": len(vcpu_host_tids),
        },
        "symbols": {
            "readelf": arguments.readelf,
            "cxxfilt": arguments.cxxfilt,
            "objdump": objdump,
            "text_start": f"0x{text_start:016x}",
            "text_end": f"0x{text_end:016x}",
            "function_symbol_count": len(symbols),
            "function_alias_count": sum(len(row.aliases) for row in symbols),
            "icf_alias_group_count": sum(len(row.aliases) > 1 for row in symbols),
            "resolved_pc_count": sum(
                symbol.address >= 0 for symbol in resolver.cache.values()
            ),
            "unresolved_static_pc_count": sum(
                symbol is UNRESOLVED_SYMBOL for symbol in resolver.cache.values()
            ),
            "dynamic_code_pc_count": sum(
                symbol is dynamic_code_symbol for symbol in resolver.cache.values()
            ),
            "dynamic_code_label": arguments.dynamic_code_label,
            "sampled_tb_instruction_symbol_ratio": symbolized_tb_instructions
            / sampled_tb_instructions,
            "sampled_clock_symbol_ratio": symbolized_kernel_clock
            / mapped_kernel_clock,
        },
        "direct_call_graph": {
            "disassembled_instruction_count": call_graph.instruction_count,
            "call_site_count": call_graph.call_site_count,
            "resolved_call_site_count": call_graph.resolved_call_site_count,
            "unresolved_or_indirect_call_site_count": call_graph.unresolved_call_site_count,
            "resolved_tail_transfer_site_count": call_graph.resolved_tail_transfer_site_count,
            "unique_edge_count": len(call_graph_rows),
            "scc_count": inclusive_closure.component_count,
            "recursive_scc_count": inclusive_closure.recursive_component_count,
            "edge_direction": "caller-to-callee",
            "scope": "jal ra,target, statically resolvable auipc+jalr/call forms, and cross-symbol direct tail transfers",
        },
        "sampling": {
            "all_perf_samples": len(samples),
            "window_vcpu_samples": window_vcpu_samples,
            "window_vcpu_task_clock_ns": window_vcpu_clock,
            "mapped_kernel_tcg_samples": mapped_kernel_samples,
            "mapped_kernel_tcg_task_clock_ns": mapped_kernel_clock,
            "sampled_translation_count": len(tb_cache),
            "sampled_tb_instruction_occurrences": sampled_tb_instructions,
            "imputed_tb_instruction_occurrences": imputed_tb_instructions,
            "imputed_tb_instruction_ratio": imputed_tb_instructions
            / sampled_tb_instructions,
        },
        "catalog_jit": {
            "catalog_records": match.catalog_records,
            "jit_loads": match.jit_loads,
            "matched_loads": match.matched_loads,
            "unmatched_guest_loads": match.unmatched_guest_loads,
            "unmatched_catalog_records": match.unmatched_catalog_records,
            "catalog_coverage_ratio": match.catalog_match_ratio,
            "guest_jit_match_ratio": match.guest_jit_match_ratio,
        },
        "model": {
            **descriptor_quality,
            "bounded_kernel_instruction_count": total_bounded_count,
            "bounded_kernel_instruction_ratio": total_bounded_count
            / expected_kernel_count,
            "strict_kernel_instruction_count": total_strict_count,
            "strict_kernel_instruction_ratio": total_strict_count
            / expected_kernel_count,
            "bounded_cost_low_ns": total_bounded_low,
            "bounded_cost_high_ns": total_bounded_high,
            "diagnostic_context_center_cost_ns": total_center,
        },
        "closure": {
            "primary_instruction_count": primary_total,
            "alternative_instruction_count": alternative_total,
            "expected_kernel_instruction_count": expected_kernel_count,
            "primary_center_cost_ns": primary_center_sum,
            "alternative_center_cost_ns": alternative_center_sum,
            "expected_center_cost_ns": total_center,
            "sampled_function_clock_ns": math.fsum(sampled_function_clock.values()),
            "expected_sampled_kernel_clock_ns": mapped_kernel_clock,
        },
        "function_count": len(logical_rows),
        "symbol_instance_count": len(function_rows),
        "function_instruction_row_count": len(logical_instruction_rows),
        "symbol_instruction_row_count": len(instruction_rows),
        "top_functions": logical_rows[:50],
        "top_inclusive_functions": inclusive_logical_rows[:50],
        "outputs": {
            "function_costs": "kernel-function-costs.csv",
            "symbol_costs": "kernel-symbol-costs.csv",
            "inclusive_function_costs": "kernel-function-inclusive-costs.csv",
            "inclusive_symbol_costs": "kernel-symbol-inclusive-costs.csv",
            "function_instructions": "kernel-function-instructions.csv",
            "symbol_instructions": "kernel-symbol-instructions.csv",
            "direct_call_graph": "kernel-direct-call-graph.csv",
        },
        "limitations": [
            "本次插件没有按 guest PC 记录动态计数，函数计数由 task-clock 样本估计",
            "微基准来自 1-vCPU TCG single-thread，目标是 8-vCPU TCG multi-thread",
            "restricted/unpriced 指令只计数，不进入 bounded 函数成本",
            "native QEMU 和未定位尾部无法回溯到 guest 函数",
            "函数占比是本次单轨迹的条件性估计，不是无条件 95% 置信区间",
            "inclusive 仅覆盖 ELF 中静态可解析的 direct-call 闭包，函数指针、动态 ELM 和其他间接调用不会被猜测",
            "没有 exclusive 暴露度的静态父函数不进入 inclusive 热点排名，避免把本次未观测入口误报为热点",
            "采集数据没有调用栈或调用边频次，inclusive 是唯一静态可达函数集合的条件性聚合，不推断每个调用点的动态次数",
            "inclusive 行之间会按调用层次重复计入同一后代，因此不能跨函数求和做全局闭合",
        ],
    }

    atomic_csv(output_dir / "kernel-function-costs.csv", list(logical_rows[0]), logical_rows)
    atomic_csv(output_dir / "kernel-symbol-costs.csv", list(function_rows[0]), function_rows)
    atomic_csv(
        output_dir / "kernel-function-inclusive-costs.csv",
        list(inclusive_logical_rows[0]),
        inclusive_logical_rows,
    )
    atomic_csv(
        output_dir / "kernel-symbol-inclusive-costs.csv",
        list(inclusive_function_rows[0]),
        inclusive_function_rows,
    )
    atomic_csv(
        output_dir / "kernel-function-instructions.csv",
        list(logical_instruction_rows[0]),
        logical_instruction_rows,
    )
    atomic_csv(
        output_dir / "kernel-symbol-instructions.csv",
        list(instruction_rows[0]),
        instruction_rows,
    )
    atomic_csv(
        output_dir / "kernel-direct-call-graph.csv",
        [
            "caller_function",
            "caller_address",
            "callee_function",
            "callee_address",
            "call_kind",
        ],
        call_graph_rows,
    )
    atomic_json(output_dir / "summary.json", summary)
    print(
        f"kernel-functions: functions={len(logical_rows):,} "
        f"kernel_instructions={expected_kernel_count:,} "
        f"sampled_kernel_tcg={mapped_kernel_clock / 1e9:.3f}s "
        f"output={output_dir}",
        file=os.sys.stderr,
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (
        FunctionCostError,
        OSError,
        ValueError,
        json.JSONDecodeError,
        csv.Error,
    ) as error:
        print(f"kernel-functions: {error}", file=os.sys.stderr)
        raise SystemExit(1)
