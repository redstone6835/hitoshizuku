//! x86_64 vDSO 镜像与周期 hook。
//!
//! 镜像是一个可被 ELF 动态链接器识别的最小 `linux-vdso.so.1`。
//! `clock_gettime(2)` 等入口使用稳定的 x86_64 syscall ABI 做 fallback；这样
//! 即使 TSC 校准或共享数据页尚未完成，入口仍然保持正确的错误码和寄存器
//! 约定。共享数据页布局与 `kernel::vdso::VdsoData` 保持一致，后续可以在不
//! 变更用户 ABI 的情况下加入 TSC fast path。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use super::syscall::nr::{
    SYS_CLOCK_GETRES, SYS_CLOCK_GETTIME, SYS_GETCPU, SYS_GETTIMEOFDAY, SYS_RT_SIGRETURN,
};

/// vDSO 数据页偏移（第二个 4 KiB 页）。
pub const VDSO_DATA_PAGE_OFFSET: usize = 0x1000;
/// 代码/ELF 元数据页长度。
pub const VDSO_TEXT_PAGE_SIZE: usize = VDSO_DATA_PAGE_OFFSET;
/// vDSO 总映射长度（代码页 + 共享数据页）。
pub const VDSO_TOTAL_SIZE: usize = 0x2000;

// 与 kernel/src/vdso.rs::VdsoData 的 repr(C) 布局保持一致。
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

const TEXT_OFF: usize = 0x400;
const DYNSYM_OFF: usize = 0x100;
const DYNSTR_OFF: usize = 0x190;
const HASH_OFF: usize = 0x230;
const VERSYM_OFF: usize = 0x260;
const VERDEF_OFF: usize = 0x270;
const DYNAMIC_OFF: usize = 0x2b0;
const DYNAMIC_ENTRY_COUNT: usize = 10;
const VERDEF_SIZE: usize = 28;
const VDSO_SONAME: &[u8] = b"linux-vdso.so.1";
const VDSO_VERSION: &[u8] = b"LINUX_4.15";

const SYMBOL_NAMES: [&[u8]; 5] = [
    b"__kernel_rt_sigreturn",
    b"__vdso_clock_gettime",
    b"__vdso_gettimeofday",
    b"__vdso_clock_getres",
    b"__vdso_getcpu",
];

// Every entry is position-independent and follows the SysV x86_64 ABI. The
// syscall instruction preserves all argument registers needed by these calls;
// RAX is overwritten with the Linux syscall number and the return value.
const SIGRETURN_CODE: &[u8] = &[
    0xb8,
    SYS_RT_SIGRETURN as u8,
    (SYS_RT_SIGRETURN >> 8) as u8,
    (SYS_RT_SIGRETURN >> 16) as u8,
    (SYS_RT_SIGRETURN >> 24) as u8,
    0x0f,
    0x05,
    0x0f,
    0x0b,
];
const CLOCK_GETTIME_CODE: &[u8] = &[
    0xb8,
    SYS_CLOCK_GETTIME as u8,
    (SYS_CLOCK_GETTIME >> 8) as u8,
    (SYS_CLOCK_GETTIME >> 16) as u8,
    (SYS_CLOCK_GETTIME >> 24) as u8,
    0x0f,
    0x05,
    0xc3,
];
const GETTIMEOFDAY_CODE: &[u8] = &[
    0xb8,
    SYS_GETTIMEOFDAY as u8,
    (SYS_GETTIMEOFDAY >> 8) as u8,
    (SYS_GETTIMEOFDAY >> 16) as u8,
    (SYS_GETTIMEOFDAY >> 24) as u8,
    0x0f,
    0x05,
    0xc3,
];
const CLOCK_GETRES_CODE: &[u8] = &[
    0xb8,
    SYS_CLOCK_GETRES as u8,
    (SYS_CLOCK_GETRES >> 8) as u8,
    (SYS_CLOCK_GETRES >> 16) as u8,
    (SYS_CLOCK_GETRES >> 24) as u8,
    0x0f,
    0x05,
    0xc3,
];
const GETCPU_CODE: &[u8] = &[
    0xb8,
    SYS_GETCPU as u8,
    (SYS_GETCPU >> 8) as u8,
    (SYS_GETCPU >> 16) as u8,
    (SYS_GETCPU >> 24) as u8,
    0x0f,
    0x05,
    0xc3,
];

