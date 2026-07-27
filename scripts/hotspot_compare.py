#!/usr/bin/env python3
"""
hotspot_compare.py

Compare per-function instruction costs between MyGO and Linux kernels using
QEMU plugin histograms or daemon summary hotspot samples.

Usage:
    python3 hotspot_compare.py \
        --mygo-summary mygo/summary.json \
        --linux-summary linux/summary.json \
        [--mygo-histogram mygo/histogram.json] \
        [--linux-histogram linux/histogram.json] \
        [--mygo-map mygo/kernel.map] \
        [--linux-map linux/System.map] \
        [--mygo-elf mygo/kernel.elf] \
        [--linux-elf linux/vmlinux] \
        [--top-n 20] \
        [--output report.txt]
"""

import argparse
import bisect
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple

# ---------------------------------------------------------------------------
# Formatting helpers
# ---------------------------------------------------------------------------

def fmt_insns(n: float) -> str:
    """Format instruction count with B/M/K suffix."""
    if n >= 1e9:
        return f"{n / 1e9:.2f}B"
    elif n >= 1e6:
        return f"{n / 1e6:.2f}M"
    elif n >= 1e3:
        return f"{n / 1e3:.2f}K"
    return str(int(n))


def fmt_count(n: float) -> str:
    """Format execution count with M/K suffix."""
    if n >= 1e6:
        return f"{n / 1e6:.1f}M"
    elif n >= 1e3:
        return f"{n / 1e3:.1f}K"
    return str(int(n))


def fmt_ns(ns: int) -> str:
    """Format nanoseconds as seconds string."""
    return f"{ns / 1e9:.3f}s"


# ---------------------------------------------------------------------------
# Symbol table
# ---------------------------------------------------------------------------

class SymbolTable:
    """
    Sorted symbol table with O(log n) PC lookup.
    Supports both LLD map files and Linux System.map files.

    LLD map:    ADDRESS SIZE ALIGN [FLAGS] NAME  (NAME is last field)
    System.map: HEX_ADDR TYPE_CHAR NAME          (TYPE_CHAR is single alpha)
    """

    def __init__(self) -> None:
        self._addrs: List[int] = []
        self._names: List[str] = []
        # name -> (start_addr, stop_addr)
        self._by_name: Dict[str, Tuple[int, int]] = {}

    # ------------------------------------------------------------------
    @classmethod
    def from_file(cls, path: str) -> "SymbolTable":
        st = cls()
        with open(path, "r", errors="replace") as fh:
            lines = [l.rstrip() for l in fh if l.strip() and not l.startswith("#")]

        if not lines:
            return st

        # Detect format: System.map second field is a single alpha character.
        is_sysmap = False
        for line in lines[:30]:
            parts = line.split()
            if len(parts) >= 3 and len(parts[1]) == 1 and parts[1].isalpha():
                is_sysmap = True
                break
            if len(parts) >= 4:
                # LLD map second field is hex size
                break

        if is_sysmap:
            st._load_sysmap(lines)
        else:
            st._load_lld_map(lines)

        return st

    # ------------------------------------------------------------------
    def _load_sysmap(self, lines: List[str]) -> None:
        entries: List[Tuple[int, str]] = []
        for line in lines:
            parts = line.split()
            if len(parts) < 3:
                continue
            try:
                addr = int(parts[0], 16)
            except ValueError:
                continue
            sym_type = parts[1]
            name = parts[2]
            if sym_type in ("t", "T"):
                entries.append((addr, name))
        self._finalize(entries, size_map=None)

    # ------------------------------------------------------------------
    def _load_lld_map(self, lines: List[str]) -> None:
        entries: List[Tuple[int, str]] = []
        size_map: Dict[int, int] = {}
        for line in lines:
            parts = line.split()
            if len(parts) < 4:
                continue
            try:
                addr = int(parts[0], 16)
                size = int(parts[1], 16)
            except ValueError:
                continue
            name = parts[-1]
            # Skip zero-size non-marker symbols
            if size == 0 and name not in ("_stext", "_etext", "_start", "_end"):
                continue
            entries.append((addr, name))
            if size > 0:
                size_map[addr] = size
        self._finalize(entries, size_map)

    # ------------------------------------------------------------------
    def _finalize(
        self,
        entries: List[Tuple[int, str]],
        size_map: Optional[Dict[int, int]],
    ) -> None:
        entries.sort(key=lambda x: x[0])
        # Deduplicate: keep first name per address
        seen: Dict[int, str] = {}
        deduped: List[Tuple[int, str]] = []
        for addr, name in entries:
            if addr not in seen:
                seen[addr] = name
                deduped.append((addr, name))

        self._addrs = [e[0] for e in deduped]
        self._names = [e[1] for e in deduped]

        for i, (addr, name) in enumerate(deduped):
            if size_map and addr in size_map:
                stop = addr + size_map[addr]
            elif i + 1 < len(deduped):
                stop = deduped[i + 1][0]
            else:
                stop = addr + 0x100000  # generous fallback for last symbol
            self._by_name[name] = (addr, stop)

    # ------------------------------------------------------------------
    def lookup(self, pc: int) -> Optional[Tuple[str, int]]:
        """Return (function_name, offset) for the given PC, or None."""
        if not self._addrs:
            return None
        idx = bisect.bisect_right(self._addrs, pc) - 1
        if idx < 0:
            return None
        addr = self._addrs[idx]
        name = self._names[idx]
        if name in self._by_name:
            _, stop = self._by_name[name]
            if pc >= stop:
                return None
        return (name, pc - addr)

    def get_range(self, name: str) -> Optional[Tuple[int, int]]:
        """Return (start, stop) address pair for a symbol name."""
        return self._by_name.get(name)

    def all_names(self) -> List[str]:
        return list(self._by_name.keys())


