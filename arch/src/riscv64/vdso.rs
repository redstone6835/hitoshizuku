//! RISC-V64 vDSO 镜像生成 + timer tick / net poll hook。
//!
//! 构造一个最小 ELF shared object，导出以下符号供用户态 C 库使用：
//! - `__kernel_rt_sigreturn` — 信号返回 trampoline
//! - `__vdso_clock_gettime` — clock_gettime 入口
//! - `__vdso_gettimeofday` — gettimeofday 入口
//! - `__vdso_clock_getres` — clock_getres 入口
//! - `__vdso_getcpu`       — getcpu 入口
//!
//! 代码 trampoline 定义在 `vdso_text.S`，通过 `global_asm!` 引入。

use alloc::vec::Vec;
use core::arch::global_asm;
use core::mem::size_of;
use core::ptr::addr_of;
use core::slice;

// ── 常量 ──────────────────────────────────────────────────────────────────────

// ELF 元数据必须完整落在代码前。原先 0x200 的代码起点会覆盖 PT_DYNAMIC
// 后半段，同时 .dynstr 也会覆盖最后一个 Elf64_Sym，导致 libc 无法解析 vDSO。
const TEXT_OFF: usize = 0x300;
const DYNSYM_OFF: usize = 0x0B0;
const DYNSTR_OFF: usize = 0x140;
const HASH_OFF: usize = 0x1C0;
const VERSYM_OFF: usize = 0x1EC;
const VERDEF_OFF: usize = 0x1F8;
const DYNAMIC_OFF: usize = 0x230;

const VDSO_SONAME: &[u8] = b"linux-vdso.so.1";
const VDSO_VERSION: &[u8] = b"LINUX_4.15";
const DYNAMIC_ENTRY_COUNT: usize = 10;
const VERDEF_SIZE: usize = 28;

const _: () = {
    assert!(DYNSYM_OFF + 6 * 24 <= DYNSTR_OFF);
    assert!(HASH_OFF + 44 <= VERSYM_OFF);
    assert!(VERSYM_OFF + 6 * 2 <= VERDEF_OFF);
    assert!(VERDEF_OFF + 2 * VERDEF_SIZE <= DYNAMIC_OFF);
    assert!(DYNAMIC_OFF + DYNAMIC_ENTRY_COUNT * 16 <= TEXT_OFF);
    assert!(TEXT_OFF < VDSO_DATA_PAGE_OFFSET);
};

/// vDSO 数据页偏移（第 2 页）。
pub const VDSO_DATA_PAGE_OFFSET: usize = 0x1000;
/// vDSO 第一页长度。
pub const VDSO_TEXT_PAGE_SIZE: usize = VDSO_DATA_PAGE_OFFSET;
/// vDSO 总映射长度（text + data，各 4KiB）。
pub const VDSO_TOTAL_SIZE: usize = 8192;

// 与 kernel/src/vdso.rs::VdsoData 的 repr(C) 布局保持一致。用户态汇编只读共享页。
pub const VDSO_DATA_SEQ_OFFSET: usize = 0x00;
pub const VDSO_DATA_CLOCK_MODE_OFFSET: usize = 0x04;
pub const VDSO_DATA_HZ_OFFSET: usize = 0x08;
pub const VDSO_DATA_WALL_TIME_SEC_OFFSET: usize = 0x10;
pub const VDSO_DATA_WALL_TIME_NSEC_OFFSET: usize = 0x18;
pub const VDSO_DATA_MONOTONIC_BASE_NS_OFFSET: usize = 0x20;
pub const VDSO_DATA_CS_CYCLE_LAST_OFFSET: usize = 0x28;
pub const VDSO_DATA_CS_MULT_OFFSET: usize = 0x30;
pub const VDSO_DATA_CS_SHIFT_OFFSET: usize = 0x38;
pub const VDSO_DATA_CPU_ID_OFFSET: usize = 0x3c;
pub const VDSO_DATA_NODE_ID_OFFSET: usize = 0x40;
pub const VDSO_DATA_CLOCK_REALTIME_RES_OFFSET: usize = 0x44;

