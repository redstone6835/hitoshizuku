# Loongson LS2X 看门狗驱动

`platform.ls2x-wdt` 把 LS2X 32 位倒计时看门狗发布为通用 `WdtFunction`，支持设置超时、
启动、停止和 ping。

## 实现范围

- 匹配 `loongson,ls2x-wdt`，要求覆盖 EN/TMR/CNT 的 0x0c 字节 MMIO。
- 优先通过首个 `clocks` lease 取得频率；缺少 provider 引用时接受固件 `clock-frequency`。
- 超时换算为 `seconds * clock_hz` 并限制到 32 位，最大超时随输入 clock 变化。
- probe 只设置最大可表达超时，不自动启动；stop 清 EN 并把 TMR 写为 `0xffffffff`。

驱动不提供 pretimeout、IRQ、windowed watchdog、nowayout 或 suspend 策略。`set_timeout(0)`
可写入零计数，调用方应避免把它当作安全关闭方式；实机还需确认复位极性和计数频率。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2x-wdt` |
| ELM 名称 | `platform.ls2x-wdt` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2X_WDT` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus；推荐 Loongson clock provider |

binding 持有 WDT device 与可选 clock lease。remove 先停 watchdog、Disable clock，再把 function
标记 gone；模块正在被 consumer 使用时 factory 注销会失败。

## 验证

```sh
cargo check -p platform-ls2x-wdt --lib --target loongarch64-unknown-none
```

该板级安全设备默认保持 `m`，复位测试应在可恢复环境执行。
