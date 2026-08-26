# Loongson LS2X 时钟驱动

`platform.loongson-clk` 读取 LS2K1000 PLL 与分频寄存器，并以 DT clock provider 返回
LS2X clock ID 对应的当前速率。

## 实现范围

- 匹配 `loongson,ls2x-clk`，要求 `#clock-cells = <1>`、一个 phandle、一个外部父 clock
  和一个寄存器窗口。
- 支持 ID 0..13 的 REF、NODE、CPU、DDR、GPU、HDA、DC、PIX0/1、GMAC、SATA、USB、
  APB、SPI 速率；Enable/Disable 转发给父 clock。
- 接受 LS2K1000 固件把窗口长度错误写为 1 的已知布局，否则要求完整 0x58 字节窗口。
- ID 14 `I2S_MCLK` 明确不支持；本驱动只读速率，不写 PLL、门控或分频寄存器。

速率公式依赖当前 LS2K1000 寄存器布局和可信固件；零乘数/除数、零父频率或未知 ID 会
失败。它不是可编程时钟树，也没有 rate-change 通知或 consumer 引用计数。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-loongson-clk` |
| ELM 名称 | `platform.loongson-clk` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LOONGSON_CLK` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、`platform.dt-providers` 提供参考时钟 |

父 clock lease 与本 provider handle 都归 PnP 设备所有；释放顺序先撤销本 provider，再
释放父 lease。MMIO 映射由 platform 设备生命周期保证。

## 验证

```sh
cargo check -p platform-loongson-clk --lib --target loongarch64-unknown-none
```

该板级 clock provider 默认保持 `m`。
