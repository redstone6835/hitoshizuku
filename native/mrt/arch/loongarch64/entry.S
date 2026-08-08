    .section .text._start,"ax",@progbits
    .globl _start
    .type _start,@function
_start:
    or $a4, $tp, $zero
    bl __mrt_start
    break 0
    .size _start, .-_start

    .section .text.__mrt_native_call,"ax",@progbits
    .globl __mrt_native_call
    .type __mrt_native_call,@function
__mrt_native_call:
    addi.d $sp, $sp, -16
    st.d $s0, $sp, 0
    or $s0, $a1, $zero
    or $t0, $a0, $zero
    ld.d $a7, $t0, 0
    ld.d $a6, $t0, 8
    ld.d $a0, $t0, 16
    ld.d $a1, $t0, 24
    ld.d $a2, $t0, 32
    ld.d $a3, $t0, 40
    ld.d $a4, $t0, 48
    or $a5, $zero, $zero
    syscall 0
    st.w $a0, $s0, 0
    st.w $zero, $s0, 4
    st.d $a1, $s0, 8
    st.d $a2, $s0, 16
    ld.d $s0, $sp, 0
    addi.d $sp, $sp, 16
    jr $ra
    .size __mrt_native_call, .-__mrt_native_call

    .section .note.GNU-stack,"",@progbits
