//! 架构无关的随机数熵源抽象。
//!
//! `general` 层不直接包含 `cfg(target_arch = ...)` 的内联汇编——所有
//! 需要架构特化信息才能采集的"原始熵"（时间戳、栈指针、内存布局指
//! 针等）都通过本模块定义的 [`EntropySource`] trait 暴露。
//!
//! # 用法
//!
//! ```rust,ignore
//! // arch 侧（loongarch64/riscv64）提供具体实现并注册：
//! arch::register_entropy_source(loongarch64_entropy_source());
//!
//! // general 侧启动时调用：
//! general::dev::random_source::with_source(|src| {
//!     let mut buf = [0u8; 64];
//!     let sample = src.sample_with_credit(&mut buf);
//!     general::dev::random::add_bootloader_randomness(
//!         &buf[..sample.bytes_written],
//!         sample.entropy_bits,
//!     );
//! });
//! ```
//!
//! 不在 `general` 中直接依赖 `arch` 是分层原则的要求：
//! `general` 仅消费 trait object，arch 通过注册路径反向往里注入。

use core::any::Any;

/// 一次熵源采样的结果。
///
/// `bytes_written` 表示已经写入调用方缓冲区的原始样本长度；`entropy_bits`
/// 是熵源实现愿意为这段样本显式承担的 credit。两者必须分开表达：时间戳、
/// 栈地址、self 地址这类值可以混入池子增加状态扰动，但除非平台能证明它们
/// 在攻击者视角下有足够不可预测性，否则不能默认按 full entropy 记账。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntropySample {
    pub bytes_written: usize,
    pub entropy_bits: u64,
}

impl EntropySample {
    pub const fn none() -> Self {
        Self {
            bytes_written: 0,
            entropy_bits: 0,
        }
    }

    pub fn new(bytes_written: usize, entropy_bits: u64) -> Self {
        let max_bits = (bytes_written as u64).saturating_mul(8);
        Self {
            bytes_written,
            entropy_bits: entropy_bits.min(max_bits),
        }
    }
}

/// 一次启动期可采集的"原始熵源"。
///
/// arch 层提供一个实现，并在 `register_entropy_source` 中挂载。
/// general 层（random 驱动）只通过 `dyn EntropySource` 与其交互。
///
/// 所有方法**必须**：
/// - 简单（不应分配内存、不会 panic、不会睡眠）；
/// - 可混入的原始样本尽量包含攻击者难以复现的运行时状态；
/// - 只有 `sample_with_credit()` 显式返回的 bit 数会增加熵估计；
/// - 在架构所允许的最小开销下完成。
pub trait EntropySource: Send + Sync {
    /// 返回一个 64-bit 单调时间戳。
    ///
    /// 实现应使用硬件单调计数器（loongarch64: `rdtime.d`、riscv64:
    /// `rdtime`）而非 PIT/HPET 等软件时钟。
    fn timestamp(&self) -> u64;

    /// 返回当前栈指针的近似值。
    ///
    /// 实现可以读 `$sp`/`$r3`，也可以只返回一个 0（实现层没有 SP 寄存器
    /// 时）。这是 KASLR 类地址随机的代理，攻击者难以预测。
    fn stack_pointer_hint(&self) -> u64;

    /// 返回一个用于"熵源指纹"的运行时地址，例如 random core 自身地址。
    ///
    /// 默认实现返回 0，表示不提供此项。
    fn self_address_hint(&self) -> u64 {
        0
    }

    /// 实现类名（用于日志/诊断）。
    fn name(&self) -> &'static str;

