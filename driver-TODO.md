# driver-TODO: VisionFive 2 (VF2) 驱动支持交接和代办

> 状态更新：2026-08-15 深夜。**真机用户态启动验证通过**：MyGO 原生引导 → 84 设备
> PnP 发现 → eMMC 就绪（/dev/mmcblk0）→ initramfs 解包 → 静态 ELF /init →
> 用户态 write 系统调用输出「MYGO USERSPACE RUNNING」（串口可见）。
> 引导链、驱动绑定、块读写、用户态切换均已实机验证；剩余为增强项（见第四节）。

## 一、已完成

### 1. 构建与注册
- `drivers/Modules.toml`：jh7110 全部模块登记（uart/crg/pinctrl/mmc/designware-i2c=y；
  pl022/dwmac/cdns-usb3=m）；
- `Cargo.toml` workspace exclude 补 8 个驱动目录；
- 交叉工具链：riscv64 用 clang + 从 vf2-binhost 容器提取的 GNU binutils（tools-cross/，
  已 gitignore）；`make ARCH=riscv64` 全量构建通过（16 线程 ~3 分钟）。

### 2. 驱动实现（drivers/）
- **platform.jh7110-crg**：完整 clock provider（#clock-cells=1 按 ID 查表，50 项速率
  来自板载 Debian 6.12 clk_summary 实测：OSC=24MHz、UART0_CORE(146)=24MHz、
  APB=49.5MHz、SDIO biu(91/92)=198MHz、ciu(93/94)=49.5MHz、GMAC=198MHz）+
  no-op reset provider；支持 clock-frequency 覆盖 OSC（测试用）。
- **platform.jh7110-uart**：完整 DW_APB_UART 控制台驱动（reg-shift/io-width 解析、
  CRG baudclk 协商、pinctrl-0 默认态应用、PLIC RX 中断、/dev/uartN console、
  SPECIFIC 优先级）；uart16550 移除 jh7110/dw-apb compatible 消除 DriverAmbiguous。
- **platform.jh7110-pinctrl**：真实 pinmux/padcfg 应用（din/dout/doen/function/pin
  编码；DOUT=0x40/DOEN=0x0/GPI=0x80 寄存器；0x29c-0x2b0 功能选择表；
  0x120/0x284 padcfg 基址）；每个 pin 配置子节点注册独立 Pinctrl provider。
- **platform.jh7110-mmc**：DesignWare MSHC 块设备驱动——复位/FIFO 阈值/时钟分频、
  SD（CMD0/8/55/41/2/3/9/7 + ACMD6 4bit）与 eMMC（CMD0/1/2/3/9/7）卡初始化、
  PIO 单块读写（CMD17/24，BLKSIZ=512）、注册 /dev/mmcblkN（整盘，无分区扫描）；
  超时基于 rdtime（CLINT mtime，VF2 为 4MHz），不依赖调度器时钟。
- **bus.designware-i2c / arm-pl022 / dwmac / cdns-usb3**：ELM 脚手架 + 真实兼容串
  匹配的最小 PnP 驱动（probe 记录 MMIO/IRQ）。

### 3. 架构层（arch/、kernel/）
- `arch/src/loongarch64/early_console_config.rs`：早期控制台 JH7110 syscrg 时钟
  内建查表（UART0_CORE=24MHz → 115200 波特率正确）+ clock-frequency 覆盖；
- `kernel/src/panic.rs`：cmdline `mygo.reboot=1` → panic 时热重启（实机一次性测试
  用）。注意：本板热重启疑似挂死（Debian reboot 也挂，见下文），最终方案是
  mygo.once 一次性标记 + 断电重启恢复。

## 二、验证结果

### QEMU 模拟（已通过，可复现，含用户态启动）
除基础驱动绑定外，QEMU 已复现完整用户态启动链路（含 initramfs + /init
静态 ELF + write 系统调用输出「MYGO USERSPACE RUNNING」）：

```bash
# 1) 生成带 initrd 属性的测试 DTB（initrd 装入 QEMU RAM 0x88000000）
#    build/vf2-sim-initrd.dts 已就绪（linux,initrd-start/end 指向 0x88000000）
cd build && dtc -I dts -O dtb vf2-sim-initrd.dts -o vf2-sim-initrd.dtb
# 2) 打包 initrd（host cpio，/init 为打印版静态 ELF）
(cd build/mygo-initramfs/cpio-root && find . -print0 | cpio --quiet -o -0 -H newc) \
  > build/mygo-initramfs/initramfs-real.cpio
# 3) 运行
timeout 90 qemu-system-riscv64 -machine virt,dtb=build/vf2-sim-initrd.dtb \
  -kernel build/riscv64/kernel -m 1G -nographic -smp 1 -no-reboot \
  -device loader,file=build/mygo-initramfs/initramfs-real.cpio,addr=0x88000000
# 预期最后输出：MYGO USERSPACE RUNNING
```
`qemu-system-riscv64 -machine virt,dtb=build/vf2-sim.dtb -kernel build/riscv64/kernel
-m 1G -nographic`（vf2-sim.dtb 由 QEMU virt DTB 手工加 jh7110 节点生成，见
build/vf2-sim.dts）：

