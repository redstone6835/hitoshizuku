# kernel：最终镜像与内核编排

`kernel/` 是最终内核镜像 crate。它把 `arch`、`hal`、`general`、共享库和选中的驱动
组合成启动映像，负责启动阶段、系统调用、进程、ELM host、网络 host、VFS 挂载和
架构相关的最终链接。

## 这里负责什么

- 建立启动期的全局上下文和基础设施；
- 根据 `drivers/Modules.toml` 启用内建驱动或导出模块；
- 管理 ELM 生命周期、Kernel API Profile 和内核导入符号；
- 将外部 initramfs 作为输入加载，不负责生成 rootfs 或 CPIO 镜像。

## 构建

通常从根目录调用：

```sh
cargo xtask build --target loongarch64-unknown-none
cargo xtask build --target riscv64gc-unknown-none-elf
```

直接构建只适合检查 Cargo 依赖；涉及 ELM 接口导出时应使用 `xtask`，以保证接口、
共享 framework 和模块配置使用同一份构建上下文。
