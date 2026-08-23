# VirtIO 框架

本目录同时包含 `virtio-framework` ELM 和供 VirtIO consumer 复用的三个接口 crate。
框架的稳定边界是 `driver.virtio.framework@1`：受管 consumer 在进入设备探测前，先通过
该契约确认自己绑定到 revision 1 的 framework provider。

框架本身不扫描总线、不认领设备，也不注册块设备或网卡。MMIO/PCI 匹配、feature
协商、队列创建、IRQ 所有权和 `DeviceFunction` 注册分别由
[`virtio-blk`](../virtio-blk/README.md) 与 [`virtio-net`](../virtio-net/README.md)
完成。

## 目录与依赖关系

| 路径 | Cargo 包 | 职责 |
| --- | --- | --- |
| `src/main.rs` | `virtio-framework` | ELM 生命周期；初始化时校验 provider revision |
| [`api/`](api/README.md) | `virtio` | PCI/MMIO 传输类型与 split virtqueue 实现 |
| [`provider-api/`](provider-api/README.md) | `virtio-provider` | 重导出公共 API，并导出 framework revision |
| [`consumer-api/`](consumer-api/README.md) | `virtio-consumer` | 重导出公共 API，并导入、校验 framework revision |

`virtio-framework` 依赖 `virtio-provider`；块设备和网卡 ELM 依赖
`virtio-consumer`。provider 与 consumer 因而不会在同一 crate 中合并互斥角色，公共协议
类型仍只在 `virtio` crate 中维护。

当前 provider 契约只承担版本就绪检查，不是通用的队列 RPC。consumer 的设备数据路径
直接使用共享 Rust API、内核设备抽象和自身持有的 DMA/transport 对象。

## ELM 生命周期与构建模式

[`Elm.toml`](Elm.toml) 发布以下 API：

- ELM 名称：`virtio.framework`；
- 类型：`service`，设备阶段初始化；
- API crate：`virtio`，路径 `api/`；
- 契约：`driver.virtio.framework@1`，版本 `1`；
- Kernel API Profile：`hitoshizuku-default`。

实际部署模式由根目录 `.config` 中的 `CONFIG_VIRTIO` 选择：

- `m`：生成受 `elm-mgr` 管理的 EKI；provider 导出 `direct-pinned` revision，consumer
  必须在装载时完成精确契约和 Rust ABI 绑定；
- `y`：以 `elm-integrated` feature 构建静态归档并链接进内核；ELM 导入导出元数据关闭，
  依赖和初始化顺序由模块清单保证；
- `n`：不构建 framework，依赖它的 VirtIO consumer 也不能启用。

`CONFIG_VIRTIO_BLK` 和 `CONFIG_VIRTIO_NET` 必须与 framework 使用兼容的模式；
`cargo xtask modules` 会检查依赖和拓扑顺序。不要手工修改生成目录来模拟模式切换。

## 验证

以下命令均在仓库根目录执行。Cargo 检查适合验证 Rust 依赖图，完整 ELM Profile、契约、
归档或 EKI 则必须通过 `xtask modules` 验证。

```sh
cargo check -p virtio-framework --lib --target riscv64gc-unknown-none-elf
cargo check -p virtio-framework --lib --target loongarch64-unknown-none
cargo xtask modules --target riscv64gc-unknown-none-elf
```

需要调整 `y/m/n` 时运行 `cargo xtask config`，然后重新运行对应目标的
`cargo xtask modules --target <triple>`。
