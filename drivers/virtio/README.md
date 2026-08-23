# virtio

VirtIO framework/provider。它描述设备队列、feature 协商、配置空间和设备生命周期，
不直接实现 block 或 network 的上层策略。

- `provider-api/`：内核 host 提供给 VirtIO consumer 的契约；
- `consumer-api/`：模块侧消费 provider 能力的契约；
- `api/`：稳定的 VirtIO 类型和版本化接口。

```sh
cargo check -p virtio-framework --target riscv64gc-unknown-none-elf
```
