/*
 * RISC-V64 vDSO 用户态代码段
 *
 * 映射到每个用户进程地址空间，通过 ecall 陷入内核完成系统调用。
 *
 * 设计要点（区别于 LoongArch）：
 *   - 每个入口点 .balign 4，确保符号地址 4 字节对齐
 *   - rt_sigreturn 后跟 unimp 安全网（ecall 不返回，防御性编程）
 *   - 不含浮点/RVC 指令，e_flags = 0
 *   - 不需要 data_anchor（RISC-V auipc 可直接计算 PC 相对偏移）
 *
 * 布局：blob_start 位于 .rodata.vdso 节，
 *       .org 0xe00 使 blob 总大小 = 0xe00，
 *       加上 ELF header 占用的 TEXT_OFF=0x200 正好填满 4KiB text 页。
 */

.section .rodata.vdso, "a"
.balign 4

.global __mygo_rv64_vdso_blob_start
__mygo_rv64_vdso_blob_start:

/* ── rt_sigreturn ── ecall 不返回，后接 unimp 作为陷阱 */
.balign 4
.global __mygo_rv64_vdso_rt_sigreturn
__mygo_rv64_vdso_rt_sigreturn:
    li      a7, 139             /* __NR_rt_sigreturn */
    ecall
    unimp                       /* 不应到达 */

/* ── clock_gettime ── */
.balign 4
.global __mygo_rv64_vdso_clock_gettime
__mygo_rv64_vdso_clock_gettime:
    li      a7, 113             /* __NR_clock_gettime */
    ecall
    ret

/* ── gettimeofday ── */
.balign 4
.global __mygo_rv64_vdso_gettimeofday
__mygo_rv64_vdso_gettimeofday:
    li      a7, 169             /* __NR_gettimeofday */
    ecall
    ret

/* ── clock_getres ── */
.balign 4
.global __mygo_rv64_vdso_clock_getres
__mygo_rv64_vdso_clock_getres:
    li      a7, 114             /* __NR_clock_getres */
    ecall
    ret

/* ── getcpu ── */
.balign 4
.global __mygo_rv64_vdso_getcpu
__mygo_rv64_vdso_getcpu:
    li      a7, 168             /* __NR_getcpu */
    ecall
    ret

.global __mygo_rv64_vdso_text_end
__mygo_rv64_vdso_text_end:

.org 0xe00
.global __mygo_rv64_vdso_blob_end
__mygo_rv64_vdso_blob_end:
