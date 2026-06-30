<div align="center">
  <img src="docs/assets/tyut-logo.jpg" width="384" alt="太原理工大学" />

</div>

# MyGO!!!!! OS

<div align="center">
  <p>
    面向多架构的高可移植性的操作系统内核。
  </p>

  <p>
    <img alt="language" src="https://img.shields.io/badge/Rust%20%2B%20C-2024-b7410e?style=for-the-badge&logo=rust&logoColor=white" />
    <img alt="targets" src="https://img.shields.io/badge/Targets-LA64%20%7C%20RV64-0b4ea2?style=for-the-badge" />
    <img alt="kernel" src="https://img.shields.io/badge/Kernel-MyGO!!!!!-1f2937?style=for-the-badge" />
  </p>
</div>

## 项目速览

本项目是 2026 年全国大学生计算机系统能力大赛操作系统设计赛内核赛道的参赛作品，实现上是一个宏内核，主体使用 Rust 编写，并在 EFI / 启动链路中包含少量 C 代码。

| 方向 | 内容 |
| --- | --- |
| 内核语言 | Rust 2024 + C |
| 支持架构 | LoongArch64, RISC-V64 |
| 启动环境 | QEMU virt / contest image |
| 用户态 | glibc / musl 测试镜像 |
| 重点能力 | ELF 加载、虚拟内存、调度、VFS、ext4、socket、virtio、RTC、IRQ |

## 文档

- 初赛技术文档：[`docs/main.typ`](docs/main.typ)
- 架构说明：[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- 安全分析报告：[`docs/SECURITY_REPORT.md`](docs/SECURITY_REPORT.md)
- 开发与贡献说明：[`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md)
- 文档样式约定：[`docs/STYLES.md`](docs/STYLES.md)

## 代码目录地图

```text
.
├── arch/       # 架构相关代码：LoongArch64 / RISC-V64 入口、陷入、页表等
├── kernel/     # syscall、exec、调度、进程与内核主路径
├── general/    # 设备、内存、VFS 投影、平台驱动等通用内核设施
├── libs/       # vfs、net、socket、extfs、sched、allocator 等共享库
├── hal/        # 平台抽象层
├── userland/   # initramfs、rcS、测试入口脚本
├── third/      # 外部组件
└── vendor/     # 离线 Cargo 依赖镜像
```

## 快速启动

所有构建与运行建议放在评测镜像中执行：

```sh
docker run --rm -it -v "$PWD":/work -w /work zhouzhouyi/os-contest:20260510 bash
```

进入容器后：

```sh
make all            # 构建 LoongArch64 和 RISC-V64 内核
make kernel-la      # 构建 LoongArch64 内核，输出 ./kernel-la
make kernel-rv      # 构建 RISC-V64 内核，输出 ./kernel-rv
cargo fmt --all     # 格式化 workspace
```

构建目标会自动将可提交的 `cargo-config/` 同步为本地 `.cargo/`，并把用户态
initramfs 打包进内核镜像。最终输出文件为仓库根目录下的 `kernel-la` 和
`kernel-rv`。

QEMU 运行示例默认使用 `build/sdcard-la.img` 和 `build/sdcard-rv.img`。
这些镜像由比赛评测环境提供；本地复现时可从评测数据包中的对应压缩镜像解压到
`build/` 目录后运行。

## QEMU 运行示例

LoongArch64：

```sh
qemu-system-loongarch64 -kernel kernel-la -m 1G -nographic -smp 1 \
  -drive file=./build/sdcard-la.img,if=none,format=raw,id=x0 \
  -device virtio-blk-pci,drive=x0 -no-reboot \
  -device virtio-net-pci,netdev=net0 -netdev user,id=net0 -rtc base=utc
```

RISC-V64：

```sh
qemu-system-riscv64 -machine virt -kernel kernel-rv -m 1G -nographic -smp 1 \
  -drive file=./build/sdcard-rv.img,if=none,format=raw,id=x0 \
  -device virtio-blk-device,drive=x0 -no-reboot \
  -device virtio-net-device,netdev=net0 -netdev user,id=net0 -rtc base=utc
```

## 测试入口

| 类型 | 命令 |
| --- | --- |
| socket 单测 | `cargo test -p socket` |
| extfs 单测 | `cargo test -p extfs` |
| 内核启动验证 | 使用上方 QEMU 命令启动目标架构 |
| 用户态集成测试 | 由 `userland/rootfs-*/etc/init.d/rcS` 按测试镜像脚本触发 |

## 第三方组件与参考来源

本仓库包含离线 Cargo 依赖镜像和若干外部组件，主要位于 `vendor/`、`third/`、
`libs/mygo-smoltcp/` 和 `libs/acpi/`。其中网络协议栈、ACPI 解析、BusyBox
用户态工具链及 Rust 生态依赖均按其原始许可证保留来源信息。

MyGO!!!!! OS 的主要工作集中在内核架构分层、多架构启动适配、系统调用兼容层、
任务调度、虚拟内存、VFS 投影、设备模型、virtio 块/网卡接入、测试镜像集成和
比赛测例适配等部分。更详细的来源、差异和创新点说明见
[`docs/main.typ`](docs/main.typ) 及 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。

## 备注

<div>
  <img src="docs/assets/preview.gif" alt="MyGO 预览 GIF" width="200" />
  <br />
</div>
