# BuildStorm 内核设计与优化报告

## 1. 项目目标

BuildStorm 在 MyGO OS 上从源码构建 `arceos-helloworld`。负载包含数百个 Rust crate，
并行运行 `cargo`、`rustc`、链接器和构建脚本，持续产生进程创建、动态链接、文件映射、
匿名内存写入、页错误、文件系统读取、管道通信和任务等待。是对内核执行环境的综合检验。

本轮工作的目标包括三项：

1. 完整测试 BuildStorm；
2. 在 RISC-V64 与 LoongArch64 多核环境中缩短完整编译时间；
3. 保持 Linux/POSIX 兼容语义，并确保 CAgent 以及初赛测试用例等既有测试不发生回归。

优化遵循两个原则。第一，先修复会造成停滞或提前退出的并发缺陷，再比较性能；一次没有
完整结束的运行不能作为性能样本。第二，优化必须减少内核实际工作量，而不是修改计时、
测试脚本或构建产物。

## 2. 工作负载分析与根因定位

### 2.1 分析方法

项目建立了三层测量链路。

- 端到端层以客体 `/proc/uptime` 记录正式编译区间，检查工具链、最小项目、完整构建和
  产物运行结果；
- 内核层通过 `performance-profile` 统计系统调用、页错误、内存管理、调度、分配器和
  文件系统事件，区分 on-CPU 执行与阻塞等待；
- 指令层使用 QEMU plugin 统计客体指令，并结合最终内核符号表归因到函数，用于发现没有
  被 Rust 函数级计时覆盖的架构入口、TLB 和内存原语成本。

仓库中的 `scripts/buildstorm-profile-host.sh` 和 `scripts/buildstorm-profile-guest.sh` 负责
对齐测量窗口并记录内核、磁盘、QEMU、CPU 数量和工作负载身份；`scripts/profile-report.sh`
解析内核统计，`scripts/analyze-buildstorm-syscalls.py` 汇总系统调用，QEMU plugin 位于
`tools/qemu-plugins/`。比较脚本会拒绝元数据不一致、采集未静止、边界延迟异常和方差过大
的样本，避免把宿主机波动误判为内核收益。

RISC-V 指令级分析采用仓库中的
[《RISC-V64 指令权重微基准：方法、数据与结论论证》](riscv-instruction-weight-model-report.md)
作为成本模型依据。该报告说明了 QEMU TCG 环境下的探针设计、成对差分、指令编码校验、
统计门禁和适用边界。BuildStorm 分析只使用通过质量门禁的权重，并保留函数级动态指令数
与未加权计数作为对照，不把微基准权重解释为真实 RISC-V 处理器的硬件周期数。

### 2.2 主要瓶颈

分析结果表明，BuildStorm 的内核开销主要来自五条路径。

| 路径 | 原有行为 | 根因 |
| --- | --- | --- |
| 等待与唤醒 | 已登记可靠 waiter 的 `poll/ppoll` 仍周期唤醒并重新扫描全部文件 | 兼容轮询没有区分“可靠事件源”和“不支持 waiter 的事件源” |
| 用户缺页 | 一次缺页重复查找 VMA、PTE 和 resident 状态，连续页逐页提交 | 地址空间元数据访问和页表发布粒度过细 |
| Slab 与堆 | 高频小对象命中仍进入共享状态；释放路径需要定位 zone；普通堆也承担 ELM 归属维护 | per-CPU 缓存没有完整隔离共享元数据，跟踪机制覆盖范围过宽 |
| 调度与定时器 | 空闲 CPU 缺少主动拉取入口，批量唤醒逐项处理，硬件 deadline 重复编程 | 多核负载均衡和超时事件仍以单任务粒度推进 |
| extfs 与 TLB | 顺序读重复解析块映射和校验元数据；地址空间切换或连续页更新触发过多失效 | 缺少局部缓存、批量校验和按地址空间定向失效 |

这些成本会被 Rust 构建放大。`rustc` 的大量短生命周期进程使调度和执行映像切换频繁；
代码生成和链接产生大量匿名写缺页；依赖读取和动态链接形成密集的只读文件页错误；Cargo
的管道与子进程管理又使 `poll`、唤醒和小对象分配成为高频操作。单独看每次调用开销不大，
累计到完整构建后会占据数十至数百秒。

### 2.3 正确性前提

压力测试还暴露了两类会破坏测量有效性的并发问题：任务迁移后 RISC-V hart 本地状态恢复
不完整，以及多线程 `exec` 等待兄弟线程退出时的唤醒竞态。前者会在用户态返回边界使用
不属于当前任务的 hart 状态，后者会让执行映像替换永久等待已经退出的线程。

