//! Linux ELF64 镜像的顶层解析。
//!
//! 对外只暴露 [`LinuxElfImage`]：
//! - `parse(bytes)` 一次性做完 Ehdr/Phdr 校验、切出 interp 字符串；
//! - `segments_typed()` 静态分派迭代 PT_LOAD；
//! - `impl Image` 走 `crate::parse` 的动态分派路径。
//!
//! **零 alloc 于解析本身**：所有缓存字段都是对原字节切片的借用或少量 `Copy`
//! 标量。`Box<dyn Image>` 的堆分配只发生在 [`crate::parse`] 返回时。

use core::convert::TryFrom;
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
        let entry =
            usize::try_from(read_u64(bytes, EHDR_OFF_ENTRY)).map_err(|_| ElfError::InvalidEntry)?;
        let phoff = read_u64(bytes, EHDR_OFF_PHOFF);
        let phentsize = read_u16(bytes, EHDR_OFF_PHENTSIZE);
        let phnum = read_u16(bytes, EHDR_OFF_PHNUM);

        let phdrs = PhdrView::new(bytes, phoff, phentsize, phnum)?;
        validate_load_segments(bytes, &phdrs)?;
        validate_phdr_table(bytes, &phdrs)?;
        validate_entry(entry, &phdrs)?;
        let interp = find_interp(bytes, &phdrs)?;
        let phdr_vaddr = find_phdr_vaddr(&phdrs);
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
            if phdr.p_memsz == 0 {
                continue;
            }
            // 切段数据：[p_offset, p_offset + p_filesz)。
            let off = usize::try_from(phdr.p_offset).ok()?;
            let filesz = usize::try_from(phdr.p_filesz).ok()?;
            let end = off.checked_add(filesz)?;
            debug_assert!(end <= self.bytes.len());
            return Some(Segment {
                vaddr: usize::try_from(phdr.p_vaddr).ok()?,
                memsz: usize::try_from(phdr.p_memsz).ok()?,
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
        let range = file_range_in_image(bytes, ph.p_offset, ph.p_filesz)?;
        let raw = &bytes[range];
        if raw.len() <= 1 || raw.last() != Some(&0) {
            return Err(ElfError::InvalidInterp);
        }
        let path = &raw[..raw.len() - 1];
        if path.is_empty() || path.contains(&0) {
            return Err(ElfError::InvalidInterp);
        }
        let s = str::from_utf8(path).map_err(|_| ElfError::InvalidInterp)?;
        return Ok(Some(s));
    }
    Ok(None)
}

fn validate_load_segments(bytes: &[u8], view: &PhdrView<'_>) -> Result<(), ElfError> {
    for idx in 0..view.count() {
        let ph = view.get(idx).ok_or(ElfError::TruncatedPhdr)?;
        if ph.p_type != PT_LOAD {
            continue;
        }
        validate_load_alignment(&ph)?;
        if ph.p_filesz > ph.p_memsz {
            return Err(ElfError::InvalidSegment);
        }
        let _ = file_range_in_image(bytes, ph.p_offset, ph.p_filesz)?;
        let _ = vaddr_range(ph.p_vaddr, ph.p_memsz)?;
    }
    validate_load_overlaps(view)
}

fn validate_load_alignment(ph: &Phdr64) -> Result<(), ElfError> {
    if ph.p_align <= 1 {
        return Ok(());
    }
    if !ph.p_align.is_power_of_two() {
        return Err(ElfError::InvalidSegment);
    }
    if ph.p_vaddr % ph.p_align != ph.p_offset % ph.p_align {
        return Err(ElfError::InvalidSegment);
    }
    Ok(())
}

fn validate_load_overlaps(view: &PhdrView<'_>) -> Result<(), ElfError> {
    for i in 0..view.count() {
        let left = view.get(i).ok_or(ElfError::TruncatedPhdr)?;
        if left.p_type != PT_LOAD || left.p_memsz == 0 {
            continue;
        }
        let left_range = vaddr_range(left.p_vaddr, left.p_memsz)?;
        for j in (i + 1)..view.count() {
            let right = view.get(j).ok_or(ElfError::TruncatedPhdr)?;
            if right.p_type != PT_LOAD || right.p_memsz == 0 {
                continue;
            }
            let right_range = vaddr_range(right.p_vaddr, right.p_memsz)?;
            if ranges_overlap(&left_range, &right_range) {
                return Err(ElfError::InvalidSegment);
            }
        }
    }
    Ok(())
}

