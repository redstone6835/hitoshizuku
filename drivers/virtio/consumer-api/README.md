# VirtIO consumer 接口

本目录是 `virtio-consumer` crate，供 [`virtio-blk`](../../virtio-blk/README.md) 和
[`virtio-net`](../../virtio-net/README.md) 使用。它重导出公共
[`virtio`](../api/README.md) API，并把 framework 依赖收敛为一个明确的就绪检查。

## 当前契约

受管构建声明以下导入：

```text
名称      virtio.framework.revision
契约      driver.virtio.framework@1
版本      1
调用模式  direct-pinned
Rust 类型 fn() -> u32
```

consumer 在 ELM `initialize` 开始时调用 `framework_ready()`：只有导入槽已经绑定且 provider
返回 revision 1 时，函数才返回 `true`。块设备和网卡据此在注册 PnP driver factory 之前
拒绝缺失或不兼容的 framework。

这不是设备服务代理。`virtio-consumer` 不创建队列、不扫描 PCI/MMIO、不注册 IRQ，也不
保存设备状态；具体驱动从重导出的公共 API 构造 transport 和 `SplitVirtQueue`，并自行遵守
PnP 与资源所有权规则。

## `m` 与 `y`

- `m` 模式不启用 `elm-integrated`。`framework_ready()` 读取由 ELM 装载器填充的
  `DirectImport<fn() -> u32>`，验证绑定和 revision；
- `y` 模式由具体驱动把 `elm-integrated` 转发到本 crate。此时组件已由内核构建图静态组合，
  `framework_ready()` 返回 `true`，不生成运行时 import；
- `n` 模式不构建 consumer。启用 consumer 而禁用 `virtio.framework` 属于无效模块配置。

`drivers/Modules.toml` 中的 `depends = "virtio.framework"` 是部署依赖；Cargo 的 path
dependency 只解决源码编译，不能替代 ELM 运行时绑定。

## 验证

在仓库根目录执行：

```sh
cargo check -p virtio-consumer --target riscv64gc-unknown-none-elf
cargo check -p virtio-consumer --target loongarch64-unknown-none
cargo check -p virtio-block --lib --target riscv64gc-unknown-none-elf
cargo check -p net-virtio --lib --target riscv64gc-unknown-none-elf
```

检查实际 provider-consumer 绑定和所选 `y/m/n` 模式：

```sh
cargo xtask modules --target riscv64gc-unknown-none-elf
```