global_asm!(
    include_str!("vdso_text.S"),
    vdso_data_seq_offset = const VDSO_DATA_SEQ_OFFSET,
    vdso_data_clock_mode_offset = const VDSO_DATA_CLOCK_MODE_OFFSET,
    vdso_data_hz_offset = const VDSO_DATA_HZ_OFFSET,
    vdso_data_wall_time_sec_offset = const VDSO_DATA_WALL_TIME_SEC_OFFSET,
    vdso_data_wall_time_nsec_offset = const VDSO_DATA_WALL_TIME_NSEC_OFFSET,
    vdso_data_monotonic_base_ns_offset = const VDSO_DATA_MONOTONIC_BASE_NS_OFFSET,
    vdso_data_cs_cycle_last_offset = const VDSO_DATA_CS_CYCLE_LAST_OFFSET,
    vdso_data_cs_mult_offset = const VDSO_DATA_CS_MULT_OFFSET,
    vdso_data_cs_shift_offset = const VDSO_DATA_CS_SHIFT_OFFSET,
    vdso_data_cpu_id_offset = const VDSO_DATA_CPU_ID_OFFSET,
    vdso_data_node_id_offset = const VDSO_DATA_NODE_ID_OFFSET,
    vdso_data_clock_realtime_res_offset = const VDSO_DATA_CLOCK_REALTIME_RES_OFFSET,
);

// 编译期验证：fn 指针与 usize 同宽
const _: () = assert!(size_of::<fn(u64)>() == size_of::<usize>());

// ── 汇编符号引用 ─────────────────────────────────────────────────────────────

unsafe extern "C" {
    static __mygo_rv64_vdso_blob_start: u8;
    static __mygo_rv64_vdso_blob_end: u8;
    static __mygo_rv64_vdso_text_end: u8;
    static __mygo_rv64_vdso_rt_sigreturn: u8;
    static __mygo_rv64_vdso_clock_gettime: u8;
    static __mygo_rv64_vdso_gettimeofday: u8;
    static __mygo_rv64_vdso_clock_getres: u8;
    static __mygo_rv64_vdso_getcpu: u8;
}

// ── Hook 注册与触发 ──────────────────────────────────────────────────────────

use core::sync::atomic::{AtomicPtr, Ordering};

type TickHookFn = fn(u64);

static TIMER_TICK_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static NET_POLL_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static TTY_POLL_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

#[inline]
fn register_hook(slot: &AtomicPtr<()>, hook: TickHookFn) {
    slot.store(hook as *mut (), Ordering::Release);
}

#[inline]
fn run_hook(slot: &AtomicPtr<()>, now_ns: u64) {
    let raw = slot.load(Ordering::Acquire);
    if !raw.is_null() {
        let hook: TickHookFn = unsafe { core::mem::transmute::<*mut (), TickHookFn>(raw) };
        hook(now_ns);
    }
}

pub fn sigreturn_entry_offset() -> usize {
    symbol_offset(addr_of!(__mygo_rv64_vdso_rt_sigreturn))
}

pub fn register_timer_tick_hook(hook: fn(u64)) {
    register_hook(&TIMER_TICK_HOOK, hook);
}

pub fn run_timer_tick_hook(now_ns: u64) {
    run_hook(&TIMER_TICK_HOOK, now_ns);
}

pub fn register_net_poll_hook(hook: fn(u64)) {
    register_hook(&NET_POLL_HOOK, hook);
}

pub fn run_net_poll_hook(now_ns: u64) {
    run_hook(&NET_POLL_HOOK, now_ns);
}

pub fn register_tty_poll_hook(hook: fn(u64)) {
    register_hook(&TTY_POLL_HOOK, hook);
}

pub fn run_tty_poll_hook(now_ns: u64) {
    run_hook(&TTY_POLL_HOOK, now_ns);
}

// ── vDSO ELF 镜像生成 ────────────────────────────────────────────────────────

pub fn vdso_image() -> Vec<u8> {
    assert_eq!(DYNSTR_NAMES.len() + 1, 6);
    assert!(DYNSTR_OFF + dynstr_total_len() <= HASH_OFF);
    let mut img = alloc::vec![0u8; VDSO_TOTAL_SIZE];
    build_elf_header(&mut img);
    build_phdr(&mut img);
    build_dynsym(&mut img);
    build_dynstr(&mut img);
    build_hash(&mut img);
    build_versions(&mut img);
    build_dynamic(&mut img);
    build_text(&mut img);
    img
}

