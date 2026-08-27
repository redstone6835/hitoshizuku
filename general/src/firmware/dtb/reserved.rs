//! DT `reserved-memory` 的运行期租用与固定 DMA 池。
//!
//! 启动内存策略已经把静态和动态保留请求解析为最终物理范围；本模块在该快照
//! 之上提供设备驱动可长期持有的拥有型句柄。普通区域按 consumer 路径互斥，
//! `shared-dma-pool` 允许多个 consumer 并发租用，并在固定物理范围内进行无重叠
//! 分配。`no-map` 区域只暴露物理范围，不伪造 CPU 映射；`reusable` 区域在没有
//! 架构回收协议时仍永久留在保留集合中，不会被静默交回通用页分配器。

use alloc::boxed::Box;
use alloc::sync::Arc;
#[cfg(test)]
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use allocator::MemorySegment;
use spin::Mutex as SpinMutex;

use crate::dev::pnp::{PnpError, PnpResource, PnpResourceKind, PnpResourceReleaseError};

use super::{DtbNodeInfo, DtbProviderReference, DtbResolvedReservedMemory};

const SHARED_DMA_POOL_COMPATIBLE: &str = "shared-dma-pool";
const RESERVED_MEMORY_RESOURCE_LABEL: &str = "dt-memory-region";
const RESERVED_DMA_RESOURCE_LABEL: &str = "dt-shared-dma-pool-allocation";

/// 运行期 reserved-memory 操作失败的稳定原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DtbReservedMemoryError {
    /// 启动阶段尚未发布 reserved-memory 快照。
    RegistryUnavailable,
    /// 运行期注册表已经由另一份不同的快照初始化。
    AlreadyInstalled,
    /// 快照中的节点标识、标志或范围不满足安全运行条件。
    MalformedRegion,
    /// 两个不同 reserved-memory 节点覆盖同一物理字节。
    OverlappingRegions,
    /// consumer 路径不在当前拥有型 DT 节点图中。
    ConsumerNotFound,
    /// consumer 或 provider 被 `status` 禁用。
    ProviderUnavailable,
    /// consumer 没有请求指定的 `memory-region` 项。
    ReferenceNotFound,
    /// `memory-region-names` 中存在重复名称，无法唯一选择。
    AmbiguousReference,
    /// 引用是空 phandle、带有参数或缺少已解析 provider 身份。
    MalformedReference,
    /// 引用目标不是已发布的 reserved-memory 节点。
    ProviderNotReservedMemory,
    /// 普通保留区正被另一个 consumer 租用。
    RegionBusy,
    /// 请求的节点不是 `shared-dma-pool`。
    NotSharedDmaPool,
    /// 分配长度为零。
    InvalidSize,
    /// 对齐不是非零 2 次幂。
    InvalidAlignment,
    /// 固定 DMA 池没有满足长度与对齐的连续空洞。
    PoolExhausted,
    /// 句柄已经失效或不属于对应区域。
    LeaseNotFound,
    /// 租用仍有未释放的池内分配。
    LeaseHasAllocations,
    /// 池内分配已经释放或句柄与登记记录不一致。
    AllocationNotFound,
    /// 稳定运行期 ID 空间耗尽。
    IdExhausted,
    /// 扩展运行期记账结构失败。
    OutOfMemory,
}

impl fmt::Display for DtbReservedMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DT reserved-memory error: {self:?}")
    }
}

impl DtbReservedMemoryError {
    /// 将固件资源错误转换为 PnP probe 可直接返回的稳定错误。
    pub const fn into_pnp_error(self) -> PnpError {
        match self {
            Self::OutOfMemory => PnpError::OutOfMemory,
            Self::NotSharedDmaPool => PnpError::unsupported("reserved-memory is not a DMA pool"),
            Self::RegistryUnavailable
            | Self::ConsumerNotFound
            | Self::ReferenceNotFound
            | Self::ProviderNotReservedMemory => PnpError::missing(
                PnpResourceKind::Other("reserved-memory"),
                "DT memory-region is unavailable",
            ),
            Self::ProviderUnavailable => PnpError::missing(
                PnpResourceKind::Other("reserved-memory"),
                "DT memory-region provider is disabled",
            ),
            Self::RegionBusy | Self::PoolExhausted => PnpError::registration_failed(
                PnpResourceKind::Dma,
                "reserved-memory resource is busy or exhausted",
            ),
            Self::AlreadyInstalled
            | Self::MalformedRegion
            | Self::OverlappingRegions
            | Self::AmbiguousReference
            | Self::MalformedReference
            | Self::InvalidSize
            | Self::InvalidAlignment
            | Self::LeaseNotFound
            | Self::LeaseHasAllocations
            | Self::AllocationNotFound
            | Self::IdExhausted => PnpError::malformed(
                PnpResourceKind::Other("reserved-memory"),
                "invalid DT reserved-memory runtime state",
            ),
        }
    }
}

