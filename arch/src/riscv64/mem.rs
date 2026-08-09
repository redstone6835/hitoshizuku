//! RISC-V64 内核内存原语。
//!
//! 实现只使用基础整数指令，不要求向量扩展，也不假定平台能够高效处理
//! 非对齐机器字访问。共同对齐的区域走展开块循环，不同余区域通过对齐加载拼接。

#![allow(named_asm_labels)]

use core::arch::naked_asm;

/// 复制两个不重叠的内存区域。
///
/// # Safety
///
/// `[src, src + len)` 与 `[dst, dst + len)` 必须有效且不得重叠。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(_dst: *mut u8, _src: *const u8, _len: usize) -> *mut u8 {
    naked_asm!(
        r#"
        .option push
        .option norvc

        mv      t6, a0
        beqz    a2, .Lrv_copy_return
        li      t0, 8
        bltu    a2, t0, .Lrv_copy_scalar_tail

        /* 只有源和目的地址同余时，才能同时对齐后使用机器字访问。 */
        xor     t0, a0, a1
        andi    t0, t0, 7
        bnez    t0, .Lrv_copy_scalar

        andi    t0, a0, 7
        beqz    t0, .Lrv_copy_blocks
        li      t1, 8
        sub     t1, t1, t0
        bltu    a2, t1, .Lrv_copy_bytes
.Lrv_copy_align:
        lbu     t2, 0(a1)
        sb      t2, 0(a0)
        addi    a0, a0, 1
        addi    a1, a1, 1
        addi    a2, a2, -1
        addi    t1, t1, -1
        bnez    t1, .Lrv_copy_align

        /* 每轮 256 字节，分成四个互不重叠的 64 字节读取组。 */
.Lrv_copy_blocks:
        li      t0, 256
        bltu    a2, t0, .Lrv_copy_block64
        ld      a3, 0(a1)
        ld      a4, 8(a1)
        ld      a5, 16(a1)
        ld      a6, 24(a1)
        ld      a7, 32(a1)
        ld      t0, 40(a1)
        ld      t1, 48(a1)
        ld      t2, 56(a1)
        sd      a3, 0(a0)
        sd      a4, 8(a0)
        sd      a5, 16(a0)
        sd      a6, 24(a0)
        sd      a7, 32(a0)
        sd      t0, 40(a0)
        sd      t1, 48(a0)
        sd      t2, 56(a0)
        ld      a3, 64(a1)
        ld      a4, 72(a1)
        ld      a5, 80(a1)
        ld      a6, 88(a1)
        ld      a7, 96(a1)
        ld      t0, 104(a1)
        ld      t1, 112(a1)
        ld      t2, 120(a1)
        sd      a3, 64(a0)
        sd      a4, 72(a0)
        sd      a5, 80(a0)
        sd      a6, 88(a0)
        sd      a7, 96(a0)
        sd      t0, 104(a0)
        sd      t1, 112(a0)
        sd      t2, 120(a0)
        ld      a3, 128(a1)
        ld      a4, 136(a1)
        ld      a5, 144(a1)
        ld      a6, 152(a1)
        ld      a7, 160(a1)
        ld      t0, 168(a1)
        ld      t1, 176(a1)
        ld      t2, 184(a1)
        sd      a3, 128(a0)
        sd      a4, 136(a0)
        sd      a5, 144(a0)
        sd      a6, 152(a0)
        sd      a7, 160(a0)
        sd      t0, 168(a0)
        sd      t1, 176(a0)
        sd      t2, 184(a0)
        ld      a3, 192(a1)
        ld      a4, 200(a1)
        ld      a5, 208(a1)
        ld      a6, 216(a1)
        ld      a7, 224(a1)
        ld      t0, 232(a1)
        ld      t1, 240(a1)
        ld      t2, 248(a1)
        sd      a3, 192(a0)
        sd      a4, 200(a0)
        sd      a5, 208(a0)
        sd      a6, 216(a0)
        sd      a7, 224(a0)
        sd      t0, 232(a0)
        sd      t1, 240(a0)
        sd      t2, 248(a0)
        addi    a0, a0, 256
        addi    a1, a1, 256
        addi    a2, a2, -256
        j       .Lrv_copy_blocks

.Lrv_copy_block64:
        li      t0, 64
        bltu    a2, t0, .Lrv_copy_block32
        ld      a3, 0(a1)
        ld      a4, 8(a1)
        ld      a5, 16(a1)
        ld      a6, 24(a1)
        ld      a7, 32(a1)
        ld      t0, 40(a1)
        ld      t1, 48(a1)
        ld      t2, 56(a1)
        sd      a3, 0(a0)
        sd      a4, 8(a0)
        sd      a5, 16(a0)
        sd      a6, 24(a0)
        sd      a7, 32(a0)
        sd      t0, 40(a0)
        sd      t1, 48(a0)
        sd      t2, 56(a0)
        addi    a0, a0, 64
        addi    a1, a1, 64
        addi    a2, a2, -64
        j       .Lrv_copy_block64

.Lrv_copy_block32:
        li      t0, 32
        bltu    a2, t0, .Lrv_copy_words
        ld      a3, 0(a1)
        ld      a4, 8(a1)
        ld      a5, 16(a1)
        ld      a6, 24(a1)
        sd      a3, 0(a0)
        sd      a4, 8(a0)
        sd      a5, 16(a0)
        sd      a6, 24(a0)
        addi    a0, a0, 32
        addi    a1, a1, 32
        addi    a2, a2, -32

.Lrv_copy_words:
        li      t0, 8
        bltu    a2, t0, .Lrv_copy_bytes
.Lrv_copy_word_loop:
        ld      t1, 0(a1)
        sd      t1, 0(a0)
        addi    a0, a0, 8
        addi    a1, a1, 8
        addi    a2, a2, -8
        bgeu    a2, t0, .Lrv_copy_word_loop
        j       .Lrv_copy_bytes

        /* 不同余地址先对齐目的端，再用两个对齐双字拼出源端的错位双字。
         * 小复制保留集中式字节循环，避免支付对齐和移位的固定成本。 */
.Lrv_copy_scalar:
        li      t0, 24
        bltu    a2, t0, .Lrv_copy_scalar_tail
        mv      t5, a1

        andi    t0, a0, 7
        beqz    t0, .Lrv_copy_misaligned_check_prefix
        li      t1, 8
        sub     t1, t1, t0
.Lrv_copy_misaligned_align:
        lbu     t2, 0(a1)
        sb      t2, 0(a0)
        addi    a0, a0, 1
        addi    a1, a1, 1
        addi    a2, a2, -1
        addi    t1, t1, -1
        bnez    t1, .Lrv_copy_misaligned_align

.Lrv_copy_misaligned_check_prefix:
        andi    t0, a1, -8
        bgeu    t0, t5, .Lrv_copy_misaligned_prepare

        /* 第一次向下对齐会越过源区间起点时，先精确复制八字节。
         * 目的地址仍保持对齐，随后所有对齐读取都位于原始源区间内。 */
        lbu     a3, 0(a1)
        lbu     a4, 1(a1)
        lbu     a5, 2(a1)
        lbu     a6, 3(a1)
        lbu     a7, 4(a1)
        lbu     t0, 5(a1)
        lbu     t1, 6(a1)
        lbu     t2, 7(a1)
        sb      a3, 0(a0)
        sb      a4, 1(a0)
        sb      a5, 2(a0)
        sb      a6, 3(a0)
        sb      a7, 4(a0)
        sb      t0, 5(a0)
        sb      t1, 6(a0)
        sb      t2, 7(a0)
        addi    a0, a0, 8
        addi    a1, a1, 8
        addi    a2, a2, -8

.Lrv_copy_misaligned_prepare:
        andi    t1, a1, 7
        li      t2, 16
        sub     t2, t2, t1
        bltu    a2, t2, .Lrv_copy_scalar_tail
        andi    t4, a1, -8
        ld      a3, 0(t4)
        slli    t3, t1, 3
        li      t5, 64
        sub     t5, t5, t3

        /* 每个新双字只需再读一个对齐双字；32 字节循环减少分支开销。
         * t2 是产生八字节时所需的最小剩余长度，确保高端读取不越界。 */
.Lrv_copy_misaligned_blocks:
        addi    a7, t2, 24
        bltu    a2, a7, .Lrv_copy_misaligned_words
        ld      a4, 8(t4)
        srl     t0, a3, t3
        sll     t1, a4, t5
        or      t0, t0, t1
        sd      t0, 0(a0)
        ld      a5, 16(t4)
        srl     t0, a4, t3
        sll     t1, a5, t5
        or      t0, t0, t1
        sd      t0, 8(a0)
        ld      a6, 24(t4)
        srl     t0, a5, t3
        sll     t1, a6, t5
        or      t0, t0, t1
        sd      t0, 16(a0)
        ld      a7, 32(t4)
        srl     t0, a6, t3
        sll     t1, a7, t5
        or      t0, t0, t1
        sd      t0, 24(a0)
        mv      a3, a7
        addi    t4, t4, 32
        addi    a0, a0, 32
        addi    a1, a1, 32
        addi    a2, a2, -32
        j       .Lrv_copy_misaligned_blocks

.Lrv_copy_misaligned_words:
        bltu    a2, t2, .Lrv_copy_scalar_tail
        ld      a4, 8(t4)
        srl     t0, a3, t3
        sll     t1, a4, t5
        or      t0, t0, t1
        sd      t0, 0(a0)
        mv      a3, a4
        addi    t4, t4, 8
        addi    a0, a0, 8
        addi    a1, a1, 8
        addi    a2, a2, -8
        j       .Lrv_copy_misaligned_words

        /* 短复制和拼接尾部保留自然对齐的 4/2 字节访问。 */
.Lrv_copy_scalar_tail:
        or      t0, a0, a1
        andi    t1, t0, 3
        bnez    t1, .Lrv_copy_half_check
        li      t1, 4
.Lrv_copy_u32_loop:
        bltu    a2, t1, .Lrv_copy_bytes
        lw      t2, 0(a1)
        sw      t2, 0(a0)
        addi    a0, a0, 4
        addi    a1, a1, 4
        addi    a2, a2, -4
        j       .Lrv_copy_u32_loop
.Lrv_copy_half_check:
        andi    t0, t0, 1
        bnez    t0, .Lrv_copy_byte_blocks
        li      t1, 2
.Lrv_copy_u16_loop:
        bltu    a2, t1, .Lrv_copy_bytes
        lhu     t2, 0(a1)
        sh      t2, 0(a0)
        addi    a0, a0, 2
        addi    a1, a1, 2
        addi    a2, a2, -2
        j       .Lrv_copy_u16_loop

        /* 错位地址每轮先预读八个字节，再集中写入。 */
.Lrv_copy_byte_blocks:
        li      t0, 8
        bltu    a2, t0, .Lrv_copy_bytes
        lbu     a3, 0(a1)
        lbu     a4, 1(a1)
        lbu     a5, 2(a1)
        lbu     a6, 3(a1)
        lbu     a7, 4(a1)
        lbu     t0, 5(a1)
        lbu     t1, 6(a1)
        lbu     t2, 7(a1)
        sb      a3, 0(a0)
        sb      a4, 1(a0)
        sb      a5, 2(a0)
        sb      a6, 3(a0)
        sb      a7, 4(a0)
        sb      t0, 5(a0)
        sb      t1, 6(a0)
        sb      t2, 7(a0)
        addi    a0, a0, 8
        addi    a1, a1, 8
        addi    a2, a2, -8
        j       .Lrv_copy_byte_blocks

.Lrv_copy_bytes:
        beqz    a2, .Lrv_copy_return
.Lrv_copy_byte_loop:
        lbu     t0, 0(a1)
        sb      t0, 0(a0)
        addi    a0, a0, 1
        addi    a1, a1, 1
        addi    a2, a2, -1
        bnez    a2, .Lrv_copy_byte_loop

.Lrv_copy_return:
        mv      a0, t6
        .option pop
        ret
        "#,
    )
}

