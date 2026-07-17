//! pinned buffer pool、generation 校验与跨 CPU 回收。

use alloc::boxed::Box;
use core::cell::Cell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use super::PacketMetadata;

const SLOT_FREE: u8 = 0;
const SLOT_LEASED: u8 = 1;
const SLOT_SHARED: u8 = 2;
const SLOT_QUEUED: u8 = 3;
const SLOT_RETIRED: u8 = 4;
const EMPTY_SLOT: u16 = u16::MAX;
const HEAD_TAG_MAX: u64 = (1u64 << 48) - 1;

static NEXT_POOL_ID: AtomicU32 = AtomicU32::new(1);

/// 一次启动内单调且不复用的 pool 编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetBufPoolId(pub u32);

/// pool 内 slot 编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetBufId(pub u16);

/// slot 每次出租时更新的 generation。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetBufGeneration(pub u32);

/// pool 生命周期 generation。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct PoolGeneration(pub u32);

/// 架构无关的预分配 storage backend。
pub trait NetBufStorage: Send {
    fn capacity(&self) -> usize;
    fn base_ptr(&self) -> NonNull<u8>;
    fn dma_addr(&self) -> Option<u64>;
    fn sync_for_cpu(&self, offset: usize, len: usize);
    fn sync_for_device(&self, offset: usize, len: usize);
}

/// pool 构造或租借失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetBufPoolError {
    Empty,
    TooManySlots,
    StorageTooLarge,
    InvalidRange,
    Exhausted,
    Dying,
    StaleIdentity,
    RefcountOverflow,
    CorruptState,
}

struct Slot {
    storage: Box<dyn NetBufStorage>,
    generation: AtomicU32,
    refcount: AtomicU16,
    state: AtomicU8,
    recycle_next: AtomicU16,
}

impl Slot {
    fn new(storage: Box<dyn NetBufStorage>) -> Self {
        Self {
            storage,
            generation: AtomicU32::new(0),
            refcount: AtomicU16::new(0),
            state: AtomicU8::new(SLOT_FREE),
            recycle_next: AtomicU16::new(EMPTY_SLOT),
        }
    }
}

/// pool 诊断计数快照。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetBufPoolStats {
    pub local_recycle: u64,
    pub remote_recycle: u64,
    pub stale_identity: u64,
    pub refcount_error: u64,
    pub state_error: u64,
    pub retired_slots: u64,
}

struct PoolCounters {
    local_recycle: AtomicU64,
    remote_recycle: AtomicU64,
    stale_identity: AtomicU64,
    refcount_error: AtomicU64,
    state_error: AtomicU64,
    retired_slots: AtomicU64,
}