/// 一个 reserved-memory 节点的运行期拥有型描述。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbReservedMemoryDescriptor {
    path: Box<str>,
    purpose: Box<str>,
    phandle: Option<u32>,
    compatible: Box<[Box<str>]>,
    ranges: Box<[MemorySegment]>,
    no_map: bool,
    reusable: bool,
    shared_dma_pool: bool,
}

#[kernel_symbols::export]
impl DtbReservedMemoryDescriptor {
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.path",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.purpose",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.phandle",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn phandle(&self) -> Option<u32> {
        self.phandle
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.compatible",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
    )]
    pub fn compatible(&self) -> &[Box<str>] {
        &self.compatible
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.ranges",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn ranges(&self) -> &[MemorySegment] {
        &self.ranges
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.no_map",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn no_map(&self) -> bool {
        self.no_map
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.reusable",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn reusable(&self) -> bool {
        self.reusable
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.is_shared_dma_pool",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn is_shared_dma_pool(&self) -> bool {
        self.shared_dma_pool
    }

    /// `no-map` 节点没有隐式 CPU 地址；调用方只能消费物理范围或另行显式映射。
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryDescriptor.cpu_accessible",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn cpu_accessible(&self) -> bool {
        !self.no_map
    }
}

impl DtbReservedMemoryDescriptor {
    fn from_resolved(region: &DtbResolvedReservedMemory) -> Result<Self, DtbReservedMemoryError> {
        if region.request.path.is_empty()
            || !region.request.path.starts_with("/reserved-memory/")
            || region.request.purpose.is_empty()
            || (region.request.no_map && region.request.reusable)
        {
            return Err(DtbReservedMemoryError::MalformedRegion);
        }
        let compatible: Vec<Box<str>> = region
            .request
            .compatible
            .iter()
            .map(|value| value.clone().into_boxed_str())
            .collect();
        let shared_dma_pool = compatible
            .iter()
            .any(|value| value.as_ref() == SHARED_DMA_POOL_COMPATIBLE);
        Ok(Self {
            path: region.request.path.clone().into_boxed_str(),
            purpose: region.request.purpose.clone().into_boxed_str(),
            phandle: region.request.phandle,
            compatible: compatible.into_boxed_slice(),
            ranges: normalize_ranges(region.ranges.clone())?,
            no_map: region.request.no_map,
            reusable: region.request.reusable,
            shared_dma_pool,
        })
    }

    #[cfg(test)]
    fn test_descriptor(
        path: &str,
        phandle: u32,
        ranges: Vec<MemorySegment>,
        shared_dma_pool: bool,
        no_map: bool,
        reusable: bool,
    ) -> Self {
        Self {
            path: path.into(),
            purpose: "test".into(),
            phandle: Some(phandle),
            compatible: if shared_dma_pool {
                vec![Box::<str>::from(SHARED_DMA_POOL_COMPATIBLE)].into_boxed_slice()
            } else {
                Box::new([])
            },
            ranges: normalize_ranges(ranges).unwrap(),
            no_map,
            reusable,
            shared_dma_pool,
        }
    }
}

/// reserved-memory 注册表的只读运行期状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtbReservedMemoryRuntimeSnapshot {
    pub descriptor: DtbReservedMemoryDescriptor,
    pub active_leases: usize,
    pub active_consumers: usize,
    pub pool_allocations: usize,
    pub pool_allocated_bytes: usize,
}

#[derive(Debug)]
struct LeaseRecord {
    id: u64,
    consumer_path: Box<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoolAllocationRecord {
    id: u64,
    lease_id: u64,
    range: MemorySegment,
}

#[derive(Debug, Default)]
struct FixedPool {
    allocations: Vec<PoolAllocationRecord>,
}

#[derive(Debug)]
struct RuntimeRegion {
    descriptor: DtbReservedMemoryDescriptor,
    leases: Vec<LeaseRecord>,
    pool: Option<FixedPool>,
}

impl RuntimeRegion {
    fn new(descriptor: DtbReservedMemoryDescriptor) -> Self {
        let pool = descriptor.shared_dma_pool.then(FixedPool::default);
        Self {
            descriptor,
            leases: Vec::new(),
            pool,
        }
    }
}

#[derive(Debug)]
struct RuntimeRegistry {
    installed: bool,
    next_id: u64,
    regions: Vec<RuntimeRegion>,
}

impl RuntimeRegistry {
    const fn new() -> Self {
        Self {
            installed: false,
            next_id: 1,
            regions: Vec::new(),
        }
    }

    fn install(
        &mut self,
        descriptors: Vec<DtbReservedMemoryDescriptor>,
    ) -> Result<(), DtbReservedMemoryError> {
        if self.installed {
            let same = self.regions.len() == descriptors.len()
                && self
                    .regions
                    .iter()
                    .zip(&descriptors)
                    .all(|(current, candidate)| current.descriptor == *candidate);
            return if same {
                Ok(())
            } else {
                Err(DtbReservedMemoryError::AlreadyInstalled)
            };
        }

        validate_descriptors(&descriptors)?;
        self.regions
            .try_reserve(descriptors.len())
            .map_err(|_| DtbReservedMemoryError::OutOfMemory)?;
        self.regions
            .extend(descriptors.into_iter().map(RuntimeRegion::new));
        self.installed = true;
        Ok(())
    }

