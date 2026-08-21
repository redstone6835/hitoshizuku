#![no_std]

//! VirtIO 公共传输与 split virtqueue 支持。
//!
//! 本模块只放 VirtIO 协议本身的公共概念：PCI 传输层设备匹配、capability 类型、
//! device status 位，以及 split virtqueue 的 DMA 布局和描述符记账。具体驱动仍负责
//! 选择队列编号、编程设备寄存器，并在 [`SplitVirtQueue::push_avail`] 后通知设备。

extern crate alloc;

use alloc::vec::Vec;
use core::mem;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use general::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use general::dev::pci::{PciBarType, PciDevice};

pub mod virtio_mmio;

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;

/// VirtIO over PCI 的 vendor id。
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;

/// 现代 VirtIO PCI capability 使用 PCI vendor-specific capability 承载。
pub const VIRTIO_PCI_CAP_VENDOR_SPECIFIC: u8 = 0x09;
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// VirtIO PCI capability 基础头长度；notify capability 额外包含 multiplier。
pub const VIRTIO_PCI_CAP_BASE_LEN: u8 = 16;
pub const VIRTIO_PCI_CAP_NOTIFY_LEN: u8 = 20;
pub const VIRTIO_PCI_CAP_LEN_OFFSET: u16 = 2;
pub const VIRTIO_PCI_CAP_CFG_TYPE_OFFSET: u16 = 3;
pub const VIRTIO_PCI_CAP_BAR_OFFSET: u16 = 4;
pub const VIRTIO_PCI_CAP_MMIO_OFFSET: u16 = 8;
pub const VIRTIO_PCI_CAP_MMIO_LENGTH_OFFSET: u16 = 12;
pub const VIRTIO_PCI_CAP_NOTIFY_MULT_OFFSET: u16 = 16;
pub const VIRTIO_PCI_CAP_BAR_INDEX_MASK: u8 = 0x7;

/// VirtIO common_cfg 中的 device status 位。
pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
pub const VIRTIO_STATUS_DRIVER: u8 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
pub const VIRTIO_STATUS_FAILED: u8 = 128;

/// 所有 modern VirtIO 设备都必须支持的基础 feature。
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// split virtqueue event suppression。
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;
/// reset 后等待 status 清零的默认自旋上限。
pub const VIRTIO_PCI_RESET_SPIN_LIMIT: u32 = 1_000_000;

// ── common_cfg 寄存器布局 ──────────────────────────────────────────────

const VIRTIO_CC_DEVICE_FEATURE_SELECT: usize = 0x00;
const VIRTIO_CC_DEVICE_FEATURE: usize = 0x04;
const VIRTIO_CC_DRIVER_FEATURE_SELECT: usize = 0x08;
const VIRTIO_CC_DRIVER_FEATURE: usize = 0x0c;
const VIRTIO_CC_CONFIG_MSIX_VECTOR: usize = 0x10;
const VIRTIO_CC_DEVICE_STATUS: usize = 0x14;
const VIRTIO_CC_QUEUE_SELECT: usize = 0x16;
const VIRTIO_CC_QUEUE_SIZE: usize = 0x18;
const VIRTIO_CC_QUEUE_MSIX_VECTOR: usize = 0x1a;
const VIRTIO_CC_QUEUE_ENABLE: usize = 0x1c;
const VIRTIO_CC_QUEUE_NOTIFY_OFF: usize = 0x1e;
const VIRTIO_CC_QUEUE_DESC: usize = 0x20;
const VIRTIO_CC_QUEUE_DRIVER: usize = 0x28;
const VIRTIO_CC_QUEUE_DEVICE: usize = 0x30;
pub const VIRTIO_MSI_NO_VECTOR: u16 = u16::MAX;

/// VirtIO PCI function 描述。
///
/// 这里描述的是 VirtIO over PCI 传输层的设备 ID 投影，不是 `/dev` 节点、
/// 主次设备号或其它 POSIX 兼容层概念。驱动持有一个描述对象来表达“我要绑定
/// 哪类 VirtIO function”，避免各驱动重复散落 vendor/device id 判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioPciFunction {
    /// 调试和日志用的稳定名称。
    pub name: &'static str,
    /// transitional 设备使用的旧 ID。
    pub legacy_transitional_device_id: u16,
    /// non-transitional modern 设备使用的新 ID。
    pub modern_device_id: u16,
}

impl VirtioPciFunction {
    /// 创建一个 VirtIO PCI function 描述。
    pub const fn new(
        name: &'static str,
        legacy_transitional_device_id: u16,
        modern_device_id: u16,
    ) -> Self {
        Self {
            name,
            legacy_transitional_device_id,
            modern_device_id,
        }
    }

    /// 判断 PCI vendor/device id 是否属于该 VirtIO 设备类型。
    pub const fn matches_pci_ids(self, vendor: u16, device_id: u16) -> bool {
        vendor == VIRTIO_PCI_VENDOR_ID
            && (device_id == self.legacy_transitional_device_id
                || device_id == self.modern_device_id)
    }
}

/// VirtIO PCI capability 对应的一段 BAR 内 MMIO 窗口。
#[derive(Clone, Copy, Debug)]
pub struct VirtioPciCap {
    /// 虚拟地址基址：BAR 映射基址加 capability offset。
    pub vaddr: usize,
    /// capability 声明的可访问字节长度。
    pub length: u32,
    /// notify capability 专用的队列通知偏移倍率；其它 capability 固定为 0。
    pub notify_off_multiplier: u32,
}

impl VirtioPciCap {
    /// 判断 `[offset, offset + len)` 是否完全落在该 capability 窗口内。
    pub fn covers(self, offset: usize, len: usize) -> bool {
        offset
            .checked_add(len)
            .is_some_and(|end| end <= self.length as usize)
    }

    /// 计算窗口内偏移对应的虚拟地址，同时校验访问范围不会越过 capability 边界。
    pub fn checked_addr(self, offset: usize, len: usize) -> Option<usize> {
        if !self.covers(offset, len) {
            return None;
        }
        self.vaddr.checked_add(offset)
    }
}

/// 已解析的 VirtIO PCI capability 集合。
#[derive(Clone, Copy, Debug)]
pub struct VirtioPciCaps {
    pub common: VirtioPciCap,
    pub notify: VirtioPciCap,
    pub isr: VirtioPciCap,
    pub device: Option<VirtioPciCap>,
}

