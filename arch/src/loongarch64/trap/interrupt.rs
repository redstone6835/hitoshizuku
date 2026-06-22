//! LoongArch64 的中断逻辑。
//!
//! LoongArch64 的中断管理主要通过 CRMD 寄存器中的 IE 位来控制中断的使能状态。我
//! 们提供了保存和恢复中断状态的函数，以及直接使能和禁用中断的接口。这些函数使用
//! 内联汇编直接操作 CRMD 寄存器，以确保高效和正确地管理中断状态。
//!
//! 本模块还提供 MSGI（message interrupt）相关接口，用于读取/配置 CSR_MSGIE 并向
//! 目标 CPU 核心通过 CSR_MSGIR 发送消息中断。

use crate::*;
use core::sync::atomic::{Ordering, compiler_fence};
use general::dev::irq::{IocsrOps, IrqLine, IrqLineOps};

/// `CSR_CRMD` 中 IE（Interrupt Enable）位的掩码。
const CSR_CRMD_IE_MASK: usize = 1usize << CSR_CRMD_IE_OFFSET;
const LOONGARCH_HWI_COUNT: usize = 6;
const LOONGARCH_HWI_LIE_BASE: usize = 2;

/// 本地中断控制辅助实现（基于 `CSR_CRMD.IE`）。
pub struct LoongArch64InterruptOps;
/// 核间消息中断控制辅助实现（基于 `CSR_MSGIE / CSR_MSGIR`）。
pub struct LoongArch64MessageInterruptOps;

/// 安装设备 IRQ registry 使用的架构级 line 控制回调。
///
/// 当前只处理 LoongArch ESTAT/ECFG 中的 HWI0-5。timer/IPI 有独立时钟和消息中断
/// 语义，级联控制器子线则由对应 controller driver 自己完成 demux/ack。
pub fn install_loongarch_irq_line_ops() {
    general::dev::irq::install_irq_line_ops(IrqLineOps {
        enable: enable_irq_line,
        disable: disable_irq_line,
    });
    general::dev::irq::install_iocsr_ops(IocsrOps {
        read32: iocsr_read32,
        write32: iocsr_write32,
        read64: iocsr_read64,
        write64: iocsr_write64,
    });
}

fn enable_irq_line(line: IrqLine) -> bool {
    set_irq_line_enabled(line, true)
}

fn disable_irq_line(line: IrqLine) -> bool {
    set_irq_line_enabled(line, false)
}

fn set_irq_line_enabled(line: IrqLine, enabled: bool) -> bool {
    let IrqLine::Hardware(hwi) = line else {
        return false;
    };
    if hwi >= LOONGARCH_HWI_COUNT {
        return false;
    }
    let mask = 1usize << (LOONGARCH_HWI_LIE_BASE + hwi);
    let val = if enabled { mask } else { 0 };
    unsafe {
        core::arch::asm!(
            "csrxchg {val}, {mask}, {csr}",
            val = inout(reg) val => _,
            mask = in(reg) mask,
            csr = const CSR_ECFG,
            options(nostack, preserves_flags)
        );
    }
    compiler_fence(Ordering::SeqCst);
    true
}

