//! 基于 reader 的 Linux ELF64 规范元数据解码器与校验器。

use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::ops::Range;

use crate::error::ElfError;
use crate::reader::{ElfReadAt, ElfReadError, ElfReadLimits, read_range};
use crate::types::{Arch, SegmentPerms};

use super::raw::{
    EHDR_OFF_ENTRY, EHDR_OFF_MACHINE, EHDR_OFF_PHENTSIZE, EHDR_OFF_PHNUM, EHDR_OFF_PHOFF,
    EHDR_OFF_TYPE, EHDR_SIZE, EM_AARCH64, EM_LOONGARCH, EM_RISCV, EM_X86_64, ET_DYN, ET_EXEC, PF_R,
    PF_W, PF_X, PHDR_SIZE, PT_DYNAMIC, PT_INTERP, PT_LOAD, PT_PHDR,
};

const DYN_ENTRY_SIZE: usize = 16;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_REL: u64 = 17;
const DT_RELSZ: u64 = 18;
const DT_JMPREL: u64 = 23;
const DT_PLTRELSZ: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfFileType {
    Exec,
    Dyn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawPhdr {
    ty: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

/// 已校验的 PT_LOAD 元数据；这里不会保留段内容字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfLoadSegment {
    pub vaddr: usize,
    pub mem_size: usize,
    pub file_offset: u64,
    pub file_size: usize,
    pub alignment: u64,
    pub permissions: SegmentPerms,
}

/// 由 VFS reader 与内存 ELF 视图共享的自持有元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxElfMetadata {
    entry: usize,
    arch: Arch,
    file_type: ElfFileType,
    program_header_offset: u64,
    program_header_entry_size: u16,
    program_header_count: u16,
    interpreter: Option<String>,
    program_header_vaddr: Option<usize>,
    load_range: Option<Range<usize>>,
    load_segments: Vec<ElfLoadSegment>,
    can_run_without_interpreter: bool,
}

impl LinuxElfMetadata {
    pub fn entry(&self) -> usize {
        self.entry
    }

    pub fn arch(&self) -> Arch {
        self.arch
    }

    pub fn file_type(&self) -> ElfFileType {
        self.file_type
    }

    pub fn is_pie(&self) -> bool {
        self.file_type == ElfFileType::Dyn
    }

    pub fn program_header_offset(&self) -> u64 {
        self.program_header_offset
    }

    pub fn program_header_entry_size(&self) -> u16 {
        self.program_header_entry_size
    }

    pub fn program_header_count(&self) -> u16 {
        self.program_header_count
    }

    pub fn interpreter(&self) -> Option<&str> {
        self.interpreter.as_deref()
    }

    pub fn program_header_vaddr(&self) -> Option<usize> {
        self.program_header_vaddr
    }

    pub fn load_range(&self) -> Option<Range<usize>> {
        self.load_range.clone()
    }

    pub fn load_segments(&self) -> &[ElfLoadSegment] {
        &self.load_segments
    }

    pub fn can_run_without_interpreter(&self) -> bool {
        self.can_run_without_interpreter
    }
}