/// 从 PCI capability chain 中解析 modern VirtIO 传输窗口。
///
/// 本函数只处理 VirtIO PCI 传输层通用规则：必须是 vendor-specific
/// capability、长度满足基础结构要求、BAR 必须是 MMIO、offset/length 不能越过
/// BAR 边界。各设备类型的寄存器访问范围仍由具体驱动按自己的 common/device
/// config 使用方式继续校验。
pub fn parse_virtio_pci_caps(pci: &PciDevice) -> Option<VirtioPciCaps> {
    let mut common: Option<VirtioPciCap> = None;
    let mut notify: Option<VirtioPciCap> = None;
    let mut isr: Option<VirtioPciCap> = None;
    let mut device: Option<VirtioPciCap> = None;

    for cap_header in pci
        .capabilities_snapshot()
        .into_iter()
        .filter(|cap| cap.id == VIRTIO_PCI_CAP_VENDOR_SPECIFIC)
    {
        let ptr = cap_header.offset;
        let cap_len = match pci.try_read_config_u8(ptr + VIRTIO_PCI_CAP_LEN_OFFSET) {
            Ok(cap_len) => cap_len,
            Err(_) => continue,
        };
        let cfg_type = match pci.try_read_config_u8(ptr + VIRTIO_PCI_CAP_CFG_TYPE_OFFSET) {
            Ok(cfg_type) => cfg_type,
            Err(_) => continue,
        };
        let min_len = if cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG {
            VIRTIO_PCI_CAP_NOTIFY_LEN
        } else {
            VIRTIO_PCI_CAP_BASE_LEN
        };
        if cap_len < min_len {
            continue;
        }

        let bar_idx = match pci.try_read_config_u8(ptr + VIRTIO_PCI_CAP_BAR_OFFSET) {
            Ok(raw) => raw & VIRTIO_PCI_CAP_BAR_INDEX_MASK,
            Err(_) => continue,
        };
        let offset = match pci.try_read_config_u32(ptr + VIRTIO_PCI_CAP_MMIO_OFFSET) {
            Ok(offset) => offset,
            Err(_) => continue,
        };
        let length = match pci.try_read_config_u32(ptr + VIRTIO_PCI_CAP_MMIO_LENGTH_OFFSET) {
            Ok(length) => length,
            Err(_) => continue,
        };
        if length == 0 {
            continue;
        }

        let Some((bar, bar_vaddr)) = pci.map_bar_virt(bar_idx as usize) else {
            continue;
        };
        if !matches!(bar.bar_type, PciBarType::Memory) {
            continue;
        }
        let Some(end) = (offset as u64).checked_add(length as u64) else {
            continue;
        };
        if end > bar.size {
            continue;
        }
        let Some(vaddr) = bar_vaddr.checked_add(offset as usize) else {
            continue;
        };

        let cap = VirtioPciCap {
            vaddr,
            length,
            notify_off_multiplier: 0,
        };
        match cfg_type {
            VIRTIO_PCI_CAP_COMMON_CFG => common = Some(cap),
            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                let notify_off_multiplier =
                    match pci.try_read_config_u32(ptr + VIRTIO_PCI_CAP_NOTIFY_MULT_OFFSET) {
                        Ok(multiplier) => multiplier,
                        Err(_) => continue,
                    };
                notify = Some(VirtioPciCap {
                    notify_off_multiplier,
                    ..cap
                });
            }
            VIRTIO_PCI_CAP_ISR_CFG => isr = Some(cap),
            VIRTIO_PCI_CAP_DEVICE_CFG => device = Some(cap),
            _ => {}
        }
    }

    Some(VirtioPciCaps {
        common: common?,
        notify: notify?,
        isr: isr?,
        device,
    })
}

/// VirtIO PCI common_cfg 访问错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtioPciTransportError {
    CommonTooShort,
    NotifyTooShort,
    IsrTooShort,
    NotifyOffsetOverflow,
    NotifyOutOfRange,
    NotifyAddressOverflow,
    MsixVectorRejected,
}

/// 已校验的 VirtIO PCI transport 访问器。
///
/// 该类型只封装 transport 层标准寄存器访问；设备类型相关的 config 布局、
/// feature 选择和队列中描述符语义仍由具体驱动负责。
#[derive(Clone, Copy, Debug)]
pub struct VirtioPciTransport {
    caps: VirtioPciCaps,
}

impl VirtioPciTransport {
    /// 创建 transport 访问器，并验证基础 common/notify/isr 窗口覆盖标准寄存器。
    pub fn new(caps: VirtioPciCaps) -> Result<Self, VirtioPciTransportError> {
        if !caps
            .common
            .covers(0, VIRTIO_CC_QUEUE_DEVICE + mem::size_of::<u64>())
        {
            return Err(VirtioPciTransportError::CommonTooShort);
        }
        if !caps.notify.covers(0, mem::size_of::<u16>()) {
            return Err(VirtioPciTransportError::NotifyTooShort);
        }
        if !caps.isr.covers(0, mem::size_of::<u8>()) {
            return Err(VirtioPciTransportError::IsrTooShort);
        }
        Ok(Self { caps })
    }

    pub const fn caps(&self) -> VirtioPciCaps {
        self.caps
    }

    pub fn status(&self) -> u8 {
        rd_u8(self.caps.common.vaddr + VIRTIO_CC_DEVICE_STATUS)
    }

    pub fn set_status(&self, value: u8) {
        wr_u8(self.caps.common.vaddr + VIRTIO_CC_DEVICE_STATUS, value);
    }

    pub fn add_status(&self, bit: u8) {
        self.set_status(self.status() | bit);
    }