```text
[jh7110-crg] registered clock+reset phandle=0x5 rates=50
[jh7110-pinctrl] sys registered phandle=None states=1
[jh7110-uart] bound serial@10000000 clock=3686400 -> /dev/uart0
[platform-jh7110-mmc] probe mmc@16010000 irq=Controller{hwirq:74}
[bus-designware-i2c] probe i2c@10030000
（无卡环境下 MMC 优雅失败：card init failed: NoCard）
```
基线回归：默认 QEMU DTB 下 uart16550 绑定 ns16550a 正常（无回归）。

### 实机（已全流程验证，见下方最新状态）
**引导方式（最终版）**：StarFive U-Boot 的完整 `bootm` 不会跳转到 riscv 内核
（官方流程只用 `bootm start/loados` 拆包 + `booti` 跳转）；`go` 在 riscv 上传的是
(argc, argv) 而非寄存器参数。因此采用官方同款：fatload 直载 + `booti`：

```text
fatload mmc 1:3 0x42000000 mygo.img      # 64B Image 头 + 裸内核
fatload mmc 1:3 0x46100000 mygo-initrd.cpio
setenv initrd_size ${filesize}
fatload mmc 1:3 0x46000000 mygo-vf2.dtb
booti 0x42000000 0x46100000:${initrd_size} 0x46000000
```

Image 头要点（booti 强制重定位到 ram_base+text_offset）：text_offset=0x401FFFC0
→ 内核精确落位链接基址 0x80200000；魔数同时写 0x20/0x28/0x38（新旧格式双兼容）；
code0 = `jal +64` 跳过头部。生成脚本见 build/mygo-fit/ 下 python 片段。

部署物（/boot）：mygo.img（11.98MB）、mygo-vf2.dtb、mygo-initrd.cpio（最小
ELF /init）、uEnv.txt（含 booti 流程，≤512 字节规避 env import 截断）。

板子实况：U-Boot 2021.10（StarFive SDK，bootdelay 3s，distro boot 从 mmc1 FAT 分区
导入 uEnv.txt）；OpenSBI v1.2（SBI 1.0，timer 4MHz，console uart8250，reboot/shutdown
= pm-reset）；Debian 6.12.5-starfive；硬件看门狗 StarFive Watchdog 存在。

已确认的现象：**本板 Debian 的 reboot 会在 "Restarting system" 后挂死**（无 SPL 输出，
看门狗 3+ 分钟未触发复位），怀疑 SBI srst/pm-reset 热重启路径不可用。因此：
- MyGO 的 panic→reboot（mygo.reboot=1）预计也会挂，测试后需断电重启恢复；
- mygo.once 已在上次会话创建（touch /boot/mygo.once），板子下一次上电即自动引导
  MyGO 一次（U-Boot bootcmd：test -e → fatrm → fatload+DTB → booti）。

## 三、实机操作手册（当前可用路径）

触发 MyGO 引导（三选一）：
```text
方式 A（已就绪）：板子断电重启一次即可——mygo.once 仍在 /boot，
  U-Boot 会自动 fatrm 它并引导 MyGO，之后正常进 Debian。
方式 B（需要 Debian 登录，串口或 SSH）：sudo touch /boot/mygo.once && sudo poweroff
  （不要用 reboot，本板热重启挂死；用 poweroff 后手动上电）。
方式 C（U-Boot 手动，串口 3 秒内按键）：
  fatload mmc 1:3 0x80200000 mygo-Image
  fatload mmc 1:3 0x46000000 mygo-vf2.dtb
  setenv bootargs mygo.reboot=1 console=ttyS0,115200 earlycon
  booti 0x80200000 - 0x46000000
```

抓日志（宿主机）：
```bash
# CH340 串口（pin6=GND, pin8=板TX↔转接器RX, pin10=板RX↔转接器TX, 115200 8N1）
# 宿主机若无 /dev/ttyUSB0：重插 USB 或 sudo sh -c 'echo 3-9 > /sys/bus/usb/drivers/usb/unbind; echo 3-9 > /sys/bus/usb/drivers/ch341/bind'
docker run --rm --device=/dev/ttyUSB0 debian:trixie bash -c 'stty -F /dev/ttyUSB0 115200 raw -echo clocal; cat /dev/ttyUSB0'
```

