# RISC-V IOMMU 1.0 驱动

`platform.riscv-iommu` 为 platform 或 PCI transport 的 RISC-V IOMMU 1.0 controller 提供
通用 IOMMU domain 和 DMA mapper。

## 实现范围

- platform 匹配 `riscv,iommu`；PCI function 通过 firmware compatible `riscv,pci-iommu`
  匹配。两者都要求 phandle 与 `#iommu-cells = <1>`，PCI 使用 BAR0。
- 校验 1.0 capability、PAS、Sv39/Sv48/Sv57 S-stage 和 device-directory mode，建立 command
  queue、fault queue 与最多三层 device directory。
- 每个 `<device-id>` attachment 建立独立 4 KiB leaf 页表和 IOVA allocator，支持 map-at、
  map、unmap、权限与 IOTLB/device-directory invalidate。
- platform transport 支持最多四条 WSI；没有可用 WSI 或仅 MSI signaling 时退回在 domain
  操作中轮询 fault/event。PCI transport 当前也使用轮询路径。

当前只实现无 PASID 的 S-stage 普通设备 DMA；不支持 G-stage、ATS invalidation、PRI/page
response、PASID/process context、huge page 或 identity/bypass domain。shutdown/detach 无法确认
硬件停止时会保留 queue、directory 或 page-table 内存，避免 DMA use-after-free。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-riscv-iommu` |
| ELM 名称 | `platform.riscv-iommu` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_RISCV_IOMMU` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus；WSI 依赖 PLIC/AIA，PCI transport 依赖 PCI host 枚举 |

controller handle 与 WSI handler 是 owned resource。remove 先拒绝新 attachment、关闭 event IRQ、
清 DDTP 并停 queue；仍有 attached device 时无法视为干净卸载。

## 验证

```sh
cargo check -p platform-riscv-iommu --lib --target riscv64gc-unknown-none-elf
```

该架构 DMA 隔离驱动默认保持 `m`；启用前必须完成目标 IOMMU 的故障注入和卸载测试。