    /// 写 0 reset，并在给定自旋次数内等待设备清零 status。
    pub fn reset_wait(&self, spin_limit: u32) -> bool {
        self.set_status(0);
        for _ in 0..spin_limit {
            if self.status() == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        self.status() == 0
    }

    pub fn device_features(&self) -> u64 {
        wr_u32(self.caps.common.vaddr + VIRTIO_CC_DEVICE_FEATURE_SELECT, 0);
        let lo = rd_u32(self.caps.common.vaddr + VIRTIO_CC_DEVICE_FEATURE) as u64;
        wr_u32(self.caps.common.vaddr + VIRTIO_CC_DEVICE_FEATURE_SELECT, 1);
        let hi = rd_u32(self.caps.common.vaddr + VIRTIO_CC_DEVICE_FEATURE) as u64;
        (hi << 32) | lo
    }

    pub fn set_driver_features(&self, features: u64) {
        wr_u32(self.caps.common.vaddr + VIRTIO_CC_DRIVER_FEATURE_SELECT, 0);
        wr_u32(
            self.caps.common.vaddr + VIRTIO_CC_DRIVER_FEATURE,
            features as u32,
        );
        wr_u32(self.caps.common.vaddr + VIRTIO_CC_DRIVER_FEATURE_SELECT, 1);
        wr_u32(
            self.caps.common.vaddr + VIRTIO_CC_DRIVER_FEATURE,
            (features >> 32) as u32,
        );
    }

    pub fn select_queue(&self, queue_idx: u16) {
        wr_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_SELECT, queue_idx);
    }

    pub fn selected_queue_size(&self) -> u16 {
        rd_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_SIZE)
    }

    pub fn set_selected_queue_size(&self, queue_size: u16) {
        wr_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_SIZE, queue_size);
    }

    pub fn set_config_msix_vector(&self, vector: u16) -> Result<(), VirtioPciTransportError> {
        wr_u16(
            self.caps.common.vaddr + VIRTIO_CC_CONFIG_MSIX_VECTOR,
            vector,
        );
        (rd_u16(self.caps.common.vaddr + VIRTIO_CC_CONFIG_MSIX_VECTOR) != VIRTIO_MSI_NO_VECTOR)
            .then_some(())
            .ok_or(VirtioPciTransportError::MsixVectorRejected)
    }

    pub fn set_selected_queue_msix_vector(
        &self,
        vector: u16,
    ) -> Result<(), VirtioPciTransportError> {
        wr_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_MSIX_VECTOR, vector);
        (rd_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_MSIX_VECTOR) != VIRTIO_MSI_NO_VECTOR)
            .then_some(())
            .ok_or(VirtioPciTransportError::MsixVectorRejected)
    }

    pub fn set_selected_queue_addresses(&self, desc: u64, driver: u64, device: u64) {
        wr_u64(self.caps.common.vaddr + VIRTIO_CC_QUEUE_DESC, desc);
        wr_u64(self.caps.common.vaddr + VIRTIO_CC_QUEUE_DRIVER, driver);
        wr_u64(self.caps.common.vaddr + VIRTIO_CC_QUEUE_DEVICE, device);
    }

    pub fn selected_queue_notify_addr(&self) -> Result<usize, VirtioPciTransportError> {
        let notify_off = rd_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_NOTIFY_OFF) as usize;
        let notify_offset = notify_off
            .checked_mul(self.caps.notify.notify_off_multiplier as usize)
            .ok_or(VirtioPciTransportError::NotifyOffsetOverflow)?;
        self.caps
            .notify
            .checked_addr(notify_offset, mem::size_of::<u16>())
            .ok_or_else(|| {
                if self
                    .caps
                    .notify
                    .covers(notify_offset, mem::size_of::<u16>())
                {
                    VirtioPciTransportError::NotifyAddressOverflow
                } else {
                    VirtioPciTransportError::NotifyOutOfRange
                }
            })
    }

    pub fn enable_selected_queue(&self) {
        wr_u16(self.caps.common.vaddr + VIRTIO_CC_QUEUE_ENABLE, 1);
    }

    pub fn notify_queue(&self, notify_addr: usize, queue_idx: u16) {
        wr_u16(notify_addr, queue_idx);
    }

    /// 读取 ISR capability。对 VirtIO PCI 来说该读操作同时完成设备侧 ack。
    pub fn isr_status(&self) -> u8 {
        rd_u8(self.caps.isr.vaddr)
    }
}

#[inline]
fn rd_u8(addr: usize) -> u8 {
    unsafe { read_volatile(addr as *const u8) }
}

#[inline]
fn wr_u8(addr: usize, value: u8) {
    unsafe { write_volatile(addr as *mut u8, value) }
}

#[inline]
fn rd_u16(addr: usize) -> u16 {
    unsafe { read_volatile(addr as *const u16) }
}

#[inline]
fn wr_u16(addr: usize, value: u16) {
    unsafe { write_volatile(addr as *mut u16, value) }
}

