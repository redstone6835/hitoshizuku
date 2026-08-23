# VirtIO 公共 API

本目录是 `virtio` crate，集中维护 VirtIO consumer 共享的传输层和 split virtqueue
实现。它是普通的 `no_std` Rust 库，不是可单独装载的 ELM，也没有自己的生命周期入口。

## 提供的能力

`src/lib.rs` 提供 modern VirtIO over PCI 的公共实现：

- `VirtioPciFunction` 统一描述 vendor/device ID 匹配；
- `parse_virtio_pci_caps` 校验 PCI capability chain、BAR 类型和窗口边界；
- `VirtioPciTransport` 封装状态、feature、队列地址、notify 和 ISR 访问；
- `SplitVirtQueue` 管理 descriptor/available/used 三段 DMA ring、描述符所有权、完成回收
  和 `EVENT_IDX` 辅助逻辑；
- `DescriptorChain`、`VirtqDescUpdate`、`UsedChain` 与 `VirtQueueError` 描述安全的队列记账
  边界。

`src/virtio_mmio.rs` 提供 `VirtioMmioTransport`、`ModernMmioTransport`、
`LegacyMmioTransport` 和 `detect`，把 MMIO v1 legacy 与 v2 modern 的寄存器差异收敛到
同一接口。legacy 队列使用连续 DMA 布局和 QueuePFN，modern 队列分别写入三段 DMA 地址。

`SplitVirtQueue` 拥有其 `DmaBuffer`，并在释放时归还分配；它只保证协议 ring 和描述符
状态的一致性。具体驱动仍负责以下事项：

- 选择设备类型、队列编号和设备专属 feature；
- 从 PnP 资源建立有效的 MMIO/PCI 访问前提；
- 编排请求 buffer 的 DMA 所有权和同步方向；
- 注册、中止和释放 IRQ；
- 调用 `PnpDevice::register_function` 暴露设备功能；
- 在失败或移除时停止设备并排空上层引用。

因此本 crate 不包含 PnP driver、全局设备表、块层策略、网络协议栈或 ELM
provider/consumer 绑定。

## 依赖与稳定边界

该 crate 只直接依赖 `general`，使用其中的 `DmaBuffer`、`DmaContext`、`PciDevice` 和 BAR
描述。它被 [`virtio-provider`](../provider-api/README.md) 与
[`virtio-consumer`](../consumer-api/README.md) 原样重导出；公开类型布局会进入 ELM Rust
ABI 摘要，因此修改公开结构、函数签名或 feature 图时必须同步升级 framework 契约。

当前实现支持仓库的 RISC-V64 与 LoongArch64 裸机目标。编译检查不访问真实硬件，也不等价
于完成设备探测测试。

## 验证

在仓库根目录执行：

```sh
cargo check -p virtio --target riscv64gc-unknown-none-elf
cargo check -p virtio --target loongarch64-unknown-none
cargo check -p virtio-provider --target riscv64gc-unknown-none-elf
cargo check -p virtio-consumer --target riscv64gc-unknown-none-elf
```

涉及 framework 契约或实际 consumer 时，还应运行：

```sh
cargo xtask modules --target riscv64gc-unknown-none-elf
```
