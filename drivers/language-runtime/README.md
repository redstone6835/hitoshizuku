# language-runtime

`language-runtime` 是 Hitoshizuku OS 默认以 `y` 模式集成的语言无关 ELM 基础服务。它维护
backend、instance 和异步 request 的有界注册表，把 ELM 的 cell/generation 身份落实到每个
对象，并提供未来语言支持 ELM 可以复用的调度边界。

该 crate 不执行某种语言，也不包含 JIT、解释器、GC、反射、设备 API 或 C/C++/C# SDK。
完整设计见 [`LANGUAGE_RUNTIME.md`](../../LANGUAGE_RUNTIME.md)，固定 wire 类型位于
[`elm-language-abi`](../../libs/elm-language-abi/)。

## 职责

- 注册/注销由 provider cell+generation 拥有的语言 backend；
- 为 consumer 创建 owner 绑定、带 generation 的 instance handle；
- 接受最多 192 字节业务载荷的异步请求，并实施总量、owner、backend 与 owner/backend
  组合容量限制；
- 让 backend owner 通过 `next` 领取工作、通过 `complete` 提交结果；
- 为 V2 工作签发受限 delegation token，使 backend 能在不伪造 consumer 身份的前提下代调
  资源与 kernel operation；
- 提供非消费式 `poll`、两阶段 `cancel observe/ack` 和终态 `release`；
- 通过 V2 实例入口绑定 package、AOT artifact 与接口 schema 构建身份；
- 在 owner drain 或 ELM quiesce/finalize 时撤销状态并回收资源。

当前实现上限为 32 个 backend、256 个 instance、每个 owner 32 个 instance、1024 个总请求、
每个 owner 64 个未释放请求、每个 backend 256 个未释放请求，以及 1024 条 owner 撤销记录。
backend 描述符还会限制该 backend 的实例总量和每个 owner/backend 组合的请求量。调用方应
读取 `catalog`，这些实现数值不是稳定 ABI 常量。

## Contract

| Contract | 输入/输出摘要 |
| --- | --- |
| `language.runtime.catalog@1` | 空输入，返回 `LanguageRuntimeCatalogV1` |
| `language.runtime.backend.register@1` | backend 描述符，返回已登记描述符 |
| `language.runtime.backend.unregister@1` | backend owner 与 ID，成功为空回复 |
| `language.runtime.backend.next@1` | backend owner 与 ID，返回一项固定大小工作帧 |
| `language.runtime.backend.next@2` | 仅领取 V2 工作，返回策略和 opaque delegation token |
| `language.runtime.backend.complete@1` | backend owner、request ID、状态与结果，成功为空回复 |
| `language.runtime.backend.cancel.next@1` | backend 观察一项运行中请求的取消通知 |
| `language.runtime.backend.cancel.ack@1` | backend 确认请求与异步回调已经停止 |
| `language.runtime.instance.open@1` | consumer owner 与 backend ID，返回 instance 描述符 |
| `language.runtime.instance.open@2` | 额外绑定 package/artifact/schema 构建身份 |
| `language.runtime.instance.close@1` | owner、backend ID 与 handle，成功为空回复 |
| `language.runtime.request.submit@1` | instance、request ID、opcode 与内联载荷，返回排队摘要 |
| `language.runtime.request.submit@2` | 额外声明资源 rights/opcodes 和 kernel operation 范围 |
| `language.runtime.request.poll@1` | owner 与 request ID，返回状态和结果 |
| `language.runtime.request.cancel@1` | owner、request ID 与原因，返回取消后的状态 |
| `language.runtime.request.release@1` | owner 与 request ID，仅终态可删除 |
| `language.runtime.drain@1` | owner，返回 backend/instance/request 回收计数 |
| `language.runtime.resource@1` | capability、MMIO、DMA 和 buffer lease 的固定资源帧 |
| `language.runtime.resource.delegated@1` | backend 携 token 代表原 consumer 调用获准资源操作 |
| `language.runtime.kernel.call@1` | EKI operation ID、owner 和有界输入/输出帧 |
| `language.runtime.kernel.call.delegated@1` | backend 携 token 调用唯一获准的 operation ID |

