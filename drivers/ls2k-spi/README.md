# Loongson LS2K SPI 与 SPI-NOR 驱动

`platform.ls2k-spi` 在同一模块中驱动 LS2X SPI master，并把其 `jedec,spi-nor` 子设备
注册为通用可读、可写、可擦 flash。

## 实现范围

- controller 匹配 `loongson,ls-spi`，要求至少 0x10 字节 MMIO 和首个 clock；按 firmware
  path 放入模块内 master registry。
- flash 匹配 `jedec,spi-nor`，等待父 controller 后读取三字节 JEDEC ID，并从 capacity code
  推导容量。
- master 提供 mode 0 的 8-bit 轮询传输和固定 chip-select 0；默认目标频率 30 MHz。
- NOR 支持 0x03 read、0x02 256-byte page program、0x20 4 KiB erase、WREN 和 WIP 轮询，
  以 Flash V2 接口发布。

这是针对板载 W25Q 类 3-byte-address NOR 的实现，不解析 SFDP，不支持 4-byte address、quad/
dual、其它 CS、SPI mode、保护位或 suspend/resume。controller remove 当前不会从静态 registry
删除 master，clock lease也未长期持有/显式关闭，因此热移除和重复装载仍不成熟。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2k-spi` |
| ELM 名称 | `platform.ls2k-spi` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2K_SPI` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus、Loongson clock；flash 子设备依赖父 SPI controller |

flash binding 持有 registry 返回的 master 与 Flash handle，remove 会注销 flash handle；controller
binding 仅保留 master。写擦操作必须在可恢复器件上单独验证。

## 验证

```sh
cargo check -p platform-ls2k-spi --lib --target loongarch64-unknown-none
```

该实验性板级驱动默认保持 `m`。
