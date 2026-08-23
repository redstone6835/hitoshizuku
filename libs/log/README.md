# log

`no_std` 内核日志 facade 和等级过滤。底层输出由平台控制台注入；日志路径不得在硬
中断中分配或阻塞。

```sh
cargo check -p log --target loongarch64-unknown-none
```