    /// 取一段熵样本，调用方负责消费。
    ///
    /// 默认实现：从 8 字节时间戳 + 8 字节栈指针 + 8 字节 self 地址拼接
    /// 出 24 字节样本。arch 实现可覆盖以包含更多 arch 信息（cycle 高
    /// 32 位、CPU id 等）。
    ///
    /// 兼容说明：旧实现只覆盖 `sample()` 时，默认 `sample_with_credit()`
    /// 仍会调用它，但只能按 `sample_bytes_hint()` 的保守长度消费，且不会
    /// 从这里推导 full credit。新实现应优先覆盖 `sample_with_credit()`，
    /// 把“写入了多少字节”和“能记多少熵”同时返回。
    fn sample(&self, out: &mut [u8]) {
        let ts = self.timestamp().to_le_bytes();
        let sp = self.stack_pointer_hint().to_le_bytes();
        let sa = self.self_address_hint().to_le_bytes();
        let mut written = 0usize;
        for src in [&ts, &sp, &sa] {
            let n = (out.len() - written).min(src.len());
            if n == 0 {
                break;
            }
            out[written..written + n].copy_from_slice(&src[..n]);
            written += n;
        }
        // 不够 24 字节的话不补零：调用方应能接受短样本。
    }

    /// `sample()` 默认会写入的字节数 hint。
    ///
    /// 旧实现如果只覆盖 `sample()` 并写入更多字段，应同步覆盖本方法或直接
    /// 覆盖 `sample_with_credit()`；否则 random 只会消费前 24 字节，避免把
    /// 缓冲区里未定义的尾部内容当成真实样本。
    fn sample_bytes_hint(&self) -> usize {
        24
    }

    /// 默认样本可记入的熵 bit 数。
    ///
    /// 默认返回 0：timestamp/stack/self address 仍然值得 mix，因为它们能扰动
    /// 池状态并打散可复现启动路径；但这不等价于“攻击者无法预测”，不能自动
    /// 当成 full entropy。平台若有硬件 RNG、固件 seed 或经验证的 jitter
    /// 模型，应覆盖 `sample_with_credit()` 或本方法给出保守 credit。
    fn sample_entropy_credit_bits(&self, bytes_written: usize) -> u64 {
        let _ = bytes_written;
        0
    }

    /// 取一段熵样本并显式返回本次 credit。
    ///
    /// 这是 random 子系统应使用的主接口；`sample()` 只保留给旧实现兼容。
    fn sample_with_credit(&self, out: &mut [u8]) -> EntropySample {
        self.sample(out);
        let written = self.sample_bytes_hint().min(out.len());
        EntropySample::new(written, self.sample_entropy_credit_bits(written))
    }

    /// 向下转型（用于 arch 特定的 ioctl 路径，random 不使用）。
    fn as_any(&self) -> &dyn Any {
        &()
    }
}

// ── 注册表 ────────────────────────────────────────────────────────────────

use spin::mutex::Mutex;

static REGISTERED_SOURCE: Mutex<Option<&'static dyn EntropySource>> = Mutex::new(None);

/// 架构侧注册入口：把 `&'static dyn EntropySource` 装到全局表里。
///
/// 多次注册以最后一次为准。`None` 表示 arch 没有熵源——此时 random 子
/// 系统退化为"仅靠用户态 write 与时间戳派生的弱熵"。
///
/// # Safety
///
/// `src` 必须是 `'static`，且其内部状态不依赖调度。
pub fn register_entropy_source(src: &'static dyn EntropySource) {
    *REGISTERED_SOURCE.lock() = Some(src);
}

/// 取出已注册的熵源；`None` 表示没有 arch 实现。
pub fn installed_source() -> Option<&'static dyn EntropySource> {
    REGISTERED_SOURCE.lock().as_ref().copied()
}

/// 从当前架构熵源采集一次样本。
#[kernel_symbols::export(
    name = "general.dev.random_source.sample",
    contract = "kernel.general.entropy-source@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn sample(output: &mut [u8]) -> Option<EntropySample> {
    installed_source().map(|source| source.sample_with_credit(output))
}

/// 读取当前架构熵源的单调时间戳。
#[kernel_symbols::export(
    name = "general.dev.random_source.timestamp",
    contract = "kernel.general.entropy-source@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE
)]
pub fn timestamp() -> Option<u64> {
    installed_source().map(EntropySource::timestamp)
}

/// 取出已注册熵源的便捷包装：调用 `f`，若未注册则不调用。
pub fn with_source<R>(f: impl FnOnce(&dyn EntropySource) -> R) -> Option<R> {
    let guard = REGISTERED_SOURCE.lock();
    let src = (*guard).as_ref()?;
    Some(f(*src))
}
