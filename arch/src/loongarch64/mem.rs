//! LoongArch64 内核内存原语。
//!
//! 启动阶段尚未确认非对齐访问能力时仅使用字节操作。加载器发布 UAL 能力后，
//! 大块操作使用顺序展开的标量访问，不依赖 LSX 状态。

#![allow(named_asm_labels)]

use core::arch::{asm, naked_asm};
use core::ffi::c_void;
use core::sync::atomic::{AtomicU8, Ordering};

/// 当前处理器是否允许非对齐访存。
///
/// 该值位于已初始化数据段，确保清理 BSS 时仍选择逐字节回退路径。
#[unsafe(link_section = ".data")]
#[unsafe(no_mangle)]
static MEM_UAL: AtomicU8 = AtomicU8::new(0);

/// 读取并发布启动处理器的非对齐访存能力。
pub(crate) fn init_ual() {
    let mut cpucfg1: usize;
    // CPUCFG 只读取处理器能力寄存器，不访问内存或修改处理器状态。
    unsafe {
        asm!(
            "cpucfg {value}, {index}",
            value = out(reg) cpucfg1,
            index = in(reg) 1usize,
            options(nostack, preserves_flags),
        );
    }
    MEM_UAL.store(((cpucfg1 as u32) & (1 << 20) != 0) as u8, Ordering::Release);
}

/// 复制两个不重叠的内存区域。
///
/// # Safety
///
/// `[src, src + len)` 与 `[dst, dst + len)` 必须有效且不得重叠。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(
    _dst: *mut c_void,
    _src: *const c_void,
    _len: usize,
) -> *mut c_void {
    naked_asm!(
        r#"
        move    $t8, $a0
        beqz    $a2, .Lla_copy_return
        la.pcrel $t0, {ual}
        ld.bu   $t1, $t0, 0
        beqz    $t1, .Lla_copy_byte_loop

        /* UAL 路径每轮顺序处理 64 字节，所有读取先于写入。 */
.Lla_copy_blocks:
        srli.d  $t5, $a2, 8
        beqz    $t5, .Lla_copy_blocks64
.Lla_copy_block_loop:
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        ld.d    $a3, $a1, 64
        ld.d    $a4, $a1, 72
        ld.d    $a5, $a1, 80
        ld.d    $a6, $a1, 88
        ld.d    $a7, $a1, 96
        ld.d    $t2, $a1, 104
        ld.d    $t3, $a1, 112
        ld.d    $t4, $a1, 120
        st.d    $a3, $a0, 64
        st.d    $a4, $a0, 72
        st.d    $a5, $a0, 80
        st.d    $a6, $a0, 88
        st.d    $a7, $a0, 96
        st.d    $t2, $a0, 104
        st.d    $t3, $a0, 112
        st.d    $t4, $a0, 120
        ld.d    $a3, $a1, 128
        ld.d    $a4, $a1, 136
        ld.d    $a5, $a1, 144
        ld.d    $a6, $a1, 152
        ld.d    $a7, $a1, 160
        ld.d    $t2, $a1, 168
        ld.d    $t3, $a1, 176
        ld.d    $t4, $a1, 184
        st.d    $a3, $a0, 128
        st.d    $a4, $a0, 136
        st.d    $a5, $a0, 144
        st.d    $a6, $a0, 152
        st.d    $a7, $a0, 160
        st.d    $t2, $a0, 168
        st.d    $t3, $a0, 176
        st.d    $t4, $a0, 184
        ld.d    $a3, $a1, 192
        ld.d    $a4, $a1, 200
        ld.d    $a5, $a1, 208
        ld.d    $a6, $a1, 216
        ld.d    $a7, $a1, 224
        ld.d    $t2, $a1, 232
        ld.d    $t3, $a1, 240
        ld.d    $t4, $a1, 248
        st.d    $a3, $a0, 192
        st.d    $a4, $a0, 200
        st.d    $a5, $a0, 208
        st.d    $a6, $a0, 216
        st.d    $a7, $a0, 224
        st.d    $t2, $a0, 232
        st.d    $t3, $a0, 240
        st.d    $t4, $a0, 248
        addi.d  $a0, $a0, 256
        addi.d  $a1, $a1, 256
        addi.d  $t5, $t5, -1
        bnez    $t5, .Lla_copy_block_loop
        andi    $a2, $a2, 255

.Lla_copy_blocks64:
        srli.d  $t5, $a2, 6
        beqz    $t5, .Lla_copy_words
.Lla_copy_block64_loop:
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        addi.d  $a0, $a0, 64
        addi.d  $a1, $a1, 64
        addi.d  $t5, $t5, -1
        bnez    $t5, .Lla_copy_block64_loop
        andi    $a2, $a2, 63

.Lla_copy_words:
        sltui   $t0, $a2, 8
        bnez    $t0, .Lla_copy_tail4
        ld.d    $t1, $a1, 0
        st.d    $t1, $a0, 0
        addi.d  $a0, $a0, 8
        addi.d  $a1, $a1, 8
        addi.d  $a2, $a2, -8
        b       .Lla_copy_words
.Lla_copy_tail4:
        sltui   $t0, $a2, 4
        bnez    $t0, .Lla_copy_tail2
        ld.w    $t1, $a1, 0
        st.w    $t1, $a0, 0
        addi.d  $a0, $a0, 4
        addi.d  $a1, $a1, 4
        addi.d  $a2, $a2, -4
.Lla_copy_tail2:
        sltui   $t0, $a2, 2
        bnez    $t0, .Lla_copy_tail1
        ld.h    $t1, $a1, 0
        st.h    $t1, $a0, 0
        addi.d  $a0, $a0, 2
        addi.d  $a1, $a1, 2
        addi.d  $a2, $a2, -2
.Lla_copy_tail1:
        beqz    $a2, .Lla_copy_return
        ld.b    $t1, $a1, 0
        st.b    $t1, $a0, 0
        b       .Lla_copy_return

        /* 未确认 UAL 时严格逐字节复制。 */
.Lla_copy_byte_loop:
        ld.b    $t0, $a1, 0
        st.b    $t0, $a0, 0
        addi.d  $a0, $a0, 1
        addi.d  $a1, $a1, 1
        addi.d  $a2, $a2, -1
        bnez    $a2, .Lla_copy_byte_loop
.Lla_copy_return:
        move    $a0, $t8
        jr      $ra
        "#,
        ual = sym MEM_UAL,
    )
}

