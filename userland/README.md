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

LoongArch64 的 `rootfs-la/etc/init.d/rcS` 会依次执行：

1. 建立基础目录并挂载 initramfs 的 `/dev`、`/proc` 和 `/sys`。
2. 将 ext4 设备 `/dev/vd0` 挂载到 `/mnt`，并在测试盘内挂载对应的伪文件系统。
3. 通过 `chroot /mnt` 进入测试盘自身的 Debian 根环境。
4. 忽略 musl，自动运行 `/glibc/cagent_testcode.sh` 和 `/glibc/buildstorm_testcode.sh`。
5. 输出两个测试的结果后返回控制台。

LoongArch64 启动器通过临时副本修正 cagent 脚本会等待常驻服务器的问题，并以可回收
整个进程组的 shell 超时器运行两个测试。buildstorm 工具链预检通过时保留原有的
14400 秒编译上限；预检已经失败时只保留 30 秒失败清理窗口，避免不可能成功的编译
永久阻塞 init。测试盘中的原始脚本不会被修改。RISC-V64 仍使用原有的磁盘目录检查
启动脚本。

## 构建

```sh
make busybox ARCH=loongarch64
make busybox ARCH=riscv64
make kernel-la
make kernel-rv
```

`make kernel-la` 和 `make kernel-rv` 会依次安装 BusyBox、架构配置、`elmctl` 以及
所选 ELM 镜像，生成 `build/<arch>/compat-initramfs.cpio`，再将其嵌入对应内核。
