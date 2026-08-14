# RISC-V64 指令权重微基准：方法、数据与结论论证

> 本报告记录 MyGO OS 在 QEMU TCG 环境下构建 RISC-V64 指令成本模型的完整实验方法、质量门禁、可视化数据和结论边界。报告中的“权重”表示固定宿主、固定 QEMU、固定执行 pattern 下的稳健典型 CPU 时间，不表示真实 RISC-V 芯片的流水线 latency/throughput。

实验日期：2026-08-09（Asia/Shanghai）；正式 run 标识前缀：`run-20260809T042758Z-measure-90861-*`。

## 1. 结论摘要

- 正式实验包含 **12 个独立 QEMU run、213 个执行上下文、153,360 个计时窗口、76,680 个 probe/baseline pair**。
- 三档 batch 各产生 51,120 个窗口；AB/BA 窗口为 76,706/76,654，配对纯度范围为 1.0–1.0。
- 检测到 670 个含动态翻译的窗口；模型剔除 669 个污染 pair，保留 76,011 个 pair。
- 目标汇编在保留窗口中总计执行 **2,173,366,272 次**。
- 主估计使用 marker-only QEMU vCPU thread CPU time、成对差分、异方差 Huber 回归和 run-cluster moving-block bootstrap。
- 正式 bootstrap 为 **999/999 有效**；主权重使用全族 95% max-standardized-deviation 同时区间。
- 模型层有 4 个 high-confidence、209 个 low-confidence 上下文。catalog 层严格赋权 3 类，另保留 183 类单一探索估计。
- 严格可发布 catalog 权重只有 `divuw`、`rem`、`remw`；`time` CSR 在模型层通过，但因 CSR 上下文受限而禁止转成通用 catalog 权重。
- 大多数 low-confidence 不是“主计时没有信号”：主同时区间过宽仅 16 项；主要降级原因是 plugin-off、batch、顺序和漂移等辅助稳健性门禁。

## 2. 范围与不可外推内容

本实验回答的是：在 QEMU 10.0.2、TCG 单线程翻译、单 vCPU、1 GiB 客体内存和指定 probe pattern 下，一条目标指令相对同形态 baseline 的稳健典型成本是多少。

本实验不直接回答以下问题：

- 真实 RISC-V 处理器的硬件 cycle latency、发射吞吐或乱序执行行为；
- 未采样的 cache/TLB miss、页错误、跨核争用、不同 FP operand class 或不同分支历史；
- RVV、B/crypto、Zacas、Zfh、H 等当前 catalog 未出现或 decoder 未覆盖的扩展；
- CSR、特权、陷阱和 cache-block 操作在任意系统状态下的唯一通用成本。

因此，报告同时保留 `execution pattern`、raw encoding 和语义 key，不把同名助记符静默平均。

## 3. 可复现实验身份

| 项目 | 固定值 |
| --- | --- |
| 容器 | `zhouzhouyi/os-contest:20260510` |
| QEMU | `QEMU emulator version 10.0.2` |
| machine | `virt`，`virtio-mmio.force-legacy=false` |
| accel | `tcg,thread=single` |
| vCPU / 内存 | `-smp 1` / `-m 1G` |
| 客体输出 | `-nographic -no-reboot -rtc base=utc` |
| 正式 run 数 | 12 |
| probe blocks / rounds | 4 / 10 |
| batch | 4096、16384、65536 条目标指令 |
| bootstrap | 999 replicates，8 worker |
| 正式输出目录 | [`build/riscv-instruction-weight-runs/formal-v2-20260809`](../build/riscv-instruction-weight-runs/formal-v2-20260809/) |
| BuildStorm catalog | [`instruction-catalog.jsonl`](../build/buildstorm-rv-runs/1200s-current-20260809/buildstorm-profile-rv-buildstorm-1200s-current.Sn74Gv/instruction-catalog.jsonl) |

### 3.1 正式产物哈希

| 产物 | SHA-256 |
| --- | --- |
| 正式 kernel-rv | `f963e6b6f72a9892574283128bc0000916938d9cec12068ea44694cfd3655a6e` |
| probe ELF | `6aa736ae988614bea7972edba83e84d5a3893835e83e2024f6508d55ce8a4aa7` |
| QEMU plugin | `ff7a28701a20fba6b7f1c99899a7d9052e58e6a75f673b467ec6a07c2e4d2c87` |
| samples.jsonl | `7fe88af482e66b858d808d2f590b0310cad906694e496320cd17ad33b933ec16` |
| weights.json | `d8cbf38ec15de2819fbf4d781dabc5d55a6712048eb0f213eddea4a613c70ffb` |
| catalog-weights.json | `bb6ee48ec728ed12538f6dc2ef1e6b496e50b9d9abbfc8732e399c6042a39110` |

注意：这些是正式采样 manifest 中记录的身份，不应使用之后重建的同名 `kernel-rv` 替代。

## 4. 测量链路

```text
BuildStorm catalog -> 规范语义 key -> probe/baseline 汇编 kernel
                                      |
                                      +-> validation plugin: 精确 raw encoding 计数
                                      |
                                      +-> timing plugin: 仅 start/stop marker 计时
                                      |
                                      +-> plugin-off guest: 插桩扰动对照
                                      v
                              pair merge 与闭合校验
                                      v
                     Huber WLS + run/order/drift/batch 模型
                                      v
                   run-cluster moving-block bootstrap (999)
                                      v
                     同时区间 + 稳健性门禁 + catalog 映射
```

关键实现：

- [探针与汇编 kernel](../userland/tests/riscv_instruction_weight_probe.c)；
- [QEMU 双模式插件](../tools/qemu-plugins/riscv_instruction_weight.c)；
- [正式 runner](../scripts/riscv-instruction-weight.sh)；
- [样本闭合与 merge](../scripts/merge-riscv-instruction-weight-samples.py)；
- [统计模型](../scripts/rv_instruction_microbench_model.py)；
- [catalog 映射](../scripts/map-riscv-instruction-weights.py)。

## 5. 探针设计

### 5.1 批量执行与 baseline

每个汇编 kernel 用 `.rept 1024` 生成目标槽，再由外层调用次数形成 4096、16384、65536 三档 batch。每个 probe 配一个同形态 baseline：普通算术通常对比同宽 `nop`；分支、跳转、栈和 SC 等路径使用专门 baseline，保证控制流和公共指令尽量一致。

若写成

$$T_{probe}=C+N\theta+\epsilon_p,\qquad T_{base}=C+\epsilon_b,$$

则配对差分

$$d_i=\frac{T_{probe,i}-T_{base,i}}{N_i}\approx\theta+\frac{\epsilon_{p,i}-\epsilon_{b,i}}{N_i}$$

消除了 marker、调用、循环、返回和计时器等共同成本 $C$。这比直接用 `probe_time / N` 更能抵抗固定开销。

### 5.2 执行 pattern

同一语义在不同上下文下可能具有不同成本，因此探针显式区分：dependency/independent、hot-load/hot-store、taken/not-taken branch、direct/indirect jump、reservation-pair、FP dependency/convert/compare、CSR 编号和 fence immediate。

这种区分解释了为什么一个 mnemonic 不一定得到一个全局标量。例如 `beq` 的 taken 和 not-taken 是两个上下文；`jalr` 的 call、link、jump、ret 也不能合并。

### 5.3 AB/BA 与预热

probe-first 和 baseline-first 顺序随机化，防止固定先后顺序与宿主漂移混淆。每个窗口前预热相同 kernel/path；FP kernel 在 marker 外重置 FCSR 并装载固定 normal 操作数；栈、分支和跳转都使用确定性参数。

## 6. QEMU 插件方法

### 6.1 validation 模式

validation 模式只统计 probe ELF 可执行 LOAD 半开区间 `[user_min_pc, user_max_pc)` 内的 raw encoding。它用于证明：目标 encoding 动态计数等于 requested count，probe/baseline canonical 差分完全闭合，且没有把低地址 firmware 或其他客体路径误算成 probe。

正式 validation footer：12780 个窗口，`nested_starts=0`、`inactive_stops=0`、`translation_failures=0`、`timer_failures=0`。

### 6.2 timing 模式

timing 模式只在 start/stop marker 的 TB 上注册执行 callback，普通指令没有执行 callback。主时钟为 vCPU 线程的 `CLOCK_THREAD_CPUTIME_ID`，避免把宿主调度等待直接计入 wall time。

每个正式 timing run 都有 12780 个窗口，12 个 run 均无 nested/inactive marker、translation failure 或 timer failure。

### 6.3 plugin-off 对照

每个 timing run 后以相同 run-id 和探针参数再启动一次不加载插件的 QEMU，使用客体计时输出对照。这个门禁很保守：plugin-on 固定先于 plugin-off，仍可能与宿主时间漂移混杂；低成本指令的比值也容易因分母接近零而放大噪声。因此 plugin-off 失败会降低严格质量，但不能单独证明主 CPU-time 点估计错误。

## 7. 正式数据矩阵与完整性

| 指标 | 数值 |
| --- | ---: |
| 独立 QEMU run | 12 |
| 执行上下文 | 213 |
| 原始窗口 | 153,360 |
| probe 窗口 | 76,680 |
| baseline 窗口 | 76,680 |
| 原始 pair | 76,680 |
| 保留 pair | 76,011 |
| 翻译污染 pair | 669 |
| 保留目标指令总数 | 2,173,366,272 |
| purity 最小/最大 | 1.000 / 1.000 |

### 7.1 Pair 过滤漏斗

```text
原始 pair                           76,680  ##################################################
保留 pair                           76,011  ##################################################
翻译污染剔除                          669  #
```

保留率为 99.128%。每个上下文保留 251–360 对；剔除后所有 213 个上下文和全部 12 个 run 仍可拟合。

### 7.2 Batch 与顺序平衡

```text
batch=4096                         25,560 pair  ##########################################
batch=16384                        25,560 pair  ##########################################
batch=65536                        25,560 pair  ##########################################

AB                                  38,353 pair  ##########################################
BA                                  38,327 pair  ##########################################
```

## 8. 统计模型

### 8.1 回归式

每个 pair 先归一化为 ns/target-instruction，然后拟合：

$$d_i=\theta+\alpha_{run(i)}+\beta_o O_i+\beta_d D_i+\beta_b\log(N_i/N_0)+\beta_t Q_i+\epsilon_i.$$

其中 $\theta$ 是目标对 baseline 的 contrast；$\alpha_{run}$ 吸收不同 QEMU 进程的固定效应；$O_i$ 是 AB/BA；$D_i$ 是 run 内时间位置；$N_i$ 是 batch；$Q_i$ 是翻译事件率。正式高质量数据在拟合前已经剔除所有已观测翻译污染。

若 baseline 本身也是被测 encoding，模型通过 control-reference 图递归恢复绝对成本；循环、缺失或歧义引用会变成不可辨识错误，不按 mnemonic 猜测。

### 8.2 Huber IRLS 与异方差

Huber 损失在小残差区间使用二次损失，在大残差区间使用线性增长：

$$\rho_\delta(r)=\begin{cases}\frac{1}{2}r^2,&|r|\le\delta,\\\delta(|r|-\frac{1}{2}\delta),&|r|>\delta.\end{cases}$$

本模型使用 $\delta=1.345$，通过 IRLS 迭代；不同 batch 的 MAD 方差估计向全局尺度收缩后形成异方差权重。最终估计是稳健中心，不是简单算术均值。

### 8.3 有效样本量

先用 Huber×异方差联合权重计算 Kish ESS：

$$ESS_{Kish}=\frac{(\sum_i w_i)^2}{\sum_i w_i^2}.$$

再将同一 run 内 residual 按 probe-round block 聚合，估计积分自相关时间 $\tau$：

$$ESS=ESS_{Kish}/\tau.$$

最终 ESS 中位数为 210.26，最小 124.96，最大 297.34，均远高于 20 的最低门禁。

### 8.4 分层 moving-block bootstrap

不能把 76011 个 pair 当成独立同分布样本，否则会产生伪重复。最高独立层级是 QEMU run，因此每次 bootstrap：

1. 对 12 个 run 有放回重采样；
2. 在每个被抽中的 run 内，按长度 4 的 probe-round block 循环重采样；
3. 对每个上下文完整重跑 Huber 拟合和 control 解析；
4. 保留所有 key 同一 replicate 的联合分布。

### 8.5 全族同时区间

对第 $b$ 个 replicate 计算：

$$M_b=\max_k\left|\frac{\hat\theta_k^{(b)}-\hat\theta_k}{s_k}\right|.$$

令 $c$ 为 $M_b$ 的 95% 分位，则：

$$CI_k=[\hat\theta_k-cs_k,\hat\theta_k+cs_k].$$

这里是 max-standardized-deviation 区间，不是 replicate-specific studentized max-t。它控制整族主权重的同时覆盖，避免 213 个逐项 95% 区间产生严重 family-wise 假阳性。

### 8.6 负权重和零成本

物理权重不能为负，但显著负的无约束估计也不能直接截成 0。模型保留 unconstrained estimate/CI；只有整个同时区间位于 ±0.15 ns 实用零区间内时，才发布 `zero_cost_equivalent=true` 和物理零权重。否则负估计不发布。

### 8.7 严重异常和 Wilson 门禁

Huber 权重低于 0.25 相当于残差大于约 $1.345/0.25=5.38$ 个 robust scale，才计为严重异常。模型对严重异常比例计算 95% 单侧 Wilson 上界，并要求其不超过 10%。正常 Huber 降权本身不会被误判为失败。

## 9. 质量门禁

| 门禁 | 阈值/要求 |
| --- | --- |
| pair 数 | ≥ 30 |
| Kish+ACF ESS | ≥ 20 |
| 独立 run | ≥ 10 |
| batch level | ≥ 3，且每档至少 4 pair |
| encoding purity | ≥ 0.99 |
| 主同时区间相对半宽 | ≤ 15%，且正权重下界 > 0 |
| 实用零区间 | 同时区间完整位于 ±0.15 ns |
| bootstrap | ≥ 999，valid fraction ≥ 0.99 |
| AB/BA 平衡 | 较少一侧 ≥ 35% |
| 严重异常 | `Huber weight < 0.25` 的 Wilson 上界 ≤ 10% |
| run 异质性 | I² 与 prediction interval 联合门禁 |
| order/drift/batch | 全族同时区间落入 10% 实用等价区间 |
| cross-clock | ratio CI 位于 [0.75, 1.50]；零成本使用差值 |
| plugin-off | ratio CI 位于 [0.85, 1.15]；零成本使用差值 |
| IRLS / 设计矩阵 | 全局和逐 run 收敛、条件数 ≤ 1e8 |

## 10. Bootstrap 收敛与稳定性

| 指标 | 99 次预检 | 999 次正式 |
| --- | ---: | ---: |
| valid replicates | 99 | 999 |
| valid fraction | 1.000 | 1.000 |
| max-stat critical | 3.663794 | 4.128065 |
| 95% quantile Monte Carlo SE | 0.021904 | 0.006895 |
| diagnostic critical | 6.305331 | 6.145232 |
| auxiliary critical | 4.151173 | 4.178241 |

999 次相对 99 次的同时区间宽度比中位数为 1.1386，95% 分位为 1.2923。区间没有因 replicate 增多而机械缩窄；更充分的尾部分位采样反而使多数区间更保守，同时把分位概率 Monte Carlo SE 从 0.0219 降至 0.00690。

## 11. 数据分布可视化

### 11.1 模型质量

```text
high-confidence                                       4  #
low-confidence                                      209  ##########################################
```

### 11.2 Catalog 发布状态

```text
high-confidence                                       3  #
low-confidence                                      183  ####################################
context-dependent                                     8  ##
restricted-context                                  215  ##########################################
```

这里的 183 个 low-confidence 是有单一数值上下文的探索估计；8 个 context-dependent 类存在多个不等价上下文，顶层不聚合数值；215 个 restricted 类不发布。

### 11.3 Catalog 扩展分布

```text
zicsr                                               207  ##########################################
i                                                    76  ###############
a                                                    42  #########
c                                                    38  ########
d                                                    20  ####
m                                                    12  ##
f                                                     6  #
priv                                                  5  #
zicboz                                                1  #
zifencei                                              1  #
zihintpause                                           1  #
```

### 11.4 质量失败原因

同一上下文可以同时触发多个门禁，因此以下计数不可相加为 209。

