# native-abi

Native runtime 使用的稳定 ABI 类型、操作编号和 ABI family。这里的 `MYGO_*` 常量是
冻结的兼容协议标识，不等同于当前项目品牌，修改必须增加版本并保留旧值。

SOYO 工具通过固定 Git revision 消费本 crate；ABI 变更应同步更新
[`SOYO_FORMAT.md`](../../SOYO_FORMAT.md) 和独立 linker 仓库。