impl PoolCounters {
    const fn new() -> Self {
        Self {
            local_recycle: AtomicU64::new(0),
            remote_recycle: AtomicU64::new(0),
            stale_identity: AtomicU64::new(0),
            refcount_error: AtomicU64::new(0),
            state_error: AtomicU64::new(0),
            retired_slots: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> NetBufPoolStats {
        NetBufPoolStats {
            local_recycle: self.local_recycle.load(Ordering::Relaxed),
            remote_recycle: self.remote_recycle.load(Ordering::Relaxed),
            stale_identity: self.stale_identity.load(Ordering::Relaxed),
            refcount_error: self.refcount_error.load(Ordering::Relaxed),
            state_error: self.state_error.load(Ordering::Relaxed),
            retired_slots: self.retired_slots.load(Ordering::Relaxed),
        }
    }
}

/// 地址稳定的 pool 本体。
pub struct NetBufPool {
    id: NetBufPoolId,
    generation: AtomicU32,
    dying: AtomicBool,
    buffer_capacity: u16,
    slots: Box<[Slot]>,
    remote_head: AtomicU64,
    counters: PoolCounters,
    _pin: PhantomPinned,
}

// pool 的可变状态只通过原子和唯一 owner 修改；storage 的可变字节访问由
// lease/refcount 所有权协议约束，backend 在构造后不再移动。
unsafe impl Send for NetBufPool {}
unsafe impl Sync for NetBufPool {}

impl NetBufPool {
    /// 构造 pinned pool 和唯一 owner。任一 storage 不合规时整体失败。
    pub fn new(
        storages: Box<[Box<dyn NetBufStorage>]>,
    ) -> Result<NetBufPoolOwner, NetBufPoolError> {
        if storages.is_empty() {
            return Err(NetBufPoolError::Empty);
        }
        if storages.len() > (u16::MAX as usize - 1) {
            return Err(NetBufPoolError::TooManySlots);
        }
        if storages
            .iter()
            .any(|storage| storage.capacity() > u16::MAX as usize)
        {
            return Err(NetBufPoolError::StorageTooLarge);
        }

        let buffer_capacity = storages
            .iter()
            .map(|storage| storage.capacity() as u16)
            .min()
            .expect("非空 pool 必须拥有 storage");
        let raw_id = NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed);
        assert!(raw_id != 0, "NetBufPoolId 已耗尽");
        let slots = storages
            .into_vec()
            .into_iter()
            .map(Slot::new)
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        let pool = Box::pin(Self {
            id: NetBufPoolId(raw_id),
            generation: AtomicU32::new(1),
            dying: AtomicBool::new(false),
            buffer_capacity,
            slots,
            remote_head: AtomicU64::new(pack_head(0, EMPTY_SLOT)),
            counters: PoolCounters::new(),
            _pin: PhantomPinned,
        });
        let pool_ptr = NonNull::from(Pin::as_ref(&pool).get_ref());
        let capacity = pool.slots.len();
        let mut local_free = alloc::vec![EMPTY_SLOT; capacity].into_boxed_slice();
        for (index, entry) in local_free.iter_mut().enumerate() {
            *entry = (capacity - index - 1) as u16;
        }
        let owner = NetBufPoolOwner {
            pool,
            pool_ptr,
            local_free,
            local_len: capacity,
        };
        Ok(owner)
    }

    pub const fn id(&self) -> NetBufPoolId {
        self.id
    }

    pub fn generation(&self) -> PoolGeneration {
        PoolGeneration(self.generation.load(Ordering::Acquire))
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// 任意 slot 都能提供的最小连续字节数。
    pub const fn buffer_capacity(&self) -> u16 {
        self.buffer_capacity
    }

    pub fn outstanding(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.refcount.load(Ordering::Acquire) != 0)
            .count()
    }

    pub fn stats(&self) -> NetBufPoolStats {
        self.counters.snapshot()
    }

    fn slot(&self, id: NetBufId) -> Option<&Slot> {
        self.slots.get(id.0 as usize)
    }

    fn validate_identity(
        &self,
        pool: NetBufPoolId,
        pool_generation: PoolGeneration,
        buffer: NetBufId,
        generation: NetBufGeneration,
    ) -> Result<&Slot, NetBufPoolError> {
        let slot = self.slot(buffer).ok_or(NetBufPoolError::StaleIdentity)?;
        if self.id != pool
            || self.generation.load(Ordering::Acquire) != pool_generation.0
            || slot.generation.load(Ordering::Acquire) != generation.0
        {
            self.counters.stale_identity.fetch_add(1, Ordering::Relaxed);
            return Err(NetBufPoolError::StaleIdentity);
        }
        Ok(slot)
    }

    fn release_remote(
        &self,
        pool: NetBufPoolId,
        pool_generation: PoolGeneration,
        buffer: NetBufId,
        generation: NetBufGeneration,
    ) {
        let Some(slot) = self.slot(buffer) else {
            self.counters.stale_identity.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let current_pool_generation = self.generation.load(Ordering::Acquire);
        let release_generation_valid = pool_generation.0 == current_pool_generation
            || (self.dying.load(Ordering::Acquire)
                && pool_generation.0.checked_add(1) == Some(current_pool_generation));
        if self.id != pool
            || !release_generation_valid
            || slot.generation.load(Ordering::Acquire) != generation.0
        {
            self.counters.stale_identity.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut current = slot.refcount.load(Ordering::Acquire);
        loop {
            if current == 0 {
                self.counters.refcount_error.fetch_add(1, Ordering::Relaxed);
                return;
            }
            match slot.refcount.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) if current == 1 => {
                    self.enqueue_remote(buffer, slot);
                    return;
                }
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn enqueue_remote(&self, buffer: NetBufId, slot: &Slot) {
        let old_state = slot.state.swap(SLOT_QUEUED, Ordering::AcqRel);
        if old_state != SLOT_LEASED && old_state != SLOT_SHARED {
            self.counters.state_error.fetch_add(1, Ordering::Relaxed);
            slot.state.store(SLOT_RETIRED, Ordering::Release);
            return;
        }

        let mut head = self.remote_head.load(Ordering::Acquire);
        loop {
            let (tag, head_id) = unpack_head(head);
            assert!(tag != HEAD_TAG_MAX, "NetBuf remote recycle tag 已耗尽");
            slot.recycle_next.store(head_id, Ordering::Relaxed);
            let next = pack_head(tag + 1, buffer.0);
            match self.remote_head.compare_exchange_weak(
                head,
                next,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.counters.remote_recycle.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(actual) => head = actual,
            }
        }
    }
}

/// 只能由对应 NetWorker 持有的 pool owner。
pub struct NetBufPoolOwner {
    pool: Pin<Box<NetBufPool>>,
    pool_ptr: NonNull<NetBufPool>,
    local_free: Box<[u16]>,
    local_len: usize,
}

unsafe impl Send for NetBufPoolOwner {}

impl NetBufPoolOwner {
    pub fn pool(&self) -> &NetBufPool {
        self.pool.as_ref().get_ref()
    }

    pub fn pool_id(&self) -> NetBufPoolId {
        self.pool().id
    }

    pub fn available(&self) -> usize {
        self.local_len
    }

    pub fn buffer_capacity(&self) -> u16 {
        self.pool().buffer_capacity()
    }

    /// 在 poll turn 前后收割跨 CPU Drop 产生的回收链。
    pub fn drain_remote(&mut self) -> usize {
        let pool_ptr = self.pool_ptr;
        // SAFETY: pinned pool 在 owner 存活期间地址稳定。
        let pool = unsafe { pool_ptr.as_ref() };
        let detached = loop {
            let head = pool.remote_head.load(Ordering::Acquire);
            let (tag, _) = unpack_head(head);
            assert!(tag != HEAD_TAG_MAX, "NetBuf remote recycle tag 已耗尽");
            let empty = pack_head(tag + 1, EMPTY_SLOT);
            match pool.remote_head.compare_exchange_weak(
                head,
                empty,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break head,
                Err(_) => continue,
            }
        };

        let (_, mut current) = unpack_head(detached);
        let mut drained = 0usize;
        while current != EMPTY_SLOT {
            let slot = &pool.slots[current as usize];
            let next = slot.recycle_next.swap(EMPTY_SLOT, Ordering::Relaxed);
            if slot
                .state
                .compare_exchange(SLOT_QUEUED, SLOT_FREE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.push_local(current);
                drained += 1;
            } else {
                pool.counters.state_error.fetch_add(1, Ordering::Relaxed);
                slot.state.store(SLOT_RETIRED, Ordering::Release);
            }
            current = next;
        }
        drained
    }

    /// 租借一个独占 buffer，并设置有效数据范围。
    pub fn lease(
        &mut self,
        data_offset: u16,
        data_len: u16,
        metadata: PacketMetadata,
    ) -> Result<NetBufLease, NetBufPoolError> {
        let pool_ptr = self.pool_ptr;
        // SAFETY: pinned pool 在 owner 存活期间地址稳定。
        let pool = unsafe { pool_ptr.as_ref() };
        if pool.dying.load(Ordering::Acquire) {
            return Err(NetBufPoolError::Dying);
        }

        loop {
            let Some(id) = self.pop_local() else {
                return Err(NetBufPoolError::Exhausted);
            };
            let slot = &pool.slots[id as usize];
            let capacity = slot.storage.capacity();
            let end = usize::from(data_offset)
                .checked_add(usize::from(data_len))
                .ok_or(NetBufPoolError::InvalidRange)?;
            if end > capacity {
                self.push_local(id);
                return Err(NetBufPoolError::InvalidRange);
            }
            let current = slot.generation.load(Ordering::Relaxed);
            let Some(next_generation) = current.checked_add(1).filter(|next| *next != 0) else {
                slot.state.store(SLOT_RETIRED, Ordering::Release);
                pool.counters.retired_slots.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if slot
                .state
                .compare_exchange(SLOT_FREE, SLOT_LEASED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                pool.counters.state_error.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            slot.generation.store(next_generation, Ordering::Release);
            slot.refcount.store(1, Ordering::Release);
            return Ok(NetBufLease {
                pool: pool_ptr,
                pool_id: pool.id,
                pool_generation: pool.generation(),
                buffer: NetBufId(id),
                generation: NetBufGeneration(next_generation),
                data_offset,
                data_len,
                capacity: capacity as u16,
                metadata,
                _not_sync: PhantomData,
            });
        }
    }

    /// worker 本地显式回收，避免进入 remote MPSC。
    pub fn recycle_local(&mut self, lease: NetBufLease) -> Result<(), NetBufPoolError> {
        let lease = ManuallyDrop::new(lease);
        if lease.pool != self.pool_ptr {
            self.pool()
                .counters
                .stale_identity
                .fetch_add(1, Ordering::Relaxed);
            return Err(NetBufPoolError::StaleIdentity);
        }
        let pool_ptr = self.pool_ptr;
        // SAFETY: pinned pool 在 owner 存活期间地址稳定。
        let pool = unsafe { pool_ptr.as_ref() };
        let slot = pool.validate_identity(
            lease.pool_id,
            lease.pool_generation,
            lease.buffer,
            lease.generation,
        )?;
        if slot
            .refcount
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            pool.counters.refcount_error.fetch_add(1, Ordering::Relaxed);
            return Err(NetBufPoolError::CorruptState);
        }
        if slot
            .state
            .compare_exchange(SLOT_LEASED, SLOT_FREE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            pool.counters.state_error.fetch_add(1, Ordering::Relaxed);
            slot.state.store(SLOT_RETIRED, Ordering::Release);
            return Err(NetBufPoolError::CorruptState);
        }
        self.push_local(lease.buffer.0);
        pool.counters.local_recycle.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 属于当前 pool 的 lease 走本地快路径，否则交还给其原始 pool 的远程回收链。
    pub fn recycle_local_or_defer(&mut self, lease: NetBufLease) -> Result<(), NetBufPoolError> {
        if lease.pool != self.pool_ptr {
            drop(lease);
            return Ok(());
        }
        self.recycle_local(lease)
    }

    /// 停止新租借。detach 后续必须等待 outstanding 归零。
    pub fn begin_dying(&mut self) {
        let pool = self.pool();
        pool.dying.store(true, Ordering::Release);
        let current = pool.generation.load(Ordering::Acquire);
        if let Some(next) = current.checked_add(1).filter(|next| *next != 0) {
            pool.generation.store(next, Ordering::Release);
        }
    }

    pub fn outstanding(&self) -> usize {
        self.pool().outstanding()
    }

    fn pop_local(&mut self) -> Option<u16> {
        if self.local_len == 0 {
            return None;
        }
        self.local_len -= 1;
        Some(self.local_free[self.local_len])
    }

    fn push_local(&mut self, id: u16) {
        if self.local_len >= self.local_free.len() {
            self.pool()
                .counters
                .state_error
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.local_free[self.local_len] = id;
        self.local_len += 1;
    }
}

/// 独占 buffer lease。可跨 CPU 移动，但不可共享引用。
pub struct NetBufLease {
    pool: NonNull<NetBufPool>,
    pool_id: NetBufPoolId,
    pool_generation: PoolGeneration,
    buffer: NetBufId,
    generation: NetBufGeneration,
    data_offset: u16,
    data_len: u16,
    capacity: u16,
    metadata: PacketMetadata,
    _not_sync: PhantomData<Cell<()>>,
}

unsafe impl Send for NetBufLease {}

impl NetBufLease {
    pub const fn pool_id(&self) -> NetBufPoolId {
        self.pool_id
    }

    pub const fn buffer_id(&self) -> NetBufId {
        self.buffer
    }

    pub const fn generation(&self) -> NetBufGeneration {
        self.generation
    }

    pub const fn data_offset(&self) -> u16 {
        self.data_offset
    }

    pub const fn len(&self) -> usize {
        self.data_len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.data_len == 0
    }

    pub const fn capacity(&self) -> u16 {
        self.capacity
    }

    pub const fn metadata(&self) -> &PacketMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut PacketMetadata {
        &mut self.metadata
    }

    /// completion 后把 descriptor 覆盖范围收窄为实际 packet 数据。
    pub fn set_data_range(
        &mut self,
        data_offset: u16,
        data_len: u16,
    ) -> Result<(), NetBufPoolError> {
        let end = usize::from(data_offset)
            .checked_add(usize::from(data_len))
            .ok_or(NetBufPoolError::InvalidRange)?;
        if end > usize::from(self.capacity) {
            return Err(NetBufPoolError::InvalidRange);
        }
        self.data_offset = data_offset;
        self.data_len = data_len;
        Ok(())
    }

    pub fn as_slice(&self) -> Result<&[u8], NetBufPoolError> {
        let (slot, start, len) = self.validated_range()?;
        slot.storage.sync_for_cpu(start, len);
        // SAFETY: identity/range 已校验；lease 独占该 slot 的有效范围。
        Ok(
            unsafe {
                core::slice::from_raw_parts(slot.storage.base_ptr().as_ptr().add(start), len)
            },
        )
    }

    pub fn as_mut_slice(&mut self) -> Result<&mut [u8], NetBufPoolError> {
        let (slot, start, len) = self.validated_range()?;
        slot.storage.sync_for_cpu(start, len);
        // SAFETY: identity/range 已校验；&mut self 保证独占访问。
        Ok(unsafe {
            core::slice::from_raw_parts_mut(slot.storage.base_ptr().as_ptr().add(start), len)
        })
    }

    pub fn sync_for_device(&self) -> Result<(), NetBufPoolError> {
        let (slot, start, len) = self.validated_range()?;
        slot.storage.sync_for_device(start, len);
        Ok(())
    }

    /// 在当前有效数据前预留并清零协议头。
    pub fn prepend_zeroed(&mut self, len: u16) -> Result<(), NetBufPoolError> {
        if len > self.data_offset {
            return Err(NetBufPoolError::InvalidRange);
        }
        let new_len = self
            .data_len
            .checked_add(len)
            .ok_or(NetBufPoolError::InvalidRange)?;
        let new_offset = self.data_offset - len;
        if usize::from(new_offset) + usize::from(new_len) > usize::from(self.capacity) {
            return Err(NetBufPoolError::InvalidRange);
        }
        self.data_offset = new_offset;
        self.data_len = new_len;
        self.as_mut_slice()?[..usize::from(len)].fill(0);
        Ok(())
    }

    pub fn dma_addr(&self) -> Result<Option<u64>, NetBufPoolError> {
        let (slot, start, _) = self.validated_range()?;
        Ok(slot.storage.dma_addr().map(|base| base + start as u64))
    }

    /// 把独占 lease 转为第一个共享引用，不增加 refcount。
    pub fn into_chunk(self) -> Result<ChunkRef, NetBufPoolError> {
        let lease = ManuallyDrop::new(self);
        // SAFETY: pool 地址在 lease 生命周期内固定。
        let pool = unsafe { lease.pool.as_ref() };
        let slot = pool.validate_identity(
            lease.pool_id,
            lease.pool_generation,
            lease.buffer,
            lease.generation,
        )?;
        slot.state
            .compare_exchange(
                SLOT_LEASED,
                SLOT_SHARED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| NetBufPoolError::CorruptState)?;
        Ok(ChunkRef {
            pool: lease.pool,
            pool_id: lease.pool_id,
            pool_generation: lease.pool_generation,
            buffer: lease.buffer,
            generation: lease.generation,
            data_offset: lease.data_offset,
            data_len: lease.data_len,
            capacity: lease.capacity,
            metadata: lease.metadata,
        })
    }

    fn validated_range(&self) -> Result<(&Slot, usize, usize), NetBufPoolError> {
        // SAFETY: pool 地址在 lease 生命周期内固定。
        let pool = unsafe { self.pool.as_ref() };
        let slot = pool.validate_identity(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        )?;
        if slot.state.load(Ordering::Acquire) != SLOT_LEASED {
            return Err(NetBufPoolError::CorruptState);
        }
        let start = self.data_offset as usize;
        let len = self.data_len as usize;
        if start
            .checked_add(len)
            .is_none_or(|end| end > slot.storage.capacity())
        {
            return Err(NetBufPoolError::InvalidRange);
        }
        Ok((slot, start, len))
    }
}

impl Drop for NetBufLease {
    fn drop(&mut self) {
        // SAFETY: pool teardown 必须等待 outstanding 归零，因此 Drop 时地址有效。
        unsafe { self.pool.as_ref() }.release_remote(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        );
    }
}

/// 可由 descriptor、重传队列和 loopback receiver 共享的 chunk 引用。
pub struct ChunkRef {
    pool: NonNull<NetBufPool>,
    pool_id: NetBufPoolId,
    pool_generation: PoolGeneration,
    buffer: NetBufId,
    generation: NetBufGeneration,
    data_offset: u16,
    data_len: u16,
    capacity: u16,
    metadata: PacketMetadata,
}

unsafe impl Send for ChunkRef {}
unsafe impl Sync for ChunkRef {}

impl ChunkRef {
    /// 校验 generation 后增加 slot 引用。
    pub fn pin(&self) -> Result<Self, NetBufPoolError> {
        // SAFETY: pool teardown 必须等待全部引用归零。
        let pool = unsafe { self.pool.as_ref() };
        let slot = pool.validate_identity(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        )?;
        if slot.state.load(Ordering::Acquire) != SLOT_SHARED {
            pool.counters.state_error.fetch_add(1, Ordering::Relaxed);
            return Err(NetBufPoolError::CorruptState);
        }
        let mut refs = slot.refcount.load(Ordering::Acquire);
        loop {
            if refs == u16::MAX {
                return Err(NetBufPoolError::RefcountOverflow);
            }
            match slot.refcount.compare_exchange_weak(
                refs,
                refs + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => refs = actual,
            }
        }
        Ok(Self {
            pool: self.pool,
            pool_id: self.pool_id,
            pool_generation: self.pool_generation,
            buffer: self.buffer,
            generation: self.generation,
            data_offset: self.data_offset,
            data_len: self.data_len,
            capacity: self.capacity,
            metadata: self.metadata,
        })
    }

    pub const fn len(&self) -> usize {
        self.data_len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.data_len == 0
    }

    pub const fn metadata(&self) -> &PacketMetadata {
        &self.metadata
    }

    pub fn as_slice(&self) -> Result<&[u8], NetBufPoolError> {
        // SAFETY: pool teardown 必须等待全部引用归零。
        let pool = unsafe { self.pool.as_ref() };
        let slot = pool.validate_identity(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        )?;
        if slot.state.load(Ordering::Acquire) != SLOT_SHARED {
            return Err(NetBufPoolError::CorruptState);
        }
        let start = self.data_offset as usize;
        let len = self.data_len as usize;
        if start
            .checked_add(len)
            .is_none_or(|end| end > slot.storage.capacity())
        {
            return Err(NetBufPoolError::InvalidRange);
        }
        slot.storage.sync_for_cpu(start, len);
        // SAFETY:共享状态只暴露只读 slice，范围已校验。
        Ok(
            unsafe {
                core::slice::from_raw_parts(slot.storage.base_ptr().as_ptr().add(start), len)
            },
        )
    }

    pub fn dma_addr(&self) -> Result<Option<u64>, NetBufPoolError> {
        let pool = unsafe { self.pool.as_ref() };
        let slot = pool.validate_identity(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        )?;
        Ok(slot
            .storage
            .dma_addr()
            .map(|base| base + u64::from(self.data_offset)))
    }

    pub fn sync_for_device(&self) -> Result<(), NetBufPoolError> {
        let pool = unsafe { self.pool.as_ref() };
        let slot = pool.validate_identity(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        )?;
        slot.storage
            .sync_for_device(self.data_offset as usize, self.data_len as usize);
        Ok(())
    }
}

impl Drop for ChunkRef {
    fn drop(&mut self) {
        // SAFETY: pool teardown 必须等待全部引用归零，因此 Drop 时地址有效。
        unsafe { self.pool.as_ref() }.release_remote(
            self.pool_id,
            self.pool_generation,
            self.buffer,
            self.generation,
        );
    }
}

fn pack_head(tag: u64, slot: u16) -> u64 {
    (tag << 16) | u64::from(slot)
}

fn unpack_head(value: u64) -> (u64, u16) {
    (value >> 16, value as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    struct TestStorage {
        bytes: Box<[u8]>,
    }

    impl TestStorage {
        fn new(len: usize) -> Self {
            Self {
                bytes: vec![0; len].into_boxed_slice(),
            }
        }
    }

    impl NetBufStorage for TestStorage {
        fn capacity(&self) -> usize {
            self.bytes.len()
        }

        fn base_ptr(&self) -> NonNull<u8> {
            NonNull::new(self.bytes.as_ptr() as *mut u8).unwrap()
        }

        fn dma_addr(&self) -> Option<u64> {
            None
        }

        fn sync_for_cpu(&self, _offset: usize, _len: usize) {}
        fn sync_for_device(&self, _offset: usize, _len: usize) {}
    }

    fn make_pool(count: usize) -> NetBufPoolOwner {
        let storage = (0..count)
            .map(|_| Box::new(TestStorage::new(4096)) as Box<dyn NetBufStorage>)
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        NetBufPool::new(storage).unwrap()
    }

    #[test]
    fn local_and_remote_recycle_keep_pool_conserved() {
        let mut owner = make_pool(4);
        let first = owner.lease(128, 64, PacketMetadata::default()).unwrap();
        let first_id = first.buffer_id();
        owner.recycle_local(first).unwrap();
        assert_eq!(owner.available(), 4);

        let second = owner.lease(128, 64, PacketMetadata::default()).unwrap();
        assert_eq!(second.buffer_id(), first_id);
        drop(second);
        assert_eq!(owner.available(), 3);
        assert_eq!(owner.drain_remote(), 1);
        assert_eq!(owner.available(), 4);
        assert_eq!(owner.pool().outstanding(), 0);
        assert_eq!(owner.pool().stats().local_recycle, 1);
        assert_eq!(owner.pool().stats().remote_recycle, 1);
    }

    #[test]
    fn pool_reports_capacity_guaranteed_by_every_slot() {
        let storage = [4096, 2048, 8192]
            .into_iter()
            .map(|len| Box::new(TestStorage::new(len)) as Box<dyn NetBufStorage>)
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice();
        let owner = NetBufPool::new(storage).unwrap();
        assert_eq!(owner.buffer_capacity(), 2048);
    }

    #[test]
    fn foreign_lease_is_deferred_to_its_original_pool() {
        let mut rx_owner = make_pool(1);
        let mut tx_owner = make_pool(1);
        let lease = tx_owner.lease(0, 64, PacketMetadata::default()).unwrap();

        rx_owner.recycle_local_or_defer(lease).unwrap();

        assert_eq!(rx_owner.outstanding(), 0);
        assert_eq!(tx_owner.outstanding(), 0);
        assert_eq!(tx_owner.available(), 0);
        assert_eq!(tx_owner.drain_remote(), 1);
        assert_eq!(tx_owner.outstanding(), 0);
        assert_eq!(tx_owner.available(), 1);
    }

    #[test]
    fn chunk_pin_recycles_only_after_last_reference() {
        let mut owner = make_pool(1);
        let lease = owner.lease(0, 32, PacketMetadata::default()).unwrap();
        let chunk = lease.into_chunk().unwrap();
        let pinned = chunk.pin().unwrap();
        drop(chunk);
        assert_eq!(owner.drain_remote(), 0);
        assert_eq!(owner.pool().outstanding(), 1);
        drop(pinned);
        assert_eq!(owner.drain_remote(), 1);
        assert_eq!(owner.pool().outstanding(), 0);
    }

    #[test]
    fn generation_changes_on_every_lease() {
        let mut owner = make_pool(1);
        let first = owner.lease(0, 1, PacketMetadata::default()).unwrap();
        let first_generation = first.generation();
        owner.recycle_local(first).unwrap();
        let second = owner.lease(0, 1, PacketMetadata::default()).unwrap();
        assert_ne!(first_generation, second.generation());
    }

    #[test]
    fn dying_pool_accepts_final_drop_from_previous_pool_generation() {
        let mut owner = make_pool(1);
        let lease = owner.lease(0, 1, PacketMetadata::default()).expect("lease");
        owner.begin_dying();
        drop(lease);
        assert_eq!(owner.drain_remote(), 1);
        assert_eq!(owner.outstanding(), 0);
    }
}