fn build_elf_header(b: &mut [u8]) {
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // ELFDATA2LSB
    b[6] = 1; // EV_CURRENT
    w16(b, 16, 3); // ET_DYN
    w16(b, 18, 243); // EM_RISCV
    w32(b, 20, 1); // e_version
    w64(b, 24, sigreturn_entry_offset() as u64); // e_entry
    w64(b, 32, 64); // e_phoff
    w16(b, 52, 64); // e_ehsize
    w16(b, 54, 56); // e_phentsize
    w16(b, 56, 2); // e_phnum
}

fn build_phdr(b: &mut [u8]) {
    // PH[0]: PT_LOAD covering text + data
    let p = 64;
    w32(b, p, 1); // PT_LOAD
    w32(b, p + 4, 5); // PF_R | PF_X
    w64(b, p + 8, 0);
    w64(b, p + 16, 0);
    w64(b, p + 24, 0);
    w64(b, p + 32, VDSO_TOTAL_SIZE as u64);
    w64(b, p + 40, VDSO_TOTAL_SIZE as u64);
    w64(b, p + 48, 4096);

    // PH[1]: PT_DYNAMIC
    let p = 64 + 56;
    w32(b, p, 2); // PT_DYNAMIC
    w32(b, p + 4, 4); // PF_R
    w64(b, p + 8, DYNAMIC_OFF as u64);
    w64(b, p + 16, DYNAMIC_OFF as u64);
    w64(b, p + 24, DYNAMIC_OFF as u64);
    let dyn_sz: u64 = (DYNAMIC_ENTRY_COUNT * 16) as u64;
    w64(b, p + 32, dyn_sz);
    w64(b, p + 40, dyn_sz);
    w64(b, p + 48, 8);
}

const DYNSTR_NAMES: [&[u8]; 5] = [
    b"__kernel_rt_sigreturn",
    b"__vdso_clock_gettime",
    b"__vdso_gettimeofday",
    b"__vdso_clock_getres",
    b"__vdso_getcpu",
];

fn dynstr_offsets() -> [usize; 5] {
    let mut offsets = [0usize; 5];
    let mut pos = 1;
    for (i, name) in DYNSTR_NAMES.iter().enumerate() {
        offsets[i] = pos;
        pos += name.len() + 1;
    }
    offsets
}

fn dynstr_total_len() -> usize {
    let mut len = 1;
    for name in &DYNSTR_NAMES {
        len += name.len() + 1;
    }
    len + VDSO_SONAME.len() + 1 + VDSO_VERSION.len() + 1
}

fn dynstr_soname_offset() -> usize {
    1 + DYNSTR_NAMES
        .iter()
        .map(|name| name.len() + 1)
        .sum::<usize>()
}

fn dynstr_version_offset() -> usize {
    dynstr_soname_offset() + VDSO_SONAME.len() + 1
}

fn build_dynsym(b: &mut [u8]) {
    let names = dynstr_offsets();
    let layouts = symbol_layouts();
    for (i, (value, size)) in layouts.iter().enumerate() {
        let off = DYNSYM_OFF + (i + 1) * 24;
        w32(b, off, names[i] as u32);
        b[off + 4] = (1 << 4) | 2; // STB_GLOBAL | STT_FUNC
        b[off + 5] = 0;
        w16(b, off + 6, 1);
        w64(b, off + 8, *value as u64);
        w64(b, off + 16, *size as u64);
    }
}

fn build_dynstr(b: &mut [u8]) {
    b[DYNSTR_OFF] = 0;
    let mut pos = DYNSTR_OFF + 1;
    for name in &DYNSTR_NAMES {
        b[pos..pos + name.len()].copy_from_slice(name);
        b[pos + name.len()] = 0;
        pos += name.len() + 1;
    }
    b[pos..pos + VDSO_SONAME.len()].copy_from_slice(VDSO_SONAME);
    b[pos + VDSO_SONAME.len()] = 0;
    pos += VDSO_SONAME.len() + 1;
    b[pos..pos + VDSO_VERSION.len()].copy_from_slice(VDSO_VERSION);
    b[pos + VDSO_VERSION.len()] = 0;
}

