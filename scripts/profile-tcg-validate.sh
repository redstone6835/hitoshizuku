#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <report> <riscv64|loongarch64> <smp> [<start-pc> <stop-pc>]" >&2
    exit 2
}

case "$#" in 3|5) ;; *) usage ;; esac
report=$1
expected_target=$2
expected_vcpus=$3
expected_start_pc=${4:-}
expected_stop_pc=${5:-}
case "$expected_target" in riscv64|loongarch64) ;; *) usage ;; esac
case "$expected_vcpus" in ''|*[!0-9]*|0) usage ;; esac
[ -r "$report" ] || {
    echo "profile TCG validate: report is unreadable: $report" >&2
    exit 1
}

python3 - "$report" "$expected_target" "$expected_vcpus" \
    "$expected_start_pc" "$expected_stop_pc" <<'PY'
from __future__ import annotations

import re
import sys
from pathlib import Path


FIELD_NAME = re.compile(r"^[A-Za-z0-9_]+$")
UINT = re.compile(r"^[0-9]+$")
HEX = re.compile(r"^0x[0-9a-fA-F]+$")


def fail(message: str) -> None:
    raise ValueError(message)


def parse_fields(line: str, record: str, required: set[str]) -> dict[str, str]:
    words = line.split()
    if not words or words[0] != record:
        fail(f"malformed {record} record")
    values: dict[str, str] = {}
    for word in words[1:]:
        if word.count("=") != 1:
            fail(f"malformed {record} field={word!r}")
        name, value = word.split("=", 1)
        if not FIELD_NAME.fullmatch(name) or not value:
            fail(f"malformed {record} field={word!r}")
        if name in values:
            fail(f"duplicate {record} field={name}")
        values[name] = value
    missing = required - values.keys()
    extra = values.keys() - required
    if missing or extra:
        fail(
            f"invalid {record} fields: missing={sorted(missing)} extra={sorted(extra)}"
        )
    return values


HEADER_FIELDS = {
    "version",
    "target",
    "configured_vcpus",
    "active_vcpus",
    "table_bits",
    "table_slots",
    "table_probes",
    "counter_bytes_per_vcpu",
    "translated_blocks",
    "occupied_slots",
    "dropped",
    "collision_probes",
    "max_probe",
    "total_blocks",
    "total_instructions",
    "reported_hotspots",
    "windowed",
    "start_pc",
    "stop_pc",
    "start_events",
    "stop_events",
    "active_at_exit",
}
VCPU_FIELDS = {"cpu", "blocks", "instructions"}
HOT_FIELDS = {"rank", "pc", "blocks", "instructions"}


def as_uint(values: dict[str, str], name: str, record: str) -> int:
    value = values[name]
    if not UINT.fullmatch(value):
        fail(f"non-numeric {record} field={name}")
    return int(value)


def as_pc(values: dict[str, str], name: str, record: str) -> int:
    value = values[name]
    if not HEX.fullmatch(value):
        fail(f"invalid {record} PC field={name}")
    return int(value, 16)


def parse_expected_pc(value: str, name: str) -> int | None:
    if not value:
        return None
    try:
        parsed = int(value, 0)
    except ValueError:
        fail(f"invalid expected {name}={value!r}")
    if parsed < 0 or parsed > (1 << 64) - 1:
        fail(f"out-of-range expected {name}")
    return parsed


