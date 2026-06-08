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
use core::arch::global_asm;
use core::mem::transmute;
use core::ptr::addr_of;
use core::slice;
use core::sync::atomic::{AtomicUsize, Ordering};

global_asm!(include_str!("vdso_text.S"));

static TIMER_TICK_HOOK: AtomicUsize = AtomicUsize::new(0);
static NET_POLL_HOOK: AtomicUsize = AtomicUsize::new(0);

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

unsafe extern "C" {
    static __mygo_vdso_blob_start: u8;
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
    let start = addr_of!(__mygo_vdso_blob_start) as usize;
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
    let start = addr_of!(__mygo_vdso_blob_start) as usize;
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
