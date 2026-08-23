# ls7a-rtc

LS7A 平台 RTC 驱动。它实现寄存器读取、时间校验和 realtime source 的 PnP 生命周期，
并通过统一 RTC function 交给通用设备层。

```sh
cargo check -p platform-ls7a-rtc --target loongarch64-unknown-none
```
