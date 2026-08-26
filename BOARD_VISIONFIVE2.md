# VisionFive 2 开发板构建与验证指南

本文档覆盖 StarFive JH7110 VisionFive 2 的板级构建、OpenSBI/U-Boot 启动交接和
当前驱动验证范围。启动时必须使用与具体板卡 revision 匹配的固件 DTB，不要把 QEMU
virt DTB 用于实机。

当前 RISC-V 入口是 OpenSBI S-mode payload：`a0=hartid`、`a1=dtb_paddr`、
`satp=0`。U-Boot 必须按这个约定直接跳入 `_start`；内核没有 EFI 入口。

## 1. 构建

```sh
# --board 自动选择 riscv64 target 与 VisionFive 2 preset
cargo xtask modules --board visionfive2
cargo xtask build --board visionfive2

# 最终 ELF
# target/riscv64/visionfive2/riscv64gc-unknown-none-elf/release/kernel
```

板级默认配置是 `configs/visionfive2.config`。PLIC、DT provider、JH7110 CRG、
pinctrl、UART 和 MMC 启动链内建；TRNG、PMU、PCI host 与 syscon 保留为受管 ELM。
模块、Kernel API Profile 和 Cargo 缓存分别位于：

```text
build/riscv64/visionfive2/modules
build/elm-interface/riscv64/visionfive2
target/riscv64/visionfive2
```

显式的 `--config`、`--modules`、`--output` 和 `--target-dir` 仍可覆盖默认路径。
`--board visionfive2` 若与 LoongArch target 组合，`xtask` 会在构建前拒绝该命令。

## 2. 制作 U-Boot 镜像

规范 RISC-V 链接布局的物理装载地址和入口均为 `0x80200000`。最终 Cargo 产物是 ELF；
先导出 loadable segments，再封装成 U-Boot legacy image：

```sh
mkdir -p build/riscv64/visionfive2
llvm-objcopy -O binary \
  target/riscv64/visionfive2/riscv64gc-unknown-none-elf/release/kernel \
  build/riscv64/visionfive2/kernel.bin
mkimage -A riscv -O linux -T kernel -C none \
  -a 0x80200000 -e 0x80200000 -n "hitoshizuku-visionfive2" \
  -d build/riscv64/visionfive2/kernel.bin \
  build/riscv64/visionfive2/uImage
```

不要对 ELF 直接运行 `bootm`，也不要使用 `bootefi`。若本机 LLVM 工具带版本后缀，使用
对应的 `llvm-objcopy-<版本>`；转换后可用 `file` 和 `mkimage -l` 核对格式、装载地址和
入口地址。

## 3. 从 OpenSBI/U-Boot 启动

把 `uImage` 和板卡当前固件使用的 VisionFive 2 DTB 放入 TFTP 目录。在 U-Boot 中保留
OpenSBI 驻留，并显式把 DTB 作为第三个 `bootm` 参数传入：

```text
dhcp
tftpboot ${loadaddr} uImage
tftpboot ${fdt_addr_r} jh7110-starfive-visionfive-2.dtb
fdt addr ${fdt_addr_r}
bootm ${loadaddr} - ${fdt_addr_r}
```

实际 DTB 文件名随板卡 revision 和固件包变化；应沿用该板正常启动 Linux 时选中的 DTB。
`bootm` 会按 uImage 头把内核放到 `0x80200000`，不能把入口改成 U-Boot 常用的
`0x40200000` Linux Image 地址。当前早期页表和 RISC-V 链接布局要求物理 RAM 覆盖
`0x80200000`。

## 4. 验证点

启动日志至少应依次证明以下路径已经工作：

| 子系统 | 期望结果 |
| --- | --- |
| OpenSBI/loader | `[loader] RISC-V64 boot` 后显示有效 DTB 地址和大小 |
| DTB/内存 | 解析 `/memory`、`/reserved-memory`、`/chosen/stdout-path` 和 CPU ISA |
| PLIC | `platform.plic` 完成 JH7110 中断域注册，设备 IRQ 可申请 |
| CRG | `[jh7110-crg] registered clock+reset` |
| pinctrl | `[jh7110-pinctrl] ... registered`，默认 pin state 可取得 |
| UART | `[jh7110-uart] bound ... -> /dev/uartN`，轮询和 RX IRQ 均可用 |
| MMC | `[jh7110-mmc] bound ... -> /dev/mmcblkN`，只读识别容量正确 |
| TRNG | 加载受管模块后出现 `[jh7110-trng] bound ... credited=256 bits` |

根文件系统和 initramfs 由仓库外部提供。仅看到驱动绑定不代表介质写路径已验证；MMC
目前只支持单块 CMD17/CMD24，首次实机回归应先只读核对分区和边界 LBA，再在可恢复介质
上单独测试写入。

## 5. 当前边界

- VF2 使用 PLIC；本板 preset 不依赖 RISC-V AIA/IMSIC。
- JH7110 MMC 的 IDMAC 仍受 32 位地址和单描述符 bounce buffer 限制，不支持 UHS、
  HS200、热插拔或多块传输。
- JH7110 UART 面向控制台，不实现 TX DMA、硬件流控或高吞吐 TX IRQ。
- 当前没有完整的 JH7110 PCIe PHY、USB、以太网和显示链；`pci-host-ecam=m` 只保留
  通用 host bridge，不能代表整条板级 PCIe 链已经可用。
- QEMU 与 VisionFive 2 共享 RISC-V 入口代码，但配置、模块输出、接口快照和 Cargo 缓存
  已按板卡隔离，禁止交叉复用清单。
