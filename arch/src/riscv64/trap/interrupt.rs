//! RISC-V64 的中断逻辑。
//!
//! RISC-V64 的中断管理主要通过 SSTATUS 寄存器中的 SIE 位来控制中断的使能状态。我
//! 们提供了保存和恢复中断状态的函数，以及直接使能和禁用中断的接口。这些函数使用
//! 内联汇编直接操作 SSTATUS 寄存器，以确保高效和正确地管理中断状态。
//!
//! 本模块还提供 MSGI（message interrupt）相关接口，用于读取/配置 CSR_SIE 并向
//! 目标 CPU 核心通过 CSR_MSIP 发送消息中断。

use crate::*;

/// 本地中断控制辅助实现（基于 `SSTATUS.SIE`）。
pub struct Riscv64InterruptOps;
/// 核间消息中断控制辅助实现（基于 `CSR_SIE / CSR_MSIP`）。
pub struct Riscv64MessageInterruptOps;

impl Riscv64InterruptOps {
    /// 保存中断状态。
    ///
    /// # 返回值
    ///
    /// 返回当前 `SSTATUS` 原始值，供 [`restore_interrupt_state`] 恢复 SIE 位。
    #[inline]
    pub unsafe fn save_interrupt_state() -> usize {
        read_csr!(sstatus)
    }

    /// 恢复中断状态。
    ///
    /// # 参数
    ///
    /// - `state`: 之前保存的 `SSTATUS` 原始值。
    #[inline]
    pub unsafe fn restore_interrupt_state(state: usize) {
        // 无分支恢复：先清 SIE，再用 csrs 写回原来的 SIE 位。
        // 如果 state 中 SIE=0，csrs 写 0 是 no-op；如果 SIE=1，csrs 置位。
        clear_csr!(sstatus, SSTATUS_SIE);
        set_csr!(sstatus, state & SSTATUS_SIE);
    }

    /// 使能中断（原子置位 `SSTATUS.SIE`）。
    #[inline]
    pub unsafe fn enable_interrupts() {
        // 这里只触碰 `SSTATUS.SIE`，不改变当前特权级或地址翻译状态。也就是说，这个接口的
        // 职责只是打开本地可屏蔽中断响应能力，不负责更广义的执行上下文切换。
        unsafe {
            core::arch::asm!(
                "csrs {csr}, {mask}",
                csr = const CSR_SSTATUS,
                mask = const SSTATUS_SIE,
                options(nostack, preserves_flags)
            )
        }
    }

    /// 禁用中断（原子清零 `SSTATUS.SIE`）。
    #[inline]
    pub unsafe fn disable_interrupts() {
        // 关闭本地中断通常用于构造短临界区。由于只清 SIE 位，外部中断源并没有消失，只是
        // 暂时不会在本核上被响应；重新开中断后，pending 事件仍可能立刻到来。
        unsafe {
            core::arch::asm!(
                "csrc {csr}, {mask}",
                csr = const CSR_SSTATUS,
                mask = const SSTATUS_SIE,
                options(nostack, preserves_flags)
            )
        }
    }

    /// 检查中断是否已使能。
    #[inline]
    pub unsafe fn is_interrupt_enabled() -> bool {
        let sstatus: usize;
        unsafe {
            core::arch::asm!(
                "csrr {v}, {csr}",
                v = out(reg) sstatus,
                csr = const CSR_SSTATUS,
                options(nostack, preserves_flags)
            )
        }
        (sstatus & SSTATUS_SIE) != 0
    }
}

impl Riscv64MessageInterruptOps {
    /// 获取当前 CPU 的 hart ID。
    #[inline]
    pub fn current_cpu_id() -> usize {
        crate::riscv64::specific::current_cpu_id()
    }

    /// 读取全局中断使能位（CSR_SIE）。
    ///
    /// # 返回值
    ///
    /// 返回 SIE 寄存器的当前值，包含软件中断、定时器中断和外部中断的使能位。
    #[inline]
    pub unsafe fn message_interrupt_enable_bits() -> usize {
        let sie: usize;
        unsafe {
            core::arch::asm!(
                "csrr {v}, {csr}",
                v = out(reg) sie,
                csr = const CSR_SIE,
                options(nostack, preserves_flags)
            )
        }
        sie
    }

    /// 设置全局中断使能位（CSR_SIE）。
    ///
    /// # 参数
    ///
    /// - `bits`: 要设置的 SIE 值。
    #[inline]
    pub unsafe fn set_message_interrupt_enable_bits(bits: usize) {
        unsafe {
            core::arch::asm!(
                "csrw {csr}, {v}",
                csr = const CSR_SIE,
                v = in(reg) bits,
                options(nostack, preserves_flags)
            )
        }
    }
}
