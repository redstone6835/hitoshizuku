# goldfish-rtc

Goldfish RTC 驱动。它注册平台 realtime clock source，并在移除时撤销 source owner，
避免已经失效的 MMIO 地址继续被时钟服务使用。

```sh
cargo check -p platform-goldfish-rtc --target riscv64gc-unknown-none-elf
```
