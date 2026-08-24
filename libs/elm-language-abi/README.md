# elm-language-abi

`elm-language-abi` 定义 Hitoshizuku OS `language-runtime` ELM 的稳定通用协议。它是一个
独立的 `no_std` crate，不依赖内核、ELM 实现、分配器或任何具体编程语言。

该 crate 只负责线格式，不负责执行语言代码。未来增加一种语言时，语言支持 ELM 和 SDK
复用这里的类型与合约；内核 loader 不需要认识语言名称，也不需要为语言增加分支。

## V1 边界

跨边界允许使用固定宽度整数、稳定状态码、opaque handle、offset/length 和定长字节数组。
禁止传递 Rust 引用、裸指针、`usize`、trait object、Rust 容器、GC 指针或语言对象地址。

所有 `repr(C)` 结构都包含显式 ABI 版本和结构尺寸，并要求：

- ABI 版本和结构尺寸精确匹配；
- 未定义 flags 和非零保留字段被拒绝；
- ID、owner cell 和 owner generation 必须非零；
- 句柄的 slot 与 generation 必须非零，并由运行时另行绑定 owner；
- 请求与结果长度不能超过固定缓冲区；
- 未知请求状态不能进入状态机。

ELM managed call 的总载荷上限为 256 字节。V1 为请求头保留空间，因此单个请求或结果的
业务内联载荷上限是 `LANGUAGE_FRAME_PAYLOAD_LEN`（192 字节）；完整的最大轮询回复仍恰好是
256 字节。需要传递更大对象时应使用受管 buffer handle/lease，而不是传递地址。

## 稳定合约

V1 发布以下 managed contracts：

```text
language.runtime.catalog@1
language.runtime.backend.register@1
language.runtime.backend.unregister@1
language.runtime.backend.next@1
language.runtime.backend.complete@1
language.runtime.backend.cancel.next@1
language.runtime.backend.cancel.ack@1
language.runtime.backend.next@2
language.runtime.instance.open@1
language.runtime.instance.open@2
language.runtime.instance.close@1
language.runtime.request.submit@1
language.runtime.request.submit@2
language.runtime.request.poll@1
language.runtime.request.cancel@1
language.runtime.request.release@1
language.runtime.drain@1
language.runtime.resource@1
language.runtime.resource.delegated@1
language.runtime.kernel.call@1
language.runtime.kernel.call.delegated@1
```

取消握手、V2 实例身份以及最后两个资源合约都是兼容扩展，不改变旧目录的 12 个基础
contract 计数；消费者应显式检查 `LANGUAGE_RUNTIME_LIFECYCLE_CONTRACTS` 和
`LANGUAGE_RUNTIME_RESOURCE_CONTRACTS`。

`LanguageBackendDescriptorV1` 描述后端，`LanguageInstanceDescriptorV1` 描述 owner 绑定的
实例，`LanguageRequestV1`、`LanguagePollRequestV1`、`LanguageCancelRequestV1` 和
`LanguageDrainRequestV1` 定义请求控制面。调用方应在读取结构字段前调用其 `validate()`。

语言后端通过 `backend.next@1` 拉取 `LanguageBackendWorkV1`，并通过 `backend.complete@1`
提交 `LanguageBackendCompleteRequestV1`。两个工作帧都不超过 256 字节。owner 读取终态结果
后使用 `request.release@1` 释放记录；该操作复用 `LanguagePollRequestV1`，不能释放仍在排队
或执行的请求。

运行中请求的取消采用两阶段协议。consumer 调用 `request.cancel@1` 后，旧 V1 poll 仍返回
`Running`；backend 通过 `backend.cancel.next@1` 观察原因与预期终态，在保证代码、异步回调
和资源访问均已停止后通过 `backend.cancel.ack@1` 确认。确认前 request、instance、backend
以及 owner 资源都不得回收。尚未交给 backend 的 `Queued` 请求可以由 runtime 直接终止。

`instance.open@2` 在不改变 V1 布局的情况下加入 `LanguageArtifactIdentityV2`。它把 package
manifest、实际 AOT artifact 和接口 schema 的 SHA-256 以及稳定 ID 绑定到实例 generation。
该身份由 loader/package policy 验证，wire 结构本身不替代签名或来源认证。

