# plic

RISC-V PLIC 驱动。它维护外部中断 source 的优先级、目标 hart、屏蔽和 claim/complete
流程，向 HAL/通用 IRQ 层提供统一入口。

```sh
cargo check -p platform-plic --target riscv64gc-unknown-none-elf
```
