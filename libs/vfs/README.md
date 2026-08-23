# vfs

VFS inode、mount、lease、file descriptor 和设备投影接口。具体文件系统通过注册的
block/filesystem driver 接入，VFS 不根据设备名字猜测底层硬件。

```sh
cargo check -p vfs --target loongarch64-unknown-none
```
