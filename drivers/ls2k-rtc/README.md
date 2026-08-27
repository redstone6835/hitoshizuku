# Loongson LS2K RTC 驱动

`platform.ls2k-rtc` 将 LS2K TOY RTC 注册为通用 RTC function，并在时间有效时把它安装为
内核 realtime source。

## 实现范围

- 匹配 `loongson,ls2k-rtc` 与 `loongson,ls2k1000-rtc`，使用 TOY READ/WRITE、MATCH0 和
  RTC control 寄存器。
- 支持读取/设置日期时间，跨秒稳定读取重试，以及从 1900 起的 year offset 校验。
- RTC 窗口覆盖 MATCH0 时提供 alarm；由 `rtc_base - 0x800` 推导 PM1 status/enable，IRQ
  可解析时再声明 ALARM_IRQ 能力。
- 初始日期无效不会阻止 RTC function 注册；有效值才参与 realtime source 仲裁。

PM base 是 LS2K1000 固定偏移而非独立 DT resource，alarm 年字段也只有相对当前 64 年窗口。
驱动不提供 periodic/update IRQ、校准、温补或电池状态；没有 IRQ 时 alarm 寄存器仍可读写，
但不能启用 alarm IRQ。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2k-rtc` |
| ELM 名称 | `platform.ls2k-rtc` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2K_RTC` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus；alarm IRQ 依赖 Loongson IRQ domain |

IRQ handle 是 owned resource。remove 先撤销 realtime source ownership，关闭 alarm/IRQ 能力并
标记 RTC function gone；PnP 随后注销 IRQ 和 function。

## 验证

```sh
cargo check -p platform-ls2k-rtc --lib --target loongarch64-unknown-none
```

该板级 RTC 默认保持 `m`；还需实机验证掉电保持、跨年和 alarm 唤醒。
