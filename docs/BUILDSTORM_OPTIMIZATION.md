# BuildStorm 性能分析与验收

## 目标与测量原则

最终目标是 `tg-xtask` 不超过 15 分钟、后续内核构建不超过 5 分钟，并以三次独立运行确认。所有对比使用固定容器 `zhouzhouyi/os-contest:20260510`、8 GiB 内存、8 个 QEMU vCPU、同一只读 raw 基准盘和每轮新建的 qcow2 overlay。不得复用 guest 的 `target/debug`，也不得在测量期间并发运行 Cargo、Make 或其他 QEMU。

`scripts/buildstorm-profile-host.sh` 使用 guest gate 对齐窗口：工作负载进程组先停止，profile-on/off 都经过相同的 START/STOP 控制；guest 与正式 init 一样先把 `/work/tgoskits/target` 挂为上限 5 GiB 的 tmpfs，并输出 `@@PROFILE_TARGET_FS` 标记，避免把 extfs 写回或损坏混入编译器/MM 数据。停止时先冻结计数器并记录 host/QEMU 边界，再在窗口外导出快照。summary 同时记录 Cargo progress、QEMU CPU、主机 PSI、控制延迟和镜像哈希。

## 构建固定内核

必须完整重建 ELM 模块，不能复用其他 feature 的归档：

```sh
docker run --rm -v "$PWD":/work -w /work \
  zhouzhouyi/os-contest:20260510 bash -lc 'make kernel-la'
cp kernel-la /tmp/kernel-la-model-off

docker run --rm -v "$PWD":/work -w /work \
  zhouzhouyi/os-contest:20260510 \
  bash -lc 'make kernel-la FEATURES="performance-profile"'
cp kernel-la /tmp/kernel-la-model-profile
```

构建生成的 `drivers/*/Elm.lock`、`kernel-la`、`build/` 和 `target/` 不应提交。

## 宿主机 QEMU observer

### 构建内核、符号快照和 plugin

observer 必须使用与被引导内核同一次最终链接产生的 map。构建时通过 Make 传入 `KERNEL_MAP`，再在比赛容器中构建固定 QEMU plugin ABI 的共享库：

```sh
docker run --rm -v "$PWD":/work -w /work \
  zhouzhouyi/os-contest:20260510 bash -lc \
  'make kernel-la KERNEL_MAP=/work/build/loongarch64/kernel.map'

scripts/build-qemu-profile-plugin.sh
```

成功后必须把以下三项视为一个不可拆分的符号快照：`kernel-la`、`build/loongarch64/kernel.map` 和 `build/loongarch64/kernel.map.manifest`。manifest 固定包含 `schema`、`target`、`kernel_sha256`、`symbol_map_sha256` 四个字段；consumer 必须校验两个 SHA-256，不能按文件名猜测 kernel 与 map 是否匹配。

设置 `KERNEL_MAP` 时一次只能构建一个架构，`make all` 等双架构目标会被拒绝，路径也不能含空白字符。最终链接在 Cargo source、build kernel、map 和根目录 kernel 各自的 `${path}.lock` 上持有非阻塞锁；任一共享资源已被其他发布器占用时直接失败，即使两个发布器使用不同 map 路径也不能并发覆盖同一 Cargo 输出。kernel 和 map 都先写同目录唯一临时文件，发布顺序为 build kernel、根目录 kernel、map、manifest，manifest 是最后的提交标记。复制失败不会替换任何 kernel；rename 中途失败只可能留下完整的新 kernel 文件和旧 manifest，consumer 必须因哈希不匹配而失败，不能把半写 kernel 或单独补拷的 kernel、map、manifest 当作有效快照。

### 运行和产物

先用 20 秒窗口检查链路，再使用固定窗口采集。`PROFILE_REQUIRE_SYMBOL_MANIFEST=1` 和 `PROFILE_OBSERVER_REQUIRE_VALID=1` 均为默认值，建议显式保留在可复现实验命令中：

