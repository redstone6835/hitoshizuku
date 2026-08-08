//! LLVM ThreadSanitizer 普通访问 hook ABI。
//!
//! 构建包装器关闭 atomic、memintrinsic 和函数进入/退出插桩，因此这里只实现
//! 不会替换原始操作语义的地址通知 hook。volatile hook 故意为空，避免延迟 MMIO。

use crate::{AccessKind, check_access};

#[inline(always)]
fn caller_pc() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        let pc: usize;
        // Safety: 只复制返回地址寄存器，不访问内存或修改控制状态。
        unsafe {
            core::arch::asm!(
                "move {pc}, $ra",
                pc = out(reg) pc,
                options(nomem, nostack, preserves_flags),
            );
        }
        return pc;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let pc: usize;
        // Safety: 只复制返回地址寄存器，不访问内存或修改控制状态。
        unsafe {
            core::arch::asm!("mv {pc}, ra", pc = out(reg) pc, options(nomem, nostack));
        }
        return pc;
    }

    #[cfg(not(any(target_arch = "loongarch64", target_arch = "riscv64")))]
    {
        0
    }
}

macro_rules! access_hook {
    ($name:ident, $size:expr, $kind:expr) => {
        #[unsafe(no_mangle)]
        #[inline(never)]
        pub unsafe extern "C" fn $name(address: *const u8) {
            check_access(address as usize, $size, $kind, caller_pc());
        }
    };
}

macro_rules! volatile_hook {
    ($name:ident) => {
        #[unsafe(no_mangle)]
        #[inline(never)]
        pub unsafe extern "C" fn $name(_address: *const u8) {}
    };
}

access_hook!(__tsan_read1, 1, AccessKind::Read);
access_hook!(__tsan_read2, 2, AccessKind::Read);
access_hook!(__tsan_read4, 4, AccessKind::Read);
access_hook!(__tsan_read8, 8, AccessKind::Read);
access_hook!(__tsan_read16, 16, AccessKind::Read);
access_hook!(__tsan_write1, 1, AccessKind::Write);
access_hook!(__tsan_write2, 2, AccessKind::Write);
access_hook!(__tsan_write4, 4, AccessKind::Write);
access_hook!(__tsan_write8, 8, AccessKind::Write);
access_hook!(__tsan_write16, 16, AccessKind::Write);
access_hook!(__tsan_read_write1, 1, AccessKind::ReadWrite);
access_hook!(__tsan_read_write2, 2, AccessKind::ReadWrite);
access_hook!(__tsan_read_write4, 4, AccessKind::ReadWrite);
access_hook!(__tsan_read_write8, 8, AccessKind::ReadWrite);
access_hook!(__tsan_read_write16, 16, AccessKind::ReadWrite);

access_hook!(__tsan_unaligned_read1, 1, AccessKind::Read);
access_hook!(__tsan_unaligned_read2, 2, AccessKind::Read);
access_hook!(__tsan_unaligned_read4, 4, AccessKind::Read);
access_hook!(__tsan_unaligned_read8, 8, AccessKind::Read);
access_hook!(__tsan_unaligned_read16, 16, AccessKind::Read);
access_hook!(__tsan_unaligned_write1, 1, AccessKind::Write);
access_hook!(__tsan_unaligned_write2, 2, AccessKind::Write);
access_hook!(__tsan_unaligned_write4, 4, AccessKind::Write);
access_hook!(__tsan_unaligned_write8, 8, AccessKind::Write);
access_hook!(__tsan_unaligned_write16, 16, AccessKind::Write);
access_hook!(__tsan_unaligned_read_write1, 1, AccessKind::ReadWrite);
access_hook!(__tsan_unaligned_read_write2, 2, AccessKind::ReadWrite);
access_hook!(__tsan_unaligned_read_write4, 4, AccessKind::ReadWrite);
access_hook!(__tsan_unaligned_read_write8, 8, AccessKind::ReadWrite);
access_hook!(__tsan_unaligned_read_write16, 16, AccessKind::ReadWrite);

volatile_hook!(__tsan_volatile_read1);
volatile_hook!(__tsan_volatile_read2);
volatile_hook!(__tsan_volatile_read4);
volatile_hook!(__tsan_volatile_read8);
volatile_hook!(__tsan_volatile_read16);
volatile_hook!(__tsan_volatile_write1);
volatile_hook!(__tsan_volatile_write2);
volatile_hook!(__tsan_volatile_write4);
volatile_hook!(__tsan_volatile_write8);
volatile_hook!(__tsan_volatile_write16);
volatile_hook!(__tsan_unaligned_volatile_read1);
volatile_hook!(__tsan_unaligned_volatile_read2);
volatile_hook!(__tsan_unaligned_volatile_read4);
volatile_hook!(__tsan_unaligned_volatile_read8);
volatile_hook!(__tsan_unaligned_volatile_read16);
volatile_hook!(__tsan_unaligned_volatile_write1);
volatile_hook!(__tsan_unaligned_volatile_write2);
volatile_hook!(__tsan_unaligned_volatile_write4);
volatile_hook!(__tsan_unaligned_volatile_write8);
volatile_hook!(__tsan_unaligned_volatile_write16);

/// 兼容可能由显式注解生成的范围读取 hook。
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn __tsan_read_range(address: *const u8, size: usize) {
    check_access(address as usize, size, AccessKind::Read, caller_pc());
}

/// 兼容可能由显式注解生成的范围写入 hook。
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn __tsan_write_range(address: *const u8, size: usize) {
    check_access(address as usize, size, AccessKind::Write, caller_pc());
}

/// `tsan-module` 未启用，因此正常不会引用该符号；保留空实现便于诊断构建组合。
#[unsafe(no_mangle)]
pub extern "C" fn __tsan_init() {}

/// 显式忽略区兼容入口。当前自动 pass 不生成该调用。
#[unsafe(no_mangle)]
pub extern "C" fn __tsan_ignore_thread_begin() {}

/// 显式忽略区兼容入口。当前自动 pass 不生成该调用。
#[unsafe(no_mangle)]
pub extern "C" fn __tsan_ignore_thread_end() {}
