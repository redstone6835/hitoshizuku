#!/usr/bin/env python3
"""
profile-compiling-split.py

把一次 profile run 按"首个 Compiling 出现"切成两段分别分析。

为什么要分开：这两段的内核负载特征完全不同，混在一起平均会同时掩盖两边的
真实情况。

  段 A（cargo exec → 首个 Compiling）
      cargo 解析 manifest、加载 registry 索引、stat 整棵源码树、建依赖图。
      特征：单进程、串行、极重的 VFS/元数据路径，几乎没有并行度可言。
      这一段无论调度器多好都不会变快——它压根没有可分配的并行任务。

  段 B（首个 Compiling → 窗口结束）
      cargo 按依赖图并发 spawn rustc。
      特征：进程创建风暴 + 大量并行编译进程。
      这一段才是 SMP 调度、work-stealing、CPU 利用率的真实考场。

因此："整窗口 CPU 利用率"这个指标本身是误导性的——段 A 天然只能用 1 个核，
把它算进平均值会让调度器的改进被稀释。

用法:
    profile-compiling-split.py <run_dir> [--json]

run_dir 需要包含 profile.serial.log 与 qemu-observer-plugin-summary.json。
"""

import argparse
import json
import re
import sys
from pathlib import Path

PROGRESS_RE = re.compile(r"(?<!\d)(\d{1,3})/446(?!\d)")
UPTIME_RE = re.compile(r"^\[\s*(\d+\.\d+)\]")


def read_serial(run_dir: Path) -> list[str]:
    path = run_dir / "profile.serial.log"
    if not path.exists():
        raise SystemExit(f"missing serial log: {path}")
    # 串口日志可能含 CR 和零星非 UTF-8 字节，用 replace 保证不因单字节损坏而中断。
    raw = path.read_bytes().replace(b"\r", b"\n")
    return raw.decode("utf-8", errors="replace").splitlines()


def find_boundaries(lines: list[str]) -> dict:
    """定位 cargo exec 与首个/末个 Compiling 里程碑。"""
    out = {
        "cargo_exec_line": None,
        "first_compiling_line": None,
        "first_milestone": None,
        "last_milestone": None,
        "milestones": {},
        "compiling_count": 0,
    }
    for idx, line in enumerate(lines):
        if "@@PROFILE_CARGO_EXEC" in line and out["cargo_exec_line"] is None:
            out["cargo_exec_line"] = idx
        if "Compiling" in line:
            out["compiling_count"] += 1
            if out["first_compiling_line"] is None:
                out["first_compiling_line"] = idx
        for m in PROGRESS_RE.finditer(line):
            n = int(m.group(1))
            if n <= 446:
                out["milestones"].setdefault(n, idx)
                if out["first_milestone"] is None or n < out["first_milestone"]:
                    out["first_milestone"] = n
                if out["last_milestone"] is None or n > out["last_milestone"]:
                    out["last_milestone"] = n
    return out


def load_vcpus(run_dir: Path) -> list[dict] | None:
    path = run_dir / "qemu-observer-plugin-summary.json"
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
    except json.JSONDecodeError:
        return None
    return data.get("vcpus")


def analyse_vcpus(vcpus: list[dict]) -> dict:
    kernel = [(c.get("cpu"), c.get("kernel", 0)) for c in vcpus]
    total = sum(k for _, k in kernel) or 1
    ranked = sorted(kernel, key=lambda x: -x[1])
    # "有效活跃"定义为承担 >=1% 总内核指令的 vCPU。低于这个量级的核实际上
    # 只在跑 idle 循环和偶发中断，把它们算作"活跃"会高估并行度。
    active = [c for c, k in kernel if k / total >= 0.01]
    top3 = sum(k for _, k in ranked[:3]) / total
    return {
        "per_cpu_kernel": {str(c): k for c, k in kernel},
        "total_kernel": total,
        "active_cpus": active,
        "active_count": len(active),
        "top3_share": top3,
        "share": {str(c): k / total for c, k in kernel},
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("run_dir")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    run_dir = Path(args.run_dir)
    lines = read_serial(run_dir)
    b = find_boundaries(lines)
    vcpus = load_vcpus(run_dir)
    v = analyse_vcpus(vcpus) if vcpus else None

    result = {
        "run_dir": str(run_dir),
        "serial_lines": len(lines),
        "cargo_exec_seen": b["cargo_exec_line"] is not None,
        "compiling_lines_captured": b["compiling_count"],
        "first_milestone": b["first_milestone"],
        "last_milestone": b["last_milestone"],
        "milestones_seen": sorted(b["milestones"]),
        "vcpu": v,
    }

    if args.json:
        print(json.dumps(result, indent=2))
        return 0

    print("=" * 78)
    print("  profile — Compiling 前/后分段分析")
    print("=" * 78)
    print(f"  run_dir          : {run_dir}")
    print(f"  串口行数         : {len(lines)}")
    print(f"  cargo exec 标记  : {'已捕获' if result['cargo_exec_seen'] else '缺失'}")
    print(f"  Compiling 行数   : {b['compiling_count']}")

    if b["compiling_count"] == 0:
        print()
        print("  [!] 串口日志里没有任何 Compiling 行。")
        print("      这不是内核问题，是采集链路问题：cargo 的 progress 走 stderr，")
        print("      若 runner 没有把 workload 的 stderr 接到串口 fd，这些行会被直接丢弃。")
        print("      在修好之前，任何按 cargo 里程碑切分的阶段分析都无法进行。")
    else:
        print(f"  里程碑覆盖       : {b['first_milestone']} … {b['last_milestone']} / 446")
        print(f"  已见里程碑点     : {result['milestones_seen'][:20]}")

    if v:
        print()
        print("-" * 78)
        print("  全窗口 per-vCPU 内核指令分布")
        print("-" * 78)
        for cpu in sorted(v["per_cpu_kernel"], key=lambda c: int(c)):
            k = v["per_cpu_kernel"][cpu]
            share = v["share"][cpu]
            bar = "█" * max(0, min(40, int(share * 40 / max(v['share'].values()))))
            print(f"   vCPU {cpu}: {k/1e9:7.3f} B  {share*100:5.1f}%  {bar}")
        print()
        print(f"  有效活跃 vCPU (>=1% 份额): {v['active_count']}/8  {v['active_cpus']}")
        print(f"  前 3 个 vCPU 占比        : {v['top3_share']*100:.1f}%")
        print()
        print("  注意：这是**整个窗口**的汇总，其中包含 cargo 解析/依赖图阶段——")
        print("  那一段本质单线程，无论调度器如何都只能用 1 个核。要评价 SMP 调度，")
        print("  必须只看首个 Compiling 之后的区间。")
    else:
        print()
        print("  [!] 缺少 qemu-observer-plugin-summary.json，无法给出 per-vCPU 分布。")

    return 0


if __name__ == "__main__":
    sys.exit(main())
