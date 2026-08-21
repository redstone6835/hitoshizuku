# 主机工具

`elm-tools`（`cargo-elm`）和 `soyo-linker` 是主机端 Rust 应用，不是内核 crate。
每个目录都是独立的 Cargo workspace，并有意排除在内核 workspace 之外。

使用默认 Rust 工具链在本地构建：

```sh
cargo build --manifest-path tools/elm-tools/Cargo.toml --release
cargo build --manifest-path tools/soyo-linker/Cargo.toml --release
cargo install --path tools/elm-tools
```

`soyo-linker` 的 Rust 单元测试不依赖 C 编译器；运行完整的 C header/ELF 集成测试时，
请在 PATH 中提供 `clang`。

`cargo xtask modules` 使用已安装的 `cargo-elm` 子命令。当前 checkout 为私有
ELM 和 Native ABI crate 使用 path 依赖，使源码仍能一起构建。当这些 API 获得
带版本的内核标签后，工具可以迁移到独立仓库，并固定 Git 或已发布的 crate
依赖；它们不需要成为内核树中的 submodule。