    fn allocate_id(&mut self) -> Result<u64, DtbReservedMemoryError> {
        if self.next_id == u64::MAX {
            return Err(DtbReservedMemoryError::IdExhausted);
        }
        let id = self.next_id;
        self.next_id += 1;
        Ok(id)
    }

    fn acquire(
        &mut self,
        consumer_path: &str,
        provider_path: &str,
        phandle: u32,
    ) -> Result<LeaseGrant, DtbReservedMemoryError> {
        if !self.installed {
            return Err(DtbReservedMemoryError::RegistryUnavailable);
        }
        let index = self
            .regions
            .iter()
            .position(|region| {
                region.descriptor.path.as_ref() == provider_path
                    && region.descriptor.phandle == Some(phandle)
            })
            .ok_or(DtbReservedMemoryError::ProviderNotReservedMemory)?;
        {
            let region = &mut self.regions[index];
            if !region.descriptor.shared_dma_pool
                && region
                    .leases
                    .iter()
                    .any(|lease| lease.consumer_path.as_ref() != consumer_path)
            {
                return Err(DtbReservedMemoryError::RegionBusy);
            }
            region
                .leases
                .try_reserve(1)
                .map_err(|_| DtbReservedMemoryError::OutOfMemory)?;
        }
        let id = self.allocate_id()?;
        let descriptor = self.regions[index].descriptor.clone();
        self.regions[index].leases.push(LeaseRecord {
            id,
            consumer_path: consumer_path.into(),
        });
        Ok(LeaseGrant { id, descriptor })
    }

    fn release_lease(
        &mut self,
        provider_path: &str,
        lease_id: u64,
    ) -> Result<(), DtbReservedMemoryError> {
        let region = self
            .regions
            .iter_mut()
            .find(|region| region.descriptor.path.as_ref() == provider_path)
            .ok_or(DtbReservedMemoryError::LeaseNotFound)?;
        let index = region
            .leases
            .iter()
            .position(|lease| lease.id == lease_id)
            .ok_or(DtbReservedMemoryError::LeaseNotFound)?;
        if region.pool.as_ref().is_some_and(|pool| {
            pool.allocations
                .iter()
                .any(|allocation| allocation.lease_id == lease_id)
        }) {
            return Err(DtbReservedMemoryError::LeaseHasAllocations);
        }
        region.leases.remove(index);
        Ok(())
    }

    fn allocate_pool(
        &mut self,
        provider_path: &str,
        lease_id: u64,
        size: usize,
        alignment: usize,
    ) -> Result<PoolGrant, DtbReservedMemoryError> {
        if size == 0 {
            return Err(DtbReservedMemoryError::InvalidSize);
        }
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(DtbReservedMemoryError::InvalidAlignment);
        }
        let region_index = self
            .regions
            .iter()
            .position(|region| region.descriptor.path.as_ref() == provider_path)
            .ok_or(DtbReservedMemoryError::LeaseNotFound)?;
        let range = {
            let region = &self.regions[region_index];
            if !region.leases.iter().any(|lease| lease.id == lease_id) {
                return Err(DtbReservedMemoryError::LeaseNotFound);
            }
            let pool = region
                .pool
                .as_ref()
                .ok_or(DtbReservedMemoryError::NotSharedDmaPool)?;
            first_pool_fit(
                &region.descriptor.ranges,
                &pool.allocations,
                size,
                alignment,
            )
            .ok_or(DtbReservedMemoryError::PoolExhausted)?
        };
        self.regions[region_index]
            .pool
            .as_mut()
            .expect("a checked shared DMA pool remains a pool")
            .allocations
            .try_reserve(1)
            .map_err(|_| DtbReservedMemoryError::OutOfMemory)?;
        let id = self.allocate_id()?;
        let allocations = &mut self.regions[region_index]
            .pool
            .as_mut()
            .expect("a checked shared DMA pool remains a pool")
            .allocations;
        allocations.push(PoolAllocationRecord {
            id,
            lease_id,
            range,
        });
        allocations.sort_unstable_by_key(|allocation| (allocation.range.start, allocation.id));
        Ok(PoolGrant { id, range })
    }

    fn free_pool(
        &mut self,
        provider_path: &str,
        lease_id: u64,
        allocation_id: u64,
        range: MemorySegment,
    ) -> Result<(), DtbReservedMemoryError> {
        let pool = self
            .regions
            .iter_mut()
            .find(|region| region.descriptor.path.as_ref() == provider_path)
            .and_then(|region| region.pool.as_mut())
            .ok_or(DtbReservedMemoryError::AllocationNotFound)?;
        let index = pool
            .allocations
            .iter()
            .position(|allocation| {
                allocation.id == allocation_id
                    && allocation.lease_id == lease_id
                    && allocation.range == range
            })
            .ok_or(DtbReservedMemoryError::AllocationNotFound)?;
        pool.allocations.remove(index);
        Ok(())
    }

