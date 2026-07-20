# MyGO 网络栈 ELM 化架构规范

- 版本：`v1.0-draft1`
- 日期：`2026-07-18`
- 状态：设计冻结候选
- 适用仓库：`oskernel2026-mygo`

## 1. 规范目的

本文规定 MyGO 网络栈从常驻内核实现迁移为 ELM 的最终边界、运行模型、生命周期、所有权、故障处理、构建方式、迁移阶段和验收标准。

本文解决的不是“把网络驱动放进带有 `Elm.toml` 的目录”这一局部问题，而是以下完整目标：

1. VirtIO-net、loopback 等网络设备驱动成为可管理 ELM。
2. Ethernet、ARP、IPv4、IPv6、TCP、UDP、ICMP、RAW、路由、邻居、socket 状态和网络控制面成为 `net.stack` ELM 的实现。
3. 常驻内核不再包含网络协议算法和网络连接状态，只保留安全运行 ELM 所需的通用宿主、ABI、调度、VFS 和资源回收机制。
4. 网络栈可以在内核启动后装载、卸载、故障隔离和重新装载，且不得留下悬空代码指针、worker、IRQ、DMA lease、timer、socket 或 waiter。
5. 数据热路径不得经过 `elm-mgr`、provider 固定帧或每 packet ELM 切换。

本文使用“必须”“禁止”“应”“可以”表达规范强度。“必须”和“禁止”为验收条件。

## 2. 与现有规范的关系

本文是 `docs/network-stack-v2-spec.md` 的 ELM 边界专项规范。

- packet、flow、TCP/UDP、readiness、资源上限和性能原则继续以 v2 网络栈规范为准。
- ELM 模块边界、常驻宿主边界、装载顺序、kernel-to-ELM 调用、generation、卸载和热替换以本文为准。
- 本文替代 v2 规范第 21 节中“协议实现长期位于常驻 `libs/net`”的部署假设。
- 本文不允许重新引入旧 smoltcp 栈、第二套网络栈或隐藏 fallback。
- 实现若发现本文不可行，必须先按 v2 规范第 31 节提交规范修订，不得先写另一条实现路径再追认。

## 3. 当前实现问题清单

以下问题必须在 `CONFIG_NET_STACK=m` 前解决。

### 3.1 ELM 源码模式不等于动态 ELM

当前 loopback、VirtIO framework、VirtIO block 等模块虽然使用 `ElmModule`，但根目录 `.config` 全部选择 `y`，生成的是 `.integrated.a`。它们不具有动态 cell、generation、运行时卸载或热替换语义。

因此：

- `Elm.toml` 中存在 `mode = "m"` 不能作为动态运行证明。
- 只有最终构建产生 `.eki`、由 `elm-mgr` 激活并进入 cell 快照，才属于受管 ELM。
- `y` 只用于同一源码的集成验证和启动过渡，不能作为最终网络 ELM 验收结果。

### 3.2 网络 Kernel API 导出不完整

当前 `libs/net` 类型可以进入 metadata façade，但实时 Kernel API Profile 中缺少新版 `net.*` 直接符号。动态模块无法可靠导入 `register_device`、`begin_remove`、NetBuf、queue 和 socket host 契约。

必须建立一份只包含网络 ABI 与宿主入口的审核目录。协议算法不得为了方便重新导出为常驻内核符号。

### 3.3 NetWorker 只支持启动期设备

当前 runtime 只在 `start_workers()` 中扫描一次尚未启动的设备，随后装载的 BuildBound ELM 只会把设备加入 registry，不会创建 queue worker。

必须删除以下启动假设：

- 启动 worker 前必然已经存在 queue。
- worker 集合在启动后不再变化。
- protocol shard 的 egress 列表不可更新。
- 没有 queue 时可以 panic。

网络宿主必须允许空启动、late attach、late detach 和重复装载 stack generation。

### 3.4 PnP remove 无法传播 Busy

当前 `PnpDriver::remove()` 返回 `()`。如果网络 `begin_remove()` 因 outstanding lease 或 worker 未排空返回 `Busy`，PnP 仍可能继续释放 IRQ、MSI、DMA 和 driver data。

该行为违反网络设备同步两阶段 detach 契约。实现必须保证：

1. queue 和 pool 未排空时，硬件资源不得释放。
2. `Busy` 必须阻止 ELM 镜像卸载。
3. remove retry 必须能够从隔离状态继续。
4. PnP、网络宿主和 ELM owned resource 只能有一个权威提交点。

### 3.5 常驻对象可能保存 ELM vtable

当前 `Box<dyn NetQueuePair>`、`Arc<dyn QueueIrqControl>` 和 `Box<dyn NetBufStorage>` 可以由模块实现。如果这些 trait object 在 ELM 镜像释放后仍由内核保存，其 vtable 和方法地址将悬空。

最终动态模式禁止常驻内核长期保存未由 ELM runtime pin 和调用门管理的模块 vtable。

### 3.6 SocketFacade 仍是常驻协议对象

