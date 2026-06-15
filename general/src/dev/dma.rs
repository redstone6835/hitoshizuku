//! 通用 DMA 分配与同步辅助。

use allocator::{KERNEL_ALLOCATOR, PAGE_SIZE, PhysicalAllocRequest, PhysicalAllocation};
use spin::mutex::Mutex;

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
}

struct LegacyGlobalDmaMapper;

impl DmaMapper for LegacyGlobalDmaMapper {
    fn sync_for_device(&self, region: DmaSyncRegion) {
        let ops = *DMA_OPS.lock();
        (ops.sync_for_device)(region);
    }

    fn sync_for_cpu(&self, region: DmaSyncRegion) {
        let ops = *DMA_OPS.lock();
        (ops.sync_for_cpu)(region);
    }

    fn phys_to_dma(&self, region: DmaSyncRegion, constraints: DmaConstraints) -> Option<usize> {
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
#[derive(Clone, Copy)]
pub struct DmaContext {
    constraints: DmaConstraints,
    mapper: &'static dyn DmaMapper,
}

impl DmaContext {
    pub const fn new(constraints: DmaConstraints, mapper: &'static dyn DmaMapper) -> Self {
        Self {
            constraints,
            mapper,
        }
    }

    /// 使用默认平台 mapper 和指定设备约束构造 DMA 上下文。
    ///
    /// 这是总线层给设备生成 per-device DMA 能力的常用入口。全局 mapper 只负责
    /// 执行平台地址转换/cache 同步，地址位宽、coherent 等能力来自设备或桥。
    pub const fn with_constraints(constraints: DmaConstraints) -> Self {
        Self::new(constraints, &LEGACY_GLOBAL_DMA_MAPPER)
    }

    pub const fn default_coherent() -> Self {
        Self::new(
            DmaConstraints::coherent_identity(),
            &LEGACY_GLOBAL_DMA_MAPPER,
        )
    }

    pub const fn constraints(self) -> DmaConstraints {
        self.constraints
    }
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

/// 安装平台默认 DMA mapper。
///
/// 这个入口只定义“未提供设备专属 mapper 时”的平台默认行为。设备的地址位宽、
/// 段大小、coherency 等能力仍由 [`DmaContext`] 内的 per-device constraints
/// 表达，驱动不通过这里反推设备能力。
pub fn set_dma_ops(ops: DmaOps) {
    *DMA_OPS.lock() = ops;
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
    vaddr: usize,
    dma_addr: usize,
    len: usize,
    direction: DmaDirection,
}

impl DmaBuffer {
    /// 分配一个已清零的 DMA 缓冲区，至少暴露 `len` 字节可用空间。
    pub fn new(len: usize, align: usize, direction: DmaDirection) -> Result<Self, &'static str> {
        Self::new_in(DmaContext::default_coherent(), len, align, direction)
    }

    /// 使用指定设备 DMA 上下文分配缓冲区。
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
        let allocation = KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(alloc_len, align))
            .map_err(|_| "failed to allocate DMA buffer")?;
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
        let Some(dma_addr) = context.mapper.phys_to_dma(region, context.constraints()) else {
            let _ = KERNEL_ALLOCATOR.free_physical(allocation);
            return Err("DMA buffer is outside device DMA constraints");
        };

        Ok(Self {
            allocation,
            context,
            vaddr,
            dma_addr,
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
        self.allocation.paddr
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
    pub fn sync_for_device(&self) {
        self.context.mapper.sync_for_device(self.sync_region());
    }

    /// 将设备写入的内容同步到 CPU 可见状态。
    pub fn sync_for_cpu(&self) {
        self.context.mapper.sync_for_cpu(self.sync_region());
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
        Self {
            allocation: PhysicalAllocation {
                paddr: 0,
                size: 0,
                order: 0,
                page_size: 0,
            },
            context: DmaContext::default_coherent(),
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
        Self {
            allocation: alloc,
            context: DmaContext::default_coherent(),
            vaddr,
            dma_addr,
            len,
            direction,
        }
    }

    /// 消费 DmaBuffer，返回内部 PhysicalAllocation 供手动管理。
    pub fn take_allocation(self) -> PhysicalAllocation {
        let alloc = self.allocation;
        core::mem::forget(self);
        alloc
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        let _ = KERNEL_ALLOCATOR.free_physical(self.allocation);
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
