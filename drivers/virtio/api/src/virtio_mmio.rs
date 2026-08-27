//! VirtIO MMIO 传输层抽象。
//!
//! 封装 legacy (v1) 和 modern (v2) 的寄存器布局差异，
//! 驱动代码通过 [`VirtioMmioTransport`] trait 操作设备，
//! 不需要关心版本细节。

use alloc::boxed::Box;
use core::ptr::{read_volatile, write_volatile};

// ── 公共常量 ────────────────────────────────────────────────────────────────

/// MMIO 魔数值（"virt"，小端）。
const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x74726976;

/// VirtIO 设备状态位（legacy & modern 一致）。
pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_STATUS_DRIVER: u32 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
pub const VIRTIO_STATUS_FAILED: u32 = 128;

/// VirtIO Feature: VERSION_1（仅 modern 模式存在）。
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// 设备必须通过平台 DMA/IOMMU API 访问队列和数据缓冲区。
pub const VIRTIO_F_ACCESS_PLATFORM: u64 = 1 << 33;
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;

// ── Modern MMIO 寄存器偏移 ──────────────────────────────────────────────────

const MODERN_DEVICE_FEATURES: usize = 0x010;
const MODERN_DEVICE_FEATURES_SEL: usize = 0x014;
const MODERN_DRIVER_FEATURES: usize = 0x020;
const MODERN_DRIVER_FEATURES_SEL: usize = 0x024;
const MODERN_QUEUE_SEL: usize = 0x030;
const MODERN_QUEUE_NUM_MAX: usize = 0x034;
const MODERN_QUEUE_NUM: usize = 0x038;
const MODERN_QUEUE_READY: usize = 0x044;
const MODERN_QUEUE_NOTIFY: usize = 0x050;
const MODERN_INTERRUPT_STATUS: usize = 0x060;
const MODERN_INTERRUPT_ACK: usize = 0x064;
const MODERN_STATUS: usize = 0x070;
const MODERN_QUEUE_DESC_LOW: usize = 0x080;
const MODERN_QUEUE_DESC_HIGH: usize = 0x084;
const MODERN_QUEUE_AVAIL_LOW: usize = 0x090;
const MODERN_QUEUE_AVAIL_HIGH: usize = 0x094;
const MODERN_QUEUE_USED_LOW: usize = 0x0a0;
const MODERN_QUEUE_USED_HIGH: usize = 0x0a4;

// ── Legacy MMIO 寄存器偏移 ──────────────────────────────────────────────────

const LEGACY_DEVICE_FEATURES: usize = 0x010;
const LEGACY_DEVICE_FEATURES_SEL: usize = 0x014;
const LEGACY_DRIVER_FEATURES: usize = 0x020;
const LEGACY_DRIVER_FEATURES_SEL: usize = 0x024;
const LEGACY_QUEUE_SEL: usize = 0x030;
const LEGACY_QUEUE_NUM_MAX: usize = 0x034;
const LEGACY_QUEUE_NUM: usize = 0x038;
const LEGACY_QUEUE_ALIGN: usize = 0x03c;
const LEGACY_QUEUE_PFN: usize = 0x040;
const LEGACY_QUEUE_NOTIFY: usize = 0x050;
const LEGACY_INTERRUPT_STATUS: usize = 0x060;
const LEGACY_INTERRUPT_ACK: usize = 0x064;
const LEGACY_STATUS: usize = 0x070;

// ── Transport Trait ──────────────────────────────────────────────────────────

/// VirtIO MMIO 传输层抽象。
///
/// 封装 legacy (v1) 和 modern (v2) 的寄存器布局差异。
/// 驱动代码通过本 trait 操作设备，不直接写裸偏移量。
pub trait VirtioMmioTransport: Send + Sync {
    /// 原始 MMIO 基地址（用于设备配置空间等偏移 ≥ 0x100 的直接访问）。
    fn base(&self) -> usize;

    /// 是否 legacy 模式（v1）。
    fn is_legacy(&self) -> bool;

