# virtio/provider-api

内核 VirtIO framework 提供给 consumer 的能力契约。它描述队列、DMA buffer、配置空间
和生命周期操作的边界；实现位于上级 `virtio/` 驱动。

```sh
cargo check -p virtio-provider --target riscv64gc-unknown-none-elf
```
