//! VirtIO Block Device (MMIO) 驱动
//!
//! 实现 VirtIO 1.0 规范的块设备驱动，支持：
//! - MMIO 传输层
//! - 异步 I/O 完成回调
//! - Read / Write / Flush 操作
//! - 多队列支持（当前实现单队列）

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use allocator::{KERNEL_ALLOCATOR, PAGE_SIZE, PhysicalAllocRequest, PhysicalAllocation};
use core::any::Any;
use core::mem;
use core::num::NonZeroU32;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::mutex::Mutex;

use crate::dev::block::{
    BlockClass, BlockCompletion, BlockDevice, BlockDeviceInit, BlockDeviceKind, BlockFeatures,
    BlockGeometry, BlockIo, BlockIoCompletion, BlockIoError, BlockIoRequest, BlockLimits,
    BlockSubmitError,
};

// ───────── VirtIO MMIO 寄存器布局 ─────────

const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x74726976; // "virt"
const VIRTIO_MMIO_VERSION: u32 = 0x2; // VirtIO 1.0
const VIRTIO_MMIO_DEVICE_ID_BLOCK: u32 = 2;

// MMIO 寄存器偏移
const MMIO_MAGIC: usize = 0x000;
const MMIO_VERSION: usize = 0x004;
const MMIO_DEVICE_ID: usize = 0x008;
const MMIO_DEVICE_FEATURES: usize = 0x010;
const MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
const MMIO_DRIVER_FEATURES: usize = 0x020;
const MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
const MMIO_QUEUE_SEL: usize = 0x030;
const MMIO_QUEUE_NUM_MAX: usize = 0x034;
const MMIO_QUEUE_NUM: usize = 0x038;
const MMIO_QUEUE_READY: usize = 0x044;
const MMIO_QUEUE_NOTIFY: usize = 0x050;
const MMIO_INTERRUPT_STATUS: usize = 0x060;
const MMIO_INTERRUPT_ACK: usize = 0x064;
const MMIO_STATUS: usize = 0x070;
const MMIO_QUEUE_DESC_LOW: usize = 0x080;
const MMIO_QUEUE_DESC_HIGH: usize = 0x084;
const MMIO_QUEUE_AVAIL_LOW: usize = 0x090;
const MMIO_QUEUE_AVAIL_HIGH: usize = 0x094;
const MMIO_QUEUE_USED_LOW: usize = 0x0a0;
const MMIO_QUEUE_USED_HIGH: usize = 0x0a4;

// VirtIO 设备状态位
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FAILED: u32 = 128;

// VirtIO Block 特性位
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// VirtIO Block 设备配置空间偏移（相对于 0x100）
const BLK_CFG_CAPACITY: usize = 0x100;
const BLK_CFG_BLK_SIZE: usize = 0x114;

// VirtIO Block 请求类型
const VIRTIO_BLK_T_IN: u32 = 0; // Read
const VIRTIO_BLK_T_OUT: u32 = 1; // Write
const VIRTIO_BLK_T_FLUSH: u32 = 4; // Flush

// VirtIO Block 请求状态
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// Virtqueue 描述符标志
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// ───────── VirtIO 数据结构 ─────────

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; 256], // 假设队列大小为 256
    used_event: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; 256],
    avail_event: u16,
}

#[repr(C)]
struct VirtioBlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
struct VirtioBlkReqMeta {
    header: VirtioBlkReqHeader,
    status: u8,
    _pad: [u8; 7],
}

struct DmaBuffer {
    allocation: PhysicalAllocation,
    vaddr: usize,
    len: usize,
}

impl DmaBuffer {
    fn new(len: usize) -> Result<Self, &'static str> {
        let len = len.max(1);
        let allocation = KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(len, PAGE_SIZE))
            .map_err(|_| "Failed to allocate DMA buffer")?;
        let vaddr = dma_vaddr(allocation);
        unsafe {
            core::ptr::write_bytes(vaddr as *mut u8, 0, allocation.size);
        }
        Ok(Self {
            allocation,
            vaddr,
            len,
        })
    }

    fn paddr(&self) -> u64 {
        self.allocation.paddr as u64
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vaddr as *const u8, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.vaddr as *mut u8, self.len) }
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        let _ = KERNEL_ALLOCATOR.free_physical(self.allocation);
    }
}

struct PendingVirtioRequest {
    head: u16,
    completion: BlockCompletion,
    request: BlockIoRequest,
    meta_dma: DmaBuffer,
    data_dma: Option<DmaBuffer>,
    desc_count: usize,
}

