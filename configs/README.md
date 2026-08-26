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

`ls2k1000.config` 与 `visionfive2.config` 是受版本控制的板级 preset。它们把中断、时钟、
DT provider、控制台和启动存储链所需模块设为 `y`，把非关键外设保留为 `m`，并禁用目标
架构不可能使用的模块。`cargo xtask modules/build --board <board>` 仅在没有显式
`--config` 时选用对应 preset：

```sh
cargo xtask modules --board ls2k1000
cargo xtask build --board visionfive2
```

板级 preset 是可审查基线，不是交互配置输出。需要实验性取值时另建本地配置，并通过
`--config <path>` 传入。