    fn snapshot(&self) -> Vec<DtbReservedMemoryRuntimeSnapshot> {
        self.regions
            .iter()
            .map(|region| {
                let active_consumers = region
                    .leases
                    .iter()
                    .enumerate()
                    .filter(|(index, lease)| {
                        !region.leases[..*index]
                            .iter()
                            .any(|seen| seen.consumer_path == lease.consumer_path)
                    })
                    .count();
                let (pool_allocations, pool_allocated_bytes) = region
                    .pool
                    .as_ref()
                    .map(|pool| {
                        (
                            pool.allocations.len(),
                            pool.allocations.iter().fold(0usize, |total, allocation| {
                                total.saturating_add(allocation.range.size)
                            }),
                        )
                    })
                    .unwrap_or((0, 0));
                DtbReservedMemoryRuntimeSnapshot {
                    descriptor: region.descriptor.clone(),
                    active_leases: region.leases.len(),
                    active_consumers,
                    pool_allocations,
                    pool_allocated_bytes,
                }
            })
            .collect()
    }
}

#[derive(Debug)]
struct LeaseGrant {
    id: u64,
    descriptor: DtbReservedMemoryDescriptor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PoolGrant {
    id: u64,
    range: MemorySegment,
}

static RESERVED_MEMORY_RUNTIME: SpinMutex<RuntimeRegistry> = SpinMutex::new(RuntimeRegistry::new());

fn install_runtime_from_snapshot() -> Result<(), DtbReservedMemoryError> {
    if RESERVED_MEMORY_RUNTIME.lock().installed {
        return Ok(());
    }
    let regions = super::reserved_memory_snapshot();
    if regions.is_empty() {
        return Err(DtbReservedMemoryError::RegistryUnavailable);
    }
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve(regions.len())
        .map_err(|_| DtbReservedMemoryError::OutOfMemory)?;
    for region in &regions {
        descriptors.push(DtbReservedMemoryDescriptor::from_resolved(region)?);
    }
    RESERVED_MEMORY_RUNTIME.lock().install(descriptors)
}

/// 在启动 reserved-memory 快照发布前完成运行期注册表校验与安装。
pub(super) fn install_reserved_memory_runtime(
    regions: &[DtbResolvedReservedMemory],
) -> Result<(), DtbReservedMemoryError> {
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve(regions.len())
        .map_err(|_| DtbReservedMemoryError::OutOfMemory)?;
    for region in regions {
        descriptors.push(DtbReservedMemoryDescriptor::from_resolved(region)?);
    }
    RESERVED_MEMORY_RUNTIME.lock().install(descriptors)
}

fn acquire_reference(
    consumer_path: &str,
    reference: &DtbProviderReference,
) -> Result<DtbReservedMemoryHandle, DtbReservedMemoryError> {
    if !reference.args.is_empty() || reference.phandle == 0 {
        return Err(DtbReservedMemoryError::MalformedReference);
    }
    match reference.provider_available {
        Some(true) => {}
        Some(false) => return Err(DtbReservedMemoryError::ProviderUnavailable),
        None => return Err(DtbReservedMemoryError::MalformedReference),
    }
    let provider_path = reference
        .provider_path
        .as_deref()
        .ok_or(DtbReservedMemoryError::MalformedReference)?;
    install_runtime_from_snapshot()?;
    let grant =
        RESERVED_MEMORY_RUNTIME
            .lock()
            .acquire(consumer_path, provider_path, reference.phandle)?;
    Ok(DtbReservedMemoryHandle {
        lease: Arc::new(ReservedMemoryLease {
            id: grant.id,
            consumer_path: consumer_path.into(),
            descriptor: grant.descriptor,
        }),
    })
}

fn memory_region_references(node: &DtbNodeInfo) -> impl Iterator<Item = &DtbProviderReference> {
    node.bindings
        .references
        .iter()
        .filter(|reference| reference.property.as_ref() == "memory-region")
}

fn memory_region_reference_by_name<'a>(
    references: &'a [DtbProviderReference],
    name: &str,
) -> Result<&'a DtbProviderReference, DtbReservedMemoryError> {
    let mut matching = references.iter().filter(|reference| {
        reference.property.as_ref() == "memory-region" && reference.name.as_deref() == Some(name)
    });
    let reference = matching
        .next()
        .ok_or(DtbReservedMemoryError::ReferenceNotFound)?;
    if matching.next().is_some() {
        return Err(DtbReservedMemoryError::AmbiguousReference);
    }
    Ok(reference)
}

/// 按 consumer 绝对路径和 `memory-region-names` 名称租用保留区。
#[kernel_symbols::export(
    name = "general.firmware.dtb.acquire_memory_region_by_name",
    contract = "kernel.general.reserved-memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn acquire_memory_region_by_name(
    consumer_path: &str,
    name: &str,
) -> Result<DtbReservedMemoryHandle, DtbReservedMemoryError> {
    let node = consumer_node(consumer_path)?;
    let reference = memory_region_reference_by_name(&node.bindings.references, name)?;
    acquire_reference(consumer_path, reference)
}

