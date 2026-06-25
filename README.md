<div align="center">
  <img src="https://www.tyut.edu.cn/__local/C/F7/F9/0713FC3F036E6F49D48EA3B1504_80DE602B_A853.jpg" width="384" alt="太原理工大学" />

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

- 初赛技术文档：`doc/main.typ`
- 初赛技术报告：(待上传)
- 初赛安全分析报告：(待上传)
- 初赛 PPT: (待上传)

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
make kernel-la      # 构建 LoongArch64 内核，输出 ./kernel-la
make kernel-rv      # 构建 RISC-V64 内核，输出 ./kernel-rv
cargo fmt --all     # 格式化 workspace
```

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

## 备注

<div>
  <img src="docs/assets/preview.gif" alt="MyGO 预览 GIF" width="200" />
  <br />
</div>
