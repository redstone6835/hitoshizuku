# Goldfish RTC 驱动

`platform.goldfish-rtc` 为 QEMU RISC-V virt 机器上的 Goldfish RTC 提供时间读写，
并把首次读取的硬件时间候选为内核 realtime source。

## 职责与边界

- 匹配 `compatible = "google,goldfish-rtc"` 的 platform 设备。
- 从两个 32 位 MMIO 寄存器读取或写入 64 位 Unix 纳秒时间。
- 注册 `RtcFunction`，声明 `READ_TIME` 与 `SET_TIME`。
- 在 probe 时安装 realtime source；只有第一个被时钟核心接受的 RTC 成为 owner。
- remove 时撤销本设备拥有的 realtime source，并把 `RtcDevice` 标记为 gone。

本驱动不实现 alarm、周期中断、update IRQ、校准或电池状态。固件中的 IRQ 资源即使
存在也不会被本 crate 使用。

## 平台与匹配条件

[`../Modules.toml`](../Modules.toml) 将该模块限制为 RISC-V64：

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-goldfish-rtc` |
| ELM 名称 | `platform.goldfish-rtc` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_GOLDFISH_RTC`，默认 `m` |
| target | `riscv64gc-unknown-none-elf` |
| 构建顺序 | `platform.plic` 之后 |

probe 要求第一段 MMIO 窗口至少 8 字节，并且初始时间必须能转换成有效的
`RtcDateTime`；否则不创建 function。

## 对象与生命周期

ELM `initialize` 注册 `GoldfishRtcFactory`。匹配设备由
`GoldfishRtcPlatformDriver` 创建 `GoldfishRtc` 和稳定命名的 `RtcDevice`，先读取
初始时间，再注册 function 和 realtime source，最后把二者的生命周期信息保存为
driver data。

realtime source 以物理基址派生非零 ID，并由 `AtomicUsize` 跟踪所有者。时钟核心拒绝
安装时，RTC function 仍然可用，但不会覆盖已有墙钟。remove 只撤销自己拥有的 source。
ELM `finalize` 负责注销 factory。

## MMIO、并发与安全

- 读取顺序固定为 low 后 high；读取 low 会让 Goldfish 硬件锁存对应的 high，形成一致
  快照。
- 写入顺序固定为 high 后 low；low 写入触发 QEMU 更新时间 offset。
- MMIO 访问是 32 位 `read_volatile`/`write_volatile`，地址只来自已验证的第一段窗口。
- owner 状态用 Acquire/Release 原子操作保护。时间寄存器本身没有软件锁；多个并发
  `set_time` 调用可能交错，调用层需要串行化写时间操作。
- 本驱动只校验窗口长度和日期可表示性，不验证来宾外部的时间可信度。

## 依赖关系

核心接口来自 `general::dev::{platform,pnp,rtc}` 和 PnP realtime hooks；`elm` 管理模块
生命周期，`allocator` 提供分配，`log` 记录绑定状态。其它 façade 依赖属于 Kernel API
Profile 的统一闭包。清单中的 `after = platform.plic` 是构建/装载顺序，不表示本驱动
实际注册 IRQ handler。

## 检查与构建

```sh
cargo check -p platform-goldfish-rtc --lib --target riscv64gc-unknown-none-elf

cargo xtask config
cargo xtask modules --target riscv64gc-unknown-none-elf

cargo elm check drivers/goldfish-rtc --arch riscv64
cargo elm build drivers/goldfish-rtc --arch riscv64 --unsigned
```

`--unsigned` 仅用于本地测试；发布 EKI 应通过 `--key` 和非零 `--epoch` 签名。
