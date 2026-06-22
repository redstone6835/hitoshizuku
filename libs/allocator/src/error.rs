//! 分配器错误类型定义。
//!
//! 内核内存系统是一个由多层组成的复杂子系统：引导分配器、伙伴分配器、虚拟地址空间、
//! slab、内核堆、受管堆、注册表……每一层都可能在某些条件下失败。如果把这些失败统一
//! 压缩成一个模糊的布尔值或单一错误码，那么当系统日志中出现"分配失败"时，没有人能
//! 说清楚到底是物理内存耗尽了，还是虚拟地址空间碎片化了，还是某个回调函数尚未绑定。
//!
//! 这个模块的作用，就是把初始化、地址空间操作、分配、释放、所有权查洵等不同阶段可
//! 能出现的失败原因拆成明确的枚举变体。每个变体携带的语义信息足够丰富，使得：
//!
//! - 日志系统可以精确描述"哪一层因为什么原因失败了"；
//! - 测试代码可以断言特定错误类型，而不是笼统地检查"是否失败了"；
//! - 调用方可以根据错误类型决定是重试、回退还是向上传播。
//!
//! 下面的枚举按照"从底层到高层、从通用到专用"的顺序组织。类型之间的转换关系（例如
//! `From<VmemError> for AddressSpaceError`）则保证错误信息在层间传递时不会丢失精度。
//!
//! # 错误类型层次
//!
//! ```text
//! BuddyAllocError / BuddyFreeError  (物理页分配层)
//!   └── VmemError                   (虚拟地址区间管理层)
//!         └── AddressSpaceError      (地址空间协调层)
//!               ├── AllocationError  (通用分配层)
//!               ├── DeallocationError(普通释放路径)
//!               └── PhysicalFreeError(显式物理页释放路径)
//! InitError                          (初始化阶段)
//! RegistryError                      (分配记录注册表)
//! OwnershipError                     (所有权查询)
//! ManagedHandleError                 (受管句柄操作)
//! ```
use crate::buddy::{BuddyAllocError, BuddyFreeError};
use crate::request::{AllocationKind, AllocationRequestError};

/// 分配器初始化阶段错误。
///
/// 这些错误发生在系统 bring-up 的早期阶段。在这个阶段，很多基础设施尚未就绪，因此
/// 错误通常意味着某个必要的前提条件没有被满足。理解这些错误的含义，有助于快速定位
/// 启动过程中卡在哪一步。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `BootNotInitialized` | 引导分配器尚未初始化，但后续初始化步骤需要它 |
/// | `MissingPhysToVirt` | 物理地址到虚拟地址的转换函数未绑定 |
/// | `MissingVirtToPhys` | 虚拟地址到物理地址的转换函数未绑定 |
/// | `MissingKernelHeapRegion` | 内核堆区域描述回调未绑定 |
/// | `MissingKernelHeapMappingOps` | 内核堆的映射/解映射回调未绑定 |
/// | `PhysNotInitialized` | 物理页分配器尚未初始化 |
/// | `InvalidMemoryMap` | 固件/设备树提供的内存映射无效（空或格式错误） |
/// | `MetadataOutOfMemory` | 初始化过程中需要分配元数据，但元数据内存不足 |
/// | `AddressSpaceInitFailed` | 虚拟地址空间初始化失败 |
/// | `ZoneNotInitialized` | Slab 分配器尚未初始化 |
/// | `LargeAllocatorNotInitialized` | 大对象分配器尚未初始化 |
/// | `ManagedAlreadyInitialized` | 受管堆已经被初始化，不能重复初始化 |
/// | `ManagedRegionUnavailable` | 无法为受管堆预留地址空间 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    BootNotInitialized,
    MissingPhysToVirt,
    MissingVirtToPhys,
    MissingKernelHeapRegion,
    MissingKernelHeapMappingOps,
    PhysNotInitialized,
    InvalidMemoryMap,
    MetadataOutOfMemory,
    AddressSpaceInitFailed,
    ZoneNotInitialized,
    LargeAllocatorNotInitialized,
    ManagedAlreadyInitialized,
    ManagedRegionUnavailable,
}