当前 VFS 保存 `Arc<net::SocketFacade>`，而 `SocketFacade` 本身包含网络数据环、owner、generation、等待和部分协议可见状态。直接把协议 worker 移入 ELM 会造成状态跨越两套生命周期。

最终必须把 VFS 持有对象缩减为常驻 `NetSocketProxy`，协议和 socket 数据状态由 `net.stack` generation 拥有。

### 3.7 SocketRuntime 是不可替换静态后端

当前 `install_socket_runtime(&'static dyn SocketRuntime)` 只允许安装一次，不支持 generation、撤销、替换和故障隔离。

该接口必须由可注册、可撤销、可切换 generation 的 stack host 替代。

### 3.8 ELM 缺少适合网络热路径的 kernel consumer 调用门

provider 使用固定 256 字节帧，适合管理控制面，不适合 PacketBatch、Socket I/O 和每 worker turn 的高频调用。普通 Rust trait object 调用又不能自动提供 ELM fault guard、CPU 预算、active-call 计数和 generation pin。

必须增加 runtime 管理的 pinned native batch call。它每个 batch 或 syscall 进入一次 ELM 保护域，不按 packet 进入，不经过 provider。

### 3.9 ELM 自建 worker 缺少完整任务回收协议

当前 scheduler 没有面向网络 ELM worker 的完整 `quiesce/cancel/drain/release` owned-resource 后端。让 ELM 自己创建永久 kthread 会扩大卸载风险。

最终网络 worker 入口必须位于常驻宿主；ELM 只提供被 worker 调用的协议逻辑和 generation 内状态。

### 3.10 网络启动密钥边界过宽

当前单一 `NetBootConfig` 同时包含 RSS、TCP ISN、临时端口、hash、generation 和 MAC 派生材料。网卡驱动不应获得 TCP ISN 和端口密钥。

最终必须按使用者拆分只读启动材料：

- driver 只获得 RSS 配置和 MAC 派生结果。
- stack 获得 TCP ISN、端口、flow hash 和 generation 材料。
- 原始随机材料和未授权密钥始终留在常驻宿主。

## 4. 目标与非目标

### 4.1 核心目标

最终实现必须满足：

- 网络协议算法和运行状态全部位于 `net.stack` ELM。
- VirtIO-net 和 loopback 分别位于独立 ELM。
- 驱动与协议栈互不直接依赖，由常驻 host broker 连接。
- stack 和 driver 可以按任意顺序装载。
- stack 可以在设备保持存在时卸载和重新装载。
- driver 可以在 stack 保持存在时卸载和重新装载。
- ELM fault 不得使 kernel worker 返回到已释放代码。
- ELM detach 不得留下任何 generation 所属资源。
- `y` 与 `m` 使用同一份业务源码和同一套协议状态机。
- 数据面不经过管理通道，不产生每 packet 动态分配和每 packet ELM 进入。

### 4.2 非目标

第一版明确不要求：

- 保持既有 TCP 连接的无感 stack 热替换。
- 在不同 rustc、target spec 或 Kernel API Profile 间迁移原生状态。
- 把 TCP、UDP、路由等拆成多个独立 ELM。
- 让 ELM 直接处理硬 IRQ。
- 让 ELM 长期保存用户空间裸指针或内核任务裸指针。
- 使用 provider 传输 packet 或 socket payload。
- 同时运行两个可接收真实 packet 的网络栈 generation。

### 4.3 方案选择依据

本文采用“常驻通用 host + 完整 `net.stack` ELM + 独立 driver ELM”，而不采用以下方案：

| 方案 | 结论 | 原因 |
|---|---|---|
| 整个 worker 和 scheduler task 都放入 ELM | 拒绝 | 当前任务、timer 和 wake callback 缺少足以保证镜像卸载的完整 owned-resource 协议；fault 后也不能依赖模块代码自行退出 |
| 协议 worker 进入 ELM，但 `SocketFacade` 和数据环继续常驻 | 拒绝 | socket、flow 和协议状态会跨越两套 generation，stack 卸载不能形成单一提交点，也不属于完整网络栈 ELM 化 |
| packet 数据面使用 provider | 拒绝 | 固定帧容量、管理路由、序列化和租约开销不适合 batch 热路径，也会令 provider 调用数随 packet 线性增长 |
| kernel 长期保存 ELM 实现的普通 trait object | 拒绝 | vtable、drop glue 和方法地址属于模块镜像，缺少 pin 和 fault guard 时会产生悬空代码引用 |
| TCP、UDP、route、neighbor 分别成为独立 ELM | 第一版拒绝 | 会在每 packet/flow/timer 路径产生嵌套跨 ELM 调用，并把单 shard 所有权拆成分布式事务 |
| 常驻 host 分段调用 driver ELM 和 stack ELM | 采用 | worker、IRQ、VFS 和回收边界稳定；每 batch 只进入有限次数；driver 与 stack 可以独立装卸 |

采用方案的关键证明义务是：常驻 host 不理解协议，只管理 generation、调用、批次和资源；从而 host 的存在不能成为保留第二套网络栈的理由。

## 5. 最终边界

### 5.1 进入 ELM 的内容

