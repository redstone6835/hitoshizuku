# kernel/linker：目标链接脚本

本目录保存 QEMU 启动目标使用的 GNU ld linker script。脚本只描述镜像布局、启动栈、
异常向量和架构保留区域；Rust crate 的符号导出和 ELM 接口仍由 Cargo/`cargo-elm`
负责。

仓库只维护每个架构的规范脚本：

- `qemu-loongarch64.ld`；
- `qemu-riscv64.ld`。

`kernel/build.rs` 会从规范脚本生成调试变体，并写入 Cargo 的构建输出目录；不要在
本目录手工维护 `*-debug.ld`。LoongArch64 板级脚本同样由构建脚本派生，启动路径
保持 U-Boot/固件直接跳转到内核入口，不包含 EFI 入口或 PE/COFF 头。

修改脚本后至少执行一次对应目标的 `cargo xtask build`，并检查 map、入口地址、栈边界
和 VDSO 符号。不要把 initramfs 或磁盘镜像地址硬编码进这里。
