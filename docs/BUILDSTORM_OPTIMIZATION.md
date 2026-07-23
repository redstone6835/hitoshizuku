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

验收条件为共同 Cargo milestone 或进度的退化不超过 2%，组内 CV 不超过 5%，START/STOP 边界观测延迟不超过 1 秒。任何快照不单调、timing 样本少于 32、PC sample dropped、trace overwritten、镜像/config 不一致都会被标记为无效或低可信度。低可信 timing 不参与热点排序，但精确 calls/bytes/packets 仍可使用。

## 优化交付

只在两项 A/A 验证均通过后增加分类插桩或修改热路径。候选优化必须以三次 clean-overlay 运行复核；要求 2× 时设置 `PROFILE_REQUIRED_SPEEDUP=2` 运行比较脚本。阶段性成果需记录 summary、串口日志、内核哈希和提交 ID，并运行 `cargo fmt --all`、受影响 host 测试、完整 `make kernel-la` 与 QEMU 验证。
