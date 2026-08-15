//! 通用 DMA 分配与同步辅助。

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use allocator::{
    KERNEL_ALLOCATOR, MemoryPlacement, PAGE_SIZE, PhysicalAllocError, PhysicalAllocRequest,
    PhysicalAllocation,
};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::mutex::Mutex;

use super::iommu::IommuConsumerLease;
use super::pnp::PnpResource;

/// CPU 与设备之间的 DMA 所有权转移方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    /// CPU 写入缓冲区，随后设备读取。
    ToDevice,
    /// 设备写入缓冲区，随后 CPU 读取。
    FromDevice,
    /// CPU 和设备都可能读写缓冲区。
    Bidirectional,
}

#[derive(Clone, Copy)]
pub struct DmaSyncRegion {
    pub paddr: usize,
    pub vaddr: usize,
    pub len: usize,
    pub direction: DmaDirection,
}

/// 设备可见 DMA 地址窗口。
///
/// `cpu_start..cpu_start+size` 是内核物理地址范围，`dma_start` 是同一窗口在设备
/// 描述符中应看到的起始地址。没有 IOMMU 或偏移窗口的平台通常使用 identity。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaWindow {
    pub cpu_start: usize,
    pub dma_start: usize,
    pub size: usize,
}

impl DmaWindow {
    pub const fn identity(start: usize, size: usize) -> Self {
        Self {
            cpu_start: start,
            dma_start: start,
            size,
        }
    }

    pub fn translate(self, paddr: usize, len: usize) -> Option<usize> {
        let end = paddr.checked_add(len)?;
        let window_end = self.cpu_start.checked_add(self.size)?;
        if paddr < self.cpu_start || end > window_end {
            return None;
        }
        self.dma_start.checked_add(paddr - self.cpu_start)
    }
}

/// DMA bounce buffer 策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaBouncePolicy {
    /// 地址无法被设备直接访问时返回错误。
    Disabled,
    /// 允许 DMA 层后续引入 bounce buffer。
    Allowed,
}

/// 单个设备的 DMA 能力约束。
///
/// 该结构描述设备自身能力：地址位宽、单段大小、scatter-gather 能力和是否 cache
/// coherent。它不从全局平台 hook 推断，后续 PCI/platform 总线应按设备/桥属性填充。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaConstraints {
    pub address_mask: usize,
    pub max_segment_size: usize,
    pub max_segments: usize,
    pub coherent: bool,
    pub supports_scatter_gather: bool,
    pub bounce: DmaBouncePolicy,
}

impl DmaConstraints {
    pub const fn coherent_identity() -> Self {
        Self {
            address_mask: usize::MAX,
            max_segment_size: usize::MAX,
            max_segments: 1,
            coherent: true,
            supports_scatter_gather: false,
            bounce: DmaBouncePolicy::Disabled,
        }
    }

    pub fn accepts_dma_addr(self, dma_addr: usize, len: usize) -> bool {
        let Some(end) = dma_addr.checked_add(len.saturating_sub(1)) else {
            return false;
        };
        end <= self.address_mask && len <= self.max_segment_size
    }
}

/// 设备 DMA 地址映射与 cache 同步接口。
///
/// mapper 是 per-device 上下文的一部分；全局 `DMA_OPS` 只作为默认 mapper 的
/// 兼容入口存在，不能再作为设备能力模型本身。
pub trait DmaMapper: Send + Sync {
    fn sync_for_device(&self, region: DmaSyncRegion);
    fn sync_for_cpu(&self, region: DmaSyncRegion);
    fn phys_to_dma(&self, region: DmaSyncRegion, constraints: DmaConstraints) -> Option<usize>;

    /// 建立一个可能有状态的设备地址映射。
    ///
    /// 直连 mapper 沿用 `phys_to_dma` 并返回 token 0；IOMMU mapper 覆盖本方法，
    /// 分配 IOVA、更新页表并把撤销所需的 opaque token 放入结果。
    fn map_region(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
    ) -> Option<DmaMappedRegion> {
        Some(DmaMappedRegion {
            dma_addr: self.phys_to_dma(region, constraints)?,
            token: 0,
        })
    }

    /// 在多 IOMMU path 组成一个逻辑设备域时，把同一物理区间映射到指定 IOVA。
    ///
    /// 默认实现只接受 mapper 自主分配后恰好一致的地址；支持多 path 的 IOMMU
    /// domain 必须覆盖本方法并真正尊重 `dma_addr`。
    fn map_region_at(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        dma_addr: usize,
    ) -> Option<DmaMappedRegion> {
        let mapping = self.map_region(region, constraints)?;
        if mapping.dma_addr == dma_addr {
            return Some(mapping);
        }
        let _ = self.unmap_region(region, mapping);
        None
    }

    /// 撤销先前建立的映射并完成必要的 IOTLB 失效。
    ///
    /// 返回 `false` 表示 mapper 无法确认撤销完成；调用方会记录错误，并且不会在
    /// 撤销前把后端物理页交还分配器。
    fn unmap_region(&self, _region: DmaSyncRegion, _mapping: DmaMappedRegion) -> bool {
        true
    }
}

/// mapper 返回的设备地址和 opaque 撤销 token。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaMappedRegion {
    pub dma_addr: usize,
    pub token: u64,
}

struct LegacyGlobalDmaMapper;

impl DmaMapper for LegacyGlobalDmaMapper {
    fn sync_for_device(&self, region: DmaSyncRegion) {
        if !DMA_OPS_OVERRIDDEN.load(Ordering::Acquire) {
            return;
        }
        let ops = *DMA_OPS.lock();
        (ops.sync_for_device)(region);
    }

    fn sync_for_cpu(&self, region: DmaSyncRegion) {
        if !DMA_OPS_OVERRIDDEN.load(Ordering::Acquire) {
            return;
        }
        let ops = *DMA_OPS.lock();
        (ops.sync_for_cpu)(region);
    }

