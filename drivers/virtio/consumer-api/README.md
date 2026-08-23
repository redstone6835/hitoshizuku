# virtio/consumer-api

VirtIO consumer 使用的抽象接口。`virtio-blk` 与 `virtio-net` 通过这个 crate 请求
provider 能力，不直接依赖 provider 的内部状态。

```sh
cargo check -p virtio-consumer --target riscv64gc-unknown-none-elf
```
