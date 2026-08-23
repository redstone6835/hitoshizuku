# LS7A RTC 驱动

`platform.ls7a-rtc` 驱动 Loongson LS7A TOY RTC，向通用设备层注册 `RtcFunction`，并在
固件资源完整时提供 alarm IRQ。probe 读取的硬件时间也会作为内核 realtime source 的
候选。

## 职责与边界

- 匹配 `compatible = "loongson,ls7a-rtc"`。
- 启用 RTC oscillator/TOY counter，稳定读取年月日时分秒，支持设置硬件时间。
- 读写 TOY alarm match，并在第二段 PM MMIO 和 IRQ 均可用时启用/确认 alarm IRQ。
- 注册稳定命名的 `RtcFunction`，动态声明 `READ_TIME`、`SET_TIME`、`ALARM`、
  `ALARM_IRQ` 能力。
- 安装并在移除时撤销本设备拥有的 realtime source。

本驱动不提供周期中断、update IRQ、NVRAM、电池状态或跨 64 年 alarm 窗口的隐式换算。
alarm 年字段只有 6 位，目标时间必须落在当前 `fix_year` 的 64 年窗口内。

## 平台与资源匹配

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls7a-rtc` |
| ELM 名称 | `platform.ls7a-rtc` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_LS7A_RTC`，默认 `m` |
| target | `loongarch64-unknown-none` |
| 构建顺序 | `platform.loongson-irq` 之后 |

第一段 MMIO 是 TOY RTC。若存在 `reg-names`，PM 窗口按 `pm`、`acpi`、`alarm`、
`rtc-pm` 查找；否则第二段 MMIO 作为 PM1 status/enable 窗口。缺少 PM 窗口或 IRQ 时，
基本时间功能仍可工作，但不会声明 `ALARM_IRQ`。

## 对象与生命周期

ELM `initialize` 注册一个 `Ls7aRtcFactory`。probe 创建 `Ls7aRtc`，先读取并验证初始时间，
再创建 `RtcDevice`/`RtcFunction`。若固件 IRQ 可解析，则注册 `Ls7aRtcIrqHandler`，将
handler handle 交给 PnP 设备持有；无法解析父 domain 时返回显式 dependency，让 PnP
稍后重试。

`Ls7aRtcBinding` 持有硬件对象和 RTC function。remove 会撤销 realtime owner、禁止
alarm IRQ、清 PM enable 并把 function 标记为 gone。PnP owned resource 负责注销 IRQ
handler；ELM `finalize` 注销 factory。

## 并发、时间一致性与安全

- 时间读取最多重试三次，要求两次 year 寄存器读数一致，避免跨年翻转产生撕裂日期。
- `AtomicU32` 保存 alarm 的 64 年基准，`AtomicBool` 表示 IRQ 能力，`AtomicUsize` 管理
  realtime owner，均使用 Acquire/Release 顺序。
- PM1 status/enable 的读取、清 pending 和 RMW 由 `Spinlock<()>` 串行化。
- TOY 时间与 alarm match 的多寄存器写入没有统一设备锁；并发设置时间/alarm 的调用者
  需要在 RTC class 层串行化。
- 所有寄存器均以 32 位易失访问。主窗口和 PM 窗口来自固件资源；固定偏移、窗口最小
  长度、日期字段和地址加法在访问前校验。

## 依赖关系

主要依赖 `general::dev::{platform,pnp,rtc,irq}`、PnP realtime hooks、
`vfs::sync::Spinlock`、`elm`、`allocator` 与 `log`。alarm IRQ 运行时依赖由固件指定的
IRQ domain，通常由 `platform.loongson-irq` 提供。

## 检查与构建

```sh
cargo check -p platform-ls7a-rtc --lib --target loongarch64-unknown-none

cargo xtask config
cargo xtask modules --target loongarch64-unknown-none

cargo elm check drivers/ls7a-rtc --arch loongarch64
cargo elm build drivers/ls7a-rtc --arch loongarch64 --unsigned
```

模块目标和顺序来自 [`../Modules.toml`](../Modules.toml)。生产 EKI 必须使用签名密钥和
非零 release epoch。