    fn phys_to_dma(&self, region: DmaSyncRegion, constraints: DmaConstraints) -> Option<usize> {
        if !DMA_OPS_OVERRIDDEN.load(Ordering::Acquire) {
            return constraints
                .accepts_dma_addr(region.paddr, region.len)
                .then_some(region.paddr);
        }
        let ops = *DMA_OPS.lock();
        let dma_addr = (ops.phys_to_dma)(region);
        constraints
            .accepts_dma_addr(dma_addr, region.len)
            .then_some(dma_addr)
    }
}

static LEGACY_GLOBAL_DMA_MAPPER: LegacyGlobalDmaMapper = LegacyGlobalDmaMapper;

/// 单个设备使用的 DMA 上下文。
///
/// 这是 DMA 子系统的设备级边界：驱动只持有本设备的约束与 mapper，不直接读取
/// 平台全局状态。直连、cache coherent 的平台可以使用默认 mapper；带地址窗口或
/// 隔离域的总线应在枚举设备时构造专属 mapper 并传入 [`DmaContext::new`]。
#[derive(Clone)]
pub struct DmaContext {
    constraints: DmaConstraints,
    mapper: DmaMapperRef,
    windows: Option<DmaWindows>,
    iommu_consumer: Option<IommuConsumerLease>,
    preferred_numa_node: Option<u32>,
    blocked: bool,
}

#[derive(Clone)]
enum DmaMapperRef {
    Static(&'static dyn DmaMapper),
    Owned(Arc<dyn DmaMapper>),
}

impl DmaMapperRef {
    fn get(&self) -> &dyn DmaMapper {
        match self {
            Self::Static(mapper) => *mapper,
            Self::Owned(mapper) => mapper.as_ref(),
        }
    }
}

#[derive(Clone)]
enum DmaWindows {
    Static(&'static [DmaWindow]),
    Owned(Arc<[DmaWindow]>),
}

impl DmaWindows {
    fn get(&self) -> &[DmaWindow] {
        match self {
            Self::Static(windows) => windows,
            Self::Owned(windows) => windows.as_ref(),
        }
    }
}

impl fmt::Debug for DmaContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmaContext")
            .field("constraints", &self.constraints)
            .field(
                "window_count",
                &self
                    .windows
                    .as_ref()
                    .map_or(0, |windows| windows.get().len()),
            )
            .field("preferred_numa_node", &self.preferred_numa_node)
            .field("blocked", &self.blocked)
            .finish_non_exhaustive()
    }
}

#[kernel_symbols::export]
impl DmaContext {
    pub const fn new(constraints: DmaConstraints, mapper: &'static dyn DmaMapper) -> Self {
        Self {
            constraints,
            mapper: DmaMapperRef::Static(mapper),
            windows: None,
            iommu_consumer: None,
            preferred_numa_node: None,
            blocked: false,
        }
    }

    /// 使用拥有型 mapper 构造可安全跨 ELM 生命周期保存的 DMA 上下文。
    pub fn with_mapper(constraints: DmaConstraints, mapper: Arc<dyn DmaMapper>) -> Self {
        Self {
            constraints,
            mapper: DmaMapperRef::Owned(mapper),
            windows: None,
            iommu_consumer: None,
            preferred_numa_node: None,
            blocked: false,
        }
    }

    /// 使用带 PnP consumer lease 的拥有型 IOMMU mapper。
    pub(crate) fn with_iommu_mapper(
        constraints: DmaConstraints,
        mapper: Arc<dyn DmaMapper>,
        consumer: IommuConsumerLease,
    ) -> Self {
        Self {
            constraints,
            mapper: DmaMapperRef::Owned(mapper),
            windows: None,
            iommu_consumer: Some(consumer),
            preferred_numa_node: None,
            blocked: false,
        }
    }

    /// 使用默认平台 mapper 和指定设备约束构造 DMA 上下文。
    ///
    /// 这是总线层给设备生成 per-device DMA 能力的常用入口。全局 mapper 只负责
    /// 执行平台地址转换/cache 同步，地址位宽、coherent 等能力来自设备或桥。
    pub const fn with_constraints(constraints: DmaConstraints) -> Self {
        Self::new(constraints, &LEGACY_GLOBAL_DMA_MAPPER)
    }

    /// 使用一组固件已规范化的 CPU-physical -> device-DMA 窗口。
    ///
    /// `windows` 必须在设备上下文可能被使用期间保持有效；启动固件描述通常具有
    /// 内核全生命周期。空 `dma-ranges` 的 identity 语义应使用
    /// [`Self::with_constraints`]，空窗口切片在本接口中表示没有可达地址。
    pub const fn with_windows(constraints: DmaConstraints, windows: &'static [DmaWindow]) -> Self {
        Self {
            constraints,
            mapper: DmaMapperRef::Static(&LEGACY_GLOBAL_DMA_MAPPER),
            windows: Some(DmaWindows::Static(windows)),
            iommu_consumer: None,
            preferred_numa_node: None,
            blocked: false,
        }
    }

    /// 使用由固件枚举对象拥有的 DMA 窗口，避免把运行期重解析结果泄漏为 `'static`。
    pub fn with_owned_windows(constraints: DmaConstraints, windows: Arc<[DmaWindow]>) -> Self {
        Self {
            constraints,
            mapper: DmaMapperRef::Static(&LEGACY_GLOBAL_DMA_MAPPER),
            windows: Some(DmaWindows::Owned(windows)),
            iommu_consumer: None,
            preferred_numa_node: None,
            blocked: false,
        }
    }

    /// 构造一个保留同步能力、但拒绝生成任何设备地址的上下文。
    ///
    /// 固件要求 IOMMU 或声明了当前内核不能安全表达的地址转换时，总线层使用该
    /// fail-closed 上下文，避免静默退化成 identity DMA。
    pub const fn blocked(constraints: DmaConstraints) -> Self {
        Self {
            constraints,
            mapper: DmaMapperRef::Static(&LEGACY_GLOBAL_DMA_MAPPER),
            windows: None,
            iommu_consumer: None,
            preferred_numa_node: None,
            blocked: true,
        }
    }