修复后，调度交接显式恢复当前 hart 状态；`exec` 的退出请求、状态转换和等待唤醒形成完整
闭环。只有工具链检查、最小构建、完整 BuildStorm 和后续测试均能结束的运行，才进入性能
比较。

## 3. 优化设计与实现

### 3.1 事件驱动的等待路径

旧 `poll/ppoll` 路径用固定周期重扫避免丢失不支持 waiter 的事件。这个兜底对设备兼容有
意义，但也使管道、socket、timerfd 等已经能够可靠注册 waiter 的对象每隔固定时间被无效
唤醒。

新的等待路径记录每个文件是否成功注册可靠 waiter：

- 所有来源均可靠时，任务直接睡眠到事件、信号或用户 deadline；
- 只要存在一个不可靠来源，仍保留周期重扫，并取兼容重扫时间与用户 deadline 的较早值；
- waiter 注册后立即复查 readiness，关闭“检查为空到进入睡眠”之间的丢唤醒窗口；
- 返回顺序仍遵守就绪事件、信号中断和超时语义。

因此，常见管道等待从周期轮询变为真正的事件驱动等待，同时没有降低对旧设备的兼容性。
主要实现位于 `kernel/src/syscalls/fs.rs` 和各 VFS 对象的 `poll_add_waiter`/
`poll_remove_waiter` 接口。

### 3.2 缺页、resident 索引与 TLB

虚拟内存优化围绕“共享查找结果、批量处理连续工作、提交前重新验证”展开。

首先，缺页路径在已有 VMA 锁和快照内完成 resident/PTE 判断，避免同一次缺页反复取得相同
元数据。resident 映射改用按虚拟页索引的数据结构，范围删除也按连续区间批量摘除，降低
映射数量增长后的查找和回收成本。

其次，fault-around 会为同一 VMA 中相邻的只读私有文件页准备一个有界窗口。缓存命中页
可以批量安装；缓存缺失页完成读取后，提交前再次检查 VMA 快照、文件映射代际和目标 PTE，
防止与 `munmap`、`mprotect`、文件截断和并发缺页冲突。用户地址读写也复用相同的页表遍历
结果，减少大缓冲区复制中的重复缺页准备。

最后，连续页权限或访问状态更新先合并为范围，再发布页表失效。RISC-V 使用 ASID 与范围
RFENCE，只向仍可能执行目标地址空间的 hart 发送失效；LoongArch 为用户地址空间分配带
代际的 ASID，在首次使用、ASID 复用、共享 fallback 或错过更新时才执行保守全刷。内核
全局映射仍使用完整失效，不把用户地址空间优化错误套用到全局页表。

这组实现主要位于 `general/src/mm/`、`libs/mm/`、`arch/src/riscv64/paging.rs`、
`arch/src/riscv64/heap_vm.rs`、`arch/src/loongarch64/paging.rs` 和
`arch/src/loongarch64/asid_tracker.rs`。它保持 COW、文件截断、权限变更和 TLB shootdown
的可见性语义，只减少重复查找与重复发布。

### 3.3 分配器与 ELM 归属隔离

BuildStorm 会创建大量 VFS、调度和进程管理小对象。优化后的 Slab 使用真正的 per-CPU
magazine：命中时仅在本 CPU 的固定容量缓存中弹入或弹出对象，只有 refill 和 flush 才批量
进入共享 `ZoneState`。补货过程保留链游标，释放过程通过地址范围直接定位尺寸类和 zone，
避免随 zone 数量增长的线性搜索。

ELM 的所有者跟踪只对需要参与模块生命周期管理的受追踪堆有意义。普通内核堆与受追踪堆
被分离后，分配器缓存受追踪区间边界：普通地址直接进入 Slab 分配和释放路径，不访问 owner
registry；只有落在受追踪区间内的对象才登记、更新和删除归属。当前执行上下文的 owner 由
CPU-local guard 读取，无动态 provider 时直接走静态实现。

这项设计不是关闭 ELM 检查，而是把检查限制在其语义负责的对象集合中。动态模块对象仍保留
归属、卸载审计和回收能力，普通内核对象不再为未使用的动态生命周期支付全局索引成本。
主要实现位于 `libs/allocator/src/slab.rs`、`libs/allocator/src/lib.rs`、
`general/src/elm_guard.rs` 和 `libs/elm/`。

### 3.4 调度、超时与系统调用返回

多核编译要求空闲 CPU 能主动参与，而不能只等待繁忙 CPU 发起均衡。调度器在 idle 路径
调用既有迁移框架，从其他运行队列拉取满足 affinity、调度类和迁移条件的任务。迁移仍按
固定锁序取得运行队列锁，并保留 Deadline 准入和任务状态检查。

