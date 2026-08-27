//! LoongArch64 vDSO 镜像生成。
//!
//! 构造一个最小 ELF shared object，导出以下符号供用户态 C 库使用：
//! - `__kernel_rt_sigreturn` — 信号返回 trampoline
//! - `__vdso_clock_gettime` — clock_gettime 入口
//! - `__vdso_gettimeofday` — gettimeofday 入口
//! - `__vdso_clock_getres` — clock_getres 入口
//! - `__vdso_getcpu`       — getcpu 入口

use alloc::vec;
use alloc::vec::Vec;
use core::arch::naked_asm;
use core::mem::transmute;
use core::ptr::addr_of;
use core::slice;
use core::sync::atomic::{AtomicUsize, Ordering};

static TIMER_TICK_HOOK: AtomicUsize = AtomicUsize::new(0);
static NET_POLL_HOOK: AtomicUsize = AtomicUsize::new(0);
static TTY_POLL_HOOK: AtomicUsize = AtomicUsize::new(0);

const TEXT_OFF: usize = 0x200;
const DYNSYM_OFF: usize = 0x0B0;
const DYNSTR_OFF: usize = 0x130;
const HASH_OFF: usize = 0x1B0;
const DYNAMIC_OFF: usize = 0x1D0;

pub const VDSO_DATA_PAGE_OFFSET: usize = 0x1000;
pub const VDSO_TEXT_PAGE_SIZE: usize = VDSO_DATA_PAGE_OFFSET;
pub const VDSO_TOTAL_SIZE: usize = 8192;

pub const VDSO_DATA_SEQ_OFFSET: usize = 0x00;
pub const VDSO_DATA_CLOCK_MODE_OFFSET: usize = 0x04;
pub const VDSO_DATA_HZ_OFFSET: usize = 0x08;
pub const VDSO_DATA_WALL_TIME_SEC_OFFSET: usize = 0x10;
pub const VDSO_DATA_WALL_TIME_NSEC_OFFSET: usize = 0x18;
pub const VDSO_DATA_MONOTONIC_BASE_NS_OFFSET: usize = 0x20;
pub const VDSO_DATA_CS_CYCLE_LAST_OFFSET: usize = 0x28;
pub const VDSO_DATA_CS_MULT_OFFSET: usize = 0x30;
pub const VDSO_DATA_CS_SHIFT_OFFSET: usize = 0x38;
pub const VDSO_DATA_CPU_ID_OFFSET: usize = 0x3C;
pub const VDSO_DATA_NODE_ID_OFFSET: usize = 0x40;
pub const VDSO_DATA_CLOCK_REALTIME_RES_OFFSET: usize = 0x44;

const SYS_CLOCK_GETTIME: usize = 113;
const SYS_CLOCK_GETRES: usize = 114;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_GETCPU: usize = 168;
const SYS_RT_SIGRETURN: usize = 139;

const CLOCK_REALTIME: usize = 0;
const CLOCK_MONOTONIC: usize = 1;
const CLOCK_REALTIME_COARSE: usize = 5;
const CLOCK_MONOTONIC_COARSE: usize = 6;
const CLOCK_BOOTTIME: usize = 7;

const VDSO_CLOCK_MODE_RDTIME: usize = 0;

const NSEC_PER_SEC: usize = 1_000_000_000;
const USEC_PER_SEC: usize = 1_000_000;
const NSEC_PER_USEC: usize = 1_000;
const VDSO_TIME_REALTIME: usize = 0;
const VDSO_TIME_MONOTONIC: usize = 1;
const VDSO_BLOB_SIZE: usize = VDSO_DATA_PAGE_OFFSET - TEXT_OFF;

