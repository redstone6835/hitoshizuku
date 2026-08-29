//! LLVM ThreadSanitizer 普通访问 hook ABI。
//!
//! 构建包装器关闭 atomic、memintrinsic 和函数进入/退出插桩，因此这里只实现
//! 不会替换原始操作语义的地址通知 hook。volatile hook 故意为空，避免延迟 MMIO。

use crate::{AccessKind, check_access};

// LLVM invokes these hooks with the instrumented address as the first SysV
// argument.  The x86 entry stubs preserve the caller return address before
// tail-jumping to Rust, so the detector receives the actual instrumented
// instruction instead of a compiler-dependent stack-frame guess.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    r#"
.macro KCSAN_HOOK name, impl
.global \name
.type \name,@function
\name:
    mov rsi, [rsp]
    jmp \impl
.size \name, .-\name
.endm
KCSAN_HOOK __tsan_read1, __tsan_read1_impl
KCSAN_HOOK __tsan_read2, __tsan_read2_impl
KCSAN_HOOK __tsan_read4, __tsan_read4_impl
KCSAN_HOOK __tsan_read8, __tsan_read8_impl
KCSAN_HOOK __tsan_read16, __tsan_read16_impl
KCSAN_HOOK __tsan_write1, __tsan_write1_impl
KCSAN_HOOK __tsan_write2, __tsan_write2_impl
KCSAN_HOOK __tsan_write4, __tsan_write4_impl
KCSAN_HOOK __tsan_write8, __tsan_write8_impl
KCSAN_HOOK __tsan_write16, __tsan_write16_impl
KCSAN_HOOK __tsan_read_write1, __tsan_read_write1_impl
KCSAN_HOOK __tsan_read_write2, __tsan_read_write2_impl
KCSAN_HOOK __tsan_read_write4, __tsan_read_write4_impl
KCSAN_HOOK __tsan_read_write8, __tsan_read_write8_impl
KCSAN_HOOK __tsan_read_write16, __tsan_read_write16_impl
KCSAN_HOOK __tsan_unaligned_read1, __tsan_unaligned_read1_impl
KCSAN_HOOK __tsan_unaligned_read2, __tsan_unaligned_read2_impl
KCSAN_HOOK __tsan_unaligned_read4, __tsan_unaligned_read4_impl
KCSAN_HOOK __tsan_unaligned_read8, __tsan_unaligned_read8_impl
KCSAN_HOOK __tsan_unaligned_read16, __tsan_unaligned_read16_impl
KCSAN_HOOK __tsan_unaligned_write1, __tsan_unaligned_write1_impl
KCSAN_HOOK __tsan_unaligned_write2, __tsan_unaligned_write2_impl
KCSAN_HOOK __tsan_unaligned_write4, __tsan_unaligned_write4_impl
KCSAN_HOOK __tsan_unaligned_write8, __tsan_unaligned_write8_impl
KCSAN_HOOK __tsan_unaligned_write16, __tsan_unaligned_write16_impl
KCSAN_HOOK __tsan_unaligned_read_write1, __tsan_unaligned_read_write1_impl
KCSAN_HOOK __tsan_unaligned_read_write2, __tsan_unaligned_read_write2_impl
KCSAN_HOOK __tsan_unaligned_read_write4, __tsan_unaligned_read_write4_impl
KCSAN_HOOK __tsan_unaligned_read_write8, __tsan_unaligned_read_write8_impl
KCSAN_HOOK __tsan_unaligned_read_write16, __tsan_unaligned_read_write16_impl
.global __tsan_read_range
.type __tsan_read_range,@function
__tsan_read_range:
    mov rdx, [rsp]
    jmp __tsan_read_range_impl
.size __tsan_read_range, .-__tsan_read_range
.global __tsan_write_range
.type __tsan_write_range,@function
__tsan_write_range:
    mov rdx, [rsp]
    jmp __tsan_write_range_impl
.size __tsan_write_range, .-__tsan_write_range
"#
);

