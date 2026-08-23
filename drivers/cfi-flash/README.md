# cfi-flash

CFI 闪存驱动。它负责识别平台提供的 CFI 设备、建立受 PnP 管理的 flash function，并
通过统一的 flash 接口暴露读写和擦除能力。具体芯片差异应停留在本目录，通用存储语义
由 `general` 和 `libs` 提供。

构建选择由 [`../Modules.toml`](../Modules.toml) 管理：

```sh
cargo check -p platform-cfi-flash --target loongarch64-unknown-none
```