    #[kernel_symbols::export(
        name = "general.dev.dma.DmaContext.default_coherent",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA
    )]
    pub fn default_coherent() -> Self {
        Self::new(
            DmaConstraints::coherent_identity(),
            &LEGACY_GLOBAL_DMA_MAPPER,
        )
    }

    pub const fn constraints(&self) -> DmaConstraints {
        self.constraints
    }

    /// 设置固件给出的 DMA 分配亲和节点。
    ///
    /// 该值表达 Linux `dev_to_node()` 风格的优先级，而不是硬件可达性限制：
    /// [`DmaBuffer`] 会先在目标节点严格分配，仅在该节点容量或碎片不足时回退到
    /// [`MemoryPlacement::Any`]。显式 allocator `NumaNode` 请求本身仍保持严格。
    pub fn with_preferred_numa_node(mut self, node_id: Option<u32>) -> Self {
        self.preferred_numa_node = node_id;
        self
    }

    pub const fn preferred_numa_node(&self) -> Option<u32> {
        self.preferred_numa_node
    }

    /// 取得一次性的 IOMMU consumer bus resource。
    pub(crate) fn claim_iommu_pnp_resource(
        &self,
        label: &'static str,
    ) -> Option<Box<dyn PnpResource>> {
        self.iommu_consumer.as_ref()?.claim_pnp_resource(label)
    }

    pub(crate) const fn has_iommu_consumer(&self) -> bool {
        self.iommu_consumer.is_some()
    }

    /// 当前设备地址必须由支持 `VIRTIO_F_ACCESS_PLATFORM` 等等价协议能力的
    /// 驱动提交；否则设备会把 IOVA 当成 CPU 物理地址并绕过 IOMMU。
    pub const fn requires_access_platform(&self) -> bool {
        self.iommu_consumer.is_some()
    }

    pub(crate) fn iommu_consumer_released(&self) -> bool {
        self.iommu_consumer
            .as_ref()
            .is_some_and(IommuConsumerLease::released)
    }

    /// 由具体设备驱动确认 scatter/gather descriptor 能力后设置段数上限。
    ///
    /// `supports_scatter_gather == false` 表示总线尚未启用 SG，设备驱动可以用
    /// 自己的协议上限激活它；若总线已经显式声明 SG 能力，则只能与现有上限取
    /// 交集，不能覆盖 IOMMU、桥或 mapper 给出的更严格限制。
    pub const fn with_scatter_gather(mut self, max_segments: usize) -> Self {
        let requested = if max_segments == 0 { 1 } else { max_segments };
        let effective = if self.constraints.supports_scatter_gather {
            let existing = if self.constraints.max_segments == 0 {
                1
            } else {
                self.constraints.max_segments
            };
            if existing < requested {
                existing
            } else {
                requested
            }
        } else {
            requested
        };
        self.constraints.max_segments = effective;
        self.constraints.supports_scatter_gather = effective > 1;
        self
    }

    /// 为调用者持有的稳定内核虚拟区间生成非拥有 DMA 映射。
    ///
    /// 当前 mapper 契约是无状态的物理地址投影；因此这里不创建需要显式 unmap 的
    /// IOMMU 映射。地址转换、设备窗口与单段长度全部由既有 mapper/constraints
    /// 校验，虚拟区间跨物理不连续页时直接返回 `None`，由驱动走 bounce fallback。
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaContext.map_borrowed",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn map_borrowed(
        &self,
        vaddr: usize,
        len: usize,
        direction: DmaDirection,
    ) -> Option<DmaBorrowedMapping> {
        let paddr = borrowed_range_paddr(vaddr, len)?;
        let region = DmaSyncRegion {
            paddr,
            vaddr,
            len,
            direction,
        };
        let mapping = self.map_region(region)?;
        Some(DmaBorrowedMapping {
            mapping: Some(mapping),
            sync: self.sync_handle(region),
        })
    }

    pub(crate) fn sync_handle(&self, region: DmaSyncRegion) -> DmaSyncHandle {
        DmaSyncHandle {
            context: self.clone(),
            region,
            coherent: self.constraints.coherent,
        }
    }

    fn map_region(&self, region: DmaSyncRegion) -> Option<DmaMappedRegion> {
        if self.blocked {
            return None;
        }
        if let Some(windows) = self.windows.as_ref() {
            let dma_addr = windows
                .get()
                .iter()
                .find_map(|window| window.translate(region.paddr, region.len))?;
            return self
                .constraints
                .accepts_dma_addr(dma_addr, region.len)
                .then_some(DmaMappedRegion { dma_addr, token: 0 });
        }
        self.mapper.get().map_region(region, self.constraints)
    }

    fn unmap_region(&self, region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
        if self.windows.is_some() {
            return true;
        }
        self.mapper.get().unmap_region(region, mapping)
    }

    /// 把一段设备 MMIO doorbell 映射到相同的设备地址。
    ///
    /// MSI/MSI-X message address 是设备将要写入的地址，启用 IOMMU 后也必须存在
    /// 对应页表项。这里固定 IOVA=物理地址，保持 MSI controller 生成的消息 ABI；
    /// 普通 identity mapper 会得到无状态映射，IOMMU mapper 则通过
    /// [`DmaMapper::map_region_at`] 建立可撤销映射。
    pub(crate) fn map_identity_mmio(&self, paddr: usize, len: usize) -> Option<DmaAddressMapping> {
        if self.blocked || len == 0 {
            return None;
        }
        let end = paddr.checked_add(len)?;
        let page_start = paddr & !(PAGE_SIZE - 1);
        let page_end = end.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        let mapped_len = page_end.checked_sub(page_start)?;
        let region = DmaSyncRegion {
            paddr: page_start,
            // MMIO doorbell 不参与 cache 同步；保留零值可阻止调用方误解为可解引用映射。
            vaddr: 0,
            len: mapped_len,
            direction: DmaDirection::FromDevice,
        };
        let mapping = self
            .mapper
            .get()
            .map_region_at(region, self.constraints, page_start)?;
        (mapping.dma_addr == page_start).then_some(DmaAddressMapping {
            context: self.clone(),
            region,
            mapping: Some(mapping),
        })
    }
}

