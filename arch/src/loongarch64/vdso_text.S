.section .rodata.vdso, "a"
.balign 4

.equ SYS_CLOCK_GETTIME, 113
.equ SYS_CLOCK_GETRES, 114
.equ SYS_GETTIMEOFDAY, 169
.equ SYS_GETCPU, 168
.equ SYS_RT_SIGRETURN, 139

.equ CLOCK_REALTIME, 0
.equ CLOCK_MONOTONIC, 1
.equ CLOCK_REALTIME_COARSE, 5
.equ CLOCK_MONOTONIC_COARSE, 6
.equ CLOCK_BOOTTIME, 7

.equ VDSO_CLOCK_MODE_RDTIME, 0

.equ VDSO_DATA_SEQ_OFFSET, 0x00
.equ VDSO_DATA_CLOCK_MODE_OFFSET, 0x04
.equ VDSO_DATA_HZ_OFFSET, 0x08
.equ VDSO_DATA_WALL_TIME_SEC_OFFSET, 0x10
.equ VDSO_DATA_WALL_TIME_NSEC_OFFSET, 0x18
.equ VDSO_DATA_MONOTONIC_BASE_NS_OFFSET, 0x20
.equ VDSO_DATA_CS_CYCLE_LAST_OFFSET, 0x28
.equ VDSO_DATA_CS_MULT_OFFSET, 0x30
.equ VDSO_DATA_CS_SHIFT_OFFSET, 0x38
.equ VDSO_DATA_CLOCK_REALTIME_RES_OFFSET, 0x44

.equ NSEC_PER_SEC, 1000000000
.equ USEC_PER_SEC, 1000000
.equ NSEC_PER_USEC, 1000
.equ VDSO_TIME_REALTIME, 0
.equ VDSO_TIME_MONOTONIC, 1

.global __mygo_vdso_blob_start
__mygo_vdso_blob_start:

.global __mygo_vdso_rt_sigreturn
__mygo_vdso_rt_sigreturn:
    li.w $a7, SYS_RT_SIGRETURN
    syscall 0

.global __mygo_vdso_clock_gettime
__mygo_vdso_clock_gettime:
    li.w $r6, VDSO_TIME_REALTIME
    beq $a0, $zero, .Lclock_gettime_supported
    li.w $r12, CLOCK_REALTIME_COARSE
    beq $a0, $r12, .Lclock_gettime_supported
    li.w $r6, VDSO_TIME_MONOTONIC
    li.w $r12, CLOCK_MONOTONIC
    beq $a0, $r12, .Lclock_gettime_supported
    li.w $r12, CLOCK_MONOTONIC_COARSE
    beq $a0, $r12, .Lclock_gettime_supported
    li.w $r12, CLOCK_BOOTTIME
    beq $a0, $r12, .Lclock_gettime_supported
    b .Lclock_gettime_fallback

.Lclock_gettime_supported:
    beqz $a1, .Lclock_gettime_fallback
    la.local $r12, __mygo_vdso_data_anchor

.Lclock_gettime_retry:
    ld.wu $r13, $r12, VDSO_DATA_SEQ_OFFSET
    andi $r14, $r13, 1
    bnez $r14, .Lclock_gettime_retry
    dbar 0

    ld.wu $r14, $r12, VDSO_DATA_CLOCK_MODE_OFFSET
    bnez $r14, .Lclock_gettime_fallback
    ld.d $r15, $r12, VDSO_DATA_HZ_OFFSET
    beqz $r15, .Lclock_gettime_fallback
    ld.d $r16, $r12, VDSO_DATA_WALL_TIME_SEC_OFFSET
    ld.d $r17, $r12, VDSO_DATA_WALL_TIME_NSEC_OFFSET
    ld.d $r18, $r12, VDSO_DATA_MONOTONIC_BASE_NS_OFFSET
    ld.d $r19, $r12, VDSO_DATA_CS_CYCLE_LAST_OFFSET
    ld.d $r20, $r12, VDSO_DATA_CS_MULT_OFFSET
    ld.wu $r7, $r12, VDSO_DATA_CS_SHIFT_OFFSET
    rdtime.d $r8, $zero

    dbar 0
    ld.wu $r9, $r12, VDSO_DATA_SEQ_OFFSET
    bne $r13, $r9, .Lclock_gettime_retry

    sub.d $r8, $r8, $r19
    mul.d $r9, $r8, $r20
    mulh.du $r10, $r8, $r20
    srl.d $r9, $r9, $r7
    li.w $r11, 64
    sub.d $r11, $r11, $r7
    sll.d $r10, $r10, $r11
    or $r9, $r9, $r10

    bnez $r6, .Lclock_gettime_monotonic

    add.d $r17, $r17, $r9
    li.w $r10, NSEC_PER_SEC
    div.du $r18, $r17, $r10
    mod.du $r17, $r17, $r10
    add.d $r16, $r16, $r18
    b .Lclock_gettime_store