```sh
mkdir -p /tmp/mygo-qemu-profile
PROFILE_RUN_ROOT=/tmp/mygo-qemu-profile \
PROFILE_BASE_IMAGE=/home/redstone/src/oskernel2026-mygo-network-cagent/build/sdcard-la-pub.img \
PROFILE_CPUSET=0,2,4,6,8,10,12,14 \
PROFILE_KERNEL="$PWD/kernel-la" \
PROFILE_QEMU_OBSERVER=1 PROFILE_SYSTEM=mygo \
PROFILE_QEMU_PLUGIN="$PWD/build/qemu-plugins/buildstorm_observer.so" \
PROFILE_SYMBOL_MAP="$PWD/build/loongarch64/kernel.map" \
PROFILE_SYMBOL_MANIFEST="$PWD/build/loongarch64/kernel.map.manifest" \
PROFILE_REQUIRE_SYMBOL_MANIFEST=1 PROFILE_OBSERVER_REQUIRE_VALID=1 \
PROFILE_CAPTURE=0 PROFILE_LABEL=observer-smoke \
PROFILE_DURATION_MS=20000 scripts/buildstorm-profile-host.sh
```

`scripts/buildstorm-profile-host.sh` 是 MyGO 和 Linux 共用的 runner：两种启动模式使用同一套串口 gate、workload plan、qcow2 overlay、5 GiB target tmpfs 和 observer metadata。默认 `PROFILE_BOOT_MODE=mygo`，工作盘和工具盘分别为 `/dev/vd0`、`/dev/vd1`；`scripts/buildstorm-profile-linux.sh` 是薄 wrapper，设置 `PROFILE_BOOT_MODE=linux` 及 Linux kernel/map/manifest/initramfs 默认路径，把设备切换为 `/dev/vda`、`/dev/vdb`，强制 `PROFILE_CAPTURE=0`，然后直接执行共用 host。`PROFILE_SYSTEM` 只标识 observer 摘要中的系统，不能代替启动模式。QEMU 始终使用 `-name guest=buildstorm-profile,debug-threads=on`，daemon 才能完整、唯一地识别 8 个 vCPU 线程。

Linux observer 冒烟使用同一组公平性参数，只需改用 wrapper：

```sh
mkdir -p /tmp/linux-qemu-profile
PROFILE_RUN_ROOT=/tmp/linux-qemu-profile \
PROFILE_BASE_IMAGE=/home/redstone/src/oskernel2026-mygo-network-cagent/build/sdcard-la-pub.img \
PROFILE_CPUSET=0,2,4,6,8,10,12,14 \
PROFILE_DURATION_MS=20000 \
PROFILE_QEMU_OBSERVER=1 PROFILE_REQUIRE_SYMBOL_MANIFEST=1 \
scripts/buildstorm-profile-linux.sh
```

MyGO 构建把 `build/loongarch64/compat-initramfs.cpio` 嵌入内核，Linux wrapper 默认把同一 cpio 通过 `-initrd` 和 `rdinit=/linuxrc` 外部传入。observer 模式要求该文件可读，并把其 SHA-256 记录为 `guest_initramfs_sha256`；MyGO/Linux 成对实验必须使用与 MyGO 内核同次构建的 cpio，比较器会要求两侧该 SHA 精确相等。

guest 的挂载判定统一读取 `/proc/mounts`：`/mnt` 和 `/tmp/p` 必须作为独立挂载点出现；initramfs 命名空间中的 `/mnt/work/tgoskits/target`（进入 chroot 后为 `/work/tgoskits/target`）还必须以 `tmpfs` 类型出现。这里不能改用 BusyBox `mountpoint -q`，因为它依赖 `st_dev` 区分挂载边界，而 MyGO VFS 尚不能可靠提供该语义；`rcS`、host setup 和 guest runner 都以 `/proc/mounts` 为准。

命令末尾会打印精确 `run_dir`。其中的主要证据为：

- `summary.json`：外层 BuildStorm 窗口、Cargo progress、QEMU CPU 和 host PSI 汇总；
- `qemu-profile-summary.json`：observer 的规范化摘要，也是 `qemu_profile_compare.py` 的输入；
- `qemu-profile.jsonl`：daemon 记录的阶段、`/proc`、plugin、QMP/GDB 校验事件；
- `qemu-observer-plugin-summary.json`：QEMU 退出时写出的 plugin 配置和原始计数摘要；
- `profile.serial.log`、`host-samples.tsv`、`qemu-cpu-boundaries.tsv` 和 `qmp.log`：窗口边界与宿主侧佐证；
- `kernel-la`、`kernel.map`、`kernel.map.manifest`、`qemu-observer-plugin.so` 和 `metadata.env`：本轮实际使用的可复现身份。

