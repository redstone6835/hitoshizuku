# elm

ELM runtime-model、模块生命周期和 EBI/接口类型。它是内核和可加载 ELM 单元之间的
契约层；新增导出必须同时考虑 symbol profile、generation、owned resource 和卸载顺序。

```sh
cargo check -p elm --target loongarch64-unknown-none
```

完整设计见根目录 [`ELM.md`](../../ELM.md)。
