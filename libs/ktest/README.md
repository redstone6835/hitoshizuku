# ktest

内核测试断言、fixture 和 `no_std` 测试支持。测试代码可依赖内核共享 crate，但不能把
宿主测试 runner 当作目标架构运行时。

```sh
cargo test -p ktest --target x86_64-unknown-linux-gnu
```