#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
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
    ($name:ident, $implementation:ident, $size:expr, $kind:expr) => {
        #[cfg(target_arch = "x86_64")]
        #[unsafe(no_mangle)]
        #[inline(never)]
        pub unsafe extern "C" fn $implementation(address: *const u8, pc: usize) {
            check_access(address as usize, $size, $kind, pc);
        }

        #[cfg(not(target_arch = "x86_64"))]
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

access_hook!(__tsan_read1, __tsan_read1_impl, 1, AccessKind::Read);
access_hook!(__tsan_read2, __tsan_read2_impl, 2, AccessKind::Read);
access_hook!(__tsan_read4, __tsan_read4_impl, 4, AccessKind::Read);
access_hook!(__tsan_read8, __tsan_read8_impl, 8, AccessKind::Read);
access_hook!(__tsan_read16, __tsan_read16_impl, 16, AccessKind::Read);
access_hook!(__tsan_write1, __tsan_write1_impl, 1, AccessKind::Write);
access_hook!(__tsan_write2, __tsan_write2_impl, 2, AccessKind::Write);
access_hook!(__tsan_write4, __tsan_write4_impl, 4, AccessKind::Write);
access_hook!(__tsan_write8, __tsan_write8_impl, 8, AccessKind::Write);
access_hook!(__tsan_write16, __tsan_write16_impl, 16, AccessKind::Write);
access_hook!(
    __tsan_read_write1,
    __tsan_read_write1_impl,
    1,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_read_write2,
    __tsan_read_write2_impl,
    2,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_read_write4,
    __tsan_read_write4_impl,
    4,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_read_write8,
    __tsan_read_write8_impl,
    8,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_read_write16,
    __tsan_read_write16_impl,
    16,
    AccessKind::ReadWrite
);

access_hook!(
    __tsan_unaligned_read1,
    __tsan_unaligned_read1_impl,
    1,
    AccessKind::Read
);
access_hook!(
    __tsan_unaligned_read2,
    __tsan_unaligned_read2_impl,
    2,
    AccessKind::Read
);
access_hook!(
    __tsan_unaligned_read4,
    __tsan_unaligned_read4_impl,
    4,
    AccessKind::Read
);
access_hook!(
    __tsan_unaligned_read8,
    __tsan_unaligned_read8_impl,
    8,
    AccessKind::Read
);
access_hook!(
    __tsan_unaligned_read16,
    __tsan_unaligned_read16_impl,
    16,
    AccessKind::Read
);
access_hook!(
    __tsan_unaligned_write1,
    __tsan_unaligned_write1_impl,
    1,
    AccessKind::Write
);
access_hook!(
    __tsan_unaligned_write2,
    __tsan_unaligned_write2_impl,
    2,
    AccessKind::Write
);
access_hook!(
    __tsan_unaligned_write4,
    __tsan_unaligned_write4_impl,
    4,
    AccessKind::Write
);
access_hook!(
    __tsan_unaligned_write8,
    __tsan_unaligned_write8_impl,
    8,
    AccessKind::Write
);
access_hook!(
    __tsan_unaligned_write16,
    __tsan_unaligned_write16_impl,
    16,
    AccessKind::Write
);
access_hook!(
    __tsan_unaligned_read_write1,
    __tsan_unaligned_read_write1_impl,
    1,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_unaligned_read_write2,
    __tsan_unaligned_read_write2_impl,
    2,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_unaligned_read_write4,
    __tsan_unaligned_read_write4_impl,
    4,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_unaligned_read_write8,
    __tsan_unaligned_read_write8_impl,
    8,
    AccessKind::ReadWrite
);
access_hook!(
    __tsan_unaligned_read_write16,
    __tsan_unaligned_read_write16_impl,
    16,
    AccessKind::ReadWrite
);

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
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn __tsan_read_range_impl(address: *const u8, size: usize, pc: usize) {
    check_access(address as usize, size, AccessKind::Read, pc);
}

#[cfg(not(target_arch = "x86_64"))]
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn __tsan_read_range(address: *const u8, size: usize) {
    check_access(address as usize, size, AccessKind::Read, caller_pc());
}

/// 兼容可能由显式注解生成的范围写入 hook。
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
#[inline(never)]
pub unsafe extern "C" fn __tsan_write_range_impl(address: *const u8, size: usize, pc: usize) {
    check_access(address as usize, size, AccessKind::Write, pc);
}

#[cfg(not(target_arch = "x86_64"))]
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
