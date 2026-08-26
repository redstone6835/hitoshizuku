# 内核驱动

`drivers/` 保存 Hitoshizuku OS 自有的硬件驱动、设备服务和网络执行模块。它们与
`general` 中的设备核心、`hal` 中的架构无关接口以及 `libs/*` 中的共享协议类型一起
演进，因此留在内核 Cargo workspace 中，不通过 Git submodule 引入。

每个可部署驱动都是普通 Cargo package，同时具有一份 `Elm.toml` 模块描述。Cargo
负责依赖解析和 Rust 编译；`cargo-elm` 负责 ELM ABI、接口 Profile、模块镜像和装载顺序；
根目录 `xtask` 把两者编排成完整内核构建。设备对象、PnP 和 `DeviceFunction` 的公共约束
见 [`DEVICE_ABSTRACTION.md`](../DEVICE_ABSTRACTION.md)。

## 分层

```text
固件描述 / PCI 枚举
        |
        v
platform bus + PnP core          general/src/dev
        |
        +-- 平台驱动             firmware-bus、PLIC、UART、RTC、syscon ...
        +-- VirtIO provider      virtio + virtio/{api,provider-api,consumer-api}
        +-- VirtIO consumer      virtio-blk、virtio-net
        +-- 网络设备执行面       loopback、virtio-net
        +-- 网络协议执行面       net-stack
        `-- 通用 ELM 服务        language-runtime
