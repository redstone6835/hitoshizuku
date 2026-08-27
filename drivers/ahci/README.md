# Platform AHCI 驱动

`platform.ahci` 为 LS2K1000 设备树中的 platform AHCI 控制器提供 SATA 块设备，按活动
端口发布 `/dev/sdN`。

## 实现范围

- 匹配 `snps,spear-ahci`，要求一个覆盖 HBA 与已实现端口的 MMIO 窗口和一条可解析 IRQ。
- 执行 BIOS ownership handoff、HBA reset、COMRESET、ATA IDENTIFY，并只接管 SATA disk
  signature 的端口。
- 每端口使用 command slot 0、32 位 DMA command/FIS/table 地址和 1 MiB staging buffer；
  支持 LBA48 READ/WRITE DMA EXT，以及设备声明支持时的 FLUSH CACHE EXT。
- BIO 以 IRQ 或 `drain` 轮询完成；每端口同时只允许一个请求，discard、write-zeroes 和
  FUA 不受支持。

当前实现面向 LS2K1000 的窄平台路径，不是覆盖 ATAPI、port multiplier、NCQ、热插拔或
64 位 AHCI DMA 的通用驱动。端口无法停止时会保留 DMA 对象以避免硬件 use-after-free；
还需要真实 SATA 盘、超时恢复和卸载压力测试。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ahci` |
| ELM 名称 | `platform.ahci` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_PLATFORM_AHCI` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、Loongson IRQ；板级时钟先于本模块就绪 |

probe 持有 IRQ、每端口 DMA buffer、块设备和 controller binding。remove 先标记块设备 gone，
关闭全局/端口中断并停止 command/FIS engine；factory 只有在所有绑定均可移除时才可注销。

## 验证

```sh
cargo check -p platform-ahci --lib --target loongarch64-unknown-none
```

模块选择和顺序由 [`../Modules.toml`](../Modules.toml) 管理；该板级存储驱动默认保持 `m`。
