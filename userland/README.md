# userland

本目录保存 BusyBox initramfs 骨架、架构相关 rootfs 配置和 ELM 用户态管理工具。
实际 rootfs 由根目录 Makefile 在 `build/<arch>/compat-rootfs` 中组装，不会把生成物
写回源目录。

## 目录结构

- `busybox-initramfs/`：两种架构共用的 BusyBox 配置骨架。
- `rootfs-rv/`：RISC-V64 的 `/etc` 配置覆盖层。
- `rootfs-la/`：LoongArch64 的 `/etc` 配置覆盖层。
- `elmctl/`：ELM 用户态管理工具及其固定布局控制面头文件。

## rcS 启动流程

`rootfs-{la,rv}/etc/init.d/rcS` 与 BusyBox 骨架中的启动脚本保持一致，依次执行：

1. 建立基础目录并挂载 `/dev`、`/proc` 和 `/sys`。
2. 扫描设备节点并将 ext4 设备 `/dev/vd0` 挂载到 `/mnt`。
3. 完整输出 `/mnt` 的排序目录树，同时记录其中的常规 `.sh` 文件。
4. 目录树输出结束后，按相同顺序打印各 `.sh` 文件内容。

启动脚本只检查磁盘内容，不执行磁盘中的脚本，也不会自动关机。脚本返回后由
BusyBox init 启动控制台 shell。

## 构建

```sh
make busybox ARCH=loongarch64
make busybox ARCH=riscv64
make kernel-la
make kernel-rv
```

`make kernel-la` 和 `make kernel-rv` 会依次安装 BusyBox、架构配置、`elmctl` 以及
所选 ELM 镜像，生成 `build/<arch>/compat-initramfs.cpio`，再将其嵌入对应内核。
