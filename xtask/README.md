# xtask：内核工程入口

`xtask` 是根 workspace 的 Cargo 工程命令，替代旧的 Makefile 编排。它只编排内核
源码、ELM 工具和驱动配置，不生成 initramfs，也不下载或管理第三方 rootfs。

## 命令

```sh
cargo xtask defconfig
cargo xtask oldconfig
cargo xtask config
cargo xtask modules [--board <qemu|ls2k1000|visionfive2>] \
  [--target <triple>] \
  [--config .config] [--output build/loongarch64/modules] \
  [--target-dir target/loongarch64] [--features <列表>]
cargo xtask build [--board <qemu|ls2k1000|visionfive2>] \
  [--target <triple>] \
  [--config .config] [--modules build/loongarch64/modules] \
  [--target-dir target/loongarch64] [--features <列表>] \
  [--initramfs <cpio>]
cargo xtask clean
```

`defconfig` 从受版本控制的默认值重新生成 `.config`；`oldconfig` 补齐新增选项；
`config` 提供交互式配置。`modules` 先构建用于接口导出的 kernel，再用
`cargo-elm profile-export` 生成 Kernel API Profile，最后按
`drivers/Modules.toml` 构建 `y/m/n` 模块集合。

`build` 消费现有 `modules.manifest` 和 `integrated.archives` 完成最终链接；只有模块清单
不存在时才自动调用 `modules`。修改 `.config`、模块源码或模块 feature 后，应先显式运行
`cargo xtask modules`，避免复用过期清单。所有子进程共享 `target/<arch>` 和
`build/elm-interface/<arch>`，这样 Cargo 可以复用公共依赖的指纹与接口快照。

`--board` 同时选择板卡允许的 target、板级默认配置和隔离产物路径：

| board | 默认 target | 未指定 `--config` | 默认产物前缀 |
| --- | --- | --- | --- |
| `qemu` | `loongarch64-unknown-none` | `.config` | `target/<arch>`、`build/<arch>` |
| `ls2k1000` | `loongarch64-unknown-none` | `configs/ls2k1000.config` | `target/loongarch64/ls2k1000`、`build/loongarch64/ls2k1000` |
| `visionfive2` | `riscv64gc-unknown-none-elf` | `configs/visionfive2.config` | `target/riscv64/visionfive2`、`build/riscv64/visionfive2` |

物理板的 Kernel API Profile 也按板卡放在 `build/elm-interface/<arch>/<board>`。
`qemu` 保持原有架构级路径兼容，并可显式选择两个受支持 target。物理板与错误架构组合会
在启动 Cargo 前失败。LS2K1000 构建自动向接口导出用 kernel 和最终 kernel 传入
`MYGO_LA_BOARD=ls2k1000`，选择 U-Boot 的 `0x200000` 直接入口布局。

```sh
cargo xtask modules --board ls2k1000
cargo xtask build --board ls2k1000
cargo xtask modules --board visionfive2
cargo xtask build --board visionfive2
```

默认目标是 `loongarch64-unknown-none`，另一个受支持目标是
`riscv64gc-unknown-none-elf`。`--initramfs` 只接收已经生成的 CPIO 镜像；本仓库不负责
制作 rootfs。

执行 `cargo xtask` 的当前目录必须是内核仓库根目录。跨目录调用 `cargo-elm` 时设置
`HITOSHIZUKU_KERNEL_ROOT`。
