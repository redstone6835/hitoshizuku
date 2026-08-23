# 仓库布局

Hitoshizuku 内核仓库只承载紧耦合的 Rust 代码和核心协议文档。驱动与
`arch/`、`hal/`、`general/`、`kernel/`、`libs/` 共用同一 Cargo workspace；主机工具、
Native runtime 和性能分析使用独立仓库及固定版本依赖，不通过本地 submodule 拼接。

| 仓库 | 内容 | 集成方式 |
| --- | --- | --- |
| [`hitoshizuku`](https://github.com/redstone6835/hitoshizuku) | 内核、紧耦合驱动、ELM/Native ABI crate、核心中文文档 | Cargo workspace，`kernel` 为默认成员 |
| [`hitoshizuku-elm-tools`](https://github.com/redstone6835/hitoshizuku-elm-tools) | `cargo-elm` 和 ELM 工程工具 | `cargo install --locked --git`；用 `HITOSHIZUKU_KERNEL_ROOT` 选择内核 checkout |
| [`hitoshizuku-soyo-linker`](https://github.com/redstone6835/hitoshizuku-soyo-linker) | `soyo-ld`、`soyo-verify`、`soyo-inspect` | `cargo install --locked --git`；依赖固定内核 revision 的 ABI crate |
| [`hitoshizuku-native`](https://github.com/redstone6835/hitoshizuku-native) | MRT、Ranalib、Anonlib、C/Rust 示例和测试 | Native 自己的 workspace；通过 `SOYO_LD` 接入链接器 |
| [`hitoshizuku-bench`](https://github.com/redstone6835/hitoshizuku-bench) | QEMU 插件、画像脚本、统计学习模型和工作负载 | 消费带提交标识的内核产物与外部输入镜像 |
| `hitoshizuku-initramfs`（未来） | BusyBox、rootfs、CPIO 和镜像组装 | 作为 `cargo xtask build --initramfs` 的外部输入 |

## 内核 workspace

```text
arch/       启动、异常、中断、页表和架构上下文
hal/        时间、中断、用户地址访问和 CPU 控制抽象
general/    PnP、DeviceFunction、VFS、固件和通用设备设施
kernel/     最终镜像、系统调用、进程、ELM 管理和网络 host
libs/       allocator、sched、net、socket、vfs、elm、soyo 等共享 crate
drivers/    项目自有硬件驱动与 ELM；由 Modules.toml 选择 y/m/n
xtask/      内核 Cargo 编排入口
```

`tools/`、`native/`、`bench/` 不再位于内核 checkout。需要构建模块时先安装
`cargo-elm`；`xtask` 自动传入当前内核根目录，直接从其他目录调用工具时使用
`HITOSHIZUKU_KERNEL_ROOT`。工具仓库使用固定 Git revision；ABI 版本化后再切换为发布 crate。

核心目录的入口说明位于各目录的 `README.md`；设备对象、驱动资源和热拔顺序见
[`DEVICE_ABSTRACTION.md`](DEVICE_ABSTRACTION.md)。目录 README 只解释源码边界和本地
检查命令，不重复维护 Cargo manifest 或生成文件清单。

## 内核工作流

```sh
cargo metadata --no-deps --format-version 1
cargo check --workspace --lib --target loongarch64-unknown-none
cargo xtask config
cargo xtask modules --target loongarch64-unknown-none
cargo xtask build --target loongarch64-unknown-none
cargo test -p socket --target x86_64-unknown-linux-gnu
```

`drivers/Modules.toml` 是类似 Kconfig 的声明源：`y` 集成进内核，`m` 生成受管 EKI，
`n` 禁用模块。内核仓库不包含 initramfs、rootfs 或性能输入镜像。

## 依赖和 submodule 原则

项目自有代码使用独立仓库、Cargo Git 依赖和标签管理；不把自有驱动、工具或文档做成本地
submodule。submodule 只适用于必须固定版本的外部源码，例如 Native 仓库中的 TLSF
上游快照。所有工具和 runtime 都提交自己的 `Cargo.lock`，内核 workspace 也提交锁文件。