type Hook = fn(u64);
static TIMER_TICK_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static NET_POLL_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static TTY_POLL_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

#[inline]
fn install_hook(slot: &AtomicPtr<()>, hook: Hook) {
    slot.store(hook as *mut (), Ordering::Release);
}

#[inline]
fn invoke_hook(slot: &AtomicPtr<()>, now_ns: u64) {
    let raw = slot.load(Ordering::Acquire);
    if !raw.is_null() {
        // Function pointers and data pointers have the same representation on
        // the supported x86_64 ABIs. Registration accepts only `fn(u64)`.
        let hook: Hook = unsafe { core::mem::transmute(raw) };
        hook(now_ns);
    }
}

pub fn sigreturn_entry_offset() -> usize {
    TEXT_OFF
}

pub fn register_timer_tick_hook(hook: Hook) {
    install_hook(&TIMER_TICK_HOOK, hook);
}

pub fn run_timer_tick_hook(now_ns: u64) {
    invoke_hook(&TIMER_TICK_HOOK, now_ns);
}

pub fn register_net_poll_hook(hook: Hook) {
    install_hook(&NET_POLL_HOOK, hook);
}

pub fn run_net_poll_hook(now_ns: u64) {
    invoke_hook(&NET_POLL_HOOK, now_ns);
}

pub fn register_tty_poll_hook(hook: Hook) {
    install_hook(&TTY_POLL_HOOK, hook);
}

pub fn run_tty_poll_hook(now_ns: u64) {
    invoke_hook(&TTY_POLL_HOOK, now_ns);
}

/// 生成完整 vDSO ELF 镜像。
pub fn vdso_image() -> Vec<u8> {
    let mut image = alloc::vec![0u8; VDSO_TOTAL_SIZE];
    build_elf_header(&mut image);
    build_program_headers(&mut image);
    build_dynsym(&mut image);
    build_dynstr(&mut image);
    build_hash(&mut image);
    build_versions(&mut image);
    build_dynamic(&mut image);
    build_text(&mut image);
    image
}

fn build_elf_header(image: &mut [u8]) {
    image[0..4].copy_from_slice(b"\x7fELF");
    image[4] = 2; // ELFCLASS64
    image[5] = 1; // ELFDATA2LSB
    image[6] = 1; // EV_CURRENT
    write_u16(image, 16, 3); // ET_DYN
    write_u16(image, 18, 62); // EM_X86_64
    write_u32(image, 20, 1); // EV_CURRENT
    write_u64(image, 24, sigreturn_entry_offset() as u64); // e_entry
    write_u64(image, 32, 64); // e_phoff
    write_u64(image, 40, 0); // e_shoff
    write_u32(image, 48, 0); // e_flags
    write_u16(image, 52, 64); // e_ehsize
    write_u16(image, 54, 56); // e_phentsize
    write_u16(image, 56, 2); // e_phnum
}

fn build_program_headers(image: &mut [u8]) {
    // PT_LOAD: the kernel maps the first page executable and the second page
    // separately as the shared, read-only VdsoData page.
    let load = 64;
    write_u32(image, load, 1);
    write_u32(image, load + 4, 5); // PF_R | PF_X
    write_u64(image, load + 8, 0);
    write_u64(image, load + 16, 0);
    write_u64(image, load + 24, 0);
    write_u64(image, load + 32, VDSO_TOTAL_SIZE as u64);
    write_u64(image, load + 40, VDSO_TOTAL_SIZE as u64);
    write_u64(image, load + 48, 4096);

    // PT_DYNAMIC lies in the read-only metadata prefix of the text page.
    let dynamic = load + 56;
    write_u32(image, dynamic, 2);
    write_u32(image, dynamic + 4, 4); // PF_R
    write_u64(image, dynamic + 8, DYNAMIC_OFF as u64);
    write_u64(image, dynamic + 16, DYNAMIC_OFF as u64);
    write_u64(image, dynamic + 24, DYNAMIC_OFF as u64);
    let size = (DYNAMIC_ENTRY_COUNT * 16) as u64;
    write_u64(image, dynamic + 32, size);
    write_u64(image, dynamic + 40, size);
    write_u64(image, dynamic + 48, 8);
}

