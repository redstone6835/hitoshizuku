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

`qemu.config`、`qemu-x86_64.config`、`ls2k1000.config` 与 `visionfive2.config` 是受版本控制的平台 preset。
`qemu.config` 把 QEMU virt 实际提供的中断控制器、RTC、PCI ECAM、syscon、fw_cfg、CFI
flash、UART 和 VirtIO 块/网启动链全部设为 `y`；LoongArch 与 RISC-V 专属模块由
`Modules.toml` 的 target 限制自动筛选。JH7110、LS2K 等物理 SoC 外设以及 QEMU 默认未
实例化的 RISC-V IOMMU 保持 `n`，避免把“目标架构可编译”误当作“当前平台存在”。物理板
preset 则把启动链所需模块设为 `y`，非关键外设保留为 `m`。`cargo xtask
modules/build --board <board>` 仅在没有显式 `--config` 时选用对应 preset：

```sh
cargo xtask modules --board ls2k1000
cargo xtask build --board visionfive2
cargo xtask image --board qemu --target loongarch64-unknown-none
```

`qemu-x86_64.config` 面向 QEMU `pc`/`q35` 机器，使用 ACPI 设备枚举与 16550 控制台；
内核提供 Multiboot2 入口，独立 `BOOTX64.EFI` loader 负责 UEFI 交接，两者最终都生成
x86_64 arch loader 规范化的 `StartContext`。Linux boot protocol 目前只有解析与上下文
构造接口，没有可执行入口。

`platforms.toml` 是独立的构建平台目录：它把 board/target、链接布局、物理/虚拟基址、
默认 preset、Cargo 输出隔离路径和内核镜像封装配方绑定在一起。高半区地址使用固定的
`0x0000_0000_0000_0000` 字符串格式，加载时会校验 DMW1、Sv48 或 x86_64 higher-half
映射关系。新增同架构
板卡只增加平台项，不复制链接脚本；平台 ID 还会生成唯一的 ELF provenance tag，防止
同 target、同地址的板卡产物互相封装。新增链接布局才需要同时扩展架构链接脚本和校验器。

平台 preset 是可审查基线，不是交互配置输出。需要实验性取值时另建本地配置，并通过
`--config <path>` 传入。
