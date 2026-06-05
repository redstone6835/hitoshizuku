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
//! general::dev::random_source::with_entropy_source(|src| {
//!     random_core().reseed_from_source(src);
//! });
//! ```
//!
//! 不在 `general` 中直接依赖 `arch` 是分层原则的要求：
//! `general` 仅消费 trait object，arch 通过注册路径反向往里注入。

use core::any::Any;

/// 一次启动期可采集的"原始熵源"。
///
/// arch 层提供一个实现，并在 `register_entropy_source` 中挂载。
/// general 层（random 驱动）只通过 `dyn EntropySource` 与其交互。
///
/// 所有方法**必须**：
/// - 简单（不应分配内存、不会 panic、不会睡眠）；
/// - 多次调用返回的字节流在攻击者视角下不可预测；
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
    fn sample(&self, out: &mut [u8]) {
        let ts = self.timestamp().to_le_bytes();
        let sp = self.stack_pointer_hint().to_le_bytes();
        let sa = self.self_address_hint().to_le_bytes();
        let total = ts.len() + sp.len() + sa.len();
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
        let _ = total;
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

/// 取出已注册熵源的便捷包装：调用 `f`，若未注册则不调用。
pub fn with_source<R>(f: impl FnOnce(&dyn EntropySource) -> R) -> Option<R> {
    let guard = REGISTERED_SOURCE.lock();
    let src = (*guard).as_ref()?;
    Some(f(*src))
}
