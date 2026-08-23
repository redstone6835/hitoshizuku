# VirtIO 块设备驱动

本目录实现 `virtio-block` Cargo 包和 `virtio.block` ELM。驱动把 VirtIO-MMIO 或 modern
VirtIO-PCI block function 转换为内核通用块设备，提交异步 `Bio`，并通过
`BlockFunction` 将设备投影为稳定的 `vd*` 名称。

它不负责分区扫描、文件系统、页缓存、PCI host bridge 或 platform 设备枚举；这些职责分别
属于块层/VFS和总线、固件驱动。本驱动只在 PnP core 已发布相应设备后参与匹配和 probe。

## 源码结构

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | ELM 生命周期，校验 framework，注册/注销 MMIO 与 PCI driver factory |
| `src/common.rs` | feature 与 config 解析、请求规划、DMA 映射、descriptor chain 和 pending 队列 |
| `src/mmio.rs` | VirtIO-MMIO v1/v2 探测、队列寄存器、中断与 platform PnP 绑定 |
| `src/pci.rs` | modern VirtIO-PCI capability、队列、中断与 PCI PnP 绑定 |

公共层支持 `Read`、`Write`、`Flush`、`Discard` 和 `WriteZeroes`，并把设备声明的只读、
逻辑块大小、段数、单段大小及范围操作限制转换为内核 `BlockFeatures`/`BlockLimits`。
提交路径优先借用 BIO 页的 DMA 映射，必要时使用驱动缓冲；header/data/status descriptor
和完成状态由同一 pending 记录管理。队列锁在持有期间关闭本地中断，防止完成中断重入同一
virtqueue。

`block-profile` feature 增加块控制面的调试画像文本。它会改变内核 API 的
request/response 枚举形状，因此默认构建不会启用；只有内核先以
`general/block-profile` 构建并导出匹配的 API Profile 时，模块构建才能显式启用。
[`../Modules.toml`](../Modules.toml) 中的 `features` 字段只声明该 feature 归属本模块，
不会默认打开它。

## 探测、功能与资源所有权

MMIO 路径匹配 platform ID `virtio,mmio` 或 `LNRO0005`，再验证 magic、v1/v2 和 block
device ID 2。它使用固件提供的 MMIO、DMA context 和可选 IRQ；IRQ handle 通过
`PnpDevice::own_resource` 登记。

PCI 路径匹配 VirtIO vendor `0x1af4` 的 block IDs `0x1001`/`0x1042`，启用 MMIO decode
和 bus master，校验 modern capabilities 后完成 feature 与队列协商。中断优先使用 MSI，
否则回退到路由 INTx；MSI 配置资源和 IRQ handler 都归对应 `PnpDevice` 所有。

probe 成功后，两条路径都调用 `PnpDevice::register_function` 注册 `BlockFunction`，稳定名称
由 `FunctionProjectionNameAllocator("vd")` 按 PnP key 分配。DMA ring、请求 buffer 和 transport
由块设备对象持有；PnP 移除先排空 function 引用和设备资源，最终对象释放时 reset 设备。
初始化任一步失败都会撤销已注册的 IRQ、队列或 driver factory，避免半绑定设备。

## ELM 与模块配置

| 项目 | 值 |
| --- | --- |
| 配置键 | `CONFIG_VIRTIO_BLK` |
| ELM 名称 | `virtio.block` |
| 类型/阶段 | `driver` / `device` |
| framework 依赖 | `virtio.framework` |
| 契约 | `driver.virtio.framework@1` |
| API crate | `virtio-consumer`，在源码中命名为 `virtio` |
| 支持目标 | `riscv64gc-unknown-none-elf`、`loongarch64-unknown-none` |

`initialize` 先执行 `virtio::framework_ready()`，再注册 MMIO 和 PCI factory；任一注册失败会
回滚另一项。`finalize` 按 PCI、MMIO 的逆序注销 factory，仍有绑定设备时会返回 busy，防止
模块代码先于回调释放。

`m` 模式生成受管 EKI，并通过 `direct-pinned` revision import 绑定 framework；`y` 模式启用
`elm-integrated`，把同一 `src/main.rs` 编成静态归档并依赖构建图排序；`n` 模式不构建。
`CONFIG_VIRTIO` 必须启用并与 consumer 使用兼容模式。

## 验证

在仓库根目录执行：

```sh
cargo check -p virtio-block --lib --target riscv64gc-unknown-none-elf
cargo check -p virtio-block --lib \
  --features virtio-block/block-profile,general/block-profile \
  --target riscv64gc-unknown-none-elf
cargo xtask modules --target riscv64gc-unknown-none-elf
```

第一条检查默认 feature，第二条额外检查画像接口；第三条才会按 `.config` 验证 Kernel API
Profile、framework 依赖和 EKI/集成归档。LoongArch64 检查可把命令中的 target 替换为
`loongarch64-unknown-none`。部署模式通过 `cargo xtask config` 修改。
