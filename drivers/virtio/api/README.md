# virtio/api

VirtIO 的稳定类型和版本化契约 crate。这里放设备类型、feature、队列和 provider/consumer
共享的数据结构，不放具体 MMIO/PCI 探测逻辑。

该 crate 与 `provider-api`、`consumer-api` 一起留在内核 workspace，避免驱动和 ELM ABI
发生版本漂移：

```sh
cargo check -p virtio --target riscv64gc-unknown-none-elf
```
