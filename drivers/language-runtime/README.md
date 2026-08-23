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
- 接受最多 192 字节业务载荷的异步请求，并实施总量、owner 和 backend 三层容量限制；
- 让 backend owner 通过 `next` 领取工作、通过 `complete` 提交结果；
- 提供非消费式 `poll`、显式 `cancel` 和终态 `release`；
- 在 owner drain 或 ELM quiesce/finalize 时撤销状态并回收资源。

当前实现上限为 32 个 backend、256 个 instance、1024 个总请求、每个 owner 64 个未释放
请求，以及 1024 条 owner 撤销记录。调用方应读取 `catalog`，这些数值不是稳定 ABI 常量。

## Contract

| Contract | 输入/输出摘要 |
| --- | --- |
| `language.runtime.catalog@1` | 空输入，返回 `LanguageRuntimeCatalogV1` |
| `language.runtime.backend.register@1` | backend 描述符，返回已登记描述符 |
| `language.runtime.backend.unregister@1` | backend owner 与 ID，成功为空回复 |
| `language.runtime.backend.next@1` | backend owner 与 ID，返回一项固定大小工作帧 |
| `language.runtime.backend.complete@1` | backend owner、request ID、状态与结果，成功为空回复 |
| `language.runtime.instance.open@1` | consumer owner 与 backend ID，返回 instance 描述符 |
| `language.runtime.instance.close@1` | owner、backend ID 与 handle，成功为空回复 |
| `language.runtime.request.submit@1` | instance、request ID、opcode 与内联载荷，返回排队摘要 |
| `language.runtime.request.poll@1` | owner 与 request ID，返回状态和结果 |
| `language.runtime.request.cancel@1` | owner、request ID 与原因，返回取消后的状态 |
| `language.runtime.request.release@1` | owner 与 request ID，仅终态可删除 |
| `language.runtime.drain@1` | owner，返回 backend/instance/request 回收计数 |
| `language.runtime.resource@1` | capability、MMIO、DMA 和 buffer lease 的固定资源帧 |
| `language.runtime.kernel.call@1` | EKI operation ID、owner 和有界输入/输出帧 |

所有携带 owner 的 wire 输入都不可信。managed export 必须从 `ManagedRequest` 取得真实调用方
cell/generation，再与输入结构逐项比较；只校验 payload 中自报的 owner 不构成授权。

## 状态与生命周期

请求由 `submit` 进入 `Queued`，被 backend `next` 领取后进入 `Running`，由 `complete`
进入 `Completed` 或 `Failed`。取消可以产生 `Canceled`，运行时撤销可以产生 `Expired`。
这四个终态都保留到 owner 调用 `release`；`poll` 本身不释放记录。

模块初始化和恢复时接受新对象；pause/quiesce 时停止接受新对象并使未完成请求过期；
finalize 清空注册表。语言 backend 卸载前应停止 `next`、处理已领取工作并调用 `drain`，
consumer 卸载前应停止 submit、释放终态请求并关闭 instance。

`drain` 在清理运行时注册表后还会调用 `general.dev.language.resource.revoke_owner`，因此
同一个 ELM generation 的 capability、DMA handle 和 buffer lease 不会在 runtime 对象清空后
继续存活。`finalize` 调用 `general.dev.language.resource.reset`；内核侧 handler 会释放
语言资源表中的拥有对象。未安装对应 kernel symbol 时返回 `UNSUPPORTED`，不会把空的
`DirectImport` 槽当作函数地址调用。

资源帧的统一定义位于 `elm-language-abi`。当前内核已接通 capability 与 DMA allocate/sync/
release；MMIO 和受管 buffer 的 wire、权限、范围及 lease 生命周期已经固定，具体硬件映射
能力在对应 General resource provider 注册前保持 `UNSUPPORTED`。这让 SDK 可以先稳定发布，
而不会把未审核的物理地址操作伪装成可用能力。

## `y` 模式限制

默认配置把本服务作为 `elm-integrated` Rust crate 常驻内核。当前集成构建不会保留动态 EKI
的 managed trampoline 和 `.elm.meta`，所以集成 Rust consumer 使用 crate 的普通 Rust
入口，ABI crate 则保存稳定 contract 与 wire 定义。动态 `m` consumer 能否绑定到 `y`
provider 仍取决于 ELM 的通用 binding 能力；当前不能把本 crate 描述为已经支持外语 ELM。

未来若补齐该组合，应扩展语言无关的 ELM binding，不在 loader 中增加语言名称判断。

## 检查

```sh
cargo test -p language-runtime --lib --locked
cargo check -p language-runtime --lib --target loongarch64-unknown-none
cargo check -p language-runtime --lib --target riscv64gc-unknown-none-elf
cargo xtask modules --target loongarch64-unknown-none
```

语言无关的 `LanguagePackage.toml`、`LanguageBridge.toml`、schema 和 Rust SDK 由独立
`cargo-elm` 工具生成。一个不引入新语言的调用示例见
[`examples/rust-language-sdk`](../../examples/rust-language-sdk)。