// vDSO 的用户态入口不能由普通 Rust 函数直接生成：最终映射给用户态的是一段
// 独立 ELF shared object，代码地址也不是内核自身的 text 地址。这里用一个
// naked 函数把 LoongArch64 指令编译进 `.rodata.vdso`，再由 `build_text()`
// 拷贝为 vDSO 镜像的代码页。
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".rodata.vdso")]
#[allow(named_asm_labels)]
pub unsafe extern "C" fn __mygo_vdso_blob_start() {
    naked_asm!(
        "",
        ".balign 4",
        "",
        // Linux/LoongArch64 syscall ABI 中的系统调用号。vDSO 只在无法用户态完成时
        // fallback 到这些 syscall；rt_sigreturn 必须回到内核，不能在用户态模拟。
        ".equ SYS_CLOCK_GETTIME, {sys_clock_gettime}",
        ".equ SYS_CLOCK_GETRES, {sys_clock_getres}",
        ".equ SYS_GETTIMEOFDAY, {sys_gettimeofday}",
        ".equ SYS_GETCPU, {sys_getcpu}",
        ".equ SYS_RT_SIGRETURN, {sys_rt_sigreturn}",
        "",
        // 当前 vDSO 用户态快速路径支持的 clock id。coarse 时钟复用同一份共享数据页，
        // 精度由内核写入的 realtime_res 控制。
        ".equ CLOCK_REALTIME, {clock_realtime}",
        ".equ CLOCK_MONOTONIC, {clock_monotonic}",
        ".equ CLOCK_REALTIME_COARSE, {clock_realtime_coarse}",
        ".equ CLOCK_MONOTONIC_COARSE, {clock_monotonic_coarse}",
        ".equ CLOCK_BOOTTIME, {clock_boottime}",
        "",
        // data page 中 clock_mode == 0 表示用户态可以直接执行 rdtime.d。
        ".equ VDSO_CLOCK_MODE_RDTIME, {vdso_clock_mode_rdtime}",
        "",
        // vdso_data 布局必须与内核侧共享页完全一致。汇编只读这些偏移，不理解 Rust
        // 结构体布局；修改内核结构时必须同步这里和本文件顶部的 Rust 常量。
        ".equ VDSO_DATA_SEQ_OFFSET, {vdso_data_seq_offset}",
        ".equ VDSO_DATA_CLOCK_MODE_OFFSET, {vdso_data_clock_mode_offset}",
        ".equ VDSO_DATA_HZ_OFFSET, {vdso_data_hz_offset}",
        ".equ VDSO_DATA_WALL_TIME_SEC_OFFSET, {vdso_data_wall_time_sec_offset}",
        ".equ VDSO_DATA_WALL_TIME_NSEC_OFFSET, {vdso_data_wall_time_nsec_offset}",
        ".equ VDSO_DATA_MONOTONIC_BASE_NS_OFFSET, {vdso_data_monotonic_base_ns_offset}",
        ".equ VDSO_DATA_CS_CYCLE_LAST_OFFSET, {vdso_data_cs_cycle_last_offset}",
        ".equ VDSO_DATA_CS_MULT_OFFSET, {vdso_data_cs_mult_offset}",
        ".equ VDSO_DATA_CS_SHIFT_OFFSET, {vdso_data_cs_shift_offset}",
        ".equ VDSO_DATA_CLOCK_REALTIME_RES_OFFSET, {vdso_data_clock_realtime_res_offset}",
        "",
        ".equ NSEC_PER_SEC, {nsec_per_sec}",
        ".equ USEC_PER_SEC, {usec_per_sec}",
        ".equ NSEC_PER_USEC, {nsec_per_usec}",
        ".equ VDSO_TIME_REALTIME, {vdso_time_realtime}",
        ".equ VDSO_TIME_MONOTONIC, {vdso_time_monotonic}",
        "",
        ".global __mygo_vdso_rt_sigreturn",
        "__mygo_vdso_rt_sigreturn:",
        // sigreturn 必须由内核恢复上下文；这里仅提供 C 库期望的 trampoline。
        "    li.w $a7, SYS_RT_SIGRETURN",
        "    syscall 0",
        "",
        ".global __mygo_vdso_clock_gettime",
        "__mygo_vdso_clock_gettime:",
        // 先把支持的 clock id 归一成 realtime/monotonic 两类；其它 id 走 syscall。
        "    li.w $r6, VDSO_TIME_REALTIME",
        "    beq $a0, $zero, .Lclock_gettime_supported",
        "    li.w $r12, CLOCK_REALTIME_COARSE",
        "    beq $a0, $r12, .Lclock_gettime_supported",
        "    li.w $r6, VDSO_TIME_MONOTONIC",
        "    li.w $r12, CLOCK_MONOTONIC",
        "    beq $a0, $r12, .Lclock_gettime_supported",
        "    li.w $r12, CLOCK_MONOTONIC_COARSE",
        "    beq $a0, $r12, .Lclock_gettime_supported",
        "    li.w $r12, CLOCK_BOOTTIME",
        "    beq $a0, $r12, .Lclock_gettime_supported",
        "    b .Lclock_gettime_fallback",
        "",
        ".Lclock_gettime_supported:",
        "    beqz $a1, .Lclock_gettime_fallback",
        "    la.local $r12, __mygo_vdso_data_anchor",
        "",
        ".Lclock_gettime_retry:",
        // seqlock 协议：奇数表示内核正在写共享页，偶数才可读取快照。
        "    ld.wu $r13, $r12, VDSO_DATA_SEQ_OFFSET",
        "    andi $r14, $r13, 1",
        "    bnez $r14, .Lclock_gettime_retry",
        "    dbar 0",
        "",
        // 读取共享页快照；clock_mode/hz 无效时直接回退 syscall，避免返回坏时间。
        "    ld.wu $r14, $r12, VDSO_DATA_CLOCK_MODE_OFFSET",
        "    bnez $r14, .Lclock_gettime_fallback",
        "    ld.d $r15, $r12, VDSO_DATA_HZ_OFFSET",
        "    beqz $r15, .Lclock_gettime_fallback",
        "    ld.d $r16, $r12, VDSO_DATA_WALL_TIME_SEC_OFFSET",
        "    ld.d $r17, $r12, VDSO_DATA_WALL_TIME_NSEC_OFFSET",
        "    ld.d $r18, $r12, VDSO_DATA_MONOTONIC_BASE_NS_OFFSET",
        "    ld.d $r19, $r12, VDSO_DATA_CS_CYCLE_LAST_OFFSET",
        "    ld.d $r20, $r12, VDSO_DATA_CS_MULT_OFFSET",
        "    ld.wu $r7, $r12, VDSO_DATA_CS_SHIFT_OFFSET",
        "    rdtime.d $r8, $zero",
        "",
        // 再读 seq，若内核更新过共享页就丢弃快照重试。
        "    dbar 0",
        "    ld.wu $r9, $r12, VDSO_DATA_SEQ_OFFSET",
        "    bne $r13, $r9, .Lclock_gettime_retry",
        "",
        // delta_ns = ((rdtime - cycle_last) * mult) >> shift。
        // 这里用 64x64 -> 128 的低/高半结果拼出右移后的 64 位纳秒增量。
        "    sub.d $r8, $r8, $r19",
        "    mul.d $r9, $r8, $r20",
        "    mulh.du $r10, $r8, $r20",
        "    srl.d $r9, $r9, $r7",
        "    li.w $r11, 64",
        "    sub.d $r11, $r11, $r7",
        "    sll.d $r10, $r10, $r11",
        "    or $r9, $r9, $r10",
        "",
        "    bnez $r6, .Lclock_gettime_monotonic",
        "",
        // CLOCK_REALTIME：内核给出 epoch 秒和纳秒余数，用户态只补上 delta。
        "    add.d $r17, $r17, $r9",
        "    li.w $r10, NSEC_PER_SEC",
        "    div.du $r18, $r17, $r10",
        "    mod.du $r17, $r17, $r10",
        "    add.d $r16, $r16, $r18",
        "    b .Lclock_gettime_store",
        "",
        ".Lclock_gettime_monotonic:",
        // CLOCK_MONOTONIC/BOOTTIME：单调基准是纳秒总量，最后拆成 timespec。
        "    add.d $r18, $r18, $r9",
        "    li.w $r10, NSEC_PER_SEC",
        "    div.du $r16, $r18, $r10",
        "    mod.du $r17, $r18, $r10",
        "",
        ".Lclock_gettime_store:",
        "    st.d $r16, $a1, 0",
        "    st.d $r17, $a1, 8",
        "    addi.d $a0, $zero, 0",
        "    ret",
        "",
        ".Lclock_gettime_fallback:",
        "    li.w $a7, SYS_CLOCK_GETTIME",
        "    syscall 0",
        "    ret",
        "",
        ".global __mygo_vdso_gettimeofday",
        "__mygo_vdso_gettimeofday:",
        "    la.local $r12, __mygo_vdso_data_anchor",
        "",
        ".Lgettimeofday_retry:",
        // gettimeofday 只需要 realtime；仍使用同一套 seqlock + rdtime 快照。
        "    ld.wu $r13, $r12, VDSO_DATA_SEQ_OFFSET",
        "    andi $r14, $r13, 1",
        "    bnez $r14, .Lgettimeofday_retry",
        "    dbar 0",
        "",
        "    ld.wu $r14, $r12, VDSO_DATA_CLOCK_MODE_OFFSET",
        "    bnez $r14, .Lgettimeofday_fallback",
        "    ld.d $r15, $r12, VDSO_DATA_HZ_OFFSET",
        "    beqz $r15, .Lgettimeofday_fallback",
        "    ld.d $r16, $r12, VDSO_DATA_WALL_TIME_SEC_OFFSET",
        "    ld.d $r17, $r12, VDSO_DATA_WALL_TIME_NSEC_OFFSET",
        "    ld.d $r19, $r12, VDSO_DATA_CS_CYCLE_LAST_OFFSET",
        "    ld.d $r20, $r12, VDSO_DATA_CS_MULT_OFFSET",
        "    ld.wu $r7, $r12, VDSO_DATA_CS_SHIFT_OFFSET",
        "    rdtime.d $r8, $zero",
        "",
        "    dbar 0",
        "    ld.wu $r9, $r12, VDSO_DATA_SEQ_OFFSET",
        "    bne $r13, $r9, .Lgettimeofday_retry",
        "",
        "    sub.d $r8, $r8, $r19",
        "    mul.d $r9, $r8, $r20",
        "    mulh.du $r10, $r8, $r20",
        "    srl.d $r9, $r9, $r7",
        "    li.w $r11, 64",
        "    sub.d $r11, $r11, $r7",
        "    sll.d $r10, $r10, $r11",
        "    or $r9, $r9, $r10",
        "",
        "    add.d $r17, $r17, $r9",
        "    li.w $r10, NSEC_PER_SEC",
        "    div.du $r18, $r17, $r10",
        "    mod.du $r17, $r17, $r10",
        "    add.d $r16, $r16, $r18",
        "    li.w $r10, NSEC_PER_USEC",
        "    div.du $r17, $r17, $r10",
        "",
        "    beqz $a0, .Lgettimeofday_success",
        "    st.d $r16, $a0, 0",
        "    st.d $r17, $a0, 8",
        "",
        ".Lgettimeofday_success:",
        "    addi.d $a0, $zero, 0",
        "    ret",
        "",
        ".Lgettimeofday_fallback:",
        "    li.w $a7, SYS_GETTIMEOFDAY",
        "    syscall 0",
        "    ret",
        "",
        ".global __mygo_vdso_clock_getres",
        "__mygo_vdso_clock_getres:",
        // 支持的 clock id 直接读取共享页中的精度；未知 clock id 保持 syscall 语义。
        "    beq $a0, $zero, .Lclock_getres_supported",
        "    li.w $r12, CLOCK_REALTIME_COARSE",
        "    beq $a0, $r12, .Lclock_getres_supported",
        "    li.w $r12, CLOCK_MONOTONIC",
        "    beq $a0, $r12, .Lclock_getres_supported",
        "    li.w $r12, CLOCK_MONOTONIC_COARSE",
        "    beq $a0, $r12, .Lclock_getres_supported",
        "    li.w $r12, CLOCK_BOOTTIME",
        "    beq $a0, $r12, .Lclock_getres_supported",
        "    b .Lclock_getres_fallback",
        "",
        ".Lclock_getres_supported:",
        "    la.local $r12, __mygo_vdso_data_anchor",
        "",
        ".Lclock_getres_retry:",
        "    ld.wu $r13, $r12, VDSO_DATA_SEQ_OFFSET",
        "    andi $r14, $r13, 1",
        "    bnez $r14, .Lclock_getres_retry",
        "    dbar 0",
        "",
        "    ld.wu $r14, $r12, VDSO_DATA_CLOCK_MODE_OFFSET",
        "    bnez $r14, .Lclock_getres_fallback",
        "    ld.d $r15, $r12, VDSO_DATA_HZ_OFFSET",
        "    beqz $r15, .Lclock_getres_fallback",
        "    ld.wu $r16, $r12, VDSO_DATA_CLOCK_REALTIME_RES_OFFSET",
        "",
        "    dbar 0",
        "    ld.wu $r17, $r12, VDSO_DATA_SEQ_OFFSET",
        "    bne $r13, $r17, .Lclock_getres_retry",
        "",
        "    beqz $a1, .Lclock_getres_success",
        "    st.d $zero, $a1, 0",
        "    st.d $r16, $a1, 8",
        "",
        ".Lclock_getres_success:",
        "    addi.d $a0, $zero, 0",
        "    ret",
        "",
        ".Lclock_getres_fallback:",
        "    li.w $a7, SYS_CLOCK_GETRES",
        "    syscall 0",
        "    ret",
        "",
        ".global __mygo_vdso_getcpu",
        "__mygo_vdso_getcpu:",
        // getcpu 目前保持 syscall fallback；共享页已有 cpu/node 字段，后续可以
        // 按同一 seqlock 模式改成纯用户态读取。
        "    li.w $a7, SYS_GETCPU",
        "    syscall 0",
        "    ret",
        "",
        ".global __mygo_vdso_text_end",
        "__mygo_vdso_text_end:",
        "",
        // 代码 blob 固定填充到 0xe00；Rust 侧从 TEXT_OFF(0x200) 放入后刚好占满
        // 第一页到 0x1000。data_anchor 是用户态代码用 la.local 定位共享数据页的锚点。
        ".org {vdso_blob_size}",
        ".global __mygo_vdso_blob_end",
        ".global __mygo_vdso_data_anchor",
        "__mygo_vdso_blob_end:",
        "__mygo_vdso_data_anchor:",
        sys_clock_gettime = const SYS_CLOCK_GETTIME,
        sys_clock_getres = const SYS_CLOCK_GETRES,
        sys_gettimeofday = const SYS_GETTIMEOFDAY,
        sys_getcpu = const SYS_GETCPU,
        sys_rt_sigreturn = const SYS_RT_SIGRETURN,
        clock_realtime = const CLOCK_REALTIME,
        clock_monotonic = const CLOCK_MONOTONIC,
        clock_realtime_coarse = const CLOCK_REALTIME_COARSE,
        clock_monotonic_coarse = const CLOCK_MONOTONIC_COARSE,
        clock_boottime = const CLOCK_BOOTTIME,
        vdso_clock_mode_rdtime = const VDSO_CLOCK_MODE_RDTIME,
        vdso_data_seq_offset = const VDSO_DATA_SEQ_OFFSET,
        vdso_data_clock_mode_offset = const VDSO_DATA_CLOCK_MODE_OFFSET,
        vdso_data_hz_offset = const VDSO_DATA_HZ_OFFSET,
        vdso_data_wall_time_sec_offset = const VDSO_DATA_WALL_TIME_SEC_OFFSET,
        vdso_data_wall_time_nsec_offset = const VDSO_DATA_WALL_TIME_NSEC_OFFSET,
        vdso_data_monotonic_base_ns_offset = const VDSO_DATA_MONOTONIC_BASE_NS_OFFSET,
        vdso_data_cs_cycle_last_offset = const VDSO_DATA_CS_CYCLE_LAST_OFFSET,
        vdso_data_cs_mult_offset = const VDSO_DATA_CS_MULT_OFFSET,
        vdso_data_cs_shift_offset = const VDSO_DATA_CS_SHIFT_OFFSET,
        vdso_data_clock_realtime_res_offset = const VDSO_DATA_CLOCK_REALTIME_RES_OFFSET,
        nsec_per_sec = const NSEC_PER_SEC,
        usec_per_sec = const USEC_PER_SEC,
        nsec_per_usec = const NSEC_PER_USEC,
        vdso_time_realtime = const VDSO_TIME_REALTIME,
        vdso_time_monotonic = const VDSO_TIME_MONOTONIC,
        vdso_blob_size = const VDSO_BLOB_SIZE,
    )
}

