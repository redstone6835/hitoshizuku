# 内核随机服务

`kernel.random` 是通用随机后端，而不是某个 platform 硬件 RNG 驱动。它混合架构层和
启动加载器提供的熵，维护 ChaCha20 CSPRNG，并通过 `general::dev::random` 为内核及
`/dev/random`、`/dev/urandom` 路径提供字节。

## 职责与边界

- 维护 16 个 `u64` 的输入池和独立的熵估值，上限 1024 bit。
- 累计至少 256 bit **已声明信用**后完成安全播种并唤醒等待者。
- 使用 256-bit key、96-bit nonce 的 20-round ChaCha20 生成输出；每 1 MiB 或 counter
  边界前重新播种。
- 通过 `RandomBackend` 实现熵注入、用户写入和 reseed，并由通用随机服务暴露稳定入口。
- 注册一个 `RandomBackend`，由通用设备层决定具体字符设备投影。

本 crate 不直接读取特定 CSR、RDRAND、TPM 或 virtio-rng，也不判断硬件采样质量。架构
相关采样必须先实现并注册 `general::dev::random_source::EntropySource`。它也不把地址、
时间戳或测试数据自动视为可信熵。

## 模块选择

该实现没有架构专用代码，在两个 bare-metal target 上都可用：

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `kernel-random` |
| ELM 名称 | `kernel.random` |
| ELM 模式/阶段 | `y` / `device` |
| 配置项 | `CONFIG_RANDOM`，默认 `y` |
| target 限制 | 无 |
| 模块顺序 | 无显式 `after` 依赖 |

## 对象与生命周期

`RANDOM_CORE` 是整个内核运行期共享的静态 `RandomCore`。ELM `initialize` 采集启动样本、
执行首次 reseed，并将 `RandomBackendImpl` 注册到通用随机服务；返回的 handle 存在
`RandomElm` 中。`finalize` 通过该 handle 注销后端，失败时返回 busy 类错误。

模块卸载只撤销服务后端，不清零静态池、key 或统计状态；同一映像内再次加载会继续使用
原静态对象并重新采样。这一点与可热替换、可擦除密钥的独立硬件后端不同。

## 读取与熵信用语义

- secure/entropy 读取在未安全播种时：阻塞模式进入 `WaitQueue`，非阻塞模式返回零长度。
- insecure（`/dev/urandom` 语义）和
  `general::dev::random::fill(..., RandomReadMode::Insecure)` 不等待 `secure_ready`，极早期
  调用可能只基于弱启动状态；需要密钥安全的调用方必须选择 secure 路径或确认就绪。
- 只有调用者显式提供的 `entropy_bits` 会推进安全就绪。普通启动 hint 可以混入，但默认
  记为 0 bit。
- bootloader seed 按 8 bit/byte 计入；用户写入当前保守记为 1 bit/byte。用户可控输入
  不应被安全边界外的代码抬高信用。
- 输出不会扣减熵估值；该估值表示 CSPRNG 是否曾被充分播种，不是可消耗字节配额。

## 并发与安全边界

- 输入池和 CSPRNG 分别由 Acquire/Release 自旋锁保护；就绪位和统计使用原子变量。
- 等待者在唤醒后重新检查 `secure_ready`，并处理打断信号；调度器未 ready 的极早期路径
  只能自旋等待。
- 自定义输入池负责混合和信用记账，ChaCha20 负责输出。输入池不是外部熵质量检测器；
  系统安全最终依赖被授予信用的 seed 真实不可预测。
- 测试用确定性 seed 只能用于测试，不得在生产构建中授予真实熵信用。

## 依赖关系

主要依赖 `general::dev::{random,random_source}`、`sched::WaitQueue`、`elm` 和
`allocator`。本模块不经 PnP 匹配 platform 设备；具体熵源可由架构层或其它驱动通过
通用 random-source 接口注入。

## 检查与构建

```sh
cargo check -p kernel-random --lib --target loongarch64-unknown-none
cargo check -p kernel-random --lib --target riscv64gc-unknown-none-elf

cargo xtask config
cargo xtask modules --target riscv64gc-unknown-none-elf

cargo elm check drivers/random --arch riscv64
cargo elm build drivers/random --arch riscv64
```

仓库级 `y/m/n` 选择以 [`../Modules.toml`](../Modules.toml) 和 `.config` 为准。本模块
默认以 `y` 模式集成；只有切换到 `m` 模式发布独立 EKI 时才涉及签名。
