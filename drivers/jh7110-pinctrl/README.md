# StarFive JH7110 pinctrl 驱动

`platform.jh7110-pinctrl` 将 VisionFive 2 设备树中的 pin state 转换成通用 Pinctrl DT
resource。UART 等 consumer 只需获取 `pinctrl-N` 引用并发送 `Configure`，无需依赖本
crate 的 Rust 类型。

## 实现范围

- 匹配 `starfive,jh7110-sys-pinctrl` 和 `starfive,jh7110-aon-pinctrl`。
- 枚举 controller 拥有的后代节点，将带 `pinmux` 的 pin group 合并到最近的 phandle
  配置节点，并为每个 state phandle 注册独立 provider。
- 解析 JH7110 pinmux 编码中的 pin、function、dout、doen 和 din 字段，按 SYS 域功能
  选择表编程寄存器。
- 支持 `bias-disable`、`bias-pull-up/down`、`input-enable`、
  `input-schmitt-enable`、`drive-strength` 和 `slew-rate` pad 配置。
- provider handle 作为 PnP owned resource 保存，解绑时自动撤销。

成熟度为实验性。SYS 域实现了 VisionFive 2 当前使用的 pin state 编程，但功能选择表不是
所有 pin 的完整硬件描述；AON 域目前只注册可解析的占位 provider，不写寄存器。本 crate
也没有向通用设备层发布 GPIO line/chip API，不提供运行时方向切换、GPIO IRQ、pin ownership
仲裁或配置恢复。未知/不支持的字段目前会被忽略，可信设备树是安全前提。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-jh7110-pinctrl` |
| ELM 名称 | `platform.jh7110-pinctrl` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_JH7110_PINCTRL` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus 已保留 controller 的 DT 后代节点 |

SYS controller 要求第一段 MMIO；AON 占位路径不映射 MMIO。probe 预留所有 provider
资源槽，controller provider 注册失败会回滚；单个 state provider 注册失败不会阻止其它
有效 state 对外提供服务。

## 验证

```sh
cargo check -p platform-jh7110-pinctrl --lib --target riscv64gc-unknown-none-elf
cargo elm check drivers/jh7110-pinctrl --arch riscv64
```

还应在实机上逐项确认 UART/MMC 等 consumer 的默认 state，以及重复 `Configure` 的幂等性。
该板级驱动默认应保持 `m`，VisionFive 2 配置可以按启动依赖显式内置。
