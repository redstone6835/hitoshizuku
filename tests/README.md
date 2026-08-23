# tests：内核测试工程

本目录放置需要完整 ELM 工程上下文的集成测试。当前 `tests/elm/kernel-mixin` 用于
验证 kernel API snapshot、ELM facade 和模块混入路径；它被根 workspace 排除，避免测试
清单被当成正式内核成员。

测试前先完成接口导出：

```sh
cargo xtask build --target loongarch64-unknown-none
cargo test --manifest-path tests/elm/kernel-mixin/Cargo.toml \
  --target loongarch64-unknown-none
```

不应在这里放置第三方镜像、BusyBox 或性能结果。跨仓性能测试位于
[`hitoshizuku-bench`](https://github.com/redstone6835/hitoshizuku-bench)。
