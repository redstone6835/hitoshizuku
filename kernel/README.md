# kernel：最终镜像与内核编排

`kernel/` 是最终内核镜像 crate。它把 `arch`、`hal`、`general`、共享库和选中的驱动
组合成启动映像，负责启动阶段、系统调用、进程、ELM host、网络 host、VFS 挂载和
架构相关的最终链接。

## 这里负责什么

- 建立启动期的全局上下文和基础设施；
- 根据 `drivers/Modules.toml` 启用内建驱动或导出模块；
- 管理 ELM 生命周期、Kernel API Profile 和内核导入符号；
- 将外部 initramfs 作为输入加载，不负责生成 rootfs 或 CPIO 镜像。

## 构建

通常从根目录调用：

```sh
cargo xtask build --target loongarch64-unknown-none
cargo xtask build --target riscv64gc-unknown-none-elf
cargo xtask build --target x86_64-unknown-none
```

直接构建只适合检查 Cargo 依赖；涉及 ELM 接口导出时应使用 `xtask`，以保证接口、
共享 framework 和模块配置使用同一份构建上下文。

## ACPI 启动路径

ACPI 启动会先校验 RSDP/RSDT/XSDT 并枚举全部 SDT，再对典型平台启动所需的
FADT、MADT、MCFG、HPET、BGRT、SPCR、SRAT 和 SLIT 做有界字段解析。DSDT 与所有
SSDT 只加载到一个 `AmlContext`，`_INI` 最多执行一次；电源控制和设备发现复用同一
namespace，并解析 `_S5`、`_STA`、`_HID`、`_CID` 与 `_CRS`。

AML 的 SystemIO 访问由架构启动上下文提供，PCIConfig 优先使用经过校验的 MCFG
ECAM，缺少 MCFG 时才使用架构回调。若固件声明了当前 `aml` crate 或架构尚未提供的
OperationRegion，内核保留已解析的静态 namespace，但禁用动态方法执行，不伪造硬件
返回值。新增架构必须在 `StartAcpiHostOps` 中接入真实 Host I/O，或显式使用
`StartAcpiHostOps::NONE` 接受该降级行为。

SPCR 的 SystemMemory 串口可直接登记为平台设备；SystemIO 形式会被完整解析并记录，
但在通用设备资源模型增加 I/O port 类型之前不会伪装成 MMIO 串口。
