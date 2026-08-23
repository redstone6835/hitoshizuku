# kcsan

内核 KCSAN 运行时和注入点支持。编译 wrapper 与符号化脚本位于根 `scripts/`，本 crate
只保留运行时接口和无竞态的诊断数据结构。

```sh
cargo check -p kcsan --target loongarch64-unknown-none
```