fn build_dynsym(image: &mut [u8]) {
    let names = dynstr_offsets();
    let layouts = symbol_layouts();
    // Entry zero is the required STN_UNDEF all-zero record.
    for (index, (value, size)) in layouts.iter().enumerate() {
        let offset = DYNSYM_OFF + (index + 1) * 24;
        write_u32(image, offset, names[index] as u32);
        image[offset + 4] = 0x12; // STB_GLOBAL | STT_FUNC
        image[offset + 5] = 0;
        write_u16(image, offset + 6, 1); // first load section
        write_u64(image, offset + 8, *value as u64);
        write_u64(image, offset + 16, *size as u64);
    }
}

fn build_dynstr(image: &mut [u8]) {
    image[DYNSTR_OFF] = 0;
    let mut cursor = DYNSTR_OFF + 1;
    for name in SYMBOL_NAMES {
        image[cursor..cursor + name.len()].copy_from_slice(name);
        cursor += name.len();
        image[cursor] = 0;
        cursor += 1;
    }
    image[cursor..cursor + VDSO_SONAME.len()].copy_from_slice(VDSO_SONAME);
    cursor += VDSO_SONAME.len();
    image[cursor] = 0;
    cursor += 1;
    image[cursor..cursor + VDSO_VERSION.len()].copy_from_slice(VDSO_VERSION);
    image[cursor + VDSO_VERSION.len()] = 0;
}

fn build_hash(image: &mut [u8]) {
    const BUCKETS: u32 = 3;
    const CHAINS: u32 = 6;
    write_u32(image, HASH_OFF, BUCKETS);
    write_u32(image, HASH_OFF + 4, CHAINS);
    let buckets = HASH_OFF + 8;
    let chains = buckets + BUCKETS as usize * 4;
    for symbol in 1u32..=5 {
        let bucket = elf_hash(SYMBOL_NAMES[(symbol - 1) as usize]) % BUCKETS;
        let bucket_offset = buckets + bucket as usize * 4;
        let previous = read_u32(image, bucket_offset);
        if previous == 0 {
            write_u32(image, bucket_offset, symbol);
            continue;
        }
        let mut current = previous;
        loop {
            let next_offset = chains + current as usize * 4;
            let next = read_u32(image, next_offset);
            if next == 0 {
                write_u32(image, next_offset, symbol);
                break;
            }
            current = next;
        }
    }
}

fn build_versions(image: &mut [u8]) {
    // .gnu.version: symbol zero is local; all exported entries use index 2.
    write_u16(image, VERSYM_OFF, 0);
    for index in 1..=5 {
        write_u16(image, VERSYM_OFF + index * 2, 2);
    }
    build_verdef(image, VERDEF_OFF, 1, 1, dynstr_soname_offset(), VERDEF_SIZE);
    build_verdef(
        image,
        VERDEF_OFF + VERDEF_SIZE,
        2,
        0,
        dynstr_version_offset(),
        0,
    );
}

fn build_verdef(image: &mut [u8], offset: usize, index: u16, flags: u16, name: usize, next: usize) {
    write_u16(image, offset, 1); // VER_DEF_CURRENT
    write_u16(image, offset + 2, flags);
    write_u16(image, offset + 4, index);
    write_u16(image, offset + 6, 1); // vd_cnt
    let source = if index == 1 {
        VDSO_SONAME
    } else {
        VDSO_VERSION
    };
    write_u32(image, offset + 8, elf_hash(source));
    write_u32(image, offset + 12, 20); // vd_aux
    write_u32(image, offset + 16, next as u32);
    write_u32(image, offset + 20, name as u32); // Verdaux.vda_name
    write_u32(image, offset + 24, 0); // Verdaux.vda_next
}

