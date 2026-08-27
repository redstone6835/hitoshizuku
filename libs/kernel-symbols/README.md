# kernel-symbols

内核符号导出宏、ABI manifest 和符号审计元数据。每个暴露给 ELM 的入口必须显式声明
contract、版本、能力和所有权语义；工具仓库消费固定 revision。

```sh
cargo check -p kernel-symbols --target loongarch64-unknown-none
```
