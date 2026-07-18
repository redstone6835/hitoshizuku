/*
 * RISC-V64 vDSO 用户态代码段。
 *
 * clock_gettime/gettimeofday/getcpu/clock_getres 快路径读取紧邻代码页的共享
 * 数据页；共享页由 kernel 以 seqlock 协议更新。boot 阶段为当前 hart 设置
 * scounteren.TIME，因此用户态可以直接执行 rdtime；共享页无效时回退 syscall。
 */

.section .rodata.vdso, "a"
.balign 4
.option push
.option norvc
.option arch, +m

.equ SYS_CLOCK_GETTIME, 113
.equ SYS_CLOCK_GETRES, 114
.equ SYS_RT_SIGRETURN, 139
.equ SYS_GETCPU, 168
.equ SYS_GETTIMEOFDAY, 169

.equ CLOCK_REALTIME, 0
.equ CLOCK_MONOTONIC, 1
.equ CLOCK_MONOTONIC_RAW, 4
.equ CLOCK_REALTIME_COARSE, 5
.equ CLOCK_MONOTONIC_COARSE, 6
.equ CLOCK_BOOTTIME, 7

.equ VDSO_DATA_SEQ_OFFSET, {vdso_data_seq_offset}
.equ VDSO_DATA_CLOCK_MODE_OFFSET, {vdso_data_clock_mode_offset}
.equ VDSO_DATA_HZ_OFFSET, {vdso_data_hz_offset}
.equ VDSO_DATA_WALL_TIME_SEC_OFFSET, {vdso_data_wall_time_sec_offset}
.equ VDSO_DATA_WALL_TIME_NSEC_OFFSET, {vdso_data_wall_time_nsec_offset}
.equ VDSO_DATA_MONOTONIC_BASE_NS_OFFSET, {vdso_data_monotonic_base_ns_offset}
.equ VDSO_DATA_CS_CYCLE_LAST_OFFSET, {vdso_data_cs_cycle_last_offset}
.equ VDSO_DATA_CS_MULT_OFFSET, {vdso_data_cs_mult_offset}
.equ VDSO_DATA_CS_SHIFT_OFFSET, {vdso_data_cs_shift_offset}
.equ VDSO_DATA_CPU_ID_OFFSET, {vdso_data_cpu_id_offset}
.equ VDSO_DATA_NODE_ID_OFFSET, {vdso_data_node_id_offset}
.equ VDSO_DATA_CLOCK_REALTIME_RES_OFFSET, {vdso_data_clock_realtime_res_offset}

.equ NSEC_PER_SEC, 1000000000
.equ QEMU_TIMER_HZ, 10000000
.equ QEMU_NSEC_PER_TICK, 100
.equ DIV_NSEC_MAGIC, 0x0044b82fa09b5a53
.equ DIV_USEC_MAGIC, 0x10624dd3
.equ VDSO_TIME_REALTIME, 0
.equ VDSO_TIME_MONOTONIC, 1

.global __mygo_rv64_vdso_blob_start
__mygo_rv64_vdso_blob_start:

/* rt_sigreturn 必须陷入内核；ecall 不应返回。 */
.balign 4
.global __mygo_rv64_vdso_rt_sigreturn
__mygo_rv64_vdso_rt_sigreturn:
    li      a7, SYS_RT_SIGRETURN
    ecall
    unimp

/* int clock_gettime(clockid_t clock, struct timespec *tp) */
.balign 4
.global __mygo_rv64_vdso_clock_gettime
__mygo_rv64_vdso_clock_gettime:
    li      a6, VDSO_TIME_REALTIME
    beqz    a0, .Lcgt_supported
    li      t0, CLOCK_REALTIME_COARSE
    beq     a0, t0, .Lcgt_supported
    li      a6, VDSO_TIME_MONOTONIC
    li      t0, CLOCK_MONOTONIC
    beq     a0, t0, .Lcgt_supported
    li      t0, CLOCK_MONOTONIC_RAW
    beq     a0, t0, .Lcgt_supported
    li      t0, CLOCK_MONOTONIC_COARSE
    beq     a0, t0, .Lcgt_supported
    li      t0, CLOCK_BOOTTIME
    beq     a0, t0, .Lcgt_supported
    j       .Lcgt_fallback

.Lcgt_supported:
    beqz    a1, .Lcgt_fallback
    lla     t0, __mygo_rv64_vdso_data_anchor

