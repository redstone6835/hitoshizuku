# Loongson LS2K 温度传感器驱动

`platform.ls2k-tsensor` 读取 LS2K temperature sensor，发布毫摄氏度读取 function，并用
硬件阈值 IRQ 记录越界事件。

## 实现范围

- 匹配 `loongson,ls2k-tsensor`，要求覆盖 0x30 字节寄存器的 MMIO 和一条可解析 IRQ。
- probe 写入默认 60/95 摄氏度低/高阈值并开启传感器中断。
- `mygo.device.thermal@1` 只提供 `read_temp_milli:i32`；换算为 `(OUT & 0xff) - 100`。
- IRQ handler 清状态、更新最后温度与 crossing counter，并输出日志。

当前没有通用 thermal-zone/cooling-device 投影，不解析 DT trip points，也不支持多 sensor、
阈值运行时配置、校准、采样过滤或热管理策略。固定阈值和线性换算只适用于当前 LS2K binding。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2k-tsensor` |
| ELM 名称 | `platform.ls2k-tsensor` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2K_TSENSOR` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、Loongson IRQ domain |

binding 持有 sensor 与 IRQ handle。remove 先令 function 返回 gone，再注销 handler；factory
注销失败时 ELM finalize 保持 busy。

## 验证

```sh
cargo check -p platform-ls2k-tsensor --lib --target loongarch64-unknown-none
```

该板级传感器驱动默认保持 `m`。
