# elf

ELF 字节流解析和加载视图。该 crate 只负责结构化解析、范围检查和重定位元数据，不
执行用户程序，也不读取宿主文件系统。

```sh
cargo test -p elf --target x86_64-unknown-linux-gnu
```