.Lcgt_retry:
    lwu     t1, VDSO_DATA_SEQ_OFFSET(t0)
    andi    a2, t1, 1
    bnez    a2, .Lcgt_retry
    fence   r, r

    lwu     t2, VDSO_DATA_CLOCK_MODE_OFFSET(t0)
    bnez    t2, .Lcgt_fallback
    ld      t2, VDSO_DATA_HZ_OFFSET(t0)
    beqz    t2, .Lcgt_fallback
    ld      t5, VDSO_DATA_CS_CYCLE_LAST_OFFSET(t0)
    rdtime  a3
    li      a4, QEMU_TIMER_HZ
    beq     t2, a4, .Lcgt_load_base
    ld      t6, VDSO_DATA_CS_MULT_OFFSET(t0)
    lwu     a2, VDSO_DATA_CS_SHIFT_OFFSET(t0)

.Lcgt_load_base:
    bnez    a6, .Lcgt_load_monotonic
    ld      t3, VDSO_DATA_WALL_TIME_SEC_OFFSET(t0)
    ld      t4, VDSO_DATA_WALL_TIME_NSEC_OFFSET(t0)
    j       .Lcgt_data_loaded

.Lcgt_load_monotonic:
    ld      t4, VDSO_DATA_MONOTONIC_BASE_NS_OFFSET(t0)

.Lcgt_data_loaded:
    fence   r, r
    lwu     a7, VDSO_DATA_SEQ_OFFSET(t0)
    bne     t1, a7, .Lcgt_retry

    sub     a3, a3, t5
    li      a4, QEMU_TIMER_HZ
    beq     t2, a4, .Lcgt_delta_10mhz

    /* generic delta_ns = ((rdtime - cycle_last) * mult) >> shift */
    mul     t5, a3, t6
    mulhu   a3, a3, t6
    beqz    a2, .Lcgt_delta_ready
    srl     t5, t5, a2
    li      a4, 64
    sub     a4, a4, a2
    sll     a3, a3, a4
    or      t5, t5, a3
    j       .Lcgt_delta_ready

.Lcgt_delta_10mhz:
    li      a4, QEMU_NSEC_PER_TICK
    mul     t5, a3, a4

.Lcgt_delta_ready:
    add     t4, t4, t5
    bnez    a6, .Lcgt_split_monotonic
    li      a4, NSEC_PER_SEC
    bltu    t4, a4, .Lcgt_store
    sub     t4, t4, a4
    addi    t3, t3, 1
    bltu    t4, a4, .Lcgt_store

    /* timer 数据异常陈旧时仍正确处理多秒 delta；正常 100Hz tick 不走这里。 */
    li      a2, DIV_NSEC_MAGIC
    srli    a3, t4, 9
    mulhu   a3, a3, a2
    srli    a3, a3, 11
    mul     a2, a3, a4
    sub     t4, t4, a2
    add     t3, t3, a3
    j       .Lcgt_store

.Lcgt_split_monotonic:
    /* monotonic_base_ns 是现有共享 ABI；用常数倒数拆分，避免 divu/remu。 */
    li      a2, DIV_NSEC_MAGIC
    srli    t3, t4, 9
    mulhu   t3, t3, a2
    srli    t3, t3, 11
    li      a4, NSEC_PER_SEC
    mul     a2, t3, a4
    sub     t4, t4, a2

.Lcgt_store:
    sd      t3, 0(a1)
    sd      t4, 8(a1)
    li      a0, 0
    ret

.Lcgt_fallback:
    li      a7, SYS_CLOCK_GETTIME
    ecall
    ret

/* int gettimeofday(struct timeval *tv, struct timezone *tz) */
.balign 4
.global __mygo_rv64_vdso_gettimeofday
__mygo_rv64_vdso_gettimeofday:
    beqz    a0, .Lgtod_success
    lla     t0, __mygo_rv64_vdso_data_anchor

.Lgtod_retry:
    lwu     t1, VDSO_DATA_SEQ_OFFSET(t0)
    andi    a2, t1, 1
    bnez    a2, .Lgtod_retry
    fence   r, r

    lwu     t2, VDSO_DATA_CLOCK_MODE_OFFSET(t0)
    bnez    t2, .Lgtod_fallback
    ld      t2, VDSO_DATA_HZ_OFFSET(t0)
    beqz    t2, .Lgtod_fallback
    ld      t3, VDSO_DATA_WALL_TIME_SEC_OFFSET(t0)
    ld      t4, VDSO_DATA_WALL_TIME_NSEC_OFFSET(t0)
    ld      t5, VDSO_DATA_CS_CYCLE_LAST_OFFSET(t0)
    rdtime  a3
    li      a4, QEMU_TIMER_HZ
    beq     t2, a4, .Lgtod_data_loaded
    ld      t6, VDSO_DATA_CS_MULT_OFFSET(t0)
    lwu     a2, VDSO_DATA_CS_SHIFT_OFFSET(t0)

