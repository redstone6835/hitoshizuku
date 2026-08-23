# uart16550

16550 兼容串口驱动。它提供早期控制台和普通串口 function，区分启动期轮询路径与运行
期中断路径，避免在早期启动阶段依赖尚未初始化的调度器。

```sh
cargo check -p platform-uart16550 --target loongarch64-unknown-none
```
