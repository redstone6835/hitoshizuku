# net-stack

网络栈 ELM 模块。它提供 `net.stack` 的协议执行面，并通过 `FlowExecution` 将每个 flow
shard 绑定到唯一 owner worker；不同 shard 可以并行，同一份协议状态不会出现并发写者。

该模块不是网卡驱动，网卡由 VirtIO 或其他 DeviceFunction 驱动提供。完整构建由根目录
的 `cargo xtask modules` 负责：

```sh
cargo check -p net-stack --target riscv64gc-unknown-none-elf
```