`request.submit@2` 为每项工作声明 `LanguageDelegationPolicyV1`：资源 opcode、capability
rights 和至多一个 kernel operation ID 都必须显式列入。`backend.next@2` 只领取 V2 请求，
并返回 runtime 私下签发的 opaque token；V1 `next` 不会意外领取需要委托的工作。V2 请求与
工作帧仍固定为 256 字节，策略和 8 字节 token 占用原帧空间，因此业务内联 payload 上限为
152 字节，大数据必须走 buffer lease。

backend 代 consumer 调用时使用 `resource.delegated@1` 或 `kernel.call.delegated@1`。这两个
256 字节帧携带原 consumer owner 和 token，但 payload 中的 owner 本身从来不是凭据。runtime
还必须从受信任 managed caller 取得 backend owner+generation，并把 token 与 backend ID、
instance、request、consumer owner+generation 和委托策略同时比较。资源 `request_id` 与
kernel `call_id` 在每个 token 内分别严格递增，重复值会被拒绝；取消、完成、释放、关闭、
drain、注销或 finalize 都会撤销 token。

Rust 代码通过密封的 `LanguageWire` trait 编解码这些结构。实现逐字段使用小端编码，不对
`repr(C)` 内存做转置或裸指针复制；解码要求精确长度并自动调用结构校验。语言 SDK 应实现
同一字段顺序与校验规则，不应把 Rust 内存布局当成序列化格式。

## 资源边界

`resource@1` 是语言无关的 capability 和资源控制面。`LanguageCapabilityV1` 表示由内核
授予的 capability 位集合，`LanguageResourceHandleV1` 表示带 owner/generation 的 opaque
资源句柄。句柄永远不是地址，运行时必须在每次使用时检查 owner、generation、资源种类和
权限 flags。

资源请求与回复分别使用 `LanguageResourceRequestV1` 和 `LanguageResourceResponseV1`，两者
固定为 256 字节，内联 payload 上限仍为 192 字节。操作编号覆盖 capability acquire/revoke、
MMIO map/read/write/unmap、DMA allocate/sync/release、buffer create/lease/read/write/release，以及
IRQ subscribe/poll-or-take/release。

具体参数类型包括：

- `LanguageMmioMapPayloadV1`：物理范围、访问权限、cache mode；范围必须非空且不溢出；
- `LanguageMmioAccessPayloadV1`：1/2/4/8 字节对齐访问；
- `LanguageDmaAllocatePayloadV1` 与 `LanguageDmaSyncPayloadV1`：长度、二次幂对齐、方向和
  cache 同步范围；
- `LanguageBufferLeasePayloadV1`：受限 buffer handle、偏移、长度和读写权限；
- `LanguageBufferIoPayloadV1`：最多 176 字节的内联读写数据，较大传输必须拆分。
- `LanguageIrqSubscribePayloadV1`：owner 范围内的 opaque source ID 和有界 pending 容量；
- `LanguageIrqPollPayloadV1` 与 `LanguageIrqEventStateV1`：非阻塞快照或 take、单调 sequence、
  pending 计数和饱和 overflow 计数。

这些结构只描述安全边界，不会自动授予权限，也不替代 EKI import、ELM trust policy 或内核
capability mask。资源 owner 撤销后，所有关联 handle 和 lease 都必须视为 stale。

IRQ source 必须先由设备层按 `owner cell + generation` 预授权；wire 中的 `source_id` 从不解释
为硬件 IRQ 编号。订阅生成的内核 handler 只操作预分配的原子计数器，不分配、不睡眠、
不执行跨 ELM 回调；释放订阅或撤销 owner 时，内核会使 handler 失活并从 IRQ registry 注销。

## 当前范围

V1 不包含语言 SDK、编译器后端、JIT、解释器、GC、反射或 PCI 驱动实现。MMIO、DMA、共享
buffer 和 IRQ event 只定义语言无关的固定 wire 边界与内核资源生命周期，不替代具体设备
驱动；具体执行仍由内核和 `language-runtime` 提供。

完整架构、安全边界和当前 `y` 集成限制见
[`LANGUAGE_RUNTIME.md`](../../LANGUAGE_RUNTIME.md)。该 crate 定义协议不等于已有任何外语
backend；语言 SDK 与 runtime 应在独立仓库维护。

运行协议测试：

```sh
cargo test -p elm-language-abi --locked
```