// ───────── 驱动内部状态 ─────────

struct VirtioBlkQueue {
    /// Descriptor table 物理页分配记录。
    desc_alloc: Option<PhysicalAllocation>,
    /// Available ring 物理页分配记录。
    avail_alloc: Option<PhysicalAllocation>,
    /// Used ring 物理页分配记录。
    used_alloc: Option<PhysicalAllocation>,
    /// Descriptor table 物理地址
    desc_table: *mut VirtqDesc,
    /// Available ring 物理地址
    avail_ring: *mut VirtqAvail,
    /// Used ring 物理地址
    used_ring: *mut VirtqUsed,
    /// 队列大小
    queue_size: u16,
    /// 上次处理的 used ring 索引
    last_used_idx: u16,
    /// 空闲描述符栈
    free_desc: Vec<u16>,
    /// 待处理请求队列。
    pending: VecDeque<PendingVirtioRequest>,
}

// Safety: VirtioBlkQueue 的裸指针指向 DMA 内存，由 Mutex 保护并发访问
unsafe impl Send for VirtioBlkQueue {}
unsafe impl Sync for VirtioBlkQueue {}

struct VirtioBlkInner {
    /// MMIO 基地址
    base: usize,
    /// 设备容量（扇区数）
    capacity: u64,
    /// 逻辑块大小
    block_size: u32,
    /// 队列
    queue: Mutex<VirtioBlkQueue>,
    /// 中断计数（用于轮询模式）
    irq_count: AtomicUsize,
}

pub struct VirtioBlk {
    inner: Arc<VirtioBlkInner>,
}

impl Drop for VirtioBlk {
    fn drop(&mut self) {
        self.inner.write_reg(MMIO_STATUS, 0);
    }
}

// ───────── MMIO 寄存器访问辅助函数 ─────────

impl VirtioBlkInner {
    #[inline]
    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    #[inline]
    fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    #[inline]
    fn read_reg64(&self, offset: usize) -> u64 {
        let low = self.read_reg(offset) as u64;
        let high = self.read_reg(offset + 4) as u64;
        (high << 32) | low
    }

    fn read_device_features(&self) -> u64 {
        self.write_reg(MMIO_DEVICE_FEATURES_SEL, 0);
        let low = self.read_reg(MMIO_DEVICE_FEATURES) as u64;
        self.write_reg(MMIO_DEVICE_FEATURES_SEL, 1);
        let high = self.read_reg(MMIO_DEVICE_FEATURES) as u64;
        (high << 32) | low
    }

    fn write_driver_features(&self, features: u64) {
        self.write_reg(MMIO_DRIVER_FEATURES_SEL, 0);
        self.write_reg(MMIO_DRIVER_FEATURES, features as u32);
        self.write_reg(MMIO_DRIVER_FEATURES_SEL, 1);
        self.write_reg(MMIO_DRIVER_FEATURES, (features >> 32) as u32);
    }

    fn set_status(&self, status: u32) {
        let current = self.read_reg(MMIO_STATUS);
        self.write_reg(MMIO_STATUS, current | status);
    }
}

// ───────── 队列管理 ─────────

impl VirtioBlkQueue {
    fn alloc_desc_chain(&mut self, count: usize) -> Option<Vec<u16>> {
        if self.free_desc.len() < count {
            return None;
        }
        let mut chain = Vec::with_capacity(count);
        for _ in 0..count {
            chain.push(self.free_desc.pop().unwrap());
        }
        Some(chain)
    }

    fn free_desc_chain(&mut self, chain: &[u16]) {
        for &idx in chain {
            self.free_desc.push(idx);
        }
    }

    fn write_desc(&mut self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        unsafe {
            let desc = &mut *self.desc_table.add(idx as usize);
            desc.addr = addr;
            desc.len = len;
            desc.flags = flags;
            desc.next = next;
        }
    }

    fn submit_to_device(&mut self, head_idx: u16) {
        unsafe {
            let avail = &mut *self.avail_ring;
            let idx = avail.idx;
            avail.ring[idx as usize % self.queue_size as usize] = head_idx;
            core::sync::atomic::fence(Ordering::Release);
            avail.idx = idx.wrapping_add(1);
        }
    }

