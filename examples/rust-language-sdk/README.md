# Rust SDK 示例

这是一个不引入新语言的 Rust 示例，演示由语言无关 ELM 资源协议生成的 SDK façade：

- `ResourceTransport` 是 `language.runtime.resource@1` 的唯一传输边界；
- `RustDeviceSdk` 只接收 `LanguageHandle`、owner 和固定 wire payload；
- DMA 分配和释放不暴露物理地址、内核虚拟地址或 `DmaBuffer` 布局；
- 测试中的 `Fake` transport 覆盖 opaque handle 返回和显式释放。

检查：

```sh
cargo test --manifest-path examples/rust-language-sdk/Cargo.toml --locked
```

真实语言包应先从内核 EKI 生成 `interface.schema.json`，再使用独立的
`LanguagePackage.toml`/`LanguageBridge.toml` 执行：

```sh
cargo elm sdk build/elm-interface/riscv64/manifest.txt \
  --package LanguagePackage.toml \
  --adapters LanguageBridge.toml \
  --output generated/
```
