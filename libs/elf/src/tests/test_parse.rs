//! ELF64 二进制格式解析测试。
//!
//! 构造 ELF64 字节序列验证 parse() 的正向解析（static/PIE/interpreter/多段/架构检测）
//! 与错误路径（截断/魔数错误/不支持 32-bit）。

extern crate std;

use crate::{AddressWidth, Arch, parse};
use ktest::ktest;
use std::vec;
use std::vec::Vec;

/// 构造最小合法 ELF64 header。
fn elf64_header(entry: u64, phnum: u16, e_type: u16, e_machine: u16) -> [u8; 64] {
    let mut h = [0u8; 64];
    h[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    h[4] = 2; // ELFCLASS64
    h[5] = 1; // ELFDATA2LSB
    h[6] = 1; // EV_CURRENT
    h[16..18].copy_from_slice(&e_type.to_le_bytes());
    h[18..20].copy_from_slice(&e_machine.to_le_bytes());
    h[20..24].copy_from_slice(&1u32.to_le_bytes());
    h[24..32].copy_from_slice(&entry.to_le_bytes());
    h[32..40].copy_from_slice(&64u64.to_le_bytes());
    h[52] = 64;
    h[54] = 56;
    h[56..58].copy_from_slice(&phnum.to_le_bytes());
    h
}

/// PT_LOAD program header。
fn pheader_load(offset: u64, vaddr: u64, filesz: u64, memsz: u64, flags: u32) -> [u8; 56] {
    let mut p = [0u8; 56];
    p[0..4].copy_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD = 1
    p[4..8].copy_from_slice(&flags.to_le_bytes());
    p[8..16].copy_from_slice(&offset.to_le_bytes());
    p[16..24].copy_from_slice(&vaddr.to_le_bytes());
    p[24..32].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr = p_vaddr
    p[32..40].copy_from_slice(&filesz.to_le_bytes());
    p[40..48].copy_from_slice(&memsz.to_le_bytes());
    p[48..56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
    p
}

/// PT_INTERP program header。
fn pheader_interp(offset: u64, filesz: u64) -> [u8; 56] {
    let mut p = [0u8; 56];
    p[0..4].copy_from_slice(&3u32.to_le_bytes()); // p_type = PT_INTERP = 3
    p[4..8].copy_from_slice(&4u32.to_le_bytes()); // PF_R
    p[8..16].copy_from_slice(&offset.to_le_bytes());
    p[32..40].copy_from_slice(&filesz.to_le_bytes());
    p[40..48].copy_from_slice(&filesz.to_le_bytes());
    p
}

/// 组装：header + phdrs + [pad to 0x1000 page boundary] + segment data。
const DATA_BASE: usize = 0x1000;

fn build_elf(header: &[u8; 64], phdrs: &[&[u8; 56]], seg_data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(header);
    for phdr in phdrs {
        v.extend_from_slice(&phdr[..]);
    }
    v.resize(DATA_BASE, 0);
    v.extend_from_slice(seg_data);
    v
}

// ── 正向解析 ──────────────────────────────────────────────────────

/// 验证 ET_EXEC 静态 ELF64 的入口地址、架构、位宽、非 PIE、无解释器。
#[ktest]
fn parse_valid_elf64_static() {
    let seg_data = [0xccu8; 0x100];
    let hdr = elf64_header(0x10000, 1, 2, 0x102); // ET_EXEC, EM_LOONGARCH
    let phdr = pheader_load(0x1000, 0x10000, 0x100, 0x100, 5); // PF_R|PF_X
    let elf_bytes = build_elf(&hdr, &[&phdr], &seg_data);

    let img = parse(&elf_bytes).expect("parse valid ELF64 static exec");
    assert_eq!(img.entry(), 0x10000);
    assert_eq!(img.arch(), Arch::LoongArch64);
    assert_eq!(img.class(), AddressWidth::Bits64);
    assert!(!img.is_pie());
    assert!(img.interpreter().is_none());
    assert_eq!(img.format_name(), "linux-elf64");

    let segs: Vec<_> = img.segments().collect();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].vaddr, 0x10000);
    assert_eq!(segs[0].memsz, 0x100);
    assert_eq!(segs[0].file_size, 0x100);
}

/// 验证 ET_DYN (PIE) ELF64 的 is_pie() 返回 true。
#[ktest]
fn parse_valid_elf64_pie() {
    let hdr = elf64_header(0x1000, 1, 3, 0x102); // ET_DYN
    let phdr = pheader_load(0x1000, 0x1000, 0x100, 0x100, 5);
    let elf_bytes = build_elf(&hdr, &[&phdr], &[0u8; 0x100]);
    let img = parse(&elf_bytes).expect("parse PIE ELF64");
    assert!(img.is_pie());
}

