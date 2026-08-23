# loongson-irq

LoongArch/Loongson 平台中断控制器驱动。它负责控制器初始化、IRQ 路由、屏蔽和确认，
不把具体设备的中断处理逻辑放入控制器实现。

```sh
cargo check -p platform-loongson-irq --target loongarch64-unknown-none
```
