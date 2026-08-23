# net

网络数据面共享 crate。它提供 flow shard、`FlowExecution`、sidecar 校验、ownership
转移和单写者 turn 原语；网卡驱动只提供队列和设备 function，协议策略由 `net-stack`
ELM 实现。

```sh
cargo test -p net --target x86_64-unknown-linux-gnu
```