unsafe extern "C" {
    static __mygo_vdso_blob_end: u8;
    static __mygo_vdso_text_end: u8;
    static __mygo_vdso_rt_sigreturn: u8;
    static __mygo_vdso_clock_gettime: u8;
    static __mygo_vdso_gettimeofday: u8;
    static __mygo_vdso_clock_getres: u8;
    static __mygo_vdso_getcpu: u8;
}

pub fn sigreturn_entry_offset() -> usize {
    symbol_offset(addr_of!(__mygo_vdso_rt_sigreturn))
}

pub fn register_timer_tick_hook(hook: fn(u64)) {
    TIMER_TICK_HOOK.store(hook as usize, Ordering::Release);
}

pub fn run_timer_tick_hook(now_ns: u64) {
    let raw = TIMER_TICK_HOOK.load(Ordering::Acquire);
    if raw != 0 {
        let hook: fn(u64) = unsafe { transmute(raw) };
        hook(now_ns);
    }
}

/// 注册网络协议栈 poll 回调（与 vDSO tick hook 并列的另一条独立路径）。
pub fn register_net_poll_hook(hook: fn(u64)) {
    NET_POLL_HOOK.store(hook as usize, Ordering::Release);
}

/// 执行网络协议栈 poll 回调。
///
/// 陷阱入口 timer 中断处理路径上调 `run_timer_tick_hook` 之后调本函数；
/// 由本函数的 hook 内部调 `net::stack().poll(now)` 推进协议栈一帧。
/// 如果没注册 hook，本函数是 no-op。
pub fn run_net_poll_hook(now_ns: u64) {
    let raw = NET_POLL_HOOK.load(Ordering::Acquire);
    if raw != 0 {
        let hook: fn(u64) = unsafe { transmute(raw) };
        hook(now_ns);
    }
}