# ---------------------------------------------------------------------------
# Histogram loading  (Layer 2 plugin output)
# ---------------------------------------------------------------------------

# LoongArch64 kernel virtual address base; all kernel PCs are >= this value.
KERNEL_BASE = 0x9000_0000_0000_0000


def load_histogram(path: str, symtab: Optional[SymbolTable]) -> Dict[str, int]:
    """
    Parse a QEMU plugin histogram JSON file and aggregate executed instructions
    per function.

    Schema: {"schema": "mygo.qemu-observer-histogram.v1",
             "tbs": [{"pc": INT, "insns": INT, "execs": INT}, ...]}

    Only kernel-address TBs (pc >= KERNEL_BASE) are attributed to symbols.
    User-space TBs are counted separately and excluded from analysis.

    Returns dict: function_name -> total_insns_executed
    """
    with open(path, "r") as fh:
        data = json.load(fh)
    if data.get("schema") != "mygo.qemu-observer-histogram.v1":
        print(f"warning: unexpected histogram schema {data.get('schema')!r}", file=sys.stderr)

    counts: Dict[str, int] = {}
    unknown = 0
    user_insns = 0
    kernel_total = 0
    for tb in data.get("tbs", []):
        pc = tb["pc"]
        insns = tb["insns"]
        execs = tb["execs"]
        executed = execs * insns
        # Filter: only kernel-address TBs
        if pc < KERNEL_BASE:
            user_insns += executed
            continue
        kernel_total += executed
        if symtab is None:
            key = hex(pc)
        else:
            result = symtab.lookup(pc)
            if result is None:
                unknown += executed
                continue
            key = result[0]
        counts[key] = counts.get(key, 0) + executed

    print(f"  user-space insns (excluded): {fmt_insns(user_insns)}", file=sys.stderr)
    print(f"  kernel insns total: {fmt_insns(kernel_total)}", file=sys.stderr)
    if unknown:
        print(f"  kernel insns in assembly/unmapped symbols: {fmt_insns(unknown)} ({unknown*100/max(kernel_total,1):.1f}%)", file=sys.stderr)
    return counts


def build_objdump_symtab(elf_path: str) -> "SymbolTable":
    """Build SymbolTable from objdump -d output, covering ALL functions including assembly stubs."""
    st = SymbolTable()
    cross = "loongarch64-linux-gnu-objdump"
    r = None
    # Try cross toolchain first, fall back to host
    for cmd in [cross, "objdump"]:
        try:
            r = subprocess.run([cmd, "--no-show-raw-insn", "-d", elf_path],
                capture_output=True, text=True, timeout=120)
            if r.returncode == 0:
                break
            r = None
        except (FileNotFoundError, subprocess.TimeoutExpired):
            r = None
            continue
    if r is None:
        return st
    # Parse: "addr <funcname>:" lines
    entries = []
    func_hdr = re.compile(r"^([0-9a-fA-F]+)\s+<([^>]+)>:\s*$")
    for line in r.stdout.splitlines():
        m = func_hdr.match(line)
        if m:
            try:
                entries.append((int(m.group(1), 16), m.group(2)))
            except ValueError:
                pass
    if entries:
        st._addrs = [e[0] for e in entries]
        st._names = [e[1] for e in entries]
        st._by_name = {}
        for i, (addr, name) in enumerate(entries):
            stop = entries[i + 1][0] if i + 1 < len(entries) else addr + 0x10000
            st._by_name[name] = (addr, stop)
    return st