/// 不拥有后端内存、只拥有设备地址映射生命周期的对象。
pub(crate) struct DmaAddressMapping {
    context: DmaContext,
    region: DmaSyncRegion,
    mapping: Option<DmaMappedRegion>,
}

impl DmaAddressMapping {
    pub(crate) fn dma_addr(&self) -> usize {
        self.mapping
            .as_ref()
            .expect("live DMA address mapping owns its mapper token")
            .dma_addr
    }

    pub(crate) fn covers(&self, paddr: usize, len: usize) -> bool {
        let Some(end) = paddr.checked_add(len) else {
            return false;
        };
        let Some(region_end) = self.region.paddr.checked_add(self.region.len) else {
            return false;
        };
        paddr >= self.region.paddr && end <= region_end
    }

    pub(crate) fn translated_addr(&self, paddr: usize, len: usize) -> Option<usize> {
        self.covers(paddr, len)
            .then(|| self.dma_addr().checked_add(paddr - self.region.paddr))?
    }

    /// 显式撤销固定地址映射；失败时把仍拥有 token 的对象交还调用方。
    ///
    /// 消费对象后只返回 `bool` 会立刻触发 Drop，令上层无法在硬件恢复后重试。
    pub(crate) fn unmap(mut self) -> Result<(), Self> {
        if self.unmap_inner() {
            Ok(())
        } else {
            Err(self)
        }
    }

    fn unmap_inner(&mut self) -> bool {
        let Some(mapping) = self.mapping.take() else {
            return true;
        };
        if self.context.unmap_region(self.region, mapping) {
            true
        } else {
            // 保留 token，允许调用方在硬件恢复后重试；Drop 最终会再次尝试并记录。
            self.mapping = Some(mapping);
            false
        }
    }
}

// `PciMsixSet` 跨 ELM 边界按值传递，并在其自动 drop glue 中析构该私有字段。
// 因而这里也是精确 Rust ABI 的传递依赖，必须具有稳定导入符号。
#[kernel_symbols::export]
impl Drop for DmaAddressMapping {
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaAddressMapping.drop",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        if !self.unmap_inner() {
            log::error!(
                "[dma] fixed address mapping unmap failed: dma={:#x} paddr={:#x} len={}",
                self.mapping.map_or(0, |mapping| mapping.dma_addr),
                self.region.paddr,
                self.region.len
            );
        }
    }
}

/// 可跨对象保存的 DMA 同步句柄，不拥有底层内存。
pub(crate) struct DmaSyncHandle {
    context: DmaContext,
    region: DmaSyncRegion,
    coherent: bool,
}

impl DmaSyncHandle {
    #[inline]
    pub(crate) fn sync_for_device(&self) {
        if !self.coherent {
            self.context.mapper.get().sync_for_device(self.region);
        }
    }

    #[inline]
    pub(crate) fn sync_for_cpu(&self) {
        if !self.coherent {
            self.context.mapper.get().sync_for_cpu(self.region);
        }
    }
}

/// 非拥有 DMA 段的设备地址与 cache 同步句柄。
pub struct DmaBorrowedMapping {
    mapping: Option<DmaMappedRegion>,
    sync: DmaSyncHandle,
}

#[kernel_symbols::export]
impl DmaBorrowedMapping {
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBorrowedMapping.dma_addr",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA
    )]
    pub fn dma_addr(&self) -> usize {
        self.mapping
            .as_ref()
            .expect("live borrowed DMA mapping always owns its token")
            .dma_addr
    }

    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBorrowedMapping.sync_for_device",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn sync_for_device(&self) {
        self.sync.sync_for_device();
    }

    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBorrowedMapping.sync_for_cpu",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn sync_for_cpu(&self) {
        self.sync.sync_for_cpu();
    }
}

#[kernel_symbols::export]
impl Drop for DmaBorrowedMapping {
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBorrowedMapping.drop",
        contract = "kernel.general.dma-map@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        let Some(mapping) = self.mapping.take() else {
            return;
        };
        if !self.sync.context.unmap_region(self.sync.region, mapping) {
            panic!(
                "[dma] borrowed mapping unmap failed: dma={:#x} paddr={:#x} len={}",
                mapping.dma_addr, self.sync.region.paddr, self.sync.region.len
            );
        }
    }
}

fn borrowed_range_paddr(vaddr: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let end = vaddr.checked_add(len)?;
    let base = KERNEL_ALLOCATOR.virtual_to_physical(vaddr)?;
    let mut current = vaddr;
    while current < end {
        let offset = current - vaddr;
        if KERNEL_ALLOCATOR.virtual_to_physical(current)? != base.checked_add(offset)? {
            return None;
        }
        let page_left = PAGE_SIZE - current % PAGE_SIZE;
        let chunk_end = current.checked_add(page_left.min(end - current))?;
        let last = chunk_end - 1;
        let last_offset = last - vaddr;
        if KERNEL_ALLOCATOR.virtual_to_physical(last)? != base.checked_add(last_offset)? {
            return None;
        }
        current = chunk_end;
    }
    Some(base)
}

#[derive(Clone, Copy)]
pub struct DmaOps {
    pub sync_for_device: fn(DmaSyncRegion),
    pub sync_for_cpu: fn(DmaSyncRegion),
    /// 把内核物理地址转换成设备在描述符中应看到的 DMA 地址。
    ///
    /// 当前直连总线通常是 identity；一旦平台引入 IOMMU、bounce buffer 或
    /// 设备侧地址窗口，只需要替换此 hook，驱动仍统一使用 [`DmaBuffer::dma_addr`]。
    pub phys_to_dma: fn(DmaSyncRegion) -> usize,
}