`net.stack` 必须拥有：

- Ethernet、ARP、IPv4、IPv6 parser 和生成器。
- route lookup、policy route、neighbor、fragment、multicast。
- TCP、UDP、ICMP、RAW 状态机。
- FlowTable、FlowCell、FlowShard、dirty queue 和协议 timer。
- bind registry、临时端口分配、ListenGroup 和 accept backlog。
- socket 收发数据、错误队列和协议 owner 状态。
- DHCP、DAD 及其它网络控制面状态。
- 网络配置快照的网络语义。
- 网络统计、trace 和 consistency check 的业务部分。

`net.virtio` 必须拥有：

- VirtIO-net feature 协商。
- PCI/MMIO transport 的网络设备适配。
- split virtqueue descriptor/ring 状态。
- MQ/RSS/control queue 配置。
- queue completion 解析和 doorbell。
- 网络设备 probe/remove 的硬件事务。

`net.loopback` 必须拥有：

- loopback queue 和虚拟 IRQ 行为。
- loopback 自身的设备注册和撤销。

### 5.2 保持常驻的内容

常驻内核只允许保留：

- ELM Core、原生调用保护域、generation pin 和资源账本。
- scheduler、IRQ、DMA、MMIO、VFS、PollSource 等通用基础设施。
- 不解释网络协议的 NetBuf/PacketBatch 所有权类型。
- driver 与 stack 的注册 broker。
- worker 创建、CPU affinity、wake、timer deadline 投递和任务退出。
- VFS `NetSocketProxy`、fd 生命周期和用户内存复制边界。
- boot secret 保管和最小授权派生。
- stack 缺失、故障或卸载时的稳定错误回退。

常驻部分禁止包含：

- packet header 解析。
- TCP/UDP/ICMP 状态转换。
- route、neighbor、bind、listen 或 flow 算法。
- socket 数据环和协议数据。
- DHCP、DAD 或重传 timer 逻辑。
- 任何可在 stack ELM 中实现的网络策略。

### 5.3 `libs/net` 的最终职责

`libs/net` 保持单一 crate，但降级为网络契约与宿主模型 crate，不再是协议实现 crate。

允许的目录为：

```text
libs/net/
  abi/        ELM pinned native call ABI 与版本
  buf/        NetBuf、lease、pool identity、batch
  device/     driver registration 与 queue endpoint
  host/       stack registration、device/stack broker 契约
  socket/     NetSocketProxy 契约、请求/回复和 readiness
  ids/        device、queue、stack、socket、generation ID
  diag/       通用快照结构，不含协议算法
```

以下实现必须从 `libs/net` 移入 `net.stack`：

```text
pipeline/
flow/
transport/
control/
当前 SocketFacade 协议与数据状态
```

## 6. 总体架构

```text
                        elm-mgr / ELM Core
                               |
                  generation pin / native call guard
                               |
  +----------------------------+----------------------------+
  |                         常驻 net host                   |
  |  device broker  stack broker  worker shell  socket proxy|
  +-----------+---------------------+------------------------+
              |                     |
       pinned batch call      pinned batch/socket call
              |                     |
      +-------+-------+      +------+-----------------------+
      | network driver |      |          net.stack ELM       |
      | ELM generation |      | L2/L3 flow TCP/UDP control   |
      +-------+-------+      +------+-----------------------+
              |                     |
        VirtIO queue/DMA        protocol state / socket data
              |
           hardware

  VFS fd -> NetSocketProxy -> pinned socket call -> net.stack
```

常驻 host 是执行宿主，不是第二套网络栈。它只负责把 driver batch、stack batch、socket 请求和生命周期事务连接起来。

## 7. ELM 模块拓扑

### 7.1 `net.stack`

- ELM name：`net.stack`
- kind：`network`
- 默认源码 mode：`m`
- `y` 阶段：`runtime`
- 不直接依赖任何具体网卡驱动。
- 不依赖 `virtio.framework`。
- 通过 `NetStackRegistration` 向常驻 host 注册唯一活动 stack generation。

### 7.2 `net.virtio`

- ELM name：`net.virtio`
- kind：`network`
- 依赖 `virtio.framework` 的 `driver.virtio.framework@1` direct-pinned API。
- 不依赖 `net.stack` ELM。
- 通过 host device registration 发布 queue endpoint。

### 7.3 `net.loopback`

- ELM name：`net.loopback`
- kind：`network`
- 不依赖 `net.stack` ELM。
- 通过同一 device registration 契约发布 loopback queue。

### 7.4 依赖规则

- 协议栈与网络驱动之间使用 host broker，不写 `[[dependencies]]`。
- `net.virtio -> virtio.framework` 是代码/API 依赖，必须写 `depends`。
- stack 和 driver 的启动顺序最多使用模块集合 `after`，不得依赖该顺序保证正确性。
- `y/m` 模式不同的 stack 与 driver 必须仍可互操作。
- build-set 不得因为 stack 和 driver 的 host 关系强制相同模式。

## 8. 构建与启动模型

### 8.1 配置键

模块集合至少增加：

