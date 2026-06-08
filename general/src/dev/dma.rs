//! Common DMA allocation helpers.

use allocator::{KERNEL_ALLOCATOR, PAGE_SIZE, PhysicalAllocRequest, PhysicalAllocation};
use spin::mutex::Mutex;

/// Direction of DMA ownership transfer between CPU memory and a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    /// CPU writes the buffer, then the device reads it.
    ToDevice,
    /// Device writes the buffer, then the CPU reads it.
    FromDevice,
    /// Both CPU and device may read or write the buffer.
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
}

impl DmaOps {
    pub const fn coherent() -> Self {
        Self {
            sync_for_device: dma_coherent_sync,
            sync_for_cpu: dma_coherent_sync,
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

/// Physically backed DMA buffer with a stable kernel virtual mapping.
pub struct DmaBuffer {
    allocation: PhysicalAllocation,
    vaddr: usize,
    len: usize,
    direction: DmaDirection,
}

impl DmaBuffer {
    /// Allocate a zeroed DMA buffer with at least `len` usable bytes.
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

        Ok(Self {
            allocation,
            vaddr,
            len,
            direction,
        })
    }

    /// Allocate a zeroed page-sized DMA buffer.
    pub fn page(direction: DmaDirection) -> Result<Self, &'static str> {
        Self::new(PAGE_SIZE, PAGE_SIZE, direction)
    }

    /// Physical address suitable for device descriptors.
    pub const fn paddr(&self) -> usize {
        self.allocation.paddr
    }

    /// Kernel virtual address of the DMA buffer.
    pub const fn vaddr(&self) -> usize {
        self.vaddr
    }

    /// Usable byte length exposed by this buffer.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns true when the exposed usable length is zero.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Configured transfer direction for this buffer.
    pub const fn direction(&self) -> DmaDirection {
        self.direction
    }

    /// Immutable CPU view of the DMA buffer.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vaddr as *const u8, self.len) }
    }

    /// Mutable CPU view of the DMA buffer.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.vaddr as *mut u8, self.len) }
    }

    /// Prepare CPU-written contents for device access.
    pub fn sync_for_device(&self) {
        let ops = *DMA_OPS.lock();
        (ops.sync_for_device)(self.sync_region());
    }

    /// Prepare device-written contents for CPU access.
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

/// Convenience wrapper for one page-sized, page-aligned DMA allocation.
pub struct DmaPage {
    buffer: DmaBuffer,
}

impl DmaPage {
    /// Allocate a zeroed DMA page.
    pub fn new(direction: DmaDirection) -> Result<Self, &'static str> {
        Ok(Self {
            buffer: DmaBuffer::page(direction)?,
        })
    }

    /// Borrow the backing DMA buffer.
    pub const fn buffer(&self) -> &DmaBuffer {
        &self.buffer
    }

    /// Mutably borrow the backing DMA buffer.
    pub const fn buffer_mut(&mut self) -> &mut DmaBuffer {
        &mut self.buffer
    }

    /// Consume the wrapper and return the backing DMA buffer.
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
