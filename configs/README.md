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

`platforms.toml` 是独立的构建平台目录：它把 board/target、链接布局、物理/虚拟基址、
默认 preset、Cargo 输出隔离路径和内核镜像封装配方绑定在一起。高半区地址使用固定的
`0x0000_0000_0000_0000` 字符串格式，加载时会校验 DMW1/Sv48 映射关系。新增同架构
板卡只增加平台项，不复制链接脚本；平台 ID 还会生成唯一的 ELF provenance tag，防止
同 target、同地址的板卡产物互相封装。新增链接布局才需要同时扩展架构链接脚本和校验器。

板级 preset 是可审查基线，不是交互配置输出。需要实验性取值时另建本地配置，并通过
`--config <path>` 传入。
