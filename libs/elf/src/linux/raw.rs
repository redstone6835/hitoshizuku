//! Linux ELF64 的裸格式定义：Ehdr / Phdr + 常量。
//!
//! 本文件**不**做任何校验；各字段直接对应 ELF spec 的 `Elf64_Ehdr` /
//! `Elf64_Phdr`。读取由同级 [`super::parse`] 在保证对齐 / 不溢出之后做。
//!
//! 这里 `#[repr(C)]` 的结构体**从不**对入参字节切片 `transmute` —— image
//! 字节不保证 8 字节对齐。真正读值走 `from_le_bytes` 逐字段；这两个
//! 结构体主要用于描述字段偏移（`offset_of!`）与文档语义。

#![allow(dead_code)]

/// `Elf64_Ehdr`。对应 ELF64 规范 §4.1。
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Ehdr64 {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

/// `Elf64_Phdr`。对应 ELF64 规范 §5.1。
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Phdr64 {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

// ── e_ident 下标 ─────────────────────────────────────────────────────────────
pub(super) const EI_MAG0: usize = 0;
pub(super) const EI_MAG1: usize = 1;
pub(super) const EI_MAG2: usize = 2;
pub(super) const EI_MAG3: usize = 3;
pub(super) const EI_CLASS: usize = 4;
pub(super) const EI_DATA: usize = 5;
pub(super) const EI_VERSION: usize = 6;
pub(super) const EI_OSABI: usize = 7;

// ── e_ident 字段值 ───────────────────────────────────────────────────────────
pub(super) const ELFCLASS64: u8 = 2;
pub(super) const ELFDATA2LSB: u8 = 1;
pub(super) const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];

// ── e_type ───────────────────────────────────────────────────────────────────
pub(super) const ET_EXEC: u16 = 2;
pub(super) const ET_DYN: u16 = 3;

// ── e_machine（允许列表；其它落到 Arch::Unknown） ────────────────────────────
pub(super) const EM_X86_64: u16 = 62;
pub(super) const EM_AARCH64: u16 = 183;
pub(super) const EM_RISCV: u16 = 243;
pub(super) const EM_LOONGARCH: u16 = 258;

// ── p_type ───────────────────────────────────────────────────────────────────
pub(super) const PT_NULL: u32 = 0;
pub(super) const PT_LOAD: u32 = 1;
pub(super) const PT_DYNAMIC: u32 = 2;
pub(super) const PT_INTERP: u32 = 3;
pub(super) const PT_NOTE: u32 = 4;
pub(super) const PT_PHDR: u32 = 6;

// ── p_flags ──────────────────────────────────────────────────────────────────
pub(super) const PF_X: u32 = 1 << 0;
pub(super) const PF_W: u32 = 1 << 1;
pub(super) const PF_R: u32 = 1 << 2;

// ── 字段偏移（Ehdr 内）—— 解析时走 from_le_bytes，避免 align_of 依赖 ─────────
pub(super) const EHDR_SIZE: usize = 64;
pub(super) const EHDR_OFF_TYPE: usize = 0x10;
pub(super) const EHDR_OFF_MACHINE: usize = 0x12;
pub(super) const EHDR_OFF_VERSION: usize = 0x14;
pub(super) const EHDR_OFF_ENTRY: usize = 0x18;
pub(super) const EHDR_OFF_PHOFF: usize = 0x20;
pub(super) const EHDR_OFF_SHOFF: usize = 0x28;
pub(super) const EHDR_OFF_FLAGS: usize = 0x30;
pub(super) const EHDR_OFF_EHSIZE: usize = 0x34;
pub(super) const EHDR_OFF_PHENTSIZE: usize = 0x36;
pub(super) const EHDR_OFF_PHNUM: usize = 0x38;

// ── Phdr 布局 ────────────────────────────────────────────────────────────────
pub(super) const PHDR_SIZE: usize = 56;
pub(super) const PHDR_OFF_TYPE: usize = 0x00;
pub(super) const PHDR_OFF_FLAGS: usize = 0x04;
pub(super) const PHDR_OFF_OFFSET: usize = 0x08;
pub(super) const PHDR_OFF_VADDR: usize = 0x10;
pub(super) const PHDR_OFF_PADDR: usize = 0x18;
pub(super) const PHDR_OFF_FILESZ: usize = 0x20;
pub(super) const PHDR_OFF_MEMSZ: usize = 0x28;
pub(super) const PHDR_OFF_ALIGN: usize = 0x30;