```text
CONFIG_NET_STACK=y|m|n
CONFIG_NET_LOOPBACK=y|m|n
CONFIG_VIRTIO_NET=y|m|n
```

`CONFIG_VIRTIO_NET` 依赖 `CONFIG_VIRTIO`，两者模式必须一致。`CONFIG_NET_STACK` 与网卡驱动模式独立。

### 8.2 `y/m/n` 语义

- `y`：同一 stack 或 driver 源码编译为 integrated component，用于启动过渡和同源 A/B。
- `m`：生成受管 EKI，具有 cell、generation、故障隔离、资源归属和卸载语义。
- `n`：不构建并清理陈旧产物。

禁止为 `y` 和 `m` 维护不同协议实现、不同 socket 状态机或不同 queue 算法。

### 8.3 启动顺序

最终启动顺序固定为：

```text
设备通用基础设施初始化
-> 生成并封存网络 boot secrets
-> 初始化空 net host
-> scheduler boot_init
-> 初始化 elm-mgr
-> 装载 BuildBound ELM
-> stack/driver 各自向 host 注册
-> host 在 stack 与 queue 可用时创建或激活 worker
-> 启动用户态 init
```

空 host 必须合法：

- 没有 stack 时不能 panic。
- 没有 queue 时不能 panic。
- 只有 stack、只有 driver 或两者都没有时，内核仍能启动。
- stack 和 queue 任一方晚到时，host 必须在条件满足后自动连接。

## 9. Pinned Native Call

### 9.1 定位

网络数据面需要新增 runtime 管理的 kernel consumer exact-Rust 调用门。本文称其为 `PinnedNativeCall<F>`；最终代码名称可以调整，但语义不得改变。

它必须提供：

- 目标 cell 和 generation pin。
- export 名称、contract、version 和完整 Rust ABI 校验。
- 每次调用的 ELM current context。
- active native call 计数。
- CPU 时间和 stack budget 计量。
- panic、同步 fault 和超时恢复。
- quiescing 后拒绝新调用。
- generation 变更后的 stale handle 拒绝。
- detach 前等待所有在途调用退出。

### 9.2 调用粒度

允许：

- 每 queue batch 一次 driver ELM 调用。
- 每 worker turn 或每 protocol batch 一次 stack ELM 调用。
- 每次 socket syscall 或控制操作一次 stack ELM 调用。

禁止：

- 每 packet 多次跨 ELM。
- 每 header、每 flow lookup 或每 timer entry 跨 ELM。
- 从硬 IRQ 进入 stack ELM。
- 使用 provider 替代 batch call。

### 9.3 借用规则

Pinned call 可以借用常驻 host 创建的固定 batch、请求和回复对象，但：

- 借用只在当前同步调用内有效。
- ELM 不得保存引用、裸指针或 slice 地址。
- ELM 需要长期持有 packet 时必须取得明确 ownership。
- host 在调用返回后必须校验 batch 前缀、计数、generation 和 ownership。
- fault 返回时，host 必须能确定哪些项仍归自己，不能依赖 ELM 故障现场恢复 ownership。

因此所有 move 型 batch 调用必须采用“已提交前缀”协议，未提交后缀仍归调用者。

## 10. Driver 与 host 契约

### 10.1 禁止原始长期 vtable

动态模式下，`NetDeviceRegistration` 不得直接把裸 `Box<dyn NetQueuePair>` 长期交给 kernel。

它必须改为 runtime 管理的 queue endpoint，至少包含：

- owner cell/generation。
- queue ID 和冻结 capability。
- driver pinned native export handles。
- resident DMA pool owner。
- resident IRQ wake/control handle。
- queue lifecycle state。

集成 `y` 模式可以由编译器内化调用，但必须表现出相同 ownership 和状态机。

### 10.2 DMA 与 NetBuf storage

长期 NetBuf backing 的释放、sync 和 recycler 入口必须位于常驻 DMA/NetBuf substrate。

驱动 ELM 可以：

- 申请 DMA buffer。
- 配置 descriptor 指向 DMA 地址。
- 在 pinned call 中操作 ring。

驱动 ELM 禁止：

- 让 `NetBufLease` 保存指向驱动镜像的 storage vtable。
- 在设备 teardown 后由 recycler 回调驱动代码。
- 在 queue 外保存第二个 pool owner。

### 10.3 设备注册状态机

```text
Prepared
-> RegisteredNoStack
-> Attached
-> Quiescing
-> Draining
-> Detached
-> Released
```

- driver 完成 feature、queue、IRQ 和 pool 构造后提交 registration。
- host 成功后接管 queue endpoint 和 pool ownership。
- stack 不存在时设备保持 `RegisteredNoStack`，IRQ 默认 masked，不丢失 ownership。
- stack 到达后 host 创建 worker 并进入 `Attached`。
- stack 卸载时设备可回到 `RegisteredNoStack`，不要求 reset 硬件。
- driver 卸载时必须完整进入 `Released`。

### 10.4 设备卸载事务

