# arch/tests：架构测试

这里放不依赖完整用户态镜像、但需要验证架构状态机的测试。目前包括 LoongArch ASID
跟踪器测试。测试应保持纯状态转换和边界检查，硬件启动 smoke test 放在对应的 QEMU
工具仓库。

```sh
cargo test -p arch --target x86_64-unknown-linux-gnu
```
