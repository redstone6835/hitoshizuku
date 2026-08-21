# 仓库布局

内核及其紧耦合驱动组成一个 Rust workspace。其他项目代码拥有独立的
所有权和构建边界；不应为了把它们放进内核仓库而添加 submodule。

| 仓库 | 内容 | 集成方式 |
| --- | --- | --- |
| `hitoshizuku`（当前仓库） | `arch/`、`hal/`、`general/`、`kernel/`、`libs/`、`drivers/` 和核心 Markdown 文档（含 `SOYO_FORMAT.md`） | Cargo workspace；`kernel` 是默认成员 |
| `hitoshizuku-elm-tools` | `cargo-elm` 主机端程序和 ELM 工程工具 | 开发阶段使用 `cargo install --path`，发布后使用 `cargo install --git` |
| `hitoshizuku-soyo-linker` | `soyo-ld`、`soyo-verify` 和 `soyo-inspect` 主机端程序 | 开发阶段使用 `cargo install --path`，发布后使用 `cargo install --git` |
| `hitoshizuku-native` | `mrt/`、`ranalib/`、`anonlib`、C/Rust 示例和测试 | `native-xtask` 生成清单对应的绑定并调用 `soyo-ld` |
| `hitoshizuku-bench` | `bench/`、QEMU 插件、性能分析和分析脚本 | 消费带标签的内核构建产物以及外部提供的输入镜像 |
| `hitoshizuku-initramfs`（未来） | BusyBox、rootfs、CPIO 和镜像组装 | 作为 `cargo xtask build --initramfs` 的输入；绝不成为内核 workspace 成员 |

当前 checkout 暂时保留后几类源码，确保项目代码不会丢失，但 `tools`、
`native` 和 `bench` 不属于内核 workspace。将来可以使用 `git subtree split`
或复制方式迁移到上表仓库，而无需改变内核依赖图。不使用本地 submodule：
在没有公开远端和固定修订版本时，它不可复现。

公开仓库建立后，应按所有权边界使用 subtree 保留历史，例如：

```sh
git subtree split --prefix=tools/elm-tools -b hitoshizuku-elm-tools
git subtree split --prefix=tools/soyo-linker -b hitoshizuku-soyo-linker
git subtree split --prefix=native -b hitoshizuku-native
git subtree split --prefix=bench -b hitoshizuku-bench
```

`scripts/` 按用途拆分，不整体复制：内核构建和 KCSAN 辅助脚本留在内核工具中，
性能分析和 QEMU 辅助脚本归入 `hitoshizuku-bench`，BusyBox/镜像辅助脚本等待
`hitoshizuku-initramfs`。

仓库不包含 `rust-toolchain.toml` 或 Cargo 工具链覆盖文件，因此主机工具使用
用户选择的默认 Rust 工具链。ELM 内核路径仍暴露既有的 ABI/编译器能力要求
（`#[linkage]`），目标架构 EFI C 编译器是外部构建前置条件；移除该 ABI 要求属于
单独的兼容性改动，不属于仓库布局改动。

## 内核工作流

```sh
cargo check --workspace --lib --target loongarch64-unknown-none
cargo xtask config
cargo xtask modules --target loongarch64-unknown-none
cargo xtask build --target loongarch64-unknown-none
cargo test -p socket --target x86_64-unknown-linux-gnu
```

`drivers/Modules.toml` 是类似 Kconfig 的声明源。`y`、`m`、`n` 分别选择集成、
受管和禁用的 ELM 构建。`cargo-elm` 校验依赖图并生成模块清单；`xtask` 只编排
两次 Cargo 构建。initramfs 生成有意放在这条工作流之外。

## 依赖策略

在公开 API 完成版本化之前，ELM 和 Native ABI crate 留在内核 workspace 中。
拆分后，`cargo-elm` 和 `hitoshizuku-native` 使用已发布 crate，或使用固定到内核
标签的 Git 依赖，绝不跟随浮动分支。submodule 只用于必须固定版本的外部源码
（例如 native 仓库中的 TLSF 快照），不用于项目自有驱动、文档或性能脚本。
