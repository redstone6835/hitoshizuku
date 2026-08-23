# elm-language-abi

`elm-language-abi` 定义 Hitoshizuku OS `language-runtime` ELM 的 V1 通用协议。它是一个
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
256 字节。需要传递更大对象时应在后续 ABI 中引入受管 buffer handle，而不是传递地址。

## 稳定合约

V1 发布以下 managed contracts：

```text
language.runtime.catalog@1
language.runtime.backend.register@1
language.runtime.backend.unregister@1
language.runtime.backend.next@1
language.runtime.backend.complete@1
language.runtime.instance.open@1
language.runtime.instance.close@1
language.runtime.request.submit@1
language.runtime.request.poll@1
language.runtime.request.cancel@1
language.runtime.request.release@1
language.runtime.drain@1
```

`LanguageBackendDescriptorV1` 描述后端，`LanguageInstanceDescriptorV1` 描述 owner 绑定的
实例，`LanguageRequestV1`、`LanguagePollRequestV1`、`LanguageCancelRequestV1` 和
`LanguageDrainRequestV1` 定义请求控制面。调用方应在读取结构字段前调用其 `validate()`。

语言后端通过 `backend.next@1` 拉取 `LanguageBackendWorkV1`，并通过 `backend.complete@1`
提交 `LanguageBackendCompleteRequestV1`。两个工作帧都不超过 256 字节。owner 读取终态结果
后使用 `request.release@1` 释放记录；该操作复用 `LanguagePollRequestV1`，不能释放仍在排队
或执行的请求。

Rust 代码通过密封的 `LanguageWire` trait 编解码这些结构。实现逐字段使用小端编码，不对
`repr(C)` 内存做转置或裸指针复制；解码要求精确长度并自动调用结构校验。语言 SDK 应实现
同一字段顺序与校验规则，不应把 Rust 内存布局当成序列化格式。

## 当前范围

V1 不包含语言 SDK、编译器后端、JIT、解释器、GC、反射、MMIO、DMA、IRQ、PCI 或共享
内存。后续语言实现只能通过普通 ELM 和这里定义的边界接入。

完整架构、安全边界和当前 `y` 集成限制见
[`LANGUAGE_RUNTIME.md`](../../LANGUAGE_RUNTIME.md)。该 crate 定义协议不等于已有任何外语
backend；语言 SDK 与 runtime 应在独立仓库维护。

运行协议测试：

```sh
cargo test -p elm-language-abi --locked
```