/// 虚拟地址空间层错误。
///
/// `AddressSpaceError` 描述的是 `space` / `vmem` 层在保留地址区间、建立页表映射、
/// 释放映射过程中的失败。它和"物理内存不足"是两回事——很多时候物理页还很充裕，但
/// 虚拟地址空间已经被碎片化到无法满足连续分配请求。把地址空间错误单独枚举，可以
/// 帮助诊断这种"看不见的内存耗尽"。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `NotInitialized` | 地址空间管理器尚未初始化 |
/// | `ArenaNotInitialized` | 某个具体的 arena（如 direct_map）未初始化 |
/// | `OutOfVirtualAddressSpace` | 虚拟地址空间已耗尽，无法满足当前分配 |
/// | `InvalidAlignment` | 请求的对齐要求不合法（不是 2 的幂或低于 quantum） |
/// | `InvalidSize` | 请求的大小为零或非法 |
/// | `InvalidRange` | 指定的地址范围无效（如起始地址大于结束地址） |
/// | `ManagedUnavailable` | 受管堆的虚拟区域尚未建立 |
/// | `NoBackingRange` | 找不到与虚拟地址对应的物理后备区间 |
/// | `PhysicalRangeUnavailable` | 物理页分配失败 |
/// | `PhysicalReleaseFailed` | 物理页释放失败 |
/// | `MappingUnavailable` | 映射回调函数未绑定 |
/// | `MappingFailed` | 页表映射操作失败 |
/// | `UnmappingFailed` | 页表解映射操作失败 |
/// | `MetadataOutOfMemory` | 地址空间元数据分配失败 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressSpaceError {
    NotInitialized,
    ArenaNotInitialized,
    OutOfVirtualAddressSpace,
    InvalidAlignment,
    InvalidSize,
    InvalidRange,
    ManagedUnavailable,
    NoBackingRange,
    PhysicalRangeUnavailable,
    PhysicalReleaseFailed,
    MappingUnavailable,
    MappingFailed,
    UnmappingFailed,
    MetadataOutOfMemory,
}

/// vmem arena 内部错误。
///
/// 这些错误是 `VmemArena` 在管理虚拟地址区间时产生的。它们比 `AddressSpaceError`
/// 更精细，包含了诸如"区间重叠"、"大小不匹配"等仅在 vmem 内部有意义的诊断信息。
/// 上层代码通常通过 `From<VmemError> for AddressSpaceError` 将它们转换为
/// `AddressSpaceError`，但在调试和自检时可以查看原始错误以获得更准确的失败原因。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `NotInitialized` | arena 尚未初始化 |
/// | `InvalidAlignment` | 对齐值不合法 |
/// | `InvalidSize` | 大小为零 |
/// | `InvalidRange` | 地址范围无效（溢出或越界） |
/// | `Overlap` | 尝试添加的 span 与已有 span 重叠 |
/// | `OutOfAddressSpace` | 找不到足够大的连续空闲区间 |
/// | `MetadataOutOfMemory` | 无法为 boundary tag 分配内存 |
/// | `NotAllocated` | 尝试释放一个未分配的地址 |
/// | `SizeMismatch` | 释放时指定的大小与分配时记录的大小不一致 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmemError {
    NotInitialized,
    InvalidAlignment,
    InvalidSize,
    InvalidRange,
    Overlap,
    OutOfAddressSpace,
    MetadataOutOfMemory,
    NotAllocated,
    SizeMismatch { expected: usize, actual: usize },
}

/// 分配记录注册表错误。
///
/// `AllocationRegistry` 是整个分配器的"账本"。它记录每个已分配指针的来源、大小、
/// 类型等信息。当这个账本出现不一致时——例如重复插入同一个指针、查找或删除一个
/// 未知指针——说明要么是调用方传入了错误参数，要么是内部状态已经损坏。这里把
/// 这些情况显式区分，避免释放路径只能通过 `None` 猜测具体原因。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `NotInitialized` | 注册表尚未初始化 |
/// | `DuplicatePointer` | 尝试注册一个已经存在于注册表中的指针 |
/// | `UnknownPointer` | 查洵或删除一个不在注册表中的指针 |
/// | `InvalidRecord` | 提供的记录无效（如 ptr=0） |
/// | `MetadataOutOfMemory` | 注册表节点分配失败 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryError {
    NotInitialized,
    DuplicatePointer,
    UnknownPointer,
    InvalidRecord,
    MetadataOutOfMemory,
}