/// 按 consumer 绝对路径和 `memory-region` 中的零基下标租用保留区。
#[kernel_symbols::export(
    name = "general.firmware.dtb.acquire_memory_region_by_index",
    contract = "kernel.general.reserved-memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn acquire_memory_region_by_index(
    consumer_path: &str,
    index: usize,
) -> Result<DtbReservedMemoryHandle, DtbReservedMemoryError> {
    let node = consumer_node(consumer_path)?;
    let reference = memory_region_references(&node)
        .nth(index)
        .ok_or(DtbReservedMemoryError::ReferenceNotFound)?;
    acquire_reference(consumer_path, reference)
}

fn consumer_node(consumer_path: &str) -> Result<DtbNodeInfo, DtbReservedMemoryError> {
    let node =
        super::node_by_path(consumer_path).ok_or(DtbReservedMemoryError::ConsumerNotFound)?;
    if !node.enabled {
        return Err(DtbReservedMemoryError::ProviderUnavailable);
    }
    Ok(node)
}

/// 返回全部保留区的租用和池内分配诊断快照。
#[kernel_symbols::export(
    name = "general.firmware.dtb.reserved_memory_runtime_snapshot",
    contract = "kernel.general.reserved-memory@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn reserved_memory_runtime_snapshot()
-> Result<Vec<DtbReservedMemoryRuntimeSnapshot>, DtbReservedMemoryError> {
    install_runtime_from_snapshot()?;
    Ok(RESERVED_MEMORY_RUNTIME.lock().snapshot())
}

#[derive(Debug)]
struct ReservedMemoryLease {
    id: u64,
    consumer_path: Box<str>,
    descriptor: DtbReservedMemoryDescriptor,
}

impl Drop for ReservedMemoryLease {
    fn drop(&mut self) {
        if let Err(error) = RESERVED_MEMORY_RUNTIME
            .lock()
            .release_lease(&self.descriptor.path, self.id)
        {
            log::error!(
                "[dtb] failed to release reserved-memory lease {} for {}: {:?}",
                self.id,
                self.consumer_path,
                error
            );
        }
    }
}

/// consumer 对一个 `memory-region` 引用的唯一拥有型租用句柄。
///
/// 该类型故意不实现 `Clone`；若同一设备需要多份所有权，应再次 acquire，使运行期
/// 引用计数可观察。池内分配会在内部保持租用，因而不会出现先释放区域再释放 buffer。
pub struct DtbReservedMemoryHandle {
    lease: Arc<ReservedMemoryLease>,
}

impl fmt::Debug for DtbReservedMemoryHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DtbReservedMemoryHandle")
            .field("id", &self.lease.id)
            .field("consumer_path", &self.lease.consumer_path)
            .field("descriptor", &self.lease.descriptor)
            .finish()
    }
}

#[kernel_symbols::export]
impl Drop for DtbReservedMemoryHandle {
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryHandle.drop",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        // 字段 drop glue 会释放 Arc，并由最后一个 ReservedMemoryLease 归还租用。
    }
}

#[kernel_symbols::export]
impl DtbReservedMemoryHandle {
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryHandle.id",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn id(&self) -> u64 {
        self.lease.id
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryHandle.consumer_path",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn consumer_path(&self) -> &str {
        &self.lease.consumer_path
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryHandle.descriptor",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn descriptor(&self) -> &DtbReservedMemoryDescriptor {
        &self.lease.descriptor
    }

    /// 从引用的 `shared-dma-pool` 固定范围内分配一个物理连续区间。
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryHandle.allocate",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn allocate(
        &self,
        size: usize,
        alignment: usize,
    ) -> Result<DtbReservedDmaAllocation, DtbReservedMemoryError> {
        let grant = RESERVED_MEMORY_RUNTIME.lock().allocate_pool(
            &self.lease.descriptor.path,
            self.lease.id,
            size,
            alignment,
        )?;
        Ok(DtbReservedDmaAllocation {
            allocation: Some(OwnedPoolAllocation {
                provider_path: self.lease.descriptor.path.clone(),
                lease_id: self.lease.id,
                allocation_id: grant.id,
                range: grant.range,
                active: true,
            }),
            lease: Arc::clone(&self.lease),
        })
    }

    /// 在常驻内核侧构造 PnP-owned trait object，避免 ELM 链接私有 vtable。
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedMemoryHandle.boxed_pnp_resource",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn boxed_pnp_resource(self) -> Box<dyn PnpResource> {
        Box::new(self)
    }
}

impl PnpResource for DtbReservedMemoryHandle {
    fn kind(&self) -> PnpResourceKind {
        if self.lease.descriptor.shared_dma_pool {
            PnpResourceKind::Dma
        } else {
            PnpResourceKind::Other("reserved-memory")
        }
    }

    fn label(&self) -> &'static str {
        RESERVED_MEMORY_RESOURCE_LABEL
    }

    fn identity(&self) -> Option<u64> {
        Some(self.lease.id)
    }

    fn release(self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        drop(self);
        Ok(())
    }
}

