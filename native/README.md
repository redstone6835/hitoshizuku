# Hitoshizuku Native 开发包

本目录是项目自有的 native runtime 和示例源码。它有意与内核 workspace
分开；编译内核不需要 C runtime 或用户态镜像。

Rust 对象库是 `anonlib`。它的 ABI 绑定由 `soyo-ld` 根据程序清单生成，
因此必须显式提供绑定路径：

```sh
cargo build --manifest-path ../tools/soyo-linker/Cargo.toml --release
cargo run --bin native-xtask -- binding --target riscv64 \
  --manifest examples/ring-io/program.json \
  --output build/ring-io/program.rs
cargo run --bin native-xtask -- check --binding build/ring-io/program.rs
```

完整的运行时测试也使用同一入口，例如：

```sh
cargo run --bin native-xtask -- binding --target riscv64 \
  --manifest mrt/tests/process-program.json \
  --output build/test/process.rs
cargo run --bin native-xtask -- test --binding build/test/process.rs
```

`mrt/`、`ranalib/`、C 示例及其测试仍作为源码保留在这里。旧的手写编排已删除，
剩余的跨语言镜像流程正在迁移到 `native-xtask` 和 `soyo-ld`。TLSF 分配器是
native 独立仓库的外部依赖，因此有意不放入当前内核 checkout。当前的 freestanding
头文件适配器位于 `ranalib/include/tlsf/`；未来的 native 仓库应在
`deff9ab509341f264addbd3c8ada533678591905` 修订处引入
`https://github.com/mattconte/tlsf` submodule，并把其 include 路径传给 C 构建。
