# userland

多架构 rootfs 源目录，构建时由 `build.rs` 分别打包为各架构的 initramfs 镜像。

## 目录结构

- `rootfs-rv/`: RISC-V64 架构的 rootfs，使用 musl 工具链编译的用户态程序
- `rootfs-la/`: LoongArch64 架构的 rootfs，使用 glibc 工具链编译的用户态程序

每个 rootfs 目录的典型布局：

```
rootfs-{arch}/
├── bin/           # BusyBox 及 symlink（由 Makefile 自动安装）
├── etc/
│   ├── init.d/
│   │   └── rcS    # 启动脚本，内核挂载 rootfs 后由 init 执行
│   └── ltp-scenarios/  # LTP 测试用例场景文件
└── ...
```

## rcS 启动脚本

`etc/init.d/rcS` 是用户态入口脚本。内核启动 init 进程后执行此脚本，负责：

1. 挂载 `/proc`、`/sys`、`/dev` 等虚拟文件系统
2. 根据评测环境配置网络、hostname
3. 按序执行各测试组（basic、busybox、ltp 等）
4. 支持黑名单机制跳过特定测试用例

## LTP 场景文件

`etc/ltp-scenarios/` 目录存放 LTP 测试的场景定义文件，每个文件对应一个 LTP 测试组。构建时由 Makefile 的 `rootfs-ltp-scenarios-{la,rv}` 目标从 `userland/ltp-scenarios/` 同步。

## 构建

rootfs 由 Makefile 自动构建，无需手动操作：

```sh
make rootfs-la   # 构建 LoongArch64 rootfs
make rootfs-rv   # 构建 RISC-V64 rootfs
```

内核构建时 `build.rs` 会将对应的 rootfs 目录打包为 cpio 归档嵌入内核镜像。
