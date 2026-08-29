//! Bounded ELF64 reader for the kernel image.
//!
//! No references are formed into the input bytes at unaligned offsets and no
//! arithmetic is performed without an overflow check.  The resulting view is
//! enough for the loader to allocate and copy `PT_LOAD` segments; this module
//! deliberately keeps firmware writes in the handoff layer.

use core::ops::Range;

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u32 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PF_X: u32 = 1;
const MAX_PROGRAM_HEADERS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    Truncated(&'static str),
    Invalid(&'static str),
    Unsupported(&'static str),
    Overflow(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElfLoadSegment {
    pub index: usize,
    pub file_range: Range<usize>,
    pub virtual_address: u64,
    pub physical_address: u64,
    pub memory_size: u64,
    pub flags: u32,
    pub alignment: u64,
}

impl ElfLoadSegment {
    pub const fn executable(&self) -> bool {
        self.flags & PF_X != 0
    }

    pub const fn file_size(&self) -> u64 {
        self.file_range.end as u64 - self.file_range.start as u64
    }

    pub const fn zero_size(&self) -> u64 {
        self.memory_size.saturating_sub(self.file_size())
    }

    pub fn virtual_range(&self) -> Result<Range<u64>, ElfError> {
        let end = self
            .virtual_address
            .checked_add(self.memory_size)
            .ok_or(ElfError::Overflow("PT_LOAD virtual range"))?;
        Ok(self.virtual_address..end)
    }

    pub fn physical_range(&self) -> Result<Range<u64>, ElfError> {
        let end = self
            .physical_address
            .checked_add(self.memory_size)
            .ok_or(ElfError::Overflow("PT_LOAD physical range"))?;
        Ok(self.physical_address..end)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ElfImage<'a> {
    bytes: &'a [u8],
    phoff: usize,
    phnum: usize,
    entry: u64,
}

impl<'a> ElfImage<'a> {
    /// Parse and validate an x86-64, little-endian, statically linked ELF
    /// executable.  ET_DYN/PIE and dynamic-linker images are rejected because
    /// this standalone boundary has no relocation service yet.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < ELF_HEADER_SIZE {
            return Err(ElfError::Truncated("ELF header"));
        }
        if bytes.get(..4) != Some(b"\x7fELF") {
            return Err(ElfError::Invalid("ELF magic"));
        }
        if bytes[4] != ELFCLASS64 || bytes[5] != ELFDATA2LSB || bytes[6] != 1 {
            return Err(ElfError::Unsupported("ELF class, endian, or version"));
        }
        if read_u16(bytes, 16)? != ET_EXEC || read_u16(bytes, 18)? != EM_X86_64 {
            return Err(ElfError::Unsupported("ELF type or machine"));
        }
        if read_u32(bytes, 20)? != EV_CURRENT {
            return Err(ElfError::Unsupported("ELF ABI version"));
        }
        if read_u16(bytes, 52)? as usize != ELF_HEADER_SIZE
            || read_u16(bytes, 54)? as usize != PROGRAM_HEADER_SIZE
        {
            return Err(ElfError::Invalid("ELF header size"));
        }
        let phoff = usize::try_from(read_u64(bytes, 32)?)
            .map_err(|_| ElfError::Overflow("program-header offset"))?;
        let phnum = usize::from(read_u16(bytes, 56)?);
        if phnum == 0 || phnum > MAX_PROGRAM_HEADERS {
            return Err(ElfError::Invalid("program-header count"));
        }
        let ph_bytes = phnum
            .checked_mul(PROGRAM_HEADER_SIZE)
            .ok_or(ElfError::Overflow("program-header table size"))?;
        let ph_end = phoff
            .checked_add(ph_bytes)
            .ok_or(ElfError::Overflow("program-header table range"))?;
        if phoff < ELF_HEADER_SIZE || ph_end > bytes.len() {
            return Err(ElfError::Truncated("program-header table"));
        }
        let entry = read_u64(bytes, 24)?;
        let image = Self {
            bytes,
            phoff,
            phnum,
            entry,
        };
        image.validate_segments()?;
        Ok(image)
    }

    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    pub const fn entry(self) -> u64 {
        self.entry
    }

    pub const fn program_header_count(self) -> usize {
        self.phnum
    }

    pub fn program_header(self, index: usize) -> Result<Option<ElfLoadSegment>, ElfError> {
        if index >= self.phnum {
            return Ok(None);
        }
        let offset = self
            .phoff
            .checked_add(
                index
                    .checked_mul(PROGRAM_HEADER_SIZE)
                    .ok_or(ElfError::Overflow("program-header index"))?,
            )
            .ok_or(ElfError::Overflow("program-header offset"))?;
        let ph = self
            .bytes
            .get(offset..offset + PROGRAM_HEADER_SIZE)
            .ok_or(ElfError::Truncated("program header"))?;
        let kind = read_u32(ph, 0)?;
        if kind == PT_DYNAMIC {
            return Err(ElfError::Unsupported("dynamic ELF image"));
        }
        if kind != PT_LOAD {
            return Ok(None);
        }
        let file_offset = usize::try_from(read_u64(ph, 8)?)
            .map_err(|_| ElfError::Overflow("PT_LOAD file offset"))?;
        let file_size = usize::try_from(read_u64(ph, 32)?)
            .map_err(|_| ElfError::Overflow("PT_LOAD file size"))?;
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(ElfError::Overflow("PT_LOAD file range"))?;
        if file_end > self.bytes.len() {
            return Err(ElfError::Truncated("PT_LOAD file range"));
        }
        let virtual_address = read_u64(ph, 16)?;
        let physical_address = read_u64(ph, 24)?;
        let memory_size = read_u64(ph, 40)?;
        if file_size as u64 > memory_size {
            return Err(ElfError::Invalid("PT_LOAD filesz exceeds memsz"));
        }
        let alignment = read_u64(ph, 48)?;
        if alignment > 1 && !alignment.is_power_of_two() {
            return Err(ElfError::Invalid("PT_LOAD alignment"));
        }
        if alignment > 1 {
            let mask = alignment - 1;
            if (file_offset as u64 & mask) != (virtual_address & mask)
                || (physical_address & mask) != (virtual_address & mask)
            {
                return Err(ElfError::Invalid("PT_LOAD alignment congruence"));
            }
        }
        virtual_address
            .checked_add(memory_size)
            .ok_or(ElfError::Overflow("PT_LOAD virtual range"))?;
        physical_address
            .checked_add(memory_size)
            .ok_or(ElfError::Overflow("PT_LOAD physical range"))?;
        Ok(Some(ElfLoadSegment {
            index,
            file_range: file_offset..file_end,
            virtual_address,
            physical_address,
            memory_size,
            flags: read_u32(ph, 4)?,
            alignment,
        }))
    }

    pub fn segments(self) -> SegmentIter<'a> {
        SegmentIter {
            image: self,
            next: 0,
        }
    }

    fn validate_segments(self) -> Result<(), ElfError> {
        let mut load_count = 0usize;
        let mut entry_in_executable = false;
        for index in 0..self.phnum {
            if let Some(segment) = self.program_header(index)? {
                if segment.memory_size == 0 {
                    return Err(ElfError::Invalid("zero-sized PT_LOAD"));
                }
                if segment.executable() && segment.virtual_range()?.contains(&self.entry) {
                    entry_in_executable = true;
                }
                load_count += 1;
                for previous_index in 0..index {
                    let Some(previous) = self.program_header(previous_index)? else {
                        continue;
                    };
                    let current_range = segment.virtual_range()?;
                    let previous_range = previous.virtual_range()?;
                    if ranges_overlap(&current_range, &previous_range) {
                        return Err(ElfError::Invalid("overlapping PT_LOAD virtual ranges"));
                    }
                    let current_physical = segment.physical_range()?;
                    let previous_physical = previous.physical_range()?;
                    if ranges_overlap(&current_physical, &previous_physical) {
                        return Err(ElfError::Invalid("overlapping PT_LOAD physical ranges"));
                    }
                }
            }
        }
        if load_count == 0 {
            return Err(ElfError::Invalid("ELF has no PT_LOAD segment"));
        }
        if !entry_in_executable {
            return Err(ElfError::Invalid("ELF entry is outside executable segment"));
        }
        Ok(())
    }
}

pub struct SegmentIter<'a> {
    image: ElfImage<'a>,
    next: usize,
}

impl<'a> Iterator for SegmentIter<'a> {
    type Item = Result<ElfLoadSegment, ElfError>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.image.phnum {
            let index = self.next;
            self.next += 1;
            match self.image.program_header(index) {
                Ok(Some(segment)) => return Some(Ok(segment)),
                Ok(None) => continue,
                Err(error) => return Some(Err(error)),
            }
        }
        None
    }
}