```text
阻止新调度
-> mask/ack IRQ
-> 从 stack 配置中撤销接口
-> 唤醒并停止 queue worker
-> 排空 RX/TX/completion/recycle
-> 等待 outstanding lease
-> 归还 teardown token
-> PnP 释放 IRQ/MSI/DMA
-> reset device
-> 释放 driver generation
```

任一步返回 `Busy` 或错误时：

- 不得释放后续资源。
- ELM detach 必须失败或保持 Quarantined/Quiescing。
- retry 必须幂等。
- 错误不得被 `PnpDriver::remove()` 的 `()` 返回值吞掉。

PnP 必须增加可失败的 unbind/remove 事务，或由网络 owned resource 在 PnP 资源释放前完成权威 teardown。两者只能选择一个提交点，不得重复 remove。

## 11. Stack 与 host 契约

### 11.1 Stack registration

同一时刻只允许一个 active stack generation。registration 至少包含：

- stack cell/generation。
- stack ABI version。
- capability 和 tuning profile 摘要。
- worker-turn pinned export。
- socket-call pinned export。
- device attach/detach pinned export。
- control/config pinned export。
- snapshot/diagnostic provider identity。

注册成功前不得把 stack 暴露给 VFS 或 queue worker。

### 11.2 Stack 状态机

```text
Absent
-> Registering
-> Ready
-> Active
-> Quiescing
-> Draining
-> Detached

Active -> Faulted -> Quarantined -> Draining -> Detached
```

- `Ready`：ABI、exports、预算和 boot secret capability 已验证。
- `Active`：允许创建 socket、连接设备和处理 batch。
- `Quiescing`：拒绝新 socket、flow、bind 和 control mutation。
- `Draining`：不再接收 ingress，正在终止 socket、timer 和 packet ownership。
- `Faulted`：native call fault、panic、超时或协议不变量破坏。

### 11.3 Worker 模型

worker 入口和调度对象由常驻 host 拥有。ELM 只拥有协议 state handle。

一个 queue turn 固定分为：

```text
driver reclaim/refill/poll pinned call
-> host ownership validation
-> stack ingress/flow pinned call
-> host ownership validation
-> driver TX submit pinned call
-> arm/recheck/sleep
```

禁止在一个 ELM call guard 内同步进入另一个 ELM。driver 与 stack 的调用必须由 host 分段串联，避免嵌套 fault guard、锁序和 generation 依赖。

### 11.4 Shard 模型

- stack ELM 创建和拥有每个 shard 的协议状态。
- host 只保存 opaque `StackShardHandle`。
- shard mutable state 保持单 writer。
- CPU offline/online 由 host 发出控制事件，stack 决定网络语义。
- flow 不因任务迁移而迁移。
- stack generation 替换时旧 shard handle 全部失效。

## 12. Socket 与 VFS 边界

### 12.1 NetSocketProxy

VFS 不再保存 `Arc<SocketFacade>`，改为常驻 `NetSocketProxy`。proxy 只允许包含：

- socket family/type/protocol。
- opaque `SocketId`。
- stack generation。
- proxy lifecycle generation。
- `PollSource`。
- fd 可见 flags、timeout 和 socket option 镜像。
- 当前 backend 状态与稳定错误。

proxy 禁止包含：

- TCP control block。
- UDP datagram ring。
- stream byte ring。
- flow owner、route 或 neighbor 引用。
- 指向 ELM 对象的 `Arc`、trait object 或裸指针。

### 12.2 Socket 调用

VFS 通过 pinned socket call 提交固定 Rust 请求。请求可以借用当前 syscall 的内核缓冲区，但不得携带用户空间裸指针。

- 用户内存复制由 VFS/通用 uaccess 完成。
- stack 调用必须是可中断、有限时或非阻塞步骤。
- 阻塞等待由 proxy 使用 `PollSource`/waiter 完成，然后重试 stack call。
- ELM 不得在持有 stack 内部锁时让当前任务长期睡眠。
- send/recv 部分成功语义由 stack 回复显式表达。

### 12.3 Readiness

- readiness 的业务真值由 `net.stack` 计算。
- `PollSource` 和 epoll ready list 保持常驻。
- stack 通过 host service 发布 `(SocketId, generation, readiness)`。
- proxy 必须拒绝陈旧 stack generation 或 socket generation 的更新。
- stack detach/fault 时 host 统一发布 `ERROR|HANGUP` 并唤醒 waiter。

### 12.4 Stack 缺失与卸载后的 fd

- stack 从未装载时，`socket(AF_INET/AF_INET6)` 返回 `EAFNOSUPPORT`。
- stack 已存在但不支持请求的 type/protocol 时返回 `EPROTONOSUPPORT`。
- 已有 fd 遇到 stack detach/fault 时，后续数据和控制操作返回 `ENETDOWN`，已排队本地错误可以先按 POSIX 规则读取。
- close 必须始终成功清理 proxy，不依赖旧 ELM generation 仍可调用。
- `dup`、`fork` 和 fd 传递只增加 proxy 引用，不复制 ELM 内部对象。

## 13. 控制面与 provider

`net.stack` 应发布 provider 和 snapshot，用于：