/// 用给定字节填充内存区域。
///
/// # Safety
///
/// `[dst, dst + len)` 必须有效且可写。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(_dst: *mut u8, _value: i32, _len: usize) -> *mut u8 {
    naked_asm!(
        r#"
        .option push
        .option norvc

        mv      t6, a0
        beqz    a2, .Lrv_set_return

        andi    a1, a1, 255
        /* 小区域只使用低字节，跳过构造完整机器字的固定成本。 */
        li      t0, 16
        bltu    a2, t0, .Lrv_set_bytes
        slli    t0, a1, 8
        or      a1, a1, t0
        slli    t0, a1, 16
        or      a1, a1, t0
        slli    t0, a1, 32
        or      a1, a1, t0

        andi    t0, a0, 7
        beqz    t0, .Lrv_set_blocks
        li      t1, 8
        sub     t1, t1, t0
.Lrv_set_align:
        sb      a1, 0(a0)
        addi    a0, a0, 1
        addi    a2, a2, -1
        addi    t1, t1, -1
        bnez    t1, .Lrv_set_align

.Lrv_set_blocks:
        andi    t0, a2, -256
        beqz    t0, .Lrv_set_block64
        add     t1, a0, t0
        sub     a2, a2, t0
        .align 6
.Lrv_set_block_loop:
        sd      a1, 0(a0)
        sd      a1, 8(a0)
        sd      a1, 16(a0)
        sd      a1, 24(a0)
        sd      a1, 32(a0)
        sd      a1, 40(a0)
        sd      a1, 48(a0)
        sd      a1, 56(a0)
        sd      a1, 64(a0)
        sd      a1, 72(a0)
        sd      a1, 80(a0)
        sd      a1, 88(a0)
        sd      a1, 96(a0)
        sd      a1, 104(a0)
        sd      a1, 112(a0)
        sd      a1, 120(a0)
        sd      a1, 128(a0)
        sd      a1, 136(a0)
        sd      a1, 144(a0)
        sd      a1, 152(a0)
        sd      a1, 160(a0)
        sd      a1, 168(a0)
        sd      a1, 176(a0)
        sd      a1, 184(a0)
        sd      a1, 192(a0)
        sd      a1, 200(a0)
        sd      a1, 208(a0)
        sd      a1, 216(a0)
        sd      a1, 224(a0)
        sd      a1, 232(a0)
        sd      a1, 240(a0)
        sd      a1, 248(a0)
        addi    a0, a0, 256
        bltu    a0, t1, .Lrv_set_block_loop

.Lrv_set_block64:
        li      t0, 64
        bltu    a2, t0, .Lrv_set_words
        sd      a1, 0(a0)
        sd      a1, 8(a0)
        sd      a1, 16(a0)
        sd      a1, 24(a0)
        sd      a1, 32(a0)
        sd      a1, 40(a0)
        sd      a1, 48(a0)
        sd      a1, 56(a0)
        addi    a0, a0, 64
        addi    a2, a2, -64
        j       .Lrv_set_block64

.Lrv_set_words:
        li      t0, 8
        bltu    a2, t0, .Lrv_set_bytes
.Lrv_set_word_loop:
        sd      a1, 0(a0)
        addi    a0, a0, 8
        addi    a2, a2, -8
        bgeu    a2, t0, .Lrv_set_word_loop

.Lrv_set_bytes:
        beqz    a2, .Lrv_set_return
.Lrv_set_byte_loop:
        sb      a1, 0(a0)
        addi    a0, a0, 1
        addi    a2, a2, -1
        bnez    a2, .Lrv_set_byte_loop

.Lrv_set_return:
        mv      a0, t6
        .option pop
        ret
        "#,
    )
}

