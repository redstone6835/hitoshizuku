# VirtIO 网络设备驱动

本目录实现 `net-virtio` Cargo 包和 `net.virtio` ELM。驱动负责 VirtIO 网卡的 transport、
feature 协商、RX/TX 队列、DMA buffer 与中断，把队列注册到内核网络设备层，并为 PnP
设备发布 `eth0` 网络 `DeviceFunction`。

TCP/IP 协议状态、socket 语义和流调度不在本 crate 中；这些由独立的
[`net-stack`](../net-stack/README.md) ELM 处理。驱动也不枚举 PCI/platform 总线，不管理
IP 地址或路由。

## 源码结构

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | ELM 生命周期、framework 校验、PnP factory 注册、quiesce 与 detach |
| `src/common.rs` | `VirtioNetQueue`、RX/TX descriptor 与 buffer 所有权、网络设备注册 |
| `src/mmio.rs` | VirtIO-MMIO v1/v2 单队列对、platform IRQ 与 PnP 绑定 |
| `src/pci.rs` | modern VirtIO-PCI、MSI-X/MSI/INTx，以及可回退的 MQ/RSS 初始化 |

每个 `VirtioNetQueue` 拥有一对 split virtqueue。RX buffer 发布给设备后保存在
`rx_pending`，TX packet、header lease 和 descriptor 数保存在 `tx_pending`，只有 used ring
完成后才归还所有权。DMA pool 使用设备自己的 `DmaContext`，并显式区分 ToDevice、
FromDevice 与共享 payload。支持 `EVENT_IDX` 和 TX checksum offload；MAC 与
`MRG_RXBUF` 是必需能力，MTU 按设备 feature 选择。当前注册时把链路视为 up，尚未消费
设备 config change 来动态更新链路状态。

MMIO 路径固定使用一个 RX/TX 队列对。PCI 路径在设备同时支持 control virtqueue、MQ、RSS
且 MSI-X 资源足够时，按活动 CPU 数与设备上限建立多队列和 RSS 表；任一步失败都会 reset
候选配置并回退到单队列。

## PnP、DeviceFunction 与卸载

MMIO 路径匹配 platform ID `virtio,mmio` 或 `LNRO0005`，再验证 magic、v1/v2 和 network
device ID 1。IRQ handler 由 platform 资源创建并通过 `PnpDevice::own_resource` 托管。

PCI 路径匹配 VirtIO vendor `0x1af4` 的 network IDs `0x1000`/`0x1041`，启用 MMIO decode
与 bus master，校验 modern capabilities。中断优先顺序为多队列 MSI-X、单向量 MSI-X、
MSI、INTx；MSI/MSI-X 配置和所有 IRQ handler 都登记为 PnP 资源，设备移除时按所有权链
撤销。

probe 完成后，驱动先向 `net::device` 注册队列和 buffer pool，再调用
`PnpDevice::register_function(net_function("eth0", ...))` 发布网络 `DeviceFunction`。
当前 `ACTIVE_DEVICE` 只有一个槽，因此同一时刻只支持一个活动 VirtIO 网卡，名称固定为
`eth0`；这是实现限制，不是 VirtIO 或设备抽象的要求。

受管 `m` 模式下，队列操作通过私有 `direct-pinned` 导出
`net.virtio.queue-call` 连接到网络设备层；集成 `y` 模式则注册进程内
`NetQueuePair` 对象。两种模式共享同一队列实现。卸载顺序为 quiesce 队列、
`net::device::begin_remove` 排空网络引用、注销 PnP factory，最后释放队列并 reset transport；
仍有活动引用时生命周期返回 busy，不释放模块代码。

## ELM 与模块配置

| 项目 | 值 |
| --- | --- |
| 配置键 | `CONFIG_VIRTIO_NET` |
| ELM 名称 | `net.virtio` |
| 类型/阶段 | `network` / `runtime` |
| framework 依赖 | `virtio.framework` |
| 契约 | `driver.virtio.framework@1` |
| API crate | `virtio-consumer`，在源码中命名为 `virtio` |
| 支持目标 | `riscv64gc-unknown-none-elf`、`loongarch64-unknown-none` |

`initialize` 只有在 `virtio::framework_ready()` 成功后才注册 MMIO/PCI factory。`m` 生成
受管 EKI，`y` 启用 `elm-integrated` 并生成静态归档，`n` 不构建；`CONFIG_VIRTIO` 必须启用
并与本模块使用兼容模式。网络协议栈和网卡是两个独立配置项，不应把
`CONFIG_NET_STACK` 当作本驱动的 Cargo path dependency。

## 验证

在仓库根目录执行：

```sh
cargo check -p net-virtio --lib --target riscv64gc-unknown-none-elf
cargo check -p net-virtio --lib --target loongarch64-unknown-none
cargo xtask modules --target riscv64gc-unknown-none-elf
```

Cargo 检查验证 Rust 类型和依赖；`xtask modules` 才会按 `.config` 检查 framework 契约、
模块顺序和所选 `y/m/n` 产物。部署配置使用 `cargo xtask config` 修改。