.Lgtod_data_loaded:
    fence   r, r
    lwu     a6, VDSO_DATA_SEQ_OFFSET(t0)
    bne     t1, a6, .Lgtod_retry

    sub     a3, a3, t5
    li      a4, QEMU_TIMER_HZ
    beq     t2, a4, .Lgtod_delta_10mhz

    mul     t5, a3, t6
    mulhu   a3, a3, t6
    beqz    a2, .Lgtod_delta_ready
    srl     t5, t5, a2
    li      a4, 64
    sub     a4, a4, a2
    sll     a3, a3, a4
    or      t5, t5, a3
    j       .Lgtod_delta_ready

.Lgtod_delta_10mhz:
    li      a4, QEMU_NSEC_PER_TICK
    mul     t5, a3, a4

.Lgtod_delta_ready:
    add     t4, t4, t5
    li      a4, NSEC_PER_SEC
    bltu    t4, a4, .Lgtod_store
    sub     t4, t4, a4
    addi    t3, t3, 1
    bltu    t4, a4, .Lgtod_store

    li      a2, DIV_NSEC_MAGIC
    srli    a3, t4, 9
    mulhu   a3, a3, a2
    srli    a3, a3, 11
    mul     a2, a3, a4
    sub     t4, t4, a2
    add     t3, t3, a3

.Lgtod_store:
    sd      t3, 0(a0)
    li      a2, DIV_USEC_MAGIC
    mul     t5, t4, a2
    srli    t5, t5, 38
    sd      t5, 8(a0)

.Lgtod_success:
    li      a0, 0
    ret

.Lgtod_fallback:
    li      a7, SYS_GETTIMEOFDAY
    ecall
    ret

/* int clock_getres(clockid_t clock, struct timespec *res) */
.balign 4
.global __mygo_rv64_vdso_clock_getres
__mygo_rv64_vdso_clock_getres:
    beqz    a0, .Lcgr_supported
    li      t0, CLOCK_MONOTONIC
    beq     a0, t0, .Lcgr_supported
    li      t0, CLOCK_MONOTONIC_RAW
    beq     a0, t0, .Lcgr_supported
    li      t0, CLOCK_REALTIME_COARSE
    beq     a0, t0, .Lcgr_supported
    li      t0, CLOCK_MONOTONIC_COARSE
    beq     a0, t0, .Lcgr_supported
    li      t0, CLOCK_BOOTTIME
    beq     a0, t0, .Lcgr_supported
    j       .Lcgr_fallback

.Lcgr_supported:
    beqz    a1, .Lcgr_success
    lla     t0, __mygo_rv64_vdso_data_anchor

.Lcgr_retry:
    lwu     t1, VDSO_DATA_SEQ_OFFSET(t0)
    andi    t2, t1, 1
    bnez    t2, .Lcgr_retry
    fence   r, r
    lwu     t2, VDSO_DATA_CLOCK_MODE_OFFSET(t0)
    bnez    t2, .Lcgr_fallback
    ld      t3, VDSO_DATA_HZ_OFFSET(t0)
    beqz    t3, .Lcgr_fallback
    lwu     t4, VDSO_DATA_CLOCK_REALTIME_RES_OFFSET(t0)
    fence   r, r
    lwu     t2, VDSO_DATA_SEQ_OFFSET(t0)
    bne     t1, t2, .Lcgr_retry
    sd      zero, 0(a1)
    sd      t4, 8(a1)

.Lcgr_success:
    li      a0, 0
    ret

.Lcgr_fallback:
    li      a7, SYS_CLOCK_GETRES
    ecall
    ret

/* int getcpu(unsigned *cpu, unsigned *node, struct getcpu_cache *cache) */
.balign 4
.global __mygo_rv64_vdso_getcpu
__mygo_rv64_vdso_getcpu:
    lla     t0, __mygo_rv64_vdso_data_anchor

.Lgetcpu_retry:
    lwu     t1, VDSO_DATA_SEQ_OFFSET(t0)
    andi    t2, t1, 1
    bnez    t2, .Lgetcpu_retry
    fence   r, r
    lwu     t3, VDSO_DATA_CPU_ID_OFFSET(t0)
    lwu     t4, VDSO_DATA_NODE_ID_OFFSET(t0)
    fence   r, r
    lwu     t2, VDSO_DATA_SEQ_OFFSET(t0)
    bne     t1, t2, .Lgetcpu_retry
    beqz    a0, .Lgetcpu_node
    sw      t3, 0(a0)
.Lgetcpu_node:
    beqz    a1, .Lgetcpu_success
    sw      t4, 0(a1)
.Lgetcpu_success:
    li      a0, 0
    ret

.global __mygo_rv64_vdso_text_end
__mygo_rv64_vdso_text_end:

/* TEXT_OFF(0x300) + 0xd00 = 0x1000，共享数据页恰好紧随代码页。 */
.org 0xd00
.global __mygo_rv64_vdso_blob_end
.global __mygo_rv64_vdso_data_anchor
__mygo_rv64_vdso_blob_end:
__mygo_rv64_vdso_data_anchor:

.option pop