`quality.valid` 要求 QEMU 进程身份有效、8 个 host vCPU 线程在整个窗口完整且 `(tid,start_ticks)` 稳定、采样暂停比例合格，并且 plugin 至少产生记录和内核栈样本，且无 invalid record、sequence gap 或 dropped record，leaf 符号化比例达到门限。低活动 vCPU 可能未跨过一个 `PROFILE_PLUGIN_PERIOD_INSNS` 周期，因此摘要分别输出 `plugin_observed_vcpus` 和 `plugin_unobserved_vcpus`，不要求 8 个 vCPU 都产生 plugin record；host vCPU 线程完整性仍必须是 8/8。guest 指令计数必须满足 `total = user + kernel`，窗口边界误差上限固定为 `2 * period * configured_vcpu_count`。

窗口停止时先生成 preliminary summary；host 随后通过 QMP 退出 QEMU 并等待 plugin 的 atexit summary 落盘，再关闭 daemon。daemon 会排空 datagram，并将 atexit 提供的每 vCPU 最终累计量与最后收到的 counter 对账，避免把窗口尾部未发出的周期内计数静默当成完整数据。只有 `quality.plugin_exit_reconciled=true` 才能令最终 `quality.valid=true`；summary 缺失、配置不符、计数倒退或 dropped 不一致都会使本轮失效，比较器也会拒绝未完成对账的目录。

### 公平比较和热点解读

比较一对有效 observer 目录时直接传目录，加载器会优先选择其中的 `qemu-profile-summary.json`：

```sh
python3 scripts/qemu_profile_compare.py \
  /tmp/mygo-qemu-profile/baseline-run \
  /tmp/mygo-qemu-profile/candidate-run \
  --required-speedup 2
```

比较器要求两侧 `quality.valid=true` 和 `quality.plugin_exit_reconciled=true`，并精确匹配 `workload`、vCPU 数量、`/proc` 与 stack 采样周期/超时、frame 上限、暂停比例上限、plugin period/stack bytes 和 unwind 模式。`metadata.environment` 整体也必须精确相等，至少包括 raw base image SHA、共享 `guest_initramfs_sha256`、每轮 cold target、容器 image ID 与 UID:GID、cpuset、8 GiB 内存、SMP、5 GiB tmpfs、QEMU version/machine/cpu/accel/name/debug-threads、toolchain、plugin SHA，以及 workload plan/script SHA。任一字段缺失或不一致都不能用于 MyGO/Linux 或优化前后的公平结论。

plugin 记录的当前 PC 可作为 leaf 样本；`hotspots[*].sample_kind=plugin-leaf` 是当前可靠的优化排序依据。`stack-scan-guess-v1` 只是按 8 字节扫描 guest 栈窗口并把落入 text 的值猜作返回地址，深层 frame 会混入任意栈字和陈旧地址。因此不得用 `call_paths` 或猜测出的非 leaf frame 证明调用关系、归因上层模块或决定优化优先级；在实现真正的 LoongArch unwind 之前，只使用 leaf PC hotspot。

## A/A 开销验证

先用 30–60 秒窗口冒烟，再执行三组各三次的 300 秒交错运行：

- `plain-off`：普通内核，`PROFILE_CAPTURE=0`；
- `profile-idle`：profile 内核，`PROFILE_CAPTURE=0`，测量静态 feature 成本；
- `counts`：profile 内核，`PROFILE_CAPTURE=1`、sampling/trace 关闭、`PROFILE_TIMING_SHIFT=8`，测量动态计数成本。

示例：

```sh
PROFILE_RUN_ROOT=/tmp/mygo-profile-aa \
PROFILE_BASE_IMAGE=/home/redstone/src/oskernel2026-mygo-network-cagent/build/sdcard-la-pub.img \
PROFILE_CPUSET=0,2,4,6,8,10,12,14 \
PROFILE_KERNEL=/tmp/kernel-la-model-profile \
PROFILE_LABEL=counts-1 PROFILE_CAPTURE=1 \
PROFILE_SAMPLING=0 PROFILE_TRACE_ENABLED=0 PROFILE_TIMING_SHIFT=8 \
PROFILE_DURATION_MS=300000 scripts/buildstorm-profile-host.sh
```

