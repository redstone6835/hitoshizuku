# LoongArch64 LTP 全量测试记录

本文档记录 LoongArch64 架构下 LTP 20240524 的测试范围、执行环境、判定规则、
跳过依据、缺失功能和内核缺陷修复。原始串口输出、逐例结果和机器可读审计日志位于
`build/ltp-loongarch64/`，不提交到 Git。

## 测试目标

- 测试镜像：`build/sdcard-la.img`，raw ext4，镜像内 LTP 版本为 `20240524`。
- 官方测试组：`scenario_groups/default` 与 `scenario_groups/network`。
- 架构范围：仅 `loongarch64-unknown-none`，本轮不使用 RISC-V64 结果。
- `default`：31 个场景，2418 条官方 runtest 记录。
- `network`：20 个场景，937 条官方 runtest 记录。
- 总计：3355 条场景记录。两个测试组中重复引用的场景按官方分组分别执行和记录。

清单由 `scripts/ltp_la.py inventory` 直接读取镜像中的 `scenario_groups` 和 `runtest`
文件生成。测试框架不会遍历 `testcases/bin`，也不会把辅助程序误当作独立测试。

## 固定环境

所有内核构建和 QEMU 启动均在以下容器中完成：

```text
zhouzhouyi/os-contest:20260510
```

QEMU 使用 4 GiB 内存、4 个 CPU 和 VirtIO PCI 设备。磁盘顺序固定为：

| Guest 设备 | 用途 | 宿主基盘 | 保护方式 |
| --- | --- | --- | --- |
| `/dev/vd0` | 只作为 LTP 程序和 glibc 数据源 | `build/sdcard-la.img` | QEMU 临时快照 |
| `/dev/vd1` | 每个 VM 的 ext4 工作目录 | `work-ext4.img`，16 GiB | QEMU 临时快照 |
| `/dev/vd2` | LTP 可破坏测试设备 | `test-device.img`，8 GiB | QEMU 临时快照 |
| `/dev/vd3` | LTP 大容量测试设备 | `big-device.img`，16 GiB | QEMU 临时快照 |

编排器在测试活动前后计算 `sdcard-la.img` 的 SHA-256。哈希变化会使整个活动失败，
不得把受污染镜像产生的结果并入报告。

## 执行模型

Guest 入口为 `userland/rootfs-la/etc/ltp-runner.sh`。它按官方 runtest 的稳定零基索引
选择记录，为每条记录临时生成一个单例场景，并继续交由官方 `runltp`/`ltp-pan`
执行。编排层只负责以下事项：

- 每条记录使用独立工作目录和进程组。
- 单例默认上限为 300 秒，LTP 内部超时倍率默认为 4。
- 超时先终止进程组，再输出已有工件和结构化结果，然后关闭当前 VM。
- 串口 marker 使用制表符分隔的 `@@LTP` 协议，不依赖人类输出文案。
- 宿主默认按 50 条记录分片，VM 或内核异常时从第一个缺失索引恢复。
- 同一索引连续重试仍无法产生 `case_end` 时，明确记录为 `kernel-hang`，不会伪装成
  LTP 失败或通过。
- VM 在任何测试开始前连续启动失败时，活动立即停止，不把基础设施问题计入测试结果。

## 结果分类

| 分类 | 含义 |
| --- | --- |
| `pass` | 至少一个 TPASS，且没有 TFAIL、TBROK 或非零 harness 退出 |
| `pass-with-warning` | 通过，同时产生 TWARN |
| `fail` | LTP 明确产生 TFAIL，表示已有功能行为不正确，必须复现和修复 |
| `broken` | LTP 产生 TBROK，需要区分内核漏洞、环境问题和测试本身问题 |
| `tconf` | 只有 TCONF，通常表示功能、工具或测试前置条件不存在 |
| `timeout` | Guest runner 正常检测到单例超时并回收当前 VM |
| `kernel-hang` | 宿主重试后仍收不到当前用例的结束 marker |
| `harness-error` | 没有明确 LTP 失败，但 `runltp` 非零退出 |
| `static-skip` | 依据 `ltp-skip.tsv` 确认与 Linux 内核实现强绑定而跳过 |
| `unknown` | 没有可靠 LTP 状态；必须人工检查，不得算作通过 |

## 跳过边界

静态跳过规则位于 `userland/rootfs-la/etc/ltp-skip.tsv`。当前只覆盖以下明确类别：

- Linux LKM 的 `init_module`、`finit_module`、`delete_module`、`insmod` 和 `lsmod`
  实现测试。
- Linux `kallsyms` 私有导出格式。
- 依赖 LTP Linux 内核测试模块的 block、PCI、ACPI、uaccess、RCU 和 lock torture
  用例。
- Linux zram 内核模块及其 sysfs 实现。
- 针对特定 Linux 内核漏洞实现细节的整个 `cve` 场景。

仅使用 Linux API、POSIX API 或常见 Unix 行为不构成跳过理由。功能尚未实现时应记录为
Missing Feature；已有功能行为错误、卡死、越界或崩溃时必须修复内核。

## 使用方法

构建专用内核：

```sh
docker run --rm -v "$PWD":/work -w /work \
  zhouzhouyi/os-contest:20260510 make kernel-la
```

重建镜像清单并运行单例：

```sh
python3 scripts/ltp_la.py inventory
python3 scripts/ltp_la.py case syscalls getpid01 --group default --print-command
```

开始、恢复和汇总全量活动：

```sh
python3 scripts/ltp_la.py run
python3 scripts/ltp_la.py resume
python3 scripts/ltp_la.py report
```

重要输出：

| 路径 | 内容 |
| --- | --- |
| `build/ltp-loongarch64/inventory.json` | 官方场景和每条原始命令 |
| `build/ltp-loongarch64/state.json` | 活动固定参数、哈希和恢复状态 |
| `build/ltp-loongarch64/results.jsonl` | 每条正式测试结果，按场景索引唯一 |
| `build/ltp-loongarch64/case-results.jsonl` | 人工单例复现结果 |
| `build/ltp-loongarch64/journal.jsonl` | 检查、分片、结果和修复审计日志 |
| `build/ltp-loongarch64/serial/` | 不裁剪的逐 VM 串口原始日志 |
| `build/ltp-loongarch64/report.md` | 从 JSONL 自动生成的当前报告 |

## 当前进度

- [x] 从镜像确认 LTP 版本、官方分组和 3355 条场景记录。
- [x] 实现单例隔离、静态跳过、超时和结构化 marker 的 Guest runner。
- [x] 实现清单、分片、恢复、异常重试、镜像保护和报告生成的宿主编排器。
- [x] 为清单解析、marker、结果分类和恢复去重添加宿主单元测试。
- [ ] 构建专用 `kernel-la` 并完成 TPASS、静态跳过和 TCONF 三类冒烟。
- [ ] 完成 `default` 与 `network` 第一轮全量基线。
- [ ] 按独立根因修复内核漏洞并完成三次单例与场景分片回归。
- [ ] 从零完成最终全量复测。

## Missing Feature

尚未完成第一轮全量基线。本节只接受由实际用例输出确认的缺失功能，不依据代码浏览猜测。

## 内核缺陷修复

尚未开始第一轮全量基线。每个独立根因将记录复现用例、根因、修改、三次单例结果、
场景回归结果和对应中文原子提交。
