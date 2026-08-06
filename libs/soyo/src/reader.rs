//! SOYO 文件随机读取契约与可移植资源上限。

use core::convert::Infallible;

use crate::error::{ResourceKind, SoyoError};
use crate::registry::{
    MAX_CAPABILITIES, MAX_DIRECTORY_ENTRIES, MAX_IMPORTS, MAX_RELOCATIONS, MAX_SEGMENTS,
    MAX_STRING_BYTES,
};

const PORTABLE_MAX_TABLE_BYTES: usize = 4_216_928;

pub trait SoyoReadAt {
    type Error;

    fn len(&self) -> u64;

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoyoReadError<E> {
    Format(SoyoError),
    Source(E),
    ResourceExhausted(ResourceKind),
    AllocationFailed(ResourceKind),
}

impl<E> From<SoyoError> for SoyoReadError<E> {
    fn from(error: SoyoError) -> Self {
        match error {
            SoyoError::ResourceExhausted(kind) => Self::ResourceExhausted(kind),
            SoyoError::AllocationFailed(kind) => Self::AllocationFailed(kind),
            other => Self::Format(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoyoReadLimits {
    pub max_directory_entries: u32,
    pub max_table_bytes: usize,
    pub max_string_bytes: usize,
    pub max_segments: u32,
    pub max_imports: u32,
    pub max_capabilities: u32,
    pub max_relocations: u32,
}

impl SoyoReadLimits {
    pub const fn portable() -> Self {
        Self {
            max_directory_entries: MAX_DIRECTORY_ENTRIES,
            max_table_bytes: PORTABLE_MAX_TABLE_BYTES,
            max_string_bytes: MAX_STRING_BYTES,
            max_segments: MAX_SEGMENTS,
            max_imports: MAX_IMPORTS,
            max_capabilities: MAX_CAPABILITIES,
            max_relocations: MAX_RELOCATIONS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SliceSoyoReader<'a> {
    bytes: &'a [u8],
}

impl<'a> SliceSoyoReader<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl SoyoReadAt for SliceSoyoReader<'_> {
    type Error = Infallible;

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error> {
        let start = usize::try_from(offset).expect("解析器不得请求超出 usize 的 slice offset");
        let end = start
            .checked_add(output.len())
            .expect("解析器必须在读取前检查范围溢出");
        let source = self
            .bytes
            .get(start..end)
            .expect("解析器必须在读取前检查文件范围");
        output.copy_from_slice(source);
        Ok(())
    }
}
