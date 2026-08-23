# Loongson 中断控制器驱动

`platform.loongson-irq` 按固件描述建立 LoongArch/Loongson 的 CPUIC、EIOINTC、PCH PIC
与 PCH MSI 层级。设备驱动只消费通用 `IrqLine` 或 MSI vector，不需要解析各级控制器
specifier。

## 职责与边界

本 ELM 一次注册四个 PnP driver factory：

| 控制器 | 匹配条件 | 提供能力 |
| --- | --- | --- |
| CPUIC | `interrupt-controller` + `loongson,cpu-interrupt-controller` | 将 DT 中 HWI 2..7 映射为架构 `IrqLine::Hardware(0..5)` |
| EIOINTC | `interrupt-controller` + `loongson,ls2k2000-eiointc` 或 `loongson,eiointc-1.0` | 256 个 IOCSR 外部 vector 的 domain、路由、使能与 pending 分发 |
| PCH PIC | `interrupt-controller` + `loongson,pch-pic-1.0` | 64 个 PCH source 的 mask、极性、触发类型和到父 EIOINTC vector 的动态级联 |
| PCH MSI | `interrupt-controller` + `loongson,pch-msi-1.0` | 管理一段 MSI vector，并生成 message address/data 与父 domain IRQ line |

本 crate 不包含 RTC、UART、PCI 或具体设备的 handler，不建立通用 CPU 异常入口，也不
负责解析整棵 DTB。CPUIC 仅翻译 CPU HWI；EIOINTC/PCH PIC 只 demux 并调用通用 IRQ
registry；PCH MSI 只分配和释放 vector。

## 平台与模块选择

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-loongson-irq` |
| ELM 名称 | `platform.loongson-irq` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_LOONGSON_IRQ`，默认 `m` |
| target | `loongarch64-unknown-none` |
| 构建顺序 | `platform.firmware-bus` 之后 |

EIOINTC 还读取可选 `loongson,eiointc-route-cpu`；PCH PIC 读取可选
`loongson,pic-base-vec` 和 `loongson,pic-route-target`；PCH MSI 要求
`loongson,msi-base-vec` 与 `loongson,msi-num-vecs`。所有控制器都要求固件 phandle，
级联控制器还要求可解析的父中断控制器。

## 对象与生命周期

ELM `initialize` 按 CPUIC、EIOINTC、PCH PIC、PCH MSI 顺序注册 factory；任一步失败会
逆序撤销已注册项。`finalize` 同样逆序注销，仍被占用时返回 busy 类错误。

probe 后的 IRQ domain、父级 handler 和 MSI controller 都作为 PnP owned resource
保存，注册失败路径会撤销刚创建的句柄。PCH PIC 另外把 `Arc<PchPic>` 保存为 driver
data；remove 时先屏蔽全部 source、清 pending，再在内部锁外注销动态父级 handler。
其它控制器的 registry 资源由 PnP core 回收。

## 中断流与并发

```text
设备 source -> PCH PIC slot -> EIOINTC vector -> CPU HWI -> 架构 IRQ 入口
PCI requester -> PCH MSI slot/message -> EIOINTC vector
```

- EIOINTC probe 打开 EXT_IOI，配置 IP map/node map/route，先禁用全部 vector 并清 pending。
- PCH PIC 初始屏蔽全部 64 个 source。第一次翻译某个设备 specifier 时才分配 slot、安装
  父级 handler 并编程 route/type；未引用的 source 不会提前打开。
- PCH PIC 的 slot 表和 PCH MSI 的分配 bitmap 分别由 `Spinlock` 保护。注销父 handler
  时先从锁内取走句柄，再在锁外调用 IRQ registry，避免锁顺序反转。
- EIOINTC 的 IOCSR enable RMW 由通用架构接口完成；当前对象自身没有额外锁，调用方和
  IRQ core 必须保证同一寄存器的并发配置不会互相覆盖。

## 资源与安全边界

- IOCSR 访问通过 `general::dev::irq` 注入的 LoongArch 操作完成，失败会终止 probe。
- PCH PIC MMIO 基址来自 `device_mmio_to_virt`，固定偏移使用易失访问。当前实现信任固件
  提供足够大的控制器窗口，没有按 resource size 逐寄存器验证。
- 所有 hwirq、slot、vector 加法和属性转换都在使用前检查范围或溢出；MSI vector 数不得
  为零。
- 控制器仅路由中断，不验证设备 handler 的行为；handler 生命周期和启停由通用 IRQ
  registry 管理。

## 依赖关系

主要接口来自 `general::dev::{platform,pnp,irq,msi}`，并使用 `vfs::sync::Spinlock`、
`elm`、`allocator` 和 `log`。LS7A RTC、UART 等下游设备通过固件 IRQ dependency 等待
对应 domain 出现，而不是直接依赖本 crate 的 Rust 类型。

## 检查与构建

```sh
cargo check -p platform-loongson-irq --lib --target loongarch64-unknown-none

cargo xtask config
cargo xtask modules --target loongarch64-unknown-none

cargo elm check drivers/loongson-irq --arch loongarch64
cargo elm build drivers/loongson-irq --arch loongarch64 --unsigned
```

目标限制由 [`../Modules.toml`](../Modules.toml) 执行。`--unsigned` 仅适合本地启动测试。
