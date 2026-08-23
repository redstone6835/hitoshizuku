# syscon

系统控制器驱动。它封装平台提供的 syscon 寄存器窗口和受限操作，资源申请、MMIO 映射
与回收由 PnP core 统一管理。

```sh
cargo check -p platform-syscon --target riscv64gc-unknown-none-elf
```
