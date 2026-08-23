# virtio-blk

VirtIO block consumer。它使用 `virtio-provider` 建立队列和 DMA 资源，将块请求转换为
通用 block function；设备发现和卸载由 PnP/ELM 生命周期控制。

```sh
cargo check -p virtio-block --target riscv64gc-unknown-none-elf
```