    /// 读 32-bit MMIO 寄存器。
    ///
    /// # Safety
    /// `offset` 必须对此传输层有效。
    unsafe fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base() + offset) as *const u32) }
    }

    /// 写 32-bit MMIO 寄存器。
    ///
    /// # Safety
    /// `offset` 必须对此传输层有效。
    unsafe fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base() + offset) as *mut u32, value) }
    }

    /// 读 64-bit MMIO 值（两字节拼接，低 32 位在先）。
    unsafe fn read_reg64(&self, offset: usize) -> u64 {
        let lo = unsafe { self.read_reg(offset) } as u64;
        let hi = unsafe { self.read_reg(offset + 4) } as u64;
        lo | (hi << 32)
    }

    // ── 设备状态 ──

    fn read_status(&self) -> u32;
    fn write_status(&self, value: u32);
    fn add_status(&self, bit: u32) {
        self.write_status(self.read_status() | bit);
    }

    // ── 特性协商 ──

    fn read_device_features(&self) -> u64;
    fn write_driver_features(&self, features: u64);

    // ── 队列设置 ──

    fn select_queue(&self, idx: u16);
    fn read_queue_max_size(&self) -> u32;
    fn write_queue_size(&self, size: u32);
    fn configure_queue_addresses(&self, desc_dma: u64, avail_dma: u64, used_dma: u64);
    fn enable_queue(&self);

    // ── 通知 ──

    fn notify_queue(&self, queue_idx: u32);

    // ── 中断 ──

    fn read_interrupt_status(&self) -> u32;
    fn acknowledge_interrupt(&self, status: u32);
}

// ── ModernMmioTransport ──────────────────────────────────────────────────────

pub struct ModernMmioTransport {
    base: usize,
}

impl ModernMmioTransport {
    pub fn new(base: usize) -> Self {
        Self { base }
    }
}

impl VirtioMmioTransport for ModernMmioTransport {
    fn base(&self) -> usize {
        self.base
    }
    fn is_legacy(&self) -> bool {
        false
    }

    fn read_status(&self) -> u32 {
        unsafe { self.read_reg(MODERN_STATUS) }
    }

    fn write_status(&self, value: u32) {
        unsafe { self.write_reg(MODERN_STATUS, value) }
    }

    fn read_device_features(&self) -> u64 {
        let mut features: u64 = 0;
        unsafe {
            self.write_reg(MODERN_DEVICE_FEATURES_SEL, 0);
            features |= self.read_reg(MODERN_DEVICE_FEATURES) as u64;
            self.write_reg(MODERN_DEVICE_FEATURES_SEL, 1);
            features |= (self.read_reg(MODERN_DEVICE_FEATURES) as u64) << 32;
        }
        features
    }

    fn write_driver_features(&self, features: u64) {
        unsafe {
            self.write_reg(MODERN_DRIVER_FEATURES_SEL, 0);
            self.write_reg(MODERN_DRIVER_FEATURES, features as u32);
            self.write_reg(MODERN_DRIVER_FEATURES_SEL, 1);
            self.write_reg(MODERN_DRIVER_FEATURES, (features >> 32) as u32);
        }
    }

    fn select_queue(&self, idx: u16) {
        unsafe { self.write_reg(MODERN_QUEUE_SEL, idx as u32) }
    }

    fn read_queue_max_size(&self) -> u32 {
        unsafe { self.read_reg(MODERN_QUEUE_NUM_MAX) }
    }

    fn write_queue_size(&self, size: u32) {
        unsafe { self.write_reg(MODERN_QUEUE_NUM, size) }
    }

    fn configure_queue_addresses(&self, desc_dma: u64, avail_dma: u64, used_dma: u64) {
        unsafe {
            self.write_reg(MODERN_QUEUE_DESC_LOW, desc_dma as u32);
            self.write_reg(MODERN_QUEUE_DESC_HIGH, (desc_dma >> 32) as u32);
            self.write_reg(MODERN_QUEUE_AVAIL_LOW, avail_dma as u32);
            self.write_reg(MODERN_QUEUE_AVAIL_HIGH, (avail_dma >> 32) as u32);
            self.write_reg(MODERN_QUEUE_USED_LOW, used_dma as u32);
            self.write_reg(MODERN_QUEUE_USED_HIGH, (used_dma >> 32) as u32);
        }
    }

    fn enable_queue(&self) {
        unsafe { self.write_reg(MODERN_QUEUE_READY, 1) }
    }

