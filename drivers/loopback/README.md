# loopback

`loopback` 实现 Hitoshizuku OS 的本地批量回环网络设备。Cargo package 名为
`net-loopback`，ELM 模块名为 `net.loopback`，注册后的接口名为 `lo`。它用于验证
`DeviceFunction`、网络设备 broker、buffer ownership、网络协议栈和 socket/VFS 入口的
完整边界，但不替代 `net.stack`，也不与任何物理总线或 IRQ 控制器交互。

## 数据路径

实现提供一个 `NetQueuePair`：

1. RX refill 把 broker 提供的 buffer lease 收入本地 reserve；
2. TX submit 将 `PacketChain` 的所有权移入 1024 项回环 ring，同时保存 metadata 和
   completion token；
3. RX poll 从同一 ring 取回报文，不复制 payload，并遵守 packet/byte budget；
4. TX reclaim 把 completion token 成批返回给提交方。

队列明确声明 `tx_produces_rx_synchronously = true`。当前能力包括 scatter/gather、最多
32 个报文的 RX/TX batch 和 UDP segmentation；校验和 offload 不由该设备提供。接口
MTU 为 65536，MAC 地址为全零，这些值只描述本地虚拟设备，不应作为物理网卡能力模板。

## ELM 边界与生命周期

`m` 模式通过私有 direct-pinned 入口 `net.loopback.queue-call` 接收有界的队列调用；
`y` 模式使用 `elm-integrated` 将 `LoopbackQueue` 直接包装成常驻队列 endpoint。两种模式
共享同一 `NetQueuePair` 实现和注册数据结构。

生命周期按资源可见性排序：

- `initialize` 先创建队列，再向网络 broker 注册 `lo`；注册失败会回滚队列；
- `quiesce` 标记队列停止接收正常工作，后续 poll 会报告设备离线；
- `finalize` 先释放队列持有的 lease，再请求 broker 移除设备 handle。

动态 ELM 的 queue-call 只接受已登记的 opcode 和 queue id，并校验所有可变调用帧指针；
模块卸载后不会留下指向其代码或私有队列状态的活动入口。

## 源码入口

- [`src/main.rs`](src/main.rs)：`LoopbackElm` 的 create/initialize/quiesce/finalize；
- [`src/driver.rs`](src/driver.rs)：ring、buffer lease、批量队列操作和 broker 注册。

协议解析、flow shard 和 socket 状态位于 [`net-stack`](../net-stack/) 与
[`libs/net`](../../libs/net/)，本 crate 只实现设备执行面。

## 配置与验证

`drivers/Modules.toml` 中的 `CONFIG_NET_LOOPBACK` 默认是 `y`，模块在 `runtime` 阶段
初始化，不限定单一架构。

从仓库根目录检查两个受支持目标：

```sh
cargo check -p net-loopback --lib --target loongarch64-unknown-none
cargo check -p net-loopback --lib --target riscv64gc-unknown-none-elf
```

验证 ELM 构建和模块清单：

```sh
cargo xtask defconfig
cargo xtask modules --target riscv64gc-unknown-none-elf
```

回环设备默认内建，确保本地网络路径不依赖外部模块介质。需要验证动态模块边界时，可用
`cargo xtask config` 将 `CONFIG_NET_LOOPBACK` 改为 `m`；不要直接编辑生成的 `.elm/`、
`dist/` 或 `Elm.lock`。
