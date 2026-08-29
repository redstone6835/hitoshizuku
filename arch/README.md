# arch：架构实现

`arch/` 提供内核依赖的架构相关实现，不负责设备策略和通用资源管理。当前包含
x86_64、LoongArch64 与 RISC-V64 的启动入口、异常/中断分发、页表、上下文切换、VDSO
和架构测试。

## 边界

- 这里可以使用架构寄存器、异常帧和页表格式；
- 这里不应直接实现 VFS、PnP、ELM 或具体设备驱动；
- 跨架构接口通过 `hal/` 暴露，架构差异留在本 crate 的模块内；
- 新增架构时必须同时提供 linker 配置、启动路径和最小异常处理闭环。

## 常用命令

```sh
cargo check -p arch --target loongarch64-unknown-none
cargo check -p arch --target riscv64gc-unknown-none-elf
cargo check -p arch --target x86_64-unknown-none
cargo test -p arch --target x86_64-unknown-linux-gnu
```

启动流程和依赖关系见根目录的 [`ARCHITECTURE.md`](../ARCHITECTURE.md)。