impl DmaOps {
    pub const fn coherent() -> Self {
        Self {
            sync_for_device: dma_coherent_sync,
            sync_for_cpu: dma_coherent_sync,
            phys_to_dma: dma_identity_addr,
        }
    }
}

static DMA_OPS: Mutex<DmaOps> = Mutex::new(DmaOps::coherent());
/// 一旦平台安装过自定义 mapper，便永久退出默认无操作快路径。
static DMA_OPS_OVERRIDDEN: AtomicBool = AtomicBool::new(false);

/// 安装平台默认 DMA mapper。
///
/// 这个入口只定义“未提供设备专属 mapper 时”的平台默认行为。设备的地址位宽、
/// 段大小、coherency 等能力仍由 [`DmaContext`] 内的 per-device constraints
/// 表达，驱动不通过这里反推设备能力。
#[kernel_symbols::export(
    name = "general.dev.dma.set_dma_ops",
    contract = "kernel.general.dma-admin@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_ADMIN,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn set_dma_ops(ops: DmaOps) {
    if super::elm_lifecycle::install_dma_ops(ops).is_err() {
        log::error!("[dma] ELM DMA 平台操作安装失败，原操作保持不变");
    }
}

pub(crate) fn replace_dma_ops(ops: DmaOps) -> DmaOps {
    // 先关闭快路径再发布新 hook；并发同步至多调用一次旧的 coherent 空操作，
    // 不会在自定义 mapper 生效后错误跳过 cache 维护。
    DMA_OPS_OVERRIDDEN.store(true, Ordering::Release);
    let mut current = DMA_OPS.lock();
    core::mem::replace(&mut *current, ops)
}

/// 使用平台默认 mapper 把 CPU 写入同步给设备。
#[kernel_symbols::export(
    name = "general.dev.dma.sync_for_device",
    contract = "kernel.general.dma-map@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn sync_for_device(region: DmaSyncRegion) {
    LEGACY_GLOBAL_DMA_MAPPER.sync_for_device(region);
}

/// 使用平台默认 mapper 把设备写入同步给 CPU。
#[kernel_symbols::export(
    name = "general.dev.dma.sync_for_cpu",
    contract = "kernel.general.dma-map@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn sync_for_cpu(region: DmaSyncRegion) {
    LEGACY_GLOBAL_DMA_MAPPER.sync_for_cpu(region);
}

/// 使用平台默认 mapper 生成设备可见 DMA 地址并执行设备约束校验。
#[kernel_symbols::export(
    name = "general.dev.dma.phys_to_dma",
    contract = "kernel.general.dma-map@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA
)]
pub fn phys_to_dma(region: DmaSyncRegion, constraints: DmaConstraints) -> Option<usize> {
    LEGACY_GLOBAL_DMA_MAPPER.phys_to_dma(region, constraints)
}

fn dma_coherent_sync(_region: DmaSyncRegion) {
    // cache coherent DMA 平台无需显式 clean/invalidate；非 coherent 平台应在启动期
    // 通过 set_dma_ops() 安装 arch/platform 专用同步 hook。
}

fn dma_identity_addr(region: DmaSyncRegion) -> usize {
    region.paddr
}

/// 由物理页支撑、具有稳定内核虚拟映射和设备可见 DMA 地址的缓冲区。
pub struct DmaBuffer {
    allocation: PhysicalAllocation,
    context: DmaContext,
    mapping: Option<DmaMappedRegion>,
    mapped_region: Option<DmaSyncRegion>,
    paddr: usize,
    vaddr: usize,
    dma_addr: usize,
    len: usize,
    direction: DmaDirection,
}

