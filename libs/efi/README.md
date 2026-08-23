# efi

EFI 启动辅助 crate。Rust 代码提供启动协议和内存描述，`build.rs` 通过 Cargo 的 `cc`
支持编译少量 C 启动适配代码。C 文件属于项目源码，不能移入第三方 vendor。

```sh
cargo check -p efi --target loongarch64-unknown-none
```
