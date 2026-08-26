# Loongson LS2K1000 pinctrl/GPIO 驱动

`platform.loongson-pinctrl-gpio` 注册 LS2K1000 pin state 与 Loongson3 GPIO DT provider，
让 platform consumer 通过 phandle lease 配置复用和 GPIO line。

## 实现范围

- pinctrl 匹配 `loongson,2k1000-pinctrl`，解析 controller 直属 state 及其 `groups`、
  `function` 子节点，支持源码表中列出的 SATA、GMAC、UART、I2C、CAN、PWM、SDIO 等组合。
- GPIO 匹配 `loongson,loongson3-gpio`，要求 `gpio-controller`、`#gpio-cells = <2>`、
  `ngpios` 和厂商 offset 属性；仅接受 active-low flag。
- GPIO lease 独占一条 line，支持输入、输出、读写和 Assert/Deassert；`support_irq` 存在时
  只编程 line interrupt enable，并校验每 line 一个 IRQ source。
- pin state Disable 不恢复旧 mux；GPIO lease Drop 会关 line interrupt、切回输入并释放 line。

这是板级 mux 表和 GPIO provider，不是完整 pinconf/GPIO IRQ domain。它不支持 bias、drive
strength、debounce、edge/level type 设置或 GPIO handler 分发；未知 group/function 直接拒绝。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-loongson-pinctrl-gpio` |
| ELM 名称 | `platform.loongson-pinctrl-gpio` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LOONGSON_PINCTRL_GPIO` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus；GPIO IRQ 描述依赖 Loongson IRQ domain |

每个 pin state 和 GPIO controller 的 provider handle 都是 owned resource，注册失败会撤销
当前 handle；两个 factory 的第二个注册失败时也会回滚第一个。

## 验证

```sh
cargo check -p platform-loongson-pinctrl-gpio --lib --target loongarch64-unknown-none
```

该板级 provider 默认保持 `m`。
