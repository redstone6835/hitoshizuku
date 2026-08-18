//! RISC-V64 的中断逻辑。
//!
//! RISC-V64 的中断管理主要通过 SSTATUS 寄存器中的 SIE 位来控制中断的使能状态。我
//! 们提供了保存和恢复中断状态的函数，以及直接使能和禁用中断的接口。这些函数使用
//! 内联汇编直接操作 SSTATUS 寄存器，以确保高效和正确地管理中断状态。
//!
//! 本模块还提供 S-mode 软件中断（SSIP/SSIE）相关接口。跨 hart 发送由 SBI IPI
//! 或平台中断控制器完成；本模块只负责当前 hart 的 enable/ack。

use crate::*;
use general::dev::irq::{IrqLine, IrqLineOps};

/// 本地中断控制辅助实现（基于 `SSTATUS.SIE`）。
pub struct Riscv64InterruptOps;
/// 核间消息中断控制辅助实现（基于 `sie.SSIE / sip.SSIP`）。
pub struct Riscv64MessageInterruptOps;

/// 保存并关闭当前 hart 中断的 RAII guard。
pub struct LocalIrqGuard {
    state: usize,
}

impl LocalIrqGuard {
    #[inline]
    pub fn acquire() -> Self {
        let state = unsafe { Riscv64InterruptOps::save_and_disable() };
        Self { state }
    }
}

impl Drop for LocalIrqGuard {
    #[inline]
    fn drop(&mut self) {
        unsafe { Riscv64InterruptOps::restore_interrupt_state(self.state) };
    }
}

/// 安装设备 IRQ registry 使用的架构级 line 控制回调。
///
/// RISC-V 的 PLIC 外部中断统一从 `sie.SEIE` 进入 S-mode trap，PLIC 自己再
/// 通过 claim/complete 分发具体 hwirq。timer/IPI 仍由独立路径管理。
pub fn install_riscv_irq_line_ops() {
    general::dev::irq::install_irq_line_ops(IrqLineOps {
        enable: enable_irq_line,
        disable: disable_irq_line,
    });
}

/// AP 上线时开放该 hart 的 S-mode 外部中断入口。
///
/// PLIC 的每个 supervisor context 都是独立的；IRQ domain 注册通常发生在
/// boot hart，因此不能只依赖注册时对 boot hart 写入 `sie.SEIE`。
#[inline]
pub fn enable_external_interrupts() {
    let _ = set_irq_line_enabled(IrqLine::Hardware(0), true);
}

fn enable_irq_line(line: IrqLine) -> bool {
    set_irq_line_enabled(line, true)
}

fn disable_irq_line(line: IrqLine) -> bool {
    set_irq_line_enabled(line, false)
}

fn set_irq_line_enabled(line: IrqLine, enabled: bool) -> bool {
    match line {
        IrqLine::Ipi => {
            if enabled {
                set_csr!(sie, SIE_SSIE);
            } else {
                clear_csr!(sie, SIE_SSIE);
            }
        }
        IrqLine::Hardware(0) => {
            let changed = crate::riscv64::external_irq::set_enabled(enabled);
            sync_external_irq_current_cpu();
            if changed {
                crate::riscv64::smp::request_external_irq_sync();
            }
        }
        _ => return false,
    }
    true
}

pub(in crate::riscv64) fn sync_external_irq_current_cpu() {
    if crate::riscv64::external_irq::is_enabled() {
        set_csr!(sie, SIE_SEIE);
    } else {
        clear_csr!(sie, SIE_SEIE);
    }
}

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

    /// 原子保存 `sstatus` 并清除 SIE。
    #[inline]
    pub unsafe fn save_and_disable() -> usize {
        let state: usize;
        unsafe {
            core::arch::asm!(
                "csrrc {state}, {csr}, {mask}",
                state = out(reg) state,
                csr = const CSR_SSTATUS,
                mask = in(reg) SSTATUS_SIE,
                options(nostack, preserves_flags)
            );
        }
        state
    }

    /// 恢复中断状态。
    ///
    /// # 参数
    ///
    /// - `state`: 之前保存的 `SSTATUS` 原始值。
    #[inline]
    pub unsafe fn restore_interrupt_state(state: usize) {
        // save_and_disable() 已经把 SIE 清零；退出临界区时仅在原状态开启的情况下
        // 执行一次置位，避免每次 guard drop 都做“再清一次 + 写入零掩码”。
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
        if state & SSTATUS_SIE != 0 {
            unsafe {
                core::arch::asm!(
                    "csrs {csr}, {mask}",
                    csr = const CSR_SSTATUS,
                    mask = in(reg) SSTATUS_SIE,
                    options(nostack, preserves_flags)
                );
            }
        }
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

    /// 读取当前 hart 的软件中断使能位 `sie.SSIE`。
    ///
    /// # 返回值
    ///
    /// 返回值只包含 SSIE bit。
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
        sie & SIE_SSIE
    }

    /// 按 `bits.SSIE` 设置当前 hart 的软件中断使能状态。
    ///
    /// # 参数
    ///
    /// - `bits`: 要设置的 SIE 值。
    #[inline]
    pub unsafe fn set_message_interrupt_enable_bits(bits: usize) {
        if bits & SIE_SSIE != 0 {
            set_csr!(sie, SIE_SSIE);
        } else {
            clear_csr!(sie, SIE_SSIE);
        }
    }

    /// 清除当前 hart 的 SSIP pending 状态。应在 IPI handler 入口调用。
    #[inline]
    pub unsafe fn ack_ipi() {
        clear_csr!(sip, SIP_SSIP);
    }
}
