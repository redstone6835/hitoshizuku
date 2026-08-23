# NS16550A UART 驱动

`platform.uart16550` 把固件枚举的 16550 兼容 MMIO UART 注册为字符
`DeviceFunction`，提供控制台式收发、轮询、等待队列和有限的串口控制操作。

## 职责与边界

- 匹配 `ns16550`、`ns16550a`、`PNP0500`、`PNP0501` platform ID。
- 使用第一段 MMIO 建立 `Uart16550`，按稳定分配名投影为 `/dev/uart*` 字符设备。
- 固件提供输入时钟时按 `clock / (16 * baud)` 初始化 8N1、FIFO 和 DTR/RTS；未提供时钟
  时接管固件预配置状态，不猜测 divisor。
- 支持读写、`write_all`、flush、poll、RX 等待者唤醒、清 RX/TX、发送 break、查询队列
  长度和在已知时钟时重设波特率。
- 固件 IRQ 可用时只启用 receive-data-ready 中断；handler 负责唤醒等待者，真正读取仍在
  字符设备路径完成。没有 IRQ 时仍可轮询读写。

本 crate 不是完整 TTY line discipline，不处理调制解调器状态、中断式 TX、DMA、流控
协商、奇偶校验配置或多种寄存器 stride/width。当前寄存器模型固定为连续的 8 位 MMIO
16550 布局，不适用于需要 `reg-shift`、32 位访问或端口 I/O 的变体。

## 平台与模块选择

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-uart16550` |
| ELM 名称 | `platform.uart16550` |
| ELM 模式/阶段 | `y` / `device` |
| 配置项 | `CONFIG_UART16550`，默认 `y` |
| target 限制 | 无 |
| 构建顺序 | firmware bus、Loongson IRQ、PLIC 之后（按目标过滤） |

固件必须至少提供第一段 MMIO。`clock-frequency` 可选；baud 未声明时，在可编程路径取
115200。IRQ resource 可选，但若存在且父 IRQ domain 尚未注册，probe 返回依赖错误而不是
静默退化。

## 对象与生命周期

ELM `initialize` 注册 `Uart16550Factory`。probe 创建 `Arc<Uart16550>` 和 `CharDevice`，
尝试注册 IRQ handler 并把 handle 交给 PnP 资源，然后注册 `CharFunction`，最后保存
`Uart16550Binding`。IRQ 资源接管或 function 注册失败时会关闭 RX 中断，并由显式回滚/
PnP 资源清理句柄。

remove 取走 binding 并关闭 RX 中断；字符 function 和 IRQ owned resource 由 PnP core
随设备解绑。ELM `finalize` 注销 factory，活动引用使注销不安全时返回错误。

## 并发、缓冲与安全

- TX 使用 32 KiB 软件环形缓冲和 Acquire/Release 自旋锁。普通 `write` 允许部分写；
  `write_all` 在同一临界区内完成整段入队，避免多 CPU 日志在字符级交织。
- RX 的“检查 LSR.DR + 读取 RBR”由独立自旋锁保护，因为 RBR 读取会破坏性弹出字节。
- `WaitQueue` 只用于 RX 就绪通知；IRQ handler 不读取用户数据。flush 和缓冲满路径有固定
  自旋上限，超出后返回 timeout/busy。
- break 持续时间依赖调度器纳秒时钟；时钟不可用时返回 busy，不伪造延时。
- MMIO 以固定偏移进行单字节易失访问。当前 probe 没有验证 resource size 是否覆盖六个
  寄存器，也不验证硬件确实是 8 位 16550；可信固件匹配是其安全前提。

## 依赖关系

主要依赖 `general::dev::{platform,pnp,char,function,irq}`、`sched::{Task,WaitQueue}`、
`elm`、`allocator` 和 `log`。运行时 IRQ domain 由固件 parent 决定：LoongArch 通常来自
`platform.loongson-irq`，RISC-V 通常来自 `platform.plic`。

## 检查与构建

```sh
cargo check -p platform-uart16550 --lib --target loongarch64-unknown-none
cargo check -p platform-uart16550 --lib --target riscv64gc-unknown-none-elf

cargo xtask config
cargo xtask modules --target loongarch64-unknown-none

cargo elm check drivers/uart16550 --arch loongarch64
cargo elm build drivers/uart16550 --arch loongarch64
```

[`../Modules.toml`](../Modules.toml) 负责目标相关顺序和 `y/m/n` 选择。本模块默认以 `y`
模式集成；只有切换到 `m` 模式发布独立 EKI 时才涉及签名。