/// 用给定字节填充内存区域。
///
/// # Safety
///
/// `[dst, dst + len)` 必须有效且可写。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(_dst: *mut c_void, _value: i32, _len: usize) -> *mut c_void {
    naked_asm!(
        r#"
        move    $t8, $a0
        beqz    $a2, .Lla_set_return
        la.pcrel $t0, {ual}
        ld.bu   $t1, $t0, 0
        beqz    $t1, .Lla_set_byte_loop

        andi    $a1, $a1, 255
        bstrins.d $a1, $a1, 15, 8
        bstrins.d $a1, $a1, 31, 16
        bstrins.d $a1, $a1, 63, 32

.Lla_set_blocks:
        srli.d  $t5, $a2, 8
        beqz    $t5, .Lla_set_blocks64
.Lla_set_block_loop:
        st.d    $a1, $a0, 0
        st.d    $a1, $a0, 8
        st.d    $a1, $a0, 16
        st.d    $a1, $a0, 24
        st.d    $a1, $a0, 32
        st.d    $a1, $a0, 40
        st.d    $a1, $a0, 48
        st.d    $a1, $a0, 56
        st.d    $a1, $a0, 64
        st.d    $a1, $a0, 72
        st.d    $a1, $a0, 80
        st.d    $a1, $a0, 88
        st.d    $a1, $a0, 96
        st.d    $a1, $a0, 104
        st.d    $a1, $a0, 112
        st.d    $a1, $a0, 120
        st.d    $a1, $a0, 128
        st.d    $a1, $a0, 136
        st.d    $a1, $a0, 144
        st.d    $a1, $a0, 152
        st.d    $a1, $a0, 160
        st.d    $a1, $a0, 168
        st.d    $a1, $a0, 176
        st.d    $a1, $a0, 184
        st.d    $a1, $a0, 192
        st.d    $a1, $a0, 200
        st.d    $a1, $a0, 208
        st.d    $a1, $a0, 216
        st.d    $a1, $a0, 224
        st.d    $a1, $a0, 232
        st.d    $a1, $a0, 240
        st.d    $a1, $a0, 248
        addi.d  $a0, $a0, 256
        addi.d  $t5, $t5, -1
        bnez    $t5, .Lla_set_block_loop
        andi    $a2, $a2, 255

.Lla_set_blocks64:
        srli.d  $t5, $a2, 6
        beqz    $t5, .Lla_set_words
.Lla_set_block64_loop:
        st.d    $a1, $a0, 0
        st.d    $a1, $a0, 8
        st.d    $a1, $a0, 16
        st.d    $a1, $a0, 24
        st.d    $a1, $a0, 32
        st.d    $a1, $a0, 40
        st.d    $a1, $a0, 48
        st.d    $a1, $a0, 56
        addi.d  $a0, $a0, 64
        addi.d  $t5, $t5, -1
        bnez    $t5, .Lla_set_block64_loop
        andi    $a2, $a2, 63

.Lla_set_words:
        sltui   $t0, $a2, 8
        bnez    $t0, .Lla_set_tail4
        st.d    $a1, $a0, 0
        addi.d  $a0, $a0, 8
        addi.d  $a2, $a2, -8
        b       .Lla_set_words
.Lla_set_tail4:
        sltui   $t0, $a2, 4
        bnez    $t0, .Lla_set_tail2
        st.w    $a1, $a0, 0
        addi.d  $a0, $a0, 4
        addi.d  $a2, $a2, -4
.Lla_set_tail2:
        sltui   $t0, $a2, 2
        bnez    $t0, .Lla_set_tail1
        st.h    $a1, $a0, 0
        addi.d  $a0, $a0, 2
        addi.d  $a2, $a2, -2
.Lla_set_tail1:
        beqz    $a2, .Lla_set_return
        st.b    $a1, $a0, 0
        b       .Lla_set_return

.Lla_set_byte_loop:
        st.b    $a1, $a0, 0
        addi.d  $a0, $a0, 1
        addi.d  $a2, $a2, -1
        bnez    $a2, .Lla_set_byte_loop
.Lla_set_return:
        move    $a0, $t8
        jr      $ra
        "#,
        ual = sym MEM_UAL,
    )
}

