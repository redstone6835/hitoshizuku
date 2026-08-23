# kernel/linker：目标链接脚本

本目录保存 QEMU 启动目标使用的 GNU ld linker script。脚本只描述镜像布局、启动栈、
异常向量和架构保留区域；Rust crate 的符号导出和 ELM 接口仍由 Cargo/`cargo-elm`
负责。

文件名按架构和调试变体区分：

- `qemu-loongarch64.ld`、`qemu-loongarch64-debug.ld`；
- `qemu-riscv64.ld`、`qemu-riscv64-debug.ld`。

修改脚本后至少执行一次对应目标的 `cargo xtask build`，并检查 map、入口地址、栈边界
和 VDSO 符号。不要把 initramfs 或磁盘镜像地址硬编码进这里。
