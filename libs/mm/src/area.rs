//! VMA —— 单个虚拟内存区域。
//!
//! 一个 VMA 描述"从 vaddr A 到 vaddr B 的这段，权限是 R/W/X，数据来源是
//! X"。不存物理页映射——那由页表负责；VMA 只描述应有的语义。缺页处理按
//! VMA 的 backing 类别决定如何填充。
//!
//! 地址比较以 `range.start` 为序；`split_at` / `clip_to` 保持 `start < end`
//! 和 backing 覆盖整段不溢出的不变式，空 range 会在插入集合时被拒绝。

use alloc::sync::Arc;
use core::convert::TryFrom;
use core::ops::Range;

use crate::file_like::FileLike;
use crate::flags::VmFlags;

/// VMA 的数据来源。
#[derive(Clone)]
pub enum VmBacking {
    /// 匿名映射：缺页时分配一页零填充。对应 `MAP_ANONYMOUS`。
    Anon,
    /// 共享匿名对象。`id` 标识同一 shared-anon backing，`offset` 对应
    /// `range.start` 在对象内的字节偏移。
    SharedAnon { id: usize, offset: u64 },
    /// 文件映射：缺页时按偏移从文件读取；超出文件长度的尾部零填充。
    File {
        file: Arc<dyn FileLike>,
        /// 文件里对应 `range.start` 的起始偏移（字节）。
        offset: u64,
    },
    /// 直接物理页：整段一次性映射到给定物理基址（连续）。设备 mmio / framebuffer
    /// 用途。缺页本质上不会发生——插入 VMA 时就应该把页表建好。
    Direct(usize),
}

/// 单个 VMA。
#[derive(Clone)]
pub struct VmArea {
    pub range: Range<usize>,
    pub flags: VmFlags,
    pub backing: VmBacking,
}

impl VmArea {
    /// 本 VMA 的字节长度；无效 range 返回 None。
    pub fn len(&self) -> Option<usize> {
        self.range.end.checked_sub(self.range.start)
    }

    /// 公开字段构造后的统一不变式校验。
    ///
    /// 除 `start < end` 外，file/shared-anon offset 与 direct paddr 也必须能覆盖
    /// 整段长度，避免后续 split/clip/fault 路径上的地址加法溢出。
    pub fn is_well_formed(&self) -> bool {
        let Some(len) = self.len() else {
            return false;
        };
        len != 0 && self.backing.checked_shift(len).is_some()
    }

    /// 地址是否落在本 VMA 内（半开区间 `[start, end)`）。
    pub fn contains(&self, addr: usize) -> bool {
        self.range.contains(&addr)
    }

    /// 本 VMA 与给定区间是否有重叠。
    pub fn overlap(&self, other: &Range<usize>) -> bool {
        self.range.start < other.end && other.start < self.range.end
    }

    /// 在 `addr` 处劈成两段。`addr` 必须严格落在 `(start, end)` 内，否则返 None。
    /// file backing 的 offset 在右半边按距离自增。
    pub fn split_at(&self, addr: usize) -> Option<(VmArea, VmArea)> {
        if !self.is_well_formed() || addr <= self.range.start || addr >= self.range.end {
            return None;
        }
        let left = VmArea {
            range: self.range.start..addr,
            flags: self.flags,
            backing: self.backing.clone(),
        };
        let right_backing = self.backing.checked_shift(addr - self.range.start)?;
        let right = VmArea {
            range: addr..self.range.end,
            flags: self.flags,
            backing: right_backing,
        };
        Some((left, right))
    }

    /// 裁剪到给定区间。若裁剪结果非空，按"起点偏移"调整 file / Direct 的
    /// backing；完全无重叠则返 None。
    pub fn clip_to(&self, clip: &Range<usize>) -> Option<VmArea> {
        if !self.is_well_formed() {
            return None;
        }
        let start = self.range.start.max(clip.start);
        let end = self.range.end.min(clip.end);
        if start >= end {
            return None;
        }
        let shift = start - self.range.start;
        let backing = self.backing.checked_shift(shift)?;
        Some(VmArea {
            range: start..end,
            flags: self.flags,
            backing,
        })
    }
}

impl VmBacking {
    /// 返回向后移动 `shift` 字节后的 backing；任一地址/offset 加法溢出则返回 None。
    pub fn checked_shift(&self, shift: usize) -> Option<Self> {
        match self {
            VmBacking::Anon => Some(VmBacking::Anon),
            VmBacking::SharedAnon { id, offset } => Some(VmBacking::SharedAnon {
                id: *id,
                offset: offset.checked_add(u64::try_from(shift).ok()?)?,
            }),
            VmBacking::File { file, offset } => Some(VmBacking::File {
                file: Arc::clone(file),
                offset: offset.checked_add(u64::try_from(shift).ok()?)?,
            }),
            VmBacking::Direct(base) => Some(VmBacking::Direct(base.checked_add(shift)?)),
        }
    }
}