/// 通用对象分配错误。
///
/// 这是面向调用者的最外层错误类型。它把"分配失败了"这个事实和原因打包在一起。
/// 大多数情况下，调用者只需要知道是"没内存了"还是"还没初始化"，但在需要精确
/// 诊断时，可以通过 `AddressSpace` 变体进一步展开。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `NotInitialized` | 全局分配器尚未激活 |
/// | `InvalidLayout` | 请求的 `Layout` 不合法或超出支持范围 |
/// | `OutOfMemory` | 所有可用的内存都已耗尽 |
/// | `AddressSpace(AddressSpaceError)` | 地址空间层错误（详见 `AddressSpaceError`） |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocationError {
    NotInitialized,
    InvalidLayout,
    OutOfMemory,
    AddressSpace(AddressSpaceError),
}

/// 释放路径错误。
///
/// 和分配错误相比，释放错误更多地反映出"状态不一致"的问题。例如尝试释放一个
/// 不属于任何分配器的指针，或者释放时传入了错误的物理地址。这些错误通常意味着
/// 调用方存在 bug，或者内部账本（注册表）已经损坏。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `UnknownPointer` | 指针不在注册表中，无法确定其来源 |
/// | `InvalidPointer` | 指针无效（如物理地址缺失） |
/// | `AddressSpace(AddressSpaceError)` | 地址空间回收失败 |
/// | `Physical(BuddyFreeError)` | 物理页释放失败 |
/// | `ObjectStillReferenced` | 对象仍有活跃引用，不能释放 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeallocationError {
    UnknownPointer,
    InvalidPointer,
    AddressSpace(AddressSpaceError),
    Physical(BuddyFreeError),
    ObjectStillReferenced,
}

/// 显式物理页释放错误。
///
/// `free_physical()` 的历史签名只能返回布尔值，外部 DMA、页表和未来 LKM 风格扩展无法
/// 判断失败到底来自所有权账本、参数不匹配还是 buddy 层拒绝释放。新的
/// `try_free_physical()` 使用这个类型返回结构化原因；旧布尔接口保留为兼容包装。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `UnknownPointer` | active 后 registry 中不存在该物理页记录 |
/// | `Registry(RegistryError)` | registry 尚未初始化或发生其它账本错误 |
/// | `InvalidRecordKind` | 该地址存在记录，但记录不是 `AllocationKind::Physical` |
/// | `AddressMismatch` | 记录中的物理地址与调用方传入值不一致 |
/// | `OrderMismatch` | 记录中的 buddy order 与调用方传入值不一致 |
/// | `PageSizeMismatch` | 记录中的页粒度与调用方传入值不一致 |
/// | `SizeMismatch` | 记录中的保留大小与调用方传入值不一致 |
/// | `Buddy(BuddyFreeError)` | registry 校验通过，但 buddy 层拒绝实际释放 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalFreeError {
    UnknownPointer,
    Registry(RegistryError),
    InvalidRecordKind { actual: AllocationKind },
    AddressMismatch { expected: usize, actual: usize },
    OrderMismatch { expected: usize, actual: usize },
    PageSizeMismatch { expected: usize, actual: usize },
    SizeMismatch { expected: usize, actual: usize },
    Buddy(BuddyFreeError),
}

/// 所有权查询错误。
///
/// 当调用者想知道"这个指针是谁分配的"时，如果指针不属于任何已知分配器，则返回
/// 此错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnershipError {
    UnknownPointer,
}