- interface、route、neighbor、socket 和统计快照。
- 网络配置变更。
- consistency check。
- trace 控制。
- stack health 和 generation 查询。

provider 禁止用于：

- RX/TX packet。
- stream/datagram payload。
- 每 flow 定时推进。
- queue completion。
- readiness 高频通知。

控制面 payload 必须使用固定线格式；大快照使用分页 provider snapshot。

## 14. 启动密钥和权限

常驻 host 从 random 子系统取得原始材料后立即拆分：

- `DriverBootMaterial`：RSS key、允许的 queue 数、已经派生的 MAC 或 MAC 派生 nonce。
- `StackBootMaterial`：TCP ISN key、临时端口 key、flow hash key、generation nonce。
- `HostBootMaterial`：不对 ELM 暴露的原始种子和审计 nonce。

规则：

- driver ELM 不得读取 TCP ISN 或端口 key。
- stack ELM 不得读取未授权设备或平台随机材料。
- boot material 通过装载期 capability 和一次性 registration 传递，不进入 provider snapshot、日志或 sysfs。
- stack 热替换默认继承同一次启动的 secret generation，除非明确执行全网络身份重置。

## 15. 资源归属与预算

### 15.1 Stack generation 资源

以下资源必须归属 `net.stack` cell/generation：

- stack registration。
- shard state handle。
- flow、listener、socket protocol state。
- protocol timer。
- route/neighbor/control snapshot 内存。
- provider port 和订阅。
- stack-owned packet/fragment ownership。

### 15.2 Driver generation 资源

以下资源必须归属 driver ELM cell/generation：

- PnP driver handle。
- queue endpoint registration。
- VirtIO ring state。
- IRQ/MSI registration。
- transport 和 control queue state。
- 尚未移交 host 的 DMA allocation。

### 15.3 Host 资源

以下资源由常驻 host 管理，但必须记录关联 generation：

- worker task。
- NetSocketProxy。
- resident DMA pool backing。
- pinned native call handle。
- device/stack broker record。
- host scratch batch。

host 资源必须有明确 owner link；owner generation 退役后不得继续调用其 export。

### 15.4 配额

- ELM 分配必须在正确 current context 下执行并计入 cell budget。
- 每个 worker turn 必须有 CPU deadline。
- stack 内部仍须执行 v2 规范的 flow、socket、timer 和 packet 硬上限。
- host 不得通过常驻分配替 stack 隐藏超额内存。
- generation quiescing 后禁止增加长期资源。

## 16. 并发与锁序

- 硬 IRQ 只 ack/mask、设置 pending 和 wake host worker。
- driver queue ring 只有对应 host worker 通过 pinned call 取得 mutable 执行权。
- stack shard 只有 owner worker 取得 mutable 执行权。
- host 不在持有 registry 写锁时进入 ELM。
- ELM 不在 pinned call 中反向调用 host 的阻塞注册或 detach 操作。
- stack call 与 driver call 不嵌套。
- socket proxy 锁不得与 stack shard 执行权嵌套。
- lifecycle transaction 锁不得与 VFS fd table、runqueue 或设备 queue 锁嵌套。

host 与 ELM 的交互使用消息、batch 和 opaque ID，不通过全局网络 mutex。

## 17. 故障模型

### 17.1 Driver fault

driver pinned call fault 时：

1. 标记 queue/device generation faulted。
2. 保持 IRQ masked。
3. 停止向 stack 投递该设备 packet。
4. 向相关 flow/socket 发布设备错误。
5. 尝试受控 device teardown。
6. 无法安全 teardown 时保持资源和镜像 pinned，进入 Quarantined。

不得在 fault 后直接 drop queue endpoint 或 DMA pool。

### 17.2 Stack fault

stack pinned call fault、panic 或超时时：

1. ELM Core 隔离该 stack generation。
2. host 阻止新 stack 调用。
3. 所有 queue 停止 ingress 并保持设备资源。
4. 所有 proxy 发布 `ERROR|HANGUP`。
5. socket 操作返回 `ENETDOWN`。
6. host 排空已明确归自己的 batch。
7. 对归属不明的 move 前缀执行 fail-closed，保持镜像 pinned 并进入 Quarantined。

### 17.3 Host fault

常驻 host 不属于可卸载 ELM，其不变量失败属于 kernel bug。禁止把 host ownership corruption 伪装成普通 stack ELM fault 继续运行。

## 18. 生命周期

### 18.1 Stack 装载

```text
ELM create
-> initialize stack-local state
-> 解析 pinned exports
-> host preflight registration
-> 建立 generation pin
-> 发布 stack registration
-> attach 已存在设备
-> 创建 shard state
-> 激活 host worker
-> ELM Active
```

任一步失败必须回滚到 `Absent`，不得留下半发布 socket backend 或 worker。

### 18.2 Stack 卸载

```text
拒绝新 socket/control mutation
-> host 停止新 ingress
-> mask 或 park queue worker
-> 发布 proxy ERROR/HANGUP
-> 终止 flow/listener/socket protocol state
-> 排空 packet/timer/control work
-> detach 所有设备逻辑接口
-> 等待 pinned active call 为零
-> 撤销 stack registration
-> finalize
-> 释放 generation
```

