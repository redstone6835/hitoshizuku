# userland

本目录保存 BusyBox initramfs 骨架、架构相关 rootfs 配置和 ELM 用户态管理工具。
实际 rootfs 由根目录 Makefile 在 `build/<arch>/compat-rootfs` 中组装，不会把生成物
写回源目录。

## 目录结构

- `busybox-initramfs/`：两种架构共用的 BusyBox 配置骨架。
- `rootfs-rv/`：RISC-V64 的 `/etc` 配置覆盖层。
- `rootfs-la/`：LoongArch64 的 `/etc` 配置覆盖层。
- `init-keywait.c`：以非阻塞控制台读取实现 3 秒 Ctrl+C 启动选择。
- `elmctl/`：ELM 用户态管理工具及其固定布局控制面头文件。

## rcS 启动流程

LoongArch64 的 `rootfs-la/etc/init.d/rcS` 会依次执行：

1. 建立基础目录并挂载 initramfs 的 `/dev`、`/proc` 和 `/sys`。
2. 等待 3 秒；期间收到 Ctrl+C 时直接进入 initramfs shell，不再运行测试。
3. 未收到 Ctrl+C 时，将 ext4 设备 `/dev/vd0` 挂载到 `/mnt`，并挂载测试盘需要的伪文件系统。
4. 通过 `chroot /mnt` 进入测试盘自身的 Debian 根环境，仅运行 glibc CAgent。
5. 测试结束后同步文件系统并关机；测试失败时保留非零状态用于串口诊断。

LoongArch64 启动器通过临时副本修正官方 CAgent 脚本会等待常驻 LLM 服务的问题，
只调整后台测试进程的等待和失败状态传播，不修改测试内容与超时。当前启动路径不执行
BuildStorm、LTP 或 musl；测试盘中的原始脚本不会被修改。RISC-V64 仍使用原有的磁盘
目录检查启动脚本。

## 构建

```sh
make busybox ARCH=loongarch64
make busybox ARCH=riscv64
make kernel-la
make kernel-rv
```

`make kernel-la` 和 `make kernel-rv` 会依次安装 BusyBox、架构配置、`elmctl` 以及
所选 ELM 镜像，生成 `build/<arch>/compat-initramfs.cpio`，再将其嵌入对应内核。
