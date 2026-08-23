# firmware-bus

固件设备总线驱动。它把 ACPI/DT 等固件枚举结果转换成 PnP 设备拓扑，维护父子关系和
固件属性，不直接替代具体设备驱动。

```sh
cargo check -p platform-firmware-bus --target loongarch64-unknown-none
```