第一版 stack unload 会终止现有 INET 连接。VFS proxy 可以继续存在，但只能关闭或返回稳定网络错误。

### 18.3 Stack 热替换

第一版支持 destructive restart replacement：

1. 新 generation 完成装载和 ABI preflight，但不接收流量。
2. 旧 generation 执行完整卸载，现有连接被终止。
3. host 原子发布新 generation。
4. 新 generation 重新 attach 现有设备并允许创建 socket。

第一版禁止宣称保持 TCP 连接的无感热替换。

后续 live migration 必须单独版本化，至少解决：

- TCP sequence/window/retransmit/timer 状态。
- socket ring ownership。
- outstanding packet fragment。
- listener backlog 和 accept child。
- route/neighbor generation。
- migration 期间的新 packet 和 syscall 顺序。

### 18.4 Driver 装卸

driver 装卸遵守第 10.4 节。stack generation 不因单个 driver 卸载而退役；受影响 interface 和 flow 按设备失效语义处理。

## 19. 迁移实施阶段

迁移只维护一套协议实现。同一提交不得让旧常驻栈和新 ELM 栈同时接收真实 packet 或 syscall。

### Phase 0：规范与测试骨架

- 冻结本文。
- 为 host state machine、generation 和 ownership 建立 host 单测。
- 增加 `y/m/n` 组合测试清单。
- 增加禁止 raw module vtable 的静态检查。

### Phase 1：提取契约和空 host

- 把 `libs/net` 收敛为 ABI/contract crate。
- 建立 stack/device broker。
- 删除 `start_workers()` 的一次性扫描和非空 assert。
- host 支持无 stack、无 device 启动。
- 当前协议实现迁入 `net.stack` 工程，但先使用 `y`。

完成标准：默认功能不回退，协议实现已经只存在于 `net.stack` 源码。

### Phase 2：Pinned native call

- 实现 kernel consumer generation-pinned exact-Rust call。
- 接入 fault guard、active call、预算和 detach drain。
- worker turn 改为 batch call。
- 禁止 provider 数据面和 raw trait vtable。

完成标准：`net.stack=y` 与未来 `m` 使用同一调用契约。

### Phase 3：Socket proxy

- VFS 从 `Arc<SocketFacade>` 迁移为 `NetSocketProxy`。
- socket 数据和协议状态移入 `net.stack`。
- readiness 通过 generation-aware host service 发布。
- stack absent/fault/detach 错误语义固定。

完成标准：VFS 不持有任何 stack ELM 内对象或代码引用。

### Phase 4：网络驱动 ELM

- 恢复并迁移新版 VirtIO-net 为 `net.virtio`。
- queue endpoint 使用 pinned driver call。
- DMA storage 和 recycler 常驻化。
- PnP remove 支持 Busy/rollback。
- loopback 使用同一最终契约。

完成标准：driver `m` 可以 late attach、detach 和重新装载。

### Phase 5：切换 `net.stack=m`

- BuildBound 装载顺序满足空 host 模型。
- 生成并自动装载 `net.stack.eki`。
- 删除常驻协议实现链接。
- 验证 stack detach/reload。

完成标准：不装载 `net.stack.eki` 时内核可启动但无 INET；装载后网络恢复。

### Phase 6：故障、替换与性能收尾

- stack/driver native fault 注入。
- Quarantined 资源保留和 retry。
- destructive restart hot replacement。
- 双架构 QEMU、LTP、netbench 和性能 A/B。
- 删除所有临时启动顺序和静态 fallback。

## 20. 测试要求

### 20.1 Host 单元测试

必须覆盖：

- stack/device 任意装载顺序。
- 无 stack、无 queue 和空启动。
- late attach/detach。
- generation ABA 和 stale pinned handle。
- batch 已提交前缀 ownership。
- driver fault、stack fault 和 timeout。
- remove Busy 不释放 IRQ/DMA。
- proxy 在 detach 后返回 `ENETDOWN`。
- readiness 陈旧 generation 被拒绝。
- close 不依赖旧 stack generation。
- `y/m/n` 配置依赖检查。

### 20.2 内核 ktest

必须覆盖：

- loopback `y` 和 `m`。
- VirtIO-net queue attach/detach。
- stack unload 时阻塞 socket 被唤醒。
- stack reload 后新 socket 可用。
- open fd 跨 stack generation 的错误行为。
- outstanding NetBuf lease 导致 detach Busy。
- faulted ELM 不再收到 worker call。
- 所有 owned resource 最终归零。

### 20.3 QEMU

LoongArch64 和 RISC-V64 都必须验证：

1. 无 stack EKI 启动。
2. 自动装载 stack EKI。
3. loopback UDP/TCP。
4. VirtIO-net DHCP、ARP、UDP、TCP。
5. 卸载 stack。
6. 原 fd 返回 `ENETDOWN`。
7. 重新装载 stack。
8. 新连接恢复。
9. 卸载和重新装载 VirtIO-net。
10. ELM snapshot、generation、资源和审计记录正确。

