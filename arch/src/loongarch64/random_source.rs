//! LoongArch64 平台熵源实现。
//!
//! 暴露 [`EntropySource`] trait 的具体实现，由 `kernel` 启动早期通过
//! [`register_entropy_source`] 注入到 `general` 的随机子系统。
//!
//! # 熵源组成
//!
//! - `timestamp()`：`rdtime.d` 读出的稳定计数器，按 kernel 的扫频
//!   因子转为纳秒。稳定计数器由 CPU 硬件保证单调递增，各核共享。
//! - `stack_pointer_hint()`：把当前内核栈顶 `sp` 暴露为熵的一部分。
//!   攻击者要命中目标 sp 需要在 KASLR 范围内爆破，足以视作弱熵。
//! - `self_address_hint()`：返回 0（这里不需要把 random core 的
//!   私有地址带进熵池——地址随机化由 stack pointer 代理）。
//! - `sample()`：默认实现 8 + 8 + 8 = 24 字节就够；这里 override
//!   成包含 PC（程序计数器）的版本，多 8 字节熵。

use core::any::Any;

use general::dev::random_source::{EntropySource, register_entropy_source};

// ──────────────────────── 时间戳 / 栈指针 / PC 抓取 ────────────────────────

/// 抓取 `rdtime.d` 计数器的 loongarch64 字节码。
///
/// 由具体架构（loongarch64）提供 `kernel_timestamp_ns`；但本结构体是
/// 架构无关的 trait impl，所以允许走 `super::specific` 的入口。
#[inline]
fn rdtime_ns() -> u64 {
    // kernel_timestamp_ns 来自 arch::specific 内的 loongarch64 实现。
    // 它内部用 rdtime.d 读硬件稳定计数器。
    super::specific::kernel_timestamp_ns()
}

#[inline]
fn read_sp() -> u64 {
    let sp: u64;
    // 编译器在这里允许输出 sp；nomem 表明 asm 不读内存。
    unsafe {
        core::arch::asm!("move {sp}, $sp", sp = out(reg) sp, options(nomem, preserves_flags));
    }
    sp
}

#[inline]
fn read_pc() -> u64 {
    // 任何 jumps/branchs link 到 ra，pc 不通用直接读；改读 ra 当 PC
    // 代理（在取样路径上 ra 等于 PC 是合理近似）。
    let ra: u64;
    unsafe {
        core::arch::asm!("move {ra}, $ra", ra = out(reg) ra, options(nomem, preserves_flags));
    }
    ra
}

// ──────────────────────── EntropySource 实现 ──────────────────────────────

struct LoongArchEntropySource;

impl EntropySource for LoongArchEntropySource {
    fn timestamp(&self) -> u64 {
        rdtime_ns()
    }

    fn stack_pointer_hint(&self) -> u64 {
        read_sp()
    }

    fn self_address_hint(&self) -> u64 {
        0
    }

    fn name(&self) -> &'static str {
        "loongarch64"
    }

    fn sample(&self, out: &mut [u8]) {
        // 32 字节采样：timestamp(8) + sp(8) + pc(8) + 第二次 timestamp(8)
        let ts1 = self.timestamp().to_le_bytes();
        let sp = self.stack_pointer_hint().to_le_bytes();
        let pc = read_pc().to_le_bytes();
        let ts2 = self.timestamp().to_le_bytes();
        let mut pos = 0usize;
        for chunk in [&ts1, &sp, &pc, &ts2] {
            let n = (out.len() - pos).min(chunk.len());
            if n == 0 {
                break;
            }
            out[pos..pos + n].copy_from_slice(&chunk[..n]);
            pos += n;
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static SOURCE: LoongArchEntropySource = LoongArchEntropySource;

/// 把 loongarch64 熵源挂到 `general` 的注册表里。
pub fn register() {
    register_entropy_source(&SOURCE);
}