    fn poll_used(&mut self) -> Option<(u16, u32)> {
        unsafe {
            let used = &*self.used_ring;
            core::sync::atomic::fence(Ordering::Acquire);
            let used_idx = used.idx;
            if self.last_used_idx == used_idx {
                return None;
            }
            let elem = used.ring[self.last_used_idx as usize % self.queue_size as usize];
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            Some((elem.id as u16, elem.len))
        }
    }
}

impl Drop for VirtioBlkQueue {
    fn drop(&mut self) {
        if let Some(allocation) = self.desc_alloc.take() {
            let _ = KERNEL_ALLOCATOR.free_physical(allocation);
        }
        if let Some(allocation) = self.avail_alloc.take() {
            let _ = KERNEL_ALLOCATOR.free_physical(allocation);
        }
        if let Some(allocation) = self.used_alloc.take() {
            let _ = KERNEL_ALLOCATOR.free_physical(allocation);
        }
    }
}

fn alloc_dma_page() -> Result<PhysicalAllocation, &'static str> {
    KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .map_err(|_| "Failed to allocate DMA page")
}

fn dma_vaddr(allocation: PhysicalAllocation) -> usize {
    allocator::KERNEL_ALLOCATOR.load_phys_to_virt().unwrap()(allocation.paddr)
}

// ───────── 驱动初始化 ─────────

impl VirtioBlk {
    /// 创建并初始化 VirtIO Block 设备驱动
    ///
    /// # 参数
    /// - `mmio_base`: MMIO 寄存器基地址（虚拟地址）
    /// - `virt_to_phys`: 虚拟地址到物理地址的转换函数
    ///
    /// # 返回
    /// 成功时返回驱动实例，失败时返回错误信息
    pub fn new<F>(mmio_base: usize, _virt_to_phys: F) -> Result<Self, &'static str>
    where
        F: Fn(usize) -> usize,
    {
        let inner = VirtioBlkInner {
            base: mmio_base,
            capacity: 0,
            block_size: 512,
            queue: Mutex::new(VirtioBlkQueue {
                desc_alloc: None,
                avail_alloc: None,
                used_alloc: None,
                desc_table: core::ptr::null_mut(),
                avail_ring: core::ptr::null_mut(),
                used_ring: core::ptr::null_mut(),
                queue_size: 0,
                last_used_idx: 0,
                free_desc: Vec::new(),
                pending: VecDeque::new(),
            }),
            irq_count: AtomicUsize::new(0),
        };

        // 1. 验证 Magic 和 Version
        if inner.read_reg(MMIO_MAGIC) != VIRTIO_MMIO_MAGIC_VALUE {
            return Err("Invalid VirtIO magic value");
        }
        if inner.read_reg(MMIO_VERSION) != VIRTIO_MMIO_VERSION {
            return Err("Unsupported VirtIO version");
        }
        if inner.read_reg(MMIO_DEVICE_ID) != VIRTIO_MMIO_DEVICE_ID_BLOCK {
            return Err("Not a VirtIO block device");
        }

        // 2. 重置设备
        inner.write_reg(MMIO_STATUS, 0);

        // 3. 设置 ACKNOWLEDGE 和 DRIVER 状态位
        inner.set_status(VIRTIO_STATUS_ACKNOWLEDGE);
        inner.set_status(VIRTIO_STATUS_DRIVER);

        // 4. 协商特性
        let device_features = inner.read_device_features();
        let mut driver_features = 0u64;

        if device_features & VIRTIO_F_VERSION_1 == 0 {
            inner.write_reg(MMIO_STATUS, VIRTIO_STATUS_FAILED);
            return Err("VirtIO 1.0 VERSION_1 feature is missing");
        }
        driver_features |= VIRTIO_F_VERSION_1;

        // 选择我们支持的特性
        if device_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            driver_features |= VIRTIO_BLK_F_BLK_SIZE;
        }
        if device_features & VIRTIO_BLK_F_FLUSH != 0 {
            driver_features |= VIRTIO_BLK_F_FLUSH;
        }
        if device_features & VIRTIO_BLK_F_RO != 0 {
            driver_features |= VIRTIO_BLK_F_RO;
        }

        inner.write_driver_features(driver_features);
        inner.set_status(VIRTIO_STATUS_FEATURES_OK);

        // 验证特性协商是否成功
        if inner.read_reg(MMIO_STATUS) & VIRTIO_STATUS_FEATURES_OK == 0 {
            inner.write_reg(MMIO_STATUS, VIRTIO_STATUS_FAILED);
            return Err("Feature negotiation failed");
        }