```

驱动只拥有自己申请的 MMIO、IRQ、DMA、队列和 function。探测成功后，硬件能力以
`DeviceFunction` 或专用 registrar 投影给常驻内核；卸载时先停止新工作、撤销公开入口，
再按资源所有权回收底层对象。网络设备与网络协议栈有意分离：网卡驱动处理队列和 buffer
ownership，`net-stack` 处理 flow shard 的协议状态。

## 配置与构建

[`Modules.toml`](Modules.toml) 是驱动集合的声明源，记录 `CONFIG_*` 名称、默认模式、
目标限制和依赖顺序。根目录 `.config` 只选择部署方式，不改写 crate 源码：

- `y`：以 `elm-integrated` 方式集成到内核镜像；
- `m`：构建为受 ELM 管理的独立 EKI；
- `n`：不参与本次构建。

默认策略按职责而不是目录名划分。基础、通用且启动后持续使用的
`platform.firmware-bus`、`platform.uart16550`、`kernel.random`、`net.stack`、
`net.loopback`、`language.runtime` 使用 `y`；板级或架构特定的中断控制器、RTC，以及可选的
syscon、fw_cfg、CFI Flash 和 VirtIO 设备链使用 `m`。新增的 Loongson/LS2K、
RISC-V IOMMU、RISC-V PMU、通用 DT provider、PCI host 和 platform AHCI 驱动也统一默认
为 `m`；具体板型只有在启动关键路径确实要求内置时才覆盖为 `y`。`m` 模块仍按 `after` 与
`depends` 顺序装载，不代表它们是不受支持的次级实现。

从仓库根目录生成默认配置、调整选项并构建当前模块集合：

```sh
cargo xtask defconfig
cargo xtask config
cargo xtask modules --target loongarch64-unknown-none
cargo xtask modules --target riscv64gc-unknown-none-elf
```

`cargo xtask modules` 会先构建对应架构的内核并导出内核 API Profile，再按依赖顺序调用
`cargo-elm`。不要手工维护驱动目录中的 `.elm/`、`dist/` 或 `Elm.lock`；它们是可删除并
重新生成的本地产物，也不能作为 workspace member 或源码依赖路径提交。

只检查某个 crate 的 Rust 源码时，可以直接使用 Cargo：

```sh
cargo check -p platform-uart16550 --lib --target loongarch64-unknown-none
cargo check -p net-stack --lib --target riscv64gc-unknown-none-elf
cargo check -p virtio-block --lib --target riscv64gc-unknown-none-elf
```

直接 `cargo check` 不会生成 EKI，也不会验证完整模块装载图；发布前仍需执行相应目标的
`cargo xtask modules`。项目使用调用者的默认 Rust 工具链，不在驱动目录固定 toolchain。

## Crate 索引

| Crate | 模块名 | 目标与职责 |
| --- | --- | --- |
| [`firmware-bus`](firmware-bus/) | `platform.firmware-bus` | 从固件描述建立 platform 设备与资源 |
| [`loongson-irq`](loongson-irq/) | `platform.loongson-irq` | LoongArch64 Loongson 中断控制器 |
| [`plic`](plic/) | `platform.plic` | RISC-V PLIC 中断域 |
| [`riscv-iommu`](riscv-iommu/) | `platform.riscv-iommu` | RISC-V IOMMU 1.0 platform/PCI 控制器 |
| [`riscv-pmu`](riscv-pmu/) | `platform.riscv-pmu` | RISC-V 固件 PMU event/counter 约束 |
| [`dt-providers`](dt-providers/) | `platform.dt-providers` | 标准 DT fixed-clock 与 fixed-factor-clock provider |
| [`loongson-clk`](loongson-clk/) | `platform.loongson-clk` | LS2X 时钟树速率 provider |
| [`loongson-pinctrl-gpio`](loongson-pinctrl-gpio/) | `platform.loongson-pinctrl-gpio` | LS2K1000 pinctrl 与 Loongson GPIO provider |
| [`loongson-apbdma`](loongson-apbdma/) | `platform.loongson-apbdma` | LS2X APB-DMA selector 与单通道 provider |
| [`ahci`](ahci/) | `platform.ahci` | LS2K1000 platform AHCI/SATA 块设备 |
| [`loongson-sdio`](loongson-sdio/) | `platform.loongson-sdio` | LS2K SD/eMMC 块设备 |
| [`pci-host-ecam`](pci-host-ecam/) | `platform.pci-host-ecam` | 通用 CAM/ECAM 与 LS2K1000 PCI host |
| [`syscon`](syscon/) | `platform.syscon` | 固件 syscon、电源和复位操作 |
| [`ls2k-gmac`](ls2k-gmac/) | `platform.ls2k-gmac` | LS2K1000 DWMAC 以太网设备 |
| [`ls2k-i2c`](ls2k-i2c/) | `platform.ls2k-i2c` | LS2K I2C 总线 function |
| [`ls2k-spi`](ls2k-spi/) | `platform.ls2k-spi` | LS2K SPI master 与 SPI-NOR flash |
| [`ls2k-tsensor`](ls2k-tsensor/) | `platform.ls2k-tsensor` | LS2K 温度读取与阈值 IRQ |
| [`ls2k-usb`](ls2k-usb/) | `platform.ls2k-usb` | LS2K1000 DWC2/EHCI/OHCI USB host |
| [`ls2x-wdt`](ls2x-wdt/) | `platform.ls2x-wdt` | LS2X 看门狗 function |
| [`ls2k-rtc`](ls2k-rtc/) | `platform.ls2k-rtc` | LS2K RTC、alarm 与 realtime source |
| [`ls7a-rtc`](ls7a-rtc/) | `platform.ls7a-rtc` | LoongArch64 LS7A RTC |
| [`goldfish-rtc`](goldfish-rtc/) | `platform.goldfish-rtc` | RISC-V/QEMU Goldfish RTC |
| [`fw-cfg`](fw-cfg/) | `platform.fw-cfg` | QEMU fw_cfg 数据通道 |
| [`cfi-flash`](cfi-flash/) | `platform.cfi-flash` | CFI NOR flash 设备 |
| [`uart16550`](uart16550/) | `platform.uart16550` | NS16550A 兼容串口 |
| [`random`](random/) | `kernel.random` | 内核随机服务与熵输入 |
| [`net-stack`](net-stack/) | `net.stack` | 分片、单写者的网络协议执行面 |
| [`loopback`](loopback/) | `net.loopback` | 本地批量回环网络设备 |
| [`language-runtime`](language-runtime/) | `language.runtime` | 语言无关的 backend、instance 与有界请求调度基础服务 |
| [`virtio`](virtio/) | `virtio.framework` | VirtIO provider 与版本化公共契约 |
| [`virtio-blk`](virtio-blk/) | `virtio.block` | VirtIO MMIO/PCI 块设备 consumer |
| [`virtio-net`](virtio-net/) | `net.virtio` | VirtIO MMIO/PCI 网络设备 consumer |

VirtIO framework 还包含三个不单独部署的契约 crate：

- [`virtio/api`](virtio/api/)：provider 与 consumer 共用的协议类型和 split virtqueue；
- [`virtio/provider-api`](virtio/provider-api/)：framework 侧导出契约；
- [`virtio/consumer-api`](virtio/consumer-api/)：consumer 侧导入契约。

`language-runtime` 不是硬件驱动，但与 ELM 生命周期和内核构建紧耦合，因此作为可部署服务
留在 `drivers/`。它的固定 wire 类型位于 [`libs/elm-language-abi`](../libs/elm-language-abi/)，
完整边界见 [`LANGUAGE_RUNTIME.md`](../LANGUAGE_RUNTIME.md)。具体语言 backend 和 SDK 不应
继续堆入本目录，而应由外部仓库按版本引入。

## 可部署 crate 的目录约定

```text
<driver>/
├── Cargo.toml       Cargo package、feature 和源码依赖
├── Elm.toml         模块身份、模式、Profile、provider/consumer 契约
├── elm.ld           独立 ELM 的链接布局（需要时）
├── README.md        职责、资源、生命周期和验证入口
└── src/             唯一实现来源
```

许多驱动让 `src/main.rs` 同时作为 ELM 模块入口和 workspace library target，以便
`m` 模式和 `y` 模式复用同一份实现。新增或修改驱动时，应同步检查：

1. `Modules.toml` 中的依赖、`after` 顺序和 `targets` 是否真实反映探测前置条件；
2. `Elm.toml` 的模块名、契约版本和 `integrated_phase` 是否与源码一致；
3. `probe` 的失败路径是否回滚已申请资源，`remove/finalize` 是否先停止外部入口；
4. README 是否说明目标架构、可见 function、非职责边界和验证命令；
5. 两个受支持目标中相关的 `cargo check` 与 `cargo xtask modules` 是否通过。