/// 解码并校验 Linux ELF64 元数据，但不读取 PT_LOAD 段内容。
pub fn read_linux_elf<R: ElfReadAt + ?Sized>(
    source: &R,
    limits: ElfReadLimits,
) -> Result<LinuxElfMetadata, ElfReadError<R::Error>> {
    if source.len() < EHDR_SIZE as u64 {
        return Err(ElfReadError::Format(ElfError::TooShort));
    }

    let mut ehdr = [0u8; EHDR_SIZE];
    read_range(source, 0, &mut ehdr, ElfError::TruncatedHeader)?;
    validate_ident(&ehdr)?;

    let ty = read_u16(&ehdr, EHDR_OFF_TYPE);
    let file_type = match ty {
        ET_EXEC => ElfFileType::Exec,
        ET_DYN => ElfFileType::Dyn,
        other => return Err(ElfReadError::Format(ElfError::UnsupportedType(other))),
    };
    let entry = usize::try_from(read_u64(&ehdr, EHDR_OFF_ENTRY))
        .map_err(|_| ElfReadError::Format(ElfError::InvalidEntry))?;
    let phoff = read_u64(&ehdr, EHDR_OFF_PHOFF);
    let phentsize = read_u16(&ehdr, EHDR_OFF_PHENTSIZE);
    let phnum = read_u16(&ehdr, EHDR_OFF_PHNUM);
    if phoff % 8 != 0 {
        return Err(ElfReadError::Format(ElfError::MisalignedPhoff));
    }
    if phentsize as usize != PHDR_SIZE {
        return Err(ElfReadError::Format(ElfError::TruncatedPhdr));
    }
    let phdr_bytes_len = (phentsize as usize)
        .checked_mul(phnum as usize)
        .ok_or(ElfReadError::Format(ElfError::PhdrOffsetOverflow))?;
    if phdr_bytes_len > limits.max_program_header_bytes {
        return Err(ElfReadError::ResourceExhausted);
    }
    let phdr_end = phoff
        .checked_add(phdr_bytes_len as u64)
        .ok_or(ElfReadError::Format(ElfError::PhdrOffsetOverflow))?;
    if phdr_end > source.len() {
        return Err(ElfReadError::Format(ElfError::TruncatedPhdr));
    }
    let mut phdr_bytes = Vec::new();
    phdr_bytes
        .try_reserve_exact(phdr_bytes_len)
        .map_err(|_| ElfReadError::ResourceExhausted)?;
    phdr_bytes.resize(phdr_bytes_len, 0);
    if phdr_bytes_len != 0 {
        read_range(source, phoff, &mut phdr_bytes, ElfError::TruncatedPhdr)?;
    }
    let mut phdrs = Vec::new();
    phdrs
        .try_reserve_exact(phnum as usize)
        .map_err(|_| ElfReadError::ResourceExhausted)?;
    for idx in 0..phnum as usize {
        let start = idx * PHDR_SIZE;
        phdrs.push(decode_phdr(&phdr_bytes[start..start + PHDR_SIZE]));
    }

    validate_load_segments(source.len(), &phdrs)?;
    validate_phdr_table(source.len(), phoff, phdr_end, &phdrs)?;
    validate_entry(entry, &phdrs)?;

    let interpreter = read_interp(source, limits, &phdrs)?;
    let dynamic = read_dynamic(source, limits, &phdrs)?;
    let can_run_without_interpreter = dynamic_can_run_without_interpreter(dynamic.as_deref());
    let program_header_vaddr = find_phdr_vaddr(phoff, phdr_end, &phdrs);
    let load_range = load_vaddr_range(&phdrs)?;
    let mut load_segments = Vec::new();
    for ph in &phdrs {
        if ph.ty != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        load_segments
            .try_reserve_exact(1)
            .map_err(|_| ElfReadError::ResourceExhausted)?;
        load_segments.push(ElfLoadSegment {
            vaddr: usize::try_from(ph.vaddr)
                .map_err(|_| ElfReadError::Format(ElfError::SegmentOffsetOverflow))?,
            mem_size: usize::try_from(ph.memsz)
                .map_err(|_| ElfReadError::Format(ElfError::SegmentOffsetOverflow))?,
            file_offset: ph.offset,
            file_size: usize::try_from(ph.filesz)
                .map_err(|_| ElfReadError::Format(ElfError::SegmentOffsetOverflow))?,
            alignment: ph.align,
            permissions: permissions(ph.flags),
        });
    }

    Ok(LinuxElfMetadata {
        entry,
        arch: map_machine(read_u16(&ehdr, EHDR_OFF_MACHINE)),
        file_type,
        program_header_offset: phoff,
        program_header_entry_size: phentsize,
        program_header_count: phnum,
        interpreter,
        program_header_vaddr,
        load_range,
        load_segments,
        can_run_without_interpreter,
    })
}

fn validate_ident<E>(ehdr: &[u8; EHDR_SIZE]) -> Result<(), ElfReadError<E>> {
    if ehdr[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfReadError::Format(ElfError::BadMagic));
    }
    if ehdr[4] != 2 {
        return Err(ElfReadError::Format(ElfError::UnsupportedClass));
    }
    if ehdr[5] != 1 {
        return Err(ElfReadError::Format(ElfError::UnsupportedData));
    }
    Ok(())
}

fn validate_load_segments<E>(file_len: u64, phdrs: &[RawPhdr]) -> Result<(), ElfReadError<E>> {
    for ph in phdrs {
        if ph.ty != PT_LOAD {
            continue;
        }
        if ph.align > 1
            && (!ph.align.is_power_of_two() || ph.vaddr % ph.align != ph.offset % ph.align)
        {
            return Err(ElfReadError::Format(ElfError::InvalidSegment));
        }
        if ph.filesz > ph.memsz {
            return Err(ElfReadError::Format(ElfError::InvalidSegment));
        }
        checked_file_range(file_len, ph.offset, ph.filesz)?;
        checked_vaddr_range(ph.vaddr, ph.memsz)?;
    }
    for (index, left) in phdrs.iter().enumerate() {
        if left.ty != PT_LOAD || left.memsz == 0 {
            continue;
        }
        let left_range = checked_vaddr_range(left.vaddr, left.memsz)?;
        for right in phdrs.iter().skip(index + 1) {
            if right.ty != PT_LOAD || right.memsz == 0 {
                continue;
            }
            let right_range = checked_vaddr_range(right.vaddr, right.memsz)?;
            if ranges_overlap(&left_range, &right_range) {
                return Err(ElfReadError::Format(ElfError::InvalidSegment));
            }
        }
    }
    Ok(())
}