    fn notify_queue(&self, queue_idx: u32) {
        unsafe { self.write_reg(MODERN_QUEUE_NOTIFY, queue_idx) }
    }

    fn read_interrupt_status(&self) -> u32 {
        unsafe { self.read_reg(MODERN_INTERRUPT_STATUS) }
    }

    fn acknowledge_interrupt(&self, status: u32) {
        unsafe { self.write_reg(MODERN_INTERRUPT_ACK, status) }
    }
}

// ── LegacyMmioTransport ──────────────────────────────────────────────────────

pub struct LegacyMmioTransport {
    base: usize,
}

impl LegacyMmioTransport {
    pub fn new(base: usize) -> Self {
        Self { base }
    }
}

impl VirtioMmioTransport for LegacyMmioTransport {
    fn base(&self) -> usize {
        self.base
    }
    fn is_legacy(&self) -> bool {
        true
    }

    fn read_status(&self) -> u32 {
        unsafe { self.read_reg(LEGACY_STATUS) }
    }

    fn write_status(&self, value: u32) {
        unsafe { self.write_reg(LEGACY_STATUS, value) }
    }

    fn read_device_features(&self) -> u64 {
        let mut features: u64 = 0;
        unsafe {
            self.write_reg(LEGACY_DEVICE_FEATURES_SEL, 0);
            features |= self.read_reg(LEGACY_DEVICE_FEATURES) as u64;
            self.write_reg(LEGACY_DEVICE_FEATURES_SEL, 1);
            features |= (self.read_reg(LEGACY_DEVICE_FEATURES) as u64) << 32;
        }
        features
    }

    fn write_driver_features(&self, features: u64) {
        unsafe {
            self.write_reg(LEGACY_DRIVER_FEATURES_SEL, 0);
            self.write_reg(LEGACY_DRIVER_FEATURES, features as u32);
            self.write_reg(LEGACY_DRIVER_FEATURES_SEL, 1);
            self.write_reg(LEGACY_DRIVER_FEATURES, (features >> 32) as u32);
        }
    }

    fn select_queue(&self, idx: u16) {
        unsafe { self.write_reg(LEGACY_QUEUE_SEL, idx as u32) }
    }

    fn read_queue_max_size(&self) -> u32 {
        unsafe { self.read_reg(LEGACY_QUEUE_NUM_MAX) }
    }

    fn write_queue_size(&self, size: u32) {
        unsafe { self.write_reg(LEGACY_QUEUE_NUM, size) }
    }

    fn configure_queue_addresses(&self, desc_dma: u64, _avail_dma: u64, _used_dma: u64) {
        let pfn = (desc_dma >> 12) as u32;
        unsafe {
            self.write_reg(0x028, 4096); // GuestPageSize
            self.write_reg(LEGACY_QUEUE_ALIGN, 4096);
            self.write_reg(LEGACY_QUEUE_PFN, pfn);
        }
    }

    fn enable_queue(&self) {
        // Legacy: PFN 非零即 ready
    }

    fn notify_queue(&self, queue_idx: u32) {
        unsafe { self.write_reg(LEGACY_QUEUE_NOTIFY, queue_idx) }
    }

    fn read_interrupt_status(&self) -> u32 {
        unsafe { self.read_reg(LEGACY_INTERRUPT_STATUS) }
    }

    fn acknowledge_interrupt(&self, status: u32) {
        unsafe { self.write_reg(LEGACY_INTERRUPT_ACK, status) }
    }
}

// ── 工厂函数 ─────────────────────────────────────────────────────────────────

/// 读取 Magic+Version，返回对应版本的 transport 实现。
pub fn detect(mmio_base: usize) -> Result<Box<dyn VirtioMmioTransport>, &'static str> {
    let magic = unsafe { core::ptr::read_volatile((mmio_base + 0x000) as *const u32) };
    if magic != VIRTIO_MMIO_MAGIC_VALUE {
        return Err("Invalid VirtIO magic value");
    }
    let version = unsafe { core::ptr::read_volatile((mmio_base + 0x004) as *const u32) };
    match version {
        1 => Ok(Box::new(LegacyMmioTransport::new(mmio_base))),
        2 => Ok(Box::new(ModernMmioTransport::new(mmio_base))),
        _ => Err("Unsupported VirtIO version"),
    }
}