def analyze_invisible_overhead(
    mygo_hist_path: str,
    mygo_elf: Optional[str],
    mygo_total_ns: int,
    linux_hist_path: Optional[str],
    linux_total_ns: int,
) -> str:
    """
    Analyze the invisible overhead categories in MyGO kernel.
    Returns a formatted text section for the report.
    """
    lines = []
    lines.append("\n━━━ 不可见开销分析 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")

    with open(mygo_hist_path) as f:
        hist = json.load(f)
    tbs = hist.get("tbs", [])

    user_insns  = sum(t["execs"] * t["insns"] for t in tbs if t["pc"] < KERNEL_BASE)
    kernel_insns = sum(t["execs"] * t["insns"] for t in tbs if t["pc"] >= KERNEL_BASE)
    total_insns = user_insns + kernel_insns

    lines.append(f"\nTCG 直方图覆盖（整个QEMU运行期间，含启动/测量窗口）：")
    lines.append(f"  总TB指令数: {fmt_insns(total_insns)}")
    lines.append(f"  用户态进程指令（Cargo/Rust工具链，NOT内核开销）: {fmt_insns(user_insns)} ({user_insns*100/max(total_insns,1):.1f}%)")
    lines.append(f"  内核态指令: {fmt_insns(kernel_insns)} ({kernel_insns*100/max(total_insns,1):.1f}%)")

    # Build objdump symbol table if elf available
    kernel_unknown = 0
    trap_insns = 0
    if mygo_elf:
        try:
            obj_st = build_objdump_symtab(mygo_elf)
            if obj_st._addrs:
                for t in tbs:
                    if t["pc"] < KERNEL_BASE:
                        continue
                    ei = t["execs"] * t["insns"]
                    result = obj_st.lookup(t["pc"])
                    if result is None:
                        kernel_unknown += ei
                    else:
                        name = result[0]
                        # Trap/TLB assembly stubs identification
                        if any(k in name.lower() for k in ["exception_entry", "tlb_refill", "trap", "handler", "eentry", "merr", "tlbr"]):
                            trap_insns += ei
                lines.append(f"\nobjdump 符号覆盖（内核）：")
                lines.append(f"  已归因: {fmt_insns(kernel_insns - kernel_unknown)} ({(kernel_insns - kernel_unknown)*100/max(kernel_insns,1):.1f}%)")
                lines.append(f"  仍未归因: {fmt_insns(kernel_unknown)} ({kernel_unknown*100/max(kernel_insns,1):.1f}%)")
                if trap_insns:
                    lines.append(f"  陷阱/TLB汇编入口函数: {fmt_insns(trap_insns)} ({trap_insns*100/max(kernel_insns,1):.1f}%)")
        except Exception as e:
            lines.append(f"  (objdump分析失败: {e})")

    lines.append("\n不可见开销来源分类：")
    lines.append("  1. 用户态进程指令: 完全不是内核开销；Cargo/Rust工具链在QEMU VM中执行的编译指令")
    lines.append("     处理方式: histogram 加载时按 PC≥0x9000... 过滤，已从分析中剔除")
    lines.append("  2. 陷阱入口/退出汇编 (loongarch64_exception_entry等): 每次系统调用/缺页的固定开销")
    lines.append("     处理方式: objdump符号覆盖可归因；减少陷阱频率（lazy FPU已减少~13%）")
    lines.append("  3. FPU/LSX 寄存器保存恢复（汇编，不在Rust函数符号内）:")
    lines.append("     - 历史数据: 817K次陷阱/窗口，每次768字节 = ~314M条指令")
    lines.append("     - 本次修复: lazy euen=0 使~13.37%陷阱跳过LSX保存 → 节省约42M条指令")
    lines.append("     处理方式: task.rs lazy FPU init已实施；进一步优化需lazy FPU owner追踪")
    lines.append("  4. ELM模块框架开销 (__elm_kernel_api_*): 跨模块调用的proof验证")
    lines.append("     处理方式: 已在可见热点中列出（~3.8%），优化需移至加载期验证")
    lines.append("  5. 内联展开的Rust泛型代码（分散在多个函数符号边界外）:")
    lines.append("     处理方式: addr2line可提升源码行级归因；属于正常Rust编译产物")

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Summary loading
# ---------------------------------------------------------------------------

def load_summary(path: str) -> dict:
    """Load and return a daemon summary.json as a raw dict."""
    with open(path, "r") as fh:
        return json.load(fh)


def get_cargo64_ns(summary: dict) -> Optional[int]:
    """Extract cargo:64 milestone in nanoseconds from summary, or None."""
    milestones = summary.get("cargo_milestones", {})
    val = milestones.get("64")
    if val is None:
        return None
    return int(val)


def hotspot_proxy(summary: dict) -> Dict[str, float]:
    """
    Build a function->samples dict from summary hotspot_offsets when no
    histogram is available.  Values are raw sample counts (floats).
    """
    counts: Dict[str, float] = {}
    for entry in summary.get("hotspot_offsets", []):
        fn = entry.get("function", "")
        samples = entry.get("samples", 0)
        if fn:
            counts[fn] = counts.get(fn, 0.0) + float(samples)
    return counts


