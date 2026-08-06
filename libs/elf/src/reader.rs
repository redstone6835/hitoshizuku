//! 仅用于 ELF 元数据解析的读取器抽象。

use crate::error::ElfError;

/// 能够读取精确字节范围、但不暴露底层存储的输入源。
pub trait ElfReadAt {
    type Error;

    fn len(&self) -> u64;
    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), Self::Error>;
}

/// 在分配元数据缓冲区前应用的资源上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfReadLimits {
    pub max_program_header_bytes: usize,
    pub max_interpreter_bytes: usize,
    pub max_dynamic_bytes: usize,
}

impl Default for ElfReadLimits {
    fn default() -> Self {
        Self {
            max_program_header_bytes: 256 * 1024,
            max_interpreter_bytes: 4096,
            max_dynamic_bytes: 1024 * 1024,
        }
    }
}

/// 区分 ELF 格式错误与读取过程中的输入源错误。
#[derive(Debug, PartialEq, Eq)]
pub enum ElfReadError<E> {
    Format(ElfError),
    Source(E),
    ResourceExhausted,
}

impl<E> From<ElfError> for ElfReadError<E> {
    fn from(error: ElfError) -> Self {
        Self::Format(error)
    }
}

/// 规范解析器使用的内存输入源。
pub(crate) struct SliceReader<'a> {
    bytes: &'a [u8],
}

impl<'a> SliceReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl ElfReadAt for SliceReader<'_> {
    type Error = core::convert::Infallible;

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), Self::Error> {
        let start = match usize::try_from(offset) {
            Ok(start) => start,
            Err(_) => unreachable!("slice offset cannot exceed usize"),
        };
        let end = match start.checked_add(dst.len()) {
            Some(end) => end,
            None => unreachable!("slice range cannot overflow after bounds check"),
        };
        dst.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
}

/// 从输入源读取有界范围，并先校验输入源声明的长度。
pub(crate) fn read_range<R: ElfReadAt + ?Sized>(
    source: &R,
    offset: u64,
    dst: &mut [u8],
    out_of_range: ElfError,
) -> Result<(), ElfReadError<R::Error>> {
    let end = offset
        .checked_add(dst.len() as u64)
        .ok_or(ElfReadError::Format(out_of_range))?;
    if end > source.len() {
        return Err(ElfReadError::Format(out_of_range));
    }
    source
        .read_exact_at(offset, dst)
        .map_err(ElfReadError::Source)
}
