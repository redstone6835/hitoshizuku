# fw-cfg

fw_cfg 平台设备驱动。驱动通过固件总线取得 selector、DMA 和数据窗口资源，并以受
PnP 生命周期管理的 function 形式提供给上层。

```sh
cargo check -p platform-fw-cfg --target loongarch64-unknown-none
```