超时唤醒和信号扫描改为一次收集、一批入队，减少相同锁和调度提示的重复操作。硬件定时器
缓存已经编程的最早 deadline，仅当新的最早事件真正变化时才重新编程。系统调用入口缓存
当前任务和返回路径所需状态，并把信号、抢占、rseq 与页表收尾聚合到一次用户态返回边界；
它们的可观察顺序保持不变。

相关实现位于 `libs/sched/src/`、`general/src/syscall.rs`、`kernel/src/sched.rs` 和两种架构的
系统调用入口代码。

### 3.5 extfs 顺序读取

Rust 工具链会反复读取 crate 元数据、目标文件和动态库。extfs 为 inode 保存带代际的块映射
缓存，顺序读取无需重复遍历 extent 树；映射变化时更新代际，使旧缓存不能跨 truncate 或
重映射继续使用。CRC32C 使用 slicing-by-16 查表批量处理，保持与 ext4 metadata checksum
一致的多项式和初值语义。文件尾部和跨块读取直接填入调用方分散缓冲，避免为不足一块的
尾部额外分配中间页并再次复制。

主要实现位于 `libs/extfs/src/`。优化只改变块定位、校验计算和数据搬运方式，不跳过元数据
校验，也不改变文件可见内容。

## 4. 实验结果

### 4.1 测量口径

端到端数据均使用普通 release 内核，不启用 `performance-profile`。同一架构的修改前后运行
使用相同磁盘、QEMU 参数、CPU 数量和内存配置；`tg-xtask` 预构建不进入正式计时，目标架构
输出在运行前清理。计时只覆盖 `cargo xtask arceos build`，与比赛脚本口径一致。

profiling 数据用于判断具体机制是否减少目标路径工作量。它不与普通内核的绝对耗时混用，
也不把包含多个改动的端到端差值强行分配给单个函数。

### 4.2 关键机制变化

| 优化方向 | 修改前 | 修改后 | 结果 |
| --- | ---: | ---: | ---: |
| 可靠 waiter 的 `ppoll` on-CPU 时间 | 256.592 s | 5.134 s | 降低 98.00% |
| 连续页权限更新 on-CPU 时间 | 179.196 s | 113.065 s | 降低 36.90% |
| Slab slow path | 100% | 27.47% | 降低 72.53% |
| Slab refill | 100% | 27.59% | 降低 72.41% |
| 缺页与 registry 组合的 page-fault on-CPU 时间 | 100% | 46.73% | 降低 53.27% |

这些结果分别验证了事件驱动等待、连续页批量提交、per-CPU magazine 和堆归属隔离确实
命中了预期路径。调度优化后，完整构建期间能够参与工作的平均 CPU 数增加，原先“繁忙核
排队而其他核空闲”的情况明显减少。

### 4.3 完整构建结果

| 架构与配置 | 修改前 | 优化后 | 缩短 | 加速比 |
| --- | ---: | ---: | ---: | ---: |
| RISC-V64，SMP 8，16 GiB | 549.48 s | 461.94 s | 87.54 s（15.93%） | 1.190x |
| LoongArch64，SMP 12，24 GiB | 454.05 s | 352.50 s | 101.55 s（22.37%） | 1.288x |

两种架构均完成工具链检查、最小 Cargo 项目和完整 `arceos-helloworld` 构建，构建产物能够
运行。优化后还继续执行 CAgent 全部十个测试并正常关机，用于确认性能改动没有以提前失败、
跳过 I/O 或破坏等待语义换取耗时。

RISC-V 与 LoongArch 的 CPU 数量和内存配置不同，因此表中的跨架构绝对时间不能互相比较；
收益只按同一架构的前后结果计算。QEMU TCG 和宿主机负载会产生波动，最终结论同时依赖完整
构建时间和机制计数，不以单次小幅变化判断优化有效。

## 5. 正确性与设计边界

优化过程中始终保留以下边界：

- `poll/ppoll` 只有在所有来源都可靠注册 waiter 时才取消周期兜底；
- fault-around 在提交前重新验证 VMA、文件映射代际和 PTE，不跨越权限或映射变化；
- TLB 优化仅在 ASID 和目标 CPU 集合可证明时定向失效，全局映射继续保守刷新；
- Slab magazine 只缓存已由 Slab 拥有的对象，批量回收时仍在共享状态锁内更新位图；
- owner 快路径只绕过普通堆，受追踪堆的 ELM 生命周期信息没有删除；
- extfs 仍执行 metadata checksum，缓存通过 inode 映射代际失效；
- 调度迁移继续满足 affinity、调度类、任务状态和运行队列锁序。

曾尝试过更激进的内存原语、跳过页缓存代际验证以及扩大匿名页预映射窗口。这些方案要么在
完整构建中没有稳定收益，要么会扩大稀疏映射的物理内存占用或削弱失效闭环，因此没有作为
最终设计的一部分。最终代码只保留能够说明语义边界的优化。

