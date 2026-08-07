    .option norelax
    .section .text._start,"ax",@progbits
    .globl _start
    .type _start,@function
_start:
    mv a3, tp
    call __mrt_start
    ebreak
    .size _start, .-_start

    .section .text.__mrt_native_call,"ax",@progbits
    .globl __mrt_native_call
    .type __mrt_native_call,@function
__mrt_native_call:
    addi sp, sp, -16
    sd s0, 0(sp)
    mv s0, a1
    mv t0, a0
    ld a7, 0(t0)
    ld a6, 8(t0)
    ld a0, 16(t0)
    ld a1, 24(t0)
    ld a2, 32(t0)
    ld a3, 40(t0)
    ld a4, 48(t0)
    li a5, 0
    ecall
    sw a0, 0(s0)
    sw zero, 4(s0)
    sd a1, 8(s0)
    sd a2, 16(s0)
    ld s0, 0(sp)
    addi sp, sp, 16
    ret
    .size __mrt_native_call, .-__mrt_native_call

    .section .note.GNU-stack,"",@progbits