```text
plugin-off-check-divergent                          169  ##########################################
batch-size-nonlinearity                             157  #######################################
drift-effect-not-equivalent                          98  ########################
cross-clock-check-divergent                          98  ########################
order-effect-not-equivalent                          82  ####################
too-many-severe-outliers                             32  ########
cross-run-heterogeneity-high                         22  #####
simultaneous-ci-too-wide                             16  ####
cross-clock-check-unavailable                        15  ####
plugin-off-check-unavailable                         15  ####
per-run-irls-not-converged                           10  ##
```

只有 16 项主同时区间过宽；大量降级来自 plugin-off 和 nuisance-effect 等价性。这证明“low-confidence”不能直接解释为主计时没有统计信号。

### 11.5 代表性快速路径成本（线性尺度）

```text
addi                         0.3098 ns  #
mul                          0.8995 ns  ####
ld                           1.1502 ns  ####
sd                           1.3062 ns  #####
beq not-taken                2.3888 ns  #########
beq taken                    3.3591 ns  #############
amoadd.d                     2.4836 ns  ##########
sc.d                         3.5323 ns  ##############
div                          4.5249 ns  ##################
rem                          5.3846 ns  #####################
fadd.d                      11.0767 ns  ###########################################
fmul.d                       4.4962 ns  ##################
fdiv.d                       6.5626 ns  ##########################
fdiv.s                      12.2959 ns  ################################################
```

### 11.6 慢路径成本（对数尺度）

```text
jalr indirect-link          68.7578 ns  ###########################
jalr indirect-jump          69.5108 ns  ###########################
ret                         66.4826 ns  ###########################
time CSR                  1719.6672 ns  ################################################
```

`jalr` 的 66–70 ns 主要反映间接 TB lookup/dispatcher；`time` CSR 的约 1.72 µs 反映 QEMU 时钟 helper。它们不能解释为硬件指令 latency。

### 11.7 代表性点估计和同时区间

| 上下文 | ns/指令 | 95% 同时区间 | 质量 | 主要失败原因 |
| --- | ---: | ---: | --- | --- |
| addi | 0.309766 | [0.285230, 0.334302] | low-confidence | order-effect-not-equivalent, drift-effect-not-equivalent, batch-size-nonlinearity, cross-clock-check-divergent, plugin-off-check-divergent |
| mul | 0.899502 | [0.865636, 0.933367] | low-confidence | batch-size-nonlinearity, cross-clock-check-divergent, plugin-off-check-divergent |
| ld | 1.150169 | [1.086151, 1.214187] | low-confidence | too-many-severe-outliers, order-effect-not-equivalent, drift-effect-not-equivalent, batch-size-nonlinearity, cross-clock-check-divergent, plugin-off-check-divergent |
| sd | 1.306196 | [1.131722, 1.480670] | low-confidence | cross-run-heterogeneity-high, order-effect-not-equivalent, drift-effect-not-equivalent, batch-size-nonlinearity, plugin-off-check-divergent |
| beq not-taken | 2.388805 | [2.304764, 2.472846] | low-confidence | order-effect-not-equivalent, drift-effect-not-equivalent, batch-size-nonlinearity, plugin-off-check-divergent |
| beq taken | 3.359123 | [2.993135, 3.725111] | low-confidence | order-effect-not-equivalent, drift-effect-not-equivalent, batch-size-nonlinearity, plugin-off-check-divergent |
| amoadd.d | 2.483577 | [2.351328, 2.615826] | low-confidence | too-many-severe-outliers, batch-size-nonlinearity, plugin-off-check-divergent |
| sc.d | 3.532279 | [3.094562, 3.969996] | low-confidence | order-effect-not-equivalent, drift-effect-not-equivalent, batch-size-nonlinearity, plugin-off-check-divergent |
| div | 4.524948 | [4.468895, 4.581001] | low-confidence | plugin-off-check-divergent |
| rem | 5.384638 | [5.313330, 5.455946] | high-confidence |  |
| fadd.d | 11.076651 | [10.934685, 11.218616] | low-confidence | batch-size-nonlinearity |
| fmul.d | 4.496168 | [4.253856, 4.738479] | low-confidence | batch-size-nonlinearity, plugin-off-check-divergent |
| fdiv.d | 6.562581 | [6.402769, 6.722394] | low-confidence | batch-size-nonlinearity |
| fdiv.s | 12.295920 | [12.049658, 12.542181] | low-confidence | per-run-irls-not-converged, batch-size-nonlinearity, cross-clock-check-unavailable |
| jalr indirect-link | 68.757847 | [65.804228, 71.711465] | low-confidence | batch-size-nonlinearity |
| jalr indirect-jump | 69.510822 | [67.430494, 71.591150] | low-confidence | batch-size-nonlinearity |
| ret | 66.482640 | [63.421404, 69.543876] | low-confidence | drift-effect-not-equivalent, batch-size-nonlinearity |
| time CSR | 1719.667204 | [1710.847401, 1728.487008] | high-confidence |  |

### 11.8 分布分位表

| 指标 | min | P05 | P25 | P50 | P75 | P95 | max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ns/指令点估计 | 0.000000 | 0.000000 | 0.305250 | 1.840906 | 3.184151 | 68.745912 | 1719.667204 |
| Kish+ACF ESS | 124.964168 | 152.253889 | 181.038432 | 210.261262 | 238.181771 | 271.255165 | 297.341576 |
| 同时区间宽度/ns | 0.031287 | 0.045548 | 0.057486 | 0.170654 | 0.487204 | 5.053854 | 17.639607 |
| 相对同时半宽 | 0.005129 | 0.017509 | 0.035675 | 0.053571 | 0.094080 | 0.197428 | 0.251607 |
| 严重异常比例 | 0.002778 | 0.013889 | 0.030556 | 0.047222 | 0.066667 | 0.086111 | 0.100000 |
| 保留 pair/上下文 | 251.000000 | 346.000000 | 360.000000 | 360.000000 | 360.000000 | 360.000000 | 360.000000 |
| 999/99 区间宽度比 | 0.970217 | 1.012126 | 1.072306 | 1.138640 | 1.208283 | 1.292253 | 1.425529 |

## 12. Catalog 映射与发布策略

catalog loader 校验 schema、target、TB 计数、完整 bytes、decode 错误、drop/error、final quality 和 409 个规范 key。语义 key 保留 `form=`、`aq/rl`、CSR 编号、rounding mode 等 modifier，避免把 `nop/addi/li/mv` 或 `call/jump/return` 合并。

| 映射结果 | 数量 |
| --- | ---: |
| `csr-is-not-safe-or-identifiable-in-user-mode` | 207 |
| `measured-but-confidence-gates-failed` | 191 |
| `requires-privileged-context-probe` | 5 |
| `semantic-class-transfer-from-one-raw-context` | 3 |
| `trap-path-is-context-dependent` | 2 |
| `cache-block-operation-is-context-dependent` | 1 |

模型有 205 个语义 key，其中 199 与 catalog 相交，6 个为本次 BuildStorm catalog 未出现的 orphan；orphan 只报告，不参与 catalog 权重。

严格发布：

| 语义 key | ns/指令 | 95% 同时区间 | 相对权重 |
| --- | ---: | ---: | ---: |
| `rv64:32:zicsr:csrrs:csr=0xc01:write=0` | 1719.667204 | [1710.847401, 1728.487008] | 309.342545 |
| `rv64:32:m:divuw` | 3.338647 | [3.300759, 3.376535] | 0.600573 |
| `rv64:32:m:rem` | 5.384638 | [5.313330, 5.455946] | 0.968616 |
| `rv64:32:m:remw` | 5.733569 | [5.639920, 5.827218] | 1.031384 |

其中 `time` CSR 虽在模型层通过，但 mapper 先应用 restricted policy，因此 catalog 严格发布只有 `divuw/rem/remw`。

## 13. 正确性论证

### 13.1 构造有效性

- marker-only timing 避免每条指令 callback 成为主要成本；
- validation 以精确 raw encoding 和 ELF PC 范围证明测到的是目标代码；
- probe/baseline 差分消除公共固定成本；
- execution pattern 保留分支、访存、原子和 FP 语境，避免错误聚合。

数据证据：所有样本 purity 均为 1.0；validation/timing footer 没有 marker、timer 或 translation-accounting failure。

### 13.2 内部有效性

- 12 个独立 QEMU 进程而不是只重复同一进程；
- AB/BA 接近平衡，减少顺序偏差；
- 三档 batch 用于发现固定开销和非线性；
- 污染 translation pair 直接剔除，不假设差分一定相消；
- run 固定效应、漂移项和 plugin-off 对照显式建模潜在混杂。

数据证据：每档 batch 精确 25560 pair；保留率 99.128%；每个上下文至少 251 pair，ESS 最低约 124.96。

### 13.3 统计结论有效性

- Huber 回归限制少量长尾异常的影响；
- Kish ESS 与 block ACF 避免把相关样本当作独立样本；
- run-cluster bootstrap 保持最高层依赖结构；
- max-standardized-deviation 控制主权重整族同时覆盖；
- 999/999 replicate 有效，分位 Monte Carlo SE 可量化；
- 负估计、零成本、异常比例、异质性和辅助时钟均有硬门禁。

### 13.4 机制一致性

结果的相对层次符合 QEMU TCG 机制：简单整数最低；softmmu 热访存高于整数；taken branch 高于 fallthrough；原子包含 reservation/store 检查；`jalr` 触发间接 TB 查找；CSR/FP 进入 helper 或 softfloat。机制一致性不是独立的统计证明，但能发现数量级明显错误或测到了错误路径的情况。

### 13.5 为什么不能宣称 409 类全部高置信

最终只有 4/213 模型上下文通过全部门禁。失败分布表明：主 CI 过宽仅 16 项，但 plugin-off divergent 169 项、batch nonlinearity 157 项、order/drift 等价性失败 82/98 项。严格标签同时要求“点估计精确”和“对辅助设计变化稳定”，所以一个窄 CI 仍可能是 low-confidence。

例如 32 位 `addi` 为约 0.3098 ns，主同时区间 [0.2852, 0.3343] 并不宽，但它仍因 order、drift、batch、cross-clock、plugin-off 门禁失败而不被 mapper 严格发布。这是保守拒绝，不是把辅助噪声包装成确定性结论。

## 14. 局限性与后续实验

1. 只有一个宿主和一个 QEMU 版本。现有置信区间只对本宿主/QEMU/活动条件成立；跨宿主外推需要新增宿主 cluster。
2. plugin-on 固定先于 plugin-off，顺序与宿主漂移可能混杂。后续应在 QEMU 进程级随机化 on/off 顺序，并考虑全局仿射 non-invasiveness 校准。
3. 低成本指令的 plugin-off ratio 存在近零分母病态；当前严格门禁因此明显保守。
4. 157 个上下文出现 batch 非线性，其中部分是真实摊销/TCG 行为。后续可发布 reference-batch 权重或拟合 `alpha + beta*N`，不应简单删除门禁。
5. hot-load/store 只覆盖预热地址；cache/TLB miss、不同对齐、MMIO 和 page fault 必须单独建模。
6. 单 vCPU 原子 probe 不能代表多核争用；`aq/rl` 在此场景接近无额外成本是实验条件的结果。
7. FP 使用固定 normal 操作数，NaN、subnormal、异常和不同 rounding mode 可能进入不同 softfloat 路径。
8. CSR/priv/trap/CBO 必须在专用特权与状态上下文中测量，不能从用户态安全 probe 转移。
9. catalog occurrence 来自 TB 翻译记录，不是运行时动态执行频次；不能直接用 occurrence×weight 宣称运行时间占比。

## 15. 验证结果

- `python3 -m unittest discover -s scripts/tests`：167/167 通过；
- QEMU plugin validation/timing 双模式 smoke：通过；
- Python `py_compile`、shell `sh -n`、`git diff --check`：通过；
- 稀疏 WLS 与稠密参考在 24 个正式 key 上系数、残差、Huber/异方差权重和 estimate bit-for-bit 相同；
- mapper 的真实模型 schema v2 契约、409-key catalog 闭合、restricted 优先级和探索字段均有回归测试。

## 16. 原始与派生数据

- [完整采样 JSONL](../build/riscv-instruction-weight-runs/formal-v2-20260809/samples.jsonl)；
- [模型 JSON](../build/riscv-instruction-weight-runs/formal-v2-20260809/weights.json)；
- [模型 CSV](../build/riscv-instruction-weight-runs/formal-v2-20260809/weights.csv)；
- [完整 catalog 映射 JSON](../build/riscv-instruction-weight-runs/formal-v2-20260809/catalog-weights.json)；
- [完整 catalog 映射 CSV](../build/riscv-instruction-weight-runs/formal-v2-20260809/catalog-weights.csv)；
- [99 次预检模型](../build/riscv-instruction-weight-runs/formal-v2-20260809/weights-preflight-99.json)；
- [QEMU validation 窗口](../build/riscv-instruction-weight-runs/formal-v2-20260809/validation.windows.jsonl)。

## 17. 理论参考

1. Huber, P. J. (1964). Robust Estimation of a Location Parameter. *The Annals of Mathematical Statistics*.
2. Efron, B. and Tibshirani, R. J. (1993). *An Introduction to the Bootstrap*.
3. Künsch, H. R. (1989). The Jackknife and the Bootstrap for General Stationary Observations. *The Annals of Statistics*.
4. Kish, L. (1965). *Survey Sampling*.
5. DerSimonian, R. and Laird, N. (1986). Meta-analysis in Clinical Trials. *Controlled Clinical Trials*.
6. Wilson, E. B. (1927). Probable Inference, the Law of Succession, and Statistical Inference. *JASA*.
7. Westfall, P. H. and Young, S. S. (1993). *Resampling-Based Multiple Testing*.
8. RISC-V International. *The RISC-V Instruction Set Manual*.
9. QEMU Project. *QEMU TCG Plugin API*.

---

## 附录 A：213 个探针上下文完整表

以下表是 `weights.json` 的完整可读投影。`ns` 是物理非负发布值；显著负的无约束估计不会被截成零。

