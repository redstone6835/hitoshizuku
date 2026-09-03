# KCSAN 辅助脚本

内核仓库只保留 KCSAN 编译包装器、符号定位器和对应的代码生成测试。性能画像、QEMU
插件、系统调用比较和机器学习模型位于独立的
[`hitoshizuku-bench`](https://github.com/redstone6835/hitoshizuku-bench) 仓库。

脚本从内核仓库根目录运行，并使用同一次构建生成的 map/符号文件。

```sh
scripts/test-kcsan-codegen.sh
python3 -m unittest scripts.tests.test_kcsan_symbolize
```

## x86_64 Alpine 镜像

内核 ELF 是 Multiboot2 高半区镜像，不能直接作为 QEMU 的 Linux `-kernel` 输入。
下面的脚本从 Alpine 官方 `x86_64` minirootfs（默认固定为 3.24.1）安装完整的开发/服务器
用户空间，创建 root-owned ext4 根盘，并生成只包含 GRUB2 + 内核 ELF 的启动 ISO：

```sh
cargo xtask image --platform qemu-x86_64 --format elf
scripts/build-alpine-x86_64.sh
scripts/run-alpine-x86_64.sh
```

输出位于 `build/alpine/alpine-x86_64/`、`build/alpine/alpine-x86_64.img` 和
`build/x86_64/alpine-boot.iso`。其中 ext4 磁盘镜像是运行时根文件系统的权威产物；目录树
保留了宿主 user namespace 的 subordinate-ID 映射，只用于检查内容，不能直接 chroot，
也不能作为打包输入。启动 ISO 只包含 i386-pc BIOS GRUB2 和内核 ELF，不生成 EFI 启动项。
默认磁盘是 GPT 单分区，GRUB 的命令行使用内核设备名
`console=uart0 root=/dev/vd0p1`。Alpine 自带的 BusyBox init 作为 PID 1，依次启动
OpenRC 的 sysinit、boot 和 default runlevel（默认启用 networking、sshd）。网络接口地址
由 Hitoshizuku 内核网络运行时自动配置，`networking` 只负责 loopback；镜像仍包含
`dhcpcd`、`iproute2`、`tcpdump` 等工具，便于在 shell 中手动调试。串口 shell 位于
`/dev/uart0`。

运行脚本把串口接到本地 Unix socket，并由 `scripts/qemu-serial-proxy.py` 转发到
宿主终端。默认维护 shell 是 Bash，因此普通 `stdio`/raw 串口不需要光标位置查询；
若设置 `HITOSHIZUKU_SHELL=ash`，代理会自动回答 BusyBox ash 的 `ESC[6n` 查询。
宿主输入以 raw 模式转发，所以普通文本和 Ctrl+C（来宾收到 `0x03`）都可直接使用。
终端窗口大小会用于生成光标位置响应；可用 `HITOSHIZUKU_SERIAL_TMPDIR` 指定 socket
临时目录。

脚本使用 subordinate UID/GID 映射和无特权 user/mount namespace，保留 Alpine 包的
owner/group/setuid 元数据，不需要 root 或 loop mount；宿主必须提供 `unshare`、
`newuidmap`/`newgidmap` 和 `/etc/subuid`/`/etc/subgid` 映射。minirootfs 固定 SHA-256，
本次解析到的完整包版本写入镜像的 `/etc/hitoshizuku-packages`；Alpine 分支仓库会持续更新，
因此该包清单是构建结果记录，并不表示磁盘镜像可逐字节复现。可用
`ALPINE_PACKAGES="..."`、`ALPINE_IMAGE_SIZE=2G`、
`ALPINE_PARTITIONED=0 ALPINE_ROOT_DEVICE=/dev/vd0` 选择无分区 raw 镜像，或用
`ALPINE_VERSION`/`ALPINE_SHA256` 覆盖默认配方；非默认版本必须显式提供 SHA-256。
