# Loongson LS2K1000 GMAC 驱动

`platform.ls2k-gmac` 驱动 LS2K1000 的 DesignWare GMAC 3.70a 单通道 MAC/DMA，并把
每个实例注册为 `ethernetN` 网络设备。

## 实现范围

- 匹配 `snps,dwmac-3.70a` 和 `ls,ls-gmac`，要求至少 0x1100 字节 MMIO 与 macirq。
- 使用 64 项 RX/TX normal descriptor ring 和 32 位 DMA 地址；RX buffer 来自网络栈
  refill lease，TX fragment 线性化到每槽 1536 字节 buffer。
- 实现 Loongson MII 布局的 MDIO，并按固定 PHY 地址 0 初始化 YT8511/C22 自动协商；
  支持 10/100/1000 Mbps 与半/全双工结果。
- DMA IRQ 唤醒网络 runtime worker，处理收发完成和 RU/overflow/underflow 重踢。

这是针对 2K1000LA 工厂 DT 和单队列 RGMII 的板级实现。MAC 地址由 firmware path 与
`bus_id` 派生，不读取 nvmem/标准 `local-mac-address`；不支持多队列、jumbo、TSO/checksum
offload、PTP、EEE、PHY 地址扫描、phylink 或运行期链路变更通知。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2k-gmac` |
| ELM 名称 | `platform.ls2k-gmac` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2K_GMAC` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、Loongson IRQ；板级 clock/PHY 已可用 |

binding 持有 MAC、队列、IRQ handle 和 net device handle。remove 先 quiesce DMA 队列、清
waker、注销 IRQ，再对网络设备执行 `begin_remove`。

## 验证

```sh
cargo check -p platform-ls2k-gmac --lib --target loongarch64-unknown-none
```

该板级网卡驱动默认保持 `m`；静态检查不能替代实机收发和链路恢复测试。
