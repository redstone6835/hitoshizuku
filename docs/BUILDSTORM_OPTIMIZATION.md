# BuildStorm 性能分析与验收

## 目标与测量原则

最终目标是 `tg-xtask` 不超过 15 分钟、后续内核构建不超过 5 分钟，并以三次独立运行确认。所有对比使用固定容器 `zhouzhouyi/os-contest:20260510`、8 GiB 内存、8 个 QEMU vCPU、同一只读 raw 基准盘和每轮新建的 qcow2 overlay。不得复用 guest 的 `target/debug`，也不得在测量期间并发运行 Cargo、Make 或其他 QEMU。

`scripts/buildstorm-profile-host.sh` 使用 guest gate 对齐窗口：工作负载进程组先停止，profile-on/off 都经过相同的 START/STOP 控制；停止时先冻结计数器并记录 host/QEMU 边界，再在窗口外导出快照。summary 同时记录 Cargo progress、QEMU CPU、主机 PSI、控制延迟和镜像哈希。

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

## LoongArch ASID 阶段结果

BuildStorm 在 300 秒内会创建约 4,000–4,500 个 `VmSpace`。旧切换路径即使硬件 ASID 不冲突，也在每次地址空间切换时执行全 TLB 失效。当前实现为存活地址空间分配独占硬件 ASID，并用地址空间 TLB 代际闭合 PTE 更新与并发激活竞态；仅首次使用、ASID 复用、共享 fallback 或错过 shootdown 时全刷。

固定 300 秒 counts-only 三轮的 Cargo 64 milestone 为 `208.60s / 197.98s / 222.67s`，均值 `209.75s`、CV `4.82%`。相对既有 counts 基线均值 `239.02s` 下降 `12.25%`，比较脚本返回 `accepted: true`。同机单轮 before 为 `250.65s`，对应下降 `16.32%`；窗口末进度均值从 `82` 提升到 `92.67`。

## Fault-around 精确计数

`performance-profile` 内核在每 CPU 独立且按 64 字节 cache line 对齐的 Relaxed 原子槽中累计 fault-around 工作量，`/proc/meminfo` 会输出 `FaultAroundWindows`、`Requested`、`Prepared`、`Commits`、`Installed` 和 `Raced`。profile guest 已在窗口前后采集 meminfo，因此分析时使用 after-before 增量；普通内核不会编译记录调用。

`Windows` 只统计成功形成 prepared 前缀的窗口，`Requested` 表示策略计划的窗口页数，`Prepared` 表示真正完成读取或命中缓存的前缀；`Commits` 只统计通过 VMA 快照重验证的提交，VMA 变化会使它与 `Windows` 存在差值。仅在 guest gate 保证边界静止、没有跨 before/after 的在途 prepare/commit 时，窗口增量必须满足 `Installed <= Prepared <= Requested` 和 `Raced <= Commits <= Windows`。`Raced` 表示锁外读页期间另一 CPU 已先安装真实 fault 页；`Prepared - Installed` 还包含 VMA retry、并发前缀截断、页表失败和未采用投机页。计数区分计划量、实际 MM 工作与 PTE 安装量，不把预装页误称为已被用户代码消费。

单轮 300 秒 counts-only 校验得到 `Windows=344570`、`Requested=Prepared=5450848`、`Commits=344570`、`Installed=3209466`、`Raced=14`：平均每个窗口准备 `15.82` 页、安装 `9.31` 页，`41.12%` 的 prepared 页未安装；没有 prepare 缩窗或 VMA retry，竞态也不足以解释差值。该轮 Cargo 64 milestone 为 `211.28s`，相对三轮 ASID 基线均值 `209.75s` 退化 `0.73%`，落在既有方差内。短窗口中最终的单写者原子 load/store 版本为进度 20、QEMU CPU `318.26s`，对旧 ASID 冒烟的进度 21、`321.88s` 未显示动态计数开销。300 秒窗口末进度 84 低于基线 91–94，因此后续优化仍须以三轮共同 milestone 验收，不能只比较单轮末进度。