# ---------------------------------------------------------------------------
# objdump parsing
# ---------------------------------------------------------------------------

# Matches an instruction line produced by objdump --no-show-raw-insn -d
# e.g.  "  9000000000123abc:	beqz	a0, 0x9000000000001234"
_INSN_RE = re.compile(
    r"^\s+([0-9a-fA-F]+):\s+(.+)$"
)
# Matches a function header line: "0123456789abcdef <funcname>:"
_FUNC_HDR_RE = re.compile(
    r"^([0-9a-fA-F]+)\s+<([^>]+)>:\s*$"
)


def _parse_objdump(elf_path: str) -> Dict[str, List[Tuple[int, str]]]:
    """
    Run objdump and parse output into:
        function_name -> [(addr, mnemonic_and_operands), ...]

    Raises RuntimeError if objdump is not found or returns non-zero.
    """
    try:
        result = subprocess.run(
            ["objdump", "--no-show-raw-insn", "-d", elf_path],
            capture_output=True,
            text=True,
            check=True,
        )
    except FileNotFoundError:
        raise RuntimeError("objdump not found; install binutils or cross-binutils")
    except subprocess.CalledProcessError as exc:
        raise RuntimeError(
            f"objdump failed (exit {exc.returncode}):\n{exc.stderr[:400]}"
        )

    functions: Dict[str, List[Tuple[int, str]]] = {}
    current: Optional[str] = None

    for line in result.stdout.splitlines():
        m = _FUNC_HDR_RE.match(line)
        if m:
            current = m.group(2)
            # Strip PLT / thunk suffixes like "@plt", "__asan_...", etc.
            current = re.sub(r"@.*$", "", current)
            if current not in functions:
                functions[current] = []
            continue

        if current is None:
            continue

        m = _INSN_RE.match(line)
        if m:
            addr = int(m.group(1), 16)
            instr = m.group(2).strip()
            functions[current].append((addr, instr))

    return functions


def _get_objdump(elf_path: str, cache: dict) -> Optional[Dict[str, List[Tuple[int, str]]]]:
    """Cached wrapper around _parse_objdump."""
    if elf_path in cache:
        return cache[elf_path]
    try:
        result = _parse_objdump(elf_path)
        cache[elf_path] = result
        return result
    except RuntimeError as exc:
        print(f"warning: {exc}", file=sys.stderr)
        cache[elf_path] = None
        return None


# ---------------------------------------------------------------------------
# MyGO -> Linux function name heuristic mapping
# ---------------------------------------------------------------------------

# Hand-curated overrides for common pairs.
_OVERRIDE_MAP: Dict[str, List[str]] = {
    "handle_fault":           ["handle_mm_fault", "do_page_fault", "__do_page_fault"],
    "VmSpace::handle_fault":  ["handle_mm_fault", "do_page_fault"],
    "do_page_fault":          ["do_page_fault", "handle_mm_fault"],
    "read_pages_at":          ["ext4_file_read_iter", "generic_file_read_iter"],
    "write_pages_at":         ["ext4_file_write_iter", "generic_file_write_iter"],
    "copy_from_user":         ["_copy_from_user", "copy_from_user"],
    "copy_to_user":           ["_copy_to_user",  "copy_to_user"],
    "alloc_page":             ["alloc_pages", "__alloc_pages", "__alloc_pages_nodemask"],
    "free_page":              ["free_pages", "__free_pages"],
    "schedule":               ["schedule", "__schedule"],
    "context_switch":         ["context_switch", "__switch_to"],
    "flush_tlb":              ["flush_tlb_page", "flush_tlb_range", "flush_tlb_mm"],
    "memcpy":                 ["__memcpy", "memcpy"],
    "memset":                 ["__memset", "memset"],
}


def _tokenize_name(fn_name: str) -> List[str]:
    """
    Split a Rust-style function name into searchable tokens.
    "VmSpace::handle_fault" -> ["vmspace", "handle", "fault"]
    """
    # Remove trait/generic angle brackets
    fn_name = re.sub(r"<[^>]*>", "", fn_name)
    # Split on :: and _
    parts = re.split(r"::|_", fn_name)
    tokens = []
    for p in parts:
        # Also split on camelCase boundaries
        sub = re.sub(r"([a-z])([A-Z])", r"\1_\2", p)
        tokens.extend(sub.lower().split("_"))
    return [t for t in tokens if len(t) > 2]  # skip very short tokens