/// 受管句柄操作错误。
///
/// 受管堆提供了基于句柄的引用计数和 GC 集成。对句柄、根、字段访问的操作可能因为
/// 各种原因失败，这些原因被统一在这个枚举中。
///
/// # 变体说明
///
/// | 变体 | 含义 |
/// |------|------|
/// | `NotInitialized` | 受管堆尚未初始化 |
/// | `InvalidHandle` | 句柄无效（slot 不匹配或已过期） |
/// | `RootTableFull` | 根表已满，无法添加新根 |
/// | `SlotOutOfRange` | 帧槽索引超出帧容量 |
/// | `InvalidFieldOffset` | 字段偏移不合法或不在 trace descriptor 允许的范围内 |
/// | `InvalidStoredReference` | 存储的引用指向无效对象 |
/// | `NotPinned` | 尝试解钉一个未被钉住的对象 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedHandleError {
    NotInitialized,
    InvalidHandle,
    RootTableFull,
    SlotOutOfRange,
    InvalidFieldOffset,
    InvalidStoredReference,
    NotPinned,
}

// ---------------------------------------------------------------------------
// 错误类型转换实现
// ---------------------------------------------------------------------------

/// 将地址空间错误转换为通用分配错误。
///
/// 如果错误是"未初始化"，则转换为 `AllocationError::NotInitialized`；
/// 否则包装为 `AllocationError::AddressSpace`。
impl From<AddressSpaceError> for AllocationError {
    fn from(err: AddressSpaceError) -> Self {
        match err {
            AddressSpaceError::NotInitialized => AllocationError::NotInitialized,
            _ => AllocationError::AddressSpace(err),
        }
    }
}

/// 将 vmem 内部错误转换为地址空间错误。
///
/// 这个转换保留了所有有诊断价值的信息：
/// - `NotInitialized` → `ArenaNotInitialized`
/// - `InvalidAlignment` → `InvalidAlignment`
/// - `InvalidSize` → `InvalidSize`
/// - `OutOfAddressSpace` → `OutOfVirtualAddressSpace`
/// - `MetadataOutOfMemory` → `MetadataOutOfMemory`
/// - 其余（重叠、未分配、大小不匹配）统一归为 `InvalidRange`
impl From<VmemError> for AddressSpaceError {
    fn from(err: VmemError) -> Self {
        match err {
            VmemError::NotInitialized => AddressSpaceError::ArenaNotInitialized,
            VmemError::InvalidAlignment => AddressSpaceError::InvalidAlignment,
            VmemError::InvalidSize => AddressSpaceError::InvalidSize,
            VmemError::InvalidRange
            | VmemError::Overlap
            | VmemError::NotAllocated
            | VmemError::SizeMismatch { .. } => AddressSpaceError::InvalidRange,
            VmemError::OutOfAddressSpace => AddressSpaceError::OutOfVirtualAddressSpace,
            VmemError::MetadataOutOfMemory => AddressSpaceError::MetadataOutOfMemory,
        }
    }
}

/// 将地址空间错误转换为释放错误。
impl From<AddressSpaceError> for DeallocationError {
    fn from(err: AddressSpaceError) -> Self {
        DeallocationError::AddressSpace(err)
    }
}

/// 将伙伴分配器错误转换为通用分配错误。
///
/// - `NotInitialized` → `AllocationError::NotInitialized`
/// - 无效的 order 或地址 → `AllocationError::InvalidLayout`
/// - 块越界、块不空闲、碎片化、元数据不足 → `AllocationError::OutOfMemory`
impl From<BuddyAllocError> for AllocationError {
    fn from(err: BuddyAllocError) -> Self {
        match err {
            BuddyAllocError::NotInitialized => AllocationError::NotInitialized,
            BuddyAllocError::InvalidOrder | BuddyAllocError::InvalidAddress => {
                AllocationError::InvalidLayout
            }
            BuddyAllocError::BlockOutOfRange
            | BuddyAllocError::BlockNotFree
            | BuddyAllocError::Fragmented
            | BuddyAllocError::MetadataOutOfMemory => AllocationError::OutOfMemory,
        }
    }
}

/// 将请求规范化错误转换为通用分配错误。
impl From<AllocationRequestError> for AllocationError {
    fn from(err: AllocationRequestError) -> Self {
        match err {
            AllocationRequestError::InvalidSize
            | AllocationRequestError::InvalidAlignment
            | AllocationRequestError::SizeOverflow
            | AllocationRequestError::UnsupportedOrder
            | AllocationRequestError::InvalidPlacement => AllocationError::InvalidLayout,
        }
    }
}
