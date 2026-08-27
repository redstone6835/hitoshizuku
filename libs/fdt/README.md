# fdt：设备树解析与语义索引

`fdt` 是内核与板级驱动共用的 Flattened Devicetree 解析库。默认构建不分配内存，
适合启动早期直接校验并遍历固件传入的 DTB；启用 `alloc` feature 后，可建立带稳定
节点编号的语义索引，并处理地址、IRQ、MSI、PCI、NUMA、设备图和 overlay。

## 能力边界

- `Fdt::parse` 完整校验头部、reservation map、structure block 和 strings block，
  接受 DTSpec v1 到 v17 以及声明兼容 v17 的后续版本；
- `Fdt`、`Node`、`Property` 提供零拷贝借用视图，适用于分配器就绪前的启动路径；
- `Tree::from_fdt` 在 `alloc` feature 下建立 alias、phandle 和节点索引，并拒绝重复
  属性、重复同级节点及冲突 phandle；
- `Tree` 提供 `reg`、`ranges`、IRQ、MSI、PCI、NUMA、reserved-memory、RISC-V CPU
  binding 等常用语义解析；
- `OwnedTree` 支持修改、重新序列化及原子应用 dtc/Linux 风格 overlay。

该 crate 只负责解析和表达固件数据，不负责映射 MMIO、分配中断、探测驱动或修改
平台生命周期。调用方必须继续通过 `general`/HAL 完成资源所有权与硬件访问。

## 使用

启动早期只读解析不需要 feature：

```rust
let fdt = fdt::Fdt::parse(dtb_bytes)?;
for node in fdt.nodes() {
    // Node 与 Property 借用 dtb_bytes，不发生堆分配。
    let _name = node.name();
}
```

需要语义索引或 overlay 时，在依赖中启用 `alloc`：

```toml
[dependencies]
fdt = { path = "../fdt", features = ["alloc"] }
```

```rust
let fdt = fdt::Fdt::parse(dtb_bytes)?;
let tree = fdt::Tree::from_fdt(fdt)?;
let memory = tree.memory_description()?;
```

不要从未经验证的裸指针直接构造任意长度切片。启动入口应先读取并校验 FDT 魔数与
`totalsize`，确认范围位于可访问内存后，再把精确切片交给 `Fdt::parse`。

## 测试

```sh
cargo test -p fdt
cargo test -p fdt --features alloc
```

`tests/tooling_conformance.rs` 会在本机存在 `dtc`、`fdtoverlay` 时执行额外互操作测试；
缺少这些工具不会影响纯 Rust 单元测试。