        // 5. 读取设备配置
        let capacity = inner.read_reg64(BLK_CFG_CAPACITY);
        let block_size = if driver_features & VIRTIO_BLK_F_BLK_SIZE != 0 {
            inner.read_reg(BLK_CFG_BLK_SIZE)
        } else {
            512
        };
        if block_size < 512 || !block_size.is_power_of_two() || !block_size.is_multiple_of(512) {
            inner.write_reg(MMIO_STATUS, VIRTIO_STATUS_FAILED);
            return Err("Invalid VirtIO block size");
        }

        // 6. 设置队列
        inner.write_reg(MMIO_QUEUE_SEL, 0);
        let queue_size = inner.read_reg(MMIO_QUEUE_NUM_MAX) as u16;
        if queue_size == 0 {
            return Err("Queue size is zero");
        }
        let queue_size = queue_size.min(256); // 限制队列大小

        // 分配队列 DMA 内存。普通内核堆地址未必能被简单转换成物理地址，
        // 因此队列页必须直接来自物理页分配器。
        let desc_alloc = alloc_dma_page()?;
        let avail_alloc = alloc_dma_page()?;
        let used_alloc = alloc_dma_page()?;
        let desc_table = dma_vaddr(desc_alloc) as *mut VirtqDesc;
        let avail_ring = dma_vaddr(avail_alloc) as *mut VirtqAvail;
        let used_ring = dma_vaddr(used_alloc) as *mut VirtqUsed;
        unsafe {
            core::ptr::write_bytes(desc_table.cast::<u8>(), 0, PAGE_SIZE);
            core::ptr::write_bytes(avail_ring.cast::<u8>(), 0, PAGE_SIZE);
            core::ptr::write_bytes(used_ring.cast::<u8>(), 0, PAGE_SIZE);
        }

        // 初始化空闲描述符列表
        let mut free_desc = Vec::with_capacity(queue_size as usize);
        for i in (0..queue_size).rev() {
            free_desc.push(i);
        }

        let mut queue = inner.queue.lock();
        queue.desc_alloc = Some(desc_alloc);
        queue.avail_alloc = Some(avail_alloc);
        queue.used_alloc = Some(used_alloc);
        queue.desc_table = desc_table;
        queue.avail_ring = avail_ring;
        queue.used_ring = used_ring;
        queue.queue_size = queue_size;
        queue.free_desc = free_desc;
        drop(queue);

        // 配置队列地址
        inner.write_reg(MMIO_QUEUE_NUM, queue_size as u32);

        let desc_phys = desc_alloc.paddr;
        inner.write_reg(MMIO_QUEUE_DESC_LOW, desc_phys as u32);
        inner.write_reg(MMIO_QUEUE_DESC_HIGH, (desc_phys >> 32) as u32);

        let avail_phys = avail_alloc.paddr;
        inner.write_reg(MMIO_QUEUE_AVAIL_LOW, avail_phys as u32);
        inner.write_reg(MMIO_QUEUE_AVAIL_HIGH, (avail_phys >> 32) as u32);

        let used_phys = used_alloc.paddr;
        inner.write_reg(MMIO_QUEUE_USED_LOW, used_phys as u32);
        inner.write_reg(MMIO_QUEUE_USED_HIGH, (used_phys >> 32) as u32);

        inner.write_reg(MMIO_QUEUE_READY, 1);

        // 7. 设置 DRIVER_OK
        inner.set_status(VIRTIO_STATUS_DRIVER_OK);

        // 更新容量和块大小
        let inner = Arc::new(VirtioBlkInner {
            base: inner.base,
            capacity,
            block_size,
            queue: inner.queue,
            irq_count: inner.irq_count,
        });

