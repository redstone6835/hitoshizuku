#!/usr/bin/env python3
"""Forward a QEMU serial socket to the host terminal.

BusyBox ash asks an interactive terminal for its cursor position with CSI 6 n.
The proxy answers that private terminal query locally so a plain raw serial
console remains usable even when the host terminal does not implement CPR.
All other bytes, including control characters such as Ctrl+C, are forwarded
unchanged.
"""

from __future__ import annotations

import argparse
import fcntl
import os
import selectors
import signal
import socket
import struct
import sys
import termios
import time
import tty


DSR = b"\x1b[6n"
DEFAULT_ROWS = 25
DEFAULT_COLS = 80
CONNECT_TIMEOUT = 30.0


def terminal_size() -> tuple[int, int]:
    """Return a usable (rows, columns) pair for the CPR response."""

    try:
        raw = fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8)
        rows, cols, _, _ = struct.unpack("HHHH", raw)
    except (OSError, ValueError, struct.error):
        rows, cols = DEFAULT_ROWS, DEFAULT_COLS
    if rows < 1:
        rows = DEFAULT_ROWS
    if cols < 1:
        cols = DEFAULT_COLS
    return rows, cols


def emit(stream_fd: int, data: bytes) -> None:
    """Write all output bytes, tolerating short writes."""

    while data:
        try:
            written = os.write(stream_fd, data)
        except InterruptedError:
            continue
        if written <= 0:
            raise BrokenPipeError
        data = data[written:]


def forward_guest_bytes(data: bytes, pending: bytearray, sock: socket.socket) -> None:
    """Forward guest output while consuming complete cursor queries."""

    pending.extend(data)
    while True:
        query_at = pending.find(DSR)
        if query_at >= 0:
            emit(1, bytes(pending[:query_at]))
            del pending[: query_at + len(DSR)]
            rows, cols = terminal_size()
            sock.sendall(f"\x1b[{rows};{cols}R".encode("ascii"))
            continue

        # Keep a possible partial DSR suffix until the next socket read.
        keep = 0
        max_keep = min(len(pending), len(DSR) - 1)
        for length in range(1, max_keep + 1):
            if pending[-length:] == DSR[:length]:
                keep = length
        if keep:
            emit(1, bytes(pending[:-keep]))
            del pending[:-keep]
        else:
            emit(1, bytes(pending))
            pending.clear()
        return


def connect(path: str) -> socket.socket:
    deadline = time.monotonic() + CONNECT_TIMEOUT
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(path)
            return sock
        except OSError as error:
            last_error = error
            sock.close()
            time.sleep(0.05)
    detail = f": {last_error}" if last_error else ""
    raise RuntimeError(f"timed out connecting to QEMU serial socket {path}{detail}")


def run(path: str) -> int:
    sock = connect(path)
    selector = selectors.DefaultSelector()
    selector.register(sock, selectors.EVENT_READ, "guest")
    stdin_is_tty = os.isatty(0)
    old_attrs = termios.tcgetattr(0) if stdin_is_tty else None
    if stdin_is_tty:
        tty.setraw(0)
    pending = bytearray()
    stopped = False

    def stop(_signum: int, _frame: object) -> None:
        nonlocal stopped
        stopped = True

    old_sigterm = signal.signal(signal.SIGTERM, stop)
    old_sighup = signal.signal(signal.SIGHUP, stop)
    if stdin_is_tty:
        selector.register(0, selectors.EVENT_READ, "host")
    try:
        while not stopped:
            events = selector.select(0.5)
            if not events:
                continue
            for key, _ in events:
                if key.data == "guest":
                    try:
                        data = sock.recv(65536)
                    except InterruptedError:
                        continue
                    if not data:
                        stopped = True
                        break
                    forward_guest_bytes(data, pending, sock)
                else:
                    try:
                        data = os.read(0, 65536)
                    except InterruptedError:
                        continue
                    if not data:
                        stopped = True
                        break
                    sock.sendall(data)
        if pending:
            emit(1, bytes(pending))
        return 0
    finally:
        selector.close()
        sock.close()
        signal.signal(signal.SIGTERM, old_sigterm)
        signal.signal(signal.SIGHUP, old_sighup)
        if old_attrs is not None:
            termios.tcsetattr(0, termios.TCSANOW, old_attrs)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket", help="QEMU AF_UNIX serial socket")
    args = parser.parse_args()
    try:
        return run(args.socket)
    except (BrokenPipeError, ConnectionError, OSError, RuntimeError) as error:
        print(f"qemu serial proxy: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
