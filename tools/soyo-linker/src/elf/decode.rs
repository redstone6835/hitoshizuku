use std::path::Path;

use super::error::{ElfError, ElfErrorKind};

pub(crate) fn bytes<'a>(
    path: &Path,
    source: &'a [u8],
    offset: u64,
    size: u64,
    kind: ElfErrorKind,
) -> Result<&'a [u8], ElfError> {
    let start = usize::try_from(offset).map_err(|_| ElfError::new(path, kind))?;
    let length = usize::try_from(size).map_err(|_| ElfError::new(path, kind))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| ElfError::new(path, kind))?;
    source
        .get(start..end)
        .ok_or_else(|| ElfError::new(path, kind))
}

pub(crate) fn u16_at(path: &Path, source: &[u8], offset: usize) -> Result<u16, ElfError> {
    let value = source
        .get(offset..offset + 2)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::Truncated))?;
    Ok(u16::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn u32_at(path: &Path, source: &[u8], offset: usize) -> Result<u32, ElfError> {
    let value = source
        .get(offset..offset + 4)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::Truncated))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn u64_at(path: &Path, source: &[u8], offset: usize) -> Result<u64, ElfError> {
    let value = source
        .get(offset..offset + 8)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::Truncated))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn i64_at(path: &Path, source: &[u8], offset: usize) -> Result<i64, ElfError> {
    let value = source
        .get(offset..offset + 8)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::Truncated))?;
    Ok(i64::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn string_at<'a>(
    path: &Path,
    table: &'a [u8],
    offset: u32,
) -> Result<&'a str, ElfError> {
    let start =
        usize::try_from(offset).map_err(|_| ElfError::new(path, ElfErrorKind::InvalidString))?;
    let tail = table
        .get(start..)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::InvalidString))?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::InvalidString))?;
    std::str::from_utf8(&tail[..length])
        .map_err(|_| ElfError::new(path, ElfErrorKind::InvalidString))
}