/// 注册 TTY 输入泵回调。
///
/// 该回调与 vDSO / net poll hook 平级，由 timer tick 驱动，用于在没有
/// 用户进程正在 read 终端时仍能处理 Ctrl-C 等控制字符。
pub fn register_tty_poll_hook(hook: fn(u64)) {
    TTY_POLL_HOOK.store(hook as usize, Ordering::Release);
}

pub fn run_tty_poll_hook(now_ns: u64) {
    let raw = TTY_POLL_HOOK.load(Ordering::Acquire);
    if raw != 0 {
        let hook: fn(u64) = unsafe { transmute(raw) };
        hook(now_ns);
    }
}

pub fn vdso_image() -> Vec<u8> {
    let mut img = vec![0u8; VDSO_TOTAL_SIZE];
    build_elf_header(&mut img);
    build_phdr(&mut img);
    build_dynsym(&mut img);
    build_dynstr(&mut img);
    build_hash(&mut img);
    build_dynamic(&mut img);
    build_text(&mut img);
    img
}

// ── ELF Header ──────────────────────────────────────────────────────────────

fn build_elf_header(b: &mut [u8]) {
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // ELFDATA2LSB
    b[6] = 1; // EV_CURRENT
    w16(b, 16, 3); // ET_DYN
    w16(b, 18, 258); // EM_LOONGARCH
    w32(b, 20, 1); // e_version
    w64(b, 24, sigreturn_entry_offset() as u64); // e_entry
    w64(b, 32, 64); // e_phoff
    w16(b, 52, 64); // e_ehsize
    w16(b, 54, 56); // e_phentsize
    w16(b, 56, 2); // e_phnum
}