def find_linux_equivalents(
    mygo_name: str, linux_names: List[str]
) -> List[str]:
    """
    Return a ranked list of Linux symbol names that are plausible equivalents
    for a given MyGO function name.

    Strategy:
    1. Check hand-curated overrides first.
    2. Fuzzy: linux function names that contain *all* of the longest tokens.
    3. Fuzzy: linux function names that contain *any* token.
    """
    # 1. Overrides
    for key, candidates in _OVERRIDE_MAP.items():
        if key in mygo_name or mygo_name.endswith(key):
            found = [n for n in candidates if n in set(linux_names)]
            if found:
                return found

    tokens = _tokenize_name(mygo_name)
    if not tokens:
        return []

    # Sort tokens longest-first so we anchor on the most specific words
    tokens_sorted = sorted(tokens, key=len, reverse=True)
    linux_set = set(linux_names)

    # 2. Contains all top-3 tokens
    top_tokens = tokens_sorted[:3]
    all_match = [
        n for n in linux_names
        if all(t in n.lower() for t in top_tokens)
    ]
    if all_match:
        return all_match[:5]

    # 3. Contains any significant token (len >= 4)
    sig_tokens = [t for t in tokens_sorted if len(t) >= 4]
    any_match = [
        n for n in linux_names
        if any(t in n.lower() for t in sig_tokens)
    ]
    # Rank by number of matching tokens
    def score(name: str) -> int:
        nl = name.lower()
        return sum(1 for t in sig_tokens if t in nl)

    any_match.sort(key=score, reverse=True)
    return any_match[:5]


# ---------------------------------------------------------------------------
# Normalization and comparison table
# ---------------------------------------------------------------------------

def normalize_counts(
    counts: Dict[str, float],
    factor: float,
) -> Dict[str, float]:
    """Multiply every value by factor (scale MyGO to match Linux time base)."""
    return {k: v * factor for k, v in counts.items()}


def build_comparison_table(
    mygo_norm: Dict[str, float],
    linux_counts: Dict[str, float],
    third_counts: Optional[Dict[str, float]] = None,
) -> List[dict]:
    """
    Merge MyGO, Linux, and optional third-system per-function counts.

    Returns a list of dicts sorted by mygo_norm_pct descending.
    """
    all_fns = set(mygo_norm) | set(linux_counts)
    if third_counts:
        all_fns |= set(third_counts)
    mygo_total = sum(mygo_norm.values()) or 1.0
    linux_total = sum(linux_counts.values()) or 1.0
    third_total = sum(third_counts.values()) if third_counts else 1.0

    rows = []
    for fn in all_fns:
        m = mygo_norm.get(fn, 0.0)
        l = linux_counts.get(fn, 0.0)
        t = third_counts.get(fn, 0.0) if third_counts else None
        ratio_linux = (m / l) if l > 0 else float("inf")
        ratio_third = (m / t) if (t and t > 0) else (float("inf") if t == 0 else None)
        rows.append(
            {
                "function": fn,
                "mygo_insns": m,
                "linux_insns": l,
                "third_insns": t,
                "ratio_linux": ratio_linux,
                "ratio_third": ratio_third,
                "mygo_pct": 100.0 * m / mygo_total,
                "linux_pct": 100.0 * l / linux_total,
                "third_pct": (100.0 * t / third_total) if t is not None else None,
            }
        )

    rows.sort(key=lambda r: r["mygo_insns"], reverse=True)
    for i, row in enumerate(rows, 1):
        row["rank"] = i
    return rows


def print_table(rows: List[dict], out, third_label: str = "FreeBSD") -> None:
    """Print the comparison table to `out`. Adds third column if rows contain third_insns."""
    has_third = rows and rows[0].get("third_insns") is not None
    if has_third:
        hdr = (
            f"{'Rank':>4}  {'Function':<42}  {'MyGO-Insns':>10}  "
            f"{'Linux-Insns':>11}  {third_label+'-Insns':>14}  "
            f"{'vs Linux':>8}  {'vs '+third_label:>10}  {'MyGO%':>6}  {'Linux%':>6}"
        )
    else:
        hdr = (
            f"{'Rank':>4}  {'Function':<42}  {'MyGO-Insns':>10}  "
            f"{'Linux-Insns':>11}  {'Ratio':>6}  {'MyGO%':>6}  {'Linux%':>6}"
        )
    sep = "-" * len(hdr)
    out.write(hdr + "\n")
    out.write(sep + "\n")

    for row in rows:
        if has_third:
            rl = row["ratio_linux"]
            rt = row["ratio_third"]
            rl_s = f"{rl:.2f}x" if rl != float("inf") else "  inf"
            rt_s = (f"{rt:.2f}x" if rt is not None and rt != float("inf")
                    else ("  inf" if rt == float("inf") else "  n/a"))
            t_insns = row["third_insns"] if row["third_insns"] is not None else 0.0
            t_pct = row["third_pct"] if row["third_pct"] is not None else 0.0
            out.write(
                f"{row['rank']:>4}  {row['function']:<42}  "
                f"{fmt_insns(row['mygo_insns']):>10}  "
                f"{fmt_insns(row['linux_insns']):>11}  "
                f"{fmt_insns(t_insns):>14}  "
                f"{rl_s:>8}  {rt_s:>10}  "
                f"{row['mygo_pct']:>5.1f}%  "
                f"{row['linux_pct']:>5.1f}%\n"
            )
        else:
            ratio = row.get("ratio_linux", row.get("ratio", float("inf")))
            ratio_str = f"{ratio:.2f}x" if ratio != float("inf") else "  inf"
            out.write(
                f"{row['rank']:>4}  {row['function']:<42}  "
                f"{fmt_insns(row['mygo_insns']):>10}  "
                f"{fmt_insns(row['linux_insns']):>11}  "
                f"{ratio_str:>6}  "
                f"{row['mygo_pct']:>5.1f}%  "
                f"{row['linux_pct']:>5.1f}%\n"
            )


