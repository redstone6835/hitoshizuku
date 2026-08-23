# extfs

ext 文件系统只读/读写基础和 block driver 适配。底层块设备通过通用 block function
注入，挂载策略由 VFS 管理。

```sh
cargo check -p extfs --target x86_64-unknown-linux-gnu
```