分别比较三次结果：

```sh
scripts/buildstorm-profile-compare.sh \
  /tmp/mygo-profile-aa/mygo-profile-plain-{1,2,3}.*/summary.json -- \
  /tmp/mygo-profile-aa/mygo-profile-idle-{1,2,3}.*/summary.json

scripts/buildstorm-profile-compare.sh \
  /tmp/mygo-profile-aa/mygo-profile-idle-{1,2,3}.*/summary.json -- \
  /tmp/mygo-profile-aa/mygo-profile-counts-{1,2,3}.*/summary.json
```

验收条件为共同 Cargo milestone 或进度的退化不超过 2%，组内 CV 不超过 5%。START/STOP 边界观测延迟同时受 6 秒绝对上限和窗口时长 2% 的相对上限约束；300 秒窗口因此最多允许 6 秒。任何快照不单调、timing 样本少于 32、PC sample dropped、trace overwritten、镜像/config 不一致都会被标记为无效或低可信度。低可信 timing 不参与热点排序，但精确 calls/bytes/packets 仍可使用。

## 优化交付

只在两项 A/A 验证均通过后增加分类插桩或修改热路径。候选优化必须以三次 clean-overlay 运行复核；要求 2× 时设置 `PROFILE_REQUIRED_SPEEDUP=2` 运行比较脚本。阶段性成果需记录 summary、串口日志、内核哈希和提交 ID，并运行 `cargo fmt --all`、受影响 host 测试、完整 `make kernel-la` 与 QEMU 验证。

## tmpfs 模型校正

旧 profile runner 只删除 `target/debug`，实际仍在 extfs 上构建，与正式 init 的 5 GiB tmpfs 路径不一致。修正后首次 `cargo:440` 诊断使用内核哈希 `204790961a43`，串口确认 `@@PROFILE_TARGET_FS type=tmpfs`；60 分钟上限触发时到达 `439/446`，因此不能作为验收样本，但可用于定位增长区间。相对 `0/446` 的里程碑为：`64=210.65s`、`128=660.99s`、`256=1919.86s`、`384=2710.57s`。最重的 `128→256` 单段耗时约 20 分 59 秒，后期进度条不是唯一瓶颈。

在约 `258/446` 的只读现场中，8 个 QEMU vCPU 线程平均占用约 `74%–91%` 主机 CPU，guest runqueue 仍有 4 个待运行任务，并行存在 7 个 rustc/cc1，排除了整体调度停转。`aws-lc-sys` build script 当时已运行约 25 分钟；内核堆约 1.09 GiB，私有文件页缓存累计约 1610 万次 hit、7.7 万次 miss。下一轮应从 `cargo:384` 开始覆盖完整收敛段，并把 aws-lc C 构建和高频 resident fault 作为独立候选验证。

## LoongArch ASID 阶段结果

BuildStorm 在 300 秒内会创建约 4,000–4,500 个 `VmSpace`。旧切换路径即使硬件 ASID 不冲突，也在每次地址空间切换时执行全 TLB 失效。当前实现为存活地址空间分配独占硬件 ASID，并用地址空间 TLB 代际闭合 PTE 更新与并发激活竞态；仅首次使用、ASID 复用、共享 fallback 或错过 shootdown 时全刷。

固定 300 秒 counts-only 三轮的 Cargo 64 milestone 为 `208.60s / 197.98s / 222.67s`，均值 `209.75s`、CV `4.82%`。相对既有 counts 基线均值 `239.02s` 下降 `12.25%`，比较脚本返回 `accepted: true`。同机单轮 before 为 `250.65s`，对应下降 `16.32%`；窗口末进度均值从 `82` 提升到 `92.67`。

## Fault-around 精确计数

`performance-profile` 内核在每 CPU 独立且按 64 字节 cache line 对齐的 Relaxed 原子槽中累计 fault-around 工作量，`/proc/meminfo` 会输出 `FaultAroundWindows`、`Requested`、`Prepared`、`Commits`、`Installed` 和 `Raced`。profile guest 已在窗口前后采集 meminfo，因此分析时使用 after-before 增量；普通内核不会编译记录调用。

