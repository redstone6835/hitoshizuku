# libs：共享内核 crate

`libs/` 保存被多个内核层或驱动复用的 Rust crate。它们通常是 `no_std`，不应反向依赖
最终 `kernel`，也不应把架构启动逻辑塞进共享库。

## crate 分组

| 目录 | 职责 |
| --- | --- |
| `mm`、`allocator` | 地址空间、页框和分配器 |
| `sched`、`socket`、`net` | 调度、套接字和网络数据面；`net` 提供单写者 flow 执行原语 |
| `vfs`、`fatfs`、`extfs`、`elf` | 文件系统、镜像和可执行文件解析 |
| `elm`、`elm-loader`、`kernel-symbols` | ELM 运行时、装载和内核符号导出 |
| `native-abi`、`soyo` | Native ABI 与 SOYO wire 格式 |
| `fdt` | 无分配 FDT 校验视图，以及可选的索引、地址翻译和 overlay 支持 |
| `efi`、`acpi` | 固件和启动辅助 |
| `log`、`errno`、`profiling`、`kcsan`、`ktest` | 基础设施、诊断和测试支持 |

新增 crate 时优先保持单一职责，并把跨 crate 的 ABI 变更写入根目录协议文档。主机端
工具通过固定 Git revision 使用 ABI crate，不要把工具 workspace 加入这里。

```sh
cargo check --workspace --lib --target loongarch64-unknown-none
cargo test -p net --target x86_64-unknown-linux-gnu
```