#[derive(Debug)]
struct OwnedPoolAllocation {
    provider_path: Box<str>,
    lease_id: u64,
    allocation_id: u64,
    range: MemorySegment,
    active: bool,
}

impl OwnedPoolAllocation {
    fn release(&mut self) -> Result<(), DtbReservedMemoryError> {
        if !self.active {
            return Err(DtbReservedMemoryError::AllocationNotFound);
        }
        RESERVED_MEMORY_RUNTIME.lock().free_pool(
            &self.provider_path,
            self.lease_id,
            self.allocation_id,
            self.range,
        )?;
        self.active = false;
        Ok(())
    }
}

impl Drop for OwnedPoolAllocation {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.release()
        {
            log::error!(
                "[dtb] failed to free reserved DMA allocation {}: {:?}",
                self.allocation_id,
                error
            );
        }
    }
}

/// `shared-dma-pool` 中一个物理连续分配的唯一拥有型句柄。
pub struct DtbReservedDmaAllocation {
    // 字段按此顺序销毁，保证先归还池内区间，再递减 region 租用引用。
    allocation: Option<OwnedPoolAllocation>,
    lease: Arc<ReservedMemoryLease>,
}

impl fmt::Debug for DtbReservedDmaAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DtbReservedDmaAllocation")
            .field("id", &self.id())
            .field("range", &self.range())
            .field("provider_path", &self.lease.descriptor.path)
            .finish()
    }
}

#[kernel_symbols::export]
impl Drop for DtbReservedDmaAllocation {
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.drop",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        // 字段按声明顺序析构：先归还 pool allocation，再释放 region lease。
    }
}

#[kernel_symbols::export]
impl DtbReservedDmaAllocation {
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.id",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn id(&self) -> u64 {
        self.allocation
            .as_ref()
            .map_or(0, |allocation| allocation.allocation_id)
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.range",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn range(&self) -> MemorySegment {
        self.allocation
            .as_ref()
            .map_or(MemorySegment { start: 0, size: 0 }, |allocation| {
                allocation.range
            })
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.no_map",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn no_map(&self) -> bool {
        self.lease.descriptor.no_map
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.cpu_accessible",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE
    )]
    pub fn cpu_accessible(&self) -> bool {
        !self.lease.descriptor.no_map
    }

    /// 立即归还固定池区间；正常 drop 与 PnP remove 也执行同一操作。
    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.free",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn free(mut self) -> Result<(), DtbReservedMemoryError> {
        self.release_in_place()
    }

    #[kernel_symbols::export(
        name = "general.firmware.dtb.DtbReservedDmaAllocation.boxed_pnp_resource",
        contract = "kernel.general.reserved-memory@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_RESOURCE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn boxed_pnp_resource(self) -> Box<dyn PnpResource> {
        Box::new(self)
    }

    fn release_in_place(&mut self) -> Result<(), DtbReservedMemoryError> {
        self.allocation
            .as_mut()
            .ok_or(DtbReservedMemoryError::AllocationNotFound)?
            .release()?;
        self.allocation = None;
        Ok(())
    }
}

impl PnpResource for DtbReservedDmaAllocation {
    fn kind(&self) -> PnpResourceKind {
        PnpResourceKind::Dma
    }

    fn label(&self) -> &'static str {
        RESERVED_DMA_RESOURCE_LABEL
    }

    fn identity(&self) -> Option<u64> {
        Some(self.id())
    }

    fn release(mut self: Box<Self>) -> Result<(), PnpResourceReleaseError> {
        self.release_in_place().map_err(|_| {
            PnpResourceReleaseError::new(
                PnpResourceKind::Dma,
                RESERVED_DMA_RESOURCE_LABEL,
                "reserved DMA allocation release failed",
            )
        })
    }
}

fn normalize_ranges(
    mut ranges: Vec<MemorySegment>,
) -> Result<Box<[MemorySegment]>, DtbReservedMemoryError> {
    if ranges.is_empty() {
        return Err(DtbReservedMemoryError::MalformedRegion);
    }
    for range in &ranges {
        if range.size == 0 || range.start.checked_add(range.size).is_none() {
            return Err(DtbReservedMemoryError::MalformedRegion);
        }
    }
    ranges.sort_unstable_by_key(|range| range.start);
    let mut normalized: Vec<MemorySegment> = Vec::new();
    normalized
        .try_reserve(ranges.len())
        .map_err(|_| DtbReservedMemoryError::OutOfMemory)?;
    for range in ranges {
        if let Some(last) = normalized.last_mut() {
            let last_end = last.start + last.size;
            if range.start <= last_end {
                let end = last_end.max(range.start + range.size);
                last.size = end - last.start;
                continue;
            }
        }
        normalized.push(range);
    }
    Ok(normalized.into_boxed_slice())
}

