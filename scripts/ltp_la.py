#!/usr/bin/env python3
"""LoongArch64 LTP 全量测试编排器。"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
import os
import re
import selectors
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Iterable, Sequence


DEFAULT_DOCKER_IMAGE = os.environ.get("HITOSHIZUKU_LTP_IMAGE", "")
DEFAULT_GROUPS = ("default", "network")
DEFAULT_OUTPUT = Path("build/ltp-loongarch64")
DEFAULT_KERNEL = Path("kernel-la")
DEFAULT_TEST_IMAGE = Path("build/sdcard-la.img")
MARKER_PREFIX = "@@LTP\t"
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
STATUS_RE = re.compile(r"(?<![A-Z])(TPASS|TFAIL|TBROK|TCONF|TWARN)(?![A-Z])")
SUMMARY_RE = re.compile(
    r"^\s*(passed|failed|broken|skipped|warnings)\s+([0-9]+)\s*$",
    re.IGNORECASE | re.MULTILINE,
)


class LtpError(RuntimeError):
    """表示编排器无法安全继续的错误。"""


@dataclasses.dataclass(frozen=True)
class RuntestCase:
    """官方 runtest 文件中的一条原始场景记录。"""

    index: int
    tag: str
    command: str


@dataclasses.dataclass
class ParsedSerial:
    """一次 QEMU 串口输出中解析出的结构化结果。"""

    cases: list[dict[str, Any]]
    starts_without_end: list[dict[str, str]]
    shard_end: dict[str, str] | None
    shard_abort: dict[str, str] | None
    fatal: dict[str, str] | None
    runner_start: dict[str, str] | None


@dataclasses.dataclass
class QemuResult:
    """一次 QEMU 进程执行结果。"""

    return_code: int
    timed_out: bool
    timeout_kind: str | None
    elapsed: float
    log_path: Path
    parsed: ParsedSerial


def utc_now() -> str:
    """返回带时区的 UTC 时间。"""

    return dt.datetime.now(dt.timezone.utc).isoformat()


def parse_runtest(text: str) -> list[RuntestCase]:
    """按 LTP runtest 语法保留有效行，并分配稳定的零基索引。"""

    cases: list[RuntestCase] = []
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        tag = line.split(None, 1)[0]
        cases.append(RuntestCase(index=len(cases), tag=tag, command=line))
    return cases


def parse_scenario_group(text: str) -> list[str]:
    """解析 scenario_groups 文件中的场景名称。"""

    scenarios: list[str] = []
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if line and not line.startswith("#"):
            scenarios.append(line.split()[0])
    return scenarios


def parse_marker_line(line: str) -> tuple[str, dict[str, str]] | None:
    """解析 guest runner 输出的一行结构化 marker。"""

    clean = ANSI_RE.sub("", line.rstrip("\r\n"))
    offset = clean.find(MARKER_PREFIX)
    if offset < 0:
        return None
    fields = clean[offset + len(MARKER_PREFIX) :].split("\t")
    if not fields or not fields[0]:
        return None
    values: dict[str, str] = {}
    for field in fields[1:]:
        if "=" in field:
            key, value = field.split("=", 1)
            values[key] = value
    return fields[0], values


def status_counts(text: str) -> dict[str, int]:
    """从用例输出中提取 LTP 状态；Summary 存在时优先使用其最后一组值。"""

    clean = ANSI_RE.sub("", text)
    token_counts = Counter(STATUS_RE.findall(clean))
    summaries: list[dict[str, int]] = []
    current: dict[str, int] = {}
    for line in clean.splitlines():
        if line.strip() == "Summary:":
            if current:
                summaries.append(current)
            current = {}
            continue
        match = SUMMARY_RE.match(line)
        if match and current is not None:
            current[match.group(1).lower()] = int(match.group(2))
    if current:
        summaries.append(current)

    if summaries:
        summary = summaries[-1]
        return {
            "passed": summary.get("passed", 0),
            "failed": summary.get("failed", 0),
            "broken": summary.get("broken", 0),
            "skipped": summary.get("skipped", 0),
            "warnings": summary.get("warnings", 0),
        }
    return {
        "passed": token_counts["TPASS"],
        "failed": token_counts["TFAIL"],
        "broken": token_counts["TBROK"],
        "skipped": token_counts["TCONF"],
        "warnings": token_counts["TWARN"],
    }


def classify_case(fields: dict[str, str], output: str) -> tuple[str, dict[str, int]]:
    """将 guest 结果和 LTP 状态归并为稳定的宿主分类。"""

    guest_result = fields.get("result", "")
    if guest_result == "skip":
        return "static-skip", status_counts(output)
    if guest_result == "timeout":
        return "timeout", status_counts(output)

    counts = status_counts(output)
    if "ltp_stat" in fields:
        try:
            ltp_stat = int(fields["ltp_stat"])
        except ValueError:
            ltp_stat = 255
        if fields.get("termination", "exited") != "exited":
            counts["broken"] = max(counts["broken"], 1)
            return "broken", counts
        if ltp_stat == 0:
            counts["passed"] = max(counts["passed"], 1)
            if counts["warnings"]:
                return "pass-with-warning", counts
            return "pass", counts
        if ltp_stat == 32:
            counts["skipped"] = max(counts["skipped"], 1)
            return "tconf", counts
        if ltp_stat < 0 or ltp_stat > 63:
            return "harness-error", counts
        if ltp_stat & 2:
            counts["broken"] = max(counts["broken"], 1)
            return "broken", counts
        if ltp_stat & 1:
            counts["failed"] = max(counts["failed"], 1)
            return "fail", counts
        if ltp_stat & 4:
            counts["warnings"] = max(counts["warnings"], 1)
            return "warning", counts
        return "harness-error", counts

    try:
        exit_code = int(fields.get("exit", "0"))
    except ValueError:
        exit_code = 255
    if counts["failed"]:
        return "fail", counts
    if counts["broken"]:
        return "broken", counts
    if exit_code != 0:
        return "harness-error", counts
    if counts["passed"]:
        if counts["warnings"]:
            return "pass-with-warning", counts
        return "pass", counts
    if counts["skipped"]:
        return "tconf", counts
    if counts["warnings"]:
        return "warning", counts
    return "unknown", counts


def parse_serial(text: str) -> ParsedSerial:
    """把一次串口日志还原为每条 LTP 场景记录的结果。"""

    cases: list[dict[str, Any]] = []
    active: dict[tuple[str, str, str], tuple[dict[str, str], list[str]]] = {}
    shard_end: dict[str, str] | None = None
    shard_abort: dict[str, str] | None = None
    fatal: dict[str, str] | None = None
    runner_start: dict[str, str] | None = None

    for line in text.splitlines(keepends=True):
        marker = parse_marker_line(line)
        if marker is None:
            for _fields, output in active.values():
                output.append(line)
            continue
        event, fields = marker
        if event == "runner_start":
            runner_start = fields
        elif event == "case_start":
            key = (fields.get("group", ""), fields.get("scenario", ""), fields.get("index", ""))
            active[key] = (dict(fields), [])
        elif event == "case_skip":
            key = (fields.get("group", ""), fields.get("scenario", ""), fields.get("index", ""))
            if key in active:
                active[key][0].update(fields)
        elif event == "case_end":
            key = (fields.get("group", ""), fields.get("scenario", ""), fields.get("index", ""))
            start_fields, output_lines = active.pop(key, (dict(fields), []))
            start_fields.update(fields)
            output = "".join(output_lines)
            classification, counts = classify_case(start_fields, output)
            try:
                index = int(start_fields.get("index", "-1"))
            except ValueError:
                index = -1
            cases.append(
                {
                    **start_fields,
                    "index": index,
                    "classification": classification,
                    "status_counts": counts,
                }
            )
        elif event == "shard_end":
            shard_end = fields
        elif event == "shard_abort":
            shard_abort = fields
        elif event == "fatal":
            fatal = fields

    starts_without_end = [fields for fields, _output in active.values()]
    cases.sort(key=lambda item: int(item.get("index", -1)))
    return ParsedSerial(
        cases=cases,
        starts_without_end=starts_without_end,
        shard_end=shard_end,
        shard_abort=shard_abort,
        fatal=fatal,
        runner_start=runner_start,
    )


def first_missing_index(total: int, completed: Iterable[int]) -> int:
    """返回从零开始的第一个未完成索引。"""

    done = {index for index in completed if 0 <= index < total}
    for index in range(total):
        if index not in done:
            return index
    return total


def sha256_file(path: Path, chunk_size: int = 8 * 1024 * 1024) -> str:
    """流式计算大镜像的 SHA-256。"""

    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(chunk_size):
            digest.update(chunk)
    return digest.hexdigest()


def atomic_write_json(path: Path, value: Any) -> None:
    """通过同目录临时文件原子更新状态。"""

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def append_jsonl(path: Path, value: dict[str, Any]) -> None:
    """追加一条 JSONL 记录并立即刷盘。"""

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as target:
        target.write(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n")
        target.flush()
        os.fsync(target.fileno())


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    """读取允许末尾残缺行的 JSONL 文件。"""

    if not path.exists():
        return []
    records: list[dict[str, Any]] = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise LtpError(f"{path}:{number}: JSONL 损坏: {error}") from error
        if isinstance(value, dict):
            records.append(value)
    return records


class Journal:
    """保存检查、执行、异常和修复的追加式审计记录。"""

    def __init__(self, path: Path, root: Path) -> None:
        self.path = path
        self.root = root

    def record(self, event: str, message: str, **fields: Any) -> None:
        record: dict[str, Any] = {
            "time": utc_now(),
            "event": event,
            "message": message,
            "git_head": git_head(self.root),
        }
        record.update(fields)
        append_jsonl(self.path, record)


def git_head(root: Path) -> str:
    """读取当前提交，不让审计功能依赖干净工作树。"""

    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.stdout.strip() if result.returncode == 0 else "<unknown>"


class ImageReader:
    """通过 debugfs 只读提取 ext4 测试镜像中的官方清单。"""

    def __init__(self, root: Path, image: Path, docker_image: str) -> None:
        self.root = root
        self.image = image
        self.docker_image = docker_image

    def cat(self, absolute_path: str) -> str:
        if shutil.which("debugfs"):
            command = ["debugfs", "-R", f"cat {absolute_path}", str(self.image)]
            cwd = self.root
        else:
            relative_image = self.image.relative_to(self.root)
            command = [
                "docker",
                "run",
                "--rm",
                "-v",
                f"{self.root}:/work",
                "-w",
                "/work",
                self.docker_image,
                "debugfs",
                "-R",
                f"cat {absolute_path}",
                str(relative_image),
            ]
            cwd = self.root
        result = subprocess.run(
            command,
            cwd=cwd,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode != 0:
            raise LtpError(f"读取镜像 {absolute_path} 失败: {result.stderr.strip()}")
        return result.stdout


def build_inventory(reader: ImageReader, groups: Sequence[str], image: Path) -> dict[str, Any]:
    """从镜像构建官方场景清单，不推测或遍历测试二进制。"""

    group_records: dict[str, Any] = {}
    scenario_cache: dict[str, list[RuntestCase]] = {}
    for group in groups:
        scenarios = parse_scenario_group(reader.cat(f"/glibc/ltp/scenario_groups/{group}"))
        entries: list[dict[str, Any]] = []
        total = 0
        for scenario in scenarios:
            if scenario not in scenario_cache:
                scenario_cache[scenario] = parse_runtest(reader.cat(f"/glibc/ltp/runtest/{scenario}"))
            count = len(scenario_cache[scenario])
            entries.append({"name": scenario, "count": count})
            total += count
        group_records[group] = {"scenarios": entries, "total": total}

    return {
        "generated_at": utc_now(),
        "image": str(image),
        "groups": group_records,
        "runtest": {
            scenario: [dataclasses.asdict(case) for case in cases]
            for scenario, cases in sorted(scenario_cache.items())
        },
    }


def ensure_sparse(path: Path, size: int) -> None:
    """创建或校验固定大小的稀疏原始磁盘。"""

    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.stat().st_size == size:
        return
    if path.exists():
        path.unlink()
    with path.open("wb") as target:
        target.truncate(size)


def ensure_work_disks(output: Path, docker_image: str, root: Path) -> dict[str, Path]:
    """准备一个 ext4 工作盘和两个可破坏的块设备基盘。"""

    disks = output / "disks"
    work = disks / "work-ext4.img"
    test = disks / "test-device.img"
    big = disks / "big-device.img"
    work_size = 16 * 1024**3
    ensure_sparse(test, 8 * 1024**3)
    ensure_sparse(big, 16 * 1024**3)
    if not work.exists() or work.stat().st_size != work_size:
        ensure_sparse(work, work_size)
        if shutil.which("mkfs.ext4"):
            command = ["mkfs.ext4", "-F", "-q", str(work)]
        else:
            relative = work.relative_to(root)
            command = [
                "docker",
                "run",
                "--rm",
                "-v",
                f"{root}:/work",
                "-w",
                "/work",
                docker_image,
                "mkfs.ext4",
                "-F",
                "-q",
                str(relative),
            ]
        subprocess.run(command, cwd=root, check=True)
    return {"work": work, "test": test, "big": big}


def write_work_config(
    work: Path,
    fields: dict[str, Any],
    output: Path,
    docker_image: str,
    root: Path,
) -> None:
    """把启动参数写入工作盘，绕过 LoongArch64 直启不传递 -append 的限制。"""

    config_dir = output / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    source = config_dir / "ltp.conf"
    lines: list[str] = []
    for key, value in fields.items():
        text = str(value)
        if "\n" in text or "\r" in text or "=" in key:
            raise LtpError(f"工作盘配置字段无效: {key}={text!r}")
        lines.append(f"{key}={text}")
    source.write_text("\n".join(lines) + "\n", encoding="utf-8")

    if shutil.which("debugfs"):
        base = ["debugfs", "-w"]
        image_arg = str(work)
        source_arg = str(source)
    else:
        base = [
            "docker",
            "run",
            "--rm",
            "-v",
            f"{root}:/work",
            "-w",
            "/work",
            docker_image,
            "debugfs",
            "-w",
        ]
        image_arg = relative_to_root(work, root)
        source_arg = relative_to_root(source, root)

    subprocess.run(
        [*base, "-R", "rm /ltp.conf", image_arg],
        cwd=root,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    result = subprocess.run(
        [*base, "-R", f"write {source_arg} /ltp.conf", image_arg],
        cwd=root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0 or "Allocated inode" not in result.stdout:
        raise LtpError(
            "写入 LTP 工作盘配置失败: "
            f"stdout={result.stdout.strip()!r} stderr={result.stderr.strip()!r}"
        )


def relative_to_root(path: Path, root: Path) -> str:
    """把 QEMU 输入限制在映射到容器的仓库目录内。"""

    try:
        return str(path.resolve().relative_to(root.resolve()))
    except ValueError as error:
        raise LtpError(f"路径必须位于仓库内: {path}") from error


def qemu_command(
    *,
    root: Path,
    docker_image: str,
    container_name: str,
    kernel: Path,
    image: Path,
    disks: dict[str, Path],
    cmdline: str,
    memory: str,
    cpus: int,
) -> list[str]:
    """生成固定、可复现的 Docker/QEMU 命令。"""

    kernel_rel = relative_to_root(kernel, root)
    image_rel = relative_to_root(image, root)
    work_rel = relative_to_root(disks["work"], root)
    test_rel = relative_to_root(disks["test"], root)
    big_rel = relative_to_root(disks["big"], root)
    return [
        "docker",
        "run",
        "--rm",
        "--name",
        container_name,
        "-v",
        f"{root}:/work",
        "-w",
        "/work",
        docker_image,
        "qemu-system-loongarch64",
        "-kernel",
        kernel_rel,
        "-m",
        memory,
        "-smp",
        str(cpus),
        "-nographic",
        "-monitor",
        "none",
        "-no-reboot",
        "-rtc",
        "base=utc",
        "-append",
        cmdline,
        "-drive",
        f"file={image_rel},if=none,format=raw,id=ltp,snapshot=on",
        "-device",
        "virtio-blk-pci,drive=ltp",
        "-drive",
        f"file={work_rel},if=none,format=raw,id=work,snapshot=on",
        "-device",
        "virtio-blk-pci,drive=work",
        "-drive",
        f"file={test_rel},if=none,format=raw,id=test,snapshot=on",
        "-device",
        "virtio-blk-pci,drive=test",
        "-drive",
        f"file={big_rel},if=none,format=raw,id=big,snapshot=on",
        "-device",
        "virtio-blk-pci,drive=big",
        "-netdev",
        "user,id=net0",
        "-device",
        "virtio-net-pci,netdev=net0",
    ]


def terminate_process(process: subprocess.Popen[str], container_name: str) -> None:
    """终止 QEMU 进程组，并兜底清理仍在运行的容器。"""

    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)
    subprocess.run(
        ["docker", "rm", "-f", container_name],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def run_qemu(
    command: Sequence[str],
    container_name: str,
    log_path: Path,
    hard_timeout: float,
    idle_timeout: float,
    verbose: bool,
) -> QemuResult:
    """流式保存串口输出，并对内核卡死和整分片超时分别设限。"""

    log_path.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    last_output = started
    timed_out = False
    timeout_kind: str | None = None
    process = subprocess.Popen(
        list(command),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        start_new_session=True,
    )
    assert process.stdout is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)

    try:
        with log_path.open("w", encoding="utf-8", errors="replace") as log:
            while True:
                now = time.monotonic()
                if now - started > hard_timeout:
                    timed_out = True
                    timeout_kind = "hard"
                    break
                if now - last_output > idle_timeout:
                    timed_out = True
                    timeout_kind = "idle"
                    break

                events = selector.select(timeout=1.0)
                for key, _mask in events:
                    line = key.fileobj.readline()
                    if line:
                        last_output = time.monotonic()
                        log.write(line)
                        log.flush()
                        if verbose or MARKER_PREFIX in ANSI_RE.sub("", line) or line.startswith("[init]"):
                            print(line, end="", flush=True)
                if process.poll() is not None:
                    remainder = process.stdout.read()
                    if remainder:
                        log.write(remainder)
                        if verbose:
                            print(remainder, end="", flush=True)
                    break
    finally:
        selector.close()
        if timed_out or process.poll() is None:
            terminate_process(process, container_name)

    return_code = process.returncode if process.returncode is not None else -signal.SIGKILL
    elapsed = time.monotonic() - started
    text = log_path.read_text(encoding="utf-8", errors="replace")
    return QemuResult(
        return_code=return_code,
        timed_out=timed_out,
        timeout_kind=timeout_kind,
        elapsed=elapsed,
        log_path=log_path,
        parsed=parse_serial(text),
    )


def load_inventory(path: Path) -> dict[str, Any]:
    """读取并检查 inventory.json 的基本结构。"""

    if not path.exists():
        raise LtpError(f"清单不存在，请先运行 inventory: {path}")
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or "groups" not in value or "runtest" not in value:
        raise LtpError(f"清单结构无效: {path}")
    return value


def result_key(record: dict[str, Any]) -> tuple[str, str, int]:
    """返回一条结果在正式测试活动中的唯一键。"""

    return str(record.get("group", "")), str(record.get("scenario", "")), int(record.get("index", -1))


def completed_by_scenario(records: Iterable[dict[str, Any]]) -> dict[tuple[str, str], set[int]]:
    """按测试组和场景汇总已有结果，供断点恢复使用。"""

    completed: dict[tuple[str, str], set[int]] = defaultdict(set)
    for record in records:
        group, scenario, index = result_key(record)
        if group and scenario and index >= 0:
            completed[(group, scenario)].add(index)
    return completed


def normalized_case_result(case: dict[str, Any], run_id: str, log_path: Path) -> dict[str, Any]:
    """补齐宿主侧可追溯字段。"""

    return {
        **case,
        "run_id": run_id,
        "recorded_at": utc_now(),
        "serial_log": str(log_path),
        "synthetic": False,
    }


def synthetic_failure(
    *,
    run_id: str,
    group: str,
    scenario: str,
    index: int,
    tag: str,
    classification: str,
    reason: str,
    log_path: Path,
) -> dict[str, Any]:
    """在 VM 无法产生 case_end 时记录明确的基础设施或内核故障。"""

    return {
        "run_id": run_id,
        "recorded_at": utc_now(),
        "group": group,
        "scenario": scenario,
        "index": index,
        "tag": tag,
        "classification": classification,
        "reason": reason,
        "status_counts": {"passed": 0, "failed": 0, "broken": 0, "skipped": 0, "warnings": 0},
        "serial_log": str(log_path),
        "synthetic": True,
    }


def append_unique_results(path: Path, records: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    """只追加尚未记录的结果，避免重试和恢复制造重复项。"""

    existing = {result_key(record) for record in load_jsonl(path)}
    appended: list[dict[str, Any]] = []
    for record in records:
        key = result_key(record)
        if key in existing:
            continue
        append_jsonl(path, record)
        existing.add(key)
        appended.append(record)
    return appended


def scenario_cases(inventory: dict[str, Any], scenario: str) -> list[dict[str, Any]]:
    """返回指定场景的稳定清单。"""

    raw = inventory["runtest"].get(scenario)
    if not isinstance(raw, list):
        raise LtpError(f"清单缺少场景: {scenario}")
    return raw


def format_cmdline(fields: dict[str, Any]) -> str:
    """生成不含空白值的内核命令行。"""

    values: list[str] = []
    for key, value in fields.items():
        text = str(value)
        if any(character.isspace() for character in text):
            raise LtpError(f"内核命令行字段不能包含空白: {key}={text!r}")
        values.append(f"{key}={text}")
    return " ".join(values)


def run_one_shard(
    *,
    args: argparse.Namespace,
    root: Path,
    output: Path,
    disks: dict[str, Path],
    run_id: str,
    group: str,
    scenario: str,
    start: int,
    count: int,
    only: str = "",
) -> QemuResult:
    """启动一个测试分片。"""

    serial_dir = output / "serial"
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    log_path = serial_dir / f"{group}-{scenario}-{start:05d}-{stamp}-{uuid.uuid4().hex[:6]}.log"
    container_name = f"ltp-la-{os.getpid()}-{uuid.uuid4().hex[:10]}"
    config_fields = {
        "ltp_run": 1,
        "ltp_run_id": run_id,
        "ltp_group": group,
        "ltp_scenario": scenario,
        "ltp_start": start,
        "ltp_count": count,
        "ltp_only": only,
        "ltp_case_timeout": args.case_timeout,
        "ltp_kill_grace": args.kill_grace,
        "ltp_timeout_mul": args.timeout_mul,
    }
    cmdline = format_cmdline(config_fields)
    write_work_config(
        disks["work"],
        config_fields,
        output,
        args.docker_image,
        root,
    )
    command = qemu_command(
        root=root,
        docker_image=args.docker_image,
        container_name=container_name,
        kernel=args.kernel,
        image=args.image,
        disks=disks,
        cmdline=cmdline,
        memory=args.memory,
        cpus=args.cpus,
    )
    if args.print_command:
        print(" ".join(shlex.quote(part) for part in command))
    hard_timeout = args.boot_timeout + count * (args.case_timeout + args.kill_grace + 15)
    idle_timeout = args.case_timeout + args.kill_grace + args.boot_timeout
    return run_qemu(
        command,
        container_name,
        log_path,
        hard_timeout=hard_timeout,
        idle_timeout=idle_timeout,
        verbose=args.verbose,
    )


def validate_common_paths(args: argparse.Namespace, root: Path) -> None:
    """在启动长测试前拒绝明显错误的输入。"""

    if not args.docker_image:
        raise LtpError(
            "请通过 --docker-image 或 HITOSHIZUKU_LTP_IMAGE 提供运行环境镜像"
        )
    args.kernel = (root / args.kernel).resolve() if not args.kernel.is_absolute() else args.kernel.resolve()
    args.image = (root / args.image).resolve() if not args.image.is_absolute() else args.image.resolve()
    args.output = (root / args.output).resolve() if not args.output.is_absolute() else args.output.resolve()
    if not args.kernel.is_file():
        raise LtpError(f"内核镜像不存在: {args.kernel}")
    if not args.image.is_file():
        raise LtpError(f"LTP 镜像不存在: {args.image}")
    if shutil.which("docker") is None:
        raise LtpError("找不到 docker")


def inventory_command(args: argparse.Namespace, root: Path) -> int:
    """生成并打印场景清单。"""

    if not args.docker_image:
        raise LtpError(
            "请通过 --docker-image 或 HITOSHIZUKU_LTP_IMAGE 提供运行环境镜像"
        )
    image = (root / args.image).resolve() if not args.image.is_absolute() else args.image.resolve()
    output = (root / args.output).resolve() if not args.output.is_absolute() else args.output.resolve()
    if not image.is_file():
        raise LtpError(f"LTP 镜像不存在: {image}")
    reader = ImageReader(root, image, args.docker_image)
    inventory = build_inventory(reader, args.groups, image)
    path = output / "inventory.json"
    atomic_write_json(path, inventory)
    journal = Journal(output / "journal.jsonl", root)
    for group in args.groups:
        data = inventory["groups"][group]
        print(f"{group}: scenarios={len(data['scenarios'])} records={data['total']}")
    print(f"inventory: {path}")
    journal.record(
        "inventory",
        "从官方 scenario_groups 与 runtest 文件重建测试清单",
        groups=list(args.groups),
        totals={group: inventory["groups"][group]["total"] for group in args.groups},
        image=str(image),
    )
    return 0


def initial_state(
    *,
    run_id: str,
    args: argparse.Namespace,
    image_hash: str,
    kernel_hash: str,
) -> dict[str, Any]:
    """创建可恢复测试活动的固定参数快照。"""

    return {
        "version": 1,
        "run_id": run_id,
        "created_at": utc_now(),
        "updated_at": utc_now(),
        "status": "running",
        "groups": list(args.groups),
        "image": str(args.image),
        "image_sha256_before": image_hash,
        "kernel": str(args.kernel),
        "kernel_sha256": kernel_hash,
        "kernel_history": [
            {
                "sha256": kernel_hash,
                "accepted_at": utc_now(),
                "reason": "campaign-start",
            }
        ],
        "docker_image": args.docker_image,
        "shard_size": args.shard_size,
        "case_timeout": args.case_timeout,
        "kill_grace": args.kill_grace,
        "timeout_mul": args.timeout_mul,
        "boot_timeout": args.boot_timeout,
        "retries": args.retries,
        "memory": args.memory,
        "cpus": args.cpus,
        "retry_counts": {},
    }


def restore_args_from_state(args: argparse.Namespace, state: dict[str, Any]) -> None:
    """恢复时强制沿用原活动的执行参数。"""

    args.groups = tuple(state["groups"])
    args.image = Path(state["image"])
    args.kernel = Path(state["kernel"])
    args.docker_image = state["docker_image"]
    args.shard_size = int(state["shard_size"])
    args.case_timeout = int(state["case_timeout"])
    args.kill_grace = int(state["kill_grace"])
    args.timeout_mul = int(state["timeout_mul"])
    args.boot_timeout = int(state["boot_timeout"])
    args.retries = int(state["retries"])
    args.memory = str(state["memory"])
    args.cpus = int(state["cpus"])


def execute_campaign(args: argparse.Namespace, root: Path, resume: bool) -> int:
    """运行或恢复完整的 default/network 测试活动。"""

    output = (root / args.output).resolve() if not args.output.is_absolute() else args.output.resolve()
    state_path = output / "state.json"
    results_path = output / "results.jsonl"
    journal = Journal(output / "journal.jsonl", root)

    if resume:
        if not state_path.exists():
            raise LtpError(f"没有可恢复状态: {state_path}")
        state = json.loads(state_path.read_text(encoding="utf-8"))
        restore_args_from_state(args, state)
        validate_common_paths(args, root)
        run_id = str(state["run_id"])
        image_hash = sha256_file(args.image)
        if image_hash != state["image_sha256_before"]:
            raise LtpError("LTP 原始镜像哈希已变化，拒绝恢复")
        current_kernel_hash = sha256_file(args.kernel)
        if current_kernel_hash != state["kernel_sha256"]:
            if not args.accept_kernel_change:
                raise LtpError(
                    "kernel-la 与活动当前记录不同；修复后恢复测试时请显式传入 "
                    "--accept-kernel-change"
                )
            previous_kernel_hash = str(state["kernel_sha256"])
            history = state.setdefault(
                "kernel_history",
                [
                    {
                        "sha256": previous_kernel_hash,
                        "accepted_at": state.get("created_at", utc_now()),
                        "reason": "campaign-start",
                    }
                ],
            )
            history.append(
                {
                    "sha256": current_kernel_hash,
                    "accepted_at": utc_now(),
                    "reason": "resume-after-kernel-fix",
                }
            )
            state["kernel_sha256"] = current_kernel_hash
            state["updated_at"] = utc_now()
            atomic_write_json(state_path, state)
            journal.record(
                "campaign-kernel-change",
                "接受修复后的 LoongArch64 内核镜像并从断点继续",
                run_id=run_id,
                previous_kernel_sha256=previous_kernel_hash,
                kernel_sha256=current_kernel_hash,
            )
        journal.record("campaign-resume", "恢复 LoongArch64 LTP 测试活动", run_id=run_id)
    else:
        validate_common_paths(args, root)
        output.mkdir(parents=True, exist_ok=True)
        if state_path.exists() and not args.force:
            old = json.loads(state_path.read_text(encoding="utf-8"))
            if old.get("status") == "running":
                raise LtpError("已有未结束活动；请使用 resume，或使用 run --force 明确重建")
        if args.force:
            for path in (results_path, state_path):
                path.unlink(missing_ok=True)
        run_id = dt.datetime.now().strftime("la-%Y%m%d-%H%M%S")
        image_hash = sha256_file(args.image)
        state = initial_state(
            run_id=run_id,
            args=args,
            image_hash=image_hash,
            kernel_hash=sha256_file(args.kernel),
        )
        atomic_write_json(state_path, state)
        journal.record(
            "campaign-start",
            "开始 LoongArch64 LTP 官方场景全量测试",
            run_id=run_id,
            groups=list(args.groups),
            image_sha256=image_hash,
            kernel_sha256=state["kernel_sha256"],
        )

    inventory_path = output / "inventory.json"
    if not resume or not inventory_path.exists():
        reader = ImageReader(root, args.image, args.docker_image)
        atomic_write_json(inventory_path, build_inventory(reader, args.groups, args.image))
    inventory = load_inventory(inventory_path)
    missing_groups = [group for group in args.groups if group not in inventory["groups"]]
    if missing_groups:
        reader = ImageReader(root, args.image, args.docker_image)
        inventory = build_inventory(reader, args.groups, args.image)
        atomic_write_json(inventory_path, inventory)

    disks = ensure_work_disks(output, args.docker_image, root)
    retry_counts: dict[str, int] = state.setdefault("retry_counts", {})
    isolation_counts: dict[str, int] = {}

    for group in args.groups:
        scenarios = inventory["groups"][group]["scenarios"]
        for scenario_info in scenarios:
            scenario = str(scenario_info["name"])
            cases = scenario_cases(inventory, scenario)
            while True:
                records = load_jsonl(results_path)
                completed = completed_by_scenario(records)
                start = first_missing_index(len(cases), completed[(group, scenario)])
                if start >= len(cases):
                    break
                isolation_key = f"{group}/{scenario}/{start}"
                count = min(isolation_counts.get(isolation_key, args.shard_size), len(cases) - start)
                print(f"[ltp-la] {group}/{scenario}: {start}..{start + count - 1} / {len(cases)}")
                journal.record(
                    "shard-start",
                    "启动 LTP 场景分片",
                    run_id=run_id,
                    group=group,
                    scenario=scenario,
                    start=start,
                    count=count,
                )
                qemu = run_one_shard(
                    args=args,
                    root=root,
                    output=output,
                    disks=disks,
                    run_id=run_id,
                    group=group,
                    scenario=scenario,
                    start=start,
                    count=count,
                )
                normalized = [normalized_case_result(case, run_id, qemu.log_path) for case in qemu.parsed.cases]
                appended = append_unique_results(results_path, normalized)
                for record in appended:
                    journal.record(
                        "case-result",
                        "记录 LTP 用例结果",
                        run_id=run_id,
                        group=group,
                        scenario=scenario,
                        index=record["index"],
                        tag=record.get("tag", ""),
                        classification=record["classification"],
                        serial_log=str(qemu.log_path),
                    )

                journal.record(
                    "shard-end",
                    "LTP 场景分片结束",
                    run_id=run_id,
                    group=group,
                    scenario=scenario,
                    start=start,
                    parsed=len(qemu.parsed.cases),
                    return_code=qemu.return_code,
                    timed_out=qemu.timed_out,
                    timeout_kind=qemu.timeout_kind,
                    fatal=qemu.parsed.fatal,
                    serial_log=str(qemu.log_path),
                )

                if qemu.parsed.fatal:
                    state["status"] = "failed"
                    state["failure"] = {"kind": "guest-fatal", **qemu.parsed.fatal}
                    state["updated_at"] = utc_now()
                    atomic_write_json(state_path, state)
                    raise LtpError(f"guest runner fatal: {qemu.parsed.fatal}")

                after = completed_by_scenario(load_jsonl(results_path))[(group, scenario)]
                next_index = first_missing_index(len(cases), after)
                if next_index > start:
                    retry_counts.pop(f"{group}/{scenario}/{start}", None)
                    isolation_counts.pop(isolation_key, None)
                    state["updated_at"] = utc_now()
                    atomic_write_json(state_path, state)
                    continue

                retry_key = f"{group}/{scenario}/{start}"
                if count > 1:
                    reduced_count = max(1, count // 2)
                    isolation_counts[isolation_key] = reduced_count
                    retry_counts.pop(retry_key, None)
                    state["updated_at"] = utc_now()
                    atomic_write_json(state_path, state)
                    journal.record(
                        "shard-isolate",
                        "批量分片无结果，缩小范围以定位真实阻塞用例",
                        run_id=run_id,
                        group=group,
                        scenario=scenario,
                        index=start,
                        previous_count=count,
                        next_count=reduced_count,
                        timed_out=qemu.timed_out,
                        timeout_kind=qemu.timeout_kind,
                        serial_log=str(qemu.log_path),
                    )
                    continue

                attempts = retry_counts.get(retry_key, 0) + 1
                retry_counts[retry_key] = attempts
                state["updated_at"] = utc_now()
                atomic_write_json(state_path, state)
                journal.record(
                    "shard-no-progress",
                    "分片未产生当前索引结果",
                    run_id=run_id,
                    group=group,
                    scenario=scenario,
                    index=start,
                    attempt=attempts,
                    return_code=qemu.return_code,
                    timeout_kind=qemu.timeout_kind,
                    starts_without_end=qemu.parsed.starts_without_end,
                    serial_log=str(qemu.log_path),
                )
                if attempts <= args.retries:
                    continue

                started_case = next(
                    (
                        entry
                        for entry in qemu.parsed.starts_without_end
                        if entry.get("scenario") == scenario and entry.get("index") == str(start)
                    ),
                    None,
                )
                runner_reached_guest = qemu.parsed.runner_start is not None
                if started_case or (qemu.timed_out and runner_reached_guest):
                    failure = synthetic_failure(
                        run_id=run_id,
                        group=group,
                        scenario=scenario,
                        index=start,
                        tag=str(cases[start]["tag"]),
                        classification="kernel-hang",
                        reason=f"连续 {attempts} 次未产生 case_end",
                        log_path=qemu.log_path,
                    )
                    append_unique_results(results_path, [failure])
                    journal.record(
                        "case-kernel-hang",
                        "单例重试耗尽，记录内核卡死并继续后续索引",
                        run_id=run_id,
                        group=group,
                        scenario=scenario,
                        index=start,
                        tag=cases[start]["tag"],
                        attempts=attempts,
                        serial_log=str(qemu.log_path),
                    )
                    continue

                state["status"] = "failed"
                state["failure"] = {
                    "kind": "boot-or-infrastructure",
                    "group": group,
                    "scenario": scenario,
                    "index": start,
                    "attempts": attempts,
                    "serial_log": str(qemu.log_path),
                }
                atomic_write_json(state_path, state)
                raise LtpError("QEMU/runner 连续启动失败，未把基础设施故障误记为测试结果")

    final_hash = sha256_file(args.image)
    state["image_sha256_after"] = final_hash
    state["updated_at"] = utc_now()
    if final_hash != state["image_sha256_before"]:
        state["status"] = "failed"
        atomic_write_json(state_path, state)
        journal.record(
            "image-integrity-failure",
            "LTP 原始镜像在测试后发生变化",
            run_id=run_id,
            before=state["image_sha256_before"],
            after=final_hash,
        )
        raise LtpError("QEMU 快照保护失败：LTP 原始镜像哈希发生变化")

    state["status"] = "complete"
    atomic_write_json(state_path, state)
    journal.record(
        "campaign-complete",
        "LoongArch64 LTP 官方场景测试活动完成",
        run_id=run_id,
        result_count=len(load_jsonl(results_path)),
        image_sha256=final_hash,
    )
    report_command(args, root)
    return 0


def case_command(args: argparse.Namespace, root: Path) -> int:
    """在独立 VM 中复现一个指定 tag。"""

    validate_common_paths(args, root)
    output = args.output
    inventory_path = output / "inventory.json"
    if not inventory_path.exists():
        reader = ImageReader(root, args.image, args.docker_image)
        atomic_write_json(inventory_path, build_inventory(reader, DEFAULT_GROUPS, args.image))
    inventory = load_inventory(inventory_path)
    cases = scenario_cases(inventory, args.scenario)
    matches = [case for case in cases if case["tag"] == args.tag]
    if not matches:
        raise LtpError(f"{args.scenario} 中不存在 tag: {args.tag}")
    case = matches[0]
    disks = ensure_work_disks(output, args.docker_image, root)
    run_id = f"case-{dt.datetime.now().strftime('%Y%m%d-%H%M%S')}"
    qemu = run_one_shard(
        args=args,
        root=root,
        output=output,
        disks=disks,
        run_id=run_id,
        group=args.group,
        scenario=args.scenario,
        start=int(case["index"]),
        count=1,
        only=args.tag,
    )
    records = [normalized_case_result(item, run_id, qemu.log_path) for item in qemu.parsed.cases]
    for record in records:
        append_jsonl(output / "case-results.jsonl", record)
        print(
            f"{record['scenario']}[{record['index']}] {record.get('tag', '')}: "
            f"{record['classification']}"
        )
    Journal(output / "journal.jsonl", root).record(
        "case-reproduction",
        "运行单条 LTP 复现",
        run_id=run_id,
        group=args.group,
        scenario=args.scenario,
        tag=args.tag,
        parsed=len(records),
        return_code=qemu.return_code,
        timed_out=qemu.timed_out,
        serial_log=str(qemu.log_path),
    )
    if not records:
        raise LtpError(f"单例未产生结构化结果，串口日志: {qemu.log_path}")
    return 0 if all(item["classification"] in {"pass", "pass-with-warning", "tconf", "static-skip"} for item in records) else 1


def markdown_escape(text: Any) -> str:
    """转义报告表格中的少量 Markdown 特殊字符。"""

    return str(text).replace("|", "\\|").replace("\n", " ")


def report_command(args: argparse.Namespace, root: Path) -> int:
    """从不可变结果记录生成 JSON 和 Markdown 汇总。"""

    output = (root / args.output).resolve() if not args.output.is_absolute() else args.output.resolve()
    results_path = output / "results.jsonl"
    records = load_jsonl(results_path)
    inventory_path = output / "inventory.json"
    inventory = load_inventory(inventory_path) if inventory_path.exists() else None
    classifications = Counter(str(record.get("classification", "unknown")) for record in records)
    group_counts: dict[str, Counter[str]] = defaultdict(Counter)
    scenario_counts: dict[tuple[str, str], Counter[str]] = defaultdict(Counter)
    for record in records:
        group = str(record.get("group", ""))
        scenario = str(record.get("scenario", ""))
        classification = str(record.get("classification", "unknown"))
        group_counts[group][classification] += 1
        scenario_counts[(group, scenario)][classification] += 1

    expected = 0
    coverage: dict[str, Any] = {}
    if inventory:
        for group, group_info in inventory["groups"].items():
            group_expected = int(group_info["total"])
            group_actual = sum(group_counts[group].values())
            coverage[group] = {"expected": group_expected, "recorded": group_actual}
            expected += group_expected

    summary = {
        "generated_at": utc_now(),
        "expected": expected,
        "recorded": len(records),
        "coverage": coverage,
        "classifications": dict(sorted(classifications.items())),
        "groups": {group: dict(sorted(counts.items())) for group, counts in sorted(group_counts.items())},
    }
    atomic_write_json(output / "report.json", summary)

    lines = [
        "# LoongArch64 LTP 测试报告",
        "",
        f"生成时间：`{summary['generated_at']}`",
        "",
        "## 覆盖情况",
        "",
        "| 测试组 | 官方记录数 | 已记录结果 |",
        "| --- | ---: | ---: |",
    ]
    for group, item in sorted(coverage.items()):
        lines.append(f"| {markdown_escape(group)} | {item['expected']} | {item['recorded']} |")
    lines.extend(["", "## 结果分类", "", "| 分类 | 数量 |", "| --- | ---: |"])
    for classification, count in sorted(classifications.items()):
        lines.append(f"| {markdown_escape(classification)} | {count} |")
    lines.extend(["", "## 场景汇总", "", "| 测试组 | 场景 | 结果 |", "| --- | --- | --- |"])
    for (group, scenario), counts in sorted(scenario_counts.items()):
        rendered = ", ".join(f"{key}={value}" for key, value in sorted(counts.items()))
        lines.append(f"| {markdown_escape(group)} | {markdown_escape(scenario)} | {markdown_escape(rendered)} |")

    noteworthy = [
        record
        for record in records
        if record.get("classification") not in {"pass", "pass-with-warning"}
    ]
    lines.extend(
        [
            "",
            "## 非通过明细",
            "",
            "| 测试组 | 场景 | 索引 | Tag | 分类 | 原因/日志 |",
            "| --- | --- | ---: | --- | --- | --- |",
        ]
    )
    for record in noteworthy:
        reason = record.get("reason") or record.get("serial_log", "")
        lines.append(
            "| {group} | {scenario} | {index} | {tag} | {classification} | {reason} |".format(
                group=markdown_escape(record.get("group", "")),
                scenario=markdown_escape(record.get("scenario", "")),
                index=record.get("index", ""),
                tag=markdown_escape(record.get("tag", "")),
                classification=markdown_escape(record.get("classification", "")),
                reason=markdown_escape(reason),
            )
        )
    (output / "report.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    print(f"report: {output / 'report.md'}")
    return 0


def journal_command(args: argparse.Namespace, root: Path) -> int:
    """允许人工诊断和修复步骤进入同一审计日志。"""

    output = (root / args.output).resolve() if not args.output.is_absolute() else args.output.resolve()
    fields: dict[str, str] = {}
    for item in args.field:
        if "=" not in item:
            raise LtpError(f"--field 必须为 key=value: {item}")
        key, value = item.split("=", 1)
        fields[key] = value
    Journal(output / "journal.jsonl", root).record(args.event, args.message, **fields)
    return 0


def add_common_runtime_options(parser: argparse.ArgumentParser) -> None:
    """为 run、resume 和 case 注册一致的运行参数。"""

    parser.add_argument("--kernel", type=Path, default=DEFAULT_KERNEL)
    parser.add_argument("--image", type=Path, default=DEFAULT_TEST_IMAGE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--docker-image", default=DEFAULT_DOCKER_IMAGE)
    parser.add_argument("--memory", default="4G")
    parser.add_argument("--cpus", type=int, default=4)
    parser.add_argument("--case-timeout", type=int, default=300)
    parser.add_argument("--kill-grace", type=int, default=5)
    parser.add_argument("--timeout-mul", type=int, default=4)
    parser.add_argument("--boot-timeout", type=int, default=120)
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--print-command", action="store_true")


def build_parser() -> argparse.ArgumentParser:
    """构建命令行接口。"""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    inventory = subparsers.add_parser("inventory", help="读取官方场景清单")
    inventory.add_argument("--image", type=Path, default=DEFAULT_TEST_IMAGE)
    inventory.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    inventory.add_argument("--docker-image", default=DEFAULT_DOCKER_IMAGE)
    inventory.add_argument("--groups", nargs="+", default=list(DEFAULT_GROUPS))

    run = subparsers.add_parser("run", help="开始新的全量活动")
    add_common_runtime_options(run)
    run.add_argument("--groups", nargs="+", default=list(DEFAULT_GROUPS))
    run.add_argument("--shard-size", type=int, default=50)
    run.add_argument("--retries", type=int, default=2)
    run.add_argument("--force", action="store_true")

    resume = subparsers.add_parser("resume", help="恢复中断的全量活动")
    add_common_runtime_options(resume)
    resume.add_argument("--groups", nargs="+", default=list(DEFAULT_GROUPS))
    resume.add_argument("--shard-size", type=int, default=50)
    resume.add_argument("--retries", type=int, default=2)
    resume.add_argument(
        "--accept-kernel-change",
        action="store_true",
        help="记录修复后的 kernel-la 哈希并从现有断点继续",
    )
    resume.add_argument("--force", action="store_true", help=argparse.SUPPRESS)

    case = subparsers.add_parser("case", help="复现单个 LTP tag")
    add_common_runtime_options(case)
    case.add_argument("scenario")
    case.add_argument("tag")
    case.add_argument("--group", default="adhoc")
    case.add_argument("--retries", type=int, default=0, help=argparse.SUPPRESS)

    report = subparsers.add_parser("report", help="生成当前结果报告")
    report.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)

    journal = subparsers.add_parser("journal", help="追加人工检查或修复日志")
    journal.add_argument("event")
    journal.add_argument("message")
    journal.add_argument("--field", action="append", default=[])
    journal.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    return parser


def repository_root() -> Path:
    """根据脚本位置定位仓库，避免依赖调用者当前目录。"""

    return Path(__file__).resolve().parent.parent


def main(argv: Sequence[str] | None = None) -> int:
    """命令行入口。"""

    parser = build_parser()
    args = parser.parse_args(argv)
    root = repository_root()
    try:
        if args.command == "inventory":
            return inventory_command(args, root)
        if args.command == "run":
            return execute_campaign(args, root, resume=False)
        if args.command == "resume":
            return execute_campaign(args, root, resume=True)
        if args.command == "case":
            return case_command(args, root)
        if args.command == "report":
            return report_command(args, root)
        if args.command == "journal":
            return journal_command(args, root)
    except KeyboardInterrupt:
        print("ltp_la.py: 用户中断", file=sys.stderr)
        return 130
    except (LtpError, OSError, subprocess.SubprocessError, ValueError) as error:
        print(f"ltp_la.py: {error}", file=sys.stderr)
        return 2
    parser.error(f"未知命令: {args.command}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
