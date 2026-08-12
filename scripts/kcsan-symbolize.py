#!/usr/bin/env python3
"""使用同次链接的 LLD map 将 KCSAN 返回地址解析到函数。"""

from __future__ import annotations

import argparse
import bisect
import hashlib
import re
import sys
from dataclasses import dataclass
from pathlib import Path


HEX = re.compile(r"[0-9A-Fa-f]+")


@dataclass(frozen=True)
class Symbol:
    start: int
    stop: int
    name: str


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_build_pair(symbol_map: Path) -> None:
    manifest = Path(f"{symbol_map}.manifest")
    kernel = symbol_map.with_name("kernel")
    if not manifest.is_file() or not kernel.is_file():
        raise ValueError(
            f"缺少 {manifest} 或 {kernel}；不能确认 map 与运行镜像来自同一次链接"
        )
    values: dict[str, str] = {}
    for line in manifest.read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        if not separator or not key or key in values:
            raise ValueError(f"{manifest}: 非法字段 {line!r}")
        values[key] = value
    required = {"schema", "target", "kernel_sha256", "symbol_map_sha256"}
    if set(values) != required or values["schema"] != "mygo.kernel-map-manifest.v1":
        raise ValueError(f"{manifest}: 非法 manifest schema")
    if sha256(symbol_map) != values["symbol_map_sha256"]:
        raise ValueError(f"{symbol_map}: SHA-256 与 manifest 不匹配")
    if sha256(kernel) != values["kernel_sha256"]:
        raise ValueError(f"{kernel}: SHA-256 与 manifest 不匹配")


def load_symbols(path: Path) -> tuple[list[int], list[Symbol]]:
    by_start: dict[int, Symbol] = {}
    for raw_line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        fields = raw_line.split()
        if len(fields) < 5 or not all(HEX.fullmatch(field) for field in fields[:4]):
            continue
        start = int(fields[0], 16)
        size = int(fields[2], 16)
        name = " ".join(fields[4:])
        if (
            size == 0
            or name.startswith((".", "/", "<"))
            or ":(" in name
            or " = " in name
            or name == "="
        ):
            continue
        candidate = Symbol(start, start + size, name)
        previous = by_start.get(start)
        if previous is None or candidate.stop > previous.stop:
            by_start[start] = candidate
    symbols = sorted(by_start.values(), key=lambda symbol: symbol.start)
    if not symbols:
        raise ValueError(f"{path}: 未找到可解析的函数符号")
    return [symbol.start for symbol in symbols], symbols


def parse_pc(value: str) -> int:
    try:
        return int(value, 0)
    except ValueError as error:
        raise argparse.ArgumentTypeError(f"非法 PC: {value}") from error


def resolve(starts: list[int], symbols: list[Symbol], pc: int) -> Symbol | None:
    probe = pc - 1 if pc else pc
    index = bisect.bisect_right(starts, probe) - 1
    if index < 0:
        return None
    symbol = symbols[index]
    return symbol if probe < symbol.stop else None


def main() -> int:
    parser = argparse.ArgumentParser(
        description="用同次 KCSAN 链接的 kernel.map 解析 hook 返回地址"
    )
    parser.add_argument("symbol_map", type=Path, help="build/kcsan/<arch>/kernel.map")
    parser.add_argument("pc", nargs="+", type=parse_pc, help="报告中的一个或多个 PC")
    parser.add_argument(
        "--no-verify",
        action="store_true",
        help="跳过 kernel.map.manifest 与同目录 kernel 的哈希校验",
    )
    args = parser.parse_args()

    try:
        if not args.no_verify:
            verify_build_pair(args.symbol_map)
        starts, symbols = load_symbols(args.symbol_map)
    except (OSError, ValueError) as error:
        print(f"kcsan-symbolize: {error}", file=sys.stderr)
        return 2

    missing = False
    for pc in args.pc:
        symbol = resolve(starts, symbols, pc)
        if symbol is None:
            print(f"0x{pc:x} -> ??")
            missing = True
            continue
        probe = pc - 1 if pc else pc
        print(f"0x{pc:x} -> {symbol.name}+0x{probe - symbol.start:x}")
    return 1 if missing else 0


if __name__ == "__main__":
    raise SystemExit(main())