#[kernel_symbols::export]
impl DmaBuffer {
    /// 分配一个已清零的 DMA 缓冲区，至少暴露 `len` 字节可用空间。
    pub fn new(len: usize, align: usize, direction: DmaDirection) -> Result<Self, &'static str> {
        Self::new_in(DmaContext::default_coherent(), len, align, direction)
    }

    /// 使用指定设备 DMA 上下文分配缓冲区。
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBuffer.new_in",
        contract = "kernel.general.dma-buffer@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn new_in(
        context: DmaContext,
        len: usize,
        align: usize,
        direction: DmaDirection,
    ) -> Result<Self, &'static str> {
        if !align.is_power_of_two() {
            return Err("DMA alignment must be a non-zero power of two");
        }

        let alloc_len = len.max(1);
        let request = PhysicalAllocRequest::new(alloc_len, align);
        let allocation = match context.preferred_numa_node {
            Some(node_id) => match KERNEL_ALLOCATOR
                .allocate_physical(request.with_placement(MemoryPlacement::NumaNode(node_id)))
            {
                Ok(allocation) => allocation,
                // DT NUMA 归属是分配亲和性。节点局部容量/碎片不足时允许跨节点，
                // 但初始化、参数或元数据错误不能被一次 Any 重试掩盖。
                Err(PhysicalAllocError::Fragmented) => KERNEL_ALLOCATOR
                    .allocate_physical(request)
                    .map_err(|_| "failed to allocate DMA buffer")?,
                Err(_) => return Err("failed to allocate DMA buffer"),
            },
            None => KERNEL_ALLOCATOR
                .allocate_physical(request)
                .map_err(|_| "failed to allocate DMA buffer")?,
        };
        let Some(phys_to_virt) = KERNEL_ALLOCATOR.load_phys_to_virt() else {
            let _ = KERNEL_ALLOCATOR.free_physical(allocation);
            return Err("phys_to_virt hook is not installed");
        };
        let vaddr = phys_to_virt(allocation.paddr);

        unsafe {
            core::ptr::write_bytes(vaddr as *mut u8, 0, allocation.size);
        }

        let region = DmaSyncRegion {
            paddr: allocation.paddr,
            vaddr,
            len: alloc_len,
            direction,
        };
        let Some(mapping) = context.map_region(region) else {
            let _ = KERNEL_ALLOCATOR.free_physical(allocation);
            return Err("DMA buffer is outside device DMA constraints");
        };

        Ok(Self {
            paddr: allocation.paddr,
            allocation,
            context,
            mapping: Some(mapping),
            mapped_region: Some(region),
            vaddr,
            dma_addr: mapping.dma_addr,
            len,
            direction,
        })
    }

    /// 分配一个已清零的页大小 DMA 缓冲区。
    pub fn page(direction: DmaDirection) -> Result<Self, &'static str> {
        Self::new(PAGE_SIZE, PAGE_SIZE, direction)
    }

    /// 使用指定设备 DMA 上下文分配一个已清零的页大小 DMA 缓冲区。
    pub fn page_in(context: DmaContext, direction: DmaDirection) -> Result<Self, &'static str> {
        Self::new_in(context, PAGE_SIZE, PAGE_SIZE, direction)
    }

    /// 后端物理地址，仅供内核内部诊断或平台层转换使用。
    pub const fn paddr(&self) -> usize {
        self.paddr
    }

    /// 设备描述符中应写入的 DMA 地址。
    pub const fn dma_addr(&self) -> usize {
        self.dma_addr
    }

    /// DMA 缓冲区的内核虚拟地址。
    pub const fn vaddr(&self) -> usize {
        self.vaddr
    }

    /// 对外暴露的可用字节长度。
    pub const fn len(&self) -> usize {
        self.len
    }

    /// 对外暴露长度是否为 0。
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 缓冲区配置的 DMA 传输方向。
    pub const fn direction(&self) -> DmaDirection {
        self.direction
    }

    /// DMA 缓冲区的不可变 CPU 视图。
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vaddr as *const u8, self.len) }
    }

    /// DMA 缓冲区的可变 CPU 视图。
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.vaddr as *mut u8, self.len) }
    }

    /// 将 CPU 写入的内容同步到设备可见状态。
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBuffer.sync_for_device",
        contract = "kernel.general.dma-buffer@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn sync_for_device(&self) {
        self.sync_handle().sync_for_device();
    }

    /// 将设备写入的内容同步到 CPU 可见状态。
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBuffer.sync_for_cpu",
        contract = "kernel.general.dma-buffer@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn sync_for_cpu(&self) {
        self.sync_handle().sync_for_cpu();
    }

    pub(crate) fn sync_handle(&self) -> DmaSyncHandle {
        self.context.sync_handle(self.sync_region())
    }

    fn sync_region(&self) -> DmaSyncRegion {
        DmaSyncRegion {
            paddr: self.paddr(),
            vaddr: self.vaddr(),
            len: self.len(),
            direction: self.direction(),
        }
    }
}

impl DmaBuffer {
    /// 创建一个不拥有底层内存的"视图" DmaBuffer。
    /// drop 时 free_physical 是空操作（size=0）。
    pub fn sub_view(dma_addr: usize, vaddr: usize, len: usize) -> Self {
        Self::sub_view_in(
            DmaContext::default_coherent(),
            dma_addr,
            vaddr,
            dma_addr,
            len,
        )
    }

    /// 创建一个继承指定 DMA 上下文的非拥有视图。
    pub fn sub_view_in(
        context: DmaContext,
        dma_addr: usize,
        vaddr: usize,
        paddr: usize,
        len: usize,
    ) -> Self {
        Self {
            allocation: PhysicalAllocation {
                paddr: 0,
                size: 0,
                order: 0,
                page_size: 0,
            },
            context,
            mapping: None,
            mapped_region: None,
            paddr,
            vaddr,
            dma_addr,
            len,
            direction: DmaDirection::Bidirectional,
        }
    }

    /// 从已有的 PhysicalAllocation 构造 DmaBuffer（继承其生命周期）。
    /// 用于 legacy virtqueue：主分配由 desc ring 持有，avail/used 用 sub_view。
    pub fn from_allocation(
        alloc: PhysicalAllocation,
        dma_addr: usize,
        vaddr: usize,
        len: usize,
        direction: DmaDirection,
    ) -> Self {
        Self::from_allocation_in(
            DmaContext::default_coherent(),
            alloc,
            dma_addr,
            vaddr,
            len,
            direction,
        )
    }

    /// 从已有的 PhysicalAllocation 构造继承指定 DMA 上下文的 DmaBuffer。
    pub fn from_allocation_in(
        context: DmaContext,
        alloc: PhysicalAllocation,
        dma_addr: usize,
        vaddr: usize,
        len: usize,
        direction: DmaDirection,
    ) -> Self {
        Self {
            paddr: alloc.paddr,
            allocation: alloc,
            context,
            mapping: None,
            mapped_region: None,
            vaddr,
            dma_addr,
            len,
            direction,
        }
    }

    /// 消费 DmaBuffer，返回内部 PhysicalAllocation 供手动管理。
    pub fn take_allocation(mut self) -> PhysicalAllocation {
        assert!(
            self.unmap(),
            "DMA mapping must be revoked before allocation transfer"
        );
        let alloc = self.allocation;
        self.allocation.size = 0;
        alloc
    }

    /// 消费一个已映射分配并把所有权缩小为其起始视图。
    ///
    /// legacy virtqueue 等布局使用本入口让首段对象继续拥有整段物理分配和 IOMMU
    /// mapping；其余子段只能创建 `sub_view_in`，不得重复 unmap。
    pub fn into_owner_view(mut self, len: usize, direction: DmaDirection) -> Self {
        self.len = len.min(self.len);
        self.direction = direction;
        self
    }

