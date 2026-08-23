# configs：驱动选择配置

`configs/` 保存可审查的默认配置样例，不保存生成的构建状态。驱动的声明源是
[`drivers/Modules.toml`](../drivers/Modules.toml)，其中的 `y`、`m`、`n` 分别表示
内建、可单独构建的 ELM 模块和禁用。

```sh
cargo xtask defconfig
cargo xtask config
cargo xtask modules --target loongarch64-unknown-none
```

`.config` 是本地生成文件，不应提交。配置只选择项目已有的 crate；新增驱动时先更新
`Modules.toml`、依赖关系和目标限制，再补充对应 README 或架构文档。
