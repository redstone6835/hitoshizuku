# RISC-V PLIC 驱动

`platform.plic` 将固件描述的 Platform-Level Interrupt Controller 接入通用 IRQ domain，
完成外部中断 source 的翻译、使能以及 claim/dispatch/complete 级联。

## 职责与边界

- 匹配带 `interrupt-controller` 属性的 `sifive,plic-1.0.0` 或 `riscv,plic0` 节点。
- 读取 phandle、第一段 MMIO、`interrupts-extended` 和可选 `riscv,ndev`。
- 为 boot hart 选择 supervisor external interrupt（cause 9）对应的 PLIC context。
- 注册 `IrqDomain`，把单 cell source ID 转换为该控制器的 `IrqLine::Controller`。
- 在 CPU `IrqLine::Hardware(0)` 安装级联 handler，执行 claim、通用分发和 complete。

本驱动不处理 CPU local timer/software interrupt，不提供中断亲和性、多 hart context
迁移、优先级策略或 edge/level 重编程。所有启用 source 当前固定使用优先级 `1`，threshold
固定为 `0`。

## 平台与匹配条件

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-plic` |
| ELM 名称 | `platform.plic` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_RISCV_PLIC`，默认 `m` |
| target | `riscv64gc-unknown-none-elf` |
| 构建顺序 | `platform.firmware-bus` 之后 |

若 `riscv,ndev` 缺失，当前实现采用 QEMU virt 的 95 个 source。context 选择按固件 IRQ
resource 中 cause 9 项的顺序与 `boot_cpu_id` 对应；找不到该项会拒绝 probe。

## 对象与生命周期

ELM `initialize` 注册 `PlicFactory`。probe 创建共享 `Plic`，先把 source priority 清零并
设置 threshold，然后依次注册 `PlicDomain` 和 `PlicCascadeHandler`。两个 handle 都作为
PnP owned resource 保存；probe 中途失败由资源回滚路径清理已登记对象。

remove 只记录状态，IRQ domain 和 handler 的实际注销由 PnP 资源所有权完成。ELM
`finalize` 注销 factory；有活动绑定时不会强制卸载。

## 并发与 MMIO 安全

- `PlicInner` 由 `Spinlock` 保护，priority、enable、threshold 和 claim/complete 的地址
  计算与 MMIO 操作在锁内执行。
- source `0` 和大于 `ndev` 的编号被拒绝；enable 寄存器 RMW 因同一锁而不会在该实例内
  丢失更新。
- 所有硬件访问使用 32 位易失读写，映射基址来自 `device_mmio_to_virt`。
- 当前 probe 没有根据固件 resource size 验证 priority、enable 和 context 区域的最大
  偏移，因此安全前提是可信固件提供符合 PLIC 布局且足够大的窗口。
- 当前只建立 boot hart 的 context，不应把该实现描述为完整 SMP PLIC affinity 支持。

## 依赖关系

直接依赖 `general::dev::{platform,pnp,irq}`、`vfs::sync::Spinlock`、`elm`、
`allocator` 和 `log`。Goldfish RTC 等设备在模块清单中位于 PLIC 之后；真正需要 IRQ 的
设备通过 PnP dependency 等待 domain，而非直接调用本 crate。

## 检查与构建

```sh
cargo check -p platform-plic --lib --target riscv64gc-unknown-none-elf

cargo xtask config
cargo xtask modules --target riscv64gc-unknown-none-elf

cargo elm check drivers/plic --arch riscv64
cargo elm build drivers/plic --arch riscv64 --unsigned
```

目标限制由 [`../Modules.toml`](../Modules.toml) 管理。发布 EKI 不应使用
`--unsigned`。
