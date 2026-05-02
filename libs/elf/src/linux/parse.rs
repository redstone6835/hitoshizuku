//! Linux ELF64 镜像的顶层解析。
//!
//! 对外只暴露 [`LinuxElfImage`]：
//! - `parse(bytes)` 一次性做完 Ehdr/Phdr 校验、切出 interp 字符串；
//! - `segments_typed()` 静态分派迭代 PT_LOAD；
//! - `impl Image` 走 `crate::parse` 的动态分派路径。
//!
//! **零 alloc 于解析本身**：所有缓存字段都是对原字节切片的借用或少量 `Copy`
//! 标量。`Box<dyn Image>` 的堆分配只发生在 [`crate::parse`] 返回时。

use core::ops::Range;
use core::str;

use crate::error::ElfError;
use crate::image::Image;
use crate::types::{AddressWidth, Arch, Segment, SegmentPerms};

use super::header::{accept_type, validate_ident};
use super::program_header::PhdrView;
use super::raw::{
    EHDR_OFF_ENTRY, EHDR_OFF_MACHINE, EHDR_OFF_PHENTSIZE, EHDR_OFF_PHNUM, EHDR_OFF_PHOFF,
    EHDR_OFF_TYPE, EHDR_SIZE, EM_AARCH64, EM_LOONGARCH, EM_RISCV, EM_X86_64, ET_DYN, PF_R, PF_W,
    PF_X, PT_INTERP, PT_LOAD, PT_PHDR, Phdr64,
};

/// 解析完成的 Linux ELF64 镜像。字段全部是对原 image 字节的借用视图。
pub struct LinuxElfImage<'a> {
    bytes: &'a [u8],
    entry: usize,
    arch: Arch,
    ty: u16, // ET_EXEC / ET_DYN
    phentsize: u16,
    phnum: u16,
    phdrs: PhdrView<'a>,
    interp: Option<&'a str>,
    phdr_vaddr: Option<usize>,
    load_range: Option<Range<usize>>,
}

impl<'a> LinuxElfImage<'a> {
    /// 读 Ehdr + Phdr 并校验；失败时不留副作用。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::TooShort);
        }
        let mut ident = [0u8; 16];
        ident.copy_from_slice(&bytes[..16]);
        validate_ident(&ident)?;

        let ty = read_u16(bytes, EHDR_OFF_TYPE);
        accept_type(ty)?;
        let machine = read_u16(bytes, EHDR_OFF_MACHINE);
        let entry = read_u64(bytes, EHDR_OFF_ENTRY) as usize;
        let phoff = read_u64(bytes, EHDR_OFF_PHOFF);
        let phentsize = read_u16(bytes, EHDR_OFF_PHENTSIZE);
        let phnum = read_u16(bytes, EHDR_OFF_PHNUM);

        let phdrs = PhdrView::new(bytes, phoff, phentsize, phnum)?;
        validate_load_segments(bytes, &phdrs)?;
        let interp = find_interp(bytes, &phdrs)?;
        let phdr_vaddr = find_phdr_vaddr(phoff, &phdrs);
        let load_range = load_vaddr_range(&phdrs)?;

        Ok(Self {
            bytes,
            entry,
            arch: map_machine(machine),
            ty,
            phentsize,
            phnum,
            phdrs,
            interp,
            phdr_vaddr,
            load_range,
        })
    }

    /// 静态分派：遍历 PT_LOAD 段，产出格式无关的 [`Segment`]。
    pub fn segments_typed<'b>(&'b self) -> LinuxSegmentIter<'a, 'b> {
        LinuxSegmentIter {
            bytes: self.bytes,
            view: self.phdrs.clone(),
            cursor: 0,
            _m: core::marker::PhantomData,
        }
    }

    /// `e_type` 的原始值（`ET_EXEC` / `ET_DYN`），用于诊断。
    pub fn raw_type(&self) -> u16 {
        self.ty
    }
}

impl<'a> Image<'a> for LinuxElfImage<'a> {
    fn entry(&self) -> usize {
        self.entry
    }

    fn arch(&self) -> Arch {
        self.arch
    }

    fn class(&self) -> AddressWidth {
        AddressWidth::Bits64
    }

    fn is_pie(&self) -> bool {
        self.ty == ET_DYN
    }

    fn interpreter(&self) -> Option<&'a str> {
        self.interp
    }

    fn segments<'b>(&'b self) -> alloc::boxed::Box<dyn Iterator<Item = Segment<'a>> + 'b>
    where
        'a: 'b,
    {
        alloc::boxed::Box::new(self.segments_typed())
    }

    fn format_name(&self) -> &'static str {
        "linux-elf64"
    }

    fn phdr_vaddr(&self) -> Option<usize> {
        self.phdr_vaddr
    }

    fn phdr_entry_size(&self) -> usize {
        self.phentsize as usize
    }

    fn phdr_count(&self) -> usize {
        self.phnum as usize
    }

    fn load_vaddr_range(&self) -> Option<Range<usize>> {
        self.load_range.clone()
    }
}

// ── 段迭代器 ─────────────────────────────────────────────────────────────────

/// PT_LOAD 段迭代器。`'a` = image 字节生命周期，`'b` = image 自身被借用的
/// 生命周期；产出的 [`Segment<'a>`] 借自原始字节流。
pub struct LinuxSegmentIter<'a, 'b> {
    bytes: &'a [u8],
    view: PhdrView<'a>,
    cursor: usize,
    _m: core::marker::PhantomData<&'b ()>,
}