def main() -> None:
    path = Path(sys.argv[1])
    expected_target = sys.argv[2]
    expected_vcpus = int(sys.argv[3])
    expected_start = parse_expected_pc(sys.argv[4], "start_pc")
    expected_stop = parse_expected_pc(sys.argv[5], "stop_pc")
    if (expected_start is None) != (expected_stop is None):
        fail("expected start_pc/stop_pc must be a pair")
    strict_window = expected_start is not None

    headers: list[dict[str, str]] = []
    vcpus: list[dict[str, str]] = []
    hot: list[dict[str, str]] = []
    for line_number, raw_line in enumerate(
        path.read_text(encoding="utf-8", errors="strict").splitlines(), 1
    ):
        line = raw_line.strip()
        if not line:
            continue
        try:
            if line.startswith("MYGO_TCG_PROFILE "):
                headers.append(parse_fields(line, "MYGO_TCG_PROFILE", HEADER_FIELDS))
            elif line.startswith("VCPU "):
                vcpus.append(parse_fields(line, "VCPU", VCPU_FIELDS))
            elif line.startswith("HOT "):
                hot.append(parse_fields(line, "HOT", HOT_FIELDS))
            else:
                fail(f"unknown record={line.split()[0]!r}")
        except ValueError as error:
            fail(f"line {line_number}: {error}")

    if len(headers) != 1:
        fail(f"header count={len(headers)}")
    header = headers[0]
    if header["version"] != "2":
        fail(f"version={header['version']}")
    if header["target"] != expected_target:
        fail(f"target={header['target']}")

    numeric_header = HEADER_FIELDS - {"target", "start_pc", "stop_pc"}
    numbers = {name: as_uint(header, name, "header") for name in numeric_header}
    start_pc = as_pc(header, "start_pc", "header")
    stop_pc = as_pc(header, "stop_pc", "header")
    if numbers["configured_vcpus"] != expected_vcpus:
        fail(f"configured_vcpus={numbers['configured_vcpus']}")
    bits = numbers["table_bits"]
    if not 12 <= bits <= 23 or numbers["table_slots"] != 1 << bits:
        fail("invalid table geometry")
    if numbers["table_probes"] < 1 or numbers["counter_bytes_per_vcpu"] <= 16:
        fail("invalid counter layout")
    if not 1 <= numbers["active_vcpus"] <= expected_vcpus:
        fail("invalid active_vcpus")
    if (
        numbers["translated_blocks"] < 1
        or numbers["occupied_slots"] < 1
        or numbers["occupied_slots"] > numbers["table_slots"]
    ):
        fail("empty or overfull hot table")
    if (
        numbers["collision_probes"] < numbers["translated_blocks"]
        or not 1 <= numbers["max_probe"] <= numbers["table_probes"]
    ):
        fail("invalid probe accounting")
    if strict_window and numbers["dropped"] != 0:
        fail(f"dropped={numbers['dropped']}")
    if (
        numbers["total_blocks"] < 1
        or numbers["total_instructions"] < 1
        or numbers["reported_hotspots"] < 1
        or numbers["reported_hotspots"] > 4096
    ):
        fail("empty execution counters")
    if len(hot) != numbers["reported_hotspots"]:
        fail(
            f"HOT count={len(hot)} expected={numbers['reported_hotspots']}"
        )

    windowed = numbers["windowed"]
    if windowed not in (0, 1):
        fail(f"windowed={windowed}")
    if windowed:
        if (
            numbers["start_events"] != 1
            or numbers["stop_events"] != 1
            or numbers["active_at_exit"] != 0
            or start_pc == stop_pc
        ):
            fail("incomplete profile window")
    elif any(
        (numbers["start_events"], numbers["stop_events"], numbers["active_at_exit"])
    ) or start_pc != 0 or stop_pc != 0:
        fail("unexpected state in unwindowed profile")
    if expected_start is not None:
        if not windowed or start_pc != expected_start or stop_pc != expected_stop:
            fail(
                f"profile markers=0x{start_pc:x}/0x{stop_pc:x}, "
                f"expected=0x{expected_start:x}/0x{expected_stop:x}"
            )

    seen_cpus: set[int] = set()
    vcpu_blocks = 0
    vcpu_instructions = 0
    for row in vcpus:
        cpu = as_uint(row, "cpu", "VCPU")
        blocks = as_uint(row, "blocks", "VCPU")
        instructions = as_uint(row, "instructions", "VCPU")
        if cpu >= expected_vcpus or cpu in seen_cpus:
            fail(f"duplicate or out-of-range VCPU cpu={cpu}")
        if blocks < 1 or instructions < blocks:
            fail(f"invalid VCPU counters cpu={cpu}")
        seen_cpus.add(cpu)
        vcpu_blocks += blocks
        vcpu_instructions += instructions
    if len(vcpus) != numbers["active_vcpus"]:
        fail(f"VCPU count={len(vcpus)} expected={numbers['active_vcpus']}")
    if vcpu_blocks != numbers["total_blocks"]:
        fail(f"VCPU blocks sum={vcpu_blocks} expected={numbers['total_blocks']}")
    if vcpu_instructions != numbers["total_instructions"]:
        fail(
            f"VCPU instructions sum={vcpu_instructions} "
            f"expected={numbers['total_instructions']}"
        )

    seen_ranks: set[int] = set()
    seen_pcs: set[int] = set()
    hot_blocks = 0
    hot_instructions = 0
    previous_priority: tuple[int, int, int] | None = None
    for index, row in enumerate(hot, 1):
        rank = as_uint(row, "rank", "HOT")
        pc = as_pc(row, "pc", "HOT")
        blocks = as_uint(row, "blocks", "HOT")
        instructions = as_uint(row, "instructions", "HOT")
        if rank != index or rank in seen_ranks:
            fail(f"duplicate or non-contiguous HOT rank={rank}")
        if pc in seen_pcs:
            fail(f"duplicate HOT pc=0x{pc:x}")
        if blocks < 1 or instructions < blocks:
            fail(f"invalid HOT counters rank={rank}")
        priority = (-instructions, -blocks, pc)
        if previous_priority is not None and priority < previous_priority:
            fail(f"HOT rows are not sorted at rank={rank}")
        previous_priority = priority
        seen_ranks.add(rank)
        seen_pcs.add(pc)
        hot_blocks += blocks
        hot_instructions += instructions
    if hot_blocks > numbers["total_blocks"]:
        fail(f"HOT blocks sum={hot_blocks} exceeds total={numbers['total_blocks']}")
    if hot_instructions > numbers["total_instructions"]:
        fail(
            f"HOT instructions sum={hot_instructions} exceeds "
            f"total={numbers['total_instructions']}"
        )
    if numbers["dropped"] == 0:
        if hot_blocks != numbers["total_blocks"]:
            fail(f"HOT blocks sum={hot_blocks} expected={numbers['total_blocks']}")
        if hot_instructions != numbers["total_instructions"]:
            fail(
                f"HOT instructions sum={hot_instructions} "
                f"expected={numbers['total_instructions']}"
            )


try:
    main()
except (OSError, UnicodeError, ValueError) as error:
    print(f"profile TCG validate: {error}", file=sys.stderr)
    raise SystemExit(1)
PY