fn validate_descriptors(
    descriptors: &[DtbReservedMemoryDescriptor],
) -> Result<(), DtbReservedMemoryError> {
    for (index, descriptor) in descriptors.iter().enumerate() {
        if descriptor.path.is_empty()
            || descriptor.ranges.is_empty()
            || (descriptor.no_map && descriptor.reusable)
            || descriptors[..index].iter().any(|previous| {
                previous.path == descriptor.path
                    || (descriptor.phandle.is_some() && previous.phandle == descriptor.phandle)
            })
        {
            return Err(DtbReservedMemoryError::MalformedRegion);
        }
        for previous in &descriptors[..index] {
            if descriptor.ranges.iter().any(|left| {
                previous
                    .ranges
                    .iter()
                    .any(|right| ranges_overlap(*left, *right))
            }) {
                return Err(DtbReservedMemoryError::OverlappingRegions);
            }
        }
    }
    Ok(())
}

fn ranges_overlap(left: MemorySegment, right: MemorySegment) -> bool {
    left.start < right.start + right.size && right.start < left.start + left.size
}

fn first_pool_fit(
    ranges: &[MemorySegment],
    allocations: &[PoolAllocationRecord],
    size: usize,
    alignment: usize,
) -> Option<MemorySegment> {
    for range in ranges {
        let range_end = range.start.checked_add(range.size)?;
        let mut cursor = range.start;
        for allocation in allocations {
            let allocation_end = allocation.range.start.checked_add(allocation.range.size)?;
            if allocation_end <= range.start {
                continue;
            }
            if allocation.range.start >= range_end {
                break;
            }
            let candidate = align_up(cursor, alignment)?;
            if candidate.checked_add(size)? <= allocation.range.start.min(range_end) {
                return Some(MemorySegment {
                    start: candidate,
                    size,
                });
            }
            cursor = cursor.max(allocation_end);
            if cursor >= range_end {
                break;
            }
        }
        let candidate = align_up(cursor, alignment)?;
        if candidate.checked_add(size)? <= range_end {
            return Some(MemorySegment {
                start: candidate,
                size,
            });
        }
    }
    None
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: usize, size: usize) -> MemorySegment {
        MemorySegment { start, size }
    }

    fn registry_with(descriptors: Vec<DtbReservedMemoryDescriptor>) -> RuntimeRegistry {
        let mut registry = RuntimeRegistry::new();
        registry.install(descriptors).unwrap();
        registry
    }

    #[test]
    fn dedicated_region_is_reentrant_for_one_consumer_and_exclusive_between_consumers() {
        let descriptor = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/framebuffer@1000",
            1,
            vec![range(0x1000, 0x1000)],
            false,
            false,
            false,
        );
        let mut registry = registry_with(vec![descriptor]);

        let first = registry
            .acquire("/display@0", "/reserved-memory/framebuffer@1000", 1)
            .unwrap();
        let second = registry
            .acquire("/display@0", "/reserved-memory/framebuffer@1000", 1)
            .unwrap();
        assert!(matches!(
            registry.acquire("/codec@0", "/reserved-memory/framebuffer@1000", 1),
            Err(DtbReservedMemoryError::RegionBusy)
        ));

        registry
            .release_lease("/reserved-memory/framebuffer@1000", first.id)
            .unwrap();
        assert!(matches!(
            registry.acquire("/codec@0", "/reserved-memory/framebuffer@1000", 1),
            Err(DtbReservedMemoryError::RegionBusy)
        ));
        registry
            .release_lease("/reserved-memory/framebuffer@1000", second.id)
            .unwrap();
        assert!(
            registry
                .acquire("/codec@0", "/reserved-memory/framebuffer@1000", 1)
                .is_ok()
        );
    }

    #[test]
    fn shared_dma_pool_allocates_across_consumers_and_reuses_freed_holes() {
        let descriptor = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/pool@1000",
            2,
            vec![range(0x1000, 0x1000)],
            true,
            false,
            false,
        );
        let mut registry = registry_with(vec![descriptor]);
        let first = registry
            .acquire("/net@0", "/reserved-memory/pool@1000", 2)
            .unwrap();
        let second = registry
            .acquire("/storage@0", "/reserved-memory/pool@1000", 2)
            .unwrap();

        let head = registry
            .allocate_pool("/reserved-memory/pool@1000", first.id, 0x180, 0x100)
            .unwrap();
        let aligned = registry
            .allocate_pool("/reserved-memory/pool@1000", second.id, 0x100, 0x200)
            .unwrap();
        assert_eq!(head.range, range(0x1000, 0x180));
        assert_eq!(aligned.range, range(0x1200, 0x100));
        assert_eq!(registry.snapshot()[0].active_consumers, 2);
        assert_eq!(registry.snapshot()[0].pool_allocated_bytes, 0x280);

        registry
            .free_pool("/reserved-memory/pool@1000", first.id, head.id, head.range)
            .unwrap();
        let reused = registry
            .allocate_pool("/reserved-memory/pool@1000", first.id, 0x180, 0x100)
            .unwrap();
        assert_eq!(reused.range, head.range);
    }

    #[test]
    fn pool_free_is_exact_and_lease_cannot_retire_with_live_allocations() {
        let descriptor = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/pool@8000",
            3,
            vec![range(0x8000, 0x1000)],
            true,
            false,
            false,
        );
        let mut registry = registry_with(vec![descriptor]);
        let lease = registry
            .acquire("/dma@0", "/reserved-memory/pool@8000", 3)
            .unwrap();
        let allocation = registry
            .allocate_pool("/reserved-memory/pool@8000", lease.id, 0x100, 0x40)
            .unwrap();
        assert_eq!(
            registry.release_lease("/reserved-memory/pool@8000", lease.id),
            Err(DtbReservedMemoryError::LeaseHasAllocations)
        );
        assert_eq!(
            registry.free_pool(
                "/reserved-memory/pool@8000",
                lease.id,
                allocation.id,
                range(allocation.range.start, allocation.range.size + 1),
            ),
            Err(DtbReservedMemoryError::AllocationNotFound)
        );
        registry
            .free_pool(
                "/reserved-memory/pool@8000",
                lease.id,
                allocation.id,
                allocation.range,
            )
            .unwrap();
        registry
            .release_lease("/reserved-memory/pool@8000", lease.id)
            .unwrap();
    }

    #[test]
    fn non_pool_and_invalid_allocation_requests_fail_closed() {
        let descriptor = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/plain@1000",
            4,
            vec![range(0x1000, 0x1000)],
            false,
            false,
            true,
        );
        let mut registry = registry_with(vec![descriptor]);
        let lease = registry
            .acquire("/device@0", "/reserved-memory/plain@1000", 4)
            .unwrap();
        assert_eq!(
            registry.allocate_pool("/reserved-memory/plain@1000", lease.id, 0x100, 0x40),
            Err(DtbReservedMemoryError::NotSharedDmaPool)
        );
        assert_eq!(
            registry.allocate_pool("/reserved-memory/plain@1000", lease.id, 0, 0x40),
            Err(DtbReservedMemoryError::InvalidSize)
        );
        assert_eq!(
            registry.allocate_pool("/reserved-memory/plain@1000", lease.id, 0x100, 24),
            Err(DtbReservedMemoryError::InvalidAlignment)
        );
    }

    #[test]
    fn no_map_is_physical_only_and_reusable_never_enters_the_general_allocator() {
        let no_map = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/nomap@2000",
            5,
            vec![range(0x2000, 0x1000)],
            true,
            true,
            false,
        );
        let reusable = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/reusable@4000",
            6,
            vec![range(0x4000, 0x1000)],
            true,
            false,
            true,
        );
        assert!(!no_map.cpu_accessible());
        assert!(reusable.cpu_accessible());
        let mut registry = registry_with(vec![no_map, reusable]);
        let lease = registry
            .acquire("/device@0", "/reserved-memory/nomap@2000", 5)
            .unwrap();
        let allocation = registry
            .allocate_pool("/reserved-memory/nomap@2000", lease.id, 0x100, 0x40)
            .unwrap();
        assert_eq!(allocation.range, range(0x2000, 0x100));
        // 运行期快照持续保留 reusable 的完整固定池，没有向 buddy 发布回收入口。
        assert_eq!(
            registry.snapshot()[1].descriptor.ranges(),
            &[range(0x4000, 0x1000)]
        );
    }

    #[test]
    fn malformed_or_overlapping_registry_is_rejected_before_publication() {
        let overlapping_a = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/a@1000",
            7,
            vec![range(0x1000, 0x1000)],
            false,
            false,
            false,
        );
        let overlapping_b = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/b@1800",
            8,
            vec![range(0x1800, 0x1000)],
            false,
            false,
            false,
        );
        let mut registry = RuntimeRegistry::new();
        assert_eq!(
            registry.install(vec![overlapping_a, overlapping_b]),
            Err(DtbReservedMemoryError::OverlappingRegions)
        );

        let mutually_exclusive = DtbReservedMemoryDescriptor::test_descriptor(
            "/reserved-memory/invalid@4000",
            9,
            vec![range(0x4000, 0x1000)],
            true,
            true,
            true,
        );
        assert_eq!(
            registry.install(vec![mutually_exclusive]),
            Err(DtbReservedMemoryError::MalformedRegion)
        );
    }

    #[test]
    fn named_reference_selection_rejects_duplicate_names() {
        let reference = |name: &str| DtbProviderReference {
            property: "memory-region".into(),
            name: Some(name.into()),
            provider: None,
            provider_path: Some("/reserved-memory/pool@1000".into()),
            provider_available: Some(true),
            phandle: 10,
            args: Box::new([]),
        };
        let references = [reference("rx"), reference("tx"), reference("rx")];
        assert_eq!(
            memory_region_reference_by_name(&references, "rx"),
            Err(DtbReservedMemoryError::AmbiguousReference)
        );
        assert_eq!(
            memory_region_reference_by_name(&references, "missing"),
            Err(DtbReservedMemoryError::ReferenceNotFound)
        );
        assert_eq!(
            memory_region_reference_by_name(&references, "tx")
                .unwrap()
                .phandle,
            10
        );
    }
}
