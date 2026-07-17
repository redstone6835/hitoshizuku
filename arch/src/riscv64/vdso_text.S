/*
 * RISC-V64 vDSO 用户态代码段。
 *
 * getcpu/clock_getres 快路径读取紧邻代码页的共享数据页；共享页由 kernel 以
 * seqlock 协议更新。clock_gettime/gettimeofday 保留 ecall：S-mode 无法在所有
 * 固件上证明 M-mode 已允许 U-mode 读取 time CSR，vDSO 不能冒险触发 SIGILL。
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
.equ VDSO_DATA_CPU_ID_OFFSET, {vdso_data_cpu_id_offset}
.equ VDSO_DATA_NODE_ID_OFFSET, {vdso_data_node_id_offset}
.equ VDSO_DATA_CLOCK_REALTIME_RES_OFFSET, {vdso_data_clock_realtime_res_offset}

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
    li      a7, SYS_CLOCK_GETTIME
    ecall
    ret

/* int gettimeofday(struct timeval *tv, struct timezone *tz) */
.balign 4
.global __mygo_rv64_vdso_gettimeofday
__mygo_rv64_vdso_gettimeofday:
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

/* TEXT_OFF(0x200) + 0xe00 = 0x1000，共享数据页恰好紧随代码页。 */
.org 0xe00
.global __mygo_rv64_vdso_blob_end
.global __mygo_rv64_vdso_data_anchor
__mygo_rv64_vdso_blob_end:
__mygo_rv64_vdso_data_anchor:

.option pop
