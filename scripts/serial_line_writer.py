#!/usr/bin/env python3
"""以客机串口可稳定接收的速率写入一行命令。"""

from __future__ import annotations

import sys
import time
from collections.abc import Callable
from typing import BinaryIO


def write_serial_line(
    stream: BinaryIO,
    line: str,
    *,
    delay_seconds: float = 0.002,
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    if "\n" in line or "\r" in line:
        raise ValueError("serial command must contain exactly one line")
    payload = (line + "\n").encode("ascii")
    for value in payload:
        if stream.write(bytes((value,))) != 1:
            raise OSError("short serial write")
        sleep(delay_seconds)


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} FIFO LINE", file=sys.stderr)
        return 2
    with open(sys.argv[1], "wb", buffering=0) as stream:
        write_serial_line(stream, sys.argv[2])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