impl<'a, 'b> Iterator for LinuxSegmentIter<'a, 'b> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Segment<'a>> {
        while self.cursor < self.view.count() {
            let idx = self.cursor;
            self.cursor += 1;
            let phdr = self.view.get(idx)?;
            if phdr.p_type != PT_LOAD {
                continue;
            }
            // 切段数据：[p_offset, p_offset + p_filesz)。
            let off = phdr.p_offset as usize;
            let filesz = phdr.p_filesz as usize;
            let end = off.checked_add(filesz)?;
            debug_assert!(end <= self.bytes.len());
            return Some(Segment {
                vaddr: phdr.p_vaddr as usize,
                memsz: phdr.p_memsz as usize,
                file_offset: phdr.p_offset,
                file_size: filesz,
                perms: perms_from_phdr(&phdr),
                data: &self.bytes[off..end],
            });
        }
        None
    }
}

// ── 内部 helper ──────────────────────────────────────────────────────────────

fn map_machine(m: u16) -> Arch {
    match m {
        EM_LOONGARCH => Arch::LoongArch64,
        EM_RISCV => Arch::Riscv64,
        EM_X86_64 => Arch::X86_64,
        EM_AARCH64 => Arch::Aarch64,
        other => Arch::Unknown(other),
    }
}

fn perms_from_phdr(p: &Phdr64) -> SegmentPerms {
    let mut perms = SegmentPerms::EMPTY;
    if p.p_flags & PF_R != 0 {
        perms = perms.with(SegmentPerms::READ);
    }
    if p.p_flags & PF_W != 0 {
        perms = perms.with(SegmentPerms::WRITE);
    }
    if p.p_flags & PF_X != 0 {
        perms = perms.with(SegmentPerms::EXEC);
    }
    perms
}

fn find_interp<'a>(bytes: &'a [u8], view: &PhdrView<'a>) -> Result<Option<&'a str>, ElfError> {
    for ph in view.iter() {
        if ph.p_type != PT_INTERP {
            continue;
        }
        let off = ph.p_offset as usize;
        let len = ph.p_filesz as usize;
        let end = off
            .checked_add(len)
            .ok_or(ElfError::SegmentOffsetOverflow)?;
        if end > bytes.len() {
            return Err(ElfError::SegmentOffsetOverflow);
        }
        // 去掉尾部 NUL（若存在）；空段视为无效解释器。
        let raw = &bytes[off..end];
        let trimmed = match raw.last() {
            Some(&0) => &raw[..raw.len() - 1],
            _ => raw,
        };
        if trimmed.is_empty() {
            return Err(ElfError::InvalidInterp);
        }
        let s = str::from_utf8(trimmed).map_err(|_| ElfError::InvalidInterp)?;
        return Ok(Some(s));
    }
    Ok(None)
}

fn validate_load_segments(bytes: &[u8], view: &PhdrView<'_>) -> Result<(), ElfError> {
    for ph in view.iter() {
        if ph.p_type != PT_LOAD {
            continue;
        }
        if ph.p_filesz > ph.p_memsz {
            return Err(ElfError::SegmentOffsetOverflow);
        }
        let off = ph.p_offset as usize;
        let filesz = ph.p_filesz as usize;
        let end = off
            .checked_add(filesz)
            .ok_or(ElfError::SegmentOffsetOverflow)?;
        if end > bytes.len() {
            return Err(ElfError::SegmentOffsetOverflow);
        }
        let vaddr = ph.p_vaddr as usize;
        let memsz = ph.p_memsz as usize;
        let _ = vaddr
            .checked_add(memsz)
            .ok_or(ElfError::SegmentOffsetOverflow)?;
    }
    Ok(())
}

fn find_phdr_vaddr(phoff: u64, view: &PhdrView<'_>) -> Option<usize> {
    for ph in view.iter() {
        if ph.p_type == PT_PHDR {
            return Some(ph.p_vaddr as usize);
        }
    }
    for ph in view.iter() {
        if ph.p_type != PT_LOAD {
            continue;
        }
        let start = ph.p_offset;
        let end = ph.p_offset.checked_add(ph.p_filesz)?;
        if phoff >= start && phoff < end {
            return Some((ph.p_vaddr + (phoff - start)) as usize);
        }
    }
    None
}

fn load_vaddr_range(view: &PhdrView<'_>) -> Result<Option<Range<usize>>, ElfError> {
    let mut start = usize::MAX;
    let mut end = 0usize;
    let mut seen = false;
    for ph in view.iter() {
        if ph.p_type != PT_LOAD {
            continue;
        }
        let seg_start = ph.p_vaddr as usize;
        let seg_end = seg_start
            .checked_add(ph.p_memsz as usize)
            .ok_or(ElfError::SegmentOffsetOverflow)?;
        start = start.min(seg_start);
        end = end.max(seg_end);
        seen = true;
    }
    Ok(if seen { Some(start..end) } else { None })
}

fn read_u16(s: &[u8], off: usize) -> u16 {
    let mut buf = [0u8; 2];
    buf.copy_from_slice(&s[off..off + 2]);
    u16::from_le_bytes(buf)
}

fn read_u64(s: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&s[off..off + 8]);
    u64::from_le_bytes(buf)
}