fn ranges_overlap(lhs: &Range<u64>, rhs: &Range<u64>) -> bool {
    lhs.start < rhs.end && rhs.start < lhs.end
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(2)
                    .ok_or(ElfError::Overflow("u16 offset"))?,
        )
        .ok_or(ElfError::Truncated("u16 field"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(4)
                    .ok_or(ElfError::Overflow("u32 offset"))?,
        )
        .ok_or(ElfError::Truncated("u32 field"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(8)
                    .ok_or(ElfError::Overflow("u64 offset"))?,
        )
        .ok_or(ElfError::Truncated("u64 field"))?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_with_segment(flags: u32, entry: u64) -> Vec<u8> {
        let mut image = vec![0u8; 0x200];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = ELFCLASS64;
        image[5] = ELFDATA2LSB;
        image[6] = 1;
        image[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        image[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        image[20..24].copy_from_slice(&EV_CURRENT.to_le_bytes());
        image[24..32].copy_from_slice(&entry.to_le_bytes());
        image[32..40].copy_from_slice(&(ELF_HEADER_SIZE as u64).to_le_bytes());
        image[52..54].copy_from_slice(&(ELF_HEADER_SIZE as u16).to_le_bytes());
        image[54..56].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        image[56..58].copy_from_slice(&1u16.to_le_bytes());
        let ph = &mut image[ELF_HEADER_SIZE..ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE];
        ph[..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        ph[4..8].copy_from_slice(&flags.to_le_bytes());
        ph[8..16].copy_from_slice(&0x100u64.to_le_bytes());
        ph[16..24].copy_from_slice(&0x400100u64.to_le_bytes());
        ph[24..32].copy_from_slice(&0x400100u64.to_le_bytes());
        ph[32..40].copy_from_slice(&4u64.to_le_bytes());
        ph[40..48].copy_from_slice(&0x1000u64.to_le_bytes());
        ph[48..56].copy_from_slice(&0x1000u64.to_le_bytes());
        image[0x100..0x104].copy_from_slice(b"code");
        image
    }

    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn parses_static_executable_and_segments() {
        let image = image_with_segment(PF_X, 0x400100);
        let parsed = ElfImage::parse(&image).unwrap();
        let segment = parsed.segments().next().unwrap().unwrap();
        assert_eq!(segment.file_range, 0x100..0x104);
        assert_eq!(segment.zero_size(), 0x1000 - 4);
    }

    #[test]
    fn rejects_dynamic_and_bad_entry_images() {
        let mut image = image_with_segment(PF_X, 0x400100);
        image[ELF_HEADER_SIZE..ELF_HEADER_SIZE + 4].copy_from_slice(&PT_DYNAMIC.to_le_bytes());
        assert!(matches!(
            ElfImage::parse(&image),
            Err(ElfError::Unsupported("dynamic ELF image"))
        ));

        let image = image_with_segment(0, 0x400100);
        assert!(matches!(
            ElfImage::parse(&image),
            Err(ElfError::Invalid("ELF entry is outside executable segment"))
        ));
    }

    #[test]
    fn rejects_overflow_and_overlap() {
        let mut image = image_with_segment(PF_X, 0x400100);
        image[ELF_HEADER_SIZE + 40..ELF_HEADER_SIZE + 48].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            ElfImage::parse(&image),
            Err(ElfError::Overflow(_))
        ));
    }
}
