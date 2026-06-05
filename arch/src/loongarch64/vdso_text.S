.section .rodata.vdso, "a"
.balign 4

.global __mygo_vdso_blob_start
__mygo_vdso_blob_start:

.global __mygo_vdso_rt_sigreturn
__mygo_vdso_rt_sigreturn:
    li.w $a7, 139
    syscall 0

.global __mygo_vdso_clock_gettime
__mygo_vdso_clock_gettime:
    beqz $a1, .Lclock_gettime_efault
    li.w $t0, 0
    beq $a0, $t0, .Lclock_gettime_retry
    li.w $t0, 1
    beq $a0, $t0, .Lclock_gettime_retry
    li.w $t0, 7
    beq $a0, $t0, .Lclock_gettime_retry
    b .Lclock_gettime_fallback

.Lclock_gettime_retry:
    move $t8, $a1
    la.local $t0, __mygo_vdso_data_anchor
    ld.w $t1, $t0, 0
    andi $t2, $t1, 1
    bnez $t2, .Lclock_gettime_retry
    dbar 0
    ld.w $t2, $t0, 4
    bnez $t2, .Lclock_gettime_fallback
    rdtime.d $t3, $zero
    ld.d $t4, $t0, 40
    sub.d $t3, $t3, $t4
    ld.d $t5, $t0, 8
    beqz $t5, .Lclock_gettime_fallback
    div.du $t4, $t3, $t5
    mod.du $t6, $t3, $t5
    li.d $t7, 1000000000
    mul.d $t4, $t4, $t7
    mul.d $t6, $t6, $t7
    div.du $t6, $t6, $t5
    add.d $t3, $t4, $t6
    beqz $a0, .Lclock_gettime_realtime
    ld.d $t4, $t0, 32
    add.d $t4, $t4, $t3
    b .Lclock_gettime_split

.Lclock_gettime_realtime:
    ld.d $t4, $t0, 16
    ld.d $t5, $t0, 24
    li.d $t6, 1000000000
    mul.d $t4, $t4, $t6
    add.d $t4, $t4, $t5
    add.d $t4, $t4, $t3

.Lclock_gettime_split:
    li.d $t6, 1000000000
    div.du $t5, $t4, $t6
    mod.du $t4, $t4, $t6
    dbar 0
    ld.w $t2, $t0, 0
    bne $t1, $t2, .Lclock_gettime_retry
    st.d $t5, $t8, 0
    st.d $t4, $t8, 8
    move $a0, $zero
    ret

.Lclock_gettime_efault:
    li.w $a0, -14
    ret

.Lclock_gettime_fallback:
    li.w $a7, 113
    syscall 0
    ret

.global __mygo_vdso_gettimeofday
__mygo_vdso_gettimeofday:
    beqz $a0, .Lgettimeofday_ok

.Lgettimeofday_retry:
    move $t8, $a0
    la.local $t0, __mygo_vdso_data_anchor
    ld.w $t1, $t0, 0
    andi $t2, $t1, 1
    bnez $t2, .Lgettimeofday_retry
    dbar 0
    ld.w $t2, $t0, 4
    bnez $t2, .Lgettimeofday_fallback
    rdtime.d $t3, $zero
    ld.d $t4, $t0, 40
    sub.d $t3, $t3, $t4
    ld.d $t5, $t0, 8
    beqz $t5, .Lgettimeofday_fallback
    div.du $t4, $t3, $t5
    mod.du $t6, $t3, $t5
    li.d $t7, 1000000000
    mul.d $t4, $t4, $t7
    mul.d $t6, $t6, $t7
    div.du $t6, $t6, $t5
    add.d $t3, $t4, $t6
    ld.d $t4, $t0, 16
    ld.d $t5, $t0, 24
    li.d $t6, 1000000000
    mul.d $t4, $t4, $t6
    add.d $t4, $t4, $t5
    add.d $t4, $t4, $t3
    li.d $t6, 1000000000
    div.du $t5, $t4, $t6
    mod.du $t4, $t4, $t6
    li.d $t6, 1000
    div.du $t4, $t4, $t6
    dbar 0
    ld.w $t2, $t0, 0
    bne $t1, $t2, .Lgettimeofday_retry
    st.d $t5, $t8, 0
    st.d $t4, $t8, 8

.Lgettimeofday_ok:
    move $a0, $zero
    ret

.Lgettimeofday_fallback:
    li.w $a7, 169
    syscall 0
    ret

.global __mygo_vdso_clock_getres
__mygo_vdso_clock_getres:
    li.w $t0, 0
    beq $a0, $t0, .Lclock_getres_supported
    li.w $t0, 1
    beq $a0, $t0, .Lclock_getres_supported
    li.w $t0, 7
    beq $a0, $t0, .Lclock_getres_supported
    li.w $a7, 114
    syscall 0
    ret

.Lclock_getres_supported:
    beqz $a1, .Lclock_getres_ok
    st.d $zero, $a1, 0
    la.local $t0, __mygo_vdso_data_anchor
    ld.w $t1, $t0, 68
    bnez $t1, .Lclock_getres_store
    li.w $t1, 1
.Lclock_getres_store:
    st.d $t1, $a1, 8

.Lclock_getres_ok:
    move $a0, $zero
    ret

.global __mygo_vdso_getcpu
__mygo_vdso_getcpu:
    la.local $t0, __mygo_vdso_data_anchor
    beqz $a0, .Lgetcpu_skip_cpu
    ld.w $t1, $t0, 60
    st.w $t1, $a0, 0
.Lgetcpu_skip_cpu:
    beqz $a1, .Lgetcpu_ok
    ld.w $t1, $t0, 64
    st.w $t1, $a1, 0
.Lgetcpu_ok:
    move $a0, $zero
    ret

.global __mygo_vdso_text_end
__mygo_vdso_text_end:

.org 0xe00
.global __mygo_vdso_blob_end
.global __mygo_vdso_data_anchor
__mygo_vdso_blob_end:
__mygo_vdso_data_anchor:
