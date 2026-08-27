# Loongson LS2K SDIO 驱动

`platform.loongson-sdio` 初始化 LS2K SDIO controller，识别 SD 或 eMMC，并发布 512 字节
扇区的 `/dev/mmcblkN` 块设备。

## 实现范围

- 匹配 `loongson,ls2k_sdio`，要求一个至少 0x808 字节的 MMIO 窗口和首个 clock lease。
- 支持 SD CMD0/8/55/ACMD41 与 eMMC CMD0/1 初始化、CSD/EXT_CSD 容量、SD 4-bit 尝试，
  传输频率上限 25 MHz。
- 支持单块和最多 128 块的 CMD17/18/24/25，multi-block 后发送 CMD12；flush 轮询卡 ready。
- 优先取得名为 `sdio_rw` 的 APB-DMA resource；缺失/禁用时用同一 64 KiB staging buffer
  轮询 PIO。所有 BIO 由一个 mutex 同步执行。

当前没有 controller IRQ、卡检测/热插拔、电压切换、UHS/HS200/HS400、调谐、discard 或
write-zeroes。DMA/PIO 都依赖轮询，且 APB FIFO 地址限制为 32 位；需用可恢复介质验证写路径。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-loongson-sdio` |
| ELM 名称 | `platform.loongson-sdio` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LOONGSON_SDIO` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、Loongson clock；可选 Loongson APB-DMA |

clock/DMA lease、staging buffer、host 和块 function 都随绑定持有。remove 先标记 function gone，
停止 DMA/command path、复位控制器并关闭 clock，然后 owned resource 释放 provider lease。

## 验证

```sh
cargo check -p platform-loongson-sdio --lib --target loongarch64-unknown-none
```

该板级存储驱动默认保持 `m`。
