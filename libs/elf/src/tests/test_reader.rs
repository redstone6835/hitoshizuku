//! reader 元数据解析必须与字节切片解析共享同一套校验策略。

extern crate std;

use crate::{
    ElfReadAt, ElfReadError, ElfReadLimits, Image, LinuxElfImage, LinuxElfMetadata, read_linux_elf,
};
use alloc::vec;
use std::cell::RefCell;
use std::vec::Vec;

struct ChunkReader<'a> {
    bytes: &'a [u8],
    max_chunk: usize,
    ranges: RefCell<Vec<(u64, usize)>>,
}

impl<'a> ElfReadAt for ChunkReader<'a> {
    type Error = &'static str;

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), Self::Error> {
        if dst.len() > self.max_chunk {
            return Err("chunk too large");
        }
        let start = usize::try_from(offset).map_err(|_| "offset overflow")?;
        let end = start.checked_add(dst.len()).ok_or("range overflow")?;
        let src = self.bytes.get(start..end).ok_or("out of range")?;
        dst.copy_from_slice(src);
        self.ranges.borrow_mut().push((offset, dst.len()));
        Ok(())
    }
}

fn fixture() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x1200];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&0xf3u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&0x1000u64.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
    let ph = &mut bytes[64..120];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes());
    ph[4..8].copy_from_slice(&5u32.to_le_bytes());
    ph[8..16].copy_from_slice(&0x1000u64.to_le_bytes());
    ph[16..24].copy_from_slice(&0x1000u64.to_le_bytes());
    ph[32..40].copy_from_slice(&0x100u64.to_le_bytes());
    ph[40..48].copy_from_slice(&0x180u64.to_le_bytes());
    ph[48..56].copy_from_slice(&0x1000u64.to_le_bytes());
    bytes[0x1000] = 0xcc;
    bytes
}

fn assert_same_metadata(image: &LinuxElfImage<'_>, metadata: &LinuxElfMetadata) {
    assert_eq!(image.entry(), metadata.entry());
    assert_eq!(image.arch(), metadata.arch());
    assert_eq!(image.is_pie(), metadata.is_pie());
    assert_eq!(image.interpreter(), metadata.interpreter().as_deref());
    assert_eq!(image.phdr_vaddr(), metadata.program_header_vaddr());
    assert_eq!(image.phdr_entry_size(), metadata.program_header_entry_size() as usize);
    assert_eq!(image.phdr_count(), metadata.program_header_count() as usize);
    assert_eq!(image.load_vaddr_range(), metadata.load_range());
    let image_segments: Vec<_> = image
        .segments_typed()
        .map(|segment| (segment.vaddr, segment.memsz, segment.file_offset, segment.file_size))
        .collect();
    let metadata_segments: Vec<_> = metadata
        .load_segments()
        .iter()
        .map(|segment| (segment.vaddr, segment.mem_size, segment.file_offset, segment.file_size))
        .collect();
    assert_eq!(image_segments, metadata_segments);
}

#[test]
fn reader_and_slice_parsers_produce_identical_metadata() {
    let bytes = fixture();
    let image = LinuxElfImage::parse(&bytes).expect("slice parse");
    let mut reader = ChunkReader {
        bytes: &bytes,
        max_chunk: 64,
        ranges: RefCell::new(Vec::new()),
    };
    let metadata = read_linux_elf(&mut reader, ElfReadLimits::default()).expect("reader parse");
    assert_same_metadata(&image, &metadata);
    assert!(reader.ranges.borrow().iter().all(|(offset, len)| {
        offset.checked_add(*len as u64).is_some_and(|end| end <= bytes.len() as u64)
    }));
    assert!(!reader.ranges.borrow().iter().any(|(offset, _)| *offset >= 0x1000));
}

#[test]
fn reader_source_errors_are_not_reported_as_format_errors() {
    let bytes = fixture();
    let mut reader = ChunkReader {
        bytes: &bytes,
        max_chunk: 8,
        ranges: RefCell::new(Vec::new()),
    };
    let error = read_linux_elf(&mut reader, ElfReadLimits::default()).expect_err("source error");
    assert!(matches!(error, ElfReadError::Source("chunk too large")));
}

#[test]
fn reader_and_slice_parsers_share_rejection_categories() {
    for (name, offset, value, expected) in [
        ("invalid entry", 24usize, 0xdead_u64, crate::ElfError::InvalidEntry),
        (
            "invalid alignment",
            64 + 48,
            3u64,
            crate::ElfError::InvalidSegment,
        ),
        (
            "misaligned program header offset",
            32,
            65u64,
            crate::ElfError::MisalignedPhoff,
        ),
    ] {
        let mut bytes = fixture();
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        let slice_error = LinuxElfImage::parse(&bytes).err().unwrap_or_else(|| panic!("{name}"));
        assert_eq!(slice_error, expected);
        let reader = ChunkReader {
            bytes: &bytes,
            max_chunk: 64,
            ranges: RefCell::new(Vec::new()),
        };
        assert_eq!(
            read_linux_elf(&reader, ElfReadLimits::default()).expect_err(name),
            ElfReadError::Format(expected)
        );
    }
}