fn build_dynamic(image: &mut [u8]) {
    let entries = [
        (6u64, DYNSYM_OFF as u64),
        (5, DYNSTR_OFF as u64),
        (10, dynstr_total_len() as u64),
        (4, HASH_OFF as u64),
        (11, 24),
        (0x6fff_fff0, VERSYM_OFF as u64),
        (0x6fff_fffc, VERDEF_OFF as u64),
        (0x6fff_fffd, 2),
        (14, dynstr_soname_offset() as u64),
        (0, 0),
    ];
    for (index, (tag, value)) in entries.into_iter().enumerate() {
        let offset = DYNAMIC_OFF + index * 16;
        write_u64(image, offset, tag);
        write_u64(image, offset + 8, value);
    }
}

fn build_text(image: &mut [u8]) {
    let mut cursor = TEXT_OFF;
    for code in [
        SIGRETURN_CODE,
        CLOCK_GETTIME_CODE,
        GETTIMEOFDAY_CODE,
        CLOCK_GETRES_CODE,
        GETCPU_CODE,
    ] {
        cursor = (cursor + 15) & !15;
        image[cursor..cursor + code.len()].copy_from_slice(code);
        cursor += code.len();
    }
}

fn symbol_layouts() -> [(usize, usize); 5] {
    let mut cursor = TEXT_OFF;
    let mut result = [(0usize, 0usize); 5];
    for (index, code) in [
        SIGRETURN_CODE,
        CLOCK_GETTIME_CODE,
        GETTIMEOFDAY_CODE,
        CLOCK_GETRES_CODE,
        GETCPU_CODE,
    ]
    .into_iter()
    .enumerate()
    {
        cursor = (cursor + 15) & !15;
        result[index] = (cursor, code.len());
        cursor += code.len();
    }
    result
}

fn dynstr_offsets() -> [usize; 5] {
    let mut result = [0usize; 5];
    let mut cursor = 1usize;
    for (index, name) in SYMBOL_NAMES.iter().enumerate() {
        result[index] = cursor;
        cursor += name.len() + 1;
    }
    result
}

fn dynstr_total_len() -> usize {
    1 + SYMBOL_NAMES
        .iter()
        .map(|name| name.len() + 1)
        .sum::<usize>()
        + VDSO_SONAME.len()
        + 1
        + VDSO_VERSION.len()
        + 1
}

fn dynstr_soname_offset() -> usize {
    1 + SYMBOL_NAMES
        .iter()
        .map(|name| name.len() + 1)
        .sum::<usize>()
}

fn dynstr_version_offset() -> usize {
    dynstr_soname_offset() + VDSO_SONAME.len() + 1
}

fn elf_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for byte in name {
        hash = (hash << 4).wrapping_add(*byte as u32);
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

#[inline]
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_has_x86_64_elf_and_nonempty_entries() {
        let image = vdso_image();
        assert_eq!(image.len(), VDSO_TOTAL_SIZE);
        assert_eq!(&image[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes([image[18], image[19]]), 62);
        assert_eq!(
            u64::from_le_bytes(image[24..32].try_into().unwrap()),
            TEXT_OFF as u64
        );
        assert_eq!(image[TEXT_OFF], 0xb8); // mov eax, syscall_nr
        assert!(
            image[TEXT_OFF..VDSO_TEXT_PAGE_SIZE]
                .iter()
                .any(|byte| *byte != 0)
        );
    }

    #[test]
    fn dynamic_metadata_fits_before_code() {
        assert!(DYNAMIC_OFF + DYNAMIC_ENTRY_COUNT * 16 <= TEXT_OFF);
        assert!(DYNSTR_OFF + dynstr_total_len() <= HASH_OFF);
        assert!(VERDEF_OFF + 2 * VERDEF_SIZE <= DYNAMIC_OFF);
        assert!(sigreturn_entry_offset() < VDSO_TEXT_PAGE_SIZE);
    }

    #[test]
    fn hooks_are_replaceable() {
        fn tick(_: u64) {}
        register_timer_tick_hook(tick);
        run_timer_tick_hook(1);
        register_net_poll_hook(tick);
        run_net_poll_hook(2);
        register_tty_poll_hook(tick);
        run_tty_poll_hook(3);
    }
}