    fn unmap(&mut self) -> bool {
        let (Some(mapping), Some(region)) = (self.mapping.take(), self.mapped_region.take()) else {
            return true;
        };
        if !self.context.unmap_region(region, mapping) {
            self.mapping = Some(mapping);
            self.mapped_region = Some(region);
            log::error!(
                "[dma] mapping unmap failed: dma={:#x} paddr={:#x} len={}",
                mapping.dma_addr,
                region.paddr,
                region.len
            );
            return false;
        }
        true
    }
}

#[kernel_symbols::export]
impl Drop for DmaBuffer {
    #[kernel_symbols::export(
        name = "general.dev.dma.DmaBuffer.drop",
        contract = "kernel.general.dma-buffer@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DMA,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    fn drop(&mut self) {
        if !self.unmap() {
            // 设备地址仍可能访问后端页；泄漏该分配比交还 buddy 后发生 DMA UAF 安全。
            self.allocation.size = 0;
            return;
        }
        if self.allocation.size == 0 {
            return;
        }
        if let Err(error) = KERNEL_ALLOCATOR.try_free_physical(self.allocation) {
            log::error!(
                "[dma] 释放 DMA 缓冲失败: paddr={:#x} size={} error={:?}",
                self.allocation.paddr,
                self.allocation.size,
                error
            );
        }
    }
}

/// 单页、页对齐 DMA 分配的便捷包装。
pub struct DmaPage {
    buffer: DmaBuffer,
}

impl DmaPage {
    /// 分配一个已清零 DMA 页。
    pub fn new(direction: DmaDirection) -> Result<Self, &'static str> {
        Ok(Self {
            buffer: DmaBuffer::page(direction)?,
        })
    }

    /// 借用底层 DMA 缓冲区。
    pub const fn buffer(&self) -> &DmaBuffer {
        &self.buffer
    }

    /// 可变借用底层 DMA 缓冲区。
    pub const fn buffer_mut(&mut self) -> &mut DmaBuffer {
        &mut self.buffer
    }

    /// 消费包装并返回底层 DMA 缓冲区。
    pub fn into_buffer(self) -> DmaBuffer {
        self.buffer
    }
}

impl core::ops::Deref for DmaPage {
    type Target = DmaBuffer;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl core::ops::DerefMut for DmaPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

impl net::buf::NetBufStorage for DmaBuffer {
    fn capacity(&self) -> usize {
        self.len()
    }

    fn base_ptr(&self) -> core::ptr::NonNull<u8> {
        core::ptr::NonNull::new(self.vaddr() as *mut u8).expect("DMA buffer 虚拟地址为空")
    }

    fn dma_addr(&self) -> Option<u64> {
        Some(self.dma_addr() as u64)
    }

    fn sync_for_cpu(&self, offset: usize, len: usize) {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return;
        }
        self.context
            .sync_handle(DmaSyncRegion {
                paddr: self.paddr() + offset,
                vaddr: self.vaddr() + offset,
                len,
                direction: self.direction(),
            })
            .sync_for_cpu();
    }

    fn sync_for_device(&self, offset: usize, len: usize) {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return;
        }
        self.context
            .sync_handle(DmaSyncRegion {
                paddr: self.paddr() + offset,
                vaddr: self.vaddr() + offset,
                len,
                direction: self.direction(),
            })
            .sync_for_device();
    }
}

/// 在常驻 DMA 子系统中构造网络 buffer pool，使 storage trait vtable 和回收入口
/// 不依赖可卸载的 driver ELM 镜像。
#[kernel_symbols::export(
    name = "general.dev.dma.new_netbuf_pool",
    contract = "kernel.general.dma-netbuf-pool@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA
        | kernel_symbols::capability::DEVICE_RESOURCE
        | kernel_symbols::capability::ALLOCATOR_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn new_netbuf_pool(
    context: DmaContext,
    count: usize,
    size: usize,
    align: usize,
    direction: DmaDirection,
) -> Result<net::buf::NetBufPoolOwner, &'static str> {
    if count == 0 {
        return Err("DMA NetBuf pool 不能为空");
    }
    let mut storages: Vec<Box<dyn net::buf::NetBufStorage>> = Vec::new();
    storages
        .try_reserve_exact(count)
        .map_err(|_| "DMA NetBuf pool 元数据分配失败")?;
    for _ in 0..count {
        storages.push(Box::new(DmaBuffer::new_in(
            context.clone(),
            size,
            align,
            direction,
        )?));
    }
    net::buf::NetBufPool::new(storages.into_boxed_slice()).map_err(|_| "DMA NetBuf pool 构造失败")
}

