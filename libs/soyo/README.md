# soyo

SOYO wire container、manifest、签名和布局验证。它只验证字节格式和 ABI 约束，不负责
链接器进程编排；主机端命令位于独立 `hitoshizuku-soyo-linker` 仓库。

```sh
cargo test -p soyo --target x86_64-unknown-linux-gnu
```
