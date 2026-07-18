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

QEMU 9.2.1 的 LoongArch64 `-kernel` 直启路径不会把 `-append` 写入当前 EFI stub 的
LoadedImage options，loader 实际收到的 `cmdline` 为零。编排器因此把同一组参数写入
`/dev/vd1` 根目录的 `ltp.conf`；rcS 仅在该文件明确包含 `ltp_run=1` 时进入测试模式。
`/sys/kernel/cmdline` 仍作为受支持启动器的兼容输入，但不作为本测试环境的唯一参数源。

## 执行模型

Guest 入口为 `userland/rootfs-la/etc/ltp-runner.sh`。它按官方 runtest 的稳定零基索引
选择记录，将一个分片交由官方 `ltp-pan -S` 顺序执行。runner 根据 `ltp-pan` 的
`tag/stat/dur` 结果日志和 RTS 输出边界还原逐例结果，不直接执行测试二进制，也不修改
官方场景命令。编排层只负责以下事项：

- 每个分片使用独立工作目录，`ltp-pan` 为每条记录创建独立进程组。
- 单例默认上限为 300 秒，LTP 内部超时倍率默认为 4；结果日志连续无进展达到上限时，
  当前未完成记录被判定为超时。
- 超时先通知 `ltp-pan` 终止当前测试进程组，再输出已完成工件和结构化结果，然后关闭
  当前 VM。
- 串口 marker 使用制表符分隔的 `@@LTP` 协议，不依赖人类输出文案。
- 宿主默认按 50 条记录分片，VM 或内核异常时从第一个缺失索引恢复。
- 同一索引连续重试仍无法产生 `case_end` 时，明确记录为 `kernel-hang`，不会伪装成
  LTP 失败或通过。
- VM 在任何测试开始前连续启动失败时，活动立即停止，不把基础设施问题计入测试结果。

兼容 rootfs 提供 LTP `IDcheck.sh` 要求的 `root`、`nobody`、`bin`、`daemon` 用户以及
`root`、`nobody`、`bin`、`daemon`、`sys`、`users` 组。缺少这些标准条目会把权限测试
环境错误误报为内核失败，因此它们属于测试前置条件而不是测试规避。

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
- [x] 构建专用 `kernel-la` 并完成 TPASS、静态跳过和 TCONF 三类冒烟。
- [x] 修复 epoll 高精度超时链路，并完成默认四核连续五轮计时回归。
- [ ] 完成 `default` 与 `network` 第一轮全量基线。
- [ ] 按独立根因修复内核漏洞并完成三次单例与场景分片回归。
- [ ] 从零完成最终全量复测。

## Missing Feature

本节只接受由实际用例输出确认的缺失功能，不依据代码浏览猜测。

| 场景/用例 | 证据 | 结论 |
| --- | --- | --- |
| `syscalls/fsopen01` | 两次 `fsopen()` 均返回 `ENOSYS`，LTP 汇总为 2 个 TFAIL | `fsopen(2)` 尚未实现 |

## 冒烟记录

| 用例 | 结果 | 说明 |
| --- | --- | --- |
| `syscalls/getpid01` | `pass` | glibc、runltp、工作盘和 marker 全链路正常 |
| `syscalls/delete_module01` | `static-skip` | 按 Linux LKM 强绑定规则跳过，原因字段完整 |
| `syscalls/fanotify01` | `tconf` | 测试自身识别可选功能不可用，分类器未误算为通过 |
| `syscalls/fsopen01` | `fail` | 明确的缺失系统调用，记录到 Missing Feature |

## 内核缺陷修复

### epoll 控制与用户态 ABI

第一轮 `default/syscalls` 基线在索引 137--149 暴露了多项相互独立的 epoll 缺陷。
原始失败输出位于
`build/ltp-loongarch64/serial/default-syscalls-00100-20260719-014819-4e3703.log`。

| 用例 | 根因 | 修复 | 验证 |
| --- | --- | --- | --- |
| `epoll_ctl01`、`epoll_wait01` | 系统调用层把所有 64 位架构的 `struct epoll_event` 固定解释为 x86_64 的 12 字节 packed 布局；LoongArch64 实际大小为 16 字节且 `data` 偏移为 8，事件数组第二项因此从错误地址读写 | 根据目标架构选择结构大小和 `data` 偏移；VFS 就绪扫描增加轮转游标，持续就绪项不再饿死后续项 | 两个用例各连续运行三次，全部 `pass` |
| `epoll_ctl02` | 普通文件和目录虽然对 `poll(2)` 表现为立即就绪，却被错误允许加入 epoll | `FileOps` 增加显式 epoll 接纳能力，普通文件和目录默认拒绝，真实事件源逐项声明支持 | 连续运行三次，全部 `pass` |
| `epoll_ctl04` | epoll 图只检查闭环，没有限制最大嵌套深度 | 对新增边执行递归深度校验，允许最多五个 epoll 实例组成嵌套链；闭环仍优先返回 `ELOOP` | 连续运行三次，全部 `pass`；`epoll_ctl05` 的原有闭环行为由宿主测试覆盖 |
| `epoll_wait03` | `maxevents` 直接按无符号系统调用参数处理，`-1` 被解释为巨大正数 | 先按 ABI 的有符号 `int` 解码并拒绝所有非正值 | 连续运行三次，全部 `pass` |
| `epoll_wait05` | inet socket 的 VFS poll 适配只报告可读写，不传播本地读半关闭和 TCP 对端关闭状态 | 将 `SHUT_RD` 映射为 `EPOLLRDHUP`，并从 TCP `Closing/Closed` 状态导出 `POLLIN/EPOLLRDHUP/POLLHUP` | 连续运行三次，全部 `pass` |

