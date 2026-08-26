# 通用 PCI CAM/ECAM host 驱动

`platform.pci-host-ecam` 激活固件总线规范化的 PCI host descriptor，安装配置空间、BAR、
INTx/MSI 与 DMA/IOMMU 路由，并枚举 PCI bridge/function PnP 设备。

## 实现范围

- 匹配 `pci-host-ecam-generic`、`pcie-host-ecam-generic`、`pci-host-cam-generic` 和
  `loongson,ls2k1000-pci`。
- 支持标准 CAM/ECAM 与 LS2K1000 配置访问，校验 domain、bus-range 和配置窗口不重叠。
- 保留 DT `ranges` 的 PCI/CPU 双地址，优先保留固件 BAR，再从剩余 IO、memory、
  prefetchable window 分配；配置下游 bridge bus/window。
- 发布 host bridge 与 PCI function，处理多级 bridge INTx swizzle、`interrupt-map`、
  `msi-map`/`msi-parent`，并传递 `dma-ranges`、`iommus` 和 `iommu-map` metadata。

这是启动期枚举实现，不支持 PCIe hotplug、AER、SR-IOV、power management 或运行期资源
重平衡。无效 INTx/MSI route 会降级为无对应路由而不是阻止 host；固件窗口必须可信且足够。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-pci-host-ecam` |
| ELM 名称 | `platform.pci-host-ecam` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_PCI_HOST_ECAM` |
| target | `loongarch64-unknown-none`、`riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus；目标 IRQ/MSI domain，使用 IOMMU 时相应 provider 已就绪 |

host bridge handle 是 PnP owned resource；remove 前 PnP core 先深度优先移除全部 PCI child，
再撤销 host 私有 BAR/ECAM/INTx/MSI 表。ELM finalize 只有静态 runtime 表完全清空才成功。

## 验证

```sh
cargo check -p platform-pci-host-ecam --lib --target loongarch64-unknown-none
cargo check -p platform-pci-host-ecam --lib --target riscv64gc-unknown-none-elf
```

该共享 host 驱动默认保持 `m`；还需分别覆盖 LS2K1000 CAM 与 RISC-V ECAM 实机枚举。