/// 复制可能重叠的内存区域。
///
/// # Safety
///
/// `[src, src + len)` 与 `[dst, dst + len)` 必须是有效内存区域，可以重叠。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(
    _dst: *mut c_void,
    _src: *const c_void,
    _len: usize,
) -> *mut c_void {
    naked_asm!(
        r#"
        beqz    $a2, .Lla_move_return
        beq     $a0, $a1, .Lla_move_return
        bltu    $a0, $a1, memcpy
        add.d   $t0, $a1, $a2
        bgeu    $a0, $t0, memcpy

        move    $t8, $a0
        add.d   $a0, $a0, $a2
        add.d   $a1, $a1, $a2
        la.pcrel $t0, {ual}
        ld.bu   $t1, $t0, 0
        beqz    $t1, .Lla_move_reverse_bytes

        /* 重叠区域从末端分块读取，读取完成后再写入。 */
.Lla_move_reverse_blocks:
        srli.d  $t5, $a2, 8
        beqz    $t5, .Lla_move_reverse_blocks64
.Lla_move_reverse_block_loop:
        addi.d  $a0, $a0, -64
        addi.d  $a1, $a1, -64
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        addi.d  $a0, $a0, -64
        addi.d  $a1, $a1, -64
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        addi.d  $a0, $a0, -64
        addi.d  $a1, $a1, -64
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        addi.d  $a0, $a0, -64
        addi.d  $a1, $a1, -64
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        addi.d  $t5, $t5, -1
        bnez    $t5, .Lla_move_reverse_block_loop
        andi    $a2, $a2, 255

.Lla_move_reverse_blocks64:
        srli.d  $t5, $a2, 6
        beqz    $t5, .Lla_move_reverse_words
.Lla_move_reverse_block64_loop:
        addi.d  $a0, $a0, -64
        addi.d  $a1, $a1, -64
        ld.d    $a3, $a1, 0
        ld.d    $a4, $a1, 8
        ld.d    $a5, $a1, 16
        ld.d    $a6, $a1, 24
        ld.d    $a7, $a1, 32
        ld.d    $t2, $a1, 40
        ld.d    $t3, $a1, 48
        ld.d    $t4, $a1, 56
        st.d    $a3, $a0, 0
        st.d    $a4, $a0, 8
        st.d    $a5, $a0, 16
        st.d    $a6, $a0, 24
        st.d    $a7, $a0, 32
        st.d    $t2, $a0, 40
        st.d    $t3, $a0, 48
        st.d    $t4, $a0, 56
        addi.d  $t5, $t5, -1
        bnez    $t5, .Lla_move_reverse_block64_loop
        andi    $a2, $a2, 63

.Lla_move_reverse_words:
        sltui   $t0, $a2, 8
        bnez    $t0, .Lla_move_reverse_tail4
        addi.d  $a0, $a0, -8
        addi.d  $a1, $a1, -8
        ld.d    $t1, $a1, 0
        st.d    $t1, $a0, 0
        addi.d  $a2, $a2, -8
        b       .Lla_move_reverse_words
.Lla_move_reverse_tail4:
        sltui   $t0, $a2, 4
        bnez    $t0, .Lla_move_reverse_tail2
        addi.d  $a0, $a0, -4
        addi.d  $a1, $a1, -4
        ld.w    $t1, $a1, 0
        st.w    $t1, $a0, 0
        addi.d  $a2, $a2, -4
.Lla_move_reverse_tail2:
        sltui   $t0, $a2, 2
        bnez    $t0, .Lla_move_reverse_tail1
        addi.d  $a0, $a0, -2
        addi.d  $a1, $a1, -2
        ld.h    $t1, $a1, 0
        st.h    $t1, $a0, 0
        addi.d  $a2, $a2, -2
.Lla_move_reverse_tail1:
        beqz    $a2, .Lla_move_return_saved
        addi.d  $a0, $a0, -1
        addi.d  $a1, $a1, -1
        ld.b    $t1, $a1, 0
        st.b    $t1, $a0, 0
        b       .Lla_move_return_saved

.Lla_move_reverse_bytes:
        addi.d  $a0, $a0, -1
        addi.d  $a1, $a1, -1
        ld.b    $t0, $a1, 0
        st.b    $t0, $a0, 0
        addi.d  $a2, $a2, -1
        bnez    $a2, .Lla_move_reverse_bytes
.Lla_move_return_saved:
        move    $a0, $t8
.Lla_move_return:
        jr      $ra
        "#,
        ual = sym MEM_UAL,
    )
}