| # | 语义 key | raw bytes | pattern | ns | 95% 同时区间 | quality | runs | pairs | ESS | purity P05 | 翻译剔除 | failures |
| ---: | --- | --- | --- | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| 1 | `rv64:16:c:c.add` | `2e95` | `dependency` | 0.307516 | [0.281098, 0.333934] | low-confidence | 12 | 360 | 207.150 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 2 | `rv64:16:c:c.addi` | `0505` | `dependency` | 0.304301 | [0.276966, 0.331637] | low-confidence | 12 | 360 | 171.526 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 3 | `rv64:16:c:c.addi16sp` | `4161` | `stack-adjust` | 0.299824 | [0.276337, 0.323310] | low-confidence | 12 | 359 | 195.087 | 1.000 | 1 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-unavailable |
| 4 | `rv64:16:c:c.addi4spn` | `1008` | `stack-address` | 0.000000 | [0.003147, 0.048888] | low-confidence | 12 | 359 | 154.310 | 1.000 | 1 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 5 | `rv64:16:c:c.addiw` | `0525` | `dependency` | 0.616431 | [0.590053, 0.642809] | low-confidence | 12 | 360 | 211.231 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 6 | `rv64:16:c:c.addiw:form=sext.w` | `0125` | `dependency` | 0.000000 | [-0.006671, 0.038939] | low-confidence | 12 | 360 | 190.142 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 7 | `rv64:16:c:c.addw` | `2d9d` | `dependency` | 0.613338 | [0.587519, 0.639157] | low-confidence | 12 | 360 | 192.306 | 1.000 | 0 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-unavailable |
| 8 | `rv64:16:c:c.and` | `6d8d` | `dependency` | 0.309817 | [0.280295, 0.339339] | low-confidence | 12 | 360 | 178.263 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 9 | `rv64:16:c:c.andi` | `1d89` | `dependency` | 0.000000 | [-0.013699, 0.033725] | low-confidence | 12 | 360 | 176.774 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 10 | `rv64:16:c:c.beqz` | `11c1` | `not-taken-branch` | 2.397419 | [2.298512, 2.496327] | low-confidence | 12 | 360 | 230.591 | 1.000 | 0 | too-many-severe-outliers<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 11 | `rv64:16:c:c.beqz` | `11c1` | `taken-branch` | 3.297129 | [2.987390, 3.606868] | low-confidence | 12 | 360 | 244.687 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 12 | `rv64:16:c:c.bnez` | `11e1` | `not-taken-branch` | 2.366445 | [2.299484, 2.433405] | low-confidence | 12 | 360 | 233.801 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 13 | `rv64:16:c:c.bnez` | `11e1` | `taken-branch` | 3.245326 | [2.929164, 3.561489] | low-confidence | 12 | 360 | 228.643 | 1.000 | 0 | per-run-irls-not-converged<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 14 | `rv64:16:c:c.fld` | `0821` | `hot-load` | 1.855563 | [1.627618, 2.083508] | low-confidence | 12 | 346 | 189.388 | 1.000 | 14 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 15 | `rv64:16:c:c.fldsp` | `0225` | `hot-stack-load` | 1.322337 | [1.235937, 1.408737] | low-confidence | 12 | 348 | 213.806 | 1.000 | 12 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-unavailable |
| 16 | `rv64:16:c:c.fsd` | `08a1` | `hot-store` | 1.682728 | [1.410208, 1.955247] | low-confidence | 12 | 348 | 200.134 | 1.000 | 12 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 17 | `rv64:16:c:c.fsdsp` | `2aa0` | `hot-stack-store` | 1.778118 | [1.487945, 2.068291] | low-confidence | 12 | 349 | 191.916 | 1.000 | 11 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 18 | `rv64:16:c:c.j` | `11a0` | `direct-jump` | 2.368731 | [2.258367, 2.479095] | low-confidence | 12 | 360 | 244.021 | 1.000 | 0 | too-many-severe-outliers<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 19 | `rv64:16:c:c.jalr` | `8292` | `indirect-link` | 68.734911 | [65.929655, 71.540166] | low-confidence | 12 | 360 | 214.281 | 1.000 | 0 | batch-size-nonlinearity |
| 20 | `rv64:16:c:c.jr:form=jr` | `8282` | `indirect-jump` | 68.371240 | [65.746769, 70.995710] | low-confidence | 12 | 360 | 210.024 | 1.000 | 0 | batch-size-nonlinearity |
| 21 | `rv64:16:c:c.jr:form=ret` | `8280` | `indirect-return` | 69.182657 | [67.129020, 71.236294] | low-confidence | 12 | 360 | 269.697 | 1.000 | 0 | batch-size-nonlinearity |
| 22 | `rv64:16:c:c.ld` | `1061` | `hot-load` | 1.167796 | [1.078270, 1.257322] | low-confidence | 12 | 360 | 261.471 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 23 | `rv64:16:c:c.ldsp` | `0266` | `hot-stack-load` | 1.139748 | [1.079576, 1.199920] | low-confidence | 12 | 360 | 254.062 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 24 | `rv64:16:c:c.li` | `0546` | `independent` | 0.000000 | [-0.006439, 0.053271] | low-confidence | 12 | 360 | 142.455 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 25 | `rv64:16:c:c.lui` | `0566` | `independent` | 0.000000 | [-0.016177, 0.022906] | low-confidence | 12 | 360 | 166.565 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 26 | `rv64:16:c:c.lw` | `1041` | `hot-load` | 1.134475 | [1.066496, 1.202455] | low-confidence | 12 | 360 | 264.019 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 27 | `rv64:16:c:c.lwsp` | `0246` | `hot-stack-load` | 1.170763 | [1.099603, 1.241923] | low-confidence | 12 | 360 | 246.781 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 28 | `rv64:16:c:c.mv` | `2e85` | `dependency` | 0.000000 | [-0.010361, 0.028545] | low-confidence | 12 | 359 | 194.074 | 1.000 | 1 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 29 | `rv64:16:c:c.nop` | `0100` | `independent` | 0.000000 | [0.011778, 0.043064] | low-confidence | 12 | 360 | 181.273 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 30 | `rv64:16:c:c.or` | `4d8d` | `dependency` | 0.303830 | [0.274026, 0.333634] | low-confidence | 12 | 360 | 177.728 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 31 | `rv64:16:c:c.sd` | `0ce1` | `hot-store` | 1.187832 | [0.982496, 1.393168] | low-confidence | 12 | 360 | 240.474 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 32 | `rv64:16:c:c.sdsp` | `2ee0` | `hot-stack-store` | 1.189236 | [0.946449, 1.432024] | low-confidence | 12 | 359 | 233.190 | 1.000 | 1 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 33 | `rv64:16:c:c.slli` | `0605` | `dependency` | 0.000000 | [-0.002629, 0.057086] | low-confidence | 12 | 360 | 144.923 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 34 | `rv64:16:c:c.srai` | `0585` | `dependency` | 0.313509 | [0.287139, 0.339880] | low-confidence | 12 | 360 | 196.575 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 35 | `rv64:16:c:c.srli` | `0581` | `dependency` | 0.000000 | [-0.021003, 0.025195] | low-confidence | 12 | 360 | 148.409 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 36 | `rv64:16:c:c.sub` | `0d8d` | `dependency` | 0.289465 | [0.258546, 0.320384] | low-confidence | 12 | 360 | 158.463 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 37 | `rv64:16:c:c.subw` | `0d9d` | `dependency` | 0.604613 | [0.581292, 0.627934] | low-confidence | 12 | 360 | 227.076 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-unavailable |
| 38 | `rv64:16:c:c.sw` | `0cc1` | `hot-store` | 1.238632 | [0.995030, 1.482234] | low-confidence | 12 | 359 | 217.642 | 1.000 | 1 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 39 | `rv64:16:c:c.swsp` | `2ec0` | `hot-stack-store` | 1.246205 | [1.001543, 1.490868] | low-confidence | 12 | 360 | 243.413 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 40 | `rv64:16:c:c.xor` | `2d8d` | `dependency` | 0.305448 | [0.279984, 0.330911] | low-confidence | 12 | 360 | 199.233 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 41 | `rv64:32:a:amoadd.d:aq=0:rl=0` | `2f30b500` | `hot-atomic` | 2.483577 | [2.351328, 2.615826] | low-confidence | 12 | 360 | 259.128 | 1.000 | 0 | too-many-severe-outliers<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 42 | `rv64:32:a:amoadd.d:aq=0:rl=1` | `2f30b502` | `hot-atomic` | 2.497640 | [2.371538, 2.623743] | low-confidence | 12 | 360 | 222.168 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 43 | `rv64:32:a:amoadd.d:aq=1:rl=0` | `2f30b504` | `hot-atomic` | 2.503248 | [2.357813, 2.648684] | low-confidence | 12 | 360 | 219.029 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 44 | `rv64:32:a:amoadd.d:aq=1:rl=1` | `2f30b506` | `hot-atomic` | 2.477804 | [2.366008, 2.589600] | low-confidence | 12 | 360 | 292.731 | 1.000 | 0 | order-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 45 | `rv64:32:a:amoadd.w:aq=0:rl=0` | `2f20b500` | `hot-atomic` | 2.749203 | [2.676440, 2.821967] | low-confidence | 12 | 360 | 238.853 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 46 | `rv64:32:a:amoadd.w:aq=0:rl=1` | `2f20b502` | `hot-atomic` | 2.746105 | [2.661639, 2.830570] | low-confidence | 12 | 360 | 230.099 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 47 | `rv64:32:a:amoadd.w:aq=1:rl=0` | `2f20b504` | `hot-atomic` | 2.754931 | [2.633496, 2.876366] | low-confidence | 12 | 360 | 192.726 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 48 | `rv64:32:a:amoadd.w:aq=1:rl=1` | `2f20b506` | `hot-atomic` | 2.752791 | [2.666310, 2.839273] | low-confidence | 12 | 360 | 185.152 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 49 | `rv64:32:a:amoand.d:aq=0:rl=0` | `2f30b560` | `hot-atomic` | 2.491516 | [2.279846, 2.703186] | low-confidence | 12 | 360 | 194.010 | 1.000 | 0 | too-many-severe-outliers<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 50 | `rv64:32:a:amoand.d:aq=0:rl=1` | `2f30b562` | `hot-atomic` | 2.474192 | [2.364525, 2.583859] | low-confidence | 12 | 360 | 218.585 | 1.000 | 0 | plugin-off-check-divergent |
| 51 | `rv64:32:a:amoand.d:aq=1:rl=1` | `2f30b566` | `hot-atomic` | 2.493657 | [2.321200, 2.666114] | low-confidence | 12 | 360 | 220.492 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 52 | `rv64:32:a:amoand.w:aq=0:rl=0` | `2f20b560` | `hot-atomic` | 2.765374 | [2.658200, 2.872549] | low-confidence | 12 | 360 | 170.201 | 1.000 | 0 | per-run-irls-not-converged<br>too-many-severe-outliers<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 53 | `rv64:32:a:amoand.w:aq=1:rl=1` | `2f20b566` | `hot-atomic` | 2.776823 | [2.672835, 2.880811] | low-confidence | 12 | 360 | 209.409 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 54 | `rv64:32:a:amomaxu.d:aq=1:rl=0` | `2f30b5e4` | `hot-atomic` | 2.808310 | [2.706733, 2.909888] | low-confidence | 12 | 360 | 217.202 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 55 | `rv64:32:a:amomaxu.d:aq=1:rl=1` | `2f30b5e6` | `hot-atomic` | 2.787922 | [2.686286, 2.889558] | low-confidence | 12 | 360 | 211.583 | 1.000 | 0 | too-many-severe-outliers<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 56 | `rv64:32:a:amomaxu.w:aq=1:rl=0` | `2f20b5e4` | `hot-atomic` | 3.002997 | [2.863311, 3.142683] | low-confidence | 12 | 360 | 212.911 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 57 | `rv64:32:a:amoor.d:aq=0:rl=0` | `2f30b540` | `hot-atomic` | 2.462590 | [2.320693, 2.604486] | low-confidence | 12 | 360 | 249.564 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 58 | `rv64:32:a:amoor.d:aq=0:rl=1` | `2f30b542` | `hot-atomic` | 2.470735 | [2.312675, 2.628794] | low-confidence | 12 | 359 | 237.561 | 1.000 | 1 | too-many-severe-outliers<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 59 | `rv64:32:a:amoor.d:aq=1:rl=0` | `2f30b544` | `hot-atomic` | 2.464319 | [2.318138, 2.610500] | low-confidence | 12 | 360 | 208.014 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 60 | `rv64:32:a:amoor.d:aq=1:rl=1` | `2f30b546` | `hot-atomic` | 2.486289 | [2.332808, 2.639770] | low-confidence | 12 | 360 | 193.895 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 61 | `rv64:32:a:amoor.w:aq=0:rl=0` | `2f20b540` | `hot-atomic` | 2.769137 | [2.690197, 2.848078] | low-confidence | 12 | 360 | 260.432 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 62 | `rv64:32:a:amoor.w:aq=0:rl=1` | `2f20b542` | `hot-atomic` | 2.781640 | [2.689011, 2.874268] | low-confidence | 12 | 360 | 242.384 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 63 | `rv64:32:a:amoor.w:aq=1:rl=0` | `2f20b544` | `hot-atomic` | 2.791954 | [2.629725, 2.954183] | low-confidence | 12 | 360 | 205.123 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 64 | `rv64:32:a:amoor.w:aq=1:rl=1` | `2f20b546` | `hot-atomic` | 2.754687 | [2.670261, 2.839112] | low-confidence | 12 | 360 | 255.841 | 1.000 | 0 | plugin-off-check-unavailable |
| 65 | `rv64:32:a:amoswap.d:aq=0:rl=0` | `2f30b508` | `hot-atomic` | 2.388765 | [1.886652, 2.890878] | low-confidence | 12 | 360 | 247.320 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 66 | `rv64:32:a:amoswap.d:aq=1:rl=0` | `2f30b50c` | `hot-atomic` | 2.440741 | [1.871943, 3.009540] | low-confidence | 12 | 360 | 225.251 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 67 | `rv64:32:a:amoswap.d:aq=1:rl=1` | `2f30b50e` | `hot-atomic` | 2.360036 | [1.825928, 2.894144] | low-confidence | 12 | 360 | 269.719 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 68 | `rv64:32:a:amoswap.w:aq=0:rl=0` | `2f20b508` | `hot-atomic` | 2.428314 | [1.827745, 3.028882] | low-confidence | 12 | 360 | 213.040 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 69 | `rv64:32:a:amoswap.w:aq=0:rl=1` | `2f20b50a` | `hot-atomic` | 2.476489 | [1.907784, 3.045194] | low-confidence | 12 | 360 | 203.609 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 70 | `rv64:32:a:amoswap.w:aq=1:rl=0` | `2f20b50c` | `hot-atomic` | 2.437709 | [1.849710, 3.025708] | low-confidence | 12 | 360 | 184.182 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 71 | `rv64:32:a:amoswap.w:aq=1:rl=1` | `2f20b50e` | `hot-atomic` | 2.493681 | [1.866253, 3.121109] | low-confidence | 12 | 360 | 192.713 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 72 | `rv64:32:a:amoxor.d:aq=0:rl=0` | `2f30b520` | `hot-atomic` | 2.499860 | [2.357094, 2.642626] | low-confidence | 12 | 360 | 194.697 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 73 | `rv64:32:a:amoxor.w:aq=0:rl=0` | `2f20b520` | `hot-atomic` | 2.752508 | [2.644849, 2.860166] | low-confidence | 12 | 360 | 164.318 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 74 | `rv64:32:a:lr.d:aq=0:rl=0` | `af320510` | `hot-atomic` | 1.281369 | [1.193307, 1.369431] | low-confidence | 12 | 360 | 225.369 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 75 | `rv64:32:a:lr.d:aq=1:rl=0` | `af320514` | `hot-atomic` | 2.280094 | [2.188480, 2.371707] | low-confidence | 12 | 360 | 233.101 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 76 | `rv64:32:a:lr.d:aq=1:rl=1` | `af320516` | `hot-atomic` | 2.256319 | [2.160238, 2.352400] | low-confidence | 12 | 360 | 221.478 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 77 | `rv64:32:a:lr.w:aq=0:rl=0` | `af220510` | `hot-atomic` | 1.296992 | [1.208030, 1.385955] | low-confidence | 12 | 360 | 240.752 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 78 | `rv64:32:a:lr.w:aq=1:rl=0` | `af220514` | `hot-atomic` | 2.258599 | [2.185388, 2.331810] | low-confidence | 12 | 360 | 262.020 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 79 | `rv64:32:a:lr.w:aq=1:rl=1` | `af220516` | `hot-atomic` | 2.323048 | [2.233553, 2.412542] | low-confidence | 12 | 360 | 267.517 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 80 | `rv64:32:a:sc.d:aq=0:rl=0` | `2f33b518` | `reservation-pair` | 3.532279 | [3.094562, 3.969996] | low-confidence | 12 | 360 | 164.474 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 81 | `rv64:32:a:sc.d:aq=0:rl=1` | `2f33b51a` | `reservation-pair` | 3.569757 | [3.108957, 4.030557] | low-confidence | 12 | 360 | 186.759 | 1.000 | 0 | cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 82 | `rv64:32:a:sc.d:aq=1:rl=0` | `2f33b51c` | `reservation-pair` | 3.554475 | [3.128437, 3.980514] | low-confidence | 12 | 360 | 187.298 | 1.000 | 0 | too-many-severe-outliers<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 83 | `rv64:32:a:sc.w:aq=0:rl=0` | `2f23b518` | `reservation-pair` | 3.573791 | [3.170347, 3.977235] | low-confidence | 12 | 360 | 235.876 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 84 | `rv64:32:a:sc.w:aq=0:rl=1` | `2f23b51a` | `reservation-pair` | 3.548578 | [3.140766, 3.956390] | low-confidence | 12 | 360 | 241.645 | 1.000 | 0 | too-many-severe-outliers<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 85 | `rv64:32:a:sc.w:aq=1:rl=0` | `2f23b51c` | `reservation-pair` | 3.591566 | [3.214904, 3.968227] | low-confidence | 12 | 360 | 232.491 | 1.000 | 0 | too-many-severe-outliers<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 86 | `rv64:32:a:sc.w:aq=1:rl=1` | `2f23b51e` | `reservation-pair` | 3.552325 | [3.150061, 3.954590] | low-confidence | 12 | 360 | 218.001 | 1.000 | 0 | too-many-severe-outliers<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 87 | `rv64:32:d:fadd.d:rm=dyn` | `53701002` | `fp-dependency` | 11.076651 | [10.934685, 11.218616] | low-confidence | 12 | 344 | 153.114 | 1.000 | 16 | batch-size-nonlinearity |
| 88 | `rv64:32:d:fclass.d` | `531500e2` | `fp-classify` | 0.000000 | [0.044232, 0.097643] | low-confidence | 12 | 348 | 177.914 | 1.000 | 12 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-unavailable |
| 89 | `rv64:32:d:fcvt.d.l:rm=dyn` | `537025d2` | `fp-convert` | 12.466082 | [12.198756, 12.733408] | low-confidence | 12 | 346 | 223.673 | 1.000 | 14 | batch-size-nonlinearity |
| 90 | `rv64:32:d:fcvt.d.lu:rm=dyn` | `537035d2` | `fp-convert` | 11.548073 | [11.257543, 11.838603] | low-confidence | 12 | 339 | 230.884 | 1.000 | 21 | batch-size-nonlinearity |
| 91 | `rv64:32:d:fcvt.d.w:rm=rne` | `530005d2` | `fp-convert` | 12.520855 | [12.213716, 12.827994] | low-confidence | 12 | 342 | 252.081 | 1.000 | 18 | batch-size-nonlinearity |
| 92 | `rv64:32:d:fcvt.l.d:rm=rtz` | `531520c2` | `fp-convert` | 11.655546 | [11.279743, 12.031349] | low-confidence | 12 | 348 | 263.856 | 1.000 | 12 | batch-size-nonlinearity |
| 93 | `rv64:32:d:fcvt.lu.d:rm=rtz` | `531530c2` | `fp-convert` | 11.510599 | [11.197208, 11.823989] | low-confidence | 12 | 348 | 273.875 | 1.000 | 12 | batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-unavailable |
| 94 | `rv64:32:d:fcvt.s.d:rm=dyn` | `53f01040` | `fp-convert` | 11.500906 | [11.288683, 11.713128] | low-confidence | 12 | 347 | 238.182 | 1.000 | 13 | batch-size-nonlinearity |
| 95 | `rv64:32:d:fcvt.w.d:rm=rtz` | `531500c2` | `fp-convert` | 12.352561 | [12.034856, 12.670267] | low-confidence | 12 | 348 | 251.076 | 1.000 | 12 | batch-size-nonlinearity |
| 96 | `rv64:32:d:fdiv.d:rm=dyn` | `5370101a` | `fp-dependency` | 6.562581 | [6.402769, 6.722394] | low-confidence | 12 | 344 | 214.771 | 1.000 | 16 | batch-size-nonlinearity |
| 97 | `rv64:32:d:feq.d` | `532510a2` | `fp-compare` | 5.818416 | [5.609883, 6.026950] | low-confidence | 12 | 348 | 274.643 | 1.000 | 12 | batch-size-nonlinearity<br>plugin-off-check-unavailable |
| 98 | `rv64:32:d:fld` | `07300500` | `hot-load` | 1.519900 | [1.375620, 1.664181] | low-confidence | 12 | 348 | 199.252 | 1.000 | 12 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 99 | `rv64:32:d:fle.d` | `530510a2` | `fp-compare` | 5.933362 | [5.650193, 6.216530] | low-confidence | 12 | 348 | 267.999 | 1.000 | 12 | batch-size-nonlinearity |
| 100 | `rv64:32:d:flt.d` | `531510a2` | `fp-compare` | 6.225205 | [5.923943, 6.526466] | low-confidence | 12 | 348 | 183.173 | 1.000 | 12 | batch-size-nonlinearity |
| 101 | `rv64:32:d:fmul.d:rm=dyn` | `53701012` | `fp-dependency` | 4.496168 | [4.253856, 4.738479] | low-confidence | 12 | 346 | 183.490 | 1.000 | 14 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 102 | `rv64:32:d:fmv.d.x` | `530005f2` | `fp-move` | 0.000000 | [0.007612, 0.058831] | low-confidence | 12 | 349 | 147.097 | 1.000 | 11 | per-run-irls-not-converged<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 103 | `rv64:32:d:fmv.x.d` | `530500e2` | `fp-move` | 0.000000 | [0.011220, 0.083951] | low-confidence | 12 | 348 | 161.152 | 1.000 | 12 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 104 | `rv64:32:d:fsd` | `27300500` | `hot-store` | 1.840906 | [1.511166, 2.170645] | low-confidence | 12 | 348 | 178.547 | 1.000 | 12 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-unavailable |
| 105 | `rv64:32:d:fsgnj.d` | `53001022` | `fp-dependency` | 0.933594 | [0.886049, 0.981139] | low-confidence | 12 | 348 | 210.261 | 1.000 | 12 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 106 | `rv64:32:d:fsub.d:rm=dyn` | `5370100a` | `fp-dependency` | 11.052511 | [10.779684, 11.325339] | low-confidence | 12 | 341 | 225.972 | 1.000 | 19 | batch-size-nonlinearity |
| 107 | `rv64:32:f:fcvt.s.lu:rm=dyn` | `537035d0` | `fp-convert` | 12.029701 | [11.526254, 12.533148] | low-confidence | 12 | 344 | 237.766 | 1.000 | 16 | batch-size-nonlinearity |
| 108 | `rv64:32:f:fdiv.s:rm=dyn` | `53701018` | `fp-dependency` | 12.295920 | [12.049658, 12.542181] | low-confidence | 12 | 343 | 238.502 | 1.000 | 17 | per-run-irls-not-converged<br>batch-size-nonlinearity<br>cross-clock-check-unavailable |
| 109 | `rv64:32:f:flw` | `07200500` | `hot-load` | 1.601599 | [1.427681, 1.775517] | low-confidence | 12 | 346 | 183.997 | 1.000 | 14 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 110 | `rv64:32:f:fmv.w.x` | `530005f0` | `fp-move` | 0.000000 | [0.010893, 0.060133] | low-confidence | 12 | 348 | 155.999 | 1.000 | 12 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 111 | `rv64:32:f:fmv.x.w` | `530500e0` | `fp-move` | 0.000000 | [0.026716, 0.073125] | low-confidence | 12 | 348 | 150.964 | 1.000 | 12 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 112 | `rv64:32:f:fsw` | `27200500` | `hot-store` | 1.821230 | [1.459981, 2.182479] | low-confidence | 12 | 348 | 198.380 | 1.000 | 12 | per-run-irls-not-converged<br>simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-unavailable |
| 113 | `rv64:32:i:add` | `3305b500` | `dependency` | 0.302668 | [0.274849, 0.330487] | low-confidence | 12 | 360 | 176.805 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 114 | `rv64:32:i:addi` | `13051500` | `dependency` | 0.309766 | [0.285230, 0.334302] | low-confidence | 12 | 360 | 209.120 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 115 | `rv64:32:i:addi:form=li` | `13051000` | `immediate-load` | 0.000000 | [0.007998, 0.062259] | low-confidence | 12 | 360 | 212.285 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 116 | `rv64:32:i:addi:form=mv` | `13850500` | `register-move` | 0.000000 | [-0.005590, 0.048107] | low-confidence | 12 | 360 | 174.041 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 117 | `rv64:32:i:addi:form=nop` | `13000000` | `independent` | 0.000000 | [0.007056, 0.039746] | low-confidence | 12 | 360 | 159.685 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 118 | `rv64:32:i:addiw` | `1b051500` | `dependency` | 0.605556 | [0.572327, 0.638785] | low-confidence | 12 | 360 | 203.539 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 119 | `rv64:32:i:addiw:form=sext.w` | `1b050500` | `dependency` | 0.000000 | [-0.010131, 0.050661] | low-confidence | 12 | 360 | 177.247 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 120 | `rv64:32:i:addw` | `3b05b500` | `dependency` | 0.610007 | [0.579039, 0.640976] | low-confidence | 12 | 360 | 178.682 | 1.000 | 0 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 121 | `rv64:32:i:and` | `3375b500` | `dependency` | 0.304178 | [0.275435, 0.332921] | low-confidence | 12 | 360 | 161.107 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 122 | `rv64:32:i:andi` | `1375f50f` | `dependency` | 0.000000 | [0.004369, 0.049785] | low-confidence | 12 | 360 | 169.843 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 123 | `rv64:32:i:auipc` | `97020000` | `independent` | 0.000000 | [-0.006192, 0.046679] | low-confidence | 12 | 360 | 195.859 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 124 | `rv64:32:i:beq` | `6304b500` | `not-taken-branch` | 2.388805 | [2.304764, 2.472846] | low-confidence | 12 | 360 | 239.995 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 125 | `rv64:32:i:beq` | `6304b500` | `taken-branch` | 3.359123 | [2.993135, 3.725111] | low-confidence | 12 | 360 | 230.642 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 126 | `rv64:32:i:bge` | `6354b500` | `not-taken-branch` | 2.410239 | [2.268306, 2.552171] | low-confidence | 12 | 360 | 192.919 | 1.000 | 0 | too-many-severe-outliers<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-unavailable |
| 127 | `rv64:32:i:bge` | `6354b500` | `taken-branch` | 3.359649 | [3.082957, 3.636341] | low-confidence | 12 | 360 | 208.672 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 128 | `rv64:32:i:bgeu` | `6374b500` | `not-taken-branch` | 2.382763 | [2.269131, 2.496394] | low-confidence | 12 | 360 | 185.106 | 1.000 | 0 | too-many-severe-outliers<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 129 | `rv64:32:i:bgeu` | `6374b500` | `taken-branch` | 3.211584 | [2.844163, 3.579005] | low-confidence | 12 | 360 | 206.278 | 1.000 | 0 | batch-size-nonlinearity<br>plugin-off-check-divergent |
| 130 | `rv64:32:i:blt` | `6344b500` | `not-taken-branch` | 2.398404 | [2.328985, 2.467823] | low-confidence | 12 | 359 | 249.805 | 1.000 | 1 | too-many-severe-outliers<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 131 | `rv64:32:i:blt` | `6344b500` | `taken-branch` | 3.375323 | [3.051463, 3.699183] | low-confidence | 12 | 360 | 228.849 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 132 | `rv64:32:i:bltu` | `6364b500` | `not-taken-branch` | 2.368822 | [2.280428, 2.457216] | low-confidence | 12 | 360 | 242.793 | 1.000 | 0 | too-many-severe-outliers<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 133 | `rv64:32:i:bltu` | `6364b500` | `taken-branch` | 3.408736 | [3.057760, 3.759712] | low-confidence | 12 | 360 | 225.842 | 1.000 | 0 | order-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 134 | `rv64:32:i:bne` | `6314b500` | `not-taken-branch` | 2.490181 | [2.319730, 2.660633] | low-confidence | 12 | 360 | 150.452 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 135 | `rv64:32:i:bne` | `6314b500` | `taken-branch` | 3.428875 | [3.053583, 3.804166] | low-confidence | 12 | 360 | 189.497 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 136 | `rv64:32:i:fence:fm=0x0:pred=0x1:succ=0x1` | `0f001001` | `serialization-11` | 0.000000 | [0.002768, 0.045030] | low-confidence | 12 | 360 | 181.038 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-unavailable |
| 137 | `rv64:32:i:fence:fm=0x0:pred=0x1:succ=0x4` | `0f004001` | `serialization-14` | 0.000000 | [-0.000472, 0.047752] | low-confidence | 12 | 360 | 180.018 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 138 | `rv64:32:i:fence:fm=0x0:pred=0x2:succ=0x2` | `0f002002` | `serialization-22` | 0.000000 | [-0.005386, 0.040862] | low-confidence | 12 | 360 | 178.747 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 139 | `rv64:32:i:fence:fm=0x0:pred=0x2:succ=0x3` | `0f003002` | `serialization-23` | 0.000000 | [-0.002919, 0.044925] | low-confidence | 12 | 360 | 139.679 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 140 | `rv64:32:i:fence:fm=0x0:pred=0x3:succ=0x1` | `0f001003` | `serialization-31` | 0.000000 | [-0.001087, 0.047268] | low-confidence | 12 | 360 | 191.502 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 141 | `rv64:32:i:fence:fm=0x0:pred=0x3:succ=0x3` | `0f003003` | `serialization` | 0.000000 | [0.001279, 0.057471] | low-confidence | 12 | 360 | 197.918 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 142 | `rv64:32:i:fence:fm=0x0:pred=0x5:succ=0x5` | `0f005005` | `serialization-55` | 0.000000 | [-0.006947, 0.047827] | low-confidence | 12 | 360 | 168.870 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 143 | `rv64:32:i:fence:fm=0x0:pred=0x8:succ=0x2` | `0f002008` | `serialization-82` | 0.000000 | [-0.007159, 0.044708] | low-confidence | 12 | 360 | 145.525 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 144 | `rv64:32:i:fence:fm=0x0:pred=0xa:succ=0xa` | `0f00a00a` | `serialization-aa` | 0.000000 | [0.003820, 0.045938] | low-confidence | 12 | 360 | 171.374 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 145 | `rv64:32:i:fence:fm=0x0:pred=0xf:succ=0x5` | `0f00500f` | `serialization-f5` | 0.000000 | [0.002994, 0.052359] | low-confidence | 12 | 360 | 177.948 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 146 | `rv64:32:i:fence:fm=0x0:pred=0xf:succ=0xf` | `0f00f00f` | `serialization-ff` | 0.000000 | [-0.005557, 0.033304] | low-confidence | 12 | 360 | 160.092 | 1.000 | 0 | per-run-irls-not-converged<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 147 | `rv64:32:i:jal:form=call` | `ef008000` | `direct-link` | 2.557538 | [2.472212, 2.642865] | low-confidence | 12 | 360 | 237.948 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 148 | `rv64:32:i:jal:form=j` | `6f008000` | `direct-jump` | 2.388337 | [2.290442, 2.486232] | low-confidence | 12 | 360 | 227.115 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 149 | `rv64:32:i:jalr:form=call` | `e780c200` | `indirect-link` | 68.757847 | [65.804228, 71.711465] | low-confidence | 12 | 360 | 241.455 | 1.000 | 0 | batch-size-nonlinearity |
| 150 | `rv64:32:i:jalr:form=jr` | `6780c200` | `indirect-jump` | 69.510822 | [67.430494, 71.591150] | low-confidence | 12 | 360 | 183.390 | 1.000 | 0 | batch-size-nonlinearity |
| 151 | `rv64:32:i:jalr:form=link` | `e783c200` | `indirect-general-link` | 68.737955 | [66.573192, 70.902718] | low-confidence | 12 | 360 | 236.703 | 1.000 | 0 | batch-size-nonlinearity |
| 152 | `rv64:32:i:jalr:form=ret` | `67800000` | `indirect-return` | 66.482640 | [63.421404, 69.543876] | low-confidence | 12 | 360 | 238.928 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity |
| 153 | `rv64:32:i:lb` | `83020500` | `hot-load` | 1.162230 | [1.095590, 1.228871] | low-confidence | 12 | 360 | 270.568 | 1.000 | 0 | per-run-irls-not-converged<br>too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 154 | `rv64:32:i:lbu` | `83420500` | `hot-load` | 1.145496 | [1.066809, 1.224183] | low-confidence | 12 | 360 | 237.665 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 155 | `rv64:32:i:ld` | `83320500` | `hot-load` | 1.150169 | [1.086151, 1.214187] | low-confidence | 12 | 360 | 258.929 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 156 | `rv64:32:i:lh` | `83120500` | `hot-load` | 1.173152 | [1.117937, 1.228366] | low-confidence | 12 | 360 | 279.277 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 157 | `rv64:32:i:lhu` | `83520500` | `hot-load` | 1.172305 | [1.102765, 1.241846] | low-confidence | 12 | 360 | 272.287 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 158 | `rv64:32:i:lui` | `b7120000` | `independent` | 0.000000 | [0.006329, 0.048175] | low-confidence | 12 | 360 | 175.852 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 159 | `rv64:32:i:lw` | `83220500` | `hot-load` | 1.168284 | [1.098822, 1.237746] | low-confidence | 12 | 360 | 286.057 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 160 | `rv64:32:i:lwu` | `83620500` | `hot-load` | 1.164752 | [1.076933, 1.252571] | low-confidence | 12 | 360 | 275.442 | 1.000 | 0 | too-many-severe-outliers<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 161 | `rv64:32:i:or` | `3365b500` | `dependency` | 0.296318 | [0.271298, 0.321338] | low-confidence | 12 | 360 | 169.106 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 162 | `rv64:32:i:ori` | `13651500` | `dependency` | 0.298716 | [0.275484, 0.321947] | low-confidence | 12 | 360 | 154.959 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-unavailable |
| 163 | `rv64:32:i:sb` | `2300b500` | `hot-store` | 2.166466 | [2.034253, 2.298679] | low-confidence | 12 | 360 | 240.281 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 164 | `rv64:32:i:sd` | `2330b500` | `hot-store` | 1.306196 | [1.131722, 1.480670] | low-confidence | 12 | 360 | 228.956 | 1.000 | 0 | cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>plugin-off-check-divergent |
| 165 | `rv64:32:i:sh` | `2310b500` | `hot-store` | 2.166924 | [2.029288, 2.304559] | low-confidence | 12 | 360 | 286.870 | 1.000 | 0 | drift-effect-not-equivalent<br>plugin-off-check-divergent |
| 166 | `rv64:32:i:sll` | `3315b500` | `dependency` | 0.315160 | [0.277861, 0.352458] | low-confidence | 12 | 360 | 195.940 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 167 | `rv64:32:i:slli` | `13151500` | `dependency` | 0.000000 | [-0.014749, 0.061302] | low-confidence | 12 | 360 | 183.296 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 168 | `rv64:32:i:slliw` | `1b151500` | `dependency` | 0.000000 | [-0.000302, 0.046687] | low-confidence | 12 | 360 | 177.104 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 169 | `rv64:32:i:sllw` | `3b15b500` | `dependency` | 0.630890 | [0.598511, 0.663269] | low-confidence | 12 | 360 | 198.434 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 170 | `rv64:32:i:slt` | `3325b500` | `dependency` | 0.895664 | [0.860050, 0.931277] | low-confidence | 12 | 360 | 200.270 | 1.000 | 0 | drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 171 | `rv64:32:i:slt:form=sgtz` | `3325b000` | `dependency` | 0.000000 | [0.001006, 0.046463] | low-confidence | 12 | 360 | 157.334 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 172 | `rv64:32:i:slti` | `13251500` | `dependency` | 0.900136 | [0.873708, 0.926564] | low-confidence | 12 | 360 | 211.433 | 1.000 | 0 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 173 | `rv64:32:i:sltiu` | `13352500` | `dependency` | 0.000000 | [-0.010432, 0.042324] | low-confidence | 12 | 360 | 153.529 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 174 | `rv64:32:i:sltiu:form=seqz` | `13351500` | `dependency` | 0.303144 | [0.278187, 0.328101] | low-confidence | 12 | 360 | 211.929 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 175 | `rv64:32:i:sltu` | `3335b500` | `dependency` | 0.898151 | [0.867311, 0.928992] | low-confidence | 12 | 360 | 191.536 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 176 | `rv64:32:i:sltu:form=snez` | `3335b000` | `dependency` | 0.000000 | [-0.000516, 0.055760] | low-confidence | 12 | 359 | 145.609 | 1.000 | 1 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 177 | `rv64:32:i:sra` | `3355b540` | `dependency` | 0.317427 | [0.296121, 0.338733] | low-confidence | 12 | 360 | 212.041 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 178 | `rv64:32:i:srai` | `13551540` | `dependency` | 0.307568 | [0.280131, 0.335006] | low-confidence | 12 | 360 | 180.093 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 179 | `rv64:32:i:sraiw` | `1b551540` | `dependency` | 0.305250 | [0.278587, 0.331914] | low-confidence | 12 | 360 | 214.662 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 180 | `rv64:32:i:sraw` | `3b55b540` | `dependency` | 0.338522 | [0.301346, 0.375698] | low-confidence | 12 | 360 | 167.000 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 181 | `rv64:32:i:srl` | `3355b500` | `dependency` | 0.312714 | [0.281912, 0.343517] | low-confidence | 12 | 360 | 167.862 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 182 | `rv64:32:i:srli` | `13551500` | `dependency` | 0.000000 | [-0.007620, 0.050073] | low-confidence | 12 | 360 | 180.378 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 183 | `rv64:32:i:srliw` | `1b551500` | `dependency` | 0.000000 | [0.001249, 0.054169] | low-confidence | 12 | 360 | 165.728 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 184 | `rv64:32:i:srlw` | `3b55b500` | `dependency` | 0.623221 | [0.591902, 0.654541] | low-confidence | 12 | 360 | 219.105 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 185 | `rv64:32:i:sub` | `3305b540` | `dependency` | 0.297705 | [0.264813, 0.330597] | low-confidence | 12 | 360 | 189.358 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 186 | `rv64:32:i:sub:form=neg` | `3305b040` | `dependency` | 0.000000 | [-0.006211, 0.042990] | low-confidence | 12 | 359 | 197.991 | 1.000 | 1 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 187 | `rv64:32:i:subw` | `3b05b540` | `dependency` | 0.610246 | [0.578228, 0.642264] | low-confidence | 12 | 360 | 187.639 | 1.000 | 0 | per-run-irls-not-converged<br>batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 188 | `rv64:32:i:subw:form=negw` | `3b05b040` | `dependency` | 0.000000 | [0.000256, 0.046200] | low-confidence | 12 | 360 | 166.147 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 189 | `rv64:32:i:sw` | `2320b500` | `hot-store` | 1.215244 | [1.008013, 1.422474] | low-confidence | 12 | 360 | 252.305 | 1.000 | 0 | simultaneous-ci-too-wide<br>cross-run-heterogeneity-high<br>order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 190 | `rv64:32:i:xor` | `3345b500` | `dependency` | 0.312130 | [0.282380, 0.341880] | low-confidence | 12 | 360 | 177.238 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 191 | `rv64:32:i:xori` | `13451500` | `dependency` | 0.304549 | [0.279177, 0.329920] | low-confidence | 12 | 360 | 227.126 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 192 | `rv64:32:i:xori:form=not` | `1345f5ff` | `dependency` | 0.296816 | [0.267086, 0.326547] | low-confidence | 12 | 360 | 184.029 | 1.000 | 0 | order-effect-not-equivalent<br>drift-effect-not-equivalent<br>batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 193 | `rv64:32:m:div` | `3345b502` | `dependency` | 4.524948 | [4.468895, 4.581001] | low-confidence | 12 | 360 | 297.342 | 1.000 | 0 | plugin-off-check-divergent |
| 194 | `rv64:32:m:divu` | `3355b502` | `dependency` | 3.184151 | [3.122878, 3.245424] | low-confidence | 12 | 360 | 256.414 | 1.000 | 0 | plugin-off-check-divergent |
| 195 | `rv64:32:m:divuw` | `3b55b502` | `dependency` | 3.338647 | [3.300759, 3.376535] | high-confidence | 12 | 360 | 235.682 | 1.000 | 0 |  |
| 196 | `rv64:32:m:divw` | `3b45b502` | `dependency` | 4.979592 | [4.909707, 5.049476] | low-confidence | 12 | 360 | 269.274 | 1.000 | 0 | plugin-off-check-divergent |
| 197 | `rv64:32:m:mul` | `3305b502` | `dependency` | 0.899502 | [0.865636, 0.933367] | low-confidence | 12 | 360 | 230.040 | 1.000 | 0 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 198 | `rv64:32:m:mulh` | `3315b502` | `dependency` | 1.191744 | [1.160002, 1.223487] | low-confidence | 12 | 360 | 185.485 | 1.000 | 0 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 199 | `rv64:32:m:mulhsu` | `3325b502` | `dependency` | 3.152963 | [3.097681, 3.208246] | low-confidence | 12 | 360 | 240.954 | 1.000 | 0 | plugin-off-check-divergent |
| 200 | `rv64:32:m:mulhu` | `3335b502` | `dependency` | 1.194600 | [1.163041, 1.226159] | low-confidence | 12 | 360 | 195.110 | 1.000 | 0 | cross-clock-check-divergent<br>plugin-off-check-divergent |
| 201 | `rv64:32:m:mulw` | `3b05b502` | `dependency` | 1.190111 | [1.156667, 1.223556] | low-confidence | 12 | 360 | 177.266 | 1.000 | 0 | batch-size-nonlinearity<br>cross-clock-check-divergent<br>plugin-off-check-divergent |
| 202 | `rv64:32:m:rem` | `3365b502` | `dependency` | 5.384638 | [5.313330, 5.455946] | high-confidence | 12 | 359 | 297.222 | 1.000 | 1 |  |
| 203 | `rv64:32:m:remu` | `3375b502` | `dependency` | 3.296301 | [3.244312, 3.348291] | low-confidence | 12 | 360 | 215.508 | 1.000 | 0 | per-run-irls-not-converged<br>cross-clock-check-unavailable<br>plugin-off-check-divergent |
| 204 | `rv64:32:m:remuw` | `3b75b502` | `dependency` | 3.632690 | [3.569191, 3.696188] | low-confidence | 12 | 360 | 230.901 | 1.000 | 0 | plugin-off-check-divergent |
| 205 | `rv64:32:m:remw` | `3b65b502` | `dependency` | 5.733569 | [5.639920, 5.827218] | high-confidence | 12 | 360 | 235.876 | 1.000 | 0 |  |
| 206 | `rv64:32:zicsr:csrrs:csr=0x001:write=0` | `73251000` | `csr-0x001-read` | 114.410167 | [109.339887, 119.480448] | low-confidence | 12 | 348 | 262.083 | 1.000 | 12 | batch-size-nonlinearity |
| 207 | `rv64:32:zicsr:csrrs:csr=0x002:write=0` | `73252000` | `csr-0x002-read` | 112.327526 | [107.150472, 117.504580] | low-confidence | 12 | 348 | 238.864 | 1.000 | 12 | batch-size-nonlinearity |
| 208 | `rv64:32:zicsr:csrrs:csr=0x003:write=0` | `73253000` | `csr-0x003-read` | 113.618605 | [108.584398, 118.652813] | low-confidence | 12 | 348 | 266.932 | 1.000 | 12 | batch-size-nonlinearity<br>plugin-off-check-unavailable |
| 209 | `rv64:32:zicsr:csrrs:csr=0xc01:write=0` | `732510c0` | `csr-0xc01-read` | 1719.667204 | [1710.847401, 1728.487008] | high-confidence | 12 | 360 | 214.160 | 1.000 | 0 |  |
| 210 | `rv64:32:zicsr:csrrw:csr=0x003:read=0` | `73103000` | `csr-0x003-write` | 104.376422 | [101.036192, 107.716651] | low-confidence | 12 | 252 | 130.532 | 1.000 | 108 | batch-size-nonlinearity<br>cross-clock-check-unavailable<br>plugin-off-check-unavailable |
| 211 | `rv64:32:zicsr:csrrwi:csr=0x003:read=0:zimm=0x00` | `73503000` | `csr-0x003-write` | 103.697431 | [99.678746, 107.716117] | low-confidence | 12 | 251 | 124.964 | 1.000 | 109 | per-run-irls-not-converged<br>batch-size-nonlinearity<br>cross-clock-check-unavailable |
| 212 | `rv64:32:zifencei:fence.i` | `0f100000` | `serialization` | 85.314396 | [82.822944, 87.805848] | low-confidence | 12 | 360 | 277.636 | 1.000 | 0 | batch-size-nonlinearity |
| 213 | `rv64:32:zihintpause:pause` | `0f000001` | `hint` | 84.984405 | [82.404265, 87.564545] | low-confidence | 12 | 360 | 254.015 | 1.000 | 0 | batch-size-nonlinearity |