RISC-V VirtIO MMIO 测试继续要求 modern transport；不得为 ELM 化重新加入 legacy queue fallback。

### 20.4 模糊与性质测试

- batch ownership 总和守恒。
- 任意 fault 点后没有双重释放。
- 任意装卸序列后没有 stale generation 调用。
- socket close/dup/fork 与 stack detach 交错不泄漏 proxy。
- driver detach、stack detach 和 CPU offline 交错不死锁。

## 21. 性能门槛

同一内核、QEMU、CPU 数、网卡参数和 workload 下比较 `net.stack=y` 与 `net.stack=m`：

- UDP/TCP 吞吐不得低于 `y` 的 95%。
- packet worker CPU cycles/packet 不得高于 `y` 的 105%。
- ping 和 request/response p99 不得高于 `y` 的 110%。
- 每 packet ELM native entry 平均值必须小于 1；目标是每 batch/turn 一次。
- provider invocation 数不得随 packet 数线性增长。
- 稳态 RX/TX 不得增加每 packet heap allocation。

未达到门槛时必须先提供 profile 和结构原因，不得通过关闭 fault guard、generation 校验或 resource accounting 规避。

## 22. 可观测性

至少提供：

- active stack cell/generation/state。
- stack native calls、fault、timeout 和拒绝次数。
- 每 queue driver call 和 stack call 次数。
- 每次 worker turn packet/batch 数。
- late attach/detach 次数和耗时。
- stack/driver owned resource 数。
- proxy 数、orphan 数和 stale generation 拒绝数。
- detach Busy 原因和 outstanding lease 数。
- provider 控制调用和 snapshot generation。

所有计数必须能区分 host、stack generation 和 driver generation。

## 23. 明确禁止事项

禁止以下实现：

- 只把 `main.rs` 套上 `ElmModule`，协议实现仍常驻内核。
- `net.stack=m` 时仍链接另一份静态 TCP/IP 栈。
- provider 或 `elm-mgr` 进入 packet 热路径。
- 常驻内核保存未受 pin 管理的 ELM trait object/vtable。
- ELM 自建无法回收的永久 worker。
- stack ELM 在硬 IRQ 中运行。
- driver ELM 读取 TCP ISN 或临时端口 secret。
- VFS fd 保存 ELM 内部 `Arc` 或裸指针。
- stack detach 后依靠“不再访问”保留悬空对象。
- remove Busy 后继续释放 IRQ、DMA、queue 或 pool。
- 用启动顺序代替 late attach 状态机。
- 没有 queue 或 stack 时 panic。
- 为 `y` 和 `m` 维护两套协议源码。
- 为迁移方便恢复 smoltcp 或旧 NetDriver 路径。
- 在同一 worker call guard 内嵌套调用 driver ELM 和 stack ELM。
- 未经规范修订改变 socket detach 错误或 live migration 语义。

## 24. 最终验收标准

全部满足后才能宣称“网络栈完成 ELM 化”：

1. `CONFIG_NET_STACK=m` 生成并装载 `net.stack.eki`。
2. `elmctl snapshot` 显示 active `net.stack` cell 和 generation。
3. 常驻 kernel 和 `libs/net` 中不存在协议状态机、route、neighbor、flow、TCP/UDP 或 socket 数据实现。
4. 不加载 stack EKI 时内核正常启动，INET 明确不可用。
5. 加载 stack EKI 后网络设备和 INET socket 自动恢复。
6. stack EKI 可以卸载，已有 fd 被唤醒并返回稳定错误。
7. stack EKI 可以重新装载，新连接恢复。
8. VirtIO-net 和 loopback 可以作为独立 ELM 装卸。
9. stack 与 driver 可按任意顺序装载。
10. 卸载后没有 stale worker、timer、IRQ、DMA lease、packet、socket 或 native call。
11. stack/driver fault 可以隔离，不执行悬空模块代码。
12. 双架构 host test、ktest、QEMU 和用户态网络测试通过。
13. `m` 模式达到第 21 节性能门槛。
14. 当前工作树中不存在第二套网络栈、旧 polling hook 或隐藏 fallback。

## 25. 第一版冻结决策

本文冻结以下决策：

- 一个完整 `net.stack` ELM，不按协议拆分多个数据面 ELM。
- 常驻 worker shell，ELM 不拥有永久调度任务。
- 常驻 `NetSocketProxy`，ELM 拥有 socket 协议和数据状态。
- driver 与 stack 通过常驻 broker 解耦。
- 数据面使用 generation-pinned exact-Rust batch call。
- provider 只用于控制面和快照。
- NetBuf backing/recycler 使用常驻实现，禁止模块 vtable 逃逸。
- stack 与 driver 支持空启动和 late attach。
- 第一版热替换会终止现有 INET 连接，不承诺 TCP live migration。
- `y` 只作为同源集成模式，最终验收要求 `m`。

这些决策发生变化时必须更新本文版本、测试矩阵和性能基线。
