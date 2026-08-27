# kernel-symbols/macros

`kernel-symbols` 的过程宏实现。这里处理 Rust item 到稳定 kernel API profile 的映射，
不得在宏中加入运行时设备策略或隐式 ABI 别名。