## 附录 B：409 个 BuildStorm catalog 类完整映射

`raw count` 带 `~` 表示 seen-table 估算；`occurrences` 是 TB catalog 中的翻译槽出现量，不是运行时动态执行次数。`strict ns` 只在全部门禁通过时填充；`measured ns` 可包含单一 low-confidence 探索估计。

| # | catalog key | ext | bytes | raw count | RSE | occurrences | strict ns | measured ns | estimate quality | assignment |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| 1 | `rv64:16:c:c.add` | `c` | 2 | ~712 | 0.0325 | 204,800 |  | 0.307516 | low-confidence | `measured-but-confidence-gates-failed` |
| 2 | `rv64:16:c:c.addi` | `c` | 2 | ~797 | 0.0325 | 142,373 |  | 0.304301 | low-confidence | `measured-but-confidence-gates-failed` |
| 3 | `rv64:16:c:c.addi16sp` | `c` | 2 | 60 |  | 104,770 |  | 0.299824 | low-confidence | `measured-but-confidence-gates-failed` |
| 4 | `rv64:16:c:c.addi4spn` | `c` | 2 | ~1,250 | 0.0325 | 90,854 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 5 | `rv64:16:c:c.addiw` | `c` | 2 | ~271 | 0.0325 | 18,657 |  | 0.616431 | low-confidence | `measured-but-confidence-gates-failed` |
| 6 | `rv64:16:c:c.addiw:form=sext.w` | `c` | 2 | 27 |  | 9,433 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 7 | `rv64:16:c:c.addw` | `c` | 2 | 51 |  | 2,711 |  | 0.613338 | low-confidence | `measured-but-confidence-gates-failed` |
| 8 | `rv64:16:c:c.and` | `c` | 2 | 56 |  | 37,601 |  | 0.309817 | low-confidence | `measured-but-confidence-gates-failed` |
| 9 | `rv64:16:c:c.andi` | `c` | 2 | 225 |  | 28,996 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 10 | `rv64:16:c:c.beqz` | `c` | 2 | ~1,747 | 0.0325 | 144,773 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 11 | `rv64:16:c:c.bnez` | `c` | 2 | ~1,688 | 0.0325 | 71,254 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 12 | `rv64:16:c:c.fld` | `c` | 2 | 8 |  | 40 |  | 1.855563 | low-confidence | `measured-but-confidence-gates-failed` |
| 13 | `rv64:16:c:c.fldsp` | `c` | 2 | 37 |  | 515 |  | 1.322337 | low-confidence | `measured-but-confidence-gates-failed` |
| 14 | `rv64:16:c:c.fsd` | `c` | 2 | 11 |  | 620 |  | 1.682728 | low-confidence | `measured-but-confidence-gates-failed` |
| 15 | `rv64:16:c:c.fsdsp` | `c` | 2 | 65 |  | 658 |  | 1.778118 | low-confidence | `measured-but-confidence-gates-failed` |
| 16 | `rv64:16:c:c.j` | `c` | 2 | ~1,995 | 0.0325 | 101,232 |  | 2.368731 | low-confidence | `measured-but-confidence-gates-failed` |
| 17 | `rv64:16:c:c.jalr` | `c` | 2 | 27 |  | 17,694 |  | 68.734911 | low-confidence | `measured-but-confidence-gates-failed` |
| 18 | `rv64:16:c:c.jr:form=jr` | `c` | 2 | 19 |  | 6,619 |  | 68.371240 | low-confidence | `measured-but-confidence-gates-failed` |
| 19 | `rv64:16:c:c.jr:form=ret` | `c` | 2 | 1 |  | 82,450 |  | 69.182657 | low-confidence | `measured-but-confidence-gates-failed` |
| 20 | `rv64:16:c:c.ld` | `c` | 2 | ~1,747 | 0.0325 | 216,586 |  | 1.167796 | low-confidence | `measured-but-confidence-gates-failed` |
| 21 | `rv64:16:c:c.ldsp` | `c` | 2 | ~1,786 | 0.0325 | 715,012 |  | 1.139748 | low-confidence | `measured-but-confidence-gates-failed` |
| 22 | `rv64:16:c:c.li` | `c` | 2 | ~1,120 | 0.0325 | 342,900 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 23 | `rv64:16:c:c.lui` | `c` | 2 | ~730 | 0.0325 | 40,749 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 24 | `rv64:16:c:c.lw` | `c` | 2 | ~1,299 | 0.0325 | 50,263 |  | 1.134475 | low-confidence | `measured-but-confidence-gates-failed` |
| 25 | `rv64:16:c:c.lwsp` | `c` | 2 | ~1,093 | 0.0325 | 13,512 |  | 1.170763 | low-confidence | `measured-but-confidence-gates-failed` |
| 26 | `rv64:16:c:c.mv` | `c` | 2 | ~778 | 0.0325 | 518,410 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 27 | `rv64:16:c:c.nop` | `c` | 2 | 1 |  | 2 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 28 | `rv64:16:c:c.or` | `c` | 2 | 56 |  | 84,061 |  | 0.303830 | low-confidence | `measured-but-confidence-gates-failed` |
| 29 | `rv64:16:c:c.sd` | `c` | 2 | ~1,310 | 0.0325 | 136,514 |  | 1.187832 | low-confidence | `measured-but-confidence-gates-failed` |
| 30 | `rv64:16:c:c.sdsp` | `c` | 2 | ~1,988 | 0.0325 | 680,056 |  | 1.189236 | low-confidence | `measured-but-confidence-gates-failed` |
| 31 | `rv64:16:c:c.slli` | `c` | 2 | ~835 | 0.0325 | 168,621 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 32 | `rv64:16:c:c.srai` | `c` | 2 | 78 |  | 1,985 |  | 0.313509 | low-confidence | `measured-but-confidence-gates-failed` |
| 33 | `rv64:16:c:c.srli` | `c` | 2 | ~352 | 0.0325 | 43,294 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 34 | `rv64:16:c:c.sub` | `c` | 2 | 57 |  | 15,352 |  | 0.289465 | low-confidence | `measured-but-confidence-gates-failed` |
| 35 | `rv64:16:c:c.subw` | `c` | 2 | 55 |  | 2,250 |  | 0.604613 | low-confidence | `measured-but-confidence-gates-failed` |
| 36 | `rv64:16:c:c.sw` | `c` | 2 | ~920 | 0.0325 | 19,045 |  | 1.238632 | low-confidence | `measured-but-confidence-gates-failed` |
| 37 | `rv64:16:c:c.swsp` | `c` | 2 | ~1,357 | 0.0325 | 25,424 |  | 1.246205 | low-confidence | `measured-but-confidence-gates-failed` |
| 38 | `rv64:16:c:c.xor` | `c` | 2 | 56 |  | 7,786 |  | 0.305448 | low-confidence | `measured-but-confidence-gates-failed` |
| 39 | `rv64:32:a:amoadd.d:aq=0:rl=0` | `a` | 4 | ~368 | 0.0325 | 2,265 |  | 2.483577 | low-confidence | `measured-but-confidence-gates-failed` |
| 40 | `rv64:32:a:amoadd.d:aq=0:rl=1` | `a` | 4 | 136 |  | 3,207 |  | 2.497640 | low-confidence | `measured-but-confidence-gates-failed` |
| 41 | `rv64:32:a:amoadd.d:aq=1:rl=0` | `a` | 4 | 1 |  | 1 |  | 2.503248 | low-confidence | `measured-but-confidence-gates-failed` |
| 42 | `rv64:32:a:amoadd.d:aq=1:rl=1` | `a` | 4 | 26 |  | 154 |  | 2.477804 | low-confidence | `measured-but-confidence-gates-failed` |
| 43 | `rv64:32:a:amoadd.w:aq=0:rl=0` | `a` | 4 | 23 |  | 277 |  | 2.749203 | low-confidence | `measured-but-confidence-gates-failed` |
| 44 | `rv64:32:a:amoadd.w:aq=0:rl=1` | `a` | 4 | 9 |  | 47 |  | 2.746105 | low-confidence | `measured-but-confidence-gates-failed` |
| 45 | `rv64:32:a:amoadd.w:aq=1:rl=0` | `a` | 4 | 13 |  | 28 |  | 2.754931 | low-confidence | `measured-but-confidence-gates-failed` |
| 46 | `rv64:32:a:amoadd.w:aq=1:rl=1` | `a` | 4 | 2 |  | 6 |  | 2.752791 | low-confidence | `measured-but-confidence-gates-failed` |
| 47 | `rv64:32:a:amoand.d:aq=0:rl=1` | `a` | 4 | 2 |  | 3 |  | 2.474192 | low-confidence | `measured-but-confidence-gates-failed` |
| 48 | `rv64:32:a:amoand.d:aq=1:rl=1` | `a` | 4 | 4 |  | 17 |  | 2.493657 | low-confidence | `measured-but-confidence-gates-failed` |
| 49 | `rv64:32:a:amoand.w:aq=1:rl=1` | `a` | 4 | 18 |  | 67 |  | 2.776823 | low-confidence | `measured-but-confidence-gates-failed` |
| 50 | `rv64:32:a:amomaxu.d:aq=1:rl=0` | `a` | 4 | 2 |  | 3 |  | 2.808310 | low-confidence | `measured-but-confidence-gates-failed` |
| 51 | `rv64:32:a:amomaxu.d:aq=1:rl=1` | `a` | 4 | 1 |  | 2 |  | 2.787922 | low-confidence | `measured-but-confidence-gates-failed` |
| 52 | `rv64:32:a:amomaxu.w:aq=1:rl=0` | `a` | 4 | 1 |  | 2 |  | 3.002997 | low-confidence | `measured-but-confidence-gates-failed` |
| 53 | `rv64:32:a:amoor.d:aq=0:rl=0` | `a` | 4 | 2 |  | 7 |  | 2.462590 | low-confidence | `measured-but-confidence-gates-failed` |
| 54 | `rv64:32:a:amoor.d:aq=0:rl=1` | `a` | 4 | 2 |  | 4 |  | 2.470735 | low-confidence | `measured-but-confidence-gates-failed` |
| 55 | `rv64:32:a:amoor.d:aq=1:rl=0` | `a` | 4 | 2 |  | 3 |  | 2.464319 | low-confidence | `measured-but-confidence-gates-failed` |
| 56 | `rv64:32:a:amoor.d:aq=1:rl=1` | `a` | 4 | 11 |  | 26 |  | 2.486289 | low-confidence | `measured-but-confidence-gates-failed` |
| 57 | `rv64:32:a:amoor.w:aq=0:rl=0` | `a` | 4 | 2 |  | 2 |  | 2.769137 | low-confidence | `measured-but-confidence-gates-failed` |
| 58 | `rv64:32:a:amoor.w:aq=0:rl=1` | `a` | 4 | 7 |  | 62 |  | 2.781640 | low-confidence | `measured-but-confidence-gates-failed` |
| 59 | `rv64:32:a:amoor.w:aq=1:rl=0` | `a` | 4 | 223 |  | 2,552 |  | 2.791954 | low-confidence | `measured-but-confidence-gates-failed` |
| 60 | `rv64:32:a:amoor.w:aq=1:rl=1` | `a` | 4 | 13 |  | 69 |  | 2.754687 | low-confidence | `measured-but-confidence-gates-failed` |
| 61 | `rv64:32:a:amoswap.d:aq=0:rl=0` | `a` | 4 | 3 |  | 4 |  | 2.388765 | low-confidence | `measured-but-confidence-gates-failed` |
| 62 | `rv64:32:a:amoswap.d:aq=1:rl=0` | `a` | 4 | 2 |  | 6 |  | 2.440741 | low-confidence | `measured-but-confidence-gates-failed` |
| 63 | `rv64:32:a:amoswap.d:aq=1:rl=1` | `a` | 4 | 20 |  | 82 |  | 2.360036 | low-confidence | `measured-but-confidence-gates-failed` |
| 64 | `rv64:32:a:amoswap.w:aq=0:rl=0` | `a` | 4 | 6 |  | 13 |  | 2.428314 | low-confidence | `measured-but-confidence-gates-failed` |
| 65 | `rv64:32:a:amoswap.w:aq=0:rl=1` | `a` | 4 | 42 |  | 229 |  | 2.476489 | low-confidence | `measured-but-confidence-gates-failed` |
| 66 | `rv64:32:a:amoswap.w:aq=1:rl=0` | `a` | 4 | 9 |  | 37 |  | 2.437709 | low-confidence | `measured-but-confidence-gates-failed` |
| 67 | `rv64:32:a:amoswap.w:aq=1:rl=1` | `a` | 4 | 3 |  | 11 |  | 2.493681 | low-confidence | `measured-but-confidence-gates-failed` |
| 68 | `rv64:32:a:lr.d:aq=0:rl=0` | `a` | 4 | 25 |  | 88 |  | 1.281369 | low-confidence | `measured-but-confidence-gates-failed` |
| 69 | `rv64:32:a:lr.d:aq=1:rl=0` | `a` | 4 | 69 |  | 723 |  | 2.280094 | low-confidence | `measured-but-confidence-gates-failed` |
| 70 | `rv64:32:a:lr.d:aq=1:rl=1` | `a` | 4 | 11 |  | 20 |  | 2.256319 | low-confidence | `measured-but-confidence-gates-failed` |
| 71 | `rv64:32:a:lr.w:aq=0:rl=0` | `a` | 4 | 6 |  | 15 |  | 1.296992 | low-confidence | `measured-but-confidence-gates-failed` |
| 72 | `rv64:32:a:lr.w:aq=1:rl=0` | `a` | 4 | 63 |  | 504 |  | 2.258599 | low-confidence | `measured-but-confidence-gates-failed` |
| 73 | `rv64:32:a:lr.w:aq=1:rl=1` | `a` | 4 | 8 |  | 63 |  | 2.323048 | low-confidence | `measured-but-confidence-gates-failed` |
| 74 | `rv64:32:a:sc.d:aq=0:rl=0` | `a` | 4 | 89 |  | 525 |  | 3.532279 | low-confidence | `measured-but-confidence-gates-failed` |
| 75 | `rv64:32:a:sc.d:aq=0:rl=1` | `a` | 4 | 71 |  | 223 |  | 3.569757 | low-confidence | `measured-but-confidence-gates-failed` |
| 76 | `rv64:32:a:sc.d:aq=1:rl=0` | `a` | 4 | 1 |  | 1 |  | 3.554475 | low-confidence | `measured-but-confidence-gates-failed` |
| 77 | `rv64:32:a:sc.w:aq=0:rl=0` | `a` | 4 | 83 |  | 320 |  | 3.573791 | low-confidence | `measured-but-confidence-gates-failed` |
| 78 | `rv64:32:a:sc.w:aq=0:rl=1` | `a` | 4 | 34 |  | 124 |  | 3.548578 | low-confidence | `measured-but-confidence-gates-failed` |
| 79 | `rv64:32:a:sc.w:aq=1:rl=0` | `a` | 4 | 1 |  | 2 |  | 3.591566 | low-confidence | `measured-but-confidence-gates-failed` |
| 80 | `rv64:32:a:sc.w:aq=1:rl=1` | `a` | 4 | 4 |  | 42 |  | 3.552325 | low-confidence | `measured-but-confidence-gates-failed` |
| 81 | `rv64:32:d:fadd.d:rm=dyn` | `d` | 4 | 4 |  | 15 |  | 11.076651 | low-confidence | `measured-but-confidence-gates-failed` |
| 82 | `rv64:32:d:fclass.d` | `d` | 4 | 1 |  | 1 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 83 | `rv64:32:d:fcvt.d.l:rm=dyn` | `d` | 4 | 2 |  | 3 |  | 12.466082 | low-confidence | `measured-but-confidence-gates-failed` |
| 84 | `rv64:32:d:fcvt.d.lu:rm=dyn` | `d` | 4 | 8 |  | 10 |  | 11.548073 | low-confidence | `measured-but-confidence-gates-failed` |
| 85 | `rv64:32:d:fcvt.d.w:rm=rne` | `d` | 4 | 5 |  | 22 |  | 12.520855 | low-confidence | `measured-but-confidence-gates-failed` |
| 86 | `rv64:32:d:fcvt.l.d:rm=rtz` | `d` | 4 | 2 |  | 4 |  | 11.655546 | low-confidence | `measured-but-confidence-gates-failed` |
| 87 | `rv64:32:d:fcvt.lu.d:rm=rtz` | `d` | 4 | 4 |  | 9 |  | 11.510599 | low-confidence | `measured-but-confidence-gates-failed` |
| 88 | `rv64:32:d:fcvt.s.d:rm=dyn` | `d` | 4 | 1 |  | 10 |  | 11.500906 | low-confidence | `measured-but-confidence-gates-failed` |
| 89 | `rv64:32:d:fcvt.w.d:rm=rtz` | `d` | 4 | 2 |  | 8 |  | 12.352561 | low-confidence | `measured-but-confidence-gates-failed` |
| 90 | `rv64:32:d:fdiv.d:rm=dyn` | `d` | 4 | 4 |  | 4 |  | 6.562581 | low-confidence | `measured-but-confidence-gates-failed` |
| 91 | `rv64:32:d:feq.d` | `d` | 4 | 4 |  | 12 |  | 5.818416 | low-confidence | `measured-but-confidence-gates-failed` |
| 92 | `rv64:32:d:fld` | `d` | 4 | 113 |  | 241 |  | 1.519900 | low-confidence | `measured-but-confidence-gates-failed` |
| 93 | `rv64:32:d:fle.d` | `d` | 4 | 3 |  | 8 |  | 5.933362 | low-confidence | `measured-but-confidence-gates-failed` |
| 94 | `rv64:32:d:flt.d` | `d` | 4 | 2 |  | 3 |  | 6.225205 | low-confidence | `measured-but-confidence-gates-failed` |
| 95 | `rv64:32:d:fmul.d:rm=dyn` | `d` | 4 | 2 |  | 5 |  | 4.496168 | low-confidence | `measured-but-confidence-gates-failed` |
| 96 | `rv64:32:d:fmv.d.x` | `d` | 4 | 9 |  | 56 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 97 | `rv64:32:d:fmv.x.d` | `d` | 4 | 4 |  | 10 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 98 | `rv64:32:d:fsd` | `d` | 4 | 35 |  | 2,306 |  | 1.840906 | low-confidence | `measured-but-confidence-gates-failed` |
| 99 | `rv64:32:d:fsgnj.d` | `d` | 4 | 7 |  | 29 |  | 0.933594 | low-confidence | `measured-but-confidence-gates-failed` |
| 100 | `rv64:32:d:fsub.d:rm=dyn` | `d` | 4 | 2 |  | 3 |  | 11.052511 | low-confidence | `measured-but-confidence-gates-failed` |
| 101 | `rv64:32:f:fcvt.s.lu:rm=dyn` | `f` | 4 | 1 |  | 1 |  | 12.029701 | low-confidence | `measured-but-confidence-gates-failed` |
| 102 | `rv64:32:f:fdiv.s:rm=dyn` | `f` | 4 | 1 |  | 1 |  | 12.295920 | low-confidence | `measured-but-confidence-gates-failed` |
| 103 | `rv64:32:f:flw` | `f` | 4 | 33 |  | 101 |  | 1.601599 | low-confidence | `measured-but-confidence-gates-failed` |
| 104 | `rv64:32:f:fmv.w.x` | `f` | 4 | 32 |  | 32 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 105 | `rv64:32:f:fmv.x.w` | `f` | 4 | 2 |  | 3 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 106 | `rv64:32:f:fsw` | `f` | 4 | 34 |  | 208 |  | 1.821230 | low-confidence | `measured-but-confidence-gates-failed` |
| 107 | `rv64:32:i:add` | `i` | 4 | ~5,678 | 0.0325 | 49,396 |  | 0.302668 | low-confidence | `measured-but-confidence-gates-failed` |
| 108 | `rv64:32:i:addi` | `i` | 4 | ~53,779 | 0.0325 | 440,755 |  | 0.309766 | low-confidence | `measured-but-confidence-gates-failed` |
| 109 | `rv64:32:i:addi:form=li` | `i` | 4 | ~5,105 | 0.0325 | 95,062 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 110 | `rv64:32:i:addi:form=mv` | `i` | 4 | 20 |  | 167 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 111 | `rv64:32:i:addi:form=nop` | `i` | 4 | 1 |  | 49 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 112 | `rv64:32:i:addiw` | `i` | 4 | ~3,052 | 0.0325 | 14,143 |  | 0.605556 | low-confidence | `measured-but-confidence-gates-failed` |
| 113 | `rv64:32:i:addiw:form=sext.w` | `i` | 4 | ~546 | 0.0325 | 12,083 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 114 | `rv64:32:i:addw` | `i` | 4 | ~905 | 0.0325 | 5,045 |  | 0.610007 | low-confidence | `measured-but-confidence-gates-failed` |
| 115 | `rv64:32:i:and` | `i` | 4 | ~3,549 | 0.0325 | 37,502 |  | 0.304178 | low-confidence | `measured-but-confidence-gates-failed` |
| 116 | `rv64:32:i:andi` | `i` | 4 | ~3,177 | 0.0325 | 56,944 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 117 | `rv64:32:i:auipc` | `i` | 4 | ~59,597 | 0.0325 | 632,160 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 118 | `rv64:32:i:beq` | `i` | 4 | ~40,231 | 0.0325 | 164,647 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 119 | `rv64:32:i:bge` | `i` | 4 | ~2,855 | 0.0325 | 9,801 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 120 | `rv64:32:i:bgeu` | `i` | 4 | ~14,749 | 0.0325 | 50,231 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 121 | `rv64:32:i:blt` | `i` | 4 | ~4,197 | 0.0325 | 17,682 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 122 | `rv64:32:i:bltu` | `i` | 4 | ~14,058 | 0.0325 | 55,696 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 123 | `rv64:32:i:bne` | `i` | 4 | ~27,772 | 0.0325 | 115,489 |  |  | context-dependent | `measured-but-confidence-gates-failed` |
| 124 | `rv64:32:i:ebreak` | `i` | 4 | 1 |  | 1 |  |  | restricted-context | `trap-path-is-context-dependent` |
| 125 | `rv64:32:i:ecall` | `i` | 4 | 1 |  | 2,306 |  |  | restricted-context | `trap-path-is-context-dependent` |
| 126 | `rv64:32:i:fence:fm=0x0:pred=0x1:succ=0x1` | `i` | 4 | 1 |  | 6 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 127 | `rv64:32:i:fence:fm=0x0:pred=0x1:succ=0x4` | `i` | 4 | 1 |  | 6 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 128 | `rv64:32:i:fence:fm=0x0:pred=0x2:succ=0x2` | `i` | 4 | 1 |  | 204 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 129 | `rv64:32:i:fence:fm=0x0:pred=0x2:succ=0x3` | `i` | 4 | 1 |  | 12,311 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 130 | `rv64:32:i:fence:fm=0x0:pred=0x3:succ=0x1` | `i` | 4 | 1 |  | 3,745 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 131 | `rv64:32:i:fence:fm=0x0:pred=0x3:succ=0x3` | `i` | 4 | 1 |  | 454 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 132 | `rv64:32:i:fence:fm=0x0:pred=0x5:succ=0x5` | `i` | 4 | 1 |  | 14 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 133 | `rv64:32:i:fence:fm=0x0:pred=0x8:succ=0x2` | `i` | 4 | 1 |  | 4 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 134 | `rv64:32:i:fence:fm=0x0:pred=0xa:succ=0xa` | `i` | 4 | 1 |  | 3 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 135 | `rv64:32:i:fence:fm=0x0:pred=0xf:succ=0x5` | `i` | 4 | 1 |  | 73 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 136 | `rv64:32:i:fence:fm=0x0:pred=0xf:succ=0xf` | `i` | 4 | 1 |  | 530 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 137 | `rv64:32:i:jal:form=call` | `i` | 4 | ~18,819 | 0.0325 | 60,360 |  | 2.557538 | low-confidence | `measured-but-confidence-gates-failed` |
| 138 | `rv64:32:i:jal:form=j` | `i` | 4 | ~5,450 | 0.0325 | 12,128 |  | 2.388337 | low-confidence | `measured-but-confidence-gates-failed` |
| 139 | `rv64:32:i:jalr:form=call` | `i` | 4 | ~2,082 | 0.0325 | 215,605 |  | 68.757847 | low-confidence | `measured-but-confidence-gates-failed` |
| 140 | `rv64:32:i:jalr:form=jr` | `i` | 4 | ~1,914 | 0.0325 | 9,181 |  | 69.510822 | low-confidence | `measured-but-confidence-gates-failed` |
| 141 | `rv64:32:i:jalr:form=link` | `i` | 4 | 32 |  | 6,412 |  | 68.737955 | low-confidence | `measured-but-confidence-gates-failed` |
| 142 | `rv64:32:i:jalr:form=ret` | `i` | 4 | 1 |  | 133 |  | 66.482640 | low-confidence | `measured-but-confidence-gates-failed` |
| 143 | `rv64:32:i:lb` | `i` | 4 | ~889 | 0.0325 | 7,631 |  | 1.162230 | low-confidence | `measured-but-confidence-gates-failed` |
| 144 | `rv64:32:i:lbu` | `i` | 4 | ~26,883 | 0.0325 | 258,474 |  | 1.145496 | low-confidence | `measured-but-confidence-gates-failed` |
| 145 | `rv64:32:i:ld` | `i` | 4 | ~44,524 | 0.0325 | 479,305 |  | 1.150169 | low-confidence | `measured-but-confidence-gates-failed` |
| 146 | `rv64:32:i:lh` | `i` | 4 | ~1,465 | 0.0325 | 9,425 |  | 1.173152 | low-confidence | `measured-but-confidence-gates-failed` |
| 147 | `rv64:32:i:lhu` | `i` | 4 | ~3,400 | 0.0325 | 29,946 |  | 1.172305 | low-confidence | `measured-but-confidence-gates-failed` |
| 148 | `rv64:32:i:lui` | `i` | 4 | ~2,045 | 0.0325 | 32,389 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 149 | `rv64:32:i:lw` | `i` | 4 | ~11,192 | 0.0325 | 61,874 |  | 1.168284 | low-confidence | `measured-but-confidence-gates-failed` |
| 150 | `rv64:32:i:lwu` | `i` | 4 | ~6,973 | 0.0325 | 31,384 |  | 1.164752 | low-confidence | `measured-but-confidence-gates-failed` |
| 151 | `rv64:32:i:or` | `i` | 4 | ~4,471 | 0.0325 | 38,467 |  | 0.296318 | low-confidence | `measured-but-confidence-gates-failed` |
| 152 | `rv64:32:i:ori` | `i` | 4 | ~349 | 0.0325 | 9,783 |  | 0.298716 | low-confidence | `measured-but-confidence-gates-failed` |
| 153 | `rv64:32:i:sb` | `i` | 4 | ~26,379 | 0.0325 | 211,984 |  | 2.166466 | low-confidence | `measured-but-confidence-gates-failed` |
| 154 | `rv64:32:i:sd` | `i` | 4 | ~48,176 | 0.0325 | 588,726 |  | 1.306196 | low-confidence | `measured-but-confidence-gates-failed` |
| 155 | `rv64:32:i:sh` | `i` | 4 | ~7,098 | 0.0325 | 46,164 |  | 2.166924 | low-confidence | `measured-but-confidence-gates-failed` |
| 156 | `rv64:32:i:sll` | `i` | 4 | ~602 | 0.0325 | 6,677 |  | 0.315160 | low-confidence | `measured-but-confidence-gates-failed` |
| 157 | `rv64:32:i:slli` | `i` | 4 | ~4,395 | 0.0325 | 109,806 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 158 | `rv64:32:i:slliw` | `i` | 4 | ~837 | 0.0325 | 8,147 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 159 | `rv64:32:i:sllw` | `i` | 4 | ~258 | 0.0325 | 1,781 |  | 0.630890 | low-confidence | `measured-but-confidence-gates-failed` |
| 160 | `rv64:32:i:slt` | `i` | 4 | 74 |  | 130 |  | 0.895664 | low-confidence | `measured-but-confidence-gates-failed` |
| 161 | `rv64:32:i:slt:form=sgtz` | `i` | 4 | 22 |  | 183 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 162 | `rv64:32:i:slti` | `i` | 4 | 37 |  | 136 |  | 0.900136 | low-confidence | `measured-but-confidence-gates-failed` |
| 163 | `rv64:32:i:sltiu` | `i` | 4 | ~596 | 0.0325 | 2,677 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 164 | `rv64:32:i:sltiu:form=seqz` | `i` | 4 | ~287 | 0.0325 | 6,285 |  | 0.303144 | low-confidence | `measured-but-confidence-gates-failed` |
| 165 | `rv64:32:i:sltu` | `i` | 4 | ~1,141 | 0.0325 | 4,571 |  | 0.898151 | low-confidence | `measured-but-confidence-gates-failed` |
| 166 | `rv64:32:i:sltu:form=snez` | `i` | 4 | ~285 | 0.0325 | 5,849 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 167 | `rv64:32:i:sra` | `i` | 4 | 73 |  | 172 |  | 0.317427 | low-confidence | `measured-but-confidence-gates-failed` |
| 168 | `rv64:32:i:srai` | `i` | 4 | ~439 | 0.0325 | 1,675 |  | 0.307568 | low-confidence | `measured-but-confidence-gates-failed` |
| 169 | `rv64:32:i:sraiw` | `i` | 4 | 158 |  | 1,166 |  | 0.305250 | low-confidence | `measured-but-confidence-gates-failed` |
| 170 | `rv64:32:i:sraw` | `i` | 4 | 39 |  | 780 |  | 0.338522 | low-confidence | `measured-but-confidence-gates-failed` |
| 171 | `rv64:32:i:srl` | `i` | 4 | ~454 | 0.0325 | 6,410 |  | 0.312714 | low-confidence | `measured-but-confidence-gates-failed` |
| 172 | `rv64:32:i:srli` | `i` | 4 | ~4,395 | 0.0325 | 62,966 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 173 | `rv64:32:i:srliw` | `i` | 4 | ~1,460 | 0.0325 | 9,618 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 174 | `rv64:32:i:srlw` | `i` | 4 | 90 |  | 1,313 |  | 0.623221 | low-confidence | `measured-but-confidence-gates-failed` |
| 175 | `rv64:32:i:sub` | `i` | 4 | ~2,976 | 0.0325 | 27,613 |  | 0.297705 | low-confidence | `measured-but-confidence-gates-failed` |
| 176 | `rv64:32:i:sub:form=neg` | `i` | 4 | ~349 | 0.0325 | 11,946 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 177 | `rv64:32:i:subw` | `i` | 4 | ~537 | 0.0325 | 2,196 |  | 0.610246 | low-confidence | `measured-but-confidence-gates-failed` |
| 178 | `rv64:32:i:subw:form=negw` | `i` | 4 | 62 |  | 944 |  | 0.000000 | low-confidence | `measured-but-confidence-gates-failed` |
| 179 | `rv64:32:i:sw` | `i` | 4 | ~15,035 | 0.0325 | 74,142 |  | 1.215244 | low-confidence | `measured-but-confidence-gates-failed` |
| 180 | `rv64:32:i:xor` | `i` | 4 | ~1,662 | 0.0325 | 11,217 |  | 0.312130 | low-confidence | `measured-but-confidence-gates-failed` |
| 181 | `rv64:32:i:xori` | `i` | 4 | ~305 | 0.0325 | 3,586 |  | 0.304549 | low-confidence | `measured-but-confidence-gates-failed` |
| 182 | `rv64:32:i:xori:form=not` | `i` | 4 | 248 |  | 12,535 |  | 0.296816 | low-confidence | `measured-but-confidence-gates-failed` |
| 183 | `rv64:32:m:div` | `m` | 4 | 13 |  | 94 |  | 4.524948 | low-confidence | `measured-but-confidence-gates-failed` |
| 184 | `rv64:32:m:divu` | `m` | 4 | 132 |  | 445 |  | 3.184151 | low-confidence | `measured-but-confidence-gates-failed` |
| 185 | `rv64:32:m:divuw` | `m` | 4 | 43 |  | 92 | 3.338647 | 3.338647 | high-confidence | `semantic-class-transfer-from-one-raw-context` |
| 186 | `rv64:32:m:divw` | `m` | 4 | 12 |  | 13 |  | 4.979592 | low-confidence | `measured-but-confidence-gates-failed` |
| 187 | `rv64:32:m:mul` | `m` | 4 | ~2,313 | 0.0325 | 23,244 |  | 0.899502 | low-confidence | `measured-but-confidence-gates-failed` |
| 188 | `rv64:32:m:mulh` | `m` | 4 | 32 |  | 181 |  | 1.191744 | low-confidence | `measured-but-confidence-gates-failed` |
| 189 | `rv64:32:m:mulhu` | `m` | 4 | ~292 | 0.0325 | 2,509 |  | 1.194600 | low-confidence | `measured-but-confidence-gates-failed` |
| 190 | `rv64:32:m:mulw` | `m` | 4 | 78 |  | 332 |  | 1.190111 | low-confidence | `measured-but-confidence-gates-failed` |
| 191 | `rv64:32:m:rem` | `m` | 4 | 3 |  | 3 | 5.384638 | 5.384638 | high-confidence | `semantic-class-transfer-from-one-raw-context` |
| 192 | `rv64:32:m:remu` | `m` | 4 | 130 |  | 952 |  | 3.296301 | low-confidence | `measured-but-confidence-gates-failed` |
| 193 | `rv64:32:m:remuw` | `m` | 4 | 22 |  | 325 |  | 3.632690 | low-confidence | `measured-but-confidence-gates-failed` |
| 194 | `rv64:32:m:remw` | `m` | 4 | 7 |  | 14 | 5.733569 | 5.733569 | high-confidence | `semantic-class-transfer-from-one-raw-context` |
| 195 | `rv64:32:priv:hfence.gvma` | `priv` | 4 | 1 |  | 1 |  |  | restricted-context | `requires-privileged-context-probe` |
| 196 | `rv64:32:priv:mret` | `priv` | 4 | 1 |  | 8 |  |  | restricted-context | `requires-privileged-context-probe` |
| 197 | `rv64:32:priv:sfence.vma` | `priv` | 4 | 11 |  | 70 |  |  | restricted-context | `requires-privileged-context-probe` |
| 198 | `rv64:32:priv:sret` | `priv` | 4 | 1 |  | 6 |  |  | restricted-context | `requires-privileged-context-probe` |
| 199 | `rv64:32:priv:wfi` | `priv` | 4 | 1 |  | 7 |  |  | restricted-context | `requires-privileged-context-probe` |
| 200 | `rv64:32:zicboz:cbo.zero` | `zicboz` | 4 | 3 |  | 128 |  |  | restricted-context | `cache-block-operation-is-context-dependent` |
| 201 | `rv64:32:zicsr:csrrc:csr=0x100:write=1` | `zicsr` | 4 | 6 |  | 28 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 202 | `rv64:32:zicsr:csrrc:csr=0x144:write=1` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 203 | `rv64:32:zicsr:csrrci:csr=0x100:write=1:zimm=0x02` | `zicsr` | 4 | 1 |  | 21 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 204 | `rv64:32:zicsr:csrrs:csr=0x001:write=1` | `zicsr` | 4 | 1 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 205 | `rv64:32:zicsr:csrrs:csr=0x002:write=0` | `zicsr` | 4 | 5 |  | 25 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 206 | `rv64:32:zicsr:csrrs:csr=0x003:write=0` | `zicsr` | 4 | 1 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 207 | `rv64:32:zicsr:csrrs:csr=0x100:write=0` | `zicsr` | 4 | 5 |  | 16 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 208 | `rv64:32:zicsr:csrrs:csr=0x100:write=1` | `zicsr` | 4 | 8 |  | 26 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 209 | `rv64:32:zicsr:csrrs:csr=0x104:write=1` | `zicsr` | 4 | 3 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 210 | `rv64:32:zicsr:csrrs:csr=0x105:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 211 | `rv64:32:zicsr:csrrs:csr=0x106:write=1` | `zicsr` | 4 | 2 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 212 | `rv64:32:zicsr:csrrs:csr=0x141:write=0` | `zicsr` | 4 | 1 |  | 7 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 213 | `rv64:32:zicsr:csrrs:csr=0x142:write=0` | `zicsr` | 4 | 2 |  | 8 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 214 | `rv64:32:zicsr:csrrs:csr=0x143:write=0` | `zicsr` | 4 | 1 |  | 5 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 215 | `rv64:32:zicsr:csrrs:csr=0x14d:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 216 | `rv64:32:zicsr:csrrs:csr=0x180:write=0` | `zicsr` | 4 | 5 |  | 26 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 217 | `rv64:32:zicsr:csrrs:csr=0x300:write=0` | `zicsr` | 4 | 4 |  | 16 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 218 | `rv64:32:zicsr:csrrs:csr=0x300:write=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 219 | `rv64:32:zicsr:csrrs:csr=0x301:write=0` | `zicsr` | 4 | 2 |  | 16 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 220 | `rv64:32:zicsr:csrrs:csr=0x302:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 221 | `rv64:32:zicsr:csrrs:csr=0x303:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 222 | `rv64:32:zicsr:csrrs:csr=0x304:write=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 223 | `rv64:32:zicsr:csrrs:csr=0x304:write=1` | `zicsr` | 4 | 1 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 224 | `rv64:32:zicsr:csrrs:csr=0x306:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 225 | `rv64:32:zicsr:csrrs:csr=0x30a:write=0` | `zicsr` | 4 | 2 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 226 | `rv64:32:zicsr:csrrs:csr=0x30c:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 227 | `rv64:32:zicsr:csrrs:csr=0x320:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 228 | `rv64:32:zicsr:csrrs:csr=0x321:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 229 | `rv64:32:zicsr:csrrs:csr=0x340:write=0` | `zicsr` | 4 | 7 |  | 48 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 230 | `rv64:32:zicsr:csrrs:csr=0x341:write=0` | `zicsr` | 4 | 3 |  | 5 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 231 | `rv64:32:zicsr:csrrs:csr=0x342:write=0` | `zicsr` | 4 | 2 |  | 7 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 232 | `rv64:32:zicsr:csrrs:csr=0x343:write=0` | `zicsr` | 4 | 2 |  | 5 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 233 | `rv64:32:zicsr:csrrs:csr=0x34a:write=0` | `zicsr` | 4 | 2 |  | 6 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 234 | `rv64:32:zicsr:csrrs:csr=0x34b:write=0` | `zicsr` | 4 | 2 |  | 8 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 235 | `rv64:32:zicsr:csrrs:csr=0x3a0:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 236 | `rv64:32:zicsr:csrrs:csr=0x3b0:write=0` | `zicsr` | 4 | 2 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 237 | `rv64:32:zicsr:csrrs:csr=0x3b1:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 238 | `rv64:32:zicsr:csrrs:csr=0x3b2:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 239 | `rv64:32:zicsr:csrrs:csr=0x3b3:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 240 | `rv64:32:zicsr:csrrs:csr=0x3b4:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 241 | `rv64:32:zicsr:csrrs:csr=0x3b5:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 242 | `rv64:32:zicsr:csrrs:csr=0x3b6:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 243 | `rv64:32:zicsr:csrrs:csr=0x3b7:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 244 | `rv64:32:zicsr:csrrs:csr=0x3b8:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 245 | `rv64:32:zicsr:csrrs:csr=0x3b9:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 246 | `rv64:32:zicsr:csrrs:csr=0x3ba:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 247 | `rv64:32:zicsr:csrrs:csr=0x3bb:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 248 | `rv64:32:zicsr:csrrs:csr=0x3bc:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 249 | `rv64:32:zicsr:csrrs:csr=0x3bd:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 250 | `rv64:32:zicsr:csrrs:csr=0x3be:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 251 | `rv64:32:zicsr:csrrs:csr=0x3bf:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 252 | `rv64:32:zicsr:csrrs:csr=0x3c0:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 253 | `rv64:32:zicsr:csrrs:csr=0x600:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 254 | `rv64:32:zicsr:csrrs:csr=0x7a0:write=0` | `zicsr` | 4 | 2 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 255 | `rv64:32:zicsr:csrrs:csr=0x7a4:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 256 | `rv64:32:zicsr:csrrs:csr=0xb03:write=0` | `zicsr` | 4 | 2 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 257 | `rv64:32:zicsr:csrrs:csr=0xb04:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 258 | `rv64:32:zicsr:csrrs:csr=0xb05:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 259 | `rv64:32:zicsr:csrrs:csr=0xb06:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 260 | `rv64:32:zicsr:csrrs:csr=0xb07:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 261 | `rv64:32:zicsr:csrrs:csr=0xb08:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 262 | `rv64:32:zicsr:csrrs:csr=0xb09:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 263 | `rv64:32:zicsr:csrrs:csr=0xb0a:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 264 | `rv64:32:zicsr:csrrs:csr=0xb0b:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 265 | `rv64:32:zicsr:csrrs:csr=0xb0c:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 266 | `rv64:32:zicsr:csrrs:csr=0xb0d:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 267 | `rv64:32:zicsr:csrrs:csr=0xb0e:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 268 | `rv64:32:zicsr:csrrs:csr=0xb0f:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 269 | `rv64:32:zicsr:csrrs:csr=0xb10:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 270 | `rv64:32:zicsr:csrrs:csr=0xb11:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 271 | `rv64:32:zicsr:csrrs:csr=0xb12:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 272 | `rv64:32:zicsr:csrrs:csr=0xb13:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 273 | `rv64:32:zicsr:csrrs:csr=0xb14:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 274 | `rv64:32:zicsr:csrrs:csr=0xb15:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 275 | `rv64:32:zicsr:csrrs:csr=0xb16:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 276 | `rv64:32:zicsr:csrrs:csr=0xb17:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 277 | `rv64:32:zicsr:csrrs:csr=0xb18:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 278 | `rv64:32:zicsr:csrrs:csr=0xb19:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 279 | `rv64:32:zicsr:csrrs:csr=0xb1a:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 280 | `rv64:32:zicsr:csrrs:csr=0xb1b:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 281 | `rv64:32:zicsr:csrrs:csr=0xb1c:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 282 | `rv64:32:zicsr:csrrs:csr=0xb1d:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 283 | `rv64:32:zicsr:csrrs:csr=0xb1e:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 284 | `rv64:32:zicsr:csrrs:csr=0xb1f:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 285 | `rv64:32:zicsr:csrrs:csr=0xc01:write=0` | `zicsr` | 4 | 5 |  | 114 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 286 | `rv64:32:zicsr:csrrs:csr=0xda0:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 287 | `rv64:32:zicsr:csrrs:csr=0xf11:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 288 | `rv64:32:zicsr:csrrs:csr=0xf14:write=0` | `zicsr` | 4 | 10 |  | 47 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 289 | `rv64:32:zicsr:csrrs:csr=0xfb0:write=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 290 | `rv64:32:zicsr:csrrsi:csr=0x100:write=1:zimm=0x02` | `zicsr` | 4 | 1 |  | 7 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 291 | `rv64:32:zicsr:csrrsi:csr=0x304:write=1:zimm=0x08` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 292 | `rv64:32:zicsr:csrrsi:csr=0x344:write=1:zimm=0x02` | `zicsr` | 4 | 1 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 293 | `rv64:32:zicsr:csrrw:csr=0x003:read=0` | `zicsr` | 4 | 1 |  | 4 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 294 | `rv64:32:zicsr:csrrw:csr=0x100:read=0` | `zicsr` | 4 | 1 |  | 10 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 295 | `rv64:32:zicsr:csrrw:csr=0x105:read=0` | `zicsr` | 4 | 3 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 296 | `rv64:32:zicsr:csrrw:csr=0x140:read=0` | `zicsr` | 4 | 3 |  | 26 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 297 | `rv64:32:zicsr:csrrw:csr=0x140:read=1` | `zicsr` | 4 | 3 |  | 13 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 298 | `rv64:32:zicsr:csrrw:csr=0x141:read=0` | `zicsr` | 4 | 2 |  | 10 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 299 | `rv64:32:zicsr:csrrw:csr=0x142:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 300 | `rv64:32:zicsr:csrrw:csr=0x143:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 301 | `rv64:32:zicsr:csrrw:csr=0x14d:read=0` | `zicsr` | 4 | 2 |  | 9 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 302 | `rv64:32:zicsr:csrrw:csr=0x180:read=0` | `zicsr` | 4 | 4 |  | 12 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 303 | `rv64:32:zicsr:csrrw:csr=0x300:read=0` | `zicsr` | 4 | 4 |  | 9 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 304 | `rv64:32:zicsr:csrrw:csr=0x302:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 305 | `rv64:32:zicsr:csrrw:csr=0x303:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 306 | `rv64:32:zicsr:csrrw:csr=0x304:read=0` | `zicsr` | 4 | 2 |  | 5 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 307 | `rv64:32:zicsr:csrrw:csr=0x305:read=0` | `zicsr` | 4 | 8 |  | 103 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 308 | `rv64:32:zicsr:csrrw:csr=0x305:read=1` | `zicsr` | 4 | 6 |  | 98 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 309 | `rv64:32:zicsr:csrrw:csr=0x306:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 310 | `rv64:32:zicsr:csrrw:csr=0x30a:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 311 | `rv64:32:zicsr:csrrw:csr=0x320:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 312 | `rv64:32:zicsr:csrrw:csr=0x323:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 313 | `rv64:32:zicsr:csrrw:csr=0x324:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 314 | `rv64:32:zicsr:csrrw:csr=0x325:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 315 | `rv64:32:zicsr:csrrw:csr=0x326:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 316 | `rv64:32:zicsr:csrrw:csr=0x327:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 317 | `rv64:32:zicsr:csrrw:csr=0x328:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 318 | `rv64:32:zicsr:csrrw:csr=0x329:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 319 | `rv64:32:zicsr:csrrw:csr=0x32a:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 320 | `rv64:32:zicsr:csrrw:csr=0x32b:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 321 | `rv64:32:zicsr:csrrw:csr=0x32c:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 322 | `rv64:32:zicsr:csrrw:csr=0x32d:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 323 | `rv64:32:zicsr:csrrw:csr=0x32e:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 324 | `rv64:32:zicsr:csrrw:csr=0x32f:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 325 | `rv64:32:zicsr:csrrw:csr=0x330:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 326 | `rv64:32:zicsr:csrrw:csr=0x331:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 327 | `rv64:32:zicsr:csrrw:csr=0x332:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 328 | `rv64:32:zicsr:csrrw:csr=0x340:read=0` | `zicsr` | 4 | 1 |  | 4 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 329 | `rv64:32:zicsr:csrrw:csr=0x340:read=1` | `zicsr` | 4 | 1 |  | 6 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 330 | `rv64:32:zicsr:csrrw:csr=0x341:read=0` | `zicsr` | 4 | 4 |  | 8 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 331 | `rv64:32:zicsr:csrrw:csr=0x3a0:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 332 | `rv64:32:zicsr:csrrw:csr=0x3b0:read=0` | `zicsr` | 4 | 2 |  | 3 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 333 | `rv64:32:zicsr:csrrw:csr=0x3b0:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 334 | `rv64:32:zicsr:csrrw:csr=0x3b1:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 335 | `rv64:32:zicsr:csrrw:csr=0x3b1:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 336 | `rv64:32:zicsr:csrrw:csr=0x3b2:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 337 | `rv64:32:zicsr:csrrw:csr=0x3b2:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 338 | `rv64:32:zicsr:csrrw:csr=0x3b3:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 339 | `rv64:32:zicsr:csrrw:csr=0x3b3:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 340 | `rv64:32:zicsr:csrrw:csr=0x3b4:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 341 | `rv64:32:zicsr:csrrw:csr=0x3b4:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 342 | `rv64:32:zicsr:csrrw:csr=0x3b5:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 343 | `rv64:32:zicsr:csrrw:csr=0x3b5:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 344 | `rv64:32:zicsr:csrrw:csr=0x3b6:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 345 | `rv64:32:zicsr:csrrw:csr=0x3b6:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 346 | `rv64:32:zicsr:csrrw:csr=0x3b7:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 347 | `rv64:32:zicsr:csrrw:csr=0x3b7:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 348 | `rv64:32:zicsr:csrrw:csr=0x3b8:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 349 | `rv64:32:zicsr:csrrw:csr=0x3b8:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 350 | `rv64:32:zicsr:csrrw:csr=0x3b9:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 351 | `rv64:32:zicsr:csrrw:csr=0x3b9:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 352 | `rv64:32:zicsr:csrrw:csr=0x3ba:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 353 | `rv64:32:zicsr:csrrw:csr=0x3ba:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 354 | `rv64:32:zicsr:csrrw:csr=0x3bb:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 355 | `rv64:32:zicsr:csrrw:csr=0x3bb:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 356 | `rv64:32:zicsr:csrrw:csr=0x3bc:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 357 | `rv64:32:zicsr:csrrw:csr=0x3bc:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 358 | `rv64:32:zicsr:csrrw:csr=0x3bd:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 359 | `rv64:32:zicsr:csrrw:csr=0x3bd:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 360 | `rv64:32:zicsr:csrrw:csr=0x3be:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 361 | `rv64:32:zicsr:csrrw:csr=0x3be:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 362 | `rv64:32:zicsr:csrrw:csr=0x3bf:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 363 | `rv64:32:zicsr:csrrw:csr=0x3bf:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 364 | `rv64:32:zicsr:csrrw:csr=0x600:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 365 | `rv64:32:zicsr:csrrw:csr=0x643:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 366 | `rv64:32:zicsr:csrrw:csr=0x64a:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 367 | `rv64:32:zicsr:csrrw:csr=0x7a0:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 368 | `rv64:32:zicsr:csrrw:csr=0xb03:read=0` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 369 | `rv64:32:zicsr:csrrw:csr=0xb03:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 370 | `rv64:32:zicsr:csrrw:csr=0xb04:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 371 | `rv64:32:zicsr:csrrw:csr=0xb04:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 372 | `rv64:32:zicsr:csrrw:csr=0xb05:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 373 | `rv64:32:zicsr:csrrw:csr=0xb05:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 374 | `rv64:32:zicsr:csrrw:csr=0xb06:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 375 | `rv64:32:zicsr:csrrw:csr=0xb06:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 376 | `rv64:32:zicsr:csrrw:csr=0xb07:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 377 | `rv64:32:zicsr:csrrw:csr=0xb07:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 378 | `rv64:32:zicsr:csrrw:csr=0xb08:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 379 | `rv64:32:zicsr:csrrw:csr=0xb08:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 380 | `rv64:32:zicsr:csrrw:csr=0xb09:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 381 | `rv64:32:zicsr:csrrw:csr=0xb09:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 382 | `rv64:32:zicsr:csrrw:csr=0xb0a:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 383 | `rv64:32:zicsr:csrrw:csr=0xb0a:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 384 | `rv64:32:zicsr:csrrw:csr=0xb0b:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 385 | `rv64:32:zicsr:csrrw:csr=0xb0b:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 386 | `rv64:32:zicsr:csrrw:csr=0xb0c:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 387 | `rv64:32:zicsr:csrrw:csr=0xb0c:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 388 | `rv64:32:zicsr:csrrw:csr=0xb0d:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 389 | `rv64:32:zicsr:csrrw:csr=0xb0d:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 390 | `rv64:32:zicsr:csrrw:csr=0xb0e:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 391 | `rv64:32:zicsr:csrrw:csr=0xb0e:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 392 | `rv64:32:zicsr:csrrw:csr=0xb0f:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 393 | `rv64:32:zicsr:csrrw:csr=0xb0f:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 394 | `rv64:32:zicsr:csrrw:csr=0xb10:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 395 | `rv64:32:zicsr:csrrw:csr=0xb10:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 396 | `rv64:32:zicsr:csrrw:csr=0xb11:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 397 | `rv64:32:zicsr:csrrw:csr=0xb11:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 398 | `rv64:32:zicsr:csrrw:csr=0xb12:read=0` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 399 | `rv64:32:zicsr:csrrw:csr=0xb12:read=1` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 400 | `rv64:32:zicsr:csrrwi:csr=0x003:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 401 | `rv64:32:zicsr:csrrwi:csr=0x104:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 402 | `rv64:32:zicsr:csrrwi:csr=0x106:read=0:zimm=0x07` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 403 | `rv64:32:zicsr:csrrwi:csr=0x140:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 404 | `rv64:32:zicsr:csrrwi:csr=0x180:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 2 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 405 | `rv64:32:zicsr:csrrwi:csr=0x304:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 406 | `rv64:32:zicsr:csrrwi:csr=0x340:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 407 | `rv64:32:zicsr:csrrwi:csr=0x344:read=0:zimm=0x00` | `zicsr` | 4 | 1 |  | 1 |  |  | restricted-context | `csr-is-not-safe-or-identifiable-in-user-mode` |
| 408 | `rv64:32:zifencei:fence.i` | `zifencei` | 4 | 1 |  | 8 |  | 85.314396 | low-confidence | `measured-but-confidence-gates-failed` |
| 409 | `rv64:32:zihintpause:pause` | `zihintpause` | 4 | 1 |  | 311 |  | 84.984405 | low-confidence | `measured-but-confidence-gates-failed` |