/// 在常驻内核中构造共享 DMA 网络 buffer pool。
///
/// `SharedNetBufPool` 的锁类型属于常驻 `net` crate；动态驱动不能自行构造，
/// 否则模块侧的第三方锁 crate 实例会泄漏进 ELM ABI。
#[kernel_symbols::export(
    name = "general.dev.dma.new_shared_netbuf_pool",
    contract = "kernel.general.dma-netbuf-pool@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DMA
        | kernel_symbols::capability::DEVICE_RESOURCE
        | kernel_symbols::capability::ALLOCATOR_MEMORY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn new_shared_netbuf_pool(
    context: DmaContext,
    count: usize,
    size: usize,
    align: usize,
    direction: DmaDirection,
) -> Result<net::buf::SharedNetBufPool, &'static str> {
    new_netbuf_pool(context, count, size, align, direction).map(|owner| Arc::new(Mutex::new(owner)))
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static WINDOWS: [DmaWindow; 2] = [
        DmaWindow {
            cpu_start: 0x1000,
            dma_start: 0x8000,
            size: 0x100,
        },
        DmaWindow {
            cpu_start: 0x3000,
            dma_start: 0x20,
            size: 0x80,
        },
    ];

    fn constraints() -> DmaConstraints {
        DmaConstraints {
            address_mask: usize::MAX,
            max_segment_size: usize::MAX,
            max_segments: 1,
            coherent: true,
            supports_scatter_gather: false,
            bounce: DmaBouncePolicy::Disabled,
        }
    }

    fn region(paddr: usize, len: usize) -> DmaSyncRegion {
        DmaSyncRegion {
            paddr,
            vaddr: 0,
            len,
            direction: DmaDirection::Bidirectional,
        }
    }

    #[test]
    fn windowed_and_blocked_contexts_never_fall_back_to_identity() {
        let context = DmaContext::with_windows(constraints(), &WINDOWS);
        assert_eq!(
            context.map_region(region(0x1020, 0x20)),
            Some(DmaMappedRegion {
                dma_addr: 0x8020,
                token: 0,
            })
        );
        assert_eq!(
            context.map_region(region(0x3070, 0x10)),
            Some(DmaMappedRegion {
                dma_addr: 0x90,
                token: 0,
            })
        );
        assert_eq!(context.map_region(region(0x10f0, 0x20)), None);
        assert_eq!(context.map_region(region(0x2000, 0x10)), None);

        let blocked = DmaContext::blocked(constraints());
        assert_eq!(blocked.map_region(region(0x1000, 0x10)), None);
    }

    struct StatefulMapper {
        maps: AtomicUsize,
        unmaps: AtomicUsize,
    }

    impl DmaMapper for StatefulMapper {
        fn sync_for_device(&self, _region: DmaSyncRegion) {}

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            None
        }

        fn map_region(
            &self,
            region: DmaSyncRegion,
            constraints: DmaConstraints,
        ) -> Option<DmaMappedRegion> {
            self.maps.fetch_add(1, Ordering::Relaxed);
            let dma_addr = region.paddr.checked_add(0x4000)?;
            constraints
                .accepts_dma_addr(dma_addr, region.len)
                .then_some(DmaMappedRegion {
                    dma_addr,
                    token: 0x55aa,
                })
        }

        fn unmap_region(&self, _region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
            if mapping.token != 0x55aa {
                return false;
            }
            self.unmaps.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    #[test]
    fn borrowed_mapping_drop_revokes_stateful_mapper_token() {
        let mapper = Arc::new(StatefulMapper {
            maps: AtomicUsize::new(0),
            unmaps: AtomicUsize::new(0),
        });
        let context = DmaContext::with_mapper(constraints(), mapper.clone());
        let region = region(0x2000, 0x40);
        let mapping = context.map_region(region).unwrap();
        let borrowed = DmaBorrowedMapping {
            mapping: Some(mapping),
            sync: context.sync_handle(region),
        };
        assert_eq!(borrowed.dma_addr(), 0x6000);
        assert_eq!(mapper.maps.load(Ordering::Relaxed), 1);
        drop(borrowed);
        assert_eq!(mapper.unmaps.load(Ordering::Relaxed), 1);
    }

    struct FixedMapper {
        maps: AtomicUsize,
        unmaps: AtomicUsize,
    }

    impl DmaMapper for FixedMapper {
        fn sync_for_device(&self, _region: DmaSyncRegion) {}

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {}

        fn phys_to_dma(
            &self,
            _region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            None
        }

        fn map_region_at(
            &self,
            region: DmaSyncRegion,
            constraints: DmaConstraints,
            dma_addr: usize,
        ) -> Option<DmaMappedRegion> {
            assert_eq!(region.paddr, 0x2800_0000);
            assert_eq!(region.len, PAGE_SIZE);
            assert_eq!(region.direction, DmaDirection::FromDevice);
            assert_eq!(dma_addr, region.paddr);
            self.maps.fetch_add(1, Ordering::Relaxed);
            constraints
                .accepts_dma_addr(dma_addr, region.len)
                .then_some(DmaMappedRegion {
                    dma_addr,
                    token: 0x1234,
                })
        }

        fn unmap_region(&self, region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
            assert_eq!(region.paddr, 0x2800_0000);
            assert_eq!(mapping.dma_addr, 0x2800_0000);
            assert_eq!(mapping.token, 0x1234);
            self.unmaps.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    #[test]
    fn fixed_mmio_mapping_keeps_message_offset_and_unmaps() {
        let mapper = Arc::new(FixedMapper {
            maps: AtomicUsize::new(0),
            unmaps: AtomicUsize::new(0),
        });
        let context = DmaContext::with_mapper(constraints(), mapper.clone());
        let mapping = context.map_identity_mmio(0x2800_0123, 4).unwrap();
        assert_eq!(mapping.translated_addr(0x2800_0123, 4), Some(0x2800_0123));
        assert_eq!(mapper.maps.load(Ordering::Relaxed), 1);
        assert!(mapping.unmap().is_ok());
        assert_eq!(mapper.unmaps.load(Ordering::Relaxed), 1);
    }

    static SYNC_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct CountingMapper;

    impl DmaMapper for CountingMapper {
        fn sync_for_device(&self, _region: DmaSyncRegion) {
            SYNC_CALLS.fetch_add(1, Ordering::Relaxed);
        }

        fn sync_for_cpu(&self, _region: DmaSyncRegion) {
            SYNC_CALLS.fetch_add(1, Ordering::Relaxed);
        }

        fn phys_to_dma(
            &self,
            region: DmaSyncRegion,
            _constraints: DmaConstraints,
        ) -> Option<usize> {
            Some(region.paddr)
        }
    }

    static COUNTING_MAPPER: CountingMapper = CountingMapper;

    #[test]
    fn coherent_dma_context_skips_mapper_sync_hooks() {
        SYNC_CALLS.store(0, Ordering::Relaxed);
        let context = DmaContext::new(DmaConstraints::coherent_identity(), &COUNTING_MAPPER);
        let sync = context.sync_handle(DmaSyncRegion {
            paddr: 0x1000,
            vaddr: 0x2000,
            len: 4096,
            direction: DmaDirection::Bidirectional,
        });

        sync.sync_for_device();
        sync.sync_for_cpu();

        assert_eq!(SYNC_CALLS.load(Ordering::Relaxed), 0);
    }
}