## 6. AI 使用说明

项目在 BuildStorm 优化中使用 Codex 搭载 deepseek-v4-flash-0731 辅助完成以下工作：

- 阅读热点数据，整理热点瓶颈。
- 整理归档热点测试数据，辅助建立测试基线与 A/B 测试。
- 辅助审查开发者编写代码，预测改动风险。

所有 AI 的原始生成或建议的修改经过开发者审查，不存在滥用行为。

## 7. 可复现步骤

以下命令均从仓库根目录执行。构建环境使用比赛容器，依赖从仓库 `vendor/` 离线解析。

### 7.1 构建普通内核

```bash
docker run --rm -it -v "$PWD":/work -w /work \
  zhouzhouyi/os-contest:20260510 bash

make kernel-rv
make kernel-la
```

构建结果为仓库根目录的 `kernel-rv` 和 `kernel-la`。BuildStorm 测试盘由主办方提供，不属于
本仓库；复现者通过 `PROFILE_BASE_IMAGE` 显式指定它，脚本会为每轮运行创建 qcow2 overlay， 。

### 7.2 运行正式 BuildStorm

使用 `testsuits-for-oskernel` 仓库中的 `scripts/buildstorm_testcode.sh` 作为客体测试
脚本，并使用该仓库 README 公布的 QEMU 参数。结果必须同时出现以下三类成功标记：

```text
TOOLCHAIN_RESULT status=OK
MINIBUILD_RESULT status=OK
BUILDSTORM_RESULT status=OK
```

`BUILDSTORM_RESULT` 中的 `elapsed_s` 是正式编译耗时。检查串口结果时还应确认产物存在且能够
运行，不能只读取最后一个时间字段。

### 7.3 采集内核 profiling

先构建 profiling 内核：

```bash
make kernel-rv FEATURES="performance-profile"
make kernel-la FEATURES="performance-profile"
```

然后对指定架构运行固定窗口。下例使用 RISC-V、8 个 vCPU、16 GiB 内存和正式 extfs 工作
目录；LoongArch 将架构、CPU 数和内存改为对应配置。

```bash
PROFILE_ARCH=riscv64 \
PROFILE_SMP=8 \
PROFILE_MEMORY=16G \
PROFILE_TARGET_FS=extfs \
PROFILE_KERNEL="$PWD/kernel-rv" \
PROFILE_BASE_IMAGE=/path/to/sdcard-rv.img \
PROFILE_DURATION_MS=300000 \
PROFILE_RUN_ROOT=/tmp/mygo-buildstorm-profile \
scripts/buildstorm-profile-host.sh
```

runner 会输出本轮目录，其中包含 `summary.json`、串口、QEMU 线程统计和采集身份。报告解析：

```bash
scripts/profile-report.sh \
  /tmp/mygo-buildstorm-profile/<run>/profile.serial.log \
  kernel-rv

python3 scripts/analyze-buildstorm-syscalls.py \
  /tmp/mygo-buildstorm-profile/<run>/profile.serial.log \
  --output-dir /tmp/mygo-buildstorm-syscalls
```

### 7.4 前后版本比较

每个版本至少采集三轮，并保持架构、磁盘、QEMU、容器、CPU 集合、窗口和 workload metadata
一致。比较脚本会检查一致性、组内变异系数和边界误差：

```bash
scripts/buildstorm-profile-compare.sh \
  /tmp/baseline-1/summary.json \
  /tmp/baseline-2/summary.json \
  /tmp/baseline-3/summary.json -- \
  /tmp/candidate-1/summary.json \
  /tmp/candidate-2/summary.json \
  /tmp/candidate-3/summary.json
```

验收一项性能修改时，应同时满足：完整构建成功、正确性测试通过、目标机制计数下降、端到端
耗时没有超过噪声范围的回退。这样可以区分真实优化、宿主性能波动和 profiling 本身的扰动。

## 8. 总结

BuildStorm 的主要问题不是某一条系统调用缺失，而是成熟工具链把等待、缺页、对象分配、
多核调度和文件读取中的细粒度重复成本同时放大。MyGO 的优化因此没有集中在单个“特殊
BuildStorm 快路径”，而是把这些通用机制改为事件驱动、范围批量、per-CPU 缓存、定向失效
和带代际的局部缓存。

最终结果表明，两种架构都能完成完整 Rust 构建，并在相同架构的前后对比中获得 15% 以上
的端到端加速。更重要的是，这些改动保留了 POSIX 等待、虚拟内存一致性、文件系统校验和
ELM 生命周期语义，因此收益能够继续服务于 CAgent、编译器和其他真实用户态负载。
