# kernel/linker：架构链接脚本

本目录保存内核使用的 GNU ld linker script。链接脚本描述架构 ABI、段顺序、启动栈、
异常向量和公共内核元数据，不描述具体板卡，也不负责制作启动镜像。

仓库为每种链接布局维护一份始终输出 ELF 的规范脚本：

- `loongarch64.ld`：LoongArch64 DMW1 cached 直映布局；
- `riscv64.ld`：RISC-V64 Sv48 高半区直映布局。

`common-rodata.ld` 保存两个架构共同的 ELM 符号表、异常表和测试元数据，
`common-debug.ld` 保存 VMA 为 0、不会进入装载镜像的 DWARF 段。架构入口、异常段、
对齐要求和架构特有的早期数据仍留在对应架构脚本中。

板卡地址来自 `configs/platforms.toml`。`kernel/build.rs` 根据 `TARGET` 和
`HITOSHIZUKU_PLATFORM` 选择链接布局，并通过 `--defsym` 注入物理/虚拟基址；不得通过
复制脚本或字符串替换增加板级变体。链接器同时写入平台 provenance tag，使同 target、
同地址布局的不同板卡 ELF 也不能被 `xtask image` 混用。直接使用 Cargo 且未设置平台时，
按目标选择对应的 QEMU 平台。

Cargo 的 `release/kernel` 始终是带符号 ELF。raw binary 和 uImage 由
`cargo xtask image` 从 ELF 派生，调试构建不再使用另一份链接脚本。启动路径保持固件
直接跳转到内核入口，不包含 EFI 入口或 PE/COFF 头。

修改脚本后至少执行一次对应目标的 `cargo xtask build`，并检查 ELF 入口、关键符号、
栈边界与 RX/R/RW 装载段。不要把板卡地址、initramfs 或磁盘镜像地址硬编码进这里。