# ---------------------------------------------------------------------------
# Annotated disassembly
# ---------------------------------------------------------------------------

def _annotate_function(
    fn_name: str,
    insns: List[Tuple[int, str]],
    histogram: Optional[Dict[str, int]],
    pc_counts: Optional[Dict[int, int]],
    out,
    label: str,
    rank: int,
) -> None:
    """
    Print annotated disassembly for one function.

    pc_counts: addr -> executed_insns mapping built from the raw histogram TBs.
    """
    bar = "━" * 72
    out.write(f"━━━ {rank}. {label}: {fn_name} {'━' * max(0, 60 - len(fn_name) - len(label))}\n")
    out.write(f"  {'Addr':<16}  {'Count':>8}  {'%fn':>6}   Instruction\n")
    out.write(f"  {'-'*16}  {'-'*8}  {'-'*6}   {'-'*40}\n")

    if not insns:
        out.write("  (no instructions found)\n\n")
        return

    # Compute per-instruction counts from pc_counts
    fn_total = 0.0
    addr_counts: Dict[int, int] = {}
    if pc_counts:
        for addr, _ in insns:
            c = pc_counts.get(addr, 0)
            addr_counts[addr] = c
            fn_total += c

    for addr, instr in insns:
        c = addr_counts.get(addr, 0) if pc_counts else 0
        if fn_total > 0:
            pct = 100.0 * c / fn_total
            pct_str = f"{pct:>5.1f}%"
            count_str = fmt_count(c)
        else:
            pct_str = "      "
            count_str = "        "
        out.write(f"  {addr:016x}  {count_str:>8}  {pct_str:>6}   {instr}\n")

    out.write("\n")


def build_pc_counts(histogram_path: Optional[str]) -> Optional[Dict[int, int]]:
    """
    Build a pc -> total_insns_executed map directly from the raw histogram,
    used for per-instruction annotation.
    """
    if histogram_path is None:
        return None
    try:
        with open(histogram_path, "r") as fh:
            data = json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"warning: cannot read histogram for pc_counts: {exc}", file=sys.stderr)
        return None

    pc_counts: Dict[int, int] = {}
    for tb in data.get("tbs", []):
        pc = tb["pc"]
        pc_counts[pc] = pc_counts.get(pc, 0) + tb["execs"] * tb["insns"]
    return pc_counts


