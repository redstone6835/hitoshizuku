//! Common VirtIO split virtqueue support.
//!
//! This module owns only the split virtqueue memory and descriptor bookkeeping.
//! Transports still select queue numbers, program device registers, and notify
//! devices after [`SplitVirtQueue::push_avail`].

use alloc::vec::Vec;
use core::mem;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use crate::dev::dma::{DmaBuffer, DmaDirection};

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;

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

#[derive(Debug)]
pub struct DescriptorChain {
    head: u16,
    descriptors: Vec<u16>,
}

impl DescriptorChain {
    fn new(descriptors: Vec<u16>) -> Result<Self, VirtQueueError> {
        let head = match descriptors.first() {
            Some(head) => *head,
            None => return Err(VirtQueueError::DescriptorCountZero),
        };
        Ok(Self { head, descriptors })
    }

    pub const fn head(&self) -> u16 {
        self.head
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    pub fn get(&self, offset: usize) -> Option<u16> {
        self.descriptors.get(offset).copied()
    }

    pub fn as_slice(&self) -> &[u16] {
        self.descriptors.as_slice()
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
        // VirtIO split queue 的环大小必须统一在这里校验：
        // 非 0 且为 2 的幂，后续取模和 wrap-around 逻辑依赖该约束。
        let qsz = validate_queue_size(queue_size)?;
        let desc_len = desc_table_bytes(qsz)?;
        let avail_len = avail_ring_bytes(qsz)?;
        let used_len = used_ring_bytes(qsz)?;

        let desc = DmaBuffer::new(desc_len, DESC_ALIGN, DmaDirection::ToDevice)
            .map_err(VirtQueueError::DmaAllocationFailed)?;
        let avail = DmaBuffer::new(avail_len, AVAIL_ALIGN, DmaDirection::ToDevice)
            .map_err(VirtQueueError::DmaAllocationFailed)?;
        let used = DmaBuffer::new(used_len, USED_ALIGN, DmaDirection::FromDevice)
            .map_err(VirtQueueError::DmaAllocationFailed)?;

        let mut queue = Self {
            queue_size,
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

    pub const fn queue_size(&self) -> u16 {
        self.queue_size
    }

    pub const fn desc_paddr(&self) -> usize {
        self.desc.paddr()
    }

    pub const fn avail_paddr(&self) -> usize {
        self.avail.paddr()
    }

    pub const fn used_paddr(&self) -> usize {
        self.used.paddr()
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

        let mut descriptors = Vec::new();
        reserve_total(&mut descriptors, count)?;

        for _ in 0..count {
            let idx = match self.free_desc.pop() {
                Some(idx) => idx,
                None => {
                    self.rollback_allocated(&descriptors);
                    return Err(VirtQueueError::CorruptFreeList);
                }
            };

            let Some(state) = self.desc_state.get_mut(usize::from(idx)) else {
                self.free_desc.push(idx);
                self.rollback_allocated(&descriptors);
                return Err(VirtQueueError::CorruptFreeList);
            };
            if *state != DescState::Free {
                self.free_desc.push(idx);
                self.rollback_allocated(&descriptors);
                return Err(VirtQueueError::CorruptFreeList);
            }

            *state = DescState::InUse;
            descriptors.push(idx);
        }

        DescriptorChain::new(descriptors)
    }

    pub fn free_chain(&mut self, chain: DescriptorChain) -> Result<(), VirtQueueError> {
        self.free_descriptor_slice(chain.as_slice())
    }

    pub fn free_chain_from_head(&mut self, head: u16) -> Result<(), VirtQueueError> {
        self.check_descriptor_in_use(head)?;

        let mut descriptors = Vec::new();
        let mut current = head;
        for _ in 0..self.queue_len() {
            push_checked(&mut descriptors, current)?;
            let desc = self.read_desc(current)?;
            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                let chain = DescriptorChain::new(descriptors)?;
                return self.free_chain(chain);
            }

            let next = desc.next;
            if usize::from(next) >= self.queue_len() {
                return Err(VirtQueueError::InvalidNextDescriptor);
            }
            if contains_descriptor(descriptors.as_slice(), next) {
                return Err(VirtQueueError::DuplicateDescriptor);
            }
            self.check_descriptor_in_use(next)?;
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
        self.check_descriptor_in_use(index)?;

        let mut desc_flags = flags & !VIRTQ_DESC_F_NEXT;
        let next_idx = match next {
            Some(next_idx) => {
                self.check_descriptor_in_use(next_idx)?;
                desc_flags |= VIRTQ_DESC_F_NEXT;
                next_idx
            }
            None => 0,
        };

        self.write_desc_raw(index, VirtqDesc::new(addr, len, desc_flags, next_idx))?;
        self.desc.sync_for_device();
        Ok(())
    }

    pub fn read_desc(&self, index: u16) -> Result<VirtqDesc, VirtQueueError> {
        let ptr = self.desc_ptr(index)?;
        Ok(unsafe { read_volatile(ptr.cast_const()) })
    }

    pub fn push_avail(&mut self, head: u16) -> Result<(), VirtQueueError> {
        self.check_descriptor_in_use(head)?;

        let qsz = self.queue_len();
        let avail_idx = self.avail_idx();
        let slot = usize::from(avail_idx) % qsz;
        let ring_ptr = self.avail_ring_ptr(slot)?;

        unsafe {
            write_volatile(ring_ptr, head);
        }
        self.desc.sync_for_device();
        self.avail.sync_for_device();
        fence(Ordering::Release);
        self.set_avail_idx(avail_idx.wrapping_add(1));
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

    fn rollback_allocated(&mut self, descriptors: &[u16]) {
        for idx in descriptors.iter().copied() {
            if let Some(state) = self.desc_state.get_mut(usize::from(idx)) {
                *state = DescState::Free;
            }
            self.free_desc.push(idx);
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
            self.write_desc_raw(idx, VirtqDesc::default())?;
            if let Some(state) = self.desc_state.get_mut(usize::from(idx)) {
                *state = DescState::Free;
            }
            self.free_desc.push(idx);
        }
        self.desc.sync_for_device();
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

    fn avail_idx(&self) -> u16 {
        unsafe { read_volatile(self.avail_idx_ptr().cast_const()) }
    }

    fn set_avail_idx(&mut self, idx: u16) {
        unsafe {
            write_volatile(self.avail_idx_ptr(), idx);
        }
    }

    fn used_idx(&self) -> u16 {
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

fn push_checked(vec: &mut Vec<u16>, idx: u16) -> Result<(), VirtQueueError> {
    if vec.len() == vec.capacity() {
        vec.try_reserve_exact(1)
            .map_err(|_| VirtQueueError::HostAllocationFailed)?;
    }
    vec.push(idx);
    Ok(())
}

fn contains_descriptor(descriptors: &[u16], needle: u16) -> bool {
    descriptors.iter().any(|idx| *idx == needle)
}

fn contains_descriptor_prefix(descriptors: &[u16], len: usize, needle: u16) -> bool {
    descriptors.iter().take(len).any(|idx| *idx == needle)
}