.Lclock_gettime_monotonic:
    add.d $r18, $r18, $r9
    li.w $r10, NSEC_PER_SEC
    div.du $r16, $r18, $r10
    mod.du $r17, $r18, $r10

.Lclock_gettime_store:
    st.d $r16, $a1, 0
    st.d $r17, $a1, 8
    addi.d $a0, $zero, 0
    ret

.Lclock_gettime_fallback:
    li.w $a7, SYS_CLOCK_GETTIME
    syscall 0
    ret

.global __mygo_vdso_gettimeofday
__mygo_vdso_gettimeofday:
    la.local $r12, __mygo_vdso_data_anchor

.Lgettimeofday_retry:
    ld.wu $r13, $r12, VDSO_DATA_SEQ_OFFSET
    andi $r14, $r13, 1
    bnez $r14, .Lgettimeofday_retry
    dbar 0

    ld.wu $r14, $r12, VDSO_DATA_CLOCK_MODE_OFFSET
    bnez $r14, .Lgettimeofday_fallback
    ld.d $r15, $r12, VDSO_DATA_HZ_OFFSET
    beqz $r15, .Lgettimeofday_fallback
    ld.d $r16, $r12, VDSO_DATA_WALL_TIME_SEC_OFFSET
    ld.d $r17, $r12, VDSO_DATA_WALL_TIME_NSEC_OFFSET
    ld.d $r19, $r12, VDSO_DATA_CS_CYCLE_LAST_OFFSET
    ld.d $r20, $r12, VDSO_DATA_CS_MULT_OFFSET
    ld.wu $r7, $r12, VDSO_DATA_CS_SHIFT_OFFSET
    rdtime.d $r8, $zero

    dbar 0
    ld.wu $r9, $r12, VDSO_DATA_SEQ_OFFSET
    bne $r13, $r9, .Lgettimeofday_retry

    sub.d $r8, $r8, $r19
    mul.d $r9, $r8, $r20
    mulh.du $r10, $r8, $r20
    srl.d $r9, $r9, $r7
    li.w $r11, 64
    sub.d $r11, $r11, $r7
    sll.d $r10, $r10, $r11
    or $r9, $r9, $r10

    add.d $r17, $r17, $r9
    li.w $r10, NSEC_PER_SEC
    div.du $r18, $r17, $r10
    mod.du $r17, $r17, $r10
    add.d $r16, $r16, $r18
    li.w $r10, NSEC_PER_USEC
    div.du $r17, $r17, $r10

    beqz $a0, .Lgettimeofday_success
    st.d $r16, $a0, 0
    st.d $r17, $a0, 8

.Lgettimeofday_success:
    addi.d $a0, $zero, 0
    ret

.Lgettimeofday_fallback:
    li.w $a7, SYS_GETTIMEOFDAY
    syscall 0
    ret

.global __mygo_vdso_clock_getres
__mygo_vdso_clock_getres:
    beq $a0, $zero, .Lclock_getres_supported
    li.w $r12, CLOCK_REALTIME_COARSE
    beq $a0, $r12, .Lclock_getres_supported
    li.w $r12, CLOCK_MONOTONIC
    beq $a0, $r12, .Lclock_getres_supported
    li.w $r12, CLOCK_MONOTONIC_COARSE
    beq $a0, $r12, .Lclock_getres_supported
    li.w $r12, CLOCK_BOOTTIME
    beq $a0, $r12, .Lclock_getres_supported
    b .Lclock_getres_fallback

.Lclock_getres_supported:
    la.local $r12, __mygo_vdso_data_anchor

.Lclock_getres_retry:
    ld.wu $r13, $r12, VDSO_DATA_SEQ_OFFSET
    andi $r14, $r13, 1
    bnez $r14, .Lclock_getres_retry
    dbar 0

    ld.wu $r14, $r12, VDSO_DATA_CLOCK_MODE_OFFSET
    bnez $r14, .Lclock_getres_fallback
    ld.d $r15, $r12, VDSO_DATA_HZ_OFFSET
    beqz $r15, .Lclock_getres_fallback
    ld.wu $r16, $r12, VDSO_DATA_CLOCK_REALTIME_RES_OFFSET

    dbar 0
    ld.wu $r17, $r12, VDSO_DATA_SEQ_OFFSET
    bne $r13, $r17, .Lclock_getres_retry

    beqz $a1, .Lclock_getres_success
    st.d $zero, $a1, 0
    st.d $r16, $a1, 8

.Lclock_getres_success:
    addi.d $a0, $zero, 0
    ret

.Lclock_getres_fallback:
    li.w $a7, SYS_CLOCK_GETRES
    syscall 0
    ret

.global __mygo_vdso_getcpu
__mygo_vdso_getcpu:
    li.w $a7, SYS_GETCPU
    syscall 0
    ret

.global __mygo_vdso_text_end
__mygo_vdso_text_end:

.org 0xe00
.global __mygo_vdso_blob_end
.global __mygo_vdso_data_anchor
__mygo_vdso_blob_end:
__mygo_vdso_data_anchor:
