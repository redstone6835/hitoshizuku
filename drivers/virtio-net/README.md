# virtio-net

VirtIO network consumer。它负责队列、buffer ownership、feature 协商和网卡
DeviceFunction，将协议状态交给独立的 `net-stack` ELM。

```sh
cargo check -p net-virtio --target riscv64gc-unknown-none-elf
```
