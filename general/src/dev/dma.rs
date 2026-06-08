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
    vaddr: usize,
    dma_addr: usize,
    len: usize,
    direction: DmaDirection,
}

impl DmaBuffer {
    /// 分配一个已清零的 DMA 缓冲区，至少暴露 `len` 字节可用空间。
    pub fn new(len: usize, align: usize, direction: DmaDirection) -> Result<Self, &'static str> {
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
        let ops = *DMA_OPS.lock();
        let dma_addr = (ops.phys_to_dma)(region);

        Ok(Self {
            allocation,
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
        let ops = *DMA_OPS.lock();
        (ops.sync_for_device)(self.sync_region());
    }

    /// 将设备写入的内容同步到 CPU 可见状态。
    pub fn sync_for_cpu(&self) {
        let ops = *DMA_OPS.lock();
        (ops.sync_for_cpu)(self.sync_region());
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
