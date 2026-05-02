//! ELF Phdr 表的安全切片访问。
//!
//! Phdr 表起点 `e_phoff` 不一定 8 字节对齐——为安全起见这里**不**做 slice
//! cast，而是按 `PHDR_SIZE` 一条一条 `from_le_bytes` 读出来。性能开销
//! 与一次 cast 持平（编译器会把固定 offset 的 `from_le_bytes` 优化为
//! 单条 ld.d）。
//!
//! 对外只暴露 [`PhdrView`]：构造、`count`、`get(i)`、`iter`。避免 `impl
//! Iterator` 关联的不透明类型泄漏到调用者签名里。

use crate::error::ElfError;

use super::raw::{
    PHDR_OFF_ALIGN, PHDR_OFF_FILESZ, PHDR_OFF_FLAGS, PHDR_OFF_MEMSZ, PHDR_OFF_OFFSET,
    PHDR_OFF_PADDR, PHDR_OFF_TYPE, PHDR_OFF_VADDR, PHDR_SIZE, Phdr64,
};

/// 对原始 image 字节流里 phdr 表的只读视图。`Clone` 是 `Copy` 的轻量版本
/// （`&[u8]` + usize + usize），便于 iter 场景多次持有。
#[derive(Clone)]
pub(super) struct PhdrView<'a> {
    /// 整张 phdr 表的字节切片（`phnum * phentsize` 长）。
    table: &'a [u8],
    /// 每条 phdr 的字节数。本 crate 拒绝 != 56。
    entsize: usize,
    count: usize,
}

impl<'a> PhdrView<'a> {
    pub(super) fn new(
        bytes: &'a [u8],
        phoff: u64,
        phentsize: u16,
        phnum: u16,
    ) -> Result<Self, ElfError> {
        if phentsize as usize != PHDR_SIZE {
            return Err(ElfError::TruncatedPhdr);
        }
        let phoff = phoff as usize;
        let total = (phentsize as usize)
            .checked_mul(phnum as usize)
            .ok_or(ElfError::PhdrOffsetOverflow)?;
        let end = phoff
            .checked_add(total)
            .ok_or(ElfError::PhdrOffsetOverflow)?;
        if end > bytes.len() {
            return Err(ElfError::TruncatedPhdr);
        }
        Ok(Self {
            table: &bytes[phoff..end],
            entsize: phentsize as usize,
            count: phnum as usize,
        })
    }

    pub(super) fn count(&self) -> usize {
        self.count
    }

    /// 第 `idx` 条 Phdr。`idx >= count` 返 None。
    pub(super) fn get(&self, idx: usize) -> Option<Phdr64> {
        if idx >= self.count {
            return None;
        }
        let base = idx * self.entsize;
        let s = self.table.get(base..base + PHDR_SIZE)?;
        Some(decode_phdr(s))
    }

    /// 显式游标迭代器；返回的迭代器类型不含 opaque / closure 捕获，
    /// 可被其它迭代器（FilterMap 等）安全包装。
    pub(super) fn iter(&self) -> PhdrIter<'a> {
        PhdrIter {
            view: self.clone(),
            cursor: 0,
        }
    }
}

/// 遍历 [`PhdrView`] 的显式迭代器。
#[derive(Clone)]
pub(super) struct PhdrIter<'a> {
    view: PhdrView<'a>,
    cursor: usize,
}

impl<'a> Iterator for PhdrIter<'a> {
    type Item = Phdr64;
    fn next(&mut self) -> Option<Phdr64> {
        let v = self.view.get(self.cursor)?;
        self.cursor += 1;
        Some(v)
    }
}

fn decode_phdr(s: &[u8]) -> Phdr64 {
    Phdr64 {
        p_type: read_u32(s, PHDR_OFF_TYPE),
        p_flags: read_u32(s, PHDR_OFF_FLAGS),
        p_offset: read_u64(s, PHDR_OFF_OFFSET),
        p_vaddr: read_u64(s, PHDR_OFF_VADDR),
        p_paddr: read_u64(s, PHDR_OFF_PADDR),
        p_filesz: read_u64(s, PHDR_OFF_FILESZ),
        p_memsz: read_u64(s, PHDR_OFF_MEMSZ),
        p_align: read_u64(s, PHDR_OFF_ALIGN),
    }
}

fn read_u32(s: &[u8], off: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&s[off..off + 4]);
    u32::from_le_bytes(buf)
}

fn read_u64(s: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&s[off..off + 8]);
    u64::from_le_bytes(buf)
}
