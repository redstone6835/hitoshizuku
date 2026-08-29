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
  [--reuse-modules] \
  [--target-dir target/loongarch64] [--features <列表>] \
  [--initramfs <cpio>]
cargo xtask image [--platform <id> | --board <board> [--target <triple>]] \
  [--format <elf|raw|uimage|efi|all>] [--reuse-modules] [--no-build] \
  [--objcopy <path>] [--mkimage <path>]
cargo xtask clean
```

`defconfig` 从受版本控制的默认值重新生成 `.config`；`oldconfig` 补齐新增选项；
`config` 提供交互式配置。`modules` 先构建用于接口导出的 kernel，再用
`cargo-elm profile-export` 生成 Kernel API Profile，最后按
`drivers/Modules.toml` 构建 `y/m/n` 模块集合。

`build` 默认先重新导出 Kernel API Profile 并执行 `cargo-elm build-set`，再消费本次生成的
`modules.manifest` 和 `integrated.archives` 完成最终链接。因此修改内核接口、`.config`、模块
源码或模块 feature 后，直接运行 `build`/`image` 不会静默复用过期集成归档；Cargo 和
`cargo-elm` 仍可在各自内部复用未变化的构建结果。只有明确确认现有模块产物与当前源码匹配时，
才使用 `--reuse-modules` 跳过接口导出和模块构建。该选项会校验 manifest、归档列表及归档文件
均存在，但不会推断它们是否与源码同步。所有子进程共享 `target/<arch>` 和
`build/elm-interface/<arch>`，以复用公共依赖的指纹与接口快照。

`--board` 同时选择板卡允许的 target、板级默认配置和隔离产物路径。QEMU 还可以通过
`--platform qemu-x86_64` 或 `--target x86_64-unknown-none` 选择 x86_64 higher-half
布局。Multiboot2 入口和独立 UEFI loader 会复制、校验启动协议数据，再交给通用
`StartContext`；Linux boot protocol 当前提供严格解析与上下文构造接口，不宣称已有可执行
handover 入口。

平台选择表：

| board | 默认 target | 未指定 `--config` | 默认产物前缀 |
| --- | --- | --- | --- |
| `qemu` | `loongarch64-unknown-none`（可选 `riscv64gc-unknown-none-elf`、`x86_64-unknown-none`） | `configs/qemu.config` 或 `configs/qemu-x86_64.config` | `target/<arch>`、`build/<arch>` |
| `ls2k1000` | `loongarch64-unknown-none` | `configs/ls2k1000.config` | `target/loongarch64/ls2k1000`、`build/loongarch64/ls2k1000` |
| `visionfive2` | `riscv64gc-unknown-none-elf` | `configs/visionfive2.config` | `target/riscv64/visionfive2`、`build/riscv64/visionfive2` |

物理板的 Kernel API Profile 也按板卡放在 `build/elm-interface/<arch>/<board>`。
`qemu` 保持原有架构级产物路径兼容，并可显式选择 LoongArch64、RISC-V64 和 x86_64
三个受支持 target；其默认 preset 将
QEMU virt 的固件、IRQ、RTC、PCI、控制台和 VirtIO 块/网驱动全部内建。物理板与错误架构
组合会在启动 Cargo 前失败。xtask 从 `configs/platforms.toml` 解析平台，向 Cargo 传递唯一的
`HITOSHIZUKU_PLATFORM=<id>`；链接脚本不再读取板卡专用环境变量。

```sh
cargo xtask modules --board ls2k1000
cargo xtask build --board ls2k1000
cargo xtask image --board ls2k1000
cargo xtask modules --board visionfive2
cargo xtask build --board visionfive2
cargo xtask image --board visionfive2
```

`image` 默认刷新模块并完成内核构建；`--reuse-modules` 仅复用经过完整性校验的现有模块
产物，`--no-build` 跳过内核与 ELM 构建，只校验和封装已有 Cargo ELF。选择 `efi` 时仍会
构建独立的 `x86_64-unknown-uefi` loader。LoongArch/RISC-V QEMU 默认发布
`kernel.elf`，x86_64 QEMU 默认发布 `kernel.elf` 与 `kernel.bin`，物理板默认发布
`kernel.elf`、`kernel.bin` 和 `uImage`。x86_64 显式选择 `--format efi` 会生成 GPT/FAT
`esp.img`，其中包含 `EFI/BOOT/BOOTX64.EFI` 与 `EFI/HITOSHI/KERNEL.ELF`。内核载荷
封装属于本仓库构建流程；ELF 中的平台 provenance tag 必须与请求平台一致，不能通过
`--target-dir` 混用另一块板的内核。rootfs 与 initramfs 内容仍由外部工程提供。

默认目标是 `loongarch64-unknown-none`；另外支持 `riscv64gc-unknown-none-elf`，QEMU
还支持 `x86_64-unknown-none`。`--initramfs` 只接收已经生成的 CPIO 镜像；本仓库不负责
制作 rootfs。

执行 `cargo xtask` 的当前目录必须是内核仓库根目录。跨目录调用 `cargo-elm` 时设置
`HITOSHIZUKU_KERNEL_ROOT`。