`Windows` 只统计成功形成 prepared 前缀的窗口，`Requested` 表示策略计划的窗口页数，`Prepared` 表示真正完成读取或命中缓存的前缀；`Commits` 只统计通过 VMA 快照重验证的提交，VMA 变化会使它与 `Windows` 存在差值。仅在 guest gate 保证边界静止、没有跨 before/after 的在途 prepare/commit 时，窗口增量必须满足 `Installed <= Prepared <= Requested` 和 `Raced <= Commits <= Windows`。`Raced` 表示锁外读页期间另一 CPU 已先安装真实 fault 页；`Prepared - Installed` 还包含 VMA retry、并发前缀截断、页表失败和未采用投机页。计数区分计划量、实际 MM 工作与 PTE 安装量，不把预装页误称为已被用户代码消费。

单轮 300 秒 counts-only 校验得到 `Windows=344570`、`Requested=Prepared=5450848`、`Commits=344570`、`Installed=3209466`、`Raced=14`：平均每个窗口准备 `15.82` 页、安装 `9.31` 页，`41.12%` 的 prepared 页未安装；没有 prepare 缩窗或 VMA retry，竞态也不足以解释差值。该轮 Cargo 64 milestone 为 `211.28s`，相对三轮 ASID 基线均值 `209.75s` 退化 `0.73%`，落在既有方差内。短窗口中最终的单写者原子 load/store 版本为进度 20、QEMU CPU `318.26s`，对旧 ASID 冒烟的进度 21、`321.88s` 未显示动态计数开销。300 秒窗口末进度 84 低于基线 91–94，因此后续优化仍须以三轮共同 milestone 验收，不能只比较单轮末进度。

提交损失进一步拆成 VMA retry、真实 fault race、首碰撞后已存在的 PTE、碰撞后的空洞和页表失败，并要求静止增量满足 `Prepared = Installed + VmaRetryPages + RacedPages + DuplicatePages + DiscardedUnmapped + MapFailedPages`。60 秒诊断样本精确闭合为 `1606098 = 955798 + 0 + 192 + 643154 + 6954 + 0`；`55190/101533` 个窗口发生首碰撞，未安装页的 `98.90%` 已有 PTE，真正被连续前缀策略丢弃的空洞仅 `1.07%`。

曾尝试在 prepare 前按首个 resident PTE 截窗；它把短样本的 collision/duplicate 降为 0，但没有改变每窗口实际安装页数，只省掉了热文件缓存命中。三轮 300 秒 Cargo 64 milestone 为 `223.97s / 208.94s / 217.62s`，均值 `216.84s`、CV `2.84%`，相对 ASID 基线均值 `209.75s` 稳定退化 `3.38%`，比较脚本返回 `accepted: false`；末进度均值也从 `92.67` 降到 `89.67`。该改动已回退。后续不得把 `Prepared - Installed` 直接视为等量 I/O 浪费，应转向不可避免的 PTE 安装、真实 cache miss 和 page-fault 分段耗时。

## Page-fault 分段模型

profiling event 36–39 追加为 `page_fault_resident`、`prepare`、`commit` 和 `single`，不改变既有 event ID。它们只在真实硬件 fault 路径记录；`ensure_page_access` 触发的软件 prefault 不进入分段。使用 `PROFILE_EVENT_MASK=0xf008000000` 可只开启总 page fault 与四个子阶段，四个子阶段彼此不嵌套，但 prepare/single 内仍包含 VFS/block 子调用。

60 秒 counts-only 样本中，总 page fault 估算 on-CPU 为 `157.50s`；prepare `77.87s`（`49.4%`）、single `41.28s`（`26.2%`）、commit `9.97s`（`6.3%`）、resident `9.56s`（`6.1%`），未覆盖的 VMA 查找与分派约 `18.81s`。prepare 的 `94,934` 次调用完成 `1,544,952` 页，其中私有缓存 hit/miss 为 `1,272,520 / 66,473`。下一步应拆分 prepare 的 cache hit 查找和真实 miss 读页，不能把 PTE commit 当作当前第一热点。

