# mm

地址空间、页表、映射和内存权限管理。它通过 HAL 获取架构操作，通过 allocator 获取
内存，不应直接调用具体设备驱动。

```sh
cargo check -p mm --target loongarch64-unknown-none
```
