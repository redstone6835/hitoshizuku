# VirtIO provider 接口

本目录是 `virtio-provider` crate，由 [`virtio-framework`](../README.md) 使用。它把公共
[`virtio`](../api/README.md) API 原样重导出，并声明 framework 向 consumer 提供的 ELM
导出。

## 当前契约

当前唯一导出是：

```text
名称      virtio.framework.revision
契约      driver.virtio.framework@1
版本      1
调用模式  direct-pinned
可见性    dependency
Rust 类型 fn() -> u32
```

`framework_revision()` 返回 `1`。framework 自身在初始化时也会检查这个值，避免清单版本
与实际 provider 实现分离。

该导出只证明 consumer 绑定到了兼容的 framework generation。它不传递裸指针，不提供
队列创建 RPC，也不拥有 PCI/MMIO、DMA、IRQ、PnP device 或 `DeviceFunction`。这些资源由
具体 VirtIO consumer 在 probe 路径中取得，并登记到相应 `PnpDevice`。

## 与 consumer 的关系

受管 `m` 模式下，`#[elm::export]` 生成带精确 Rust ABI 摘要的 `direct-pinned` 导出；装载器
只有在 provider 名称、契约、版本和函数类型全部匹配时才会绑定 consumer。绑定固定 provider
generation，framework 被替换或撤销时不会留下可猜测的符号调用。

集成 `y` 模式下，`elm-integrated` 关闭运行时 ELM 导出元数据。framework 和 consumer 作为
静态归档按 `drivers/Modules.toml` 的依赖顺序进入内核，不再建立 cell 间绑定。

不要让设备驱动直接依赖本 crate。consumer 应依赖
[`virtio-consumer`](../consumer-api/README.md)，以免把 provider 角色和 import 角色合并到
同一个 Cargo feature 图。

## 验证

在仓库根目录执行：

```sh
cargo check -p virtio-provider --target riscv64gc-unknown-none-elf
cargo check -p virtio-provider --target loongarch64-unknown-none
cargo check -p virtio-framework --lib --target riscv64gc-unknown-none-elf
```

Cargo 检查不会解析 ELM 导入导出；完整契约校验使用：

```sh
cargo xtask modules --target riscv64gc-unknown-none-elf
```
