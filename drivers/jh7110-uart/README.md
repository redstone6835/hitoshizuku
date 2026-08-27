# StarFive JH7110 UART 驱动

`platform.jh7110-uart` 将 VisionFive 2 的 DesignWare APB UART 注册为字符
`DeviceFunction` 和控制台。它处理 JH7110 设备树布局，与只匹配 `ns16550*` 的通用
`platform.uart16550` 分工。

## 实现范围

- 以更高的板级优先级匹配 `starfive,jh7110-uart` 和 `snps,dw-apb-uart`。
- 解析 `reg-shift` 与 `reg-io-width`（1/2/4 字节），校验 LSR 所需的 MMIO 窗口。
- 通过 CRG provider 取得 `baudclk` 并按固件 baud 或 115200 初始化 8N1；没有 clock
  引用时接管固件已配置的 divisor，不猜测输入频率。
- 尝试应用 `pinctrl-0` 默认 state；provider 不可用时保留固件配置并记录告警。
- 提供字符读写、32 KiB TX 环、flush、poll 和 RX wait queue。PLIC IRQ 可用时注册接收
  handler 唤醒等待者；没有 IRQ resource 时仍可轮询。
- 以稳定分配的 `/dev/uartN` 名称注册 `CharFunction`，并标记为 TTY/console。

当前成熟度为板级功能实现、待新树实机回归。寄存器访问按 little-endian 16550 兼容布局，
不实现 DesignWare 特有的 component parameter 探测、TX IRQ/DMA、硬件流控、modem status、
奇偶校验或多种停止位。TX 满载和 flush 使用有上限的忙等；它适合作为控制台，不应视为
高吞吐串口框架。pinctrl 应用失败不会阻止绑定，这依赖固件已经留下可用引脚状态。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-jh7110-uart` |
| ELM 名称 | `platform.jh7110-uart` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_JH7110_UART` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus；推荐 JH7110 CRG、pinctrl 和 PLIC |

clock lease、pinctrl lease 和 IRQ handler 都交给 PnP owned resource 管理。function 注册
失败时会关闭 RX IRQ；remove 先关闭 UART 接收中断，再由 PnP core 回收其余资源。ELM
finalize 仅在 factory 可以安全注销时成功。

## 验证

```sh
cargo check -p platform-jh7110-uart --lib --target riscv64gc-unknown-none-elf
cargo elm check drivers/jh7110-uart --arch riscv64
```

实机验证应覆盖固件预配置和 CRG 编程两条路径、PLIC 中断输入、长日志 flush 与模块卸载。
该板级驱动默认保持 `m`；VisionFive 2 若依赖它作为首个内核控制台，可在板级配置中显式
选择 `y`。