// ── Program Headers ─────────────────────────────────────────────────────────

fn build_phdr(b: &mut [u8]) {
    // PH[0]: PT_LOAD covering both pages (runtime mapping is split by kernel)
    let p = 64;
    w32(b, p, 1); // PT_LOAD
    w32(b, p + 4, 5); // PF_R | PF_X
    w64(b, p + 8, 0); // p_offset
    w64(b, p + 16, 0); // p_vaddr
    w64(b, p + 24, 0); // p_paddr
    w64(b, p + 32, VDSO_TOTAL_SIZE as u64); // p_filesz
    w64(b, p + 40, VDSO_TOTAL_SIZE as u64); // p_memsz
    w64(b, p + 48, 4096); // p_align

    // PH[1]: PT_DYNAMIC
    let p = 64 + 56;
    w32(b, p, 2); // PT_DYNAMIC
    w32(b, p + 4, 4); // PF_R
    w64(b, p + 8, DYNAMIC_OFF as u64);
    w64(b, p + 16, DYNAMIC_OFF as u64);
    w64(b, p + 24, DYNAMIC_OFF as u64);
    let dyn_sz: u64 = 6 * 16;
    w64(b, p + 32, dyn_sz);
    w64(b, p + 40, dyn_sz);
    w64(b, p + 48, 8);
}

