# 贡献指南

## 开发环境

使用系统默认 Rust 工具链，不在仓库中提交 `rust-toolchain.toml`。提交前至少运行：

```sh
cargo fmt --all -- --check
cargo metadata --no-deps --format-version 1
cargo test -p socket --target x86_64-unknown-linux-gnu
```

内核模块构建还需要目标架构标准库、Rust 自带的 `rust-lld` 和 EFI 所需的交叉 C 编译器。
当前 [`.cargo/config.toml`](.cargo/config.toml) 使用 `loongarch64-linux-gnu-gcc` 与
`riscv64-linux-gnu-gcc`；这些命令名不需要与 Rust target triple 相同。性能、Native 和
SOYO 工具测试分别在对应 sibling repository 执行。

## 分支和提交

从 `main` 创建短生命周期分支，例如 `fix/pci-api-export` 或 `feat/device-function`。
不要直接向 `main` 推送。提交消息使用 Conventional Commits：

```text
<type>(<scope>): <imperative summary>
```

常用类型为 `feat`、`fix`、`refactor`、`build`、`docs`、`test` 和 `perf`。一次提交保持
单一目的；跨仓库变更先提交 ABI/接口仓库，再在内核仓库固定新 revision。

## 拉取请求

拉取请求应说明：

1. 变更的行为和影响范围；
2. 是否改变 ELM、Native ABI、SOYO 格式或驱动模块配置；
3. 已运行的命令、目标架构和测试结果；
4. 若有迁移要求，给出旧版本到新版本的步骤。

不要把 `target/`、`build/`、`.elm/`、镜像、测量结果或第三方缓存提交到仓库。

## 代码要求

- 内核代码保持 `no_std` 边界和现有 crate 依赖方向。
- 新的公开内核符号必须有审核过的 ELM API 路径、ABI 摘要和能力声明。
- 驱动通过 `DeviceFunction`、PnP 资源租约和 `drivers/Modules.toml` 接入，不复制私有
  总线或设备函数表。
- 网络状态遵守 shard 单写者模型；并发修复必须补充针对 generation/租约的测试。
- 文档、错误信息和新增注释使用中文；Rust API、协议字段和命令保持原始标识。