预期实机日志关键行（时钟为真实 24MHz）：
```text
R
[loader] early console from DTB: base=0x10000000 clock=24000000 baud=115200 ...
[jh7110-crg] registered clock+reset phandle=0x3 rates=50
[jh7110-pinctrl] sys registered ... states=N
[jh7110-uart] bound ... clock=24000000 -> /dev/uart0
[jh7110-mmc] bound ... blocks=... -> /dev/mmcblk0   ← eMMC 识别成功
[panic] ...（挂死于此或 reboot 挂死，断电恢复）
```

回滚：cp /boot/uEnv.txt.bak-mygo /boot/uEnv.txt（恢复原引导）；
或 U-Boot 里 setenv bootcmd run bootcmd_distro; saveenv 永久绕过。

## 四、剩余工作（按优先级）
1. **确认用户态启动**：initramfs 已解包、/init 为静态 ELF（当前为打印版
   `MYGO USERSPACE RUNNING` + 自旋）；待下轮上电看 sched 直写打点定位
   「加载挂起 vs 已进用户态」；
2. **console sink 丢失问题**：控制台绑定后 log 消息不落串口（panic-direct 直写
   可见、printk 可见、log sink 不可见）；排查 console_write→UART write_all 路径
   （怀疑 LSR 轮询或 TX 环缓冲停摆）；用户态 stdout 输出也走此路径；
3. 分区扫描缺失：/dev/mmcblk0 为整盘设备，Debian 根在分区 4（LBA 221184）；
   需 MBR/GPT 分区层后才能挂 ext4 真根（当前用 initramfs 根）；
4. MMC 实机写验证 + 速度：PIO 单块读约 5s/块（调试构建 + 12.4MHz 时钟），
   后续做多块读 + 满速 49.5MHz；
5. 2 个 DriverAmbiguous 节点待定位（已加 pnp 诊断打印，下轮上电可见）；
6. SD 卡槽（16010000）空槽 CMD8 假响应（sd_v2=true 后 NoCard）——非致命，待查；
7. **bus.dwmac 完整实现**：stmmac(DWMAC4/5, Synopsys ID 0x52) + DMA ring +
   MDIO/YT8531 PHY + NetQueuePair 对接 net.stack（参考 virtio-net 与
   tools-cross/refs/dwmac-starfive.c）；板子 eth0/eth1 MAC 在 EEPROM：
   6c:cf:39:00:79:c2 / c3；
8. designware-i2c 完整实现（寄存器模式 + dtb_owned_nodes 子设备枚举）；
9. cdns-usb3 / pl022 骨架保持或按需实现；AON 域 pinctrl/CRG 细化。

## 六、TF 卡完整镜像方案（待新卡到位）

VF2 的 U-Boot distro boot 会扫描 SD 卡（mmc0）的 extlinux/extlinux.conf。
我们的 mygo.img 已带 RISCV Image 头（booti 兼容），可直接用 sysboot 引导：

```text
# TF 卡 FAT 分区文件布局
/extlinux/extlinux.conf
/mygo.img            # 64B Image 头 + 裸内核（链接 0x80200000）
/mygo-vf2.dtb
/mygo-initrd.cpio    # busybox 版（见下方 shell 计划）
```

```text
# extlinux.conf
label mygo
    kernel /mygo.img
    initrd /mygo-initrd.cpio
    fdt /mygo-vf2.dtb
    append console=ttyS0,115200 earlycon mygo.reboot=1
```

sysboot 的 append 会写入 bootargs 并经 booti 的 fdt_chosen 注入 DTB——与当前
uEnv.txt 手工流程等价。SD 镜像可在宿主机 mkfs.vfat + 拷贝生成，再 dd 到 TF 卡。

shell 目标（板载原生 musl 交叉编译，无需外部工具链）：
```bash
# 在板子 Debian 上：
sudo apt-get install -y musl-tools cpio make
# scp third/busybox-1.36.1 到板子后：
make CC=musl-gcc defconfig && sed -i 's/.*CONFIG_STATIC.*/CONFIG_STATIC=y/' .config
make CC=musl-gcc -j4 && make CC=musl-gcc CONFIG_PREFIX=rootfs install
# rootfs 合并 userland/busybox-initramfs 骨架（inittab: console::askfirst:-/bin/sh）
# ln -sf bin/busybox rootfs/init; 打包 newc cpio
```

## 五、构建与测试速查
```bash
PATH=$PWD/tools-cross/bin:$PATH make ARCH=riscv64        # 全量构建
PATH=$PWD/tools-cross/bin:$PATH riscv64-linux-gnu-objcopy -O binary \
    build/riscv64/kernel build/riscv64/kernel.bin          # 裸镜像（实机用）
qemu-system-riscv64 -machine virt -kernel build/riscv64/kernel -m 1G -nographic  # 基线
```

