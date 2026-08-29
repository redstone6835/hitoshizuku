# x86_64 UEFI loader

本目录是独立的 `x86_64-unknown-uefi` PE32+ 应用，不参与内核 ELF 的高半区链接。
它从当前 EFI System Partition 读取 `EFI/HITOSHI/KERNEL.ELF`，验证 ELF64、x86_64、
`ET_EXEC`、平台固定 VMA/LMA 偏移以及所有 `PT_LOAD` 边界，再完成以下交接：

1. 在 Boot Services 仍可用时分配低于 4 GiB 的内存图、RSDP、Multiboot2 信息、页表、
   栈和位置无关 trampoline；
2. 能安全固定分配的内核段直接装载，和 loader 或可回收固件内存冲突的段先放入 staging；
3. 重新获取最终 UEFI memory map，逐页验证 deferred 目标只覆盖退出 Boot Services 后可回收
   的内存，并把所有内核目标区间在合成 Multiboot2 map 中标为 reserved；
4. 按 UEFI 规定在 `EFI_INVALID_PARAMETER` 时重新获取 map key 并重试
   `ExitBootServices`；成功后不再调用固件；
5. 低地址 trampoline 复制 deferred 段、安装临时恒等映射，再切到兼容模式，以
   Multiboot2 寄存器约定跳入内核 `_start`。

loader 对 Reserved、Runtime Services、ACPI Reclaim 和 ACPI NVS 覆盖均失败关闭，不会为了
满足固定内核地址而改写固件永久保留区。内核当前是无重定位的固定地址 ELF，因此平台目录
中的物理基址必须落在目标固件提供的连续可回收窗口。

常用命令：

```sh
rustup target add x86_64-unknown-uefi
cargo test --manifest-path boot/uefi-loader/Cargo.toml
cargo build --manifest-path boot/uefi-loader/Cargo.toml \
  --target x86_64-unknown-uefi --release
cargo xtask image --platform qemu-x86_64 --format efi
```

最后一个命令生成 `build/x86_64/esp.img`，其中包含标准 removable-media 路径
`EFI/BOOT/BOOTX64.EFI` 和经过平台校验的 `EFI/HITOSHI/KERNEL.ELF`。rootfs 与 initramfs
仍由外部工程提供。
