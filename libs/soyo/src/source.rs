//! 文件范围、有界读取、零 padding 与 canonical digest。

use alloc::vec::Vec;
use core::cmp;

use sha2::{Digest, Sha256};

use crate::error::{MalformedKind, ResourceKind, SoyoError, UntrustedKind};
use crate::metadata::{DirectoryEntry, SoyoHeader};
use crate::reader::{SoyoReadAt, SoyoReadError, SoyoReadLimits};
use crate::registry::TableType;
use crate::wire;

pub(crate) fn verify_hash<R: SoyoReadAt>(
    source: &R,
    header: &SoyoHeader,
    directory: &[DirectoryEntry],
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    if header.build_id != header.content_hash {
        return Err(SoyoError::Untrusted(UntrustedKind::BuildIdMismatch).into());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 4096];
    let signature_range = find_table(directory, TableType::Signature as u16).map(|table| {
        let start = table.file_offset + wire::signature::SIGNATURE as u64;
        start..start + 64
    });
    let mut offset = 0u64;
    while offset < file_size {
        let length = cmp::min(buffer.len() as u64, file_size - offset) as usize;
        read_exact(source, offset, &mut buffer[..length], file_size)?;
        for (index, byte) in buffer[..length].iter_mut().enumerate() {
            let absolute = offset + index as u64;
            if (0x50..0x90).contains(&absolute) {
                *byte = 0;
            }
            if signature_range
                .as_ref()
                .is_some_and(|range| range.contains(&absolute))
            {
                *byte = 0;
            }
        }
        hasher.update(&buffer[..length]);
        offset += length as u64;
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != header.content_hash {
        return Err(SoyoError::Untrusted(UntrustedKind::ContentHashMismatch).into());
    }
    Ok(())
}

pub(crate) fn verify_zero_range<R: SoyoReadAt>(
    source: &R,
    offset: u64,
    size: u64,
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    let mut buffer = [0u8; 4096];
    let mut read_offset = offset;
    let end = offset
        .checked_add(size)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
    while read_offset < end {
        let length = cmp::min(buffer.len() as u64, end - read_offset) as usize;
        read_exact(source, read_offset, &mut buffer[..length], file_size)?;
        if !all_zero(&buffer[..length]) {
            return Err(SoyoError::Malformed(MalformedKind::Padding).into());
        }
        read_offset += length as u64;
    }
    Ok(())
}

pub(crate) fn read_array<R: SoyoReadAt, const N: usize>(
    source: &R,
    offset: u64,
    file_size: u64,
) -> Result<[u8; N], SoyoReadError<R::Error>> {
    let mut bytes = [0; N];
    read_exact(source, offset, &mut bytes, file_size)?;
    Ok(bytes)
}

pub(crate) fn read_bytes<R: SoyoReadAt>(
    source: &R,
    offset: u64,
    size: u64,
    file_size: u64,
    limits: SoyoReadLimits,
) -> Result<Vec<u8>, SoyoReadError<R::Error>> {
    let size = usize::try_from(size)
        .map_err(|_| SoyoReadError::AllocationFailed(ResourceKind::TableBytes))?;
    if size > limits.max_table_bytes {
        return Err(SoyoReadError::ResourceExhausted(ResourceKind::TableBytes));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| SoyoReadError::AllocationFailed(ResourceKind::TableBytes))?;
    bytes.resize(size, 0);
    read_exact(source, offset, &mut bytes, file_size)?;
    Ok(bytes)
}

pub(crate) fn read_exact<R: SoyoReadAt>(
    source: &R,
    offset: u64,
    output: &mut [u8],
    file_size: u64,
) -> Result<(), SoyoReadError<R::Error>> {
    if !range_within(offset, output.len() as u64, file_size) {
        return Err(SoyoError::Malformed(MalformedKind::Range).into());
    }
    source
        .read_exact_at(offset, output)
        .map_err(SoyoReadError::Source)
}

pub(crate) fn find_table(directory: &[DirectoryEntry], table_type: u16) -> Option<&DirectoryEntry> {
    directory
        .binary_search_by_key(&table_type, |entry| entry.table_type)
        .ok()
        .map(|index| &directory[index])
}

pub(crate) fn checked_mul_u64(left: u64, right: u64) -> Result<u64, SoyoError> {
    left.checked_mul(right)
        .ok_or(SoyoError::Malformed(MalformedKind::Range))
}

pub(crate) fn align_up(value: u64, alignment: u64) -> Result<u64, SoyoError> {
    if !alignment.is_power_of_two() {
        return Err(SoyoError::Malformed(MalformedKind::Alignment));
    }
    value
        .checked_add(alignment - 1)
        .map(|sum| sum & !(alignment - 1))
        .ok_or(SoyoError::Malformed(MalformedKind::Range))
}

pub(crate) fn valid_alignment(value: u64) -> bool {
    value.is_power_of_two() && value <= 4096
}

pub(crate) fn range_within(offset: u64, size: u64, file_size: u64) -> bool {
    offset.checked_add(size).is_some_and(|end| end <= file_size)
}

pub(crate) fn all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}

pub(crate) fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("已验证记录尺寸"),
    )
}

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("已验证记录尺寸"),
    )
}

pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("已验证记录尺寸"),
    )
}

pub(crate) fn i64_at(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("已验证记录尺寸"),
    )
}
