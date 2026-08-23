# random

平台随机源驱动。它把可用的硬件熵源注册为通用随机 function，并在设备移除或熵源失效
时清理 owner，不能把确定性测试数据当作生产熵源。

```sh
cargo check -p kernel-random --target riscv64gc-unknown-none-elf
```
