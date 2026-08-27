# LS2K1000LA 开发板驱动验证指南

本文档描述 2K1000LA 开发板（BPI1001 / LS2K1000-DP-FACTORY）全驱动
验证流程：构建板级内核镜像、制作 legacy uImage、经 TFTP 加载到 fork
U-Boot，以及逐驱动核对启动日志。

该板启动链固定为 **fork U-Boot 直接交接 DTB 并跳入内核 `_start`**。内核不包含
EFI stub、PE/COFF 头或 EFI 入口，不应使用 `bootefi`；EFI 数据结构支持也不改变这条
启动边界。

## 1. 构建

```sh
# --board 自动选择 loongarch64 target、板级配置与 0x200000 链接布局。
# image 默认完成模块、内核构建以及 ELF/raw/uImage 发布。
cargo xtask image --board ls2k1000

# 可部署产物
# build/loongarch64/ls2k1000/kernel.elf
# build/loongarch64/ls2k1000/kernel.bin
# build/loongarch64/ls2k1000/uImage
```

板级默认配置是 `configs/ls2k1000.config`；模块、Kernel API Profile 和 Cargo 缓存
分别隔离在 `build/loongarch64/ls2k1000/modules`、
`build/elm-interface/loongarch64/ls2k1000` 和 `target/loongarch64/ls2k1000`。
需要临时调整配置时显式传 `--config <path>`，不要改写板级 preset。

Cargo 内部产物始终是未剥离 ELF；板级 raw 和 uImage 只能由 `xtask image` 从同一次
构建的 ELF 派生。QEMU 验证必须另建 QEMU 平台 ELF，不能复用物理板的
`0x200000` 布局：

```sh
cargo xtask image --board qemu \
  --target loongarch64-unknown-none
qemu-system-loongarch64 -machine virt -cpu la464 -m 1G -nographic \
  -kernel build/loongarch64/kernel.elf -no-reboot
```

预期：`handoff probe` 两条探测日志 → `DTB describes /memory` →
`total_ram=1024 MiB`，随后进入设备发现和根文件系统选择。缺少外部 rootfs 或
initramfs 是部署输入不完整，不再把固定的 `boot_root` panic 视为成功终态。

## 2. 制作 legacy uImage 并烧录/加载

`xtask image` 使用 `llvm-objcopy` 导出 loadable segments，再调用与板载 fork 匹配的
`mkimage` 生成 legacy image。LoongArch U-Boot fork 使用 `loongarch` 架构 token；若
系统 PATH 中的工具不支持该 token，必须显式提供板载 fork 对应的工具：

```sh
cargo xtask image --board ls2k1000 \
  --objcopy /usr/bin/llvm-objcopy \
  --mkimage /path/to/loongarch-mkimage

# TFTP 服务器把 uImage 与工厂 DTB（/tmp/board-fdt.dtb）放到同一目录
# （例如 /srv/tftp/）
cp build/loongarch64/ls2k1000/uImage /srv/tftp/uImage
cp /tmp/board-fdt.dtb /srv/tftp/mygo.dtb
```

板侧 U-Boot（进入 U-Boot 后）：

```
# 一次性配置：bootm 必须显式传 fdt 参数（无参 bootm 会传 bd_info，
# 内核会因 "no device tree in handoff" panic 并提示）
setenv bootcmd 'dhcp; tftp ${loadaddr} uImage; tftp ${fdt_addr} mygo.dtb; bootm ${loadaddr} - ${fdt_addr}'
saveenv
boot
```

`fdt_addr`/`loadaddr` 使用板子既有环境值（实测
fdt_addr=0x900000000a000000，loadaddr=0x9000000098000000）。

## 3. 预期启动日志（逐驱动核对）

| 驱动 | 期望日志（前缀） | 说明 |
| --- | --- | --- |
| loader/DTB | `[loader] handoff probe ... a1_is_fdt=true`；`[loader][dtb] DTB copied ...`；`DTB has no /memory; using 2K1000 board memory: 2 regions (2048 MiB)` | 工厂 DTB 无 /memory，走板级内存回退，`total_ram` 应 ≈1885 MiB |
| loongson-irq/clk/pinctrl | `[loongson-*]` 绑定 2k1000-icu、ls2x-clk、pinctrl/gpio | |
| uart16550 | `[platform-uart16550] bound ... serial@1fe20000 ... -> /dev/uart0` ×12 | ttyS0 alias |
| ls2k-rtc | `[platform-ls2k-rtc] installed realtime source ... phys=0x1fe27800 unix_ns=...` | TOY 时间 |
| ls2x-wdt | `[platform-ls2x-wdt] bound ... clk=... Hz max_timeout=34s` | |
| ls2k-gmac | `[ls2k-gmac] bound ... mac=02:... speed=1000Mbps full`；`phy at 0: PHYIDR1=0x0000 PHYIDR2=0x0136` | YT8511；`ethernet0/1` alias → eth0/eth1 |
| ls2k-spi | `[ls2k-spi] bound controller ...`；`bound spi-nor ... (jedec ef 40 17) size=8388608` | w25q64 8MB |
| ls2k-i2c | `[ls2k-i2c] bound ...`（i2c@1fe21800） | i2c0 disabled 不出现 |
| ls2k-usb | `[ls2k-usb] bound ... kind=Ehci/Ohci/Dwc2`；插入设备后 `enumerated device ... <vid>:<pid>` | 插 U 盘/键鼠验证枚举 |
| ls2k-tsensor | `[ls2k-tsensor] bound ... temp=... m°C thresholds=60..95°C` | |
| ahci | `[platform-ahci] ...` sata 盘挂载 rootfs | 板子 rootfs 所在盘 |
| pci-host-ecam | `[pci-host-ecam] ... /pcie@0 ...` | 2K1000 PCI 主桥 |

## 4. 未实现项（工厂 DTB 为 disabled，驱动无绑定目标）

- `nand@1fe26040`（loongson,ls-nand）：status="disabled"；
- `pwm@1fe22000/1fe23000/1fe24000/1fe25000`（ls2k-pwm）：全部
  status="disabled"；
- `pcie-msi-controller@1fe014a0`（loongson,2k1000-pci-msi）：
  status="disabled"；
- 显示 DC / 音频 / CAN（sja1000）等：计划范围外。

## 5. 已知联调点

- GMAC RGMII 延迟：DTB 无延迟属性，驱动保持硬件默认；若链路不协商
  成功，需在 `phy_bringup()` 按 YT8511 页寄存器调整 rx/tx delay。
- WDT 寄存器布局：若使能后立即复位，说明 EN/TMR 位序与预期相反，
  按真机行为调整 `regs` 常量（启动测试建议先 `set_timeout` 后
  `start`，观察是否按时复位）。
- USB 枚举在进程上下文同步轮询：高延迟设备可能超时，真机按
  `CHANNEL_TIMEOUT_LOOPS` / `TRANSFER_TIMEOUT_LOOPS` 微调。