fn validate_phdr_table(bytes: &[u8], view: &PhdrView<'_>) -> Result<(), ElfError> {
    let mut seen = false;
    for ph in view.iter() {
        if ph.p_type != PT_PHDR {
            continue;
        }
        if seen {
            return Err(ElfError::InvalidPhdr);
        }
        seen = true;
        if ph.p_filesz > ph.p_memsz {
            return Err(ElfError::InvalidPhdr);
        }
        let _ = file_range_in_image(bytes, ph.p_offset, ph.p_filesz)?;
        let end = ph
            .p_offset
            .checked_add(ph.p_filesz)
            .ok_or(ElfError::PhdrOffsetOverflow)?;
        if ph.p_offset > view.file_offset() || end < view.file_end() {
            return Err(ElfError::InvalidPhdr);
        }
        let _ = vaddr_range(ph.p_vaddr, ph.p_memsz)?;
    }
    Ok(())
}

fn validate_entry(entry: usize, view: &PhdrView<'_>) -> Result<(), ElfError> {
    for ph in view.iter() {
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 || ph.p_flags & PF_X == 0 {
            continue;
        }
        let range = vaddr_range(ph.p_vaddr, ph.p_memsz)?;
        if range.contains(&entry) {
            return Ok(());
        }
    }
    Err(ElfError::InvalidEntry)
}

fn find_phdr_vaddr(view: &PhdrView<'_>) -> Option<usize> {
    for ph in view.iter() {
        if ph.p_type == PT_PHDR {
            return phdr_table_vaddr_in_segment(
                view.file_offset(),
                view.file_end(),
                ph.p_offset,
                ph.p_filesz,
                ph.p_vaddr,
            );
        }
    }
    for ph in view.iter() {
        if ph.p_type != PT_LOAD {
            continue;
        }
        if let Some(vaddr) = phdr_table_vaddr_in_segment(
            view.file_offset(),
            view.file_end(),
            ph.p_offset,
            ph.p_filesz,
            ph.p_vaddr,
        ) {
            return Some(vaddr);
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
        if ph.p_memsz == 0 {
            continue;
        }
        let range = vaddr_range(ph.p_vaddr, ph.p_memsz)?;
        start = start.min(range.start);
        end = end.max(range.end);
        seen = true;
    }
    Ok(if seen { Some(start..end) } else { None })
}

fn file_range_in_image(bytes: &[u8], offset: u64, size: u64) -> Result<Range<usize>, ElfError> {
    let end = offset
        .checked_add(size)
        .ok_or(ElfError::SegmentOffsetOverflow)?;
    let start = usize::try_from(offset).map_err(|_| ElfError::SegmentOffsetOverflow)?;
    let end = usize::try_from(end).map_err(|_| ElfError::SegmentOffsetOverflow)?;
    if end > bytes.len() {
        return Err(ElfError::SegmentOffsetOverflow);
    }
    Ok(start..end)
}

fn vaddr_range(vaddr: u64, size: u64) -> Result<Range<usize>, ElfError> {
    let end = vaddr
        .checked_add(size)
        .ok_or(ElfError::SegmentOffsetOverflow)?;
    let start = usize::try_from(vaddr).map_err(|_| ElfError::SegmentOffsetOverflow)?;
    let end = usize::try_from(end).map_err(|_| ElfError::SegmentOffsetOverflow)?;
    Ok(start..end)
}

fn phdr_table_vaddr_in_segment(
    table_start: u64,
    table_end: u64,
    seg_offset: u64,
    seg_filesz: u64,
    seg_vaddr: u64,
) -> Option<usize> {
    let seg_end = seg_offset.checked_add(seg_filesz)?;
    if seg_offset > table_start || seg_end < table_end {
        return None;
    }
    let delta = table_start.checked_sub(seg_offset)?;
    usize::try_from(seg_vaddr.checked_add(delta)?).ok()
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
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
