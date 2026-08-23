# general：通用内核设施

`general/` 位于 HAL 和最终内核镜像之间，承载不属于某一架构、但又需要内核资源的
通用设施：PnP 设备对象、`DeviceFunction`、VFS、内存映射、固件、DMA、IRQ、任务和
系统调用适配。

## 设备相关模块

- `dev/pnp.rs`：设备身份、父子拓扑、驱动绑定、资源拥有和热拔状态机；
- `dev/function.rs`：字符、块、网络等设备功能的开放注册表；
- `dev/dma.rs`、`dev/irq.rs`：驱动申请的资源约束；
- `dev/platform.rs`、`dev/pci.rs`、`dev/usb.rs`：总线和平台设备封装。

具体硬件驱动位于根目录的 `drivers/`，不要把型号匹配和寄存器操作重新放回这里。
设备抽象的完整契约见 [`DEVICE_ABSTRACTION.md`](../DEVICE_ABSTRACTION.md)。

## 检查

```sh
cargo check -p general --lib --target loongarch64-unknown-none
cargo test -p general --lib --target x86_64-unknown-linux-gnu
```