fn build_hash(b: &mut [u8]) {
    let nbucket: u32 = 3;
    let nchain: u32 = 6;
    w32(b, HASH_OFF, nbucket);
    w32(b, HASH_OFF + 4, nchain);
    let bucket_off = HASH_OFF + 8;
    let chain_off = bucket_off + (nbucket as usize) * 4;
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

fn build_versions(b: &mut [u8]) {
    // Elf64_Half .gnu.version[6]：空符号为 local，其余导出均属于
    // RISC-V Linux ABI 使用的 LINUX_4.15 版本（索引 2）。
    w16(b, VERSYM_OFF, 0);
    for sym_idx in 1..=5 {
        w16(b, VERSYM_OFF + sym_idx * 2, 2);
    }

    // Elf64_Verdef + Elf64_Verdaux。索引 1 是 soname 的 BASE 定义；索引 2
    // 是 glibc/musl 查找 __vdso_* 时使用的 LINUX_4.15 定义。
    build_verdef(
        b,
        VERDEF_OFF,
        1,
        1, // VER_FLG_BASE
        VDSO_SONAME,
        dynstr_soname_offset(),
        VERDEF_SIZE,
    );
    build_verdef(
        b,
        VERDEF_OFF + VERDEF_SIZE,
        2,
        0,
        VDSO_VERSION,
        dynstr_version_offset(),
        0,
    );
}

fn build_verdef(
    b: &mut [u8],
    off: usize,
    index: u16,
    flags: u16,
    name: &[u8],
    name_offset: usize,
    next: usize,
) {
    w16(b, off, 1); // VER_DEF_CURRENT
    w16(b, off + 2, flags);
    w16(b, off + 4, index);
    w16(b, off + 6, 1); // vd_cnt
    w32(b, off + 8, elf_hash(name));
    w32(b, off + 12, 20); // vd_aux
    w32(b, off + 16, next as u32);
    w32(b, off + 20, name_offset as u32);
    w32(b, off + 24, 0); // vda_next
}

fn build_dynamic(b: &mut [u8]) {
    let mut off = DYNAMIC_OFF;
    for (tag, value) in [
        (6, DYNSYM_OFF as u64),              // DT_SYMTAB
        (5, DYNSTR_OFF as u64),              // DT_STRTAB
        (10, dynstr_total_len() as u64),     // DT_STRSZ
        (4, HASH_OFF as u64),                // DT_HASH
        (11, 24),                            // DT_SYMENT
        (0x6fff_fff0, VERSYM_OFF as u64),    // DT_VERSYM
        (0x6fff_fffc, VERDEF_OFF as u64),    // DT_VERDEF
        (0x6fff_fffd, 2),                    // DT_VERDEFNUM
        (14, dynstr_soname_offset() as u64), // DT_SONAME
        (0, 0),                              // DT_NULL
    ] {
        w64(b, off, tag);
        w64(b, off + 8, value);
        off += 16;
    }
}

fn build_text(b: &mut [u8]) {
    let blob = blob_bytes();
    assert_eq!(TEXT_OFF + blob.len(), VDSO_DATA_PAGE_OFFSET);
    b[TEXT_OFF..TEXT_OFF + blob.len()].copy_from_slice(blob);
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

fn blob_bytes() -> &'static [u8] {
    let start = addr_of!(__mygo_rv64_vdso_blob_start) as usize;
    let end = addr_of!(__mygo_rv64_vdso_blob_end) as usize;
    unsafe { slice::from_raw_parts(start as *const u8, end - start) }
}

fn symbol_layouts() -> [(usize, usize); 5] {
    let offsets = [
        symbol_offset(addr_of!(__mygo_rv64_vdso_rt_sigreturn)),
        symbol_offset(addr_of!(__mygo_rv64_vdso_clock_gettime)),
        symbol_offset(addr_of!(__mygo_rv64_vdso_gettimeofday)),
        symbol_offset(addr_of!(__mygo_rv64_vdso_clock_getres)),
        symbol_offset(addr_of!(__mygo_rv64_vdso_getcpu)),
    ];
    let mut out = [(0usize, 0usize); 5];
    for i in 0..offsets.len() {
        let end = if i + 1 < offsets.len() {
            offsets[i + 1]
        } else {
            symbol_offset(addr_of!(__mygo_rv64_vdso_text_end))
        };
        out[i] = (offsets[i], end - offsets[i]);
    }
    out
}

fn symbol_offset(sym: *const u8) -> usize {
    let start = addr_of!(__mygo_rv64_vdso_blob_start) as usize;
    TEXT_OFF + (sym as usize - start)
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

fn w16(b: &mut [u8], off: usize, val: u16) {
    b[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn w32(b: &mut [u8], off: usize, val: u32) {
    b[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn w64(b: &mut [u8], off: usize, val: u64) {
    b[off..off + 8].copy_from_slice(&val.to_le_bytes());
}
