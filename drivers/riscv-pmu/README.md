# RISC-V 固件 PMU 驱动

`platform.riscv-pmu` 解析固件提供的 event-to-counter 约束并注册通用 PMU descriptor；真实
counter 操作由架构层 SBI PMU backend 完成。

## 实现范围

- 匹配 `riscv,pmu`。
- 可选解析 `riscv,event-to-mhpmcounters` 三 cell matrix，将 event range 与逻辑 counter
  bitmap 转成 `PmuEventCounterRange`。
- 拒绝空/非三 cell 编码、重叠/非法范围，以及 binding 不允许出现在该属性中的 raw event。
- 属性缺失时仍注册没有额外 event range 约束的 descriptor。

本 crate 不直接读写 CSR，也不实现 SBI 调用、采样 IRQ、overflow handler、perf event 调度或
per-process accounting；没有已安装 PMU backend 时，注册后的 descriptor 不能独立驱动硬件。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-riscv-pmu` |
| ELM 名称 | `platform.riscv-pmu` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_RISCV_PMU` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus；架构层 SBI PMU backend 提供执行面 |

PMU registry handle 由 PnP owned resource 持有；probe 的 handle 归属失败会立即注销，remove
由统一资源释放路径撤销 descriptor。

## 验证

```sh
cargo check -p platform-riscv-pmu --lib --target riscv64gc-unknown-none-elf
```

该架构描述驱动默认保持 `m`。