event 40/41 继续区分有稳定代际缓存的 miss fill 和无 cache key 的 uncached fill。使用 `PROFILE_EVENT_MASK=0x30000000000`、`PROFILE_TIMING_SHIFT=8` 的 60 秒归因样本中，cache fill `59,621` 次，与 meminfo 的 `60,488` 次 miss 基本一致，估算 on-CPU `59.80s`、均值约 `1.00ms`；uncached fill `232,921` 次，估算 on-CPU `11.20s`、均值约 `48us`。两者约 `71.0s`，与上一轮 prepare `77.87s` 高度闭合，因此 lookup/循环本身不是主要杠杆，约 6 万次同步 cache-miss 读页才是。细粒度 scope 会显著降低该诊断内核吞吐，只能用于来源占比，不能与低扰动 counts-only 样本比较性能。

## LoongArch 陷阱扩展状态模型

`performance-profile` 内核按 CPU 累计用户 syscall、其他用户陷阱，以及入口实际保存 FPU/LSX 状态的次数；`/proc/meminfo` 通过六个 `ProfileLa*` 字段导出累计值。普通内核不会编译记录调用。与 fault-around 相同，分析时只使用静止窗口前后的差值。

内核哈希 `67f2c0ded667` 的 60 秒 BuildStorm counts-only 样本关闭所有 event timing、sampling 和 trace，得到 `252,690` 次 syscall 与 `564,922` 次其他用户陷阱。两类陷阱的 FPU 和 LSX 保存次数都与陷阱总数完全相等，即四项保存比例均为 `100%`。现有入口每次保存并恢复 256 字节 FPU 与 512 字节 LSX 状态，因而该窗口的 `817,612` 次陷阱至少搬运约 `1,255,852,032` 字节（`1.17 GiB`）扩展寄存器数据，且这些汇编成本不在 Rust scope 计时中。下一步应验证按 CPU 延迟拥有扩展状态、仅在任务切换或状态访问时保存，不能继续把全部未归因时间归入页故障或 VFS。

曾验证在 `LSX_SAVED` 时跳过与 VR 低 64 位重叠的 32 个标量 FPR 保存和恢复，同时保留 FCSR/FCC，并在导出信号上下文时从 LSX 补齐冗余标量编码。8 个用户进程反复写入全部 LSX/FPR、执行 `sched_yield` 并逐位校验的直接引导测试通过。固定 300 秒性能窗口中，基线三轮末进度为 `75 / 61 / 57`，候选两轮有效值为 `59 / 54`；另一候选轮停在 `59` 后发生 runner 收尾超时，按规则作废。测试期间绑定核心频率约 `3.0 GHz`、主机温度达到 `83°C`，基线组内方差超过验收上限，但候选在相邻慢性能态下也未超过基线，不能证明正收益，因此该改动未合入。后续应先把 CPU 频率、温度或更早共同 milestone 纳入模型，再评估汇编级小优化。

## tmpfs 负载的用户缺页组成

内核哈希 `4709a94ef09f` 将新用户进程改为首次真实 LSX 指令才开启 SXE，并同时统计硬件用户缺页的 backing/access/resident 类型。60 秒 counts-only BuildStorm 窗口受主机外部负载干扰，因此只使用精确计数，不使用进度或耗时。`221,312` 次 syscall 中 `87.22%` 保存 LSX，`326,473` 次其他用户陷阱中 `86.23%` 保存 LSX，即 lazy-SXE 只能让总陷阱的 `13.37%` 跳过 LSX 搬运；Rust 工具链仍会很早使用 LSX，它不是主要杠杆。

同一窗口共有 `261,780` 次 nonresident 硬件缺页：匿名 Load/Store 分别为 `7,473 / 148,752`，私有文件 Load/Store/Exec 分别为 `33,272 / 24,854 / 47,428`，另有 1 次匿名权限缺页。匿名页占 nonresident 缺页的 `59.68%`，其中匿名 Store 单项占 `56.82%`。这是 tmpfs 输出路径下最大的非 extfs 候选，但直接预分配 8 页会让稀疏 `mmap` 最坏放大 8 倍物理内存。下一步先用不改变页表的影子窗口计数估算真实可消除陷阱，再决定是否接入批量分配。

## 匿名写缺页预映射与 extfs 映射缓存

