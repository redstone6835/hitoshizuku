#!/usr/bin/env python3
"""驱动 QEMU 串口会话,依次发送命令并采集输出。

用法: qemu_probe.py <kernel> <image> <arch: la|rv> [超时秒]
"""
import os
import pty
import select
import subprocess
import sys
import time

kernel, image, arch = sys.argv[1], sys.argv[2], sys.argv[3]
timeout = float(sys.argv[4]) if len(sys.argv) > 4 else 120

qemu921 = "/opt/qemu-bin-9.2.1/bin/qemu-system-"
la_env = dict(os.environ, LD_LIBRARY_PATH="/opt/qemu921-libs")
if arch == "la":
    cmd = [qemu921 + "loongarch64",
        "-kernel", kernel, "-m", "1G", "-nographic", "-smp", "2", "-accel", "tcg,thread=single",
        "-no-reboot", "-rtc", "base=utc",
        "-drive", f"file={image},if=none,format=raw,id=x0,snapshot=on",
        "-device", "virtio-blk-pci,drive=x0",
        "-netdev", "user,id=net0",
        "-device", "virtio-net-pci,netdev=net0",
    ]
else:
    cmd = [qemu921 + "riscv64", "-machine", "virt",
        "-global", "virtio-mmio.force-legacy=false",
        "-kernel", kernel, "-m", "1G", "-nographic", "-smp", "1",
        "-no-reboot", "-rtc", "base=utc",
        "-drive", f"file={image},if=none,format=raw,id=x0,snapshot=on",
        "-device", "virtio-blk-device,drive=x0",
    ]

# 待发送的命令序列(每条前加换行确保在 shell 提示符下执行)
commands = [
    "echo ===PROBE-START===",
    "cat /proc/meminfo",
    "echo ===SWAPS===",
    "cat /proc/swaps",
    "echo ===SYSCTL===",
    "cat /proc/sys/vm/overcommit_memory /proc/sys/vm/max_map_count /proc/sys/vm/swappiness /proc/sys/vm/panic_on_oom",
    "echo ===SYSFS-THP===",
    "cat /sys/kernel/mm/transparent_hugepage/enabled",
    "cat /sys/kernel/mm/ksm/run",
    "cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages",
    "echo ===STATUS===",
    "cat /proc/self/status | grep -E 'VmSize|VmRSS|VmLck|Threads'",
    "echo ===FREE===",
    "free",
    "echo ===SYSINFO-BUSYBOX===",
    "sysinfo 2>/dev/null || echo no-sysinfo-app",
    "echo ===PROBE-END===",
]

proc = subprocess.Popen(
    cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    close_fds=True, env=la_env,
)

output = bytearray()
start = time.monotonic()
sent = 0
assert proc.stdin is not None and proc.stdout is not None
while time.monotonic() - start < timeout:
    if proc.poll() is not None:
        break
    r, _, _ = select.select([proc.stdout], [], [], 0.5)
    if r:
        try:
            chunk = os.read(proc.stdout.fileno(), 65536)
        except OSError:
            break
        if not chunk:
            break
        output.extend(chunk)
    # 启动竞态快失败:45 秒内未到 rcS 就放弃本次
    if time.monotonic() - start > 45 and b"[probe] boot" not in output:
        break
    # 启动后固定等待,再逐条发送命令
    try:
        if time.monotonic() - start > 8 and sent == 0:
            proc.stdin.write(b"\n")
            proc.stdin.flush()
            sent += 1
        if time.monotonic() - start > 10 and sent == 1:
            proc.stdin.write(b"\n")
            proc.stdin.flush()
            sent += 1
        if sent == 2 and time.monotonic() - start > 12:
            sent = 3
        if sent == 3:
            for cmd_text in commands:
                proc.stdin.write((cmd_text + "\n").encode())
                proc.stdin.flush()
                time.sleep(1.2)
            sent = 4
    except (BrokenPipeError, OSError):
        pass  # guest 已关机,不再发送
    if sent == 4 and b"===PROBE-END===" in output:
        time.sleep(2)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            r, _, _ = select.select([proc.stdout], [], [], 0.5)
            if not r:
                continue
            try:
                chunk = os.read(proc.stdout.fileno(), 65536)
            except OSError:
                break
            if not chunk:
                break
            output.extend(chunk)
        break

if proc.poll() is None:
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

# 始终落盘(即使 guest 提前关机导致发送失败)
sys.stdout.buffer.write(output)
sys.stdout.buffer.flush()