// ── .dynsym ─────────────────────────────────────────────────────────────────

fn build_dynsym(b: &mut [u8]) {
    let names = dynstr_offsets();
    let layouts = symbol_layouts();

    for (i, (value, size)) in layouts.iter().enumerate() {
        let off = DYNSYM_OFF + (i + 1) * 24;
        w32(b, off, names[i] as u32); // st_name
        b[off + 4] = (1 << 4) | 2; // STB_GLOBAL | STT_FUNC
        b[off + 5] = 0; // STV_DEFAULT
        w16(b, off + 6, 1); // st_shndx (defined)
        w64(b, off + 8, *value as u64); // st_value
        w64(b, off + 16, *size as u64); // st_size
    }
}

// ── .dynstr ─────────────────────────────────────────────────────────────────

const DYNSTR_NAMES: [&[u8]; 5] = [
    b"__kernel_rt_sigreturn",
    b"__vdso_clock_gettime",
    b"__vdso_gettimeofday",
    b"__vdso_clock_getres",
    b"__vdso_getcpu",
];

fn dynstr_offsets() -> [usize; 5] {
    let mut offsets = [0usize; 5];
    let mut pos = 1; // skip leading \0
    for (i, name) in DYNSTR_NAMES.iter().enumerate() {
        offsets[i] = pos;
        pos += name.len() + 1;
    }
    offsets
}