fn validate_phdr_table<E>(
    file_len: u64,
    phoff: u64,
    phdr_end: u64,
    phdrs: &[RawPhdr],
) -> Result<(), ElfReadError<E>> {
    let mut seen = false;
    for ph in phdrs {
        if ph.ty != PT_PHDR {
            continue;
        }
        if seen || ph.filesz > ph.memsz {
            return Err(ElfReadError::Format(ElfError::InvalidPhdr));
        }
        seen = true;
        checked_file_range(file_len, ph.offset, ph.filesz)?;
        let end = ph
            .offset
            .checked_add(ph.filesz)
            .ok_or(ElfReadError::Format(ElfError::PhdrOffsetOverflow))?;
        if ph.offset > phoff || end < phdr_end {
            return Err(ElfReadError::Format(ElfError::InvalidPhdr));
        }
        checked_vaddr_range(ph.vaddr, ph.memsz)?;
    }
    Ok(())
}

fn validate_entry<E>(entry: usize, phdrs: &[RawPhdr]) -> Result<(), ElfReadError<E>> {
    for ph in phdrs {
        if ph.ty != PT_LOAD || ph.memsz == 0 || ph.flags & PF_X == 0 {
            continue;
        }
        let range = checked_vaddr_range(ph.vaddr, ph.memsz)?;
        if range.contains(&entry) {
            return Ok(());
        }
    }
    Err(ElfReadError::Format(ElfError::InvalidEntry))
}

fn read_interp<R: ElfReadAt + ?Sized>(
    source: &R,
    limits: ElfReadLimits,
    phdrs: &[RawPhdr],
) -> Result<Option<String>, ElfReadError<R::Error>> {
    for ph in phdrs {
        if ph.ty != PT_INTERP {
            continue;
        }
        let len = usize::try_from(ph.filesz)
            .map_err(|_| ElfReadError::Format(ElfError::InvalidInterp))?;
        if len <= 1 || len > limits.max_interpreter_bytes {
            return Err(ElfReadError::Format(ElfError::InvalidInterp));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| ElfReadError::ResourceExhausted)?;
        bytes.resize(len, 0);
        read_range(source, ph.offset, &mut bytes, ElfError::InvalidInterp)?;
        if bytes.last() != Some(&0) {
            return Err(ElfReadError::Format(ElfError::InvalidInterp));
        }
        let path = &bytes[..len - 1];
        if path.is_empty() || path.contains(&0) {
            return Err(ElfReadError::Format(ElfError::InvalidInterp));
        }
        let path = core::str::from_utf8(path)
            .map_err(|_| ElfReadError::Format(ElfError::InvalidInterp))?;
        let mut result = String::new();
        result
            .try_reserve_exact(path.len())
            .map_err(|_| ElfReadError::ResourceExhausted)?;
        result.push_str(path);
        return Ok(Some(result));
    }
    Ok(None)
}

fn read_dynamic<R: ElfReadAt + ?Sized>(
    source: &R,
    limits: ElfReadLimits,
    phdrs: &[RawPhdr],
) -> Result<Option<Vec<u8>>, ElfReadError<R::Error>> {
    for ph in phdrs {
        if ph.ty != PT_DYNAMIC {
            continue;
        }
        let len = usize::try_from(ph.filesz)
            .map_err(|_| ElfReadError::Format(ElfError::SegmentOffsetOverflow))?;
        if len > limits.max_dynamic_bytes {
            return Err(ElfReadError::ResourceExhausted);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| ElfReadError::ResourceExhausted)?;
        bytes.resize(len, 0);
        if len != 0 {
            read_range(
                source,
                ph.offset,
                &mut bytes,
                ElfError::SegmentOffsetOverflow,
            )?;
        }
        return Ok(Some(bytes));
    }
    Ok(None)
}

