//! 由规范元数据支撑的借用型 Linux ELF64 镜像视图。

use alloc::boxed::Box;
use core::ops::Range;

use crate::error::ElfError;
use crate::image::Image;
use crate::reader::{ElfReadError, ElfReadLimits, SliceReader};
use crate::types::{AddressWidth, Arch, Segment};

use super::metadata::{ElfFileType, ElfLoadSegment, LinuxElfMetadata, read_linux_elf};

/// 已解析的 ELF 镜像。校验与元数据提取全部委托给 [`read_linux_elf`]，
/// 本类型只负责附加借用的段内容切片。
pub struct LinuxElfImage<'a> {
    bytes: &'a [u8],
    metadata: LinuxElfMetadata,
}

impl<'a> LinuxElfImage<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        let source = SliceReader::new(bytes);
        let metadata = read_linux_elf(&source, ElfReadLimits::default()).map_err(format_error)?;
        Ok(Self { bytes, metadata })
    }

    pub fn segments_typed<'b>(&'b self) -> LinuxSegmentIter<'a, 'b> {
        LinuxSegmentIter {
            bytes: self.bytes,
            segments: self.metadata.load_segments(),
            cursor: 0,
        }
    }

    pub fn metadata(&self) -> &LinuxElfMetadata {
        &self.metadata
    }

    pub fn raw_type(&self) -> u16 {
        match self.metadata.file_type() {
            ElfFileType::Exec => 2,
            ElfFileType::Dyn => 3,
        }
    }
}

impl<'a> Image<'a> for LinuxElfImage<'a> {
    fn entry(&self) -> usize {
        self.metadata.entry()
    }

    fn arch(&self) -> Arch {
        self.metadata.arch()
    }

    fn class(&self) -> AddressWidth {
        AddressWidth::Bits64
    }

    fn is_pie(&self) -> bool {
        self.metadata.is_pie()
    }

    fn interpreter(&self) -> Option<&str> {
        self.metadata.interpreter()
    }

    fn segments<'b>(&'b self) -> Box<dyn Iterator<Item = Segment<'a>> + 'b>
    where
        'a: 'b,
    {
        Box::new(self.segments_typed())
    }

    fn format_name(&self) -> &'static str {
        "linux-elf64"
    }

    fn phdr_vaddr(&self) -> Option<usize> {
        self.metadata.program_header_vaddr()
    }

    fn phdr_entry_size(&self) -> usize {
        self.metadata.program_header_entry_size() as usize
    }

    fn phdr_count(&self) -> usize {
        self.metadata.program_header_count() as usize
    }

    fn load_vaddr_range(&self) -> Option<Range<usize>> {
        self.metadata.load_range()
    }
}

pub struct LinuxSegmentIter<'a, 'b> {
    bytes: &'a [u8],
    segments: &'b [ElfLoadSegment],
    cursor: usize,
}

impl<'a, 'b> Iterator for LinuxSegmentIter<'a, 'b> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let segment = self.segments.get(self.cursor)?;
        self.cursor += 1;
        let start = usize::try_from(segment.file_offset).ok()?;
        let end = start.checked_add(segment.file_size)?;
        debug_assert!(end <= self.bytes.len());
        Some(Segment {
            vaddr: segment.vaddr,
            memsz: segment.mem_size,
            file_offset: segment.file_offset,
            file_size: segment.file_size,
            perms: segment.permissions,
            data: &self.bytes[start..end],
        })
    }
}

fn format_error(error: ElfReadError<core::convert::Infallible>) -> ElfError {
    match error {
        ElfReadError::Format(error) => error,
        ElfReadError::ResourceExhausted => ElfError::ResourceExhausted,
        ElfReadError::Source(error) => match error {},
    }
}