宿主执行 `cargo test -p vfs --target x86_64-unknown-linux-gnu`，共 67 项通过；新增测试
覆盖普通文件拒绝、第六层嵌套拒绝、闭环 `ELOOP` 优先级和 level-triggered 就绪轮转。
固定容器内 `make kernel-la` 构建通过。
对应提交为 `b0f46575`、`44cb8f02`、`4c3ce0a3` 和 `a07cd804`。

### pipe 容量与可写语义

`epoll_wait06` 的原始基线首先因 `F_SETPIPE_SZ` 返回 `EINVAL` 而 `TBROK`。实现动态
容量后，该用例继续暴露 pipe 把任意空闲字节误报为 `POLLOUT` 的第二层缺陷。

| 用例 | 根因 | 修复 | 验证 |
| --- | --- | --- | --- |
| `fcntl30`、`fcntl35`、`fcntl37`、`pipe2_04` | pipe 使用固定 64 KiB 数组，系统调用层没有 `F_SETPIPE_SZ/F_GETPIPE_SZ`，也没有非特权容量上限和对应 procfs 接口 | 实现可调整环形缓冲区、页大小二次幂取整、占用量 `EBUSY`、非特权 `EPERM`、`Capability::SysResource`，并提供可读写的 `/proc/sys/fs/pipe-max-size` | 四个用例各连续运行三次，全部 `pass` |
| `epoll_wait06` | pipe 只要存在一个空闲字节就报告 `POLLOUT`，且不超过 `PIPE_BUF` 的写入在剩余空间不足时会发生部分写 | 仅在空闲空间达到 4096 字节时报告 `POLLOUT`；不超过 `PIPE_BUF` 的写入保持原子性 | 连续运行三次，全部 `pass` |

宿主 VFS 测试扩展到 75 项，覆盖容量跨端点共享、零值收缩、容量取整、回绕数据保留、
占用量与权限错误、真实写容量、`PIPE_BUF` 原子写和 `POLLOUT` 阈值，全部通过。
对应提交为 `a210111c` 和 `aa8953b4`。

### LoongArch64 高精度超时与 epoll 计时

`epoll_wait02` 和 `epoll_pwait03` 的早期复现显示，1、2、5、10 ms 请求分别稳定等待
约 4、8、20、40 ms。修复四倍偏差后，`epoll_pwait03` 仍偶发在 10 ms 或 25 ms
统计组越过 LTP 阈值，因而继续检查完整 syscall、调度器和架构 timer 链路，而不是把
该结果归类为测试环境噪声。

| 层次 | 根因 | 修复 |
| --- | --- | --- |
| LoongArch64 TCFG | `TCFG.InitVal` 已经以位 47:2 的原始计数格式写入，旧代码仍再次左移两位，使硬件初值精确放大四倍 | 改用 one-shot TCFG，直接写入低两位清零的原始计数值；每次中断先恢复常规 tick，再由调度器发布更早的软件 deadline |
| 调度器 | 超时等待只能依赖周期 tick；到期项在 IRQ 中先分配临时 `Vec`，且全局最早 deadline 会被每个 CPU 同时装入本地 timer | 增加架构 deadline timer 契约；无分配地逐项消费到期 sleeper；deadline 按登记 CPU 归属，CPU 下线时迁移所有权，避免跨 CPU 定时器惊群 |
| Linux ABI | `PR_GET_TIMERSLACK` 未实现，LTP 无法得到 Linux 普通任务的 50 us 默认松弛量 | 为任务保存当前值和默认值，实现 `PR_SET_TIMERSLACK`、`PR_GET_TIMERSLACK`，并在 fork/clone 时继承 |
| epoll syscall | 内核在 fdtable、信号掩码等前处理完成后才开始计算 timeout；`epoll_pwait2` 还把纳秒 timespec 向上取整为毫秒 | 在 syscall handler 入口建立绝对 deadline，向 VFS 传递同一单调时间域的截止时间，`epoll_pwait2` 全程保留纳秒精度 |
| epoll 返回热路径 | 有限空 epoll 已自旋到真实 deadline 后，仍重新锁状态、分配 ready 向量并再次扫描，固定返回开销使截断均值停留在阈值边缘 | deadline 已到时直接完成空超时；只有期限前被事件唤醒时才重新扫描 ready 集合 |

验证结果：

- `epoll_wait02` 的 1 ms 到 1 s 共 7 个计时组全部通过。
- `epoll_pwait01`、`epoll_pwait02`、`epoll_pwait04`、`epoll_pwait05` 全部通过。
- 默认 4 CPU 配置下，`epoll_pwait03` 连续五轮、每轮 14 个计时组全部通过；原始日志
  为 `case-20260719-054354`、`054452`、`054530`、`054604` 和 `054638`。
- 宿主 `sched` 127 项、`vfs` 78 项测试全部通过，固定容器内 LoongArch64 release
  内核及全部内置 ELM 驱动重新构建成功。

对应提交为 `f91d94a5` 和 `5ecf6d3c`。