fn dynstr_total_len() -> usize {
    let mut len = 1; // leading \0
    for name in &DYNSTR_NAMES {
        len += name.len() + 1;
    }
    len
}

fn build_dynstr(b: &mut [u8]) {
    b[DYNSTR_OFF] = 0;
    let mut pos = DYNSTR_OFF + 1;
    for name in &DYNSTR_NAMES {
        b[pos..pos + name.len()].copy_from_slice(name);
        b[pos + name.len()] = 0;
        pos += name.len() + 1;
    }
}

// ── .hash (ELF hash table) ──────────────────────────────────────────────────

fn build_hash(b: &mut [u8]) {
    let nbucket: u32 = 3;
    let nchain: u32 = 6; // STN_UNDEF + 5 symbols
    w32(b, HASH_OFF, nbucket);
    w32(b, HASH_OFF + 4, nchain);

    let bucket_off = HASH_OFF + 8;
    let chain_off = bucket_off + (nbucket as usize) * 4;

    for i in 0..(nbucket as usize) {
        w32(b, bucket_off + i * 4, 0);
    }
    for i in 0..(nchain as usize) {
        w32(b, chain_off + i * 4, 0);
    }

    for sym_idx in 1u32..=5 {
        let name = DYNSTR_NAMES[(sym_idx - 1) as usize];
        let hash = elf_hash(name);
        let bucket = hash % nbucket;

        let prev = r32(b, bucket_off + (bucket as usize) * 4);
        if prev == 0 {
            w32(b, bucket_off + (bucket as usize) * 4, sym_idx);
        } else {
            let mut cur = prev;
            loop {
                let next = r32(b, chain_off + (cur as usize) * 4);
                if next == 0 {
                    w32(b, chain_off + (cur as usize) * 4, sym_idx);
                    break;
                }
                cur = next;
            }
        }
    }
}