        Ok(Self { inner })
    }

    /// 轮询并处理已完成的请求
    pub fn poll(&self) {
        let mut queue = self.inner.queue.lock();

        while let Some((desc_head, _len)) = queue.poll_used() {
            // 查找对应的待处理请求
            if let Some(pos) = queue
                .pending
                .iter()
                .position(|pending| pending.head == desc_head)
            {
                let PendingVirtioRequest {
                    completion,
                    mut request,
                    meta_dma,
                    data_dma,
                    desc_count,
                    ..
                } = queue.pending.remove(pos).unwrap();
                core::sync::atomic::fence(Ordering::Acquire);
                let meta = unsafe { &*(meta_dma.vaddr as *const VirtioBlkReqMeta) };
                let result = match meta.status {
                    VIRTIO_BLK_S_OK => Ok(()),
                    VIRTIO_BLK_S_UNSUPP => Err(BlockIoError::Unsupported),
                    _ => Err(BlockIoError::MediaError),
                };
                if result.is_ok()
                    && let BlockIoRequest::Read { buffer, .. } = &mut request
                    && let Some(data_dma) = data_dma.as_ref()
                {
                    buffer.copy_from_slice(data_dma.as_slice());
                }

                // 释放描述符链
                let mut chain = Vec::new();
                let mut idx = desc_head;
                for _ in 0..desc_count {
                    chain.push(idx);
                    unsafe {
                        let desc = &*queue.desc_table.add(idx as usize);
                        if desc.flags & VIRTQ_DESC_F_NEXT != 0 {
                            idx = desc.next;
                        } else {
                            break;
                        }
                    }
                }
                queue.free_desc_chain(&chain);

                // 调用完成回调
                drop(queue);
                completion(BlockIoCompletion { request, result });
                queue = self.inner.queue.lock();
            }
        }
    }

    /// 处理中断（由中断处理程序调用）
    pub fn handle_interrupt(&self) {
        // 确认中断
        let status = self.inner.read_reg(MMIO_INTERRUPT_STATUS);
        self.inner.write_reg(MMIO_INTERRUPT_ACK, status);
        self.inner.irq_count.fetch_add(1, Ordering::Relaxed);

        // 轮询完成的请求
        self.poll();
    }

    /// 创建 BlockDev 包装
    pub fn into_block_dev(
        self,
        name: &str,
        _virt_to_phys: fn(usize) -> usize,
    ) -> Result<Arc<BlockDevice>, &'static str> {
        let capacity = self.inner.capacity;
        let block_size = self.inner.block_size;
        let sector_scale = (block_size / 512) as u64;
        let logical_blocks = capacity / sector_scale;
        if logical_blocks == 0 {
            return Err("Invalid capacity");
        }

        let logical_size = NonZeroU32::new(block_size).ok_or("Invalid block size")?;
        let geometry = BlockGeometry::new(logical_size, logical_size, Some(logical_blocks))
            .ok_or("Invalid geometry")?;

        let limits = BlockLimits::unrestricted();

        let mut features = BlockFeatures(0);
        if self.inner.read_device_features() & VIRTIO_BLK_F_FLUSH != 0 {
            features |= BlockFeatures::FLUSH;
        }
        if self.inner.read_device_features() & VIRTIO_BLK_F_RO != 0 {
            features |= BlockFeatures::READ_ONLY;
        }

        let io = Arc::new(VirtioBlkIo {
            driver: Arc::new(self),
        });

        Ok(Arc::new(BlockDevice::new(
            BlockDeviceInit {
                name,
                kind: BlockDeviceKind::VirtioBlk,
                class: BlockClass::Whole,
                geometry,
                limits,
                features,
            },
            io,
            None,
        )))
    }
}

// ───────── BlockIo 实现 ─────────

struct VirtioBlkIo {
    driver: Arc<VirtioBlk>,
}

