# net-stack

`net-stack` 是 Hitoshizuku OS 的网络协议执行模块，Cargo package 名为 `net-stack`，
ELM 模块名为 `net.stack`。它处理 Ethernet、ARP、IPv4、IPv6、ICMP、TCP 和 UDP 的
解析与 flow 状态推进，并承接分片重组、控制面命令和发送计划；它不负责枚举网卡、管理
VirtIO 寄存器或持有设备 IRQ。

设备驱动通过 `libs/net` 注册队列与 buffer pool，常驻网络 broker 再把有界的 packet
batch 和 flow command 交给本模块。这样，设备队列生命周期、协议状态和 socket/VFS
入口可以分别演进。

## 单写者执行模型

模块为每个活动 CPU 建立一个 `FlowShard` 和一个 `FlowExecution`。`FlowExecution` 把
generation、执行者类别、CPU、busy 和 pending 状态放在同一个原子字中，并通过不可跨
generation 的执行租约保证：

- 同一 shard 同一时刻最多有一个可变访问者；
- owner worker、同步系统调用和恢复路径竞争同一租约，不另开旁路锁；
- 获取失败的调用只标记 pending 并返回 busy，不在模块边界内阻塞等待；
- 不同 shard 拥有独立状态和租约，可以在不同 CPU 上并行执行；
- 控制面命令只由 shard 0 接受，控制面对象另由自旋锁串行化。

`dispatch_shard_turn` 还要求非空 worker turn 在其 shard 对应的 CPU 上执行。租约覆盖从
批量解析、flow 命令推进到 TX 计划提交的整个 turn，因此“单写者”约束包含协议状态的
完整一次状态转换，而不只是单个函数调用。

## ELM 接口与生命周期

模块提供两个私有的 direct-pinned 入口：

- `net.stack.shard-turn`：owner worker 的批量协议 turn；
- `net.stack.local-turn`：系统调用侧的低延迟同步 turn。

两者都校验 generation、调用帧和 quiesce 状态。`m` 模式下，入口通过精确契约绑定；
`y` 模式下，`elm-integrated` feature 令常驻内核直接持有同名 Rust 调用入口。

生命周期顺序如下：

1. `create` 创建尚未注册的模块状态；
2. `initialize` 读取常驻 broker 的 boot config，按活动 CPU 数建立 flow shards，并注册
   当前 stack generation；
3. `quiesce` 拒绝新的 turn；
4. `finalize` 请求 broker 移除当前 handle，确认没有执行租约后销毁所有 shard 与控制面。

如果注册失败，初始化路径会立即销毁刚建立的 generation 状态。卸载时仍有调用或 owner
不匹配会返回错误，不会强行释放仍在使用的协议状态。

## 源码入口

当前实现集中在 [`src/main.rs`](src/main.rs)：

- 链路层、网络层和传输层 sidecar 解析；
- RSS/flow hash 与 shard 分派；
- packet batch、重组和 TX plan 的 turn 调度；
- `NetStackElm` 生命周期及两个 ELM 导出入口。

共享队列、buffer、socket 和 flow 数据类型位于 [`libs/net`](../../libs/net/)；不要在本
crate 中复制一套相同的宿主状态。

## 配置与验证

`drivers/Modules.toml` 中的 `CONFIG_NET_STACK` 默认是 `y`，`integrated_phase` 为
`runtime`，同时支持 LoongArch64 与 RISC-V64 内核 API Profile。

从仓库根目录执行源码检查：

```sh
cargo check -p net-stack --lib --target loongarch64-unknown-none
cargo check -p net-stack --lib --target riscv64gc-unknown-none-elf
```

验证真实 ELM Profile、依赖集合和镜像输出：

```sh
cargo xtask defconfig
cargo xtask modules --target riscv64gc-unknown-none-elf
```

网络协议执行面默认内建。需要专门验证动态装载、隔离和热替换边界时，可通过
`cargo xtask config` 把 `CONFIG_NET_STACK` 改为 `m`，不要手工切换 `elm-integrated`。
生成的 `.elm/`、`dist/` 和 `Elm.lock` 均是本地产物。