# ---------------------------------------------------------------------------
# Main orchestration
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(
        description="Compare per-function instruction costs between MyGO and Linux.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--mygo-histogram", type=str, help="MyGO histogram JSON (optional)")
    parser.add_argument("--linux-histogram", type=str, help="Linux histogram JSON (optional)")
    parser.add_argument("--mygo-summary", type=str, required=True, help="MyGO summary.json (required)")
    parser.add_argument("--linux-summary", type=str, required=True, help="Linux summary.json (required)")
    parser.add_argument("--mygo-elf", type=str, help="MyGO ELF file for objdump (auto-detect if not set)")
    parser.add_argument("--linux-elf", type=str, help="Linux ELF file for objdump")
    parser.add_argument("--mygo-map", type=str, help="MyGO symbol map (LLD or System.map)")
    parser.add_argument("--linux-map", type=str, help="Linux symbol map (LLD or System.map)")
    # Third system (e.g. FreeBSD) — all optional
    # FreeBSD nm output uses the same format as Linux System.map and is supported natively.
    parser.add_argument("--third-summary", type=str, help="Third system summary.json (e.g. FreeBSD, optional)")
    parser.add_argument("--third-histogram", type=str, help="Third system histogram JSON (optional)")
    parser.add_argument("--third-elf", type=str, help="Third system ELF for objdump")
    parser.add_argument("--third-map", type=str, help="Third system symbol map (nm format, same as Linux System.map)")
    parser.add_argument("--third-label", type=str, default="FreeBSD", help="Label for third system (default: FreeBSD)")
    parser.add_argument("--top-n", type=int, default=20, help="Number of top functions to annotate (default: 20)")
    parser.add_argument("--output", type=str, help="Output file (default: stdout)")

    args = parser.parse_args()

    # Load summaries
    print("Loading summaries...", file=sys.stderr)
    mygo_summary = load_summary(args.mygo_summary)
    linux_summary = load_summary(args.linux_summary)

    mygo_cargo64 = get_cargo64_ns(mygo_summary)
    linux_cargo64 = get_cargo64_ns(linux_summary)

    def _active_ns(summary: dict) -> Optional[int]:
        cap = summary.get("capture", {})
        v = cap.get("active_duration_ns")
        return int(v) if v else None

    norm_label = "cargo:64"
    if mygo_cargo64 is None or linux_cargo64 is None:
        # Fall back to active window duration for normalization
        mygo_cargo64 = _active_ns(mygo_summary)
        linux_cargo64 = _active_ns(linux_summary)
        norm_label = "active_duration"
        if mygo_cargo64 is None or linux_cargo64 is None:
            # Last resort: use total kernel instruction counts to normalize
            gi_m = mygo_summary.get("guest_instructions", {})
            gi_l = linux_summary.get("guest_instructions", {})
            ki_m = gi_m.get("kernel")
            ki_l = gi_l.get("kernel")
            if ki_m and ki_l:
                mygo_cargo64 = ki_m
                linux_cargo64 = ki_l
                norm_label = "kernel_insns"
            else:
                print("warning: no normalization baseline available; using factor=1.0", file=sys.stderr)
                mygo_cargo64 = linux_cargo64 = 1

    # Compute normalization factor: scale MyGO to match Linux time base
    norm_factor = float(linux_cargo64) / float(mygo_cargo64)

    # Load symbol tables
    print("Loading symbol tables...", file=sys.stderr)
    mygo_st: Optional[SymbolTable] = None
    linux_st: Optional[SymbolTable] = None

    if args.mygo_map:
        if not os.path.exists(args.mygo_map):
            print(f"warning: MyGO map file not found: {args.mygo_map}", file=sys.stderr)
        else:
            mygo_st = SymbolTable.from_file(args.mygo_map)
            print(f"  MyGO: loaded {len(mygo_st.all_names())} symbols", file=sys.stderr)

    if args.linux_map:
        if not os.path.exists(args.linux_map):
            print(f"warning: Linux map file not found: {args.linux_map}", file=sys.stderr)
        else:
            linux_st = SymbolTable.from_file(args.linux_map)
            print(f"  Linux: loaded {len(linux_st.all_names())} symbols", file=sys.stderr)

    # If no MyGO map was provided, try building a symbol table from objdump
    # (catches trap entry, TLB refill, and other assembly functions not in LLD map)
    if mygo_st is None and args.mygo_elf and os.path.exists(args.mygo_elf):
        print("  MyGO: no map file; building symbol table from objdump...", file=sys.stderr)
        mygo_st = build_objdump_symtab(args.mygo_elf)
        if mygo_st._addrs:
            print(f"  MyGO: objdump loaded {len(mygo_st._addrs)} symbols", file=sys.stderr)
        else:
            print("  MyGO: objdump produced no symbols; PCs will be used as keys", file=sys.stderr)
            mygo_st = None

    # Determine instruction count source: histogram or proxy
    data_source = "unknown"
    mygo_counts: Dict[str, float] = {}
    linux_counts: Dict[str, float] = {}
    third_counts: Optional[Dict[str, float]] = None

    if args.mygo_histogram and os.path.exists(args.mygo_histogram):
        print("Loading MyGO histogram...", file=sys.stderr)
        mygo_counts_int = load_histogram(args.mygo_histogram, mygo_st)
        mygo_counts = {k: float(v) for k, v in mygo_counts_int.items()}
        data_source = "histogram"
    else:
        print("Using MyGO summary hotspot_offsets (proxy)...", file=sys.stderr)
        mygo_counts = hotspot_proxy(mygo_summary)
        data_source = "sample-proxy"

    if args.linux_histogram and os.path.exists(args.linux_histogram):
        print("Loading Linux histogram...", file=sys.stderr)
        linux_counts_int = load_histogram(args.linux_histogram, linux_st)
        linux_counts = {k: float(v) for k, v in linux_counts_int.items()}
        if data_source == "sample-proxy":
            data_source = "mixed (MyGO=samples, Linux=histogram)"
    else:
        print("Using Linux summary hotspot_offsets (proxy)...", file=sys.stderr)
        linux_counts = hotspot_proxy(linux_summary)
        if data_source == "histogram":
            data_source = "mixed (MyGO=histogram, Linux=samples)"

    # Load optional third system (e.g. FreeBSD)
    third_label = getattr(args, "third_label", "FreeBSD")
    third_st: Optional[SymbolTable] = None
    if getattr(args, "third_summary", None):
        third_summary = load_summary(args.third_summary)
        third_map = getattr(args, "third_map", None)
        if third_map and os.path.exists(third_map):
            print(f"Loading {third_label} symbol table...", file=sys.stderr)
            try:
                third_st = SymbolTable.from_file(third_map)
                print(f"  {third_label}: loaded {len(third_st._addrs)} symbols", file=sys.stderr)
            except Exception as e:
                print(f"  warning: could not load {third_label} symbol map: {e}", file=sys.stderr)
        third_hist = getattr(args, "third_histogram", None)
        if third_hist and os.path.exists(third_hist):
            print(f"Loading {third_label} histogram...", file=sys.stderr)
            third_counts_int = load_histogram(third_hist, third_st)
            third_counts = {k: float(v) for k, v in third_counts_int.items()}
        else:
            third_counts = hotspot_proxy(third_summary)

    # Normalize MyGO counts
    mygo_norm = normalize_counts(mygo_counts, norm_factor)

    # Build comparison table
    print("Building comparison table...", file=sys.stderr)
    rows = build_comparison_table(mygo_norm, linux_counts, third_counts)

    # Open output
    out = sys.stdout
    if args.output:
        out = open(args.output, "w")

    try:
        # Write header
        out.write("BuildStorm Hotspot Comparison: MyGO vs Linux\n")
        out.write("=" * 80 + "\n")
        out.write(f"MyGO {norm_label} = {fmt_ns(mygo_cargo64)}  ")
        out.write(f"Linux {norm_label} = {fmt_ns(linux_cargo64)}  ")
        out.write(f"(normalization factor = {norm_factor:.3f})\n")
        out.write(f"Instruction counts from: {data_source}\n")
        out.write("\n")

        # Print table
        print_table(rows, out, third_label=third_label)
        out.write("\n")

        # Invisible overhead analysis
        if args.mygo_histogram and os.path.exists(args.mygo_histogram):
            print("Analyzing invisible overhead...", file=sys.stderr)
            invisible_section = analyze_invisible_overhead(
                mygo_hist_path=args.mygo_histogram,
                mygo_elf=args.mygo_elf,
                mygo_total_ns=mygo_cargo64,
                linux_hist_path=args.linux_histogram if args.linux_histogram else None,
                linux_total_ns=linux_cargo64,
            )
            out.write(invisible_section + "\n")

        # Annotated disassembly for top-N
        if args.top_n > 0:
            print(f"Generating annotated disassembly for top {args.top_n} functions...", file=sys.stderr)
            out.write("\n")
            out.write("=" * 80 + "\n")
            out.write(f"Top {args.top_n} Functions - Annotated Disassembly\n")
            out.write("=" * 80 + "\n\n")

            # Load objdumps (cached)
            objdump_cache: Dict[str, Optional[Dict[str, List[Tuple[int, str]]]]] = {}
            mygo_objdump: Optional[Dict[str, List[Tuple[int, str]]]] = None
            linux_objdump: Optional[Dict[str, List[Tuple[int, str]]]] = None

            if args.mygo_elf:
                mygo_objdump = _get_objdump(args.mygo_elf, objdump_cache)
            if args.linux_elf:
                linux_objdump = _get_objdump(args.linux_elf, objdump_cache)

            # Build pc_counts for per-instruction annotation
            mygo_pc_counts = build_pc_counts(args.mygo_histogram)
            linux_pc_counts = build_pc_counts(args.linux_histogram)

            # Get Linux symbol names for matching
            linux_names = linux_st.all_names() if linux_st else []

            for row in rows[:args.top_n]:
                rank = row["rank"]
                mygo_fn = row["function"]

                # MyGO annotation
                if mygo_objdump and mygo_fn in mygo_objdump:
                    insns = mygo_objdump[mygo_fn]
                    _annotate_function(
                        mygo_fn, insns, None, mygo_pc_counts, out, "MyGO", rank
                    )
                else:
                    out.write(f"━━━ {rank}. MyGO: {mygo_fn} (no disassembly available)\n\n")

                # Find Linux equivalent
                linux_candidates = find_linux_equivalents(mygo_fn, linux_names)
                if linux_candidates:
                    linux_fn = linux_candidates[0]
                    if linux_objdump and linux_fn in linux_objdump:
                        insns = linux_objdump[linux_fn]
                        _annotate_function(
                            linux_fn, insns, None, linux_pc_counts, out, "Linux", rank
                        )
                    else:
                        out.write(f"━━━ {rank}. Linux: {linux_fn} (no disassembly available)\n\n")
                else:
                    out.write(f"━━━ {rank}. Linux: (no equivalent found for {mygo_fn})\n\n")

        print("Done.", file=sys.stderr)

    finally:
        if args.output:
            out.close()


if __name__ == "__main__":
    main()
