# allocator

内核物理页、堆和分配会计支持。它提供 `no_std` 分配接口及显式 OOM 传播，不能依赖
VFS、ELM 或最终 kernel。

```sh
cargo check -p allocator --target loongarch64-unknown-none
```
