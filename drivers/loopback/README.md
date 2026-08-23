# loopback

网络回环设备。它使用 `libs/net` 的 flow 执行模型，把本地发送转为接收路径，主要用于
验证 DeviceFunction、套接字和 `net.stack` ELM 的边界。

```sh
cargo check -p net-loopback --target riscv64gc-unknown-none-elf
```