/// 复制可能重叠的内存区域。
///
/// # Safety
///
/// `[src, src + len)` 与 `[dst, dst + len)` 必须是有效内存区域，可以重叠。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(_dst: *mut u8, _src: *const u8, _len: usize) -> *mut u8 {
    naked_asm!(
        r#"
        .option push
        .option norvc

        beqz    a2, .Lrv_move_return
        beq     a0, a1, .Lrv_move_return
        bltu    a0, a1, memcpy
        add     t0, a1, a2
        bgeu    a0, t0, memcpy

        /* 重叠且目的在源之后，从区间尾部向前复制。 */
        mv      t6, a0
        add     a0, a0, a2
        add     a1, a1, a2
        xor     t0, a0, a1
        andi    t0, t0, 7
        bnez    t0, .Lrv_move_reverse_byte_blocks

        andi    t0, a0, 7
        beqz    t0, .Lrv_move_reverse_blocks
.Lrv_move_reverse_align:
        addi    a0, a0, -1
        addi    a1, a1, -1
        lbu     t1, 0(a1)
        sb      t1, 0(a0)
        addi    a2, a2, -1
        addi    t0, t0, -1
        bnez    t0, .Lrv_move_reverse_align

.Lrv_move_reverse_blocks:
        /* 距离至少 64 字节时每轮处理两个块；先完成上方块的读写，
         * 再访问下方块，即使恰好 64 字节重叠也保持 memmove 语义。 */
        sub     t3, a0, a1
        li      t4, 64
        bltu    t3, t4, .Lrv_move_reverse_blocks_narrow
        li      t0, 128
        bltu    a2, t0, .Lrv_move_reverse_blocks_narrow
        addi    a0, a0, -64
        addi    a1, a1, -64
        ld      a3, 0(a1)
        ld      a4, 8(a1)
        ld      a5, 16(a1)
        ld      a6, 24(a1)
        ld      a7, 32(a1)
        ld      t0, 40(a1)
        ld      t1, 48(a1)
        ld      t2, 56(a1)
        sd      a3, 0(a0)
        sd      a4, 8(a0)
        sd      a5, 16(a0)
        sd      a6, 24(a0)
        sd      a7, 32(a0)
        sd      t0, 40(a0)
        sd      t1, 48(a0)
        sd      t2, 56(a0)
        addi    a0, a0, -64
        addi    a1, a1, -64
        ld      a3, 0(a1)
        ld      a4, 8(a1)
        ld      a5, 16(a1)
        ld      a6, 24(a1)
        ld      a7, 32(a1)
        ld      t0, 40(a1)
        ld      t1, 48(a1)
        ld      t2, 56(a1)
        sd      a3, 0(a0)
        sd      a4, 8(a0)
        sd      a5, 16(a0)
        sd      a6, 24(a0)
        sd      a7, 32(a0)
        sd      t0, 40(a0)
        sd      t1, 48(a0)
        sd      t2, 56(a0)
        addi    a2, a2, -128
        j       .Lrv_move_reverse_blocks

.Lrv_move_reverse_blocks_narrow:
        li      t0, 64
        bltu    a2, t0, .Lrv_move_reverse_words
        addi    a0, a0, -64
        addi    a1, a1, -64
        ld      a3, 0(a1)
        ld      a4, 8(a1)
        ld      a5, 16(a1)
        ld      a6, 24(a1)
        ld      a7, 32(a1)
        ld      t0, 40(a1)
        ld      t1, 48(a1)
        ld      t2, 56(a1)
        sd      a3, 0(a0)
        sd      a4, 8(a0)
        sd      a5, 16(a0)
        sd      a6, 24(a0)
        sd      a7, 32(a0)
        sd      t0, 40(a0)
        sd      t1, 48(a0)
        sd      t2, 56(a0)
        addi    a2, a2, -64
        j       .Lrv_move_reverse_blocks

.Lrv_move_reverse_words:
        li      t0, 8
        bltu    a2, t0, .Lrv_move_reverse_bytes
.Lrv_move_reverse_word_loop:
        addi    a0, a0, -8
        addi    a1, a1, -8
        ld      t1, 0(a1)
        sd      t1, 0(a0)
        addi    a2, a2, -8
        bgeu    a2, t0, .Lrv_move_reverse_word_loop

.Lrv_move_reverse_byte_blocks:
        li      t0, 8
        bltu    a2, t0, .Lrv_move_reverse_bytes
        addi    a0, a0, -8
        addi    a1, a1, -8
        lbu     a3, 0(a1)
        lbu     a4, 1(a1)
        lbu     a5, 2(a1)
        lbu     a6, 3(a1)
        lbu     a7, 4(a1)
        lbu     t0, 5(a1)
        lbu     t1, 6(a1)
        lbu     t2, 7(a1)
        sb      a3, 0(a0)
        sb      a4, 1(a0)
        sb      a5, 2(a0)
        sb      a6, 3(a0)
        sb      a7, 4(a0)
        sb      t0, 5(a0)
        sb      t1, 6(a0)
        sb      t2, 7(a0)
        addi    a2, a2, -8
        j       .Lrv_move_reverse_byte_blocks

.Lrv_move_reverse_bytes:
        beqz    a2, .Lrv_move_return_saved
.Lrv_move_reverse_byte_loop:
        addi    a0, a0, -1
        addi    a1, a1, -1
        lbu     t0, 0(a1)
        sb      t0, 0(a0)
        addi    a2, a2, -1
        bnez    a2, .Lrv_move_reverse_byte_loop
.Lrv_move_return_saved:
        mv      a0, t6
        j       .Lrv_move_final

.Lrv_move_return:
.Lrv_move_final:
        .option pop
        ret
        "#,
    )
}

