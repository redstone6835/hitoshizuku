# fatfs

FAT 文件系统解析和块设备适配。镜像边界、簇链和目录项都必须经过范围校验；它不负责
生成或打包 initramfs 镜像。

```sh
cargo check -p fatfs --target loongarch64-unknown-none
```