所有携带 owner 的 wire 输入都不可信。managed export 必须从 `ManagedRequest` 取得真实调用方
cell/generation，再与输入结构逐项比较；只校验 payload 中自报的 owner 不构成授权。

## 状态与生命周期

请求由 `submit` 进入 `Queued`，被 backend `next` 领取后进入 `Running`，由 `complete`
进入 `Completed` 或 `Failed`。`Queued` 请求可以直接变为 `Canceled`/`Expired`；`Running`
请求只能先记录 cancellation notice，backend 通过 `cancel.next` 观察后再用 `cancel.ack` 确认
停止，随后才进入 `Canceled`/`Expired`。为保持 V1 兼容，确认前 `poll@1` 继续报告
`Running`。四个终态都保留到 owner 调用 `release`；`poll` 本身不释放记录。

模块初始化和恢复时接受新对象；pause/quiesce 时停止接受新对象，未领取请求直接过期，已领取
请求进入取消握手。存在未确认工作时 `drain` 和 `finalize` 返回 `BUSY`，不会提前删除记录或
撤销资源。语言 backend 卸载前应停止普通 `next`、继续处理 `cancel.next` 并确认已领取工作；
consumer 卸载前应停止 submit，重复调用 `drain` 直到成功。

V2 token 由 runtime 私有域签发，绑定 backend owner+generation、backend ID、instance、request、
consumer owner+generation 和显式策略。调用方只看到 opaque handle，不能用 payload 中的
consumer owner 代替它。每个 token 分别记录严格递增的资源 request ID 与 kernel call ID，
因此旧帧不能重放。取消一经请求便禁止新代调用；已经通过校验的同步调用计入 inflight，直到
返回前 `complete`、`cancel.ack`、`drain` 和回收路径都返回 `BUSY`。complete、cancel、release、
instance close、drain、backend unregister 与 finalize 均会撤销 token。

`drain` 在清理运行时注册表后还会调用 `general.dev.language.resource.revoke_owner`，因此
同一个 ELM generation 的 capability、DMA handle 和 buffer lease 不会在 runtime 对象清空后
继续存活。`finalize` 由 kernel finalize hook 清理语言资源表；全局 reset 是 Rust-only 的
内核内部操作，不在 EKI kernel symbol catalog 中。动态 `m` 模式通过 loader 审核的 kernel symbol `DirectImport`
调用该桥；未绑定时返回 `UNSUPPORTED`，不会把空槽当作函数地址调用。`elm-integrated` 的
`y` 模式则直接调用 `general::dev::language::{dispatch,revoke_owner,call}`；资源表的全局
清理由 kernel finalize hook 负责。

资源帧的统一定义位于 `elm-language-abi`。当前内核已接通 capability 与 DMA allocate/sync/
release；MMIO 和受管 buffer 的 wire、权限、范围及 lease 生命周期已经固定，具体硬件映射
能力在对应 General resource provider 注册前保持 `UNSUPPORTED`。这让 SDK 可以先稳定发布，
而不会把未审核的物理地址操作伪装成可用能力。

## `y` 模式

默认配置把本服务作为 `elm-integrated` Rust crate 常驻内核。集成分支直接进入 General 的
语言资源桥，managed export 仍只暴露固定 wire 和通用 contract；动态 consumer 与未来语言
backend 不需要知道该 provider 最终是 `y` 还是 `m`。本 crate 仍不代表已经实现任何外语
runtime 或 SDK。

## 检查

```sh
cargo test -p language-runtime --lib --locked
cargo check -p language-runtime --lib --features elm-integrated
cargo check -p language-runtime --lib --target loongarch64-unknown-none
cargo check -p language-runtime --lib --target riscv64gc-unknown-none-elf
cargo xtask modules --target loongarch64-unknown-none
```

语言无关的 `LanguagePackage.toml`、`LanguageBridge.toml`、schema 和 Rust SDK 由独立
`cargo-elm` 工具生成。一个不引入新语言的调用示例见
[`examples/rust-language-sdk`](../../examples/rust-language-sdk)。
