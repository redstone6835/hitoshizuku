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
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::file_like::FileLike;
use crate::flags::VmFlags;

/// 下一个私有匿名 VMA 合并域编号。
///
/// 编号只用于区分仍然存活的映射来源，不进入用户 ABI，也不承担安全身份用途。
/// 64 位地址空间内不可能在编号回绕前保留如此多的 VMA；仍显式跳过零值，避免
/// 回绕瞬间产生保留编号。
static NEXT_ANON_MERGE_DOMAIN: AtomicUsize = AtomicUsize::new(1);

/// 私有匿名 VMA 的合并来源。
///
/// `fork` 前，两个新建匿名映射只要其他属性相同便可像 Linux 一样合并；一旦
/// 映射被父子地址空间共同继承，其来源便被封存。封存后的映射只能与同一来源
/// 分裂出的片段重合并，不能吞并任一进程后来新建的相邻映射。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnonMergeDomain {
    id: usize,
    inherited: bool,
}

impl AnonMergeDomain {
    fn fresh() -> Self {
        loop {
            let id = NEXT_ANON_MERGE_DOMAIN.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return Self {
                    id,
                    inherited: false,
                };
            }
        }
    }

    pub(crate) fn mark_inherited(&mut self) {
        self.inherited = true;
    }

    pub(crate) fn can_merge(self, other: Self) -> bool {
        self.id == other.id || (!self.inherited && !other.inherited)
    }

    /// 比较匿名映射的稳定来源身份。
    ///
    /// `fork` 只会封存合并策略，不改变来源；同一 VMA 的 split/clip 也保留 id。
    /// 新建匿名映射即使暂时允许相邻合并，仍拥有不同 id，可用于缺页锁外准备后
    /// 检测同地址 unmap/remap ABA。
    pub fn same_snapshot_identity(self, other: Self) -> bool {
        self.id == other.id
    }
}

/// 共享匿名映射的稳定 backing 身份。
///
/// 同一对象会被 fork 后的多个 VMA 共同持有；只要仍有任意 VMA 引用该对象，
/// 已产生的共享页就必须保持有效，不能因某个进程先退出而丢失内容。
#[derive(Debug)]
pub struct SharedAnonObject {
    _private: (),
}

impl SharedAnonObject {
    /// 建立一个新的共享匿名对象；对象身份由其 `Arc` 控制块唯一确定。
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for SharedAnonObject {
    fn default() -> Self {
        Self::new()
    }
}

/// VMA 的数据来源。
#[derive(Clone)]
pub enum VmBacking {
    /// 私有匿名映射：缺页时分配一页零填充。对应 `MAP_ANONYMOUS | MAP_PRIVATE`。
    Anon {
        /// 保持分裂/合并与 `fork` 继承边界的内部来源信息。
        merge_domain: AnonMergeDomain,
    },
    /// 共享匿名对象。`object` 标识同一 shared-anon backing，`offset` 对应
    /// `range.start` 在对象内的字节偏移。
    SharedAnon {
        object: Arc<SharedAnonObject>,
        offset: u64,
    },
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

#[kernel_symbols::export]
impl VmArea {
    /// 本 VMA 的字节长度；无效 range 返回 None。
    #[kernel_symbols::export(name = "mm.area.VmArea.len", contract = "kernel.mm.vma@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn len(&self) -> Option<usize> {
        self.range.end.checked_sub(self.range.start)
    }

    /// 公开字段构造后的统一不变式校验。
    ///
    /// 除 `start < end` 外，file/shared-anon offset 与 direct paddr 也必须能覆盖
    /// 整段长度，避免后续 split/clip/fault 路径上的地址加法溢出。
    #[kernel_symbols::export(name = "mm.area.VmArea.is_well_formed", contract = "kernel.mm.vma@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn is_well_formed(&self) -> bool {
        let Some(len) = self.len() else {
            return false;
        };
        len != 0 && self.backing.can_shift(len)
    }

    /// 地址是否落在本 VMA 内（半开区间 `[start, end)`）。
    #[kernel_symbols::export(name = "mm.area.VmArea.contains", contract = "kernel.mm.vma@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn contains(&self, addr: usize) -> bool {
        self.range.contains(&addr)
    }

    /// 本 VMA 与给定区间是否有重叠。
    #[kernel_symbols::export(name = "mm.area.VmArea.overlap", contract = "kernel.mm.vma@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY)]
    pub fn overlap(&self, other: &Range<usize>) -> bool {
        self.range.start < other.end && other.start < self.range.end
    }

    /// 在 `addr` 处劈成两段。`addr` 必须严格落在 `(start, end)` 内，否则返 None。
    /// file backing 的 offset 在右半边按距离自增。
    #[kernel_symbols::export(name = "mm.area.VmArea.split_at", contract = "kernel.mm.vma@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
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
    #[kernel_symbols::export(name = "mm.area.VmArea.clip_to", contract = "kernel.mm.vma@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
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

#[kernel_symbols::export]
impl VmBacking {
    /// 建立一个具有独立合并来源的私有匿名 backing。
    #[kernel_symbols::export(name = "mm.area.VmBacking.anonymous", contract = "kernel.mm.vma-backing@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn anonymous() -> Self {
        Self::Anon {
            merge_domain: AnonMergeDomain::fresh(),
        }
    }

    /// 将私有匿名 backing 标记为已跨 `fork` 继承。
    pub(crate) fn mark_fork_inherited(&mut self) {
        if let Self::Anon { merge_domain } = self {
            merge_domain.mark_inherited();
        }
    }

    /// 判断向后移动 `shift` 字节是否会让 backing 地址或偏移溢出。
    ///
    /// 该检查不构造新 backing，因此不会为只读 VMA 校验增减文件或共享匿名对象的
    /// 引用计数。真正需要移动 backing 的 split/clip 路径仍使用 [`Self::checked_shift`]。
    fn can_shift(&self, shift: usize) -> bool {
        match self {
            Self::Anon { .. } => true,
            Self::SharedAnon { offset, .. } | Self::File { offset, .. } => u64::try_from(shift)
                .ok()
                .and_then(|shift| offset.checked_add(shift))
                .is_some(),
            Self::Direct(base) => base.checked_add(shift).is_some(),
        }
    }

    /// 返回向后移动 `shift` 字节后的 backing；任一地址/offset 加法溢出则返回 None。
    #[kernel_symbols::export(name = "mm.area.VmBacking.checked_shift", contract = "kernel.mm.vma-backing@1", version = 1, capabilities = kernel_symbols::capability::MM_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn checked_shift(&self, shift: usize) -> Option<Self> {
        match self {
            VmBacking::Anon { merge_domain } => Some(VmBacking::Anon {
                merge_domain: *merge_domain,
            }),
            VmBacking::SharedAnon { object, offset } => Some(VmBacking::SharedAnon {
                object: Arc::clone(object),
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

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::{AnonMergeDomain, SharedAnonObject, VmBacking};

    #[test]
    fn backing_shift_validation_checks_offsets_without_building_a_backing() {
        let anon = VmBacking::Anon {
            merge_domain: AnonMergeDomain::fresh(),
        };
        let shared = VmBacking::SharedAnon {
            object: Arc::new(SharedAnonObject::new()),
            offset: u64::MAX - 1,
        };
        let direct = VmBacking::Direct(usize::MAX - 1);

        assert!(anon.can_shift(usize::MAX));
        assert!(shared.can_shift(1));
        assert!(!shared.can_shift(2));
        assert!(direct.can_shift(1));
        assert!(!direct.can_shift(2));
    }
}
