# hal：硬件抽象层

`hal/` 是架构实现和通用内核之间的窄接口。它统一提供时间、中断、用户地址访问、
内存属性、随机源、控制台、调度上下文和平台启动所需的抽象，具体实现由 `arch/`
或平台代码注入。

## 设计约束

HAL 接口应描述硬件能力，而不是某个设备驱动的策略。接口需要明确：

1. 调用是否可以在中断上下文执行；
2. 地址、对齐、缓存和内存屏障的要求；
3. 失败是返回 `Result`、状态码，还是由平台保证不会发生；
4. 在 `no_std` 和多 CPU 环境下的所有权与并发语义。

通用层应依赖 HAL 的稳定接口，不应通过条件编译读取架构寄存器。

## 检查

```sh
cargo check -p hal --target loongarch64-unknown-none
cargo check -p hal --target riscv64gc-unknown-none-elf
cargo check -p hal --target x86_64-unknown-none
```