#[inline]
fn rd_u32(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[inline]
fn wr_u32(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

#[inline]
fn wr_u64(addr: usize, value: u64) {
    // common_cfg 中的 64 位队列地址按低 32 位、高 32 位顺序写入，避免依赖平台
    // 对 MMIO u64 原子写的支持。
    wr_u32(addr, value as u32);
    wr_u32(addr + 4, (value >> 32) as u32);
}

const DESC_ALIGN: usize = 16;
const AVAIL_ALIGN: usize = 2;
const USED_ALIGN: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl VirtqDesc {
    pub const fn new(addr: u64, len: u32, flags: u16, next: u16) -> Self {
        Self {
            addr,
            len,
            flags,
            next,
        }
    }
}

/// 一次描述符表更新。
///
/// 调用方先按协议准备好描述符链，再把若干更新一次性交给队列。队列会先校验所有
/// descriptor 仍处于 InUse 状态，最后只做一次描述符表同步，避免非 coherent DMA
/// 平台在提交热路径中对每段 descriptor 反复同步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtqDescUpdate {
    pub index: u16,
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: Option<u16>,
}

impl VirtqDescUpdate {
    pub const fn new(index: u16, addr: u64, len: u32, flags: u16, next: Option<u16>) -> Self {
        Self {
            index,
            addr,
            len,
            flags,
            next,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtqAvailHeader {
    pub flags: u16,
    pub idx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtqUsedHeader {
    pub flags: u16,
    pub idx: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtQueueError {
    QueueSizeZero,
    QueueSizeNotPowerOfTwo,
    LayoutOverflow,
    HostAllocationFailed,
    DmaAllocationFailed(&'static str),
    DescriptorCountZero,
    DescriptorCountTooLarge,
    QueueFull,
    DescriptorOutOfRange,
    DescriptorNotAllocated,
    DescriptorAlreadyFree,
    DuplicateDescriptor,
    InvalidNextDescriptor,
    InvalidUsedDescriptor,
    UsedRingOverrun,
    CorruptFreeList,
}

/// 描述符链内联保存的描述符数量。
///
/// split virtqueue 的块设备请求通常是 header/data/status 三段；网络队列最多使用
/// 一个 VirtIO header 加 18 个 packet fragment。覆盖这两种规范上限后，正常 I/O
/// 提交路径不需要临时堆分配；更长的非网络链仍可退化到 `Vec`。
pub const INLINE_DESCRIPTOR_CHAIN: usize = 19;

#[derive(Debug)]
enum DescriptorChainStorage {
    Inline {
        len: usize,
        descriptors: [u16; INLINE_DESCRIPTOR_CHAIN],
    },
    Heap(Vec<u16>),
}

#[derive(Debug)]
pub struct DescriptorChain {
    head: u16,
    storage: DescriptorChainStorage,
}

impl DescriptorChain {
    fn from_slice(descriptors: &[u16]) -> Result<Self, VirtQueueError> {
        let Some(head) = descriptors.first().copied() else {
            return Err(VirtQueueError::DescriptorCountZero);
        };
        let storage = if descriptors.len() <= INLINE_DESCRIPTOR_CHAIN {
            let mut inline = [0; INLINE_DESCRIPTOR_CHAIN];
            inline[..descriptors.len()].copy_from_slice(descriptors);
            DescriptorChainStorage::Inline {
                len: descriptors.len(),
                descriptors: inline,
            }
        } else {
            let mut heap = Vec::new();
            reserve_total(&mut heap, descriptors.len())?;
            heap.extend_from_slice(descriptors);
            DescriptorChainStorage::Heap(heap)
        };
        Ok(Self { head, storage })
    }

    fn from_vec(descriptors: Vec<u16>) -> Result<Self, VirtQueueError> {
        let Some(head) = descriptors.first().copied() else {
            return Err(VirtQueueError::DescriptorCountZero);
        };
        if descriptors.len() <= INLINE_DESCRIPTOR_CHAIN {
            Self::from_slice(descriptors.as_slice())
        } else {
            Ok(Self {
                head,
                storage: DescriptorChainStorage::Heap(descriptors),
            })
        }
    }

    pub const fn head(&self) -> u16 {
        self.head
    }

    pub fn len(&self) -> usize {
        match &self.storage {
            DescriptorChainStorage::Inline { len, .. } => *len,
            DescriptorChainStorage::Heap(descriptors) => descriptors.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, offset: usize) -> Option<u16> {
        match &self.storage {
            DescriptorChainStorage::Inline { len, descriptors } => {
                (offset < *len).then(|| descriptors[offset])
            }
            DescriptorChainStorage::Heap(descriptors) => descriptors.get(offset).copied(),
        }
    }

    pub fn as_slice(&self) -> &[u16] {
        match &self.storage {
            DescriptorChainStorage::Inline { len, descriptors } => &descriptors[..*len],
            DescriptorChainStorage::Heap(descriptors) => descriptors.as_slice(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsedChain {
    pub head: u16,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescState {
    Free,
    InUse,
}

pub struct SplitVirtQueue {
    queue_size: u16,
    dma_context: DmaContext,
    desc: DmaBuffer,
    avail: DmaBuffer,
    used: DmaBuffer,
    last_used_idx: u16,
    free_desc: Vec<u16>,
    desc_state: Vec<DescState>,
}

pub type VirtQueue = SplitVirtQueue;

impl SplitVirtQueue {
    pub fn new(queue_size: u16) -> Result<Self, VirtQueueError> {
        Self::new_in(DmaContext::default_coherent(), queue_size)
    }

    /// 使用指定设备 DMA 上下文创建 split virtqueue。
    ///
    /// virtqueue 的三段 ring 必须和后续请求 buffer 使用同一个设备 DMA 视图，
    /// 否则 IOMMU/地址窗口平台上描述符地址和数据地址可能属于不同地址空间。
    pub fn new_in(dma_context: DmaContext, queue_size: u16) -> Result<Self, VirtQueueError> {
        // VirtIO split queue 的环大小必须统一在这里校验：
        // 非 0 且为 2 的幂，后续取模和 wrap-around 逻辑依赖该约束。
        let qsz = validate_queue_size(queue_size)?;
        let desc_len = desc_table_bytes(qsz)?;
        let avail_len = avail_ring_bytes(qsz)?;
        let used_len = used_ring_bytes(qsz)?;

        let desc = DmaBuffer::new_in(dma_context, desc_len, DESC_ALIGN, DmaDirection::ToDevice)
            .map_err(VirtQueueError::DmaAllocationFailed)?;
        let avail = DmaBuffer::new_in(dma_context, avail_len, AVAIL_ALIGN, DmaDirection::ToDevice)
            .map_err(VirtQueueError::DmaAllocationFailed)?;
        let used = DmaBuffer::new_in(dma_context, used_len, USED_ALIGN, DmaDirection::FromDevice)
            .map_err(VirtQueueError::DmaAllocationFailed)?;

        let mut queue = Self {
            queue_size,
            dma_context,
            desc,
            avail,
            used,
            last_used_idx: 0,
            free_desc: Vec::new(),
            desc_state: Vec::new(),
        };
        queue.clear()?;
        Ok(queue)
    }

    /// 创建一个兼容 VirtIO legacy (MMIO v1) 的 split virtqueue。
    ///
    /// Legacy 传输要求 descriptor table、available ring、used ring 放在
    /// 一段物理连续的内存中，通过 QueuePFN 寄存器一次性告知设备。
    /// 布局：desc_table | avail_ring | used_ring（按各自对齐要求）。
    pub fn new_legacy(queue_size: u16) -> Result<Self, VirtQueueError> {
        Self::new_legacy_in(DmaContext::default_coherent(), queue_size)
    }

    /// 使用指定设备 DMA 上下文创建 legacy split virtqueue。
    pub fn new_legacy_in(dma_context: DmaContext, queue_size: u16) -> Result<Self, VirtQueueError> {
        let qsz = validate_queue_size(queue_size)?;
        let desc_len = desc_table_bytes(qsz)?;
        let avail_len = avail_ring_bytes(qsz)?;
        let used_len = used_ring_bytes(qsz)?;

        let page_align = DESC_ALIGN.max(AVAIL_ALIGN).max(USED_ALIGN).max(4096);
        // Legacy 要求 Used Ring 页对齐：在 avail 和 used 之间可能有填充
        let used_off = (desc_len + avail_len).next_multiple_of(4096);
        let total = used_off + used_len;
        let buf = DmaBuffer::new_in(dma_context, total, page_align, DmaDirection::Bidirectional)
            .map_err(VirtQueueError::DmaAllocationFailed)?;

        let base_dma = buf.dma_addr();
        let base_paddr = buf.paddr();
        let base_vaddr = buf.vaddr();
        let master_alloc = buf.take_allocation();

        // desc 持有主分配（drop 时释放整段）；avail/used 是零分配视图
        let desc = DmaBuffer::from_allocation_in(
            dma_context,
            master_alloc,
            base_dma,
            base_vaddr,
            desc_len,
            DmaDirection::ToDevice,
        );
        // Legacy 规范要求 Used Ring 页对齐
        let used_off = (desc_len + avail_len).next_multiple_of(4096);
        let avail = DmaBuffer::sub_view_in(
            dma_context,
            base_dma + desc_len,
            base_vaddr + desc_len,
            base_paddr + desc_len,
            avail_len,
        );
        let used = DmaBuffer::sub_view_in(
            dma_context,
            base_dma + used_off,
            base_vaddr + used_off,
            base_paddr + used_off,
            used_len,
        );

        let mut queue = Self {
            queue_size,
            dma_context,
            desc,
            avail,
            used,
            last_used_idx: 0,
            free_desc: Vec::new(),
            desc_state: Vec::new(),
        };
        queue.clear()?;
        Ok(queue)
    }

    pub const fn dma_context(&self) -> DmaContext {
        self.dma_context
    }

    pub const fn queue_size(&self) -> u16 {
        self.queue_size
    }

    pub const fn desc_dma_addr(&self) -> usize {
        self.desc.dma_addr()
    }

    pub const fn avail_dma_addr(&self) -> usize {
        self.avail.dma_addr()
    }

    pub const fn used_dma_addr(&self) -> usize {
        self.used.dma_addr()
    }

    /// 返回 split ring 的 `avail.flags` 常驻地址，供常驻 IRQ top-half 抑制中断。
    pub const fn avail_flags_addr(&self) -> usize {
        self.avail.vaddr()
    }

    /// EVENT_IDX 模式下驱动写入的 `used_event` 地址。
    pub fn used_event_addr(&self) -> Result<usize, VirtQueueError> {
        Ok(self.used_event_ptr()? as usize)
    }

    /// EVENT_IDX adapter 读取设备 `used.idx` 的地址。
    pub const fn used_idx_addr(&self) -> usize {
        self.used.vaddr() + mem::size_of::<u16>()
    }

    pub fn desc_len(&self) -> usize {
        self.desc.len()
    }

    pub fn avail_len(&self) -> usize {
        self.avail.len()
    }

    pub fn used_len(&self) -> usize {
        self.used.len()
    }

    pub fn free_descriptor_count(&self) -> usize {
        self.free_desc.len()
    }

    pub fn clear(&mut self) -> Result<(), VirtQueueError> {
        self.desc.as_mut_slice().fill(0);
        self.avail.as_mut_slice().fill(0);
        self.used.as_mut_slice().fill(0);
        self.last_used_idx = 0;

        let qsz = self.queue_len();
        reserve_total(&mut self.free_desc, qsz)?;
        reserve_total(&mut self.desc_state, qsz)?;

        self.free_desc.clear();
        self.desc_state.clear();
        for idx in (0..qsz).rev() {
            self.free_desc.push(idx as u16);
        }
        for _ in 0..qsz {
            self.desc_state.push(DescState::Free);
        }

        self.desc.sync_for_device();
        self.avail.sync_for_device();
        self.used.sync_for_device();
        Ok(())
    }

    pub fn alloc_chain(&mut self, count: usize) -> Result<DescriptorChain, VirtQueueError> {
        if count == 0 {
            return Err(VirtQueueError::DescriptorCountZero);
        }
        if count > self.queue_len() {
            return Err(VirtQueueError::DescriptorCountTooLarge);
        }
        if self.free_desc.len() < count {
            return Err(VirtQueueError::QueueFull);
        }

        let mut descriptors = [0; INLINE_DESCRIPTOR_CHAIN];
        let mut heap_descriptors = Vec::new();
        if count > INLINE_DESCRIPTOR_CHAIN {
            reserve_total(&mut heap_descriptors, count)?;
        }
        let mut allocated = 0;

        for _ in 0..count {
            let idx = match self.free_desc.pop() {
                Some(idx) => idx,
                None => {
                    self.rollback_allocated(&descriptors, &heap_descriptors, allocated);
                    return Err(VirtQueueError::CorruptFreeList);
                }
            };

            let Some(state) = self.desc_state.get_mut(usize::from(idx)) else {
                self.free_desc.push(idx);
                self.rollback_allocated(&descriptors, &heap_descriptors, allocated);
                return Err(VirtQueueError::CorruptFreeList);
            };
            if *state != DescState::Free {
                self.free_desc.push(idx);
                self.rollback_allocated(&descriptors, &heap_descriptors, allocated);
                return Err(VirtQueueError::CorruptFreeList);
            }

            *state = DescState::InUse;
            if count <= INLINE_DESCRIPTOR_CHAIN {
                descriptors[allocated] = idx;
            } else {
                heap_descriptors.push(idx);
            }
            allocated += 1;
        }

        if count <= INLINE_DESCRIPTOR_CHAIN {
            DescriptorChain::from_slice(&descriptors[..count])
        } else {
            DescriptorChain::from_vec(heap_descriptors)
        }
    }

    pub fn free_chain(&mut self, chain: DescriptorChain) -> Result<(), VirtQueueError> {
        self.free_descriptor_slice(chain.as_slice())
    }

    pub fn free_chain_from_head(&mut self, head: u16) -> Result<(), VirtQueueError> {
        self.check_descriptor_in_use(head)?;
        let queue_len = self.queue_len();
        reserve_total(&mut self.free_desc, queue_len)?;

        let mut descriptors = [0; INLINE_DESCRIPTOR_CHAIN];
        let mut heap_descriptors = Vec::new();
        let mut current = head;
        for depth in 0..queue_len {
            if descriptor_record_contains(depth, &descriptors, &heap_descriptors, current) {
                return Err(VirtQueueError::DuplicateDescriptor);
            }
            self.check_descriptor_in_use(current)?;
            let desc = self.read_desc(current)?;
            record_descriptor(current, depth, &mut descriptors, &mut heap_descriptors)?;

            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                // 完整校验链表后再统一释放，避免损坏链导致一半描述符被回收到空闲表。
                self.release_descriptor_record(&descriptors, &heap_descriptors, depth + 1)?;
                self.desc.sync_for_device();
                return Ok(());
            }

            let next = desc.next;
            if usize::from(next) >= self.queue_len() {
                return Err(VirtQueueError::InvalidNextDescriptor);
            }
            if descriptor_record_contains(depth + 1, &descriptors, &heap_descriptors, next) {
                return Err(VirtQueueError::DuplicateDescriptor);
            }
            current = next;
        }

        Err(VirtQueueError::InvalidNextDescriptor)
    }

    pub fn write_desc(
        &mut self,
        index: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: Option<u16>,
    ) -> Result<(), VirtQueueError> {
        let update = VirtqDescUpdate::new(index, addr, len, flags, next);
        self.write_descs(core::slice::from_ref(&update))
    }

    /// 批量写入一组 split virtqueue descriptor。
    ///
    /// 这是块设备等热路径的推荐入口：先完整校验，再连续写入，最后只同步一次
    /// descriptor table。单个 descriptor 写入仍通过 [`Self::write_desc`] 复用这里。
    pub fn write_descs(&mut self, updates: &[VirtqDescUpdate]) -> Result<(), VirtQueueError> {
        if updates.is_empty() {
            return Err(VirtQueueError::DescriptorCountZero);
        }
        for (pos, update) in updates.iter().enumerate() {
            self.check_descriptor_in_use(update.index)?;
            for prev in &updates[..pos] {
                if prev.index == update.index {
                    return Err(VirtQueueError::DuplicateDescriptor);
                }
            }
            if let Some(next_idx) = update.next {
                self.check_descriptor_in_use(next_idx)?;
            }
        }

        for update in updates.iter().copied() {
            let mut desc_flags = update.flags & !VIRTQ_DESC_F_NEXT;
            let next_idx = match update.next {
                Some(next_idx) => {
                    desc_flags |= VIRTQ_DESC_F_NEXT;
                    next_idx
                }
                None => 0,
            };
            self.write_desc_raw(
                update.index,
                VirtqDesc::new(update.addr, update.len, desc_flags, next_idx),
            )?;
        }

        self.desc.sync_for_device();
        Ok(())
    }

    pub fn read_desc(&self, index: u16) -> Result<VirtqDesc, VirtQueueError> {
        let ptr = self.desc_ptr(index)?;
        Ok(unsafe { read_volatile(ptr.cast_const()) })
    }

    pub fn push_avail(&mut self, head: u16) -> Result<(), VirtQueueError> {
        self.push_avail_many(core::slice::from_ref(&head))
    }

    /// 批量发布一组 descriptor head 到 available ring。
    ///
    /// descriptor table 在 `write_descs` / `write_desc` 中已经同步给设备；这里仅负责
    /// 把 head 写入 avail ring 并推进 avail.idx。批量入口把多次 ring 写入合并成
    /// 一次 DMA 同步和一次 idx 更新，减少顺序 I/O 或未来合并提交路径上的门铃前
    /// CPU 开销。调用方仍负责在返回成功后按传输层规则通知设备。
    pub fn push_avail_many(&mut self, heads: &[u16]) -> Result<(), VirtQueueError> {
        if heads.is_empty() {
            return Err(VirtQueueError::DescriptorCountZero);
        }
        if heads.len() > self.queue_len() {
            return Err(VirtQueueError::DescriptorCountTooLarge);
        }
        for (pos, head) in heads.iter().copied().enumerate() {
            self.check_descriptor_in_use(head)?;
            if contains_descriptor_prefix(heads, pos, head) {
                return Err(VirtQueueError::DuplicateDescriptor);
            }
        }

        let qsz = self.queue_len();
        let avail_idx = self.avail_idx();
        for (offset, head) in heads.iter().copied().enumerate() {
            let slot = usize::from(avail_idx.wrapping_add(offset as u16)) % qsz;
            let ring_ptr = self.avail_ring_ptr(slot)?;
            unsafe {
                write_volatile(ring_ptr, head);
            }
        }

        // 保证设备看到更新后的 idx 前，描述符表和 ring slot 内容已经对设备可见。
        // 非 coherent 平台由 sync_for_device 执行 cache clean；coherent 平台则依赖
        // release fence 约束 CPU 写入顺序。
        fence(Ordering::Release);
        self.set_avail_idx(avail_idx.wrapping_add(heads.len() as u16));
        self.avail.sync_for_device();
        Ok(())
    }

    pub fn pop_used(&mut self) -> Result<Option<UsedChain>, VirtQueueError> {
        self.used.sync_for_cpu();
        fence(Ordering::Acquire);

        let used_idx = self.used_idx();
        if self.last_used_idx == used_idx {
            return Ok(None);
        }

        let pending = used_idx.wrapping_sub(self.last_used_idx);
        if usize::from(pending) > self.queue_len() {
            return Err(VirtQueueError::UsedRingOverrun);
        }

        let slot = usize::from(self.last_used_idx) % self.queue_len();
        let elem = unsafe { read_volatile(self.used_ring_ptr(slot)?.cast_const()) };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        if elem.id > u16::MAX as u32 {
            return Err(VirtQueueError::InvalidUsedDescriptor);
        }
        let head = elem.id as u16;
        if usize::from(head) >= self.queue_len() {
            return Err(VirtQueueError::InvalidUsedDescriptor);
        }
        self.check_descriptor_in_use(head)?;

        Ok(Some(UsedChain {
            head,
            len: elem.len,
        }))
    }

    /// 不推进 consumer index，判断设备是否发布了新的 used element。
    pub fn has_used(&self) -> Result<bool, VirtQueueError> {
        self.used.sync_for_cpu();
        fence(Ordering::Acquire);
        let used_idx = self.used_idx();
        let pending = used_idx.wrapping_sub(self.last_used_idx);
        if usize::from(pending) > self.queue_len() {
            return Err(VirtQueueError::UsedRingOverrun);
        }
        Ok(pending != 0)
    }

    pub fn set_avail_flags(&mut self, flags: u16) {
        unsafe {
            write_volatile(self.avail_flags_ptr(), flags);
        }
        self.avail.sync_for_device();
    }

    pub fn used_flags(&self) -> u16 {
        self.used.sync_for_cpu();
        unsafe { read_volatile(self.used_flags_ptr().cast_const()) }
    }

    pub fn set_used_event(&mut self, idx: u16) -> Result<(), VirtQueueError> {
        unsafe {
            write_volatile(self.used_event_ptr()?, idx);
        }
        self.avail.sync_for_device();
        Ok(())
    }

    pub fn avail_event(&self) -> Result<u16, VirtQueueError> {
        self.used.sync_for_cpu();
        Ok(unsafe { read_volatile(self.avail_event_ptr()?.cast_const()) })
    }

    fn queue_len(&self) -> usize {
        usize::from(self.queue_size)
    }

    fn rollback_allocated(
        &mut self,
        inline_descriptors: &[u16; INLINE_DESCRIPTOR_CHAIN],
        heap_descriptors: &[u16],
        allocated: usize,
    ) {
        // 长链分配时所有已取出的 descriptor 都记录在 heap fallback 中；
        // 小链才使用 inline 数组。回滚路径不能按长度猜测存储形态，否则长链
        // 中途失败会错误回收 inline 数组里的默认值。
        if !heap_descriptors.is_empty() {
            for idx in heap_descriptors.iter().take(allocated).copied() {
                self.rollback_descriptor_raw(idx);
            }
            return;
        }

        let inline_len = allocated.min(INLINE_DESCRIPTOR_CHAIN);
        for idx in inline_descriptors.iter().take(inline_len).copied() {
            self.rollback_descriptor_raw(idx);
        }
    }

    fn free_descriptor_slice(&mut self, descriptors: &[u16]) -> Result<(), VirtQueueError> {
        if descriptors.is_empty() {
            return Err(VirtQueueError::DescriptorCountZero);
        }
        let new_free_len = self
            .free_desc
            .len()
            .checked_add(descriptors.len())
            .ok_or(VirtQueueError::LayoutOverflow)?;
        if new_free_len > self.queue_len() {
            return Err(VirtQueueError::CorruptFreeList);
        }
        reserve_total(&mut self.free_desc, new_free_len)?;

        for (pos, idx) in descriptors.iter().copied().enumerate() {
            if usize::from(idx) >= self.queue_len() {
                return Err(VirtQueueError::DescriptorOutOfRange);
            }
            if contains_descriptor_prefix(descriptors, pos, idx) {
                return Err(VirtQueueError::DuplicateDescriptor);
            }
            match self.desc_state.get(usize::from(idx)) {
                Some(DescState::InUse) => {}
                Some(DescState::Free) => return Err(VirtQueueError::DescriptorAlreadyFree),
                None => return Err(VirtQueueError::DescriptorOutOfRange),
            }
        }

        for idx in descriptors.iter().copied() {
            self.release_descriptor_raw(idx)?;
        }
        self.desc.sync_for_device();
        Ok(())
    }

    fn release_descriptor_raw(&mut self, idx: u16) -> Result<(), VirtQueueError> {
        self.write_desc_raw(idx, VirtqDesc::default())?;
        self.rollback_descriptor_raw(idx);
        Ok(())
    }

    fn rollback_descriptor_raw(&mut self, idx: u16) {
        if let Some(state) = self.desc_state.get_mut(usize::from(idx)) {
            *state = DescState::Free;
        }
        self.free_desc.push(idx);
    }

    fn release_descriptor_record(
        &mut self,
        inline_descriptors: &[u16; INLINE_DESCRIPTOR_CHAIN],
        heap_descriptors: &[u16],
        len: usize,
    ) -> Result<(), VirtQueueError> {
        let inline_len = len.min(INLINE_DESCRIPTOR_CHAIN);
        for idx in inline_descriptors.iter().take(inline_len).copied() {
            self.release_descriptor_raw(idx)?;
        }
        if len > INLINE_DESCRIPTOR_CHAIN {
            for idx in heap_descriptors
                .iter()
                .take(len - INLINE_DESCRIPTOR_CHAIN)
                .copied()
            {
                self.release_descriptor_raw(idx)?;
            }
        }
        Ok(())
    }

    fn check_descriptor_in_use(&self, index: u16) -> Result<(), VirtQueueError> {
        if usize::from(index) >= self.queue_len() {
            return Err(VirtQueueError::DescriptorOutOfRange);
        }
        match self.desc_state.get(usize::from(index)) {
            Some(DescState::InUse) => Ok(()),
            Some(DescState::Free) => Err(VirtQueueError::DescriptorNotAllocated),
            None => Err(VirtQueueError::DescriptorOutOfRange),
        }
    }

    fn write_desc_raw(&mut self, index: u16, desc: VirtqDesc) -> Result<(), VirtQueueError> {
        let ptr = self.desc_ptr(index)?;
        unsafe {
            write_volatile(ptr, desc);
        }
        Ok(())
    }

    fn desc_ptr(&self, index: u16) -> Result<*mut VirtqDesc, VirtQueueError> {
        let idx = usize::from(index);
        if idx >= self.queue_len() {
            return Err(VirtQueueError::DescriptorOutOfRange);
        }
        let offset = mem::size_of::<VirtqDesc>()
            .checked_mul(idx)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        ptr_at(self.desc.vaddr(), offset)
    }

    fn avail_flags_ptr(&self) -> *mut u16 {
        self.avail.vaddr() as *mut u16
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        (self.avail.vaddr() + mem::size_of::<u16>()) as *mut u16
    }

    fn used_flags_ptr(&self) -> *mut u16 {
        self.used.vaddr() as *mut u16
    }

    fn used_idx_ptr(&self) -> *mut u16 {
        (self.used.vaddr() + mem::size_of::<u16>()) as *mut u16
    }

    pub fn avail_idx(&self) -> u16 {
        unsafe { read_volatile(self.avail_idx_ptr().cast_const()) }
    }

    fn set_avail_idx(&mut self, idx: u16) {
        unsafe {
            write_volatile(self.avail_idx_ptr(), idx);
        }
    }

    pub fn used_idx(&self) -> u16 {
        unsafe { read_volatile(self.used_idx_ptr().cast_const()) }
    }

    fn avail_ring_ptr(&self, slot: usize) -> Result<*mut u16, VirtQueueError> {
        if slot >= self.queue_len() {
            return Err(VirtQueueError::DescriptorOutOfRange);
        }
        let ring_offset = mem::size_of::<VirtqAvailHeader>();
        let elem_offset = mem::size_of::<u16>()
            .checked_mul(slot)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        let offset = ring_offset
            .checked_add(elem_offset)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        ptr_at(self.avail.vaddr(), offset)
    }

    fn used_ring_ptr(&self, slot: usize) -> Result<*mut VirtqUsedElem, VirtQueueError> {
        if slot >= self.queue_len() {
            return Err(VirtQueueError::DescriptorOutOfRange);
        }
        let ring_offset = mem::size_of::<VirtqUsedHeader>();
        let elem_offset = mem::size_of::<VirtqUsedElem>()
            .checked_mul(slot)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        let offset = ring_offset
            .checked_add(elem_offset)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        ptr_at(self.used.vaddr(), offset)
    }

    fn used_event_ptr(&self) -> Result<*mut u16, VirtQueueError> {
        let ring_bytes = mem::size_of::<u16>()
            .checked_mul(self.queue_len())
            .ok_or(VirtQueueError::LayoutOverflow)?;
        let offset = mem::size_of::<VirtqAvailHeader>()
            .checked_add(ring_bytes)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        ptr_at(self.avail.vaddr(), offset)
    }

    fn avail_event_ptr(&self) -> Result<*mut u16, VirtQueueError> {
        let ring_bytes = mem::size_of::<VirtqUsedElem>()
            .checked_mul(self.queue_len())
            .ok_or(VirtQueueError::LayoutOverflow)?;
        let offset = mem::size_of::<VirtqUsedHeader>()
            .checked_add(ring_bytes)
            .ok_or(VirtQueueError::LayoutOverflow)?;
        ptr_at(self.used.vaddr(), offset)
    }
}

/// VirtIO 1.2 `vring_need_event()` 的 wrapping index 判定。
pub const fn virtq_need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// 根据设备上报的最大队列大小选择 split virtqueue 实际大小。
///
/// VirtIO split queue 的 ring 取模逻辑要求队列大小为 2 的幂；这里把“从设备
/// 能力中挑一个可用大小”的策略集中到公共层，避免各个传输驱动各自硬编码 128/256。
pub fn choose_split_queue_size(
    max_size: u16,
    preferred_limit: Option<u16>,
) -> Result<u16, VirtQueueError> {
    if max_size == 0 {
        return Err(VirtQueueError::QueueSizeZero);
    }
    let limit = preferred_limit
        .filter(|limit| *limit != 0)
        .map(|limit| limit.min(max_size))
        .unwrap_or(max_size);
    let queue_size = highest_power_of_two_at_most(limit);
    if queue_size == 0 {
        return Err(VirtQueueError::QueueSizeZero);
    }
    Ok(queue_size)
}

fn validate_queue_size(queue_size: u16) -> Result<usize, VirtQueueError> {
    if queue_size == 0 {
        return Err(VirtQueueError::QueueSizeZero);
    }
    if !queue_size.is_power_of_two() {
        return Err(VirtQueueError::QueueSizeNotPowerOfTwo);
    }
    Ok(usize::from(queue_size))
}

fn desc_table_bytes(qsz: usize) -> Result<usize, VirtQueueError> {
    mem::size_of::<VirtqDesc>()
        .checked_mul(qsz)
        .ok_or(VirtQueueError::LayoutOverflow)
}

fn avail_ring_bytes(qsz: usize) -> Result<usize, VirtQueueError> {
    let ring_bytes = mem::size_of::<u16>()
        .checked_mul(qsz)
        .ok_or(VirtQueueError::LayoutOverflow)?;
    mem::size_of::<VirtqAvailHeader>()
        .checked_add(ring_bytes)
        .and_then(|len| len.checked_add(mem::size_of::<u16>()))
        .ok_or(VirtQueueError::LayoutOverflow)
}

fn used_ring_bytes(qsz: usize) -> Result<usize, VirtQueueError> {
    let ring_bytes = mem::size_of::<VirtqUsedElem>()
        .checked_mul(qsz)
        .ok_or(VirtQueueError::LayoutOverflow)?;
    mem::size_of::<VirtqUsedHeader>()
        .checked_add(ring_bytes)
        .and_then(|len| len.checked_add(mem::size_of::<u16>()))
        .ok_or(VirtQueueError::LayoutOverflow)
}

fn ptr_at<T>(base: usize, offset: usize) -> Result<*mut T, VirtQueueError> {
    base.checked_add(offset)
        .map(|addr| addr as *mut T)
        .ok_or(VirtQueueError::LayoutOverflow)
}

fn reserve_total<T>(vec: &mut Vec<T>, total: usize) -> Result<(), VirtQueueError> {
    if vec.capacity() < total {
        vec.try_reserve_exact(total - vec.capacity())
            .map_err(|_| VirtQueueError::HostAllocationFailed)?;
    }
    Ok(())
}

fn highest_power_of_two_at_most(value: u16) -> u16 {
    if value == 0 {
        return 0;
    }
    1u16 << (u16::BITS - 1 - value.leading_zeros())
}

fn contains_descriptor_prefix(descriptors: &[u16], len: usize, needle: u16) -> bool {
    descriptors.iter().take(len).any(|idx| *idx == needle)
}

fn record_descriptor(
    idx: u16,
    pos: usize,
    inline_descriptors: &mut [u16; INLINE_DESCRIPTOR_CHAIN],
    heap_descriptors: &mut Vec<u16>,
) -> Result<(), VirtQueueError> {
    if pos < INLINE_DESCRIPTOR_CHAIN {
        inline_descriptors[pos] = idx;
    } else {
        if heap_descriptors.len() == heap_descriptors.capacity() {
            heap_descriptors
                .try_reserve_exact(1)
                .map_err(|_| VirtQueueError::HostAllocationFailed)?;
        }
        heap_descriptors.push(idx);
    }
    Ok(())
}

fn descriptor_record_contains(
    len: usize,
    inline_descriptors: &[u16; INLINE_DESCRIPTOR_CHAIN],
    heap_descriptors: &[u16],
    needle: u16,
) -> bool {
    let inline_len = len.min(INLINE_DESCRIPTOR_CHAIN);
    if inline_descriptors
        .iter()
        .take(inline_len)
        .any(|idx| *idx == needle)
    {
        return true;
    }
    len > INLINE_DESCRIPTOR_CHAIN
        && heap_descriptors
            .iter()
            .take(len - INLINE_DESCRIPTOR_CHAIN)
            .any(|idx| *idx == needle)
}