## 附录 C：质量失败代码说明

| failure code | 数量 | 含义 |
| --- | ---: | --- |
| `plugin-off-check-divergent` | 169 | plugin-on 与 plugin-off 的全族等价区间超出允许范围。 |
| `batch-size-nonlinearity` | 157 | 三档 batch 的每指令成本未落入实用等价区间。 |
| `drift-effect-not-equivalent` | 98 | run 内时间漂移的同时区间不能证明足够小。 |
| `cross-clock-check-divergent` | 98 | 客体时钟与主 vCPU thread CPU-time 的等价区间超限。 |
| `order-effect-not-equivalent` | 82 | AB/BA 顺序效应的同时区间不能证明足够小。 |
| `too-many-severe-outliers` | 32 | 严重 Huber 异常比例的 Wilson 上界超过 10%。 |
| `cross-run-heterogeneity-high` | 22 | run 间异质性和 prediction interval 超过实践阈值。 |
| `simultaneous-ci-too-wide` | 16 | 主权重全族同时区间相对半宽超过 15% 或跨过零。 |
| `cross-clock-check-unavailable` | 15 | 辅助时钟缺少完整 run 覆盖或有效区间。 |
| `plugin-off-check-unavailable` | 15 | plugin-off 缺少完整 run 覆盖或有效区间。 |
| `per-run-irls-not-converged` | 10 | 至少一个独立 run 的稳健 IRLS 未在迭代上限内收敛。 |

## 附录 D：最终判断

1. **测量链路有效**：marker、raw encoding、PC 范围、pair 差分和 timer 均有闭合证据。
2. **主权重有可量化的不确定性**：999 次 run-cluster moving-block bootstrap 产生整族同时区间，且 replicate 全部有效。
3. **严格发布是保守的**：只有 3 个 catalog 类通过所有门禁；183 个类保留探索估计但不冒充严格权重。
4. **受限类不伪造权重**：CSR/priv/trap/CBO 需要专用上下文，当前 mapper 明确留空。
5. **结论只针对 QEMU TCG 条件**：代表值可用于当前 QEMU/BuildStorm 指令成本建模，不能直接解释为硬件 cycle。
