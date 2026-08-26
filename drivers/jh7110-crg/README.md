# StarFive JH7110 CRG 驱动

`platform.jh7110-crg` 把 VisionFive 2 设备树中的 SYS、STG 和 AON clock/reset
controller 注册到通用 DT provider 层，供 UART、MMC 和 TRNG 等板级驱动按 phandle
取得时钟或复位资源。

## 实现范围

- 匹配 `starfive,jh7110-syscrg`、`starfive,jh7110-stgcrg` 和
  `starfive,jh7110-aoncrg`。
- 以 `#clock-cells = <1>` 的 clock ID 提供 `GetRate`、`Enable` 和 `Disable`，并为同一
  phandle 注册 reset provider。
- SYS/AON 的常用时钟速率来自 JH7110 时钟树和实机观测值；节点上的
  `clock-frequency` 可覆盖外部振荡器及 UART core clock，便于固件差异和测试环境。
- STG 域实际编程 TRNG 使用的 HCLK/AHB clock 15/16，并实际控制 security reset 3。
- provider handle 由 PnP 设备持有，解绑时随 owned resource 注销。

当前实现不是完整的通用 JH7110 时钟框架。静态表之外的 clock 不提供速率；除 STG
15/16 外的门控以及除 reset 3 外的复位暂时保持兼容 no-op。它不实现父时钟切换、PLL
重编程、动态分频、速率传播或 consumer 计数。因此成熟度为“板级可用、控制面有限”，
不能把未知 ID 的成功返回理解为已经修改硬件。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-jh7110-crg` |
| ELM 名称 | `platform.jh7110-crg` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_JH7110_CRG` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus 已枚举 DT platform 设备 |

ELM 初始化只注册一个 driver factory。probe 在 provider 对外可见前校验 phandle；STG
节点还会校验 MMIO 窗口至少覆盖 `0x78` 状态寄存器。第二个 provider 注册失败时会撤销
第一个，避免留下半绑定控制器。

## 源码与验证

`src/driver.rs` 实现 PnP 和 provider，`src/hardware.rs` 隔离 STG 寄存器操作；
`tests/hardware.rs` 覆盖门控、复位完成和超时路径。

```sh
cargo check -p platform-jh7110-crg --lib --target riscv64gc-unknown-none-elf
cargo test -p platform-jh7110-crg --test hardware
cargo elm check drivers/jh7110-crg --arch riscv64
```

模块选择和装载顺序由 [`../Modules.toml`](../Modules.toml) 管理。该板级驱动默认应保持
`m`；VisionFive 2 板级配置可以显式改为 `y`。