/// 验证带 PT_INTERP 段的 ELF64 能正确提取动态链接器路径。
#[ktest]
fn parse_valid_elf64_with_interpreter() {
    let interp = b"/lib/ld-linux.so.1\0";
    let hdr = elf64_header(0x10000, 2, 3, 0xF3); // DYN, RISC-V
    let load = pheader_load(0x1000, 0x10000, 0x100, 0x100, 5);
    let interp_phdr = pheader_interp(0x1100, interp.len() as u64);
    let mut v = Vec::new();
    v.extend_from_slice(&hdr);
    v.extend_from_slice(&load[..]);
    v.extend_from_slice(&interp_phdr[..]);
    v.resize(0x1100, 0);
    v.extend_from_slice(interp);
    let img = parse(&v).expect("parse ELF with interpreter");
    assert_eq!(img.arch(), Arch::Riscv64);
    assert_eq!(img.interpreter(), Some("/lib/ld-linux.so.1"));
}

/// 验证多 PT_LOAD 段的 ELF64 能正确返回全部段。
#[ktest]
fn parse_segments_count() {
    let hdr = elf64_header(0x10000, 2, 2, 0x102);
    let ph1 = pheader_load(0x1000, 0x10000, 0x100, 0x100, 5);
    let ph2 = pheader_load(0x2000, 0x20000, 0x200, 0x200, 6);
    let elf_bytes = build_elf(&hdr, &[&ph1, &ph2], &vec![0u8; 0x2000]);
    let img = parse(&elf_bytes).expect("parse multi-segment ELF");
    let segs: Vec<_> = img.segments().collect();
    assert_eq!(segs.len(), 2);
}

/// 验证 e_machine = 0x102 (EM_LOONGARCH) 被识别为 LoongArch64。
#[ktest]
fn parse_arch_loongarch64() {
    let hdr = elf64_header(0x10000, 1, 2, 0x102);
    let ph = pheader_load(0x1000, 0x10000, 0x100, 0x100, 5);
    let elf_bytes = build_elf(&hdr, &[&ph], &[0u8; 0x1000]);
    let img = parse(&elf_bytes).expect("parse");
    assert_eq!(img.arch(), Arch::LoongArch64);
}

/// 验证 e_machine = 0xF3 (EM_RISCV) 被识别为 Riscv64。
#[ktest]
fn parse_arch_riscv64() {
    let hdr = elf64_header(0x10000, 1, 2, 0xF3);
    let ph = pheader_load(0x1000, 0x10000, 0x100, 0x100, 5);
    let elf_bytes = build_elf(&hdr, &[&ph], &[0u8; 0x1000]);
    let img = parse(&elf_bytes).expect("parse");
    assert_eq!(img.arch(), Arch::Riscv64);
}

/// 验证未知 e_machine 值被保留为 Arch::Unknown(raw)，解析仍然成功。
#[ktest]
fn parse_arch_unknown() {
    let hdr = elf64_header(0x10000, 1, 2, 0xDEAD);
    let ph = pheader_load(0x1000, 0x10000, 0x100, 0x100, 5);
    let elf_bytes = build_elf(&hdr, &[&ph], &[0u8; 0x1000]);
    let img = parse(&elf_bytes).expect("parse with unknown machine");
    assert_eq!(img.arch(), Arch::Unknown(0xDEAD));
}

// ── 错误路径 ──────────────────────────────────────────────────────

/// 输入不足 ELF header 最小长度时解析失败，保证不会越界读取。
#[ktest]
fn parse_truncated_header() {
    let bytes = [0u8; 4];
    assert!(parse(&bytes).is_err());
}

/// 前 4 字节不是 \x7fELF 魔数时返回错误，格式嗅探依赖此契约。
#[ktest]
fn parse_bad_magic() {
    let mut bytes = [0u8; 128];
    bytes[0..4].copy_from_slice(b"HELO");
    let r = parse(&bytes);
    assert!(r.is_err());
    let e: errno::Errno = r.err().unwrap().into();
    assert_eq!(e, errno::Errno::ENOEXEC);
}

/// ELFCLASS32 (32-bit) 不被支持，当前只处理 64-bit。
#[ktest]
fn parse_unsupported_class() {
    let mut bytes = [0u8; 128];
    bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    bytes[4] = 1; // ELFCLASS32
    bytes[5] = 1; // ELFDATA2LSB
    bytes[6] = 1; // EV_CURRENT
    assert!(parse(&bytes).is_err());
}

/// 空输入应安全返回错误，不 panic。
#[ktest]
fn parse_empty_input() {
    assert!(parse(&[]).is_err());
}