impl BlockIo for VirtioBlkIo {
    fn submit(
        &self,
        req: BlockIoRequest,
        completion: BlockCompletion,
    ) -> Result<(), (BlockSubmitError, BlockIoRequest, BlockCompletion)> {
        self.driver.poll();
        let mut queue = self.driver.inner.queue.lock();

        // 根据请求类型确定需要的描述符数量
        let desc_count = match &req {
            BlockIoRequest::Read { .. } | BlockIoRequest::Write { .. } => 3,
            BlockIoRequest::Flush => 2,
            _ => return Err((BlockSubmitError::Unsupported, req, completion)),
        };

        // 分配描述符链
        let chain = match queue.alloc_desc_chain(desc_count) {
            Some(c) => c,
            None => return Err((BlockSubmitError::QueueFull, req, completion)),
        };

        let head_idx = chain[0];

        let meta_dma = match DmaBuffer::new(mem::size_of::<VirtioBlkReqMeta>()) {
            Ok(buffer) => buffer,
            Err(_) => {
                queue.free_desc_chain(&chain);
                return Err((BlockSubmitError::OutOfMemory, req, completion));
            }
        };
        let sector_scale = self.driver.inner.block_size as u64 / 512;
        let meta = match &req {
            BlockIoRequest::Read { range, .. } => VirtioBlkReqMeta {
                header: VirtioBlkReqHeader {
                    req_type: VIRTIO_BLK_T_IN,
                    reserved: 0,
                    sector: range.lba.saturating_mul(sector_scale),
                },
                status: 0xff,
                _pad: [0; 7],
            },
            BlockIoRequest::Write { range, .. } => VirtioBlkReqMeta {
                header: VirtioBlkReqHeader {
                    req_type: VIRTIO_BLK_T_OUT,
                    reserved: 0,
                    sector: range.lba.saturating_mul(sector_scale),
                },
                status: 0xff,
                _pad: [0; 7],
            },
            BlockIoRequest::Flush => VirtioBlkReqMeta {
                header: VirtioBlkReqHeader {
                    req_type: VIRTIO_BLK_T_FLUSH,
                    reserved: 0,
                    sector: 0,
                },
                status: 0xff,
                _pad: [0; 7],
            },
            _ => {
                queue.free_desc_chain(&chain);
                return Err((BlockSubmitError::Unsupported, req, completion));
            }
        };
        unsafe {
            core::ptr::write(meta_dma.vaddr as *mut VirtioBlkReqMeta, meta);
        }

        let data_dma = match &req {
            BlockIoRequest::Read { buffer, .. } | BlockIoRequest::Write { buffer, .. } => {
                match DmaBuffer::new(buffer.len()) {
                    Ok(mut dma) => {
                        if let BlockIoRequest::Write { buffer, .. } = &req {
                            dma.as_mut_slice().copy_from_slice(buffer);
                        }
                        Some(dma)
                    }
                    Err(_) => {
                        queue.free_desc_chain(&chain);
                        return Err((BlockSubmitError::OutOfMemory, req, completion));
                    }
                }
            }
            BlockIoRequest::Flush => None,
            _ => None,
        };

        let header_phys = meta_dma.paddr();
        let status_phys = meta_dma.paddr() + mem::size_of::<VirtioBlkReqHeader>() as u64;

        // 构造请求
        match &req {
            BlockIoRequest::Read { range, buffer } => {
                let buffer_phys = data_dma
                    .as_ref()
                    .expect("read request must have a DMA data buffer")
                    .paddr();
                queue.write_desc(
                    chain[0],
                    header_phys,
                    mem::size_of::<VirtioBlkReqHeader>() as u32,
                    VIRTQ_DESC_F_NEXT,
                    chain[1],
                );
                queue.write_desc(
                    chain[1],
                    buffer_phys,
                    buffer.len() as u32,
                    VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                    chain[2],
                );
                queue.write_desc(chain[2], status_phys, 1, VIRTQ_DESC_F_WRITE, 0);
                let _ = range;
            }
            BlockIoRequest::Write { range, buffer, .. } => {
                let buffer_phys = data_dma
                    .as_ref()
                    .expect("write request must have a DMA data buffer")
                    .paddr();
                queue.write_desc(
                    chain[0],
                    header_phys,
                    mem::size_of::<VirtioBlkReqHeader>() as u32,
                    VIRTQ_DESC_F_NEXT,
                    chain[1],
                );
                queue.write_desc(
                    chain[1],
                    buffer_phys,
                    buffer.len() as u32,
                    VIRTQ_DESC_F_NEXT,
                    chain[2],
                );
                queue.write_desc(chain[2], status_phys, 1, VIRTQ_DESC_F_WRITE, 0);
                let _ = range;
            }
            BlockIoRequest::Flush => {
                queue.write_desc(
                    chain[0],
                    header_phys,
                    mem::size_of::<VirtioBlkReqHeader>() as u32,
                    VIRTQ_DESC_F_NEXT,
                    chain[1],
                );
                queue.write_desc(chain[1], status_phys, 1, VIRTQ_DESC_F_WRITE, 0);
            }
            _ => {
                queue.free_desc_chain(&chain);
                return Err((BlockSubmitError::Unsupported, req, completion));
            }
        }

        // 提交到设备
        queue.submit_to_device(head_idx);
        queue.pending.push_back(PendingVirtioRequest {
            head: head_idx,
            completion,
            request: req,
            meta_dma,
            data_dma,
            desc_count,
        });

        // 通知设备
        drop(queue);
        self.driver.inner.write_reg(MMIO_QUEUE_NOTIFY, 0);

        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn poll(&self) {
        self.driver.poll();
    }
}
