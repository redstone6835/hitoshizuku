# Loongson LS2X APB-DMA 驱动

`platform.loongson-apbdma` 为 LS2X APB-DMA 的 selector 和五个独立 channel 节点提供 DT
DMA resource，供 SDIO 等 platform consumer 使用。

## 实现范围

- 匹配 `loongson,ls-apbdma` 以及 `loongson,ls-apbdma-0` 到 `-4`。
- selector 要求 `#config-nr = <2>`，specifier 为 `<bit value>`；同一位只能被一个 lease
  占用，Disable/Drop 恢复申请前的寄存器值。
- channel 要求 `#dma-cells = <1>`、`dma-channels = <1>`、有效 `dma-requests` 和
  `apbdma-sel`；specifier 的 request 只用于范围校验。
- 每个 channel lease 分配一个 32 字节对齐的单传输描述符，Configure 接收方向、64 位
  内存地址、32 位 APB 地址和字节数，Enable/Disable 启停 order 寄存器。

当前只是互斥单通道 provider：不提供通道调度、scatter-gather、完成 IRQ/callback、循环
DMA、残余计数或错误状态解释。传输必须 4 字节对齐，consumer 需自行判断完成和超时。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-loongson-apbdma` |
| ELM 名称 | `platform.loongson-apbdma` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LOONGSON_APBDMA` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、DT provider；channel 依赖 selector provider |

selector lease、channel provider handle 和 descriptor DMA buffer 均有明确 owner。channel
Drop 会先写 STOP 再释放 claimed 状态；provider 尚有 lease 时不会被安全卸载。

## 验证

```sh
cargo check -p platform-loongson-apbdma --lib --target loongarch64-unknown-none
```

该 SoC 专属 provider 默认保持 `m`。
