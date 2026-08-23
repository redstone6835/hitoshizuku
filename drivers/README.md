# 内核驱动

本目录包含项目自有的内核驱动。每个驱动都是普通 Cargo 包，也是仓库
workspace 的成员；不需要生成 `.elm/framework` 目录，也不需要为驱动单独
指定 Rust 工具链。

使用默认 Rust 工具链构建或检查单个驱动：

```sh
cargo check -p platform-uart16550
cargo build -p virtio-block --target riscv64gc-unknown-none-elf
```

`Modules.toml` 是声明式驱动清单，由 `cargo xtask config` 和
`cargo xtask modules` 消费。它把类似 Linux 的 `CONFIG_*` 符号映射到驱动包，
并记录依赖关系和目标限制。ELM 清单与链接脚本仍放在驱动目录中，因为它们
描述驱动的模块 ABI；只有生成可加载 ELM 模块时才会消费这些文件。

VirtIO 协议类型位于 `virtio/api`。它的 ELM provider 和 consumer 绑定分别位于
`virtio-provider` 与 `virtio-consumer` 包中，因此 workspace 构建不会把两个
互斥的 Cargo feature 角色合并到一起。

普通内核构建不会编译所有驱动。选中的配置会通过内核 feature 启用内建驱动，
或者把 `m` 条目作为独立包构建，供后续 initramfs 或模块装载使用。

## 当前驱动目录

| 目录 | 作用 |
| --- | --- |
| `firmware-bus`、`fw-cfg` | 固件总线与 fw_cfg 设备 |
| `uart16550`、`syscon`、`plic`、`loongson-irq` | 控制台、系统控制器和中断控制器 |
| `ls7a-rtc`、`goldfish-rtc`、`random`、`cfi-flash` | 平台时钟、随机源和闪存 |
| `virtio`、`virtio-blk`、`virtio-net` | VirtIO framework、块设备和网络设备 |
| `net-stack`、`loopback` | 网络栈 ELM 与回环设备 |

驱动 crate 的 `src/main.rs` 同时作为 ELM 模块入口和 workspace library target，这是
为了让 `cargo-elm` 能在集成和独立模块两种模式下复用同一份实现。`Elm.toml`、
`Elm.lock` 和 `elm.ld` 是模块 ABI 元数据，不是 Cargo 依赖缓存。
