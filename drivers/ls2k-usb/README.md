# Loongson LS2K1000 USB host 驱动

`platform.ls2k-usb` 提供 LS2K1000 DWC2、EHCI 与 OHCI host controller，实现首次端口扫描，
并为枚举出的 device/interface 创建 USB PnP 设备。

## 实现范围

- 匹配 `loongson,loongson2-dwc2`、`loongson,ls2k-ehci` 和 `loongson,ls2k-ohci`。
- 三个 HCD 提供端口上电/复位，以及同步 control、bulk、interrupt transfer；DWC2 使用 host
  channel，EHCI 使用 async QH/qTD，OHCI 使用 ED/TD。
- 解析 device/config/interface/endpoint descriptor，设置地址与首个可用 configuration；
  EHCI 将 full/low-speed 设备交给伴生 OHCI。
- EHCI 可取得 `pinctrl-0` 与 `gpios` lease，并清除 LS2K1000 GENERAL_CFG1 的 prefetch 位。

当前所有传输都轮询完成，不注册 HCD IRQ，也没有运行期热插拔 worker、hub 递归枚举、
isochronous、USB power management 或字符串描述符。中断端点调度被简化为同步传输；首次
扫描失败只记录日志。整体属于可启动枚举的实验实现，不是完整 USB host stack。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2k-usb` |
| ELM 名称 | `platform.ls2k-usb` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2K_USB` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus；EHCI 板级资源可依赖 pinctrl/GPIO provider |

pinctrl/VBUS lease 是 owned resource；HCD 持有 schedule 与 transfer DMA buffer。remove 会停止
controller，若硬件未停止则故意保留 binding/DMA，避免释放仍被硬件引用的内存。

## 验证

```sh
cargo check -p platform-ls2k-usb --lib --target loongarch64-unknown-none
```

该板级 USB 驱动默认保持 `m`。
