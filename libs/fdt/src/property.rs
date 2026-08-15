//! FDT 属性的显式类型解码器。
//!
//! FDT 本身不携带属性类型。调用方应依据 binding 选择本模块的字符串、
//! 字符串列表或 cell 解码接口，解析器不会根据长度猜测类型。

use core::str;

use crate::PropertyError;

/// 大端 `u32` cell 迭代器。
#[derive(Clone, Debug)]
pub struct Cells<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Cells<'a> {
    /// 从完整的 32-bit cell 序列构造迭代器。
    pub fn new(bytes: &'a [u8]) -> Result<Self, PropertyError> {
        if !bytes.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidLength {
                actual: bytes.len(),
                expected: None,
            });
        }
        Ok(Self { bytes, cursor: 0 })
    }

    /// 尚未读取的 cell 数。
    #[inline]
    pub fn remaining(&self) -> usize {
        (self.bytes.len() - self.cursor) / 4
    }

    /// 从当前位置读取 `count` 个 cell，并按大端拼接为 `u128`。
    pub fn read_value(&mut self, count: usize) -> Result<u128, PropertyError> {
        if count > 4 {
            return Err(PropertyError::TooManyCells(count));
        }
        if self.remaining() < count {
            return Err(PropertyError::NotEnoughCells {
                requested: count,
                remaining: self.remaining(),
            });
        }

        let end = self.cursor + count * 4;
        let value = decode_cells(&self.bytes[self.cursor..end], count)?;
        self.cursor = end;
        Ok(value)
    }
}

impl Iterator for Cells<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.bytes.get(self.cursor..self.cursor + 4)?;
        self.cursor += 4;
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Cells<'_> {}
impl core::iter::FusedIterator for Cells<'_> {}

/// 将恰好 `count` 个大端 cell 拼接为 `u128`。
///
/// 零个 cell 合法并解码为零；超过四个 cell 无法无损放入 `u128`。
pub fn decode_cells(bytes: &[u8], count: usize) -> Result<u128, PropertyError> {
    if count > 4 {
        return Err(PropertyError::TooManyCells(count));
    }
    let expected = count
        .checked_mul(4)
        .ok_or(PropertyError::TooManyCells(count))?;
    if bytes.len() != expected {
        return Err(PropertyError::InvalidLength {
            actual: bytes.len(),
            expected: Some(expected),
        });
    }

    let mut value = 0u128;
    for chunk in bytes.chunks_exact(4) {
        value = (value << 32) | u32::from_be_bytes(chunk.try_into().unwrap()) as u128;
    }
    Ok(value)
}

/// 已完整验证的 NUL 分隔 UTF-8 字符串列表。
#[derive(Clone, Debug)]
pub struct StringList<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> StringList<'a> {
    /// 校验并借用一份 NUL 分隔 UTF-8 字符串列表。
    pub fn new(bytes: &'a [u8]) -> Result<Self, PropertyError> {
        if bytes.is_empty() {
            return Ok(Self { bytes, cursor: 0 });
        }
        if bytes.last() != Some(&0) {
            return Err(PropertyError::MissingNul);
        }

        let mut cursor = 0;
        while cursor < bytes.len() {
            let relative_end = bytes[cursor..]
                .iter()
                .position(|&byte| byte == 0)
                .ok_or(PropertyError::MissingNul)?;
            str::from_utf8(&bytes[cursor..cursor + relative_end])
                .map_err(|_| PropertyError::InvalidUtf8)?;
            cursor += relative_end + 1;
        }
        Ok(Self { bytes, cursor: 0 })
    }
}

impl<'a> Iterator for StringList<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.bytes.len() {
            return None;
        }
        let relative_end = self.bytes[self.cursor..]
            .iter()
            .position(|&byte| byte == 0)?;
        let value = str::from_utf8(&self.bytes[self.cursor..self.cursor + relative_end]).ok()?;
        self.cursor += relative_end + 1;
        Some(value)
    }
}

impl core::iter::FusedIterator for StringList<'_> {}