/// 比较两个字节序列。
///
/// # Safety
///
/// `[lhs, lhs + len)` 与 `[rhs, rhs + len)` 必须有效可读。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(_lhs: *const c_void, _rhs: *const c_void, _len: usize) -> i32 {
    naked_asm!(
        r#"
        beqz    $a2, .Lla_cmp_equal
        la.pcrel $t0, {ual}
        ld.bu   $t1, $t0, 0
        beqz    $t1, .Lla_cmp_bytes

.Lla_cmp_words:
        srli.d  $t3, $a2, 3
        beqz    $t3, .Lla_cmp_bytes
.Lla_cmp_word_loop:
        ld.d    $t1, $a0, 0
        ld.d    $t2, $a1, 0
        bne     $t1, $t2, .Lla_cmp_bytes
        addi.d  $a0, $a0, 8
        addi.d  $a1, $a1, 8
        addi.d  $t3, $t3, -1
        bnez    $t3, .Lla_cmp_word_loop
        andi    $a2, $a2, 7

.Lla_cmp_bytes:
        beqz    $a2, .Lla_cmp_equal
        ld.bu   $t0, $a0, 0
        ld.bu   $t1, $a1, 0
        bne     $t0, $t1, .Lla_cmp_diff
        addi.d  $a0, $a0, 1
        addi.d  $a1, $a1, 1
        addi.d  $a2, $a2, -1
        b       .Lla_cmp_bytes
.Lla_cmp_equal:
        move    $a0, $zero
        jr      $ra
.Lla_cmp_diff:
        sub.w   $a0, $t0, $t1
        jr      $ra
        "#,
        ual = sym MEM_UAL,
    )
}
