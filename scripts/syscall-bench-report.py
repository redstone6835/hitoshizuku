#!/usr/bin/env python3
"""严格校验 syscall 基准产物，并汇总延迟或 QEMU TCG 指令热点。"""

from __future__ import annotations

import argparse
import bisect
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any


FIELD_NAME = re.compile(r"^[A-Za-z0-9_]+$")
UINT = re.compile(r"^[0-9]+$")
SINT = re.compile(r"^-?[0-9]+$")
DECIMAL_NS = re.compile(r"^[0-9]+\.[0-9]{3}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
CASE_NAME = re.compile(r"^[A-Za-z0-9_]+$")
HEX = re.compile(r"^0x[0-9a-f]+$")
HEX_BYTES = re.compile(r"^[0-9a-f]+$")
METADATA_SCHEMA = "mygo.syscall-bench-run.v1"
MYGO_EXTERNAL_INITRAMFS_MARKER = "root source selected: external initramfs"

EXPECTED_SYSCALLS = {
    "futex_wake": 98,
    "clock_gettime": 113,
    "sched_yield": 124,
    "gettimeofday": 169,
    "getpid": 172,
    "getppid": 173,
    "getuid": 174,
    "gettid": 178,
    "read": 63,
    "write": 64,
}
ZERO_RESULT_CASES = {
    "clock_gettime",
    "futex_wake",
    "gettimeofday",
    "getuid",
    "sched_yield",
}
POSITIVE_CONSTANT_CASES = {"getpid", "getppid", "gettid"}

BENCH_HEADER_FIELDS = {"version", "arch", "iterations", "warmup", "repeats", "filter"}
RESULT_FIELDS = {
    "case",
    "syscall",
    "round",
    "iterations",
    "total_ns",
    "empty_ns",
    "net_ns",
    "avg_ns",
    "errors",
    "checksum",
}
SUMMARY_FIELDS = {
    "case",
    "syscall",
    "iterations",
    "repeats",
    "median_net_ns",
    "median_avg_ns",
}
DONE_FIELDS = {"status", "cases"}
GUEST_DONE_FIELDS = {"status"}


@dataclass(frozen=True)
class Symbol:
    start: int
    end: int
    name: str


@dataclass
class HotCount:
    blocks: int = 0
    instructions: int = 0


@dataclass(frozen=True)
class SerialRun:
    path: Path
    header: dict[str, str]
    results: dict[str, tuple[dict[str, str], ...]]
    summaries: dict[str, dict[str, str]]


def parse_fields(line: str, record: str, required: set[str]) -> dict[str, str]:
    words = line.split()
    if not words or words[0] != record:
        raise ValueError(f"{record} 记录格式错误")
    values: dict[str, str] = {}
    for word in words[1:]:
        if word.count("=") != 1:
            raise ValueError(f"{record} 字段格式错误: {word!r}")
        name, value = word.split("=", 1)
        if not FIELD_NAME.fullmatch(name) or not value:
            raise ValueError(f"{record} 字段格式错误: {word!r}")
        if name in values:
            raise ValueError(f"{record} 字段重复: {name}")
        values[name] = value
    missing = required - values.keys()
    extra = values.keys() - required
    if missing or extra:
        raise ValueError(
            f"{record} 字段集合错误: missing={sorted(missing)} extra={sorted(extra)}"
        )
    return values


def uint(values: dict[str, str], name: str, record: str) -> int:
    value = values[name]
    if not UINT.fullmatch(value):
        raise ValueError(f"{record}.{name} 不是无符号整数: {value!r}")
    return int(value)


def sint(values: dict[str, str], name: str, record: str) -> int:
    value = values[name]
    if not SINT.fullmatch(value):
        raise ValueError(f"{record}.{name} 不是整数: {value!r}")
    return int(value)


def format_avg_ns(nanoseconds: int, iterations: int) -> str:
    whole, remainder = divmod(nanoseconds, iterations)
    return f"{whole}.{remainder * 1000 // iterations:03d}"


def parse_serial(path: Path) -> SerialRun:
    headers: list[dict[str, str]] = []
    raw_results: list[dict[str, str]] = []
    raw_summaries: list[dict[str, str]] = []
    done: list[dict[str, str]] = []
    guest_done: list[dict[str, str]] = []
    errors: list[str] = []

    for line_number, raw_line in enumerate(
        path.read_text(encoding="utf-8", errors="strict").splitlines(), 1
    ):
        line = raw_line.strip()
        try:
            if line.startswith("SYSCALL_BENCH "):
                headers.append(parse_fields(line, "SYSCALL_BENCH", BENCH_HEADER_FIELDS))
            elif line.startswith("SYSCALL_RESULT "):
                raw_results.append(parse_fields(line, "SYSCALL_RESULT", RESULT_FIELDS))
            elif line.startswith("SYSCALL_SUMMARY "):
                raw_summaries.append(parse_fields(line, "SYSCALL_SUMMARY", SUMMARY_FIELDS))
            elif line.startswith("SYSCALL_BENCH_DONE "):
                done.append(parse_fields(line, "SYSCALL_BENCH_DONE", DONE_FIELDS))
            elif line.startswith("SYSCALL_GUEST_DONE "):
                guest_done.append(
                    parse_fields(line, "SYSCALL_GUEST_DONE", GUEST_DONE_FIELDS)
                )
            elif line.startswith("SYSCALL_ERROR "):
                errors.append(line)
            elif line.startswith("SYSCALL_"):
                raise ValueError(f"未知 syscall 基准记录: {line.split()[0]}")
        except ValueError as error:
            raise ValueError(f"{path}:{line_number}: {error}") from error

    if len(headers) != 1:
        raise ValueError(f"{path}: SYSCALL_BENCH 数量为 {len(headers)}，预期为 1")
    if len(done) != 1:
        raise ValueError(f"{path}: SYSCALL_BENCH_DONE 数量为 {len(done)}，预期为 1")
    if len(guest_done) != 1:
        raise ValueError(f"{path}: SYSCALL_GUEST_DONE 数量为 {len(guest_done)}，预期为 1")
    if errors:
        raise ValueError(f"{path}: 存在 SYSCALL_ERROR: {errors[0]}")

    header = headers[0]
    if header["version"] != "1" or header["arch"] != "riscv64":
        raise ValueError(
            f"{path}: 不支持的基准协议 version={header['version']} arch={header['arch']}"
        )
    iterations = uint(header, "iterations", "SYSCALL_BENCH")
    warmup = uint(header, "warmup", "SYSCALL_BENCH")
    repeats = uint(header, "repeats", "SYSCALL_BENCH")
    if not 1 <= iterations <= 1_000_000_000:
        raise ValueError(f"{path}: iterations 超出范围")
    if not 0 <= warmup <= 1_000_000_000 or not 1 <= repeats <= 31:
        raise ValueError(f"{path}: warmup/repeats 超出范围")
    if not CASE_NAME.fullmatch(header["filter"]) or (
        header["filter"] != "all" and header["filter"] not in EXPECTED_SYSCALLS
    ):
        raise ValueError(f"{path}: filter 非法: {header['filter']!r}")
    if uint(done[0], "status", "SYSCALL_BENCH_DONE") != 0:
        raise ValueError(f"{path}: SYSCALL_BENCH_DONE 非零")
    if uint(guest_done[0], "status", "SYSCALL_GUEST_DONE") != 0:
        raise ValueError(f"{path}: SYSCALL_GUEST_DONE 非零")

    by_case: dict[str, dict[int, dict[str, str]]] = defaultdict(dict)
    case_syscalls: dict[str, int] = {}
    for value in raw_results:
        name = value["case"]
        if not CASE_NAME.fullmatch(name):
            raise ValueError(f"{path}: SYSCALL_RESULT case 非法: {name!r}")
        syscall = sint(value, "syscall", "SYSCALL_RESULT")
        if EXPECTED_SYSCALLS.get(name) != syscall:
            raise ValueError(f"{path}: {name} syscall 编号错误: {syscall}")
        round_number = uint(value, "round", "SYSCALL_RESULT")
        if round_number in by_case[name]:
            raise ValueError(f"{path}: {name} round={round_number} 重复")
        if uint(value, "iterations", "SYSCALL_RESULT") != iterations:
            raise ValueError(f"{path}: {name} iterations 与 header 不一致")
        total_ns = uint(value, "total_ns", "SYSCALL_RESULT")
        empty_ns = uint(value, "empty_ns", "SYSCALL_RESULT")
        net_ns = uint(value, "net_ns", "SYSCALL_RESULT")
        if net_ns != max(total_ns - empty_ns, 0):
            raise ValueError(f"{path}: {name} round={round_number} net_ns 不一致")
        if not DECIMAL_NS.fullmatch(value["avg_ns"]) or value["avg_ns"] != format_avg_ns(
            net_ns, iterations
        ):
            raise ValueError(f"{path}: {name} round={round_number} avg_ns 不一致")
        if uint(value, "errors", "SYSCALL_RESULT") != 0:
            raise ValueError(f"{path}: {name} round={round_number} errors 非零")
        checksum = uint(value, "checksum", "SYSCALL_RESULT")
        if name in ZERO_RESULT_CASES and checksum != 0:
            raise ValueError(f"{path}: {name} 返回值校验失败: checksum={checksum}")
        if name in POSITIVE_CONSTANT_CASES and (
            checksum == 0
            or checksum % iterations != 0
            or checksum // iterations > 2_147_483_647
        ):
            raise ValueError(f"{path}: {name} 返回值不是稳定的正 PID: checksum={checksum}")
        if name in case_syscalls and case_syscalls[name] != syscall:
            raise ValueError(f"{path}: {name} syscall 编号不一致")
        case_syscalls[name] = syscall
        by_case[name][round_number] = value

    summaries: dict[str, dict[str, str]] = {}
    for value in raw_summaries:
        name = value["case"]
        if not CASE_NAME.fullmatch(name) or name in summaries:
            raise ValueError(f"{path}: SYSCALL_SUMMARY case 非法或重复: {name!r}")
        syscall = sint(value, "syscall", "SYSCALL_SUMMARY")
        if uint(value, "iterations", "SYSCALL_SUMMARY") != iterations:
            raise ValueError(f"{path}: {name} summary iterations 不一致")
        if uint(value, "repeats", "SYSCALL_SUMMARY") != repeats:
            raise ValueError(f"{path}: {name} summary repeats 不一致")
        if case_syscalls.get(name) != syscall:
            raise ValueError(f"{path}: {name} summary syscall 编号不一致")
        summaries[name] = value

    expected_cases = uint(done[0], "cases", "SYSCALL_BENCH_DONE")
    if expected_cases < 1:
        raise ValueError(f"{path}: cases 为空")
    if set(by_case) != set(summaries) or len(summaries) != expected_cases:
        raise ValueError(
            f"{path}: result/summary/DONE case 集合不完整: "
            f"results={sorted(by_case)} summaries={sorted(summaries)} done={expected_cases}"
        )
    if header["filter"] == "all":
        if set(summaries) != set(EXPECTED_SYSCALLS):
            raise ValueError(f"{path}: all filter 的 case 集合不完整")
    elif expected_cases != 1 or set(summaries) != {header["filter"]}:
        raise ValueError(f"{path}: 单 case filter 与结果不一致")

    results: dict[str, tuple[dict[str, str], ...]] = {}
    for name, rounds in by_case.items():
        if set(rounds) != set(range(1, repeats + 1)):
            raise ValueError(f"{path}: {name} round 不完整: {sorted(rounds)}")
        ordered = tuple(rounds[index] for index in range(1, repeats + 1))
        if name in POSITIVE_CONSTANT_CASES:
            returned = {
                uint(value, "checksum", "SYSCALL_RESULT") // iterations
                for value in ordered
            }
            if len(returned) != 1:
                raise ValueError(f"{path}: {name} 各轮返回值不一致: {sorted(returned)}")
        median_net_ns = sorted(
            uint(value, "net_ns", "SYSCALL_RESULT") for value in ordered
        )[repeats // 2]
        summary = summaries[name]
        if uint(summary, "median_net_ns", "SYSCALL_SUMMARY") != median_net_ns:
            raise ValueError(f"{path}: {name} median_net_ns 不一致")
        if (
            not DECIMAL_NS.fullmatch(summary["median_avg_ns"])
            or summary["median_avg_ns"] != format_avg_ns(median_net_ns, iterations)
        ):
            raise ValueError(f"{path}: {name} median_avg_ns 不一致")
        results[name] = ordered

    return SerialRun(path=path, header=header, results=results, summaries=summaries)


def validate_mygo_boot_source(run: SerialRun) -> None:
    marker_count = sum(
        MYGO_EXTERNAL_INITRAMFS_MARKER in line
        for line in run.path.read_text(encoding="utf-8", errors="strict").splitlines()
    )
    if marker_count != 1:
        raise ValueError(
            f"{run.path}: 外部 initramfs 启动标记数量为 {marker_count}，预期为 1"
        )


def validate_expected(run: SerialRun, parameters: dict[str, Any]) -> None:
    expected = {
        "iterations": str(parameters["iterations"]),
        "warmup": str(parameters["warmup"]),
        "repeats": str(parameters["repeats"]),
        "filter": parameters["case"],
    }
    actual = {name: run.header[name] for name in expected}
    if actual != expected:
        raise ValueError(f"{run.path}: 参数不匹配 actual={actual} expected={expected}")


def validate_pair(mygo: SerialRun, linux: SerialRun) -> None:
    keys = ("version", "arch", "iterations", "warmup", "repeats", "filter")
    mygo_parameters = tuple(mygo.header[name] for name in keys)
    linux_parameters = tuple(linux.header[name] for name in keys)
    if mygo_parameters != linux_parameters:
        raise ValueError("Hitoshizuku/Linux 串口日志参数不一致")
    if set(mygo.summaries) != set(linux.summaries):
        raise ValueError("Hitoshizuku/Linux syscall case 集合不一致")
    for name in mygo.summaries:
        if mygo.summaries[name]["syscall"] != linux.summaries[name]["syscall"]:
            raise ValueError(f"Hitoshizuku/Linux {name} syscall 编号不一致")


def print_timing(mygo: SerialRun, linux: SerialRun) -> None:
    print("\n延迟对比（单次启动内中位数，已扣除等价空循环）")
    print("| syscall | Hitoshizuku ns/次 | Linux ns/次 | Hitoshizuku/Linux | Hitoshizuku 总计 ms | Linux 总计 ms |")
    print("| --- | ---: | ---: | ---: | ---: | ---: |")
    for name in sorted(mygo.summaries):
        mygo_value = mygo.summaries[name]
        linux_value = linux.summaries[name]
        mygo_avg = float(mygo_value["median_avg_ns"])
        linux_avg = float(linux_value["median_avg_ns"])
        mygo_total = int(mygo_value["median_net_ns"]) / 1_000_000
        linux_total = int(linux_value["median_net_ns"]) / 1_000_000
        ratio = mygo_avg / linux_avg if linux_avg else float("inf")
        print(
            f"| {name} | {mygo_avg:.3f} | {linux_avg:.3f} | {ratio:.2f}x | "
            f"{mygo_total:.3f} | {linux_total:.3f} |"
        )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def parse_manifest(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line_number, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if raw_line.count("=") != 1:
            raise ValueError(f"{path}:{line_number}: manifest 行格式错误")
        name, value = raw_line.split("=", 1)
        if not FIELD_NAME.fullmatch(name) or not value or name in values:
            raise ValueError(f"{path}:{line_number}: manifest 字段非法或重复")
        values[name] = value
    required = {"schema", "target", "kernel_sha256", "symbol_map_sha256"}
    if not required <= values.keys() or values["schema"] != "mygo.kernel-map-manifest.v1":
        raise ValueError(f"{path}: manifest schema/字段不完整")
    for name in ("kernel_sha256", "symbol_map_sha256"):
        if not SHA256.fullmatch(values[name]):
            raise ValueError(f"{path}: {name} 非法")
    return values


def validate_manifest_binding(
    manifest_path: Path,
    kernel_path: Path,
    map_path: Path,
    expected_target: str,
    exact_fields: bool,
) -> dict[str, str]:
    values = parse_manifest(manifest_path)
    required = {"schema", "target", "kernel_sha256", "symbol_map_sha256"}
    if exact_fields and set(values) != required:
        raise ValueError(f"{manifest_path}: Hitoshizuku manifest 必须恰好包含四个字段")
    if values["target"] != expected_target:
        raise ValueError(
            f"{manifest_path}: target={values['target']} expected={expected_target}"
        )
    if values["kernel_sha256"] != sha256_file(kernel_path):
        raise ValueError(f"{manifest_path}: kernel_sha256 与 {kernel_path} 不匹配")
    if values["symbol_map_sha256"] != sha256_file(map_path):
        raise ValueError(f"{manifest_path}: symbol_map_sha256 与 {map_path} 不匹配")
    return values


def validate_linux_image(vmlinux: Path, image: Path, objcopy: str) -> str:
    with tempfile.TemporaryDirectory(prefix="syscall-bench-linux-image-") as directory:
        generated = Path(directory) / "Image"
        subprocess.run(
            [
                objcopy,
                "-O",
                "binary",
                "-R",
                ".note",
                "-R",
                ".note.gnu.build-id",
                "-R",
                ".comment",
                "-S",
                str(vmlinux),
                str(generated),
            ],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        generated_sha = sha256_file(generated)
    if generated_sha != sha256_file(image):
        raise ValueError(f"Linux Image 不是由指定 vmlinux 生成: {image}")
    return generated_sha


def validate_benchmark_binding(
    benchmark_elf: Path, initramfs: Path, strip: str, cpio: str
) -> str:
    archive = initramfs.read_bytes()
    extracted = subprocess.run(
        [cpio, "-i", "--quiet", "--to-stdout", "bin/syscall-bench"],
        input=archive,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout
    if not extracted:
        raise ValueError(f"{initramfs}: 未找到 bin/syscall-bench")
    with tempfile.TemporaryDirectory(prefix="syscall-bench-elf-") as directory:
        stripped = Path(directory) / "syscall-bench"
        shutil.copyfile(benchmark_elf, stripped)
        subprocess.run(
            [strip, str(stripped)],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        stripped_bytes = stripped.read_bytes()
    if stripped_bytes != extracted:
        raise ValueError("benchmark ELF 与 initramfs 内 bin/syscall-bench 不匹配")
    return hashlib.sha256(extracted).hexdigest()


def resolve_inside_repo(path: Path, repo_root: Path) -> tuple[Path, str]:
    resolved = path.resolve(strict=True)
    try:
        relative = resolved.relative_to(repo_root)
    except ValueError as error:
        raise ValueError(f"产物必须位于仓库内: {resolved}") from error
    if not resolved.is_file():
        raise ValueError(f"产物不是普通文件: {resolved}")
    return resolved, relative.as_posix()


def metadata_artifact(path: Path, repo_root: Path) -> dict[str, Any]:
    resolved, relative = resolve_inside_repo(path, repo_root)
    return {
        "path": relative,
        "sha256": sha256_file(resolved),
        "size": resolved.stat().st_size,
    }


def required_artifact_names(mode: str) -> set[str]:
    names = {
        "mygo_kernel",
        "mygo_map",
        "mygo_manifest",
        "linux_image",
        "linux_vmlinux",
        "linux_map",
        "linux_manifest",
        "initramfs",
        "benchmark_elf",
    }
    if mode == "profile":
        names.add("profile_plugin")
    elif mode == "trace":
        names.add("trace_plugin")
    return names


def artifact_arguments(args: argparse.Namespace) -> dict[str, Path | None]:
    return {
        "mygo_kernel": args.mygo_kernel,
        "mygo_map": args.mygo_map,
        "mygo_manifest": args.mygo_manifest,
        "linux_image": args.linux_image,
        "linux_vmlinux": args.linux_kernel,
        "linux_map": args.linux_map,
        "linux_manifest": args.linux_manifest,
        "initramfs": args.initramfs,
        "benchmark_elf": args.benchmark_elf,
        "profile_plugin": args.profile_plugin,
        "trace_plugin": args.trace_plugin,
    }


def validate_window_parameters(parameters: dict[str, Any], mode: str) -> None:
    if parameters["smp"] != 1:
        raise ValueError(f"{mode} 模式必须使用 SMP=1")
    if parameters["repeats"] != 1:
        raise ValueError(f"{mode} 模式必须使用 repeats=1")
    if parameters["case"] == "all":
        raise ValueError(f"{mode} 模式必须指定单个 syscall case")
    start_pc = int(parameters["profile_start_pc"], 0)
    stop_pc = int(parameters["profile_stop_pc"], 0)
    if start_pc == stop_pc:
        raise ValueError(f"{mode} start/stop PC 必须不同")
    if mode == "trace" and (
        parameters["iterations"] != 1 or parameters["warmup"] != 0
    ):
        raise ValueError("trace 模式必须使用 iterations=1 且 warmup=0")


def build_metadata(args: argparse.Namespace, repo_root: Path) -> dict[str, Any]:
    required = required_artifact_names(args.mode)
    supplied = artifact_arguments(args)
    missing = sorted(name for name in required if supplied[name] is None)
    if missing:
        raise ValueError(f"生成元数据缺少产物参数: {missing}")
    numeric_parameters = {
        "smp": args.smp,
        "timeout_seconds": args.timeout_seconds,
        "iterations": args.iterations,
        "repeats": args.repeats,
        "warmup": args.warmup,
        "table_bits": args.table_bits,
    }
    if any(value is None for value in numeric_parameters.values()) or any(
        value is None for value in (args.memory, args.accel, args.case, args.container_image)
    ):
        raise ValueError("生成元数据时必须提供完整运行参数")
    if not CASE_NAME.fullmatch(args.case):
        raise ValueError(f"非法 syscall case: {args.case!r}")
    if args.smp < 1 or args.timeout_seconds < 1 or args.iterations < 1 or args.repeats < 1:
        raise ValueError("运行参数必须为正数")
    if args.warmup < 0 or not 12 <= args.table_bits <= 23:
        raise ValueError("warmup/table_bits 超出范围")
    parameters: dict[str, Any] = {
        "target": "riscv64",
        "smp": args.smp,
        "memory": args.memory,
        "accel": args.accel,
        "timeout_seconds": args.timeout_seconds,
        "iterations": args.iterations,
        "repeats": args.repeats,
        "case": args.case,
        "warmup": args.warmup,
        "table_bits": args.table_bits,
        "profile_start_pc": (
            args.profile_start_pc if args.mode in ("profile", "trace") else None
        ),
        "profile_stop_pc": (
            args.profile_stop_pc if args.mode in ("profile", "trace") else None
        ),
        "container_image": args.container_image,
    }
    if args.mode == "trace":
        if args.trace_max_instructions is None or not (
            1 <= args.trace_max_instructions <= 10_000_000
        ):
            raise ValueError("trace_max_instructions 超出范围")
        parameters["trace_max_instructions"] = args.trace_max_instructions
    if args.mode in ("profile", "trace"):
        if args.profile_start_pc is None or args.profile_stop_pc is None:
            raise ValueError(f"{args.mode} 模式缺少 start/stop PC")
        try:
            parameters["profile_start_pc"] = f"0x{int(args.profile_start_pc, 0):x}"
            parameters["profile_stop_pc"] = f"0x{int(args.profile_stop_pc, 0):x}"
        except ValueError as error:
            raise ValueError(f"{args.mode} start/stop PC 非法") from error
        validate_window_parameters(parameters, args.mode)

    artifacts = {
        name: metadata_artifact(supplied[name], repo_root)  # type: ignore[arg-type]
        for name in sorted(required)
    }
    paths = {
        name: repo_root / value["path"] for name, value in artifacts.items()
    }
    mygo_manifest = validate_manifest_binding(
        paths["mygo_manifest"],
        paths["mygo_kernel"],
        paths["mygo_map"],
        "riscv64gc-unknown-none-elf",
        exact_fields=True,
    )
    linux_manifest = validate_manifest_binding(
        paths["linux_manifest"],
        paths["linux_vmlinux"],
        paths["linux_map"],
        "riscv64-linux-gnu",
        exact_fields=False,
    )
    bindings = {
        "mygo_manifest_target": mygo_manifest["target"],
        "linux_manifest_target": linux_manifest["target"],
        "linux_image_from_vmlinux_sha256": validate_linux_image(
            paths["linux_vmlinux"], paths["linux_image"], args.objcopy
        ),
        "initramfs_benchmark_sha256": validate_benchmark_binding(
            paths["benchmark_elf"], paths["initramfs"], args.strip, args.cpio
        ),
    }
    return {
        "schema": METADATA_SCHEMA,
        "mode": args.mode,
        "systems": args.systems,
        "parameters": parameters,
        "artifacts": artifacts,
        "bindings": bindings,
        "results": {},
    }


def write_metadata(path: Path, metadata: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(
        json.dumps(metadata, ensure_ascii=True, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


def load_and_verify_metadata(
    path: Path, args: argparse.Namespace, repo_root: Path
) -> dict[str, Any]:
    try:
        metadata = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise ValueError(f"{path}: JSON 非法: {error}") from error
    required_top = {
        "schema",
        "mode",
        "systems",
        "parameters",
        "artifacts",
        "bindings",
        "results",
    }
    if not isinstance(metadata, dict) or set(metadata) != required_top:
        raise ValueError(f"{path}: 元数据顶层字段不正确")
    if metadata["schema"] != METADATA_SCHEMA or metadata["mode"] != args.mode:
        raise ValueError(f"{path}: 元数据 schema/mode 不匹配")
    if metadata["systems"] not in ("mygo", "linux", "both"):
        raise ValueError(f"{path}: systems 非法")
    parameters = metadata["parameters"]
    parameter_fields = {
        "target",
        "smp",
        "memory",
        "accel",
        "timeout_seconds",
        "iterations",
        "repeats",
        "case",
        "warmup",
        "table_bits",
        "profile_start_pc",
        "profile_stop_pc",
        "container_image",
    }
    if args.mode == "trace":
        parameter_fields.add("trace_max_instructions")
    if not isinstance(parameters, dict) or set(parameters) != parameter_fields:
        raise ValueError(f"{path}: parameters 字段不正确")
    if (
        parameters["target"] != "riscv64"
        or not isinstance(parameters["case"], str)
        or not CASE_NAME.fullmatch(parameters["case"])
        or any(
            not isinstance(parameters[name], str) or not parameters[name]
            for name in ("memory", "accel", "container_image")
        )
    ):
        raise ValueError(f"{path}: target/case 非法")
    for name in ("smp", "timeout_seconds", "iterations", "repeats", "warmup", "table_bits"):
        if not isinstance(parameters[name], int) or isinstance(parameters[name], bool):
            raise ValueError(f"{path}: parameters.{name} 类型非法")
    if args.mode == "trace" and (
        not isinstance(parameters["trace_max_instructions"], int)
        or isinstance(parameters["trace_max_instructions"], bool)
        or not 1 <= parameters["trace_max_instructions"] <= 10_000_000
    ):
        raise ValueError(f"{path}: parameters.trace_max_instructions 非法")
    if (
        parameters["smp"] < 1
        or parameters["timeout_seconds"] < 1
        or parameters["iterations"] < 1
        or parameters["repeats"] < 1
        or parameters["warmup"] < 0
        or not 12 <= parameters["table_bits"] <= 23
    ):
        raise ValueError(f"{path}: parameters 数值超出范围")
    if args.mode in ("profile", "trace"):
        if not isinstance(parameters["profile_start_pc"], str) or not isinstance(
            parameters["profile_stop_pc"], str
        ):
            raise ValueError(f"{path}: {args.mode} marker 类型非法")
        validate_window_parameters(parameters, args.mode)
    elif parameters["profile_start_pc"] is not None or parameters["profile_stop_pc"] is not None:
        raise ValueError(f"{path}: timing 元数据包含 profile marker")

    artifacts = metadata["artifacts"]
    required = required_artifact_names(args.mode)
    if not isinstance(artifacts, dict) or set(artifacts) != required:
        raise ValueError(f"{path}: artifacts 字段不完整")
    resolved: dict[str, Path] = {}
    for name, value in artifacts.items():
        if not isinstance(value, dict) or set(value) != {"path", "sha256", "size"}:
            raise ValueError(f"{path}: artifacts.{name} 字段非法")
        if (
            not isinstance(value["path"], str)
            or not isinstance(value["sha256"], str)
            or not SHA256.fullmatch(value["sha256"])
            or not isinstance(value["size"], int)
            or isinstance(value["size"], bool)
            or value["size"] < 1
        ):
            raise ValueError(f"{path}: artifacts.{name} 值非法")
        candidate = (repo_root / value["path"]).resolve(strict=True)
        try:
            candidate.relative_to(repo_root)
        except ValueError as error:
            raise ValueError(f"{path}: artifacts.{name} 越出仓库") from error
        if not candidate.is_file() or candidate.stat().st_size != value["size"]:
            raise ValueError(f"{path}: artifacts.{name} 文件/大小不匹配")
        if sha256_file(candidate) != value["sha256"]:
            raise ValueError(f"{path}: artifacts.{name} SHA-256 不匹配")
        resolved[name] = candidate

    supplied = artifact_arguments(args)
    for name in required:
        supplied_path = supplied[name]
        if supplied_path is not None and supplied_path.resolve(strict=True) != resolved[name]:
            raise ValueError(f"命令行 {name} 与运行元数据不一致")
    mygo_manifest = validate_manifest_binding(
        resolved["mygo_manifest"],
        resolved["mygo_kernel"],
        resolved["mygo_map"],
        "riscv64gc-unknown-none-elf",
        exact_fields=True,
    )
    linux_manifest = validate_manifest_binding(
        resolved["linux_manifest"],
        resolved["linux_vmlinux"],
        resolved["linux_map"],
        "riscv64-linux-gnu",
        exact_fields=False,
    )
    expected_bindings = {
        "mygo_manifest_target": mygo_manifest["target"],
        "linux_manifest_target": linux_manifest["target"],
        "linux_image_from_vmlinux_sha256": validate_linux_image(
            resolved["linux_vmlinux"], resolved["linux_image"], args.objcopy
        ),
        "initramfs_benchmark_sha256": validate_benchmark_binding(
            resolved["benchmark_elf"], resolved["initramfs"], args.strip, args.cpio
        ),
    }
    if metadata["bindings"] != expected_bindings:
        raise ValueError(f"{path}: bindings 不匹配")
    results = metadata["results"]
    allowed_systems = {
        "mygo": {"mygo"},
        "linux": {"linux"},
        "both": {"mygo", "linux"},
    }[metadata["systems"]]
    if not isinstance(results, dict) or not set(results) <= allowed_systems:
        raise ValueError(f"{path}: results 系统集合非法")
    expected_result_names = {"serial"}
    if args.mode == "profile":
        expected_result_names.add("profile")
    elif args.mode == "trace":
        expected_result_names.add("trace")
    resolved_results: dict[str, dict[str, Path]] = {}
    run_directory = path.resolve(strict=True).parent
    for system, result in results.items():
        if not isinstance(result, dict) or set(result) != expected_result_names:
            raise ValueError(f"{path}: results.{system} 字段不完整")
        resolved_results[system] = {}
        for kind, value in result.items():
            if not isinstance(value, dict) or set(value) != {"path", "sha256", "size"}:
                raise ValueError(f"{path}: results.{system}.{kind} 字段非法")
            if (
                not isinstance(value["path"], str)
                or not isinstance(value["sha256"], str)
                or not SHA256.fullmatch(value["sha256"])
                or not isinstance(value["size"], int)
                or isinstance(value["size"], bool)
                or value["size"] < 1
            ):
                raise ValueError(f"{path}: results.{system}.{kind} 值非法")
            expected_name = {
                "serial": f"{system}.serial.log",
                "profile": f"{system}.tcg-profile.txt",
                "trace": f"{system}.instruction-trace.txt",
            }[kind]
            candidate = (repo_root / value["path"]).resolve(strict=True)
            if candidate != (run_directory / expected_name).resolve(strict=True):
                raise ValueError(f"{path}: results.{system}.{kind} 路径非法")
            if (
                candidate.stat().st_size != value["size"]
                or sha256_file(candidate) != value["sha256"]
            ):
                raise ValueError(f"{path}: results.{system}.{kind} 哈希/大小不匹配")
            resolved_results[system][kind] = candidate
    metadata["_resolved"] = resolved
    metadata["_resolved_results"] = resolved_results
    return metadata


def record_result_metadata(
    metadata_path: Path,
    metadata: dict[str, Any],
    repo_root: Path,
    system: str,
    serial: Path,
    auxiliary: Path | None,
) -> None:
    allowed_systems = {
        "mygo": {"mygo"},
        "linux": {"linux"},
        "both": {"mygo", "linux"},
    }[metadata["systems"]]
    if system not in allowed_systems:
        raise ValueError(f"不能为本次运行记录 {system} 结果")
    value = {"serial": metadata_artifact(serial, repo_root)}
    if metadata["mode"] == "profile":
        if auxiliary is None:
            raise ValueError(f"记录 {system} 结果时缺少 TCG profile")
        value["profile"] = metadata_artifact(auxiliary, repo_root)
    elif metadata["mode"] == "trace":
        if auxiliary is None:
            raise ValueError(f"记录 {system} 结果时缺少指令轨迹")
        value["trace"] = metadata_artifact(auxiliary, repo_root)
    elif auxiliary is not None:
        raise ValueError("timing 结果不能包含跟踪产物")
    metadata["results"][system] = value
    metadata.pop("_resolved", None)
    metadata.pop("_resolved_results", None)
    write_metadata(metadata_path, metadata)


def parse_instruction_trace(
    path: Path, expected_start: str, expected_stop: str, expected_maximum: int
) -> int:
    header_fields = {
        "version",
        "target",
        "configured_vcpus",
        "start_pc",
        "stop_pc",
        "max_instructions",
    }
    instruction_fields = {
        "sequence",
        "cpu",
        "pc",
        "size",
        "bytes",
        "disas_hex",
    }
    footer_fields = {
        "instructions",
        "dropped",
        "translation_failures",
        "start_events",
        "stop_events",
        "active_at_exit",
    }
    lines = path.read_text(encoding="utf-8", errors="strict").splitlines()
    if len(lines) < 3 or any(not line or line != line.strip() for line in lines):
        raise ValueError(f"{path}: 指令轨迹存在空行或首尾空白")
    header = parse_fields(lines[0], "MYGO_INSN_TRACE", header_fields)
    footer = parse_fields(lines[-1], "TRACE_DONE", footer_fields)
    if header["version"] != "1" or header["target"] != "riscv64":
        raise ValueError(f"{path}: 指令轨迹版本或目标架构错误")
    if uint(header, "configured_vcpus", "MYGO_INSN_TRACE") != 1:
        raise ValueError(f"{path}: 指令轨迹不是单核运行")
    for name, expected_marker in (
        ("start_pc", expected_start),
        ("stop_pc", expected_stop),
    ):
        if not HEX.fullmatch(header[name]) or int(header[name], 16) != int(
            expected_marker, 0
        ):
            raise ValueError(f"{path}: {name} 与运行元数据不一致")
    maximum = uint(header, "max_instructions", "MYGO_INSN_TRACE")
    if maximum != expected_maximum:
        raise ValueError(f"{path}: max_instructions 与运行元数据不一致")

    instruction_count = len(lines) - 2
    if instruction_count < 1 or instruction_count > maximum:
        raise ValueError(f"{path}: 指令记录数量非法: {instruction_count}")
    executed: list[tuple[int, str]] = []
    for sequence, line in enumerate(lines[1:-1]):
        values = parse_fields(line, "INSN", instruction_fields)
        if uint(values, "sequence", "INSN") != sequence:
            raise ValueError(f"{path}: INSN sequence 在 {sequence} 处不连续")
        if uint(values, "cpu", "INSN") != 0:
            raise ValueError(f"{path}: INSN cpu 不是 0")
        if not HEX.fullmatch(values["pc"]) or int(values["pc"], 16) >= 1 << 64:
            raise ValueError(f"{path}: INSN pc 非法")
        size = uint(values, "size", "INSN")
        raw = values["bytes"]
        if size not in (2, 4) or len(raw) != size * 2 or not HEX_BYTES.fullmatch(raw):
            raise ValueError(f"{path}: INSN bytes/size 非法")
        encoded_disassembly = values["disas_hex"]
        if (
            len(encoded_disassembly) % 2 != 0
            or not HEX_BYTES.fullmatch(encoded_disassembly)
        ):
            raise ValueError(f"{path}: INSN disas_hex 非法")
        disassembly = bytes.fromhex(encoded_disassembly).decode("utf-8", errors="strict")
        if any(character in disassembly for character in ("\x00", "\n", "\r")):
            raise ValueError(f"{path}: INSN 反汇编文本包含控制字符")
        mnemonic_words = disassembly.split()
        if not mnemonic_words:
            raise ValueError(f"{path}: INSN 反汇编文本为空")
        executed.append((int(values["pc"], 16), mnemonic_words[0]))

    spaces: list[str] = []
    for pc, _ in executed:
        space = "kernel" if pc & (1 << 63) else "user"
        if not spaces or spaces[-1] != space:
            spaces.append(space)
    if spaces != ["user", "kernel", "user"]:
        raise ValueError(f"{path}: 权限态轨迹不是 user -> kernel -> user: {spaces}")
    if sum(mnemonic == "ecall" for _, mnemonic in executed) != 1:
        raise ValueError(f"{path}: 指令轨迹必须恰好包含一个 ecall")
    if sum(mnemonic == "sret" for _, mnemonic in executed) != 1:
        raise ValueError(f"{path}: 指令轨迹必须恰好包含一个 sret")
    first_kernel = next(
        index for index, value in enumerate(executed) if value[0] & (1 << 63)
    )
    last_kernel = len(executed) - 1 - next(
        index for index, value in enumerate(reversed(executed)) if value[0] & (1 << 63)
    )
    if executed[first_kernel - 1][1] != "ecall" or executed[last_kernel][1] != "sret":
        raise ValueError(f"{path}: ecall/sret 不在唯一内核区段的边界")
    entry_pc = executed[first_kernel][0]
    if sum(pc == entry_pc for pc, _ in executed) != 1:
        raise ValueError(f"{path}: 内核入口 PC 重复执行，疑似发生嵌套 trap")

    if uint(footer, "instructions", "TRACE_DONE") != instruction_count:
        raise ValueError(f"{path}: TRACE_DONE instructions 与记录数量不一致")
    strict_footer = {
        "dropped": 0,
        "translation_failures": 0,
        "start_events": 1,
        "stop_events": 1,
        "active_at_exit": 0,
    }
    for name, expected_value in strict_footer.items():
        if uint(footer, name, "TRACE_DONE") != expected_value:
            raise ValueError(f"{path}: TRACE_DONE {name} 不是 {expected_value}")
    return instruction_count


def load_symbols(kernel: Path, nm: str) -> tuple[list[int], list[Symbol]]:
    output = subprocess.run(
        [nm, "-n", "-S", "-C", "--defined-only", str(kernel)],
        check=True,
        text=True,
        capture_output=True,
    ).stdout
    symbols: list[Symbol] = []
    for line in output.splitlines():
        parts = line.split(maxsplit=3)
        if len(parts) != 4 or parts[2] not in "tTwW":
            continue
        try:
            start = int(parts[0], 16)
            size = int(parts[1], 16)
        except ValueError:
            continue
        if size:
            symbols.append(Symbol(start, start + size, parts[3]))
    if not symbols:
        raise ValueError(f"{kernel}: 未加载到文本符号")
    symbols.sort(key=lambda symbol: (symbol.start, symbol.end))
    return [symbol.start for symbol in symbols], symbols


def find_symbol(pc: int, starts: list[int], symbols: list[Symbol]) -> Symbol | None:
    index = bisect.bisect_right(starts, pc) - 1
    if index < 0:
        return None
    candidate_start = starts[index]
    while index >= 0 and symbols[index].start == candidate_start:
        symbol = symbols[index]
        if pc < symbol.end:
            return symbol
        index -= 1
    return None


def parse_tcg(
    path: Path, expected_vcpus: int, expected_start: str, expected_stop: str
) -> tuple[dict[str, str], list[dict[str, str]]]:
    validator = Path(__file__).with_name("profile-tcg-validate.sh")
    result = subprocess.run(
        [
            str(validator),
            str(path),
            "riscv64",
            str(expected_vcpus),
            expected_start,
            expected_stop,
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        message = result.stderr.strip() or f"校验器退出码 {result.returncode}"
        raise ValueError(f"{path}: {message}")
    header: dict[str, str] | None = None
    hot: list[dict[str, str]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("MYGO_TCG_PROFILE "):
            header = parse_fields(
                line,
                "MYGO_TCG_PROFILE",
                {
                    "version", "target", "configured_vcpus", "active_vcpus",
                    "table_bits", "table_slots", "table_probes",
                    "counter_bytes_per_vcpu", "translated_blocks", "occupied_slots",
                    "dropped", "collision_probes", "max_probe", "total_blocks",
                    "total_instructions", "reported_hotspots", "windowed", "start_pc",
                    "stop_pc", "start_events", "stop_events", "active_at_exit",
                },
            )
        elif line.startswith("HOT "):
            hot.append(parse_fields(line, "HOT", {"rank", "pc", "blocks", "instructions"}))
    if header is None:
        raise ValueError(f"{path}: 缺少 profile header")
    return header, hot


def print_hotspots(
    system: str,
    profile_path: Path,
    kernel_path: Path,
    nm: str,
    top: int,
    parameters: dict[str, Any],
) -> None:
    header, rows = parse_tcg(
        profile_path,
        parameters["smp"],
        parameters["profile_start_pc"],
        parameters["profile_stop_pc"],
    )
    starts, symbols = load_symbols(kernel_path, nm)
    aggregate: dict[str, HotCount] = defaultdict(HotCount)
    symbolized = 0
    for row in rows:
        instructions = int(row["instructions"], 10)
        blocks = int(row["blocks"], 10)
        symbol = find_symbol(int(row["pc"], 0), starts, symbols)
        if symbol is None:
            continue
        value = aggregate[symbol.name]
        value.instructions += instructions
        value.blocks += blocks
        symbolized += instructions

    total = int(header["total_instructions"], 10)
    kernel_pct = symbolized * 100 / total
    print(f"\n{system} QEMU 指令热点")
    print(f"总客机指令={total}，HOT 覆盖=100.00%，可归属内核函数={kernel_pct:.2f}%")
    print("| 函数 | 动态指令数 | 占全部客机指令 | 占已归属内核指令 | TB 执行次数 |")
    print("| --- | ---: | ---: | ---: | ---: |")
    ordered = sorted(
        aggregate.items(), key=lambda item: item[1].instructions, reverse=True
    )
    for name, value in ordered[:top]:
        guest_share = value.instructions * 100 / total
        kernel_share = value.instructions * 100 / symbolized if symbolized else 0
        print(
            f"| {name} | {value.instructions} | {guest_share:.3f}% | "
            f"{kernel_share:.3f}% | {value.blocks} |"
        )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("timing", "profile", "trace"), required=True)
    parser.add_argument("--metadata", type=Path)
    parser.add_argument("--write-metadata", type=Path)
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    parser.add_argument("--systems", choices=("mygo", "linux", "both"), default="both")
    parser.add_argument("--validate-only", action="store_true")
    parser.add_argument("--record-system", choices=("mygo", "linux"))
    parser.add_argument("--mygo-serial", type=Path)
    parser.add_argument("--linux-serial", type=Path)
    parser.add_argument("--mygo-profile", type=Path)
    parser.add_argument("--linux-profile", type=Path)
    parser.add_argument("--mygo-trace", type=Path)
    parser.add_argument("--linux-trace", type=Path)
    parser.add_argument("--mygo-kernel", type=Path)
    parser.add_argument("--mygo-map", type=Path)
    parser.add_argument("--mygo-manifest", type=Path)
    parser.add_argument("--linux-image", type=Path)
    parser.add_argument("--linux-kernel", type=Path, help="Linux vmlinux")
    parser.add_argument("--linux-map", type=Path)
    parser.add_argument("--linux-manifest", type=Path)
    parser.add_argument("--initramfs", type=Path)
    parser.add_argument("--benchmark-elf", type=Path)
    parser.add_argument("--profile-plugin", type=Path)
    parser.add_argument("--trace-plugin", type=Path)
    parser.add_argument("--trace-max-instructions", type=int)
    parser.add_argument("--smp", type=int)
    parser.add_argument("--memory")
    parser.add_argument("--accel")
    parser.add_argument("--timeout-seconds", type=int)
    parser.add_argument("--iterations", type=int)
    parser.add_argument("--repeats", type=int)
    parser.add_argument("--case")
    parser.add_argument("--warmup", type=int)
    parser.add_argument("--table-bits", type=int)
    parser.add_argument("--profile-start-pc")
    parser.add_argument("--profile-stop-pc")
    parser.add_argument("--container-image")
    parser.add_argument("--nm", default="riscv64-linux-gnu-nm")
    parser.add_argument("--objcopy", default="riscv64-linux-gnu-objcopy")
    parser.add_argument("--strip", default="riscv64-linux-gnu-strip")
    parser.add_argument("--cpio", default="cpio")
    parser.add_argument("--top", type=int, default=25)
    return parser


def run(args: argparse.Namespace) -> int:
    repo_root = args.repo_root.resolve(strict=True)
    if args.write_metadata:
        if args.metadata or args.record_system or any(
            (
                args.mygo_serial,
                args.linux_serial,
                args.mygo_profile,
                args.linux_profile,
                args.mygo_trace,
                args.linux_trace,
            )
        ):
            raise ValueError("--write-metadata 不能与结果输入同时使用")
        metadata = build_metadata(args, repo_root)
        write_metadata(args.write_metadata, metadata)
        return 0

    if not args.metadata:
        raise ValueError("校验或报告结果必须提供 --metadata")
    metadata = load_and_verify_metadata(args.metadata, args, repo_root)
    parameters = metadata["parameters"]
    resolved = metadata["_resolved"]

    run_directory = args.metadata.resolve(strict=True).parent
    result_paths = {
        "mygo serial": (args.mygo_serial, run_directory / "mygo.serial.log"),
        "linux serial": (args.linux_serial, run_directory / "linux.serial.log"),
        "mygo profile": (args.mygo_profile, run_directory / "mygo.tcg-profile.txt"),
        "linux profile": (args.linux_profile, run_directory / "linux.tcg-profile.txt"),
        "mygo trace": (args.mygo_trace, run_directory / "mygo.instruction-trace.txt"),
        "linux trace": (args.linux_trace, run_directory / "linux.instruction-trace.txt"),
    }
    for label, (supplied, expected) in result_paths.items():
        if supplied is not None and supplied.resolve(strict=True) != expected.resolve(strict=True):
            raise ValueError(f"{label} 不属于该元数据运行目录")
    recorded_results = metadata["_resolved_results"]
    for system, supplied in (("mygo", args.mygo_serial), ("linux", args.linux_serial)):
        if supplied is not None and system not in recorded_results and args.record_system != system:
            raise ValueError(f"{system} 结果尚未写入运行元数据")

    mygo = parse_serial(args.mygo_serial) if args.mygo_serial else None
    linux = parse_serial(args.linux_serial) if args.linux_serial else None
    if not mygo and not linux:
        raise ValueError("至少提供一份串口日志")
    if mygo:
        validate_mygo_boot_source(mygo)
        validate_expected(mygo, parameters)
    if linux:
        validate_expected(linux, parameters)
    if mygo and linux:
        validate_pair(mygo, linux)

    if args.mode == "timing":
        if any((args.mygo_profile, args.linux_profile, args.mygo_trace, args.linux_trace)):
            raise ValueError("timing 模式不接受跟踪产物")
        if args.record_system:
            selected = mygo if args.record_system == "mygo" else linux
            if selected is None:
                raise ValueError(f"记录 {args.record_system} 时缺少对应串口日志")
            record_result_metadata(
                args.metadata,
                metadata,
                repo_root,
                args.record_system,
                selected.path,
                None,
            )
        if not args.validate_only:
            if not mygo or not linux:
                raise ValueError("延迟对比必须同时提供 Hitoshizuku/Linux 串口日志")
            print_timing(mygo, linux)
        return 0

    if args.mode == "trace":
        if args.mygo_profile or args.linux_profile:
            raise ValueError("trace 模式不接受 TCG profile")
        trace_paths = {"mygo": args.mygo_trace, "linux": args.linux_trace}
        counts: dict[str, int] = {}
        for system, trace_path in trace_paths.items():
            if trace_path is not None:
                counts[system] = parse_instruction_trace(
                    trace_path,
                    parameters["profile_start_pc"],
                    parameters["profile_stop_pc"],
                    parameters["trace_max_instructions"],
                )
        if args.record_system:
            selected = mygo if args.record_system == "mygo" else linux
            if selected is None:
                raise ValueError(f"记录 {args.record_system} 时缺少对应串口日志")
            record_result_metadata(
                args.metadata,
                metadata,
                repo_root,
                args.record_system,
                selected.path,
                trace_paths[args.record_system],
            )
        if args.validate_only:
            return 0
        if not counts:
            raise ValueError("trace 报告至少需要一份指令轨迹")
        for system, count in counts.items():
            print(f"{system} 单次 syscall 动态指令记录={count}")
        return 0

    if args.mygo_trace or args.linux_trace:
        raise ValueError("profile 模式不接受指令轨迹")
    if parameters["smp"] != 1 or parameters["repeats"] != 1 or parameters["case"] == "all":
        raise ValueError("profile 元数据违反单核、单轮、单 case 约束")
    profile_paths = {"mygo": args.mygo_profile, "linux": args.linux_profile}
    for system, profile_path in profile_paths.items():
        if profile_path is not None:
            parse_tcg(
                profile_path,
                parameters["smp"],
                parameters["profile_start_pc"],
                parameters["profile_stop_pc"],
            )
    if args.record_system:
        selected = mygo if args.record_system == "mygo" else linux
        if selected is None:
            raise ValueError(f"记录 {args.record_system} 时缺少对应串口日志")
        record_result_metadata(
            args.metadata,
            metadata,
            repo_root,
            args.record_system,
            selected.path,
            profile_paths[args.record_system],
        )
    if args.validate_only:
        return 0
    if not args.mygo_profile and not args.linux_profile:
        raise ValueError("profile 报告至少需要一份 TCG profile")
    if args.mygo_profile and not mygo:
        raise ValueError("Hitoshizuku profile 缺少对应串口日志")
    if args.linux_profile and not linux:
        raise ValueError("Linux profile 缺少对应串口日志")
    print("\n注意：profile 模式含逐 TB 插桩，仅报告指令计数；延迟对比已禁用。")
    if args.mygo_profile:
        print_hotspots(
            "Hitoshizuku", args.mygo_profile, resolved["mygo_kernel"], args.nm, args.top, parameters
        )
    if args.linux_profile:
        print_hotspots(
            "Linux", args.linux_profile, resolved["linux_vmlinux"], args.nm, args.top, parameters
        )
    return 0


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    if args.top < 1:
        parser.error("--top 必须为正数")
    try:
        return run(args)
    except (
        KeyError,
        OSError,
        TypeError,
        UnicodeError,
        ValueError,
        subprocess.SubprocessError,
    ) as error:
        print(f"syscall bench report: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