/// 比较两个字节序列。
///
/// # Safety
///
/// `[lhs, lhs + len)` 与 `[rhs, rhs + len)` 必须有效可读。
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(_lhs: *const u8, _rhs: *const u8, _len: usize) -> i32 {
    naked_asm!(
        r#"
        .option push
        .option norvc

        beqz    a2, .Lrv_cmp_equal
        xor     t0, a0, a1
        andi    t0, t0, 7
        bnez    t0, .Lrv_cmp_bytes

        andi    t0, a0, 7
        beqz    t0, .Lrv_cmp_words
.Lrv_cmp_align:
        lbu     t1, 0(a0)
        lbu     t2, 0(a1)
        bne     t1, t2, .Lrv_cmp_diff
        addi    a0, a0, 1
        addi    a1, a1, 1
        addi    a2, a2, -1
        beqz    a2, .Lrv_cmp_equal
        addi    t0, t0, -1
        bnez    t0, .Lrv_cmp_align

.Lrv_cmp_words:
        li      t0, 8
        bltu    a2, t0, .Lrv_cmp_bytes
.Lrv_cmp_word_loop:
        ld      t1, 0(a0)
        ld      t2, 0(a1)
        bne     t1, t2, .Lrv_cmp_bytes
        addi    a0, a0, 8
        addi    a1, a1, 8
        addi    a2, a2, -8
        bgeu    a2, t0, .Lrv_cmp_word_loop

.Lrv_cmp_bytes:
        beqz    a2, .Lrv_cmp_equal
.Lrv_cmp_byte_loop:
        lbu     t1, 0(a0)
        lbu     t2, 0(a1)
        bne     t1, t2, .Lrv_cmp_diff
        addi    a0, a0, 1
        addi    a1, a1, 1
        addi    a2, a2, -1
        bnez    a2, .Lrv_cmp_byte_loop

.Lrv_cmp_equal:
        li      a0, 0
        j       .Lrv_cmp_return
.Lrv_cmp_diff:
        subw    a0, t1, t2
.Lrv_cmp_return:
        .option pop
        ret
        "#,
    )
}