fn dynamic_can_run_without_interpreter(dynamic: Option<&[u8]>) -> bool {
    let Some(dynamic) = dynamic else { return true };
    let mut has_needed = false;
    let mut rela_size = 0u64;
    let mut rel_size = 0u64;
    let mut plt_rel_size = 0u64;
    let mut has_jmprel = false;
    for entry in dynamic.chunks_exact(DYN_ENTRY_SIZE) {
        let tag = read_u64(entry, 0);
        let value = read_u64(entry, 8);
        match tag {
            DT_NULL => break,
            DT_NEEDED => has_needed = true,
            DT_RELASZ => rela_size = value,
            DT_RELSZ => rel_size = value,
            DT_PLTRELSZ => plt_rel_size = value,
            DT_JMPREL => has_jmprel = value != 0,
            DT_RELA | DT_REL => {}
            _ => {}
        }
    }
    !has_needed && rela_size == 0 && rel_size == 0 && plt_rel_size == 0 && !has_jmprel
}

fn find_phdr_vaddr(phoff: u64, phdr_end: u64, phdrs: &[RawPhdr]) -> Option<usize> {
    for ph in phdrs {
        if ph.ty == PT_PHDR {
            if let Some(value) = phdr_table_vaddr_in_segment(phoff, phdr_end, ph) {
                return Some(value);
            }
        }
    }
    for ph in phdrs {
        if ph.ty == PT_LOAD {
            if let Some(value) = phdr_table_vaddr_in_segment(phoff, phdr_end, ph) {
                return Some(value);
            }
        }
    }
    None
}

fn load_vaddr_range<E>(phdrs: &[RawPhdr]) -> Result<Option<Range<usize>>, ElfReadError<E>> {
    let mut range: Option<Range<usize>> = None;
    for ph in phdrs {
        if ph.ty != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        let current = checked_vaddr_range(ph.vaddr, ph.memsz)?;
        range = Some(match range {
            Some(previous) => previous.start.min(current.start)..previous.end.max(current.end),
            None => current,
        });
    }
    Ok(range)
}

fn checked_file_range<E>(
    file_len: u64,
    offset: u64,
    size: u64,
) -> Result<Range<u64>, ElfReadError<E>> {
    let end = offset
        .checked_add(size)
        .ok_or(ElfReadError::Format(ElfError::SegmentOffsetOverflow))?;
    if end > file_len {
        return Err(ElfReadError::Format(ElfError::SegmentOffsetOverflow));
    }
    Ok(offset..end)
}

fn checked_vaddr_range<E>(vaddr: u64, size: u64) -> Result<Range<usize>, ElfReadError<E>> {
    let end = vaddr
        .checked_add(size)
        .ok_or(ElfReadError::Format(ElfError::SegmentOffsetOverflow))?;
    let start = usize::try_from(vaddr)
        .map_err(|_| ElfReadError::Format(ElfError::SegmentOffsetOverflow))?;
    let end =
        usize::try_from(end).map_err(|_| ElfReadError::Format(ElfError::SegmentOffsetOverflow))?;
    Ok(start..end)
}

fn phdr_table_vaddr_in_segment(phoff: u64, phdr_end: u64, ph: &RawPhdr) -> Option<usize> {
    let segment_end = ph.offset.checked_add(ph.filesz)?;
    if ph.offset > phoff || segment_end < phdr_end {
        return None;
    }
    let delta = phoff.checked_sub(ph.offset)?;
    usize::try_from(ph.vaddr.checked_add(delta)?).ok()
}

fn decode_phdr(bytes: &[u8]) -> RawPhdr {
    RawPhdr {
        ty: read_u32(bytes, 0),
        flags: read_u32(bytes, 4),
        offset: read_u64(bytes, 8),
        vaddr: read_u64(bytes, 16),
        filesz: read_u64(bytes, 32),
        memsz: read_u64(bytes, 40),
        align: read_u64(bytes, 48),
    }
}

fn permissions(flags: u32) -> SegmentPerms {
    let mut result = SegmentPerms::EMPTY;
    if flags & PF_R != 0 {
        result = result.with(SegmentPerms::READ);
    }
    if flags & PF_W != 0 {
        result = result.with(SegmentPerms::WRITE);
    }
    if flags & PF_X != 0 {
        result = result.with(SegmentPerms::EXEC);
    }
    result
}

fn map_machine(machine: u16) -> Arch {
    match machine {
        EM_LOONGARCH => Arch::LoongArch64,
        EM_RISCV => Arch::Riscv64,
        EM_X86_64 => Arch::X86_64,
        EM_AARCH64 => Arch::Aarch64,
        other => Arch::Unknown(other),
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed ELF field"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed ELF field"),
    )
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}