在 tmpfs 输出模型下，匿名 Store nonresident 缺页约占总 nonresident 缺页的 `56.82%`。现已把影子窗口估算落地为生产路径：私有匿名写缺页最多向高地址预映射 `4` 页，并导出 `ProfileAnon*` 计数用于窗口差分。同时为 extfs 引入按 inode 的块映射代际缓存，避免顺序读反复重建 extent 映射。

同一 frozen initramfs、base image、cpuset 和旧版 guest runner 下的 300 秒拆分结果如下。这里的样本没有按 baseline/candidate 交错，且每个候选不足三轮，只能用于筛选，不能验收：

| 候选 | progress | cargo:64 | QEMU CPU |
| --- | ---: | ---: | ---: |
| baseline-1 | 112 | 163.14s | 2012.15s |
| baseline-2 | 115 | 166.40s | 2013.42s |
| extfs-only | 112 | 155.65s | 1991.20s |
| anon-only-1 | 125 | 151.32s | 2043.67s |
| anon-only-2 | 107 | 155.11s | 2036.80s |
| anon + extfs | 109 | 156.80s | 2071.19s |

extfs-only 的 `0→64` 约改善 `4.6%–6.5%`，窗口末基本中性。anon-only 的 `0→64` 约改善 `6%–9%`，但末进度方差很大；组合没有显示叠加收益。当前只能说明两个实现值得继续控制变量，不能表述为已经证明完整构建正收益。后续 guest runner 已增加冻结失败诊断并改变 workload script SHA，因此必须在最终 runner 上重跑 baseline，不能直接复用本表验收。

## 自定义 memset 实验（未作为主收益）

仅覆写 ABI `memset`（64 字节标量展开）相对 compiler_builtins 的成对 plain 窗口：

| 窗口 | 内核 | progress | QEMU CPU | cargo:64 |
| --- | --- | ---: | ---: | ---: |
| 60s baseline | `bbcfecd35435` | 16 | 316.32s | n/a |
| 60s candidate | `4a458cb14e3b` | 17 | 301.35s | n/a |
| 300s candidate | `4a458cb14e3b` | 81 | 1941.86s | 254.5s |
| 300s baseline-1 | `bbcfecd35435` | 112 | 2012.15s | 163.14s |
| 300s baseline-2 | `bbcfecd35435` | 115 | 2013.42s | 166.40s |

300 秒样本中，候选到 `cargo:64` 比两轮 baseline 慢约 `53%–56%`，窗口末 progress 也低约 `28%–30%`。即使其 QEMU CPU 略低，也不能抵消实际构建吞吐的严重退化。自定义实现已撤回，当前内核重新使用 compiler_builtins 的 `memset`。

## memcpy 与 allocator registry 实验

`memcpy-only` 候选只对双端 8 字节对齐且 `len >= 64` 的复制使用 64 字节展开，其余回退 compiler_builtins。单轮 300 秒结果为 progress `109`、`cargo:64=156.41s`、QEMU CPU `2010.59s`。它只显示早段小收益，窗口末没有正收益，也没有满足三轮低方差门槛，因此未合入。

allocator registry 字段级候选把约 96 字节的 `RegistryNode` 整体读改为 `ptr/next` 字段访问，20 秒 smoke 通过。两轮 300 秒都在收尾阶段发生 `window freeze timed out`：一轮此前到达 progress `111`，另一轮只到 `9`，均未产生有效 summary。该结果不能证明性能回退，但也不能作为正收益证据；在冻结诊断能输出 PGID 成员 state/wchan 之前，不应继续用无 summary 的进度值下结论。

## tmpfs 页槽池筛选

旧 `668646df` 使用单个 superblock 全局锁管理 64 KiB slab。历史约 240 秒窗口只到 progress `53`，同阶段对照接近 `90`，已明确否决该实现。当前 `0e143006` 是后继方案：保留 16 个 4 KiB slot 的 slab，但改为 per-CPU shard、`1→4→8` 批量租槽，并在 writable fd 关闭时归还未用槽；旧 worktree 的合并写元数据修正和 VFS 测试也已经包含在当前 HEAD。

后继组合历史长窗口为 `912s / 229`，相邻对照为 `898s / 208`，约有 `10%` 进度提升；但该组合还同时包含 lazy stack、元数据和 init 修正，不能把收益全部归因于页槽池。结论是保留当前分片批量实现，不再重复移植或测试旧全局池。
