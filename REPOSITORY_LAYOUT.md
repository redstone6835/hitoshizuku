# 仓库布局

Hitoshizuku 内核仓库只承载紧耦合的 Rust 代码和核心协议文档。驱动与
`arch/`、`hal/`、`general/`、`kernel/`、`libs/` 共用同一 Cargo workspace；主机工具、
Native runtime 和性能分析使用独立仓库及固定版本依赖，不通过本地 submodule 拼接。

| 仓库 | 内容 | 集成方式 |
| --- | --- | --- |
| [`hitoshizuku`](https://github.com/redstone6835/hitoshizuku) | 内核、紧耦合驱动、ELM/Native/language ABI crate、核心中文文档 | Cargo workspace，`kernel` 为默认成员 |
| [`hitoshizuku-elm-tools`](https://github.com/redstone6835/hitoshizuku-elm-tools) | `cargo-elm` 和 ELM 工程工具 | `cargo install --locked --git`；用 `HITOSHIZUKU_KERNEL_ROOT` 选择内核 checkout |
| [`hitoshizuku-soyo-linker`](https://github.com/redstone6835/hitoshizuku-soyo-linker) | `soyo-ld`、`soyo-verify`、`soyo-inspect` | `cargo install --locked --git`；依赖固定内核 revision 的 ABI crate |
| [`hitoshizuku-native`](https://github.com/redstone6835/hitoshizuku-native) | MRT、Ranalib、Anonlib、C/Rust 示例和测试 | Native 自己的 workspace；通过 `SOYO_LD` 接入链接器 |
| [`hitoshizuku-bench`](https://github.com/redstone6835/hitoshizuku-bench) | QEMU 插件、画像脚本、统计学习模型和工作负载 | 消费带提交标识的内核产物与外部输入镜像 |
| 外部 initramfs 工程 | BusyBox、rootfs、CPIO 和镜像组装 | 产出 CPIO，再作为 `cargo xtask build --initramfs` 的显式输入 |
| 未来的语言支持仓库 | 某种语言的 backend ELM、SDK、AOT runtime 与测试 | 按 tag/revision 引入，不作为内核 submodule；只依赖版本化 `language.runtime.*` ABI |

## 内核 workspace

```text
arch/       启动、异常、中断、页表和架构上下文
hal/        时间、中断、用户地址访问和 CPU 控制抽象
general/    PnP、DeviceFunction、VFS、固件和通用设备设施
kernel/     最终镜像、系统调用、进程、ELM 管理和网络 host
libs/       allocator、sched、net、socket、vfs、elm、language ABI、soyo 等共享 crate
drivers/    项目自有硬件驱动与 ELM 服务；由 Modules.toml 选择 y/m/n
xtask/      内核 Cargo 编排入口
examples/   独立 Cargo workspace 的 Rust SDK 与 fake transport 示例
```

旧版单仓库中的 `tools/`、`native/`、`bench/` 已从内核 checkout 拆分。需要构建模块时先安装
`cargo-elm`；`xtask` 自动传入当前内核根目录，直接从其他目录调用工具时使用
`HITOSHIZUKU_KERNEL_ROOT`。首次 `cargo elm sync` 后，独立 ELM 工程会复用自身
`.elm/kernel-interface` 中已同步的 Profile；只有发现或刷新接口时才需要再次指定内核根目录。
工具仓库使用固定 Git revision；ABI 版本化后再切换为发布 crate。

核心目录的入口说明位于各目录的 `README.md`；设备对象、驱动资源和热拔顺序见
[`DEVICE_ABSTRACTION.md`](DEVICE_ABSTRACTION.md)。目录 README 只解释源码边界和本地
检查命令，不重复维护 Cargo manifest 或生成文件清单。

通用外语接入只在内核仓库保留 `elm-language-abi` 和默认 `y` 的 `language.runtime` 服务。
各语言的 backend、SDK、编译器适配、runtime 和示例应独立发布；loader 只处理通用 ELM
绑定，不按语言分支。ABI、安全约束和当前集成限制见
[`LANGUAGE_RUNTIME.md`](LANGUAGE_RUNTIME.md)。

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
`n` 禁用模块。硬依赖的模块必须使用相同的 `y/m` 模式；构建工具不会自动继承或改写
依赖模式。内核仓库不包含 initramfs、rootfs 或性能输入镜像。

## 依赖和 submodule 原则

项目自有代码按耦合程度放入当前 workspace 或独立仓库，并通过 Cargo Git 依赖和标签管理；
不把自有驱动、工具或文档拼成主仓库 submodule。submodule 只适用于必须固定版本且不由
项目维护的外部源码，例如 Native 仓库中的 TLSF 上游快照。所有工具和 runtime 都提交
自己的 `Cargo.lock`，内核 workspace 也提交锁文件。
