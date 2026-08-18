# LS2K1000LA 开发板驱动验证指南

本文档描述 2K1000LA 开发板（BPI1001 / LS2K1000-DP-FACTORY）全驱动
验证流程：构建板级内核镜像、制作 legacy uImage、经 TFTP 加载到 fork
U-Boot，以及逐驱动核对启动日志。

## 1. 构建

```sh
# 板级二进制（fork U-Boot bootm 传统镜像用，装载于物理 0x00200000，
# 对应 DMW1 地址 0x9000000000200000）
make ARCH=loongarch64 MYGO_LA_BOARD=ls2k1000
# 产物：build/loongarch64/kernel

# QEMU 验证用 ELF（QEMU loongarch64 -kernel 只接受 ELF）
make ARCH=loongarch64 MYGO_LA_DEBUG_LINKER=1
```

QEMU 启动验证：

```sh
qemu-system-loongarch64 -machine virt -cpu la464 -m 1G -nographic \
  -kernel target/loongarch64-unknown-none/release/kernel -no-reboot
```

预期：`handoff probe` 显示 EFI 配置表中的 DTB → `DTB describes /memory` →
`total_ram=1024 MiB`；裸内核最后在 boot_root 因无根文件系统 panic
属预期终态。

## 2. 制作 legacy uImage 并烧录/加载

使用 fork U-Boot 的 mkimage（/tmp/uboot-full，v2025.04）：

```sh
# 板级内核已按 0x200000 链接（ls2k1000.ld 由 qemu-loongarch64.ld 在
# kernel/build.rs 生成），传统镜像入口即内核入口
/tmp/uboot-full/tools/mkimage -A loongarch64 -O linux -T kernel \
  -C none -a 0x200000 -e 0x200000 -n "mygo-ls2k1000" \
  -d build/loongarch64/kernel uImage

# TFTP 服务器把 uImage 与工厂 DTB（/tmp/board-fdt.dtb）放到同一目录
# （例如 /srv/tftp/）
cp /tmp/board-fdt.dtb /srv/tftp/mygo.dtb
```

板侧 U-Boot（进入 U-Boot 后）：

```
# 厂商 U-Boot 从环境变量 fdt_addr 取得内核 DTB。先把 DTB 加载到该地址，
# 再执行普通 bootm；该版本的 `bootm kernel - fdt` 交接不可靠。
setenv bootcmd 'dhcp; tftp ${loadaddr} uImage; tftp ${fdt_addr} mygo.dtb; bootm ${loadaddr}'
saveenv
boot
```

`fdt_addr`/`loadaddr` 使用板子既有环境值（实测
fdt_addr=0x900000000a000000，loadaddr=0x9000000098000000）。

若从 SPI flash 启动，等价的关键顺序是：

```
sf probe
sf read ${fdt_addr} dtb
bootm ${loadaddr}
```

loader 会自动识别固件交接，优先级为 EFI 配置表、Loongson legacy `$a3`、
UHI `$a1`、兼容 `$a2`，都无有效 FDT 时才使用内置板级 DTB。厂商 U-Boot 的
`fdtcontroladdr` 是它自身使用的控制设备树，不保证适合作为内核设备树，因此
内核不会扫描任意物理内存或自动采用该地址。

## 3. 预期启动日志（逐驱动核对）

| 驱动 | 期望日志（前缀） | 说明 |
| --- | --- | --- |
| loader/DTB | 新 ABI：`EFI system table address accepted ... via=a2`；legacy ABI：`bpi_handoff=true ... a3_is_fdt=true`；随后出现 `[loader][dtb] DTB copied ...`，且不出现 `builtin board DTB copied` | 工厂 DTB 无 /memory，随后走板级内存回退，`total_ram` 应 ≈1885 MiB |
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
- USB 枚举在 IRQ 上下文同步轮询：高延迟设备可能超时，真机按
  `CHANNEL_TIMEOUT_LOOPS` / `TRANSFER_TIMEOUT_LOOPS` 微调。