fn iocsr_read32(offset: usize) -> u32 {
    let value: u32;
    unsafe {
        core::arch::asm!(
            "iocsrrd.w {value}, {offset}",
            value = out(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    compiler_fence(Ordering::SeqCst);
    value
}

fn iocsr_write32(offset: usize, value: u32) {
    unsafe {
        core::arch::asm!(
            "iocsrwr.w {value}, {offset}",
            value = in(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    compiler_fence(Ordering::SeqCst);
}

fn iocsr_read64(offset: usize) -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!(
            "iocsrrd.d {value}, {offset}",
            value = out(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    compiler_fence(Ordering::SeqCst);
    value
}

fn iocsr_write64(offset: usize, value: u64) {
    unsafe {
        core::arch::asm!(
            "iocsrwr.d {value}, {offset}",
            value = in(reg) value,
            offset = in(reg) offset,
            options(nostack, preserves_flags)
        );
    }
    compiler_fence(Ordering::SeqCst);
}

impl LoongArch64InterruptOps {
    /// 保存中断状态。
    ///
    /// # 返回值
    ///
    /// 返回当前 `CSR_CRMD` 原始值，供 [`restore_interrupt_state`] 恢复 IE 位。
    #[inline]
    pub unsafe fn save_interrupt_state() -> usize {
        // 保存的是整个 CRMD 原值，而不是单独缓存 IE 位。这样恢复时可以直接复用同一份
        // 硬件快照中的 IE 状态，避免调用者自己拼装位掩码。
        let crmd: usize;
        unsafe {
            core::arch::asm!(
                "csrrd {}, {}",
                out(reg) crmd,
                const CSR_CRMD,
                options(nostack, preserves_flags)
            )
        }
        compiler_fence(Ordering::SeqCst);
        crmd
    }

    /// 恢复中断状态。
    ///
    /// # 参数
    ///
    /// - `state`: 之前保存的 `CSR_CRMD` 原始值。
    #[inline]
    pub unsafe fn restore_interrupt_state(state: usize) {
        // 无分支恢复：仅提取 state 中 IE 位并写回 CSR_CRMD。
        // 从硬件语义看，`csrxchg` 相当于“只改 IE 这一位，其余 CRMD 字段保持当前值”。
        // 这比整寄存器回写更安全，因为不会意外覆盖 PLV/DA/PG 等其它控制位。
        let val = state;
        let mask = CSR_CRMD_IE_MASK;
        unsafe {
            core::arch::asm!(
                // 注意：csrxchg 会把旧 CSR 值写回 rd，故 rd 必须声明为 inout。
                "csrxchg {val}, {mask}, {csr}",
                val = inout(reg) val => _,
                mask = in(reg) mask,
                csr = const CSR_CRMD,
                options(nostack, preserves_flags)
            )
        }
        compiler_fence(Ordering::SeqCst);
    }

    /// 使能中断（原子置位 `CSR_CRMD.IE`）。
    #[inline]
    pub unsafe fn enable_interrupts() {
        // 这里只触碰 `CRMD.IE`，不改变当前特权级或地址翻译状态。也就是说，这个接口的
        // 职责只是打开本地可屏蔽中断响应能力，不负责更广义的执行上下文切换。
        let val = CSR_CRMD_IE_MASK;
        let mask = CSR_CRMD_IE_MASK;
        unsafe {
            core::arch::asm!(
                // 使用 csrxchg 原子置位 IE。
                // 实现 CSR = (CSR & ~mask) | (mask & mask) = CSR | mask
                "csrxchg {val}, {mask}, {csr}",
                val = inout(reg) val => _,
                mask = in(reg) mask,
                csr = const CSR_CRMD,
                options(nostack, preserves_flags)
            )
        }
        compiler_fence(Ordering::SeqCst);
    }

    /// 禁用中断（原子清零 `CSR_CRMD.IE`）。
    #[inline]
    pub unsafe fn disable_interrupts() {
        // 关闭本地中断通常用于构造短临界区。由于只清 IE 位，外部中断源并没有消失，只是
        // 暂时不会在本核上被响应；重新开中断后，pending 事件仍可能立刻到来。
        let val: usize = 0;
        let mask = CSR_CRMD_IE_MASK;
        unsafe {
            core::arch::asm!(
                // CSR = (CSR & ~mask) | (0 & mask) = CSR & ~mask
                "csrxchg {val}, {mask}, {csr}",
                val = inout(reg) val => _,
                mask = in(reg) mask,
                csr = const CSR_CRMD,
                options(nostack, preserves_flags)
            )
        }
        compiler_fence(Ordering::SeqCst);
    }

    /// 检查中断是否使能。
    ///
    /// # 返回值
    ///
    /// 返回 true 表示中断已使能，false 表示中断已禁用。
    #[inline]
    pub unsafe fn is_interrupt_enabled() -> bool {
        let crmd: usize;
        unsafe {
            core::arch::asm!(
                // 直接读取 CRMD 寄存器的值到 crmd 变量中。
                "csrrd {}, {}",
                out(reg) crmd,
                const CSR_CRMD,
                options(nostack, preserves_flags)
            )
        }
        compiler_fence(Ordering::SeqCst);
        (crmd & CSR_CRMD_IE_MASK) != 0
    }
}

impl LoongArch64MessageInterruptOps {
    /// 读取当前核的 CPU ID（来自 CSR_CPUID）。
    ///
    /// # 返回值
    ///
    /// 返回当前 CPU 的 ID，范围由 CSR_CPUID_COREID_MASK 定义。
    #[inline]
    pub fn current_cpu_id() -> usize {
        // CPUID 是硬件给出的当前核标识，常用于 per-CPU 数据、IPI 路由和日志标记。
        // 这里再按 COREID 掩码裁剪，是为了屏蔽寄存器中可能存在的保留高位。
        let cpuid: usize;
        unsafe {
            core::arch::asm!(
                // 直接读取 CSR_CPUID 寄存器的值到 cpuid 变量中。
                "csrrd {cpuid}, {csr_cpuid}",
                cpuid = out(reg) cpuid,
                csr_cpuid = const CSR_CPUID,
                options(nostack, preserves_flags)
            )
        }
        cpuid & CSR_CPUID_COREID_MASK
    }

    /// 读取消息中断使能寄存器 CSR_MSGIE。
    ///
    /// # 返回值
    ///
    /// 返回当前 CSR_MSGIE 寄存器的值，表示各消息中断线的使能状态。每个位
    /// 对应一个中断线。
    #[inline]
    pub unsafe fn message_interrupt_enable_bits() -> usize {
        // MSGIE 是“本核接收哪些 MSGI 线”的硬件门控位图。读取它可以得到当前核在
        // 核间消息中断层面的接收配置快照。
        let bits: usize;
        unsafe {
            core::arch::asm!(
                // 直接读取 CSR_MSGIE 寄存器的值到 bits 变量中。
                "csrrd {bits}, {csr_msgie}",
                bits = out(reg) bits,
                csr_msgie = const CSR_MSGIE,
                options(nostack, preserves_flags)
            )
        }
        bits
    }

    /// 直接写入 `CSR_MSGIE`。
    ///
    /// # Safety
    ///
    /// 调用者必须保证写入位图与当前平台消息中断路由策略一致。
    ///
    /// # 参数
    ///
    /// - `bits`: 要写入 `CSR_MSGIE` 的完整位图。每个位对应一条消息中断线。
    #[inline]
    pub unsafe fn set_message_interrupt_enable_bits(bits: usize) {
        // 这是全量覆盖写，不是按位修改。调用者应当把它当成“重建本核 MSGI 接收策略”的
        // 原始硬件接口，而不是细粒度开关。
        let bits = bits;
        unsafe {
            core::arch::asm!(
                // 注意：csrwr 会把旧 CSR 值写回 rd，故 rd 需声明为 inout。
                "csrwr {bits}, {csr_msgie}",
                bits = inout(reg) bits => _,
                csr_msgie = const CSR_MSGIE,
                options(nostack, preserves_flags)
            )
        }
    }

    /// 按掩码使能消息中断（CSR_MSGIE 对应位）。
    ///
    /// # Safety
    ///
    /// 调用者必须保证被打开的中断线已具备可重入、可处理的接收路径。
    ///
    /// # 参数
    ///
    /// - `mask`: 要使能的消息中断线掩码，1 位对应 1 条中断线。
    #[inline]
    pub unsafe fn enable_message_interrupt_mask(mask: usize) {
        // MSGI 常被用作核间唤醒、TLB shootdown 或调度通知。按掩码开启时，等价于允许
        // 对应消息线开始从“硬件 pending”进入本核 trap 流程。
        let tmp = mask;
        unsafe {
            core::arch::asm!(
                // 注意：csrxchg 会把旧 CSR 值写回 rd，故 rd 需声明为 inout。
                "csrxchg {tmp}, {mask}, {csr_msgie}",
                tmp = inout(reg) tmp => _,
                mask = in(reg) mask,
                csr_msgie = const CSR_MSGIE,
                options(nostack, preserves_flags)
            )
        }
    }

    /// 按掩码禁用消息中断（CSR_MSGIE 对应位）。
    ///
    /// # Safety
    ///
    /// 调用者必须保证关闭中断线不会导致关键通知永久丢失。
    ///
    /// # 参数
    ///
    /// - `mask`: 要禁用的消息中断线掩码，1 位对应 1 条中断线。
    #[inline]
    pub unsafe fn disable_message_interrupt_mask(mask: usize) {
        // 关闭消息线只影响接收端门控，不会撤销已经在飞或已经 pending 的软件语义，
        // 因而调用者仍需考虑上层协议中的竞态与补偿逻辑。
        unsafe {
            core::arch::asm!(
                // `csrxchg $zero, mask, csr`：按掩码清零对应位。
                "csrxchg $zero, {mask}, {csr_msgie}",
                mask = in(reg) mask,
                csr_msgie = const CSR_MSGIE,
                options(nostack, preserves_flags)
            )
        }
    }

    /// 向目标 CPU 发送消息中断（写 CSR_MSGIR）。
    ///
    /// # Safety
    ///
    /// 调用者必须保证目标 `cpu_id` 有效，且 `data` 语义与接收端约定一致。
    ///
    /// # 参数
    ///
    /// - `cpu_id`: 目标 CPU 的 ID，范围由 CSR_CPUID_COREID_MASK 定义。
    /// - `data`: 发送的数据，范围由 CSR_MSGIR_DATA_MASK 定义。
    #[inline]
    pub unsafe fn send_message_interrupt(cpu_id: usize, data: usize) {
        // 向 MSGIR 写入实际上是在请求片上中断路由逻辑把一条消息中断投递到目标核。
        // `cpu_id` 和 `data` 的编码格式由 SoC/架构定义，`msgir_encode` 负责把两者压成
        // 硬件要求的位布局。
        let msg = msgir_encode(cpu_id, data);
        unsafe {
            core::arch::asm!(
                // 注意：csrwr 会把旧 CSR 值写回 rd，故 rd 需声明为 inout。
                "csrwr {msg}, {csr_msgir}",
                msg = inout(reg) msg => _,
                csr_msgir = const CSR_MSGIR,
                options(nostack, preserves_flags)
            )
        }
    }
}
