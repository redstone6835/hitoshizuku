//! allocator 对动态 ELM 发布的直接内核符号。
//!
//! 这些 shim 保留真实 Rust 方法的接收者和参数 ABI，只为链接器提供稳定入口；每个函数
//! 都直接进入传入的 [`crate::KernelMemorySubsystem`]，不经过 ELM 运行时、授权令牌或
//! 函数表。权限只在镜像装载阶段按描述符能力组裁决。

use crate::buddy;
use crate::{
    AllocStats, AllocationError, AllocationKind, AllocationRecord, AllocatorCapabilities,
    AllocatorHotspotSummary, DeallocationError, KernelMemorySubsystem, MemoryRequest,
    OwnershipError, PhysicalAllocRequest, PhysicalAllocation, PhysicalFreeError,
};

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.is_active",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn is_active(allocator: &KernelMemorySubsystem) -> bool {
    allocator.is_active()
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.capabilities",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn capabilities(allocator: &KernelMemorySubsystem) -> AllocatorCapabilities {
    allocator.capabilities()
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.stats",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn stats(allocator: &KernelMemorySubsystem) -> AllocStats {
    allocator.stats()
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.pressure_level",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn pressure_level(allocator: &KernelMemorySubsystem) -> u8 {
    allocator.pressure_level()
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.hotspot_summary",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn hotspot_summary(allocator: &KernelMemorySubsystem) -> AllocatorHotspotSummary {
    allocator.hotspot_summary()
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.allocate",
    contract = "kernel.allocator.memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn allocate(
    allocator: &KernelMemorySubsystem,
    request: MemoryRequest,
) -> Result<AllocationRecord, AllocationError> {
    allocator.allocate(request)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.deallocate",
    contract = "kernel.allocator.memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn deallocate(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<(), DeallocationError> {
    allocator.deallocate(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.reallocate",
    contract = "kernel.allocator.memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn reallocate(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
    request: MemoryRequest,
) -> Result<AllocationRecord, AllocationError> {
    allocator.reallocate(pointer, request)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.can_reallocate_in_place",
    contract = "kernel.allocator.memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_MEMORY
)]
pub fn can_reallocate_in_place(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
    request: MemoryRequest,
) -> Result<bool, AllocationError> {
    allocator.can_reallocate_in_place(pointer, request)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.query_allocation",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn query_allocation(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<AllocationRecord, OwnershipError> {
    allocator.query_allocation(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.query_tracked_allocation",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn query_tracked_allocation(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<AllocationRecord, OwnershipError> {
    allocator.query_tracked_allocation(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.query_containing_allocation",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn query_containing_allocation(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
    len: usize,
) -> Result<AllocationRecord, OwnershipError> {
    allocator.query_containing_allocation(pointer, len)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.query_owned_range",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn query_owned_range(
    allocator: &KernelMemorySubsystem,
    owner: u64,
    pointer: usize,
    len: usize,
) -> bool {
    allocator.query_owned_range(owner, pointer, len)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.owns_allocation",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn owns_allocation(allocator: &KernelMemorySubsystem, pointer: usize) -> bool {
    allocator.owns_allocation(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.allocation_kind",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn allocation_kind(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<AllocationKind, OwnershipError> {
    allocator.allocation_kind(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.allocation_size",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn allocation_size(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<usize, OwnershipError> {
    allocator.allocation_size(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.allocation_usable_size",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn allocation_usable_size(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<usize, OwnershipError> {
    allocator.allocation_usable_size(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.allocation_alignment",
    contract = "kernel.allocator.query@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_DIAGNOSTIC
)]
pub fn allocation_alignment(
    allocator: &KernelMemorySubsystem,
    pointer: usize,
) -> Result<usize, OwnershipError> {
    allocator.allocation_alignment(pointer)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.allocate_physical",
    contract = "kernel.allocator.physical@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_PHYSICAL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn allocate_physical(
    allocator: &KernelMemorySubsystem,
    request: PhysicalAllocRequest,
) -> Result<PhysicalAllocation, buddy::BuddyAllocError> {
    allocator.allocate_physical(request)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.free_physical",
    contract = "kernel.allocator.physical@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_PHYSICAL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn free_physical(allocator: &KernelMemorySubsystem, allocation: PhysicalAllocation) -> bool {
    allocator.free_physical(allocation)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.try_free_physical",
    contract = "kernel.allocator.physical@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_PHYSICAL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn try_free_physical(
    allocator: &KernelMemorySubsystem,
    allocation: PhysicalAllocation,
) -> Result<(), PhysicalFreeError> {
    allocator.try_free_physical(allocation)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.query_physical_allocation",
    contract = "kernel.allocator.physical@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_PHYSICAL
)]
pub fn query_physical_allocation(
    allocator: &KernelMemorySubsystem,
    physical_address: usize,
) -> Result<PhysicalAllocation, PhysicalFreeError> {
    allocator.query_physical_allocation(physical_address)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.physical_to_virtual",
    contract = "kernel.allocator.address-translation@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_PHYSICAL
)]
pub fn physical_to_virtual(
    allocator: &KernelMemorySubsystem,
    physical_address: usize,
) -> Option<usize> {
    allocator.physical_to_virtual(physical_address)
}

#[kernel_symbols::export(
    name = "allocator.KernelMemorySubsystem.virtual_to_physical",
    contract = "kernel.allocator.address-translation@1",
    version = 1,
    capabilities = kernel_symbols::capability::ALLOCATOR_PHYSICAL
)]
pub fn virtual_to_physical(
    allocator: &KernelMemorySubsystem,
    virtual_address: usize,
) -> Option<usize> {
    allocator.virtual_to_physical(virtual_address)
}

/// 强制链接器抽取包含 allocator 符号目录的代码生成单元。
pub fn catalog_anchor() -> usize {
    <KernelMemorySubsystem as core::alloc::GlobalAlloc>::alloc as usize
        ^ <KernelMemorySubsystem as core::alloc::GlobalAlloc>::dealloc as usize
        ^ allocate as usize
        ^ allocate_physical as usize
        ^ query_physical_allocation as usize
        ^ physical_to_virtual as usize
        ^ virtual_to_physical as usize
}