fn elf_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xF000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

// ── .dynamic ────────────────────────────────────────────────────────────────

fn build_dynamic(b: &mut [u8]) {
    let mut off = DYNAMIC_OFF;
    w64(b, off, 6); // DT_SYMTAB
    w64(b, off + 8, DYNSYM_OFF as u64);
    off += 16;
    w64(b, off, 5); // DT_STRTAB
    w64(b, off + 8, DYNSTR_OFF as u64);
    off += 16;
    w64(b, off, 10); // DT_STRSZ
    w64(b, off + 8, dynstr_total_len() as u64);
    off += 16;
    w64(b, off, 4); // DT_HASH
    w64(b, off + 8, HASH_OFF as u64);
    off += 16;
    w64(b, off, 11); // DT_SYMENT
    w64(b, off + 8, 24);
    off += 16;
    w64(b, off, 0); // DT_NULL
    w64(b, off + 8, 0);
}

// ── .text / code blob ───────────────────────────────────────────────────────

fn build_text(b: &mut [u8]) {
    let blob = blob_bytes();
    assert_eq!(TEXT_OFF + blob.len(), VDSO_DATA_PAGE_OFFSET);
    b[TEXT_OFF..TEXT_OFF + blob.len()].copy_from_slice(blob);
}

fn blob_bytes() -> &'static [u8] {
    let start = __mygo_vdso_blob_start as *const () as usize;
    let end = addr_of!(__mygo_vdso_blob_end) as usize;
    unsafe { slice::from_raw_parts(start as *const u8, end - start) }
}

fn symbol_layouts() -> [(usize, usize); 5] {
    let offsets = [
        symbol_offset(addr_of!(__mygo_vdso_rt_sigreturn)),
        symbol_offset(addr_of!(__mygo_vdso_clock_gettime)),
        symbol_offset(addr_of!(__mygo_vdso_gettimeofday)),
        symbol_offset(addr_of!(__mygo_vdso_clock_getres)),
        symbol_offset(addr_of!(__mygo_vdso_getcpu)),
    ];
    let mut out = [(0usize, 0usize); 5];
    for i in 0..offsets.len() {
        let end = if i + 1 < offsets.len() {
            offsets[i + 1]
        } else {
            symbol_offset(addr_of!(__mygo_vdso_text_end))
        };
        out[i] = (offsets[i], end - offsets[i]);
    }
    out
}

fn symbol_offset(sym: *const u8) -> usize {
    let start = __mygo_vdso_blob_start as *const () as usize;
    TEXT_OFF + (sym as usize - start)
}

// ── byte writers ────────────────────────────────────────────────────────────

fn w16(b: &mut [u8], off: usize, val: u16) {
    b[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn w32(b: &mut [u8], off: usize, val: u32) {
    b[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn w64(b: &mut [u8], off: usize, val: u64) {
    b[off..off + 8].copy_from_slice(&val.to_le_bytes());
}
