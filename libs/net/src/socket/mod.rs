//! 跨 CPU 稳定 socket facade、协议数据环与精确 readiness。

mod listen_group;
mod proxy;

pub use listen_group::ListenGroup;
pub use proxy::{NetSocketProxy, detach_proxy_stack};

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{
    AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

use sched::{TaskState, WaitQueue};
use spin::{Mutex, RwLock};

use crate::IpAddr;
use crate::buf::{
    ChunkRef, NetBufLease, PacketChain, PacketFragment, PacketMetadata, RxPoolPressure,
    SharedNetBufPool,
};
use crate::control::BindOptions;
use crate::{AddressFamily, Endpoint, FlowId, InterfaceId, ListenGroupId, ShardId, SocketId};

const UDP_RING_ENTRIES: usize = 256;
const UDP_BUFFER_BYTES: usize = 128 * 1024;
const UDP_BUFFER_HARD_LIMIT: usize = 512 * 1024;
const SOCKET_CHUNK_BYTES: usize = 4096;
const MAX_DATAGRAM_CHUNKS: usize = 17;
const LOCAL_DATAGRAM_BATCH_LIMIT: u16 = 4;
const LOCAL_DATAGRAM_INLINE_BYTES: usize = 256;
const MAX_UDP4_PAYLOAD: usize = 65_507;
const MAX_UDP6_PAYLOAD: usize = 65_527;
const TCP_BUFFER_BYTES: usize = 256 * 1024;
const TCP_BUFFER_HARD_LIMIT: usize = 1024 * 1024;
const TCP_LOCAL_AUTOTUNE_LIMIT: usize = TCP_BUFFER_HARD_LIMIT;
const TCP_LOCAL_READ_BATCH_BUDGET_BYTES: usize = 8 * 1024 * 1024;
const TCP_LOCAL_READ_BATCH_MIN_BYTES: usize = 1024 * 1024;
const TCP_LOCAL_SHARED_READ_BATCH_BYTES: usize = 1024 * 1024;
const TCP_LOCAL_IMMEDIATE_HANDOFF_BYTES: usize = SOCKET_CHUNK_BYTES;
const TCP_LOCAL_PRESSURE_BYTES: usize = 64 * 1024;
const TCP_LOCAL_DIRECT_RECONCILE_BYTES: u64 = 16 * 1024 * 1024;
const TCP_LOCAL_DIRECT_RECONCILE_EVENTS: u32 = 16 * 1024;
const TCP_LOCAL_DIRECT_COPY_BYTES: usize = 256 * 1024;
const TCP_INITIAL_CHUNKS: usize = 2;
const TCP_KEEPIDLE_DEFAULT_NS: u64 = 7_200_000_000_000;
const TCP_KEEPINTVL_DEFAULT_NS: u64 = 75_000_000_000;
const TCP_KEEPCNT_DEFAULT: u16 = 9;
static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);
static SOCKET_RUNTIME: RwLock<Option<&'static dyn SocketRuntime>> = RwLock::new(None);
static SOCKET_REGISTRY: RwLock<Vec<Weak<SocketFacade>>> = RwLock::new(Vec::new());
static DMA_TX_POOL_WAITERS: Mutex<VecDeque<DmaTxPoolWaiter>> = Mutex::new(VecDeque::new());
static LOCAL_TCP_BULK_SENDERS: AtomicUsize = AtomicUsize::new(0);

fn local_tcp_read_batch_bytes() -> usize {
    let senders = LOCAL_TCP_BULK_SENDERS.load(Ordering::Acquire).max(1);
    (TCP_LOCAL_READ_BATCH_BUDGET_BYTES / senders).max(TCP_LOCAL_READ_BATCH_MIN_BYTES)
}
static LOCAL_DATAGRAM_ROUTE_EPOCH: AtomicU64 = AtomicU64::new(1);
static ACTIVE_PACKET_OBSERVERS: AtomicU32 = AtomicU32::new(0);

struct DmaTxPoolWaiter {
    pool_key: usize,
    facade: Weak<SocketFacade>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerRef {
    Unassigned,
    Bound {
        generation: u32,
    },
    Listener {
        group: ListenGroupId,
        generation: u32,
    },
    Flow {
        shard: ShardId,
        flow: FlowId,
        generation: u32,
    },
    Closed {
        generation: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketKind {
    Datagram,
    Stream,
    Raw,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Readiness(pub u16);

impl Readiness {
    pub const READABLE: Self = Self(1 << 0);
    pub const WRITABLE: Self = Self(1 << 1);
    pub const ACCEPTABLE: Self = Self(1 << 2);
    pub const ERROR: Self = Self(1 << 3);
    pub const HANGUP: Self = Self(1 << 4);
    pub const READ_HANGUP: Self = Self(1 << 5);

    pub const fn raw(self) -> u16 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl core::ops::BitOr for Readiness {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

pub trait ReadinessObserver: Send + Sync {
    fn readiness_changed(&self, readiness: Readiness, generation: u64);

    fn readiness_updates_required(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketError {
    RuntimeUnavailable,
    RuntimeBusy,
    InvalidState,
    AddressInUse,
    AddressUnavailable,
    NotConnected,
    DestinationRequired,
    AlreadyConnected,
    AlreadyInProgress,
    InProgress,
    WouldBlock,
    Interrupted,
    TimedOut,
    MessageTooLarge,
    ReadShutdown,
    WriteShutdown,
    Closed,
    NetworkUnreachable,
    HostUnreachable,
    Buffer,
    ConnectionRefused,
    ConnectionReset,
    NetworkDown,
}

/// 数据报入队时区分 socket 状态错误与外部复制源错误。
pub enum DatagramCopyError<E> {
    Socket(SocketError),
    Copy(E),
}

#[derive(Clone)]
pub(crate) struct LocalDatagramRoute {
    epoch: u64,
    stack_generation: u64,
    sender_generation: u32,
    receiver_generation: u32,
    destination: Endpoint,
    source: Endpoint,
    delivered_to: Endpoint,
    interface: InterfaceId,
    dont_route: bool,
    confirm: bool,
    mark: u32,
    hop_limit: u8,
    traffic_class: u8,
    route_mtu: u32,
    receiver: Arc<SocketFacade>,
}

struct LocalTcpDirectRoute {
    local_generation: u32,
    peer_generation: u32,
    stack_generation: u64,
    local_owner: OwnerRef,
    peer_owner: OwnerRef,
    peer: Weak<SocketFacade>,
}

pub(crate) fn invalidate_local_datagram_routes() {
    LOCAL_DATAGRAM_ROUTE_EPOCH.fetch_add(1, Ordering::AcqRel);
}

fn local_datagram_route_epoch() -> u64 {
    LOCAL_DATAGRAM_ROUTE_EPOCH.load(Ordering::Acquire)
}

fn register_packet_observer(active: &AtomicBool, observers: &AtomicU32) -> bool {
    if active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let previous = observers.fetch_add(1, Ordering::AcqRel);
    assert!(previous != u32::MAX, "packet observer 计数溢出");
    true
}

fn unregister_packet_observer(active: &AtomicBool, observers: &AtomicU32) -> bool {
    if !active.swap(false, Ordering::AcqRel) {
        return false;
    }
    let previous = observers.fetch_sub(1, Ordering::AcqRel);
    assert!(previous != 0, "packet observer 计数下溢");
    true
}

#[cfg(test)]
fn facade_requires_packet_observation(facade: &SocketFacade) -> bool {
    facade.kind == SocketKind::Raw
        && !facade.closing.load(Ordering::Acquire)
        && !facade.stack_detached.load(Ordering::Acquire)
}

pub(crate) fn local_transport_fast_path_eligible() -> bool {
    packet_observers_allow_local_transport(&ACTIVE_PACKET_OBSERVERS)
}

fn packet_observers_allow_local_transport(observers: &AtomicU32) -> bool {
    observers.load(Ordering::Acquire) == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MulticastMembership {
    pub group: IpAddr,
    pub interface: Option<InterfaceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketErrorOrigin {
    Local,
    Icmp,
    Icmpv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketErrorRecord {
    pub sequence: u64,
    pub generation: u32,
    pub error: SocketError,
    pub origin: SocketErrorOrigin,
    pub kind: u8,
    pub code: u8,
    pub info: u32,
    pub offender: Option<Endpoint>,
}

pub enum SocketCommand {
    Bind {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    },
    Connect {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        nonblocking: bool,
    },
    Listen {
        facade: Arc<SocketFacade>,
        sequence: u64,
        generation: u32,
        backlog: u32,
    },
}

pub trait SocketRuntime: Send + Sync {
    fn submit_control(&self, command: SocketCommand) -> Result<(), SocketCommand>;
    fn prepare_stream_tx(&self, facade: &Arc<SocketFacade>);
    fn notify_tx(&self, facade: Arc<SocketFacade>, cause: SocketTxCause);
    fn notify_lifecycle(&self, facade: Arc<SocketFacade>);
    fn update_multicast(
        &self,
        facade: Arc<SocketFacade>,
        membership: MulticastMembership,
        joined: bool,
    ) -> Result<(), SocketError>;
    fn interface_by_name(&self, name: &[u8]) -> Option<InterfaceId>;
}

/// 一次发送侧协议推进请求的直接触发原因。
///
/// 该类型随请求跨过 socket/runtime 边界，既用于保留调度语义，也用于判断同一
/// syscall 内的第二次协议栈调用来自数据、状态还是排空竞态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SocketTxCause {
    Datagram = 1,
    StreamPayload = 2,
    StreamState = 3,
    DrainRecheck = 4,
    StreamLocalDirect = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallSocketRuntimeError {
    AlreadyInstalled,
}

pub fn install_socket_runtime(
    runtime: &'static dyn SocketRuntime,
) -> Result<(), InstallSocketRuntimeError> {
    let mut slot = SOCKET_RUNTIME.write();
    if slot.is_some() {
        return Err(InstallSocketRuntimeError::AlreadyInstalled);
    }
    *slot = Some(runtime);
    Ok(())
}

fn socket_runtime() -> Result<&'static dyn SocketRuntime, SocketError> {
    (*SOCKET_RUNTIME.read()).ok_or(SocketError::RuntimeUnavailable)
}

pub fn interface_by_name(name: &[u8]) -> Result<InterfaceId, SocketError> {
    socket_runtime()?
        .interface_by_name(name)
        .ok_or(SocketError::AddressUnavailable)
}

pub fn new_socket_facade(family: AddressFamily) -> Result<Arc<SocketFacade>, SocketError> {
    new_facade(family, SocketKind::Datagram)
}

pub fn new_tcp_socket_facade(family: AddressFamily) -> Result<Arc<SocketFacade>, SocketError> {
    new_facade(family, SocketKind::Stream)
}

pub fn new_raw_socket_facade(
    family: AddressFamily,
    protocol: u8,
) -> Result<Arc<SocketFacade>, SocketError> {
    if protocol == 0 {
        return Err(SocketError::InvalidState);
    }
    let boot = crate::stack::boot_config().ok_or(SocketError::RuntimeUnavailable)?;
    let boot_nonce = u64::from_le_bytes(boot.generation_nonce()[..8].try_into().unwrap());
    let counter = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    assert!(counter != 0, "SocketId 已耗尽");
    let _resident = enter_resident_allocation_scope()?;
    let facade = Arc::new(SocketFacade::new_with_protocol(
        SocketId {
            boot_nonce,
            counter,
        },
        family,
        SocketKind::Raw,
        protocol,
    ));
    Ok(facade)
}

fn new_facade(family: AddressFamily, kind: SocketKind) -> Result<Arc<SocketFacade>, SocketError> {
    let boot = crate::stack::boot_config().ok_or(SocketError::RuntimeUnavailable)?;
    let boot_nonce = u64::from_le_bytes(boot.generation_nonce()[..8].try_into().unwrap());
    let counter = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    assert!(counter != 0, "SocketId 已耗尽");
    let _resident = enter_resident_allocation_scope()?;
    let facade = Arc::new(SocketFacade::new(
        SocketId {
            boot_nonce,
            counter,
        },
        family,
        kind,
    ));
    Ok(facade)
}

fn enter_resident_allocation_scope()
-> Result<Option<elm_model::ElmCurrentContextGuard>, SocketError> {
    if elm_model::current_context().is_none() {
        return Ok(None);
    }
    // SocketFacade 可能由 ELM shard turn 为被动 accept 创建，但它的身份和缓冲必须在
    // ELM 卸载后继续由 resident VFS 持有。cell 0 是 allocator 的 kernel owner。
    let context = elm_model::ElmContext::new(
        elm_model::ElmId(0),
        None,
        elm_model::Generation::FIRST,
        elm_model::ElmState::Active,
        elm_model::ElmLifecyclePhase::Initialize,
        0,
    );
    elm_model::enter_current_context(&context)
        .map(Some)
        .ok_or(SocketError::Buffer)
}

/// INET socket 快照（/proc/net/{tcp,udp} 与观测接口用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InetSocketSnapshot {
    pub id: SocketId,
    pub kind: SocketKind,
    pub family: AddressFamily,
    pub local: Option<Endpoint>,
    pub peer: Option<Endpoint>,
    /// TCP 状态（仅 Stream；Datagram/Raw 为 0）。
    pub tcp_state: u8,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// 遍历全部存活 INET socket，生成观测快照。
pub fn snapshot_inet_sockets() -> Vec<InetSocketSnapshot> {
    let registry = SOCKET_REGISTRY.read();
    let mut out = Vec::with_capacity(registry.len());
    for entry in registry.iter() {
        let Some(facade) = entry.upgrade() else {
            continue;
        };
        let info = facade.tcp_info();
        out.push(InetSocketSnapshot {
            id: facade.id(),
            kind: facade.kind(),
            family: facade.family(),
            local: facade.local_endpoint(),
            peer: facade.peer_endpoint(),
            tcp_state: info.state,
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
        });
    }
    out
}

/// 将一个已经交给常驻 host/VFS 的 socket 纳入代际卸载跟踪。
pub fn track_socket_facade(facade: &Arc<SocketFacade>, generation: u64) {
    if generation == 0 {
        facade.detach_stack();
        return;
    }
    match facade.stack_generation.compare_exchange(
        0,
        generation,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(current) if current == generation => {}
        Err(_) => {
            facade.detach_stack();
            return;
        }
    }
    let mut registry = SOCKET_REGISTRY.write();
    let mut present = false;
    registry.retain(|entry| {
        let Some(existing) = entry.upgrade() else {
            return false;
        };
        present |= Arc::ptr_eq(&existing, facade);
        true
    });
    if !present {
        registry.push(Arc::downgrade(facade));
        if facade.activate_packet_observer() {
            invalidate_local_datagram_routes();
        }
    }
    drop(registry);

    let generation = facade.stack_generation();
    let current = crate::stack::stack_snapshot();
    if generation != 0
        && (current.state != crate::stack::NetStackState::Active
            || !current.ready
            || current.generation != generation)
    {
        facade.detach_stack();
    }
}

/// 使指定网络栈代际创建的全部 socket 进入稳定的断网状态。
pub fn detach_socket_generation(generation: u64) -> usize {
    if generation == 0 {
        return 0;
    }
    let sockets = {
        let mut registry = SOCKET_REGISTRY.write();
        let mut sockets = Vec::new();
        registry.retain(|entry| {
            let Some(facade) = entry.upgrade() else {
                return false;
            };
            if facade.stack_generation() == generation {
                sockets.push(facade);
            }
            true
        });
        sockets
    };
    for facade in &sockets {
        facade.detach_stack();
    }
    sockets.len()
}

struct ByteRing {
    arena: Box<[u8]>,
    head: usize,
    len: usize,
    limit: usize,
}

struct DmaTxChunk {
    start: u64,
    used: usize,
    exclusive: Option<NetBufLease>,
    shared: Option<ChunkRef>,
}

struct DmaByteRing {
    pool: SharedNetBufPool,
    chunks: VecDeque<DmaTxChunk>,
    len: usize,
    limit: usize,
}

impl DmaByteRing {
    fn new(pool: SharedNetBufPool, limit: usize) -> Self {
        Self {
            pool,
            chunks: VecDeque::with_capacity(TCP_INITIAL_CHUNKS),
            len: 0,
            limit,
        }
    }

    fn available(&self) -> usize {
        self.limit.saturating_sub(self.len)
    }

    fn writable(&self) -> bool {
        if self.available() == 0 {
            return false;
        }
        if self
            .chunks
            .back()
            .and_then(|chunk| chunk.exclusive.as_ref().map(|lease| (chunk, lease)))
            .is_some_and(|(chunk, lease)| chunk.used < usize::from(lease.capacity()))
        {
            return true;
        }
        let mut pool = self.pool.lock();
        pool.drain_remote();
        pool.available() != 0
    }

    fn reserve(&mut self, absolute_tail: u64) -> bool {
        if !self.writable() {
            return false;
        }
        if self
            .chunks
            .back()
            .and_then(|chunk| chunk.exclusive.as_ref().map(|lease| (chunk, lease)))
            .is_some_and(|(chunk, lease)| chunk.used < usize::from(lease.capacity()))
        {
            return true;
        }
        let mut pool = self.pool.lock();
        pool.drain_remote();
        let Ok(lease) = pool.lease(0, 0, PacketMetadata::default()) else {
            return false;
        };
        drop(pool);
        self.chunks.push_back(DmaTxChunk {
            start: absolute_tail,
            used: 0,
            exclusive: Some(lease),
            shared: None,
        });
        true
    }

    fn pool_key(&self) -> usize {
        Arc::as_ptr(&self.pool) as usize
    }

    fn pool_exhausted(&self) -> bool {
        if self.available() == 0
            || self
                .chunks
                .back()
                .and_then(|chunk| chunk.exclusive.as_ref().map(|lease| (chunk, lease)))
                .is_some_and(|(chunk, lease)| chunk.used < usize::from(lease.capacity()))
        {
            return false;
        }
        let mut pool = self.pool.lock();
        pool.drain_remote();
        pool.available() == 0
    }

    fn push(&mut self, absolute_tail: u64, input: &[u8]) -> usize {
        self.push_with(absolute_tail, input.len(), &mut |offset, output| {
            output.copy_from_slice(&input[offset..offset + output.len()]);
        })
    }

    fn push_with(
        &mut self,
        absolute_tail: u64,
        input_len: usize,
        copy: &mut impl FnMut(usize, &mut [u8]),
    ) -> usize {
        let target = input_len.min(self.available());
        let mut copied = 0usize;
        while copied < target {
            let appended = self.append_to_tail_with(target - copied, |output| {
                copy(copied, output);
            });
            if appended != 0 {
                copied += appended;
                continue;
            }

            let mut pool = self.pool.lock();
            pool.drain_remote();
            let capacity = usize::from(pool.buffer_capacity());
            let Ok(mut lease) = pool.lease(0, capacity as u16, PacketMetadata::default()) else {
                break;
            };
            drop(pool);
            let len = (target - copied).min(capacity);
            copy(
                copied,
                &mut lease.as_mut_slice().expect("socket TX DMA lease 范围有效")[..len],
            );
            lease
                .set_data_range(0, len as u16)
                .expect("socket TX DMA lease 收窄有效");
            self.chunks.push_back(DmaTxChunk {
                start: absolute_tail + copied as u64,
                used: len,
                exclusive: Some(lease),
                shared: None,
            });
            copied += len;
        }
        self.len += copied;
        copied
    }

    fn append_to_tail(&mut self, input: &[u8]) -> usize {
        self.append_to_tail_with(input.len(), |output| {
            output.copy_from_slice(&input[..output.len()]);
        })
    }

    fn append_to_tail_with(&mut self, input_len: usize, mut copy: impl FnMut(&mut [u8])) -> usize {
        let Some(tail) = self.chunks.back_mut() else {
            return 0;
        };
        let Some(lease) = tail.exclusive.as_mut() else {
            return 0;
        };
        let capacity = usize::from(lease.capacity());
        let len = input_len.min(capacity.saturating_sub(tail.used));
        if len == 0 {
            return 0;
        }
        lease
            .set_data_range(0, capacity as u16)
            .expect("socket TX DMA lease 扩展有效");
        copy(
            &mut lease.as_mut_slice().expect("socket TX DMA lease 范围有效")
                [tail.used..tail.used + len],
        );
        tail.used += len;
        lease
            .set_data_range(0, tail.used as u16)
            .expect("socket TX DMA lease 收窄有效");
        len
    }

    fn copy_range(&self, absolute: u64, output: &mut [u8]) -> bool {
        let Some(end) = absolute.checked_add(output.len() as u64) else {
            return false;
        };
        let mut copied = 0usize;
        for chunk in &self.chunks {
            let chunk_end = chunk.start + chunk.used as u64;
            if absolute >= chunk_end || end <= chunk.start {
                continue;
            }
            let start = absolute.saturating_sub(chunk.start) as usize;
            let stop = chunk.used.min((end - chunk.start) as usize);
            let bytes = match (&chunk.exclusive, &chunk.shared) {
                (Some(lease), None) => lease.as_slice(),
                (None, Some(shared)) => shared.as_slice(),
                _ => return false,
            };
            let Ok(bytes) = bytes else {
                return false;
            };
            let len = stop - start;
            output[copied..copied + len].copy_from_slice(&bytes[start..stop]);
            copied += len;
            if copied == output.len() {
                return true;
            }
        }
        copied == output.len()
    }

    fn pin_range(&mut self, absolute: u64, len: usize) -> Result<PacketChain, SocketError> {
        let end = absolute
            .checked_add(len as u64)
            .ok_or(SocketError::Buffer)?;
        let mut chain = PacketChain::new();
        let mut pinned = 0usize;
        for chunk in &mut self.chunks {
            let chunk_end = chunk.start + chunk.used as u64;
            if absolute >= chunk_end || end <= chunk.start {
                continue;
            }
            if chunk.shared.is_none() {
                let lease = chunk.exclusive.take().ok_or(SocketError::Buffer)?;
                chunk.shared = Some(lease.into_chunk().map_err(|_| SocketError::Buffer)?);
            }
            let start = absolute.saturating_sub(chunk.start) as usize;
            let stop = chunk.used.min((end - chunk.start) as usize);
            let fragment = chunk
                .shared
                .as_ref()
                .ok_or(SocketError::Buffer)?
                .slice(start, stop - start)
                .and_then(|chunk| chunk.retain_pool(Arc::clone(&self.pool)))
                .map_err(|_| SocketError::Buffer)?;
            pinned += stop - start;
            chain
                .push(PacketFragment::Shared(fragment))
                .map_err(|_| SocketError::Buffer)?;
        }
        (pinned == len).then_some(chain).ok_or(SocketError::Buffer)
    }

    fn consume_to(&mut self, new_base: u64, consumed: usize) {
        self.len = self.len.saturating_sub(consumed);
        while self
            .chunks
            .front()
            .is_some_and(|chunk| chunk.start + chunk.used as u64 <= new_base)
        {
            self.chunks.pop_front();
        }
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.len = 0;
    }
}

enum StreamBytes {
    Heap(ByteRing),
    Dma(DmaByteRing),
}

impl StreamBytes {
    fn len(&self) -> usize {
        match self {
            Self::Heap(bytes) => bytes.len,
            Self::Dma(bytes) => bytes.len,
        }
    }

    fn limit(&self) -> usize {
        match self {
            Self::Heap(bytes) => bytes.limit,
            Self::Dma(bytes) => bytes.limit,
        }
    }

    fn writable(&self) -> bool {
        match self {
            Self::Heap(bytes) => bytes.available() != 0,
            Self::Dma(bytes) => bytes.writable(),
        }
    }

    fn copy_range(&self, base: u64, offset: usize, output: &mut [u8]) -> bool {
        match self {
            Self::Heap(bytes) => bytes.copy_range(offset, output),
            Self::Dma(bytes) => bytes.copy_range(base + offset as u64, output),
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Heap(bytes) => bytes.clear(),
            Self::Dma(bytes) => bytes.clear(),
        }
    }

    fn set_limit(&mut self, limit: usize) {
        let limit = limit.clamp(16 * 1024, TCP_BUFFER_HARD_LIMIT);
        match self {
            Self::Heap(bytes) => bytes.set_limit(limit),
            Self::Dma(bytes) => bytes.limit = limit,
        }
    }

    fn enable_local_autotune(&mut self) {
        match self {
            Self::Heap(bytes) => bytes.limit = TCP_LOCAL_AUTOTUNE_LIMIT,
            Self::Dma(bytes) => bytes.limit = TCP_LOCAL_AUTOTUNE_LIMIT,
        }
    }

    #[cfg(test)]
    fn allocated_capacity(&self) -> usize {
        match self {
            Self::Heap(bytes) => bytes.arena.len(),
            Self::Dma(bytes) => bytes
                .chunks
                .iter()
                .map(|chunk| {
                    chunk
                        .exclusive
                        .as_ref()
                        .map_or(SOCKET_CHUNK_BYTES, |lease| usize::from(lease.capacity()))
                })
                .sum(),
        }
    }
}

impl ByteRing {
    fn new() -> Self {
        Self {
            arena: alloc::vec![0; TCP_INITIAL_CHUNKS * SOCKET_CHUNK_BYTES].into_boxed_slice(),
            head: 0,
            len: 0,
            limit: TCP_BUFFER_BYTES,
        }
    }

    fn available(&self) -> usize {
        self.limit.saturating_sub(self.len)
    }

    fn grow_for(&mut self, additional: usize) {
        let required = self.len.saturating_add(additional).min(self.limit);
        if required <= self.arena.len() {
            return;
        }
        let capacity = required
            .next_multiple_of(SOCKET_CHUNK_BYTES)
            .max(self.arena.len().saturating_mul(2))
            .min(self.limit);
        let mut arena = alloc::vec![0; capacity].into_boxed_slice();
        self.copy_range(0, &mut arena[..self.len]);
        self.arena = arena;
        self.head = 0;
    }

    fn push(&mut self, input: &[u8]) -> usize {
        self.push_with(input.len(), &mut |offset, output| {
            output.copy_from_slice(&input[offset..offset + output.len()]);
        })
    }

    fn push_with(&mut self, input_len: usize, copy: &mut impl FnMut(usize, &mut [u8])) -> usize {
        let len = input_len.min(self.available());
        self.grow_for(len);
        if len == 0 {
            return 0;
        }
        #[cfg(feature = "performance-profile")]
        let copy_start = profiling::read_counter();
        let tail = (self.head + self.len) % self.arena.len();
        let first = len.min(self.arena.len() - tail);
        copy(0, &mut self.arena[tail..tail + first]);
        if first != len {
            copy(first, &mut self.arena[..len - first]);
        }
        self.len += len;
        #[cfg(feature = "performance-profile")]
        record_payload_copy(copy_start, len);
        len
    }

    fn copy_range(&self, offset: usize, output: &mut [u8]) -> bool {
        if offset.saturating_add(output.len()) > self.len {
            return false;
        }
        if output.is_empty() {
            return true;
        }
        #[cfg(feature = "performance-profile")]
        let copy_start = profiling::read_counter();
        let output_len = output.len();
        let start = (self.head + offset) % self.arena.len();
        let first = output_len.min(self.arena.len() - start);
        output[..first].copy_from_slice(&self.arena[start..start + first]);
        output[first..].copy_from_slice(&self.arena[..output_len - first]);
        #[cfg(feature = "performance-profile")]
        record_payload_copy(copy_start, output_len);
        true
    }

    fn visit_range(&self, offset: usize, len: usize, visit: &mut impl FnMut(&[u8])) -> bool {
        if offset.saturating_add(len) > self.len {
            return false;
        }
        if len == 0 {
            return true;
        }
        let start = (self.head + offset) % self.arena.len();
        let first = len.min(self.arena.len() - start);
        visit(&self.arena[start..start + first]);
        if first != len {
            visit(&self.arena[..len - first]);
        }
        true
    }

    fn consume(&mut self, len: usize) -> usize {
        let len = len.min(self.len);
        if !self.arena.is_empty() {
            self.head = (self.head + len) % self.arena.len();
        }
        self.len -= len;
        len
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn set_limit(&mut self, limit: usize) {
        self.limit = limit.clamp(16 * 1024, TCP_BUFFER_HARD_LIMIT);
    }
}

struct StreamTxRing {
    bytes: StreamBytes,
    base: u64,
    sent: usize,
}

impl StreamTxRing {
    fn new() -> Self {
        Self {
            bytes: StreamBytes::Heap(ByteRing::new()),
            base: 0,
            sent: 0,
        }
    }

    fn push(&mut self, input: &[u8]) -> usize {
        match &mut self.bytes {
            StreamBytes::Heap(bytes) => bytes.push(input),
            StreamBytes::Dma(bytes) => bytes.push(self.base + bytes.len as u64, input),
        }
    }

    fn push_with(&mut self, input_len: usize, copy: &mut impl FnMut(usize, &mut [u8])) -> usize {
        match &mut self.bytes {
            StreamBytes::Heap(bytes) => bytes.push_with(input_len, copy),
            StreamBytes::Dma(bytes) => {
                bytes.push_with(self.base + bytes.len as u64, input_len, copy)
            }
        }
    }

    fn writable(&self) -> bool {
        self.bytes.writable()
    }

    fn exhausted_pool_key(&self) -> Option<usize> {
        match &self.bytes {
            StreamBytes::Dma(bytes) if bytes.pool_exhausted() => Some(bytes.pool_key()),
            _ => None,
        }
    }

    fn pool_key(&self) -> Option<usize> {
        match &self.bytes {
            StreamBytes::Dma(bytes) => Some(bytes.pool_key()),
            StreamBytes::Heap(_) => None,
        }
    }

    fn reserve_pool_chunk(&mut self, pool_key: usize) -> bool {
        match &mut self.bytes {
            StreamBytes::Dma(bytes) if bytes.pool_key() == pool_key => {
                bytes.reserve(self.base.saturating_add(bytes.len as u64))
            }
            _ => false,
        }
    }

    fn take_unsent(&mut self, max_len: usize) -> Option<(u64, usize)> {
        let len = self.bytes.len().saturating_sub(self.sent).min(max_len);
        if len == 0 {
            return None;
        }
        let start = self.base + self.sent as u64;
        self.sent += len;
        Some((start, len))
    }

    fn take_unsent_without_inflight(&mut self, max_len: usize) -> Option<(u64, usize)> {
        if self.sent != 0 {
            return None;
        }
        self.take_unsent(max_len)
    }

    fn rewind_unsent(&mut self, len: usize) {
        self.sent = self.sent.saturating_sub(len);
    }

    fn copy_absolute(&self, start: u64, output: &mut [u8]) -> bool {
        let Some(offset) = start.checked_sub(self.base) else {
            return false;
        };
        self.bytes.copy_range(self.base, offset as usize, output)
    }

    fn contains(&self, start: u64, len: usize) -> bool {
        start
            .checked_sub(self.base)
            .and_then(|offset| (offset as usize).checked_add(len))
            .is_some_and(|end| end <= self.bytes.len())
    }

    fn acknowledge(&mut self, len: usize) -> usize {
        let consumed = len.min(self.sent).min(self.bytes.len());
        self.base = self.base.saturating_add(consumed as u64);
        self.sent -= consumed;
        match &mut self.bytes {
            StreamBytes::Heap(bytes) => {
                bytes.consume(consumed);
            }
            StreamBytes::Dma(bytes) => bytes.consume_to(self.base, consumed),
        }
        consumed
    }

    fn abort(&mut self) {
        self.bytes.clear();
        self.base = self.base.saturating_add(self.sent as u64);
        self.sent = 0;
    }

    fn install_pool(&mut self, pool: SharedNetBufPool) -> bool {
        if matches!(&self.bytes, StreamBytes::Dma(bytes) if Arc::ptr_eq(&bytes.pool, &pool)) {
            return true;
        }
        let StreamBytes::Heap(heap) = &self.bytes else {
            return false;
        };
        let mut dma = DmaByteRing::new(pool, heap.limit);
        let mut offset = 0usize;
        let mut buffer = [0u8; SOCKET_CHUNK_BYTES];
        while offset < heap.len {
            let len = (heap.len - offset).min(buffer.len());
            if !heap.copy_range(offset, &mut buffer[..len])
                || dma.push(self.base + offset as u64, &buffer[..len]) != len
            {
                return false;
            }
            offset += len;
        }
        self.bytes = StreamBytes::Dma(dma);
        true
    }

    fn pin_absolute(&mut self, start: u64, len: usize) -> Result<Option<PacketChain>, SocketError> {
        match &mut self.bytes {
            StreamBytes::Heap(_) => Ok(None),
            StreamBytes::Dma(bytes) => bytes.pin_range(start, len).map(Some),
        }
    }
}

enum StreamRxChunkStorage {
    Shared(ChunkRef),
    Compact(Box<[u8]>),
    Local,
}

struct StreamRxChunk {
    storage: StreamRxChunkStorage,
    consumed: usize,
    len: usize,
}

impl StreamRxChunk {
    fn bytes(&self) -> Result<&[u8], SocketError> {
        let bytes = match &self.storage {
            StreamRxChunkStorage::Shared(chunk) => {
                chunk.as_slice().map_err(|_| SocketError::Buffer)?
            }
            StreamRxChunkStorage::Compact(bytes) => bytes,
            StreamRxChunkStorage::Local => return Err(SocketError::Buffer),
        };
        Ok(&bytes[self.consumed..self.len])
    }
}

struct StreamRxBytes {
    chunks: VecDeque<StreamRxChunk>,
    local: Option<ByteRing>,
    len: usize,
    limit: usize,
}

impl StreamRxBytes {
    fn new() -> Self {
        Self {
            chunks: VecDeque::with_capacity(TCP_INITIAL_CHUNKS),
            local: None,
            len: 0,
            limit: TCP_BUFFER_BYTES,
        }
    }

    fn available(&self) -> usize {
        self.limit.saturating_sub(self.len)
    }

    fn push_compact(&mut self, input: &[u8]) -> Result<usize, SocketError> {
        self.push_compact_with(input.len(), |offset, output| {
            output.copy_from_slice(&input[offset..offset + output.len()]);
            Ok(())
        })
    }

    fn push_compact_with<E>(
        &mut self,
        len: usize,
        mut copy: impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<usize, E> {
        let len = len.min(self.available());
        let mut chunks = VecDeque::new();
        let mut copied = 0usize;
        while copied < len {
            let payload_len = (len - copied).min(SOCKET_CHUNK_BYTES);
            let capacity = compact_rx_class(payload_len);
            let mut bytes = alloc::vec![0; capacity].into_boxed_slice();
            copy(copied, &mut bytes[..payload_len])?;
            chunks.push_back(StreamRxChunk {
                storage: StreamRxChunkStorage::Compact(bytes),
                consumed: 0,
                len: payload_len,
            });
            copied += payload_len;
        }
        self.chunks.extend(chunks);
        self.len += copied;
        Ok(copied)
    }

    fn push_shared_chain(&mut self, mut chain: PacketChain) -> Result<usize, SocketError> {
        let len = chain.total_len();
        if len > self.available()
            || (0..chain.fragment_count())
                .any(|index| !matches!(chain.fragment(index), Some(PacketFragment::Shared(_))))
        {
            return Err(SocketError::Buffer);
        }
        let count = chain.fragment_count();
        for index in 0..count {
            let Some(PacketFragment::Shared(chunk)) = chain.take_fragment(index) else {
                unreachable!("shared RX chain was prevalidated");
            };
            let chunk_len = chunk.len();
            self.chunks.push_back(StreamRxChunk {
                storage: StreamRxChunkStorage::Shared(chunk),
                consumed: 0,
                len: chunk_len,
            });
        }
        self.len += len;
        Ok(len)
    }

    fn push_local_with(&mut self, len: usize, copy: &mut impl FnMut(usize, &mut [u8])) -> usize {
        let len = len.min(self.available());
        if len == 0 {
            return 0;
        }
        let local = self.local.get_or_insert_with(|| {
            let mut ring = ByteRing::new();
            ring.limit = self.limit;
            ring.grow_for(
                local_tcp_read_batch_bytes()
                    .min(2 * TCP_LOCAL_READ_BATCH_MIN_BYTES)
                    .min(self.limit),
            );
            ring
        });
        local.limit = self.limit;
        let copied = local.push_with(len, copy);
        if copied == 0 {
            return 0;
        }
        if let Some(tail) = self.chunks.back_mut()
            && matches!(tail.storage, StreamRxChunkStorage::Local)
        {
            tail.len += copied;
        } else {
            self.chunks.push_back(StreamRxChunk {
                storage: StreamRxChunkStorage::Local,
                consumed: 0,
                len: copied,
            });
        }
        self.len += copied;
        copied
    }

    fn copy_range(&self, offset: usize, output: &mut [u8]) -> bool {
        self.copy_range_with(offset, output.len(), &mut |copied, input| {
            output[copied..copied + input.len()].copy_from_slice(input);
        })
    }

    fn copy_range_with(
        &self,
        offset: usize,
        output_len: usize,
        copy: &mut impl FnMut(usize, &[u8]),
    ) -> bool {
        if offset.saturating_add(output_len) > self.len {
            return false;
        }
        let mut skipped = offset;
        let mut copied = 0usize;
        let mut local_offset = 0usize;
        for chunk in &self.chunks {
            let chunk_len = chunk.len - chunk.consumed;
            if skipped >= chunk_len {
                skipped -= chunk_len;
                if matches!(chunk.storage, StreamRxChunkStorage::Local) {
                    local_offset += chunk_len;
                }
                continue;
            }
            let len = (chunk_len - skipped).min(output_len - copied);
            if matches!(chunk.storage, StreamRxChunkStorage::Local) {
                let Some(local) = self.local.as_ref() else {
                    return false;
                };
                let mut visited = 0usize;
                if !local.visit_range(local_offset + skipped, len, &mut |input| {
                    copy(copied + visited, input);
                    visited += input.len();
                }) {
                    return false;
                }
                copied += visited;
                local_offset += chunk_len;
            } else {
                let Ok(bytes) = chunk.bytes() else {
                    return false;
                };
                copy(copied, &bytes[skipped..skipped + len]);
                copied += len;
            }
            skipped = 0;
            if copied == output_len {
                return true;
            }
        }
        copied == output_len
    }

    fn consume(&mut self, len: usize) -> usize {
        let mut remaining = len.min(self.len);
        let consumed = remaining;
        while remaining != 0 {
            let front = self.chunks.front_mut().expect("RX length tracks chunks");
            let available = front.len - front.consumed;
            let take = available.min(remaining);
            front.consumed += take;
            if matches!(front.storage, StreamRxChunkStorage::Local) {
                let consumed = self
                    .local
                    .as_mut()
                    .expect("本地 RX 分段必须有环形存储")
                    .consume(take);
                debug_assert_eq!(consumed, take);
            }
            remaining -= take;
            if front.consumed == front.len {
                self.chunks.pop_front();
            }
        }
        self.len -= consumed;
        consumed
    }

    fn clear(&mut self) {
        self.chunks.clear();
        if let Some(local) = self.local.as_mut() {
            local.clear();
        }
        self.len = 0;
    }

    fn set_limit(&mut self, limit: usize) {
        self.limit = limit.clamp(16 * 1024, TCP_BUFFER_HARD_LIMIT);
        if let Some(local) = self.local.as_mut() {
            local.limit = self.limit;
        }
    }

    fn enable_local_autotune(&mut self) {
        self.limit = TCP_LOCAL_AUTOTUNE_LIMIT;
        if let Some(local) = self.local.as_mut() {
            local.limit = self.limit;
        }
    }

    #[cfg(test)]
    fn allocated_capacity(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| match &chunk.storage {
                StreamRxChunkStorage::Shared(chunk) => chunk.len(),
                StreamRxChunkStorage::Compact(bytes) => bytes.len(),
                StreamRxChunkStorage::Local => 0,
            })
            .sum::<usize>()
            + self.local.as_ref().map_or(0, |local| local.arena.len())
    }
}

const fn compact_rx_class(len: usize) -> usize {
    match len {
        0..=256 => 256,
        257..=512 => 512,
        513..=1024 => 1024,
        1025..=2048 => 2048,
        _ => 4096,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamRxStorageKind {
    Discarded,
    Compact,
    PhysicalPinned,
    LoopbackShared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamRxCommit {
    pub len: usize,
    pub storage: StreamRxStorageKind,
    pub low_water_fallback: bool,
}

struct StreamRxRing {
    bytes: StreamRxBytes,
    eof: bool,
}

impl StreamRxRing {
    fn new() -> Self {
        Self {
            bytes: StreamRxBytes::new(),
            eof: false,
        }
    }
}

pub struct TcpTxLease {
    facade: Arc<SocketFacade>,
    pub start: u64,
    pub len: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpInfoSnapshot {
    pub state: u8,
    pub rto_us: u32,
    pub rtt_us: u32,
    pub rtt_variance_us: u32,
    pub send_mss: u32,
    pub congestion_window: u32,
    pub send_ssthresh: u32,
    pub unacknowledged: u32,
    pub retransmitted: u32,
    pub total_retransmitted: u32,
    pub receive_space: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl TcpTxLease {
    pub fn facade(&self) -> Arc<SocketFacade> {
        Arc::clone(&self.facade)
    }

    pub fn copy_out(&self, output: &mut [u8]) -> Result<usize, SocketError> {
        self.copy_range(0, output)?;
        Ok(usize::from(self.len))
    }

    pub fn copy_range(&self, offset: usize, output: &mut [u8]) -> Result<(), SocketError> {
        if offset
            .checked_add(output.len())
            .is_none_or(|end| end > usize::from(self.len))
        {
            return Err(SocketError::Buffer);
        }
        if !self
            .facade
            .stream_tx
            .lock()
            .copy_absolute(self.start + offset as u64, output)
        {
            return Err(SocketError::Closed);
        }
        Ok(())
    }

    pub fn packet_chain(&self) -> Result<Option<PacketChain>, SocketError> {
        self.facade
            .stream_tx
            .lock()
            .pin_absolute(self.start, usize::from(self.len))
    }
}

struct TxEntry {
    generation: u32,
    chunks: [u8; MAX_DATAGRAM_CHUNKS],
    chunk_count: u8,
    len: u16,
    destination: Endpoint,
    dont_route: bool,
    confirm: bool,
    dma_payload: Option<PacketChain>,
}

struct TxRing {
    arena: Box<[u8]>,
    entries: Box<[Option<TxEntry>]>,
    generations: Box<[u32]>,
    free_slots: Vec<u16>,
    queued: VecDeque<u16>,
    free_chunks: [u64; 2],
    used_bytes: usize,
    limit: usize,
    dma_pool: Option<SharedNetBufPool>,
}

impl TxRing {
    fn new() -> Self {
        let mut free_slots = Vec::with_capacity(UDP_RING_ENTRIES);
        free_slots.extend((0..UDP_RING_ENTRIES as u16).rev());
        Self {
            arena: alloc::vec![0; UDP_BUFFER_BYTES].into_boxed_slice(),
            entries: core::iter::repeat_with(|| None)
                .take(UDP_RING_ENTRIES)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            generations: alloc::vec![0; UDP_RING_ENTRIES].into_boxed_slice(),
            free_slots,
            queued: VecDeque::with_capacity(UDP_RING_ENTRIES),
            free_chunks: [u64::MAX >> 32, 0],
            used_bytes: 0,
            limit: UDP_BUFFER_BYTES,
            dma_pool: None,
        }
    }

    fn writable(&self) -> bool {
        self.can_push_len(1)
    }

    fn can_push_len(&self, payload_len: usize) -> bool {
        let chunk_count = payload_len.div_ceil(SOCKET_CHUNK_BYTES);
        if chunk_count > MAX_DATAGRAM_CHUNKS
            || self.used_bytes.saturating_add(payload_len) > self.limit
            || self.free_slots.is_empty()
        {
            return false;
        }
        let Some(pool) = self.dma_pool.as_ref() else {
            return self.free_chunk_count() >= chunk_count;
        };
        let mut owner = pool.lock();
        owner.drain_remote();
        owner.available() >= chunk_count
    }

    fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    fn next_destination(&self) -> Option<Endpoint> {
        let slot = *self.queued.front()?;
        self.entries[usize::from(slot)]
            .as_ref()
            .map(|entry| entry.destination)
    }

    #[cfg(test)]
    fn push(
        &mut self,
        payload: &[u8],
        destination: Endpoint,
        dont_route: bool,
        confirm: bool,
    ) -> Result<(), SocketError> {
        match self.push_from(
            payload.len(),
            destination,
            dont_route,
            confirm,
            &mut |offset, output| {
                output.copy_from_slice(&payload[offset..offset + output.len()]);
                Ok::<(), core::convert::Infallible>(())
            },
        ) {
            Ok(()) => Ok(()),
            Err(DatagramCopyError::Socket(error)) => Err(error),
            Err(DatagramCopyError::Copy(error)) => match error {},
        }
    }

    fn push_from<E>(
        &mut self,
        payload_len: usize,
        destination: Endpoint,
        dont_route: bool,
        confirm: bool,
        copy: &mut impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), DatagramCopyError<E>> {
        if let Some(pool) = self.dma_pool.as_ref().cloned() {
            return self.push_dma_from(payload_len, destination, dont_route, confirm, pool, copy);
        }
        let chunk_count = payload_len.div_ceil(SOCKET_CHUNK_BYTES);
        if chunk_count > MAX_DATAGRAM_CHUNKS
            || self.used_bytes.saturating_add(payload_len) > self.limit
            || self.free_slots.is_empty()
            || self.free_chunk_count() < chunk_count
        {
            return Err(DatagramCopyError::Socket(SocketError::WouldBlock));
        }
        let mut chunks = [0u8; MAX_DATAGRAM_CHUNKS];
        for chunk in chunks.iter_mut().take(chunk_count) {
            *chunk = self.take_free_chunk().expect("TX chunk 计数失配");
        }
        #[cfg(feature = "performance-profile")]
        let copy_start = profiling::read_counter();
        let mut copied = 0usize;
        for chunk in chunks.iter().take(chunk_count) {
            let len = (payload_len - copied).min(SOCKET_CHUNK_BYTES);
            let offset = usize::from(*chunk) * SOCKET_CHUNK_BYTES;
            if let Err(error) = copy(copied, &mut self.arena[offset..offset + len]) {
                for chunk in chunks.iter().take(chunk_count) {
                    let index = usize::from(*chunk);
                    self.free_chunks[index / 64] |= 1u64 << (index % 64);
                }
                return Err(DatagramCopyError::Copy(error));
            }
            copied += len;
        }
        let slot = self.free_slots.pop().unwrap();
        let generation = self.generations[usize::from(slot)].wrapping_add(1).max(1);
        self.generations[usize::from(slot)] = generation;
        self.entries[usize::from(slot)] = Some(TxEntry {
            generation,
            chunks,
            chunk_count: chunk_count as u8,
            len: payload_len as u16,
            destination,
            dont_route,
            confirm,
            dma_payload: None,
        });
        self.queued.push_back(slot);
        self.used_bytes += payload_len;
        #[cfg(feature = "performance-profile")]
        {
            record_payload_copy(copy_start, payload_len);
            profiling::observe(profiling::Metric::UdpTxQueueDepth, self.queued.len() as u64);
        }
        Ok(())
    }

    fn push_dma_from<E>(
        &mut self,
        payload_len: usize,
        destination: Endpoint,
        dont_route: bool,
        confirm: bool,
        pool: SharedNetBufPool,
        copy: &mut impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), DatagramCopyError<E>> {
        let chunk_count = payload_len.div_ceil(SOCKET_CHUNK_BYTES);
        if chunk_count > MAX_DATAGRAM_CHUNKS
            || self.used_bytes.saturating_add(payload_len) > self.limit
            || self.free_slots.is_empty()
        {
            return Err(DatagramCopyError::Socket(SocketError::WouldBlock));
        }
        let mut leases: [Option<NetBufLease>; MAX_DATAGRAM_CHUNKS] = core::array::from_fn(|_| None);
        {
            let mut owner = pool.lock();
            owner.drain_remote();
            let mut copied = 0usize;
            for lease_slot in leases.iter_mut().take(chunk_count) {
                let len = (payload_len - copied).min(SOCKET_CHUNK_BYTES);
                let mut lease = owner
                    .lease(0, len as u16, PacketMetadata::default())
                    .map_err(|_| DatagramCopyError::Socket(SocketError::WouldBlock))?;
                let output = lease
                    .as_mut_slice()
                    .map_err(|_| DatagramCopyError::Socket(SocketError::Buffer))?;
                copy(copied, output).map_err(DatagramCopyError::Copy)?;
                copied += len;
                *lease_slot = Some(lease);
            }
        }
        let mut chain = PacketChain::new();
        for lease in leases.into_iter().flatten() {
            let chunk = lease
                .into_chunk()
                .and_then(|chunk| chunk.retain_pool(Arc::clone(&pool)))
                .map_err(|_| DatagramCopyError::Socket(SocketError::Buffer))?;
            chain
                .push(PacketFragment::Shared(chunk))
                .map_err(|_| DatagramCopyError::Socket(SocketError::Buffer))?;
        }
        let slot = self.free_slots.pop().unwrap();
        let generation = self.generations[usize::from(slot)].wrapping_add(1).max(1);
        self.generations[usize::from(slot)] = generation;
        self.entries[usize::from(slot)] = Some(TxEntry {
            generation,
            chunks: [0; MAX_DATAGRAM_CHUNKS],
            chunk_count: 0,
            len: payload_len as u16,
            destination,
            dont_route,
            confirm,
            dma_payload: Some(chain),
        });
        self.queued.push_back(slot);
        self.used_bytes += payload_len;
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::UdpTxQueueDepth, self.queued.len() as u64);
        Ok(())
    }

    fn take(&mut self, facade: Arc<SocketFacade>) -> Option<UdpTxLease> {
        let slot = self.queued.pop_front()?;
        let entry = self.entries[usize::from(slot)].as_ref().unwrap();
        Some(UdpTxLease {
            facade,
            slot,
            generation: entry.generation,
            destination: entry.destination,
            len: entry.len,
            dont_route: entry.dont_route,
            confirm: entry.confirm,
            completed: false,
        })
    }

    fn copy_out(
        &self,
        slot: u16,
        generation: u32,
        output: &mut [u8],
    ) -> Result<usize, SocketError> {
        let len = usize::from(
            self.entries[usize::from(slot)]
                .as_ref()
                .filter(|entry| entry.generation == generation)
                .ok_or(SocketError::Closed)?
                .len,
        );
        if output.len() < len {
            return Err(SocketError::Buffer);
        }
        self.copy_range(slot, generation, 0, &mut output[..len])?;
        Ok(len)
    }

    fn copy_range(
        &self,
        slot: u16,
        generation: u32,
        payload_offset: usize,
        output: &mut [u8],
    ) -> Result<(), SocketError> {
        let entry = self.entries[usize::from(slot)]
            .as_ref()
            .filter(|entry| entry.generation == generation)
            .ok_or(SocketError::Closed)?;
        if let Some(payload) = entry.dma_payload.as_ref() {
            return payload
                .copy_out(payload_offset, output)
                .map_err(|_| SocketError::Buffer);
        }
        let end = payload_offset
            .checked_add(output.len())
            .ok_or(SocketError::Buffer)?;
        if end > usize::from(entry.len) {
            return Err(SocketError::Buffer);
        }
        let mut copied = 0;
        let first_chunk = payload_offset / SOCKET_CHUNK_BYTES;
        let first_offset = payload_offset % SOCKET_CHUNK_BYTES;
        for (index, chunk) in entry
            .chunks
            .iter()
            .take(usize::from(entry.chunk_count))
            .enumerate()
            .skip(first_chunk)
        {
            let start = if index == first_chunk {
                first_offset
            } else {
                0
            };
            let len = (output.len() - copied).min(SOCKET_CHUNK_BYTES - start);
            let offset = usize::from(*chunk) * SOCKET_CHUNK_BYTES;
            output[copied..copied + len]
                .copy_from_slice(&self.arena[offset + start..offset + start + len]);
            copied += len;
            if copied == output.len() {
                return Ok(());
            }
        }
        Err(SocketError::Buffer)
    }

    fn complete(&mut self, slot: u16, generation: u32) -> bool {
        let Some(entry) = self.entries[usize::from(slot)].take() else {
            return false;
        };
        if entry.generation != generation {
            self.entries[usize::from(slot)] = Some(entry);
            return false;
        }
        for chunk in entry.chunks.iter().take(usize::from(entry.chunk_count)) {
            let index = usize::from(*chunk);
            self.free_chunks[index / 64] |= 1u64 << (index % 64);
        }
        self.used_bytes = self.used_bytes.saturating_sub(usize::from(entry.len));
        self.free_slots.push(slot);
        true
    }

    fn pin_payload(&self, slot: u16, generation: u32) -> Result<Option<PacketChain>, SocketError> {
        let entry = self.entries[usize::from(slot)]
            .as_ref()
            .filter(|entry| entry.generation == generation)
            .ok_or(SocketError::Closed)?;
        entry
            .dma_payload
            .as_ref()
            .map(|payload| payload.pin_shared().map_err(|_| SocketError::Buffer))
            .transpose()
    }

    fn install_pool(&mut self, pool: SharedNetBufPool) {
        self.dma_pool = Some(pool);
    }

    fn pool_key(&self) -> Option<usize> {
        self.dma_pool
            .as_ref()
            .map(|pool| Arc::as_ptr(pool) as usize)
    }

    fn exhausted_pool_key(&self) -> Option<usize> {
        (!self.writable() && self.free_slots.len() != 0 && self.used_bytes < self.limit)
            .then(|| self.pool_key())
            .flatten()
    }

    fn free_chunk_count(&self) -> usize {
        self.free_chunks
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    fn take_free_chunk(&mut self) -> Option<u8> {
        for (word_index, word) in self.free_chunks.iter_mut().enumerate() {
            if *word == 0 {
                continue;
            }
            let bit = word.trailing_zeros() as usize;
            *word &= !(1u64 << bit);
            return Some((word_index * 64 + bit) as u8);
        }
        None
    }

    fn set_limit(&mut self, limit: usize) {
        let limit = limit.clamp(16 * 1024, UDP_BUFFER_HARD_LIMIT);
        let required_chunks = limit.div_ceil(SOCKET_CHUNK_BYTES);
        let current_chunks = self.arena.len() / SOCKET_CHUNK_BYTES;
        if required_chunks > current_chunks {
            let mut arena = core::mem::take(&mut self.arena).into_vec();
            arena.resize(required_chunks * SOCKET_CHUNK_BYTES, 0);
            self.arena = arena.into_boxed_slice();
            for index in current_chunks..required_chunks {
                self.free_chunks[index / 64] |= 1u64 << (index % 64);
            }
        }
        self.limit = limit;
    }
}

struct LocalUdpDatagram {
    chunks: [u8; MAX_DATAGRAM_CHUNKS],
    chunk_count: u8,
}

enum RxPayload {
    Packet(crate::transport::UdpDatagram),
    Local(LocalUdpDatagram),
    Shared(PacketChain),
}

struct RxDatagram {
    payload: RxPayload,
    source: Endpoint,
    destination: Endpoint,
    payload_len: u16,
    ingress_interface: InterfaceId,
    hop_limit: u8,
    traffic_class: u8,
    rx_timestamp_ns: u64,
}

struct LocalDatagramSlot {
    payload: [u8; LOCAL_DATAGRAM_INLINE_BYTES],
    payload_len: u16,
    source: Endpoint,
    destination: Endpoint,
    ingress_interface: InterfaceId,
    hop_limit: u8,
    traffic_class: u8,
    rx_timestamp_ns: u64,
    occupied: bool,
}

impl LocalDatagramSlot {
    fn new() -> Self {
        let unspecified = Endpoint {
            addr: crate::IpAddr::V4(crate::Ipv4Addr::UNSPECIFIED),
            port: 0,
        };
        Self {
            payload: [0; LOCAL_DATAGRAM_INLINE_BYTES],
            payload_len: 0,
            source: unspecified,
            destination: unspecified,
            ingress_interface: InterfaceId(0),
            hop_limit: 0,
            traffic_class: 0,
            rx_timestamp_ns: 0,
            occupied: false,
        }
    }

    fn push_from<E>(
        &mut self,
        payload_len: usize,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
        copy: &mut impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        debug_assert!(!self.occupied);
        debug_assert!(payload_len <= self.payload.len());
        copy(0, &mut self.payload[..payload_len])?;
        self.payload_len = payload_len as u16;
        self.source = source;
        self.destination = destination;
        self.ingress_interface = ingress_interface;
        self.hop_limit = hop_limit;
        self.traffic_class = traffic_class;
        self.rx_timestamp_ns = rx_timestamp_ns;
        self.occupied = true;
        Ok(())
    }

    fn clear(&mut self) {
        self.occupied = false;
        self.payload_len = 0;
    }
}

struct DatagramRx {
    ring: RxRing,
    local: Option<Box<LocalDatagramSlot>>,
}

impl DatagramRx {
    fn new() -> Self {
        Self {
            ring: RxRing::new(),
            local: None,
        }
    }

    fn prepare_local_lane(&mut self) {
        if self.local.is_none() {
            self.local = Some(Box::new(LocalDatagramSlot::new()));
        }
    }

    fn local_occupied(&self) -> bool {
        self.local.as_ref().is_some_and(|slot| slot.occupied)
    }

    fn is_empty(&self) -> bool {
        !self.local_occupied() && self.ring.is_empty()
    }

    fn len(&self) -> u16 {
        self.ring.len + u16::from(self.local_occupied())
    }

    fn local_bytes(&self) -> usize {
        self.local
            .as_ref()
            .filter(|slot| slot.occupied)
            .map_or(0, |slot| usize::from(slot.payload_len))
    }

    fn bytes(&self) -> usize {
        self.ring.bytes + self.local_bytes()
    }

    fn front_len(&self) -> Option<usize> {
        if let Some(slot) = self.local.as_ref().filter(|slot| slot.occupied) {
            return Some(usize::from(slot.payload_len));
        }
        self.ring
            .front()
            .map(|datagram| usize::from(datagram.payload_len))
    }

    fn push(
        &mut self,
        datagram: crate::transport::UdpDatagram,
    ) -> Result<(), crate::transport::UdpDatagram> {
        if self
            .bytes()
            .saturating_add(usize::from(datagram.payload_len))
            > self.ring.limit
        {
            return Err(datagram);
        }
        self.ring.push(datagram)
    }

    fn push_local_from<E>(
        &mut self,
        payload_len: usize,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
        copy: &mut impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), DatagramCopyError<E>> {
        if self.ring.is_empty()
            && payload_len <= LOCAL_DATAGRAM_INLINE_BYTES
            && self.bytes().saturating_add(payload_len) <= self.ring.limit
            && let Some(slot) = self.local.as_mut()
            && !slot.occupied
        {
            #[cfg(feature = "performance-profile")]
            let copy_start = profiling::read_counter();
            slot.push_from(
                payload_len,
                source,
                destination,
                hop_limit,
                traffic_class,
                ingress_interface,
                rx_timestamp_ns,
                copy,
            )
            .map_err(DatagramCopyError::Copy)?;
            #[cfg(feature = "performance-profile")]
            record_payload_copy(copy_start, payload_len);
            return Ok(());
        }
        if self.bytes().saturating_add(payload_len) > self.ring.limit {
            #[cfg(feature = "performance-profile")]
            self.ring.record_full_reject();
            return Err(DatagramCopyError::Socket(SocketError::WouldBlock));
        }
        self.ring.push_local_from(
            payload_len,
            source,
            destination,
            hop_limit,
            traffic_class,
            ingress_interface,
            rx_timestamp_ns,
            copy,
        )
    }

    fn push_local_shared(
        &mut self,
        payload: PacketChain,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
    ) -> Result<(), SocketError> {
        let payload_len = payload.total_len();
        if self.bytes().saturating_add(payload_len) > self.ring.limit {
            #[cfg(feature = "performance-profile")]
            self.ring.record_full_reject();
            return Err(SocketError::WouldBlock);
        }
        self.ring.push_local_shared(
            payload,
            source,
            destination,
            hop_limit,
            traffic_class,
            ingress_interface,
            rx_timestamp_ns,
        )
    }

    fn can_push_local_len(&self, payload_len: usize) -> bool {
        self.bytes().saturating_add(payload_len) <= self.ring.limit
            && ((self.ring.is_empty()
                && payload_len <= LOCAL_DATAGRAM_INLINE_BYTES
                && self.local.as_ref().is_some_and(|slot| !slot.occupied))
                || self.ring.can_push_local_len(payload_len))
    }

    fn front_local(&self) -> bool {
        self.local_occupied()
            || self.ring.front().is_some_and(|datagram| {
                matches!(datagram.payload, RxPayload::Local(_) | RxPayload::Shared(_))
            })
    }

    fn front_metadata(&self) -> Option<(usize, Endpoint, Endpoint, InterfaceId, u8, u8, u64)> {
        if let Some(slot) = self.local.as_ref().filter(|slot| slot.occupied) {
            return Some((
                usize::from(slot.payload_len),
                slot.source,
                slot.destination,
                slot.ingress_interface,
                slot.hop_limit,
                slot.traffic_class,
                slot.rx_timestamp_ns,
            ));
        }
        let datagram = self.ring.front()?;
        Some((
            usize::from(datagram.payload_len),
            datagram.source,
            datagram.destination,
            datagram.ingress_interface,
            datagram.hop_limit,
            datagram.traffic_class,
            datagram.rx_timestamp_ns,
        ))
    }

    fn copy_front(&self, output: &mut [u8]) -> Result<(), SocketError> {
        if let Some(slot) = self.local.as_ref().filter(|slot| slot.occupied) {
            output.copy_from_slice(&slot.payload[..output.len()]);
            return Ok(());
        }
        self.ring.copy_front(output)
    }

    fn copy_local_front_to<E>(
        &self,
        output_len: usize,
        copy: &mut impl FnMut(usize, &[u8]) -> Result<(), E>,
    ) -> Result<Option<usize>, DatagramCopyError<E>> {
        if let Some(slot) = self.local.as_ref().filter(|slot| slot.occupied) {
            let copied = output_len.min(usize::from(slot.payload_len));
            copy(0, &slot.payload[..copied]).map_err(DatagramCopyError::Copy)?;
            return Ok(Some(copied));
        }
        self.ring.copy_local_front_to(output_len, copy)
    }

    fn pop(&mut self) -> Option<()> {
        if let Some(slot) = self.local.as_mut().filter(|slot| slot.occupied) {
            slot.clear();
            return Some(());
        }
        self.ring.pop().map(drop)
    }

    fn set_limit(&mut self, limit: usize) {
        self.ring.set_limit(limit);
    }

    fn limit(&self) -> usize {
        self.ring.limit
    }
}

struct RxRing {
    entries: Box<[Option<RxDatagram>]>,
    head: u16,
    tail: u16,
    len: u16,
    bytes: usize,
    limit: usize,
    arena: Box<[u8]>,
    free_chunks: [u64; 2],
    #[cfg(feature = "performance-profile")]
    full_since_ns: u64,
}

impl RxRing {
    fn new() -> Self {
        Self {
            entries: core::iter::repeat_with(|| None)
                .take(UDP_RING_ENTRIES)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            head: 0,
            tail: 0,
            len: 0,
            bytes: 0,
            limit: UDP_BUFFER_BYTES,
            arena: alloc::vec![0; UDP_BUFFER_BYTES].into_boxed_slice(),
            free_chunks: [u64::MAX >> 32, 0],
            #[cfg(feature = "performance-profile")]
            full_since_ns: 0,
        }
    }

    #[cfg(feature = "performance-profile")]
    fn record_full_reject(&mut self) {
        profiling::observe(profiling::Metric::RxRingFullRejects, 1);
        if self.full_since_ns == 0 {
            self.full_since_ns = sched::now_ns_public().saturating_add(1);
        }
    }

    #[cfg(feature = "performance-profile")]
    fn finish_full_interval(&mut self) {
        if self.full_since_ns == 0 {
            return;
        }
        let started = core::mem::take(&mut self.full_since_ns);
        profiling::observe(
            profiling::Metric::RxRingFullDurationNs,
            sched::now_ns_public().saturating_sub(started - 1),
        );
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(
        &mut self,
        datagram: crate::transport::UdpDatagram,
    ) -> Result<(), crate::transport::UdpDatagram> {
        if usize::from(self.len) == self.entries.len()
            || self.bytes.saturating_add(usize::from(datagram.payload_len)) > self.limit
        {
            #[cfg(feature = "performance-profile")]
            self.record_full_reject();
            return Err(datagram);
        }
        self.bytes += usize::from(datagram.payload_len);
        self.entries[usize::from(self.tail)] = Some(RxDatagram {
            source: datagram.source,
            destination: datagram.destination,
            payload_len: datagram.payload_len,
            ingress_interface: datagram.ingress_interface,
            hop_limit: datagram.hop_limit,
            traffic_class: datagram.traffic_class,
            rx_timestamp_ns: datagram.rx_timestamp_ns,
            payload: RxPayload::Packet(datagram),
        });
        self.tail = (self.tail + 1) % self.entries.len() as u16;
        self.len += 1;
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::RxRingDepth, u64::from(self.len));
        Ok(())
    }

    fn push_local_from<E>(
        &mut self,
        payload_len: usize,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
        copy: &mut impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), DatagramCopyError<E>> {
        let chunk_count = payload_len.div_ceil(SOCKET_CHUNK_BYTES);
        if usize::from(self.len) == self.entries.len()
            || self.bytes.saturating_add(payload_len) > self.limit
            || chunk_count > MAX_DATAGRAM_CHUNKS
            || self.free_chunk_count() < chunk_count
        {
            #[cfg(feature = "performance-profile")]
            self.record_full_reject();
            return Err(DatagramCopyError::Socket(SocketError::WouldBlock));
        }
        let mut chunks = [0u8; MAX_DATAGRAM_CHUNKS];
        let mut allocated = 0;
        for chunk in chunks.iter_mut().take(chunk_count) {
            *chunk = self.take_free_chunk().expect("RX chunk 计数失配");
            allocated += 1;
        }
        #[cfg(feature = "performance-profile")]
        let copy_start = profiling::read_counter();
        for (index, chunk) in chunks.iter().take(chunk_count).enumerate() {
            let offset = index * SOCKET_CHUNK_BYTES;
            let len = (payload_len - offset).min(SOCKET_CHUNK_BYTES);
            if let Err(error) = copy(
                offset,
                &mut self.arena[usize::from(*chunk) * SOCKET_CHUNK_BYTES
                    ..usize::from(*chunk) * SOCKET_CHUNK_BYTES + len],
            ) {
                self.release_chunks(&chunks, allocated);
                return Err(DatagramCopyError::Copy(error));
            }
        }
        self.bytes += payload_len;
        self.entries[usize::from(self.tail)] = Some(RxDatagram {
            payload: RxPayload::Local(LocalUdpDatagram {
                chunks,
                chunk_count: chunk_count as u8,
            }),
            source,
            destination,
            payload_len: payload_len as u16,
            ingress_interface,
            hop_limit,
            traffic_class,
            rx_timestamp_ns,
        });
        self.tail = (self.tail + 1) % self.entries.len() as u16;
        self.len += 1;
        #[cfg(feature = "performance-profile")]
        {
            record_payload_copy(copy_start, payload_len);
            profiling::observe(profiling::Metric::RxRingDepth, u64::from(self.len));
        }
        Ok(())
    }

    fn push_local_shared(
        &mut self,
        payload: PacketChain,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
    ) -> Result<(), SocketError> {
        let payload_len = payload.total_len();
        if payload_len > u16::MAX as usize
            || usize::from(self.len) == self.entries.len()
            || self.bytes.saturating_add(payload_len) > self.limit
            || (0..payload.fragment_count())
                .any(|index| !matches!(payload.fragment(index), Some(PacketFragment::Shared(_))))
        {
            #[cfg(feature = "performance-profile")]
            self.record_full_reject();
            return Err(SocketError::WouldBlock);
        }
        self.bytes += payload_len;
        self.entries[usize::from(self.tail)] = Some(RxDatagram {
            payload: RxPayload::Shared(payload),
            source,
            destination,
            payload_len: payload_len as u16,
            ingress_interface,
            hop_limit,
            traffic_class,
            rx_timestamp_ns,
        });
        self.tail = (self.tail + 1) % self.entries.len() as u16;
        self.len += 1;
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::RxRingDepth, u64::from(self.len));
        Ok(())
    }

    /// 当前队列是否还能原子接收一个同等大小的本地数据报。
    fn can_push_local_len(&self, payload_len: usize) -> bool {
        let chunk_count = payload_len.div_ceil(SOCKET_CHUNK_BYTES);
        usize::from(self.len) < self.entries.len()
            && self.bytes.saturating_add(payload_len) <= self.limit
            && chunk_count <= MAX_DATAGRAM_CHUNKS
            && self.free_chunk_count() >= chunk_count
    }

    fn front(&self) -> Option<&RxDatagram> {
        self.entries[usize::from(self.head)].as_ref()
    }

    fn copy_front(&self, output: &mut [u8]) -> Result<(), SocketError> {
        let datagram = self.front().ok_or(SocketError::WouldBlock)?;
        let len = output.len().min(usize::from(datagram.payload_len));
        #[cfg(feature = "performance-profile")]
        let copy_start = profiling::read_counter();
        let result = match &datagram.payload {
            RxPayload::Packet(packet) => packet
                .packet
                .copy_out(usize::from(packet.payload_offset), &mut output[..len])
                .map_err(|_| SocketError::Buffer),
            RxPayload::Local(local) => {
                let mut copied = 0;
                for chunk in local.chunks.iter().take(usize::from(local.chunk_count)) {
                    let part = (len - copied).min(SOCKET_CHUNK_BYTES);
                    let offset = usize::from(*chunk) * SOCKET_CHUNK_BYTES;
                    output[copied..copied + part]
                        .copy_from_slice(&self.arena[offset..offset + part]);
                    copied += part;
                    if copied == len {
                        break;
                    }
                }
                Ok(())
            }
            RxPayload::Shared(payload) => payload
                .copy_out(0, &mut output[..len])
                .map_err(|_| SocketError::Buffer),
        };
        #[cfg(feature = "performance-profile")]
        if result.is_ok() {
            record_payload_copy(copy_start, len);
        }
        result
    }

    /// 把本地数据报直接复制到外部目标；非本地 payload 返回 `None` 交给通用路径。
    fn copy_local_front_to<E>(
        &self,
        output_len: usize,
        copy: &mut impl FnMut(usize, &[u8]) -> Result<(), E>,
    ) -> Result<Option<usize>, DatagramCopyError<E>> {
        let Some(datagram) = self.front() else {
            return Ok(None);
        };
        let len = output_len.min(usize::from(datagram.payload_len));
        match &datagram.payload {
            RxPayload::Local(local) => {
                let mut copied = 0usize;
                for chunk in local.chunks.iter().take(usize::from(local.chunk_count)) {
                    let part = (len - copied).min(SOCKET_CHUNK_BYTES);
                    let offset = usize::from(*chunk) * SOCKET_CHUNK_BYTES;
                    copy(copied, &self.arena[offset..offset + part])
                        .map_err(DatagramCopyError::Copy)?;
                    copied += part;
                    if copied == len {
                        break;
                    }
                }
                Ok(Some(copied))
            }
            RxPayload::Shared(payload) => {
                let mut copied = 0usize;
                for index in 0..payload.fragment_count() {
                    let Some(fragment) = payload.fragment(index) else {
                        return Ok(None);
                    };
                    let bytes = fragment
                        .as_slice()
                        .map_err(|_| DatagramCopyError::Socket(SocketError::Buffer))?;
                    let part = (len - copied).min(bytes.len());
                    copy(copied, &bytes[..part]).map_err(DatagramCopyError::Copy)?;
                    copied += part;
                    if copied == len {
                        break;
                    }
                }
                Ok(Some(copied))
            }
            RxPayload::Packet(_) => Ok(None),
        }
    }

    fn pop(&mut self) -> Option<RxDatagram> {
        if self.len == 0 {
            return None;
        }
        let datagram = self.entries[usize::from(self.head)].take();
        self.head = (self.head + 1) % self.entries.len() as u16;
        self.len -= 1;
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::RxRingDepth, u64::from(self.len));
        if let Some(datagram) = datagram.as_ref() {
            self.bytes = self.bytes.saturating_sub(usize::from(datagram.payload_len));
        }
        if let Some(RxDatagram {
            payload: RxPayload::Local(local),
            ..
        }) = datagram.as_ref()
        {
            self.release_chunks(&local.chunks, usize::from(local.chunk_count));
        }
        #[cfg(feature = "performance-profile")]
        self.finish_full_interval();
        datagram
    }

    fn free_chunk_count(&self) -> usize {
        self.free_chunks
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    fn take_free_chunk(&mut self) -> Option<u8> {
        for (word_index, word) in self.free_chunks.iter_mut().enumerate() {
            if *word == 0 {
                continue;
            }
            let bit = word.trailing_zeros() as usize;
            *word &= !(1u64 << bit);
            return Some((word_index * 64 + bit) as u8);
        }
        None
    }

    fn release_chunks(&mut self, chunks: &[u8; MAX_DATAGRAM_CHUNKS], count: usize) {
        for chunk in chunks.iter().take(count) {
            let index = usize::from(*chunk);
            self.free_chunks[index / 64] |= 1u64 << (index % 64);
        }
    }

    fn set_limit(&mut self, limit: usize) {
        let limit = limit.clamp(16 * 1024, UDP_BUFFER_HARD_LIMIT);
        let required_chunks = limit.div_ceil(SOCKET_CHUNK_BYTES);
        let current_chunks = self.arena.len() / SOCKET_CHUNK_BYTES;
        if required_chunks > current_chunks {
            let mut arena = core::mem::take(&mut self.arena).into_vec();
            arena.resize(required_chunks * SOCKET_CHUNK_BYTES, 0);
            self.arena = arena.into_boxed_slice();
            for index in current_chunks..required_chunks {
                self.free_chunks[index / 64] |= 1u64 << (index % 64);
            }
        }
        self.limit = limit;
    }
}

#[cfg(feature = "performance-profile")]
fn record_payload_copy(start_cycles: u64, bytes: usize) {
    profiling::observe(profiling::Metric::PayloadCopyBytes, bytes as u64);
    profiling::observe(
        profiling::Metric::PayloadCopyCycles,
        profiling::read_counter().wrapping_sub(start_cycles),
    );
}

#[cfg(feature = "performance-profile")]
fn record_socket_wakeup() {
    profiling::observe(profiling::Metric::SocketWakeup, 1);
}

fn wake_one_socket_waiter(queue: &WaitQueue) {
    if queue.len_hint() == 0 {
        return;
    }
    #[cfg(feature = "performance-profile")]
    record_socket_wakeup();
    queue.wake_one_default();
}

fn wake_one_socket_reader(queue: &WaitQueue, _socket: SocketId) {
    if queue.len_hint() == 0 {
        return;
    }
    #[cfg(feature = "performance-profile")]
    {
        record_socket_wakeup();
        let correlation = profiling::next_correlation_id();
        profiling::trace_point(profiling::Event::NetPeerRx, _socket.counter, correlation);
        queue.wake_one_default_with_cause(1, _socket.counter, correlation);
    }
    #[cfg(not(feature = "performance-profile"))]
    queue.wake_one_default();
}

/// 唤醒本地接收者，并返回可在公共调度边界复查的稳定任务身份。
///
/// 任务只获得普通可运行资格，不提升调度优先级；跨 CPU 和通用网络路径仍使用
/// 普通就绪资格。返回的目标可以保留到有界批次末尾，但不能改变目标优先级。
fn wake_one_local_socket_reader(
    queue: &WaitQueue,
    _socket: SocketId,
    request_reschedule: bool,
) -> Option<sched::HandoffTarget> {
    if queue.len_hint() == 0 {
        return None;
    }
    let task = queue.wake_one_with(|_| {})?;
    let now_ns = sched::now_ns_public();
    #[cfg(feature = "performance-profile")]
    {
        record_socket_wakeup();
        let correlation = profiling::next_correlation_id();
        profiling::trace_point(profiling::Event::NetPeerRx, _socket.counter, correlation);
        task.set_profile_wake_cause(1, _socket.counter, correlation, now_ns);
    }
    Some(sched::enqueue_task_preferred_for_handoff(
        task,
        now_ns,
        sched::HandoffReason::SocketRead,
        true,
        request_reschedule,
    ))
}

fn wake_one_socket_writer(queue: &WaitQueue, _socket: SocketId) {
    if queue.len_hint() == 0 {
        return;
    }
    #[cfg(feature = "performance-profile")]
    {
        record_socket_wakeup();
        let correlation = profiling::next_correlation_id();
        profiling::trace_point(
            profiling::Event::NetTxWritable,
            _socket.counter,
            correlation,
        );
        queue.wake_one_default_with_cause(2, _socket.counter, correlation);
    }
    #[cfg(not(feature = "performance-profile"))]
    queue.wake_one_default();
}

pub struct UdpTxLease {
    facade: Arc<SocketFacade>,
    slot: u16,
    generation: u32,
    pub destination: Endpoint,
    pub len: u16,
    pub dont_route: bool,
    pub confirm: bool,
    completed: bool,
}

impl UdpTxLease {
    pub fn facade(&self) -> Arc<SocketFacade> {
        Arc::clone(&self.facade)
    }

    pub fn copy_out(&self, output: &mut [u8]) -> Result<usize, SocketError> {
        self.facade
            .tx
            .lock()
            .as_ref()
            .expect("UDP facade 必须拥有 TX ring")
            .copy_out(self.slot, self.generation, output)
    }

    pub fn copy_range(&self, offset: usize, output: &mut [u8]) -> Result<(), SocketError> {
        self.facade
            .tx
            .lock()
            .as_ref()
            .expect("UDP facade 必须拥有 TX ring")
            .copy_range(self.slot, self.generation, offset, output)
    }

    pub fn packet_chain(&self) -> Result<Option<PacketChain>, SocketError> {
        self.facade
            .tx
            .lock()
            .as_ref()
            .expect("UDP facade 必须拥有 TX ring")
            .pin_payload(self.slot, self.generation)
    }

    pub fn complete(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        let (completed, pool_key) = {
            let mut tx = self.facade.tx.lock();
            let tx = tx.as_mut().expect("UDP facade 必须拥有 TX ring");
            let pool_key = tx.pool_key();
            (tx.complete(self.slot, self.generation), pool_key)
        };
        if !completed {
            return;
        }
        if let Some(pool_key) = pool_key {
            wake_dma_tx_pool_waiter(pool_key, Arc::as_ptr(&self.facade));
        }
        let writable = self
            .facade
            .tx
            .lock()
            .as_ref()
            .expect("UDP facade 必须拥有 TX ring")
            .writable();
        if writable {
            self.facade.set_ready(Readiness::WRITABLE);
            wake_one_socket_writer(&self.facade.write_wait, self.facade.id);
        } else {
            self.facade.clear_ready(Readiness::WRITABLE);
        }
    }
}

impl Drop for UdpTxLease {
    fn drop(&mut self) {
        self.finish();
    }
}

pub struct UdpReceive {
    pub len: usize,
    pub original_len: usize,
    pub source: Endpoint,
    pub destination: Endpoint,
    pub ingress_interface: InterfaceId,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub rx_timestamp_ns: u64,
    pub truncated: bool,
}

pub struct SocketFacade {
    id: SocketId,
    family: AddressFamily,
    kind: SocketKind,
    protocol: u8,
    stack_generation: AtomicU64,
    stack_detached: AtomicBool,
    packet_observer_active: AtomicBool,
    generation: AtomicU32,
    owner: Mutex<OwnerRef>,
    local: Mutex<Option<Endpoint>>,
    peer: Mutex<Option<Endpoint>>,
    local_datagram_route: Mutex<Option<LocalDatagramRoute>>,
    local_tcp_direct_route: Mutex<Option<LocalTcpDirectRoute>>,
    tx: Mutex<Option<TxRing>>,
    rx: Mutex<Option<DatagramRx>>,
    stream_tx: Mutex<StreamTxRing>,
    stream_rx: Mutex<StreamRxRing>,
    /// SO_OOBINLINE：紧急字节保留在普通字节流中（Linux tcp_urg 语义）。
    oob_inline: AtomicBool,
    /// 紧急字节的副本：非内联模式为从流中剔除的字节；内联模式为流内字节的镜像。
    oob_byte: Mutex<Option<u8>>,
    /// 当前紧急字节在接收流中的偏移（相对首个流字节）。
    oob_seq: Mutex<Option<u64>>,
    /// 存在可供 recv(MSG_OOB) 读取的紧急数据。
    oob_pending: AtomicBool,
    /// 非内联模式下紧急字节已先于 URG 标记进入流：普通读需要跳过该字节。
    oob_skip: AtomicBool,
    /// 接收流首个字节的绝对 TCP 序列号（由引擎在流建立时设置）。
    stream_base_seq: Mutex<Option<u32>>,
    /// 已推入接收流的字节总数。
    stream_pushed: AtomicU64,
    /// 已被用户消费的流字节数。
    stream_consumed: AtomicU64,
    /// F_SETOWN 注册的紧急数据信号接收者（owner_type, owner_pid）。
    urgent_owner: Mutex<Option<(i32, i32)>>,
    /// 发送侧待发出的紧急字节（drain_send 据此加 URG 标志）。
    urgent_tx_pending: AtomicBool,
    listen_group: Mutex<Option<Arc<ListenGroup>>>,

    readiness: AtomicU16,
    readiness_generation: AtomicU64,
    observer: Mutex<Option<Arc<dyn ReadinessObserver>>>,
    read_wait: WaitQueue,
    local_read_handoff: Mutex<Option<sched::HandoffTarget>>,
    write_wait: WaitQueue,
    accept_wait: WaitQueue,
    state_wait: WaitQueue,
    control_lock: sched::mutex::Mutex<()>,
    control_sequence: AtomicU64,
    control_pending: AtomicBool,
    control_result: Mutex<Option<(u64, Result<(), SocketError>)>>,
    tx_notified: AtomicBool,
    tx_generation: AtomicU64,
    tx_completed_generation: AtomicU64,
    lifecycle_notified: AtomicBool,
    closing: AtomicBool,
    abortive_close: AtomicBool,
    read_shutdown: AtomicBool,
    write_shutdown: AtomicBool,
    pending_error: AtomicU32,
    error_queue: Mutex<VecDeque<SocketErrorRecord>>,
    next_error_sequence: AtomicU64,
    error_queue_overflow: AtomicU64,
    interface: Mutex<Option<InterfaceId>>,
    tcp_nodelay: AtomicBool,
    tcp_cork: AtomicBool,
    tcp_more: AtomicBool,
    tcp_quick_ack: AtomicBool,
    tcp_defer_accept_ns: AtomicU64,
    tcp_notsent_lowat: AtomicU32,
    tcp_user_timeout_ns: AtomicU64,
    tcp_keepalive: AtomicBool,
    tcp_keepidle_ns: AtomicU64,
    tcp_keepintvl_ns: AtomicU64,
    tcp_keepcount: AtomicU16,
    tcp_maxseg: AtomicU16,
    tcp_state: AtomicU16,
    tcp_rto_us: AtomicU32,
    tcp_rtt_us: AtomicU32,
    tcp_rtt_variance_us: AtomicU32,
    tcp_send_mss: AtomicU32,
    tcp_congestion_window: AtomicU32,
    tcp_send_ssthresh: AtomicU32,
    tcp_unacknowledged: AtomicU32,
    tcp_retransmitted: AtomicU32,
    tcp_total_retransmitted: AtomicU32,
    tcp_bytes_sent: AtomicU64,
    tcp_bytes_received: AtomicU64,
    tcp_send_buffer_explicit: AtomicBool,
    tcp_receive_buffer_explicit: AtomicBool,
    local_tcp_tx_prepared: AtomicBool,
    local_tcp_fast_path_active: AtomicBool,
    local_tcp_window_blocked: AtomicBool,
    local_tcp_direct_pending: AtomicU64,
    local_tcp_direct_events: AtomicU32,
    local_tcp_bulk_active: AtomicBool,
    #[cfg(feature = "performance-profile")]
    tcp_profile_updates: AtomicU64,
    raw_header_included: AtomicBool,
    free_bind: AtomicBool,
    v6_only: AtomicBool,
    ip_hop_limit: AtomicU16,
    ip_traffic_class: AtomicU16,
    /// IP_OPTIONS：随发出的 IPv4 头携带的选项。
    ip_options: Mutex<crate::ip_options::IpOptions>,
    multicast_memberships: Mutex<Vec<MulticastMembership>>,
    multicast_interface: AtomicU32,
    multicast_hops: AtomicU16,
    multicast_loop: AtomicBool,
    socket_mark: AtomicU32,
    socket_priority: AtomicU32,
    rx_dropped: AtomicU64,
    receive_window_update: AtomicBool,
    connect_pending: AtomicBool,
    stream_connected: AtomicBool,
}

fn queue_dma_tx_pool_waiter(facade: &Arc<SocketFacade>, pool_key: usize) {
    let mut waiters = DMA_TX_POOL_WAITERS.lock();
    let mut present = false;
    waiters.retain(|waiter| {
        let Some(existing) = waiter.facade.upgrade() else {
            return false;
        };
        present |= waiter.pool_key == pool_key && Arc::ptr_eq(&existing, facade);
        true
    });
    if !present {
        waiters.push_back(DmaTxPoolWaiter {
            pool_key,
            facade: Arc::downgrade(facade),
        });
    }
}

fn wake_dma_tx_pool_waiter(pool_key: usize, exclude: *const SocketFacade) -> bool {
    let initial_len = DMA_TX_POOL_WAITERS.lock().len();
    for _ in 0..initial_len {
        let waiter = DMA_TX_POOL_WAITERS.lock().pop_front();
        let Some(waiter) = waiter else {
            break;
        };
        let Some(facade) = waiter.facade.upgrade() else {
            continue;
        };
        if waiter.pool_key != pool_key {
            DMA_TX_POOL_WAITERS.lock().push_back(waiter);
            continue;
        }
        if Arc::as_ptr(&facade) == exclude {
            continue;
        }
        if facade.reserve_dma_tx_capacity(pool_key) {
            return true;
        }
        if facade.exhausted_dma_tx_pool_key() == Some(pool_key) {
            DMA_TX_POOL_WAITERS.lock().push_back(waiter);
        }
    }
    false
}

impl SocketFacade {
    pub(crate) fn new(id: SocketId, family: AddressFamily, kind: SocketKind) -> Self {
        let protocol = match kind {
            SocketKind::Datagram => 17,
            SocketKind::Stream => 6,
            SocketKind::Raw => 0,
        };
        Self::new_with_protocol(id, family, kind, protocol)
    }

    pub(crate) fn new_with_protocol(
        id: SocketId,
        family: AddressFamily,
        kind: SocketKind,
        protocol: u8,
    ) -> Self {
        Self {
            id,
            family,
            kind,
            protocol,
            stack_generation: AtomicU64::new(0),
            stack_detached: AtomicBool::new(false),
            packet_observer_active: AtomicBool::new(false),
            generation: AtomicU32::new(1),
            owner: Mutex::new(OwnerRef::Unassigned),
            local: Mutex::new(None),
            peer: Mutex::new(None),
            local_datagram_route: Mutex::new(None),
            local_tcp_direct_route: Mutex::new(None),
            tx: Mutex::new((kind != SocketKind::Stream).then(TxRing::new)),
            rx: Mutex::new((kind != SocketKind::Stream).then(DatagramRx::new)),
            stream_tx: Mutex::new(StreamTxRing::new()),
            stream_rx: Mutex::new(StreamRxRing::new()),
            oob_inline: AtomicBool::new(false),
            oob_byte: Mutex::new(None),
            oob_seq: Mutex::new(None),
            oob_pending: AtomicBool::new(false),
            oob_skip: AtomicBool::new(false),
            stream_base_seq: Mutex::new(None),
            stream_pushed: AtomicU64::new(0),
            stream_consumed: AtomicU64::new(0),
            urgent_owner: Mutex::new(None),
            urgent_tx_pending: AtomicBool::new(false),
            listen_group: Mutex::new(None),

            readiness: AtomicU16::new(if kind != SocketKind::Stream {
                Readiness::WRITABLE.0
            } else {
                0
            }),
            readiness_generation: AtomicU64::new(1),
            observer: Mutex::new(None),
            read_wait: WaitQueue::new_with_reason(sched::WaitReason::SocketRead),
            local_read_handoff: Mutex::new(None),
            write_wait: WaitQueue::new_with_reason(sched::WaitReason::SocketWrite),
            accept_wait: WaitQueue::new_with_reason(sched::WaitReason::SocketRead),
            state_wait: WaitQueue::new_with_reason(sched::WaitReason::Poll),
            control_lock: sched::mutex::Mutex::new(()),
            control_sequence: AtomicU64::new(1),
            control_pending: AtomicBool::new(false),
            control_result: Mutex::new(None),
            tx_notified: AtomicBool::new(false),
            tx_generation: AtomicU64::new(0),
            tx_completed_generation: AtomicU64::new(0),
            lifecycle_notified: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            abortive_close: AtomicBool::new(false),
            read_shutdown: AtomicBool::new(false),
            write_shutdown: AtomicBool::new(false),
            pending_error: AtomicU32::new(0),
            error_queue: Mutex::new(VecDeque::with_capacity(32)),
            next_error_sequence: AtomicU64::new(1),
            error_queue_overflow: AtomicU64::new(0),
            interface: Mutex::new(None),
            tcp_nodelay: AtomicBool::new(false),
            tcp_cork: AtomicBool::new(false),
            tcp_more: AtomicBool::new(false),
            tcp_quick_ack: AtomicBool::new(false),
            tcp_defer_accept_ns: AtomicU64::new(0),
            tcp_notsent_lowat: AtomicU32::new(u32::MAX),
            tcp_user_timeout_ns: AtomicU64::new(0),
            tcp_keepalive: AtomicBool::new(false),
            tcp_keepidle_ns: AtomicU64::new(TCP_KEEPIDLE_DEFAULT_NS),
            tcp_keepintvl_ns: AtomicU64::new(TCP_KEEPINTVL_DEFAULT_NS),
            tcp_keepcount: AtomicU16::new(TCP_KEEPCNT_DEFAULT),
            tcp_maxseg: AtomicU16::new(0),
            tcp_state: AtomicU16::new(7),
            tcp_rto_us: AtomicU32::new(1_000_000),
            tcp_rtt_us: AtomicU32::new(0),
            tcp_rtt_variance_us: AtomicU32::new(0),
            tcp_send_mss: AtomicU32::new(0),
            tcp_congestion_window: AtomicU32::new(0),
            tcp_send_ssthresh: AtomicU32::new(u32::MAX),
            tcp_unacknowledged: AtomicU32::new(0),
            tcp_retransmitted: AtomicU32::new(0),
            tcp_total_retransmitted: AtomicU32::new(0),
            tcp_bytes_sent: AtomicU64::new(0),
            tcp_bytes_received: AtomicU64::new(0),
            tcp_send_buffer_explicit: AtomicBool::new(false),
            tcp_receive_buffer_explicit: AtomicBool::new(false),
            local_tcp_tx_prepared: AtomicBool::new(false),
            local_tcp_fast_path_active: AtomicBool::new(false),
            local_tcp_window_blocked: AtomicBool::new(false),
            local_tcp_direct_pending: AtomicU64::new(0),
            local_tcp_direct_events: AtomicU32::new(0),
            local_tcp_bulk_active: AtomicBool::new(false),
            #[cfg(feature = "performance-profile")]
            tcp_profile_updates: AtomicU64::new(0),
            raw_header_included: AtomicBool::new(false),
            free_bind: AtomicBool::new(false),
            v6_only: AtomicBool::new(false),
            ip_hop_limit: AtomicU16::new(64),
            ip_traffic_class: AtomicU16::new(0),
            ip_options: Mutex::new(crate::ip_options::IpOptions::empty()),
            multicast_memberships: Mutex::new(Vec::new()),
            multicast_interface: AtomicU32::new(0),
            multicast_hops: AtomicU16::new(1),
            multicast_loop: AtomicBool::new(true),
            socket_mark: AtomicU32::new(0),
            socket_priority: AtomicU32::new(0),
            rx_dropped: AtomicU64::new(0),
            receive_window_update: AtomicBool::new(false),
            connect_pending: AtomicBool::new(false),
            stream_connected: AtomicBool::new(false),
        }
    }

    pub const fn id(&self) -> SocketId {
        self.id
    }

    pub const fn family(&self) -> AddressFamily {
        self.family
    }

    pub const fn kind(&self) -> SocketKind {
        self.kind
    }

    pub const fn protocol(&self) -> u8 {
        self.protocol
    }

    fn activate_packet_observer(&self) -> bool {
        self.kind == SocketKind::Raw
            && register_packet_observer(&self.packet_observer_active, &ACTIVE_PACKET_OBSERVERS)
    }

    fn deactivate_packet_observer(&self) {
        if unregister_packet_observer(&self.packet_observer_active, &ACTIVE_PACKET_OBSERVERS) {
            invalidate_local_datagram_routes();
        }
    }

    /// 将 TCP 发送 ring 迁移到目标 queue 的稳定 payload pool。
    ///
    /// 已经排队的数据只在安装时迁移一次；失败时保留原 heap ring，发送语义不变。
    pub fn install_stream_tx_pool(&self, pool: SharedNetBufPool) -> bool {
        self.kind == SocketKind::Stream && self.stream_tx.lock().install_pool(pool)
    }

    /// 让后续 datagram 直接写入目标 queue 的 payload pool。
    pub fn install_datagram_tx_pool(&self, pool: SharedNetBufPool) {
        if let Some(tx) = self.tx.lock().as_mut() {
            tx.install_pool(pool);
        }
        self.refresh_tx_readiness();
    }

    pub fn raw_header_included(&self) -> bool {
        self.raw_header_included.load(Ordering::Acquire)
    }

    pub fn set_raw_header_included(&self, enabled: bool) {
        self.raw_header_included.store(enabled, Ordering::Release);
    }

    pub fn set_free_bind(&self, enabled: bool) {
        self.free_bind.store(enabled, Ordering::Release);
    }

    pub fn free_bind(&self) -> bool {
        self.free_bind.load(Ordering::Acquire)
    }

    pub fn set_v6_only(&self, enabled: bool) {
        self.v6_only.store(enabled, Ordering::Release);
    }

    pub fn v6_only(&self) -> bool {
        self.v6_only.load(Ordering::Acquire)
    }

    pub fn ip_hop_limit(&self) -> u8 {
        self.ip_hop_limit.load(Ordering::Acquire) as u8
    }

    pub fn set_ip_hop_limit(&self, value: u8) {
        self.ip_hop_limit.store(u16::from(value), Ordering::Release);
    }

    pub fn ip_traffic_class(&self) -> u8 {
        self.ip_traffic_class.load(Ordering::Acquire) as u8
    }

    pub fn set_ip_traffic_class(&self, value: u8) {
        self.ip_traffic_class
            .store(u16::from(value), Ordering::Release);
    }

    /// setsockopt(IP_OPTIONS)：设置随 IPv4 头携带的选项（已校验的规范化形式）。
    pub fn set_ip_options(&self, options: crate::ip_options::IpOptions) {
        *self.ip_options.lock() = options;
    }

    pub fn ip_options(&self) -> crate::ip_options::IpOptions {
        *self.ip_options.lock()
    }

    /// IP 选项的 4 字节对齐长度（MSS 计算用）。
    pub fn ip_options_wire_len(&self) -> u8 {
        self.ip_options.lock().wire_len() as u8
    }

    pub fn add_multicast_membership(
        self: &Arc<Self>,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
        self.ensure_stack_attached()?;
        if !membership.group.is_multicast()
            || matches!(
                (self.family, membership.group),
                (AddressFamily::Ipv4, IpAddr::V6(_)) | (AddressFamily::Ipv6, IpAddr::V4(_))
            )
        {
            return Err(SocketError::AddressUnavailable);
        }
        let mut memberships = self.multicast_memberships.lock();
        if memberships.contains(&membership) {
            return Err(SocketError::AddressInUse);
        }
        if memberships.len() >= 64 {
            return Err(SocketError::Buffer);
        }
        memberships.push(membership);
        drop(memberships);
        if let Ok(runtime) = socket_runtime()
            && let Err(error) = runtime.update_multicast(Arc::clone(self), membership, true)
        {
            self.multicast_memberships
                .lock()
                .retain(|entry| *entry != membership);
            return Err(error);
        }
        Ok(())
    }

    pub fn drop_multicast_membership(
        self: &Arc<Self>,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
        self.ensure_stack_attached()?;
        let mut memberships = self.multicast_memberships.lock();
        let Some(index) = memberships.iter().position(|entry| *entry == membership) else {
            return Err(SocketError::AddressUnavailable);
        };
        memberships.swap_remove(index);
        drop(memberships);
        if let Ok(runtime) = socket_runtime()
            && let Err(error) = runtime.update_multicast(Arc::clone(self), membership, false)
        {
            self.multicast_memberships.lock().push(membership);
            return Err(error);
        }
        Ok(())
    }

    pub fn accepts_multicast(&self, group: IpAddr, interface: InterfaceId) -> bool {
        self.multicast_memberships.lock().iter().any(|entry| {
            entry.group == group && entry.interface.is_none_or(|scope| scope == interface)
        })
    }

    pub fn has_multicast_memberships(&self) -> bool {
        !self.multicast_memberships.lock().is_empty()
    }

    pub fn set_multicast_interface(&self, interface: Option<InterfaceId>) {
        self.multicast_interface.store(
            interface.map_or(0, |interface| interface.0),
            Ordering::Release,
        );
    }

    pub fn multicast_interface(&self) -> Option<InterfaceId> {
        let raw = self.multicast_interface.load(Ordering::Acquire);
        (raw != 0).then_some(InterfaceId(raw))
    }

    pub fn set_multicast_hops(&self, hops: u8) {
        self.multicast_hops
            .store(u16::from(hops), Ordering::Release);
    }

    pub fn multicast_hops(&self) -> u8 {
        self.multicast_hops.load(Ordering::Acquire) as u8
    }

    pub fn set_multicast_loop(&self, enabled: bool) {
        self.multicast_loop.store(enabled, Ordering::Release);
    }

    pub fn multicast_loop(&self) -> bool {
        self.multicast_loop.load(Ordering::Acquire)
    }

    pub fn set_socket_mark(&self, mark: u32) {
        self.socket_mark.store(mark, Ordering::Release);
    }

    pub fn socket_mark(&self) -> u32 {
        self.socket_mark.load(Ordering::Acquire)
    }

    pub fn set_socket_priority(&self, priority: i32) {
        self.socket_priority
            .store(priority as u32, Ordering::Release);
    }

    pub fn socket_priority(&self) -> i32 {
        self.socket_priority.load(Ordering::Acquire) as i32
    }

    pub fn take_rx_overflow(&self) -> u32 {
        self.rx_dropped
            .swap(0, Ordering::AcqRel)
            .min(u64::from(u32::MAX)) as u32
    }

    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn stack_generation(&self) -> u64 {
        self.stack_generation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_set_stack_generation(&self, generation: u64) {
        assert_ne!(generation, 0);
        self.stack_generation.store(generation, Ordering::Release);
    }

    pub(crate) fn inherit_stack_generation(&self, parent: &Self) {
        let generation = parent.stack_generation();
        if generation != 0 {
            let _ = self.stack_generation.compare_exchange(
                0,
                generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    pub fn backend_error(&self) -> Option<SocketError> {
        self.stack_detached
            .load(Ordering::Acquire)
            .then_some(SocketError::NetworkDown)
    }

    fn ensure_stack_attached(&self) -> Result<(), SocketError> {
        self.backend_error().map_or(Ok(()), Err)
    }

    fn detach_stack(&self) {
        if self.stack_detached.swap(true, Ordering::AcqRel) {
            return;
        }
        self.deactivate_packet_observer();
        self.clear_local_datagram_route();
        self.clear_local_tcp_direct_route();
        self.local_tcp_direct_pending.store(0, Ordering::Release);
        self.local_tcp_direct_events.store(0, Ordering::Release);
        self.local_read_handoff.lock().take();
        self.local_tcp_tx_prepared.store(false, Ordering::Release);
        self.local_tcp_fast_path_active
            .store(false, Ordering::Release);
        self.local_tcp_window_blocked
            .store(false, Ordering::Release);
        if self.kind != SocketKind::Stream {
            invalidate_local_datagram_routes();
        }
        self.set_pending_error(SocketError::NetworkDown);
        self.update_ready(
            (Readiness::ERROR | Readiness::HANGUP | Readiness::READ_HANGUP).0,
            Readiness::WRITABLE.0 | Readiness::ACCEPTABLE.0,
        );
        self.read_wait.wake_all();
        self.write_wait.wake_all();
        self.accept_wait.wake_all();
        self.state_wait.wake_all();
    }

    pub fn detach_stack_for_generation(&self) {
        self.detach_stack();
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub fn request_abortive_close(&self) {
        self.abortive_close.store(true, Ordering::Release);
    }

    pub fn is_abortive_close(&self) -> bool {
        self.abortive_close.load(Ordering::Acquire)
    }

    pub fn write_is_shutdown(&self) -> bool {
        self.write_shutdown.load(Ordering::Acquire)
    }

    pub fn owner(&self) -> OwnerRef {
        *self.owner.lock()
    }

    pub fn local_endpoint(&self) -> Option<Endpoint> {
        *self.local.lock()
    }

    pub fn peer_endpoint(&self) -> Option<Endpoint> {
        *self.peer.lock()
    }

    pub fn interface(&self) -> Option<InterfaceId> {
        *self.interface.lock()
    }

    pub fn readiness(&self) -> (Readiness, u64) {
        (
            Readiness(self.readiness.load(Ordering::Acquire)),
            self.readiness_generation.load(Ordering::Acquire),
        )
    }

    pub fn set_observer(&self, observer: Weak<dyn ReadinessObserver>) {
        *self.observer.lock() = observer.upgrade();
    }

    pub fn add_poll_waiter(
        &self,
        task: &Arc<sched::Task>,
        read: bool,
        write: bool,
        state: bool,
    ) -> bool {
        if read {
            self.read_wait.enqueue(task);
        }
        if write {
            self.write_wait.enqueue(task);
        }
        if state {
            self.state_wait.enqueue(task);
        }
        read || write || state
    }

    pub fn remove_poll_waiter(&self, task: &Arc<sched::Task>) {
        self.read_wait.remove(task);
        self.write_wait.remove(task);
        self.accept_wait.remove(task);
        self.state_wait.remove(task);
    }

    pub fn bind(
        self: &Arc<Self>,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<(), SocketError> {
        let sequence = self.begin_bind(local, interface, options)?;
        self.wait_control(sequence)
    }

    pub fn begin_bind(
        self: &Arc<Self>,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<u64, SocketError> {
        self.ensure_stack_attached()?;
        let _control = self.control_lock.lock();
        if !matches!(self.owner(), OwnerRef::Unassigned) {
            return Err(SocketError::InvalidState);
        }
        if self.control_pending.swap(true, Ordering::AcqRel) {
            return Err(SocketError::AlreadyInProgress);
        }
        let sequence = self.next_control_sequence();
        let command = SocketCommand::Bind {
            facade: Arc::clone(self),
            sequence,
            generation: self.generation(),
            local,
            interface,
            options,
        };
        if socket_runtime()?.submit_control(command).is_err() {
            self.control_pending.store(false, Ordering::Release);
            return Err(SocketError::RuntimeBusy);
        }
        Ok(sequence)
    }

    pub fn connect(
        self: &Arc<Self>,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<(), SocketError> {
        self.connect_with_mode(peer, interface, options, false)
    }

    pub fn connect_with_mode(
        self: &Arc<Self>,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        nonblocking: bool,
    ) -> Result<(), SocketError> {
        self.ensure_stack_attached()?;
        let _control = self.control_lock.lock();
        if self.kind == SocketKind::Stream && self.connect_pending.swap(true, Ordering::AcqRel) {
            return Err(SocketError::AlreadyInProgress);
        }
        if self.peer_endpoint().is_some() {
            let error = if self.kind == SocketKind::Stream
                && !self.stream_connected.load(Ordering::Acquire)
                && matches!(self.owner(), OwnerRef::Flow { .. })
            {
                SocketError::AlreadyInProgress
            } else {
                SocketError::AlreadyConnected
            };
            self.connect_pending.store(false, Ordering::Release);
            return Err(error);
        }
        let sequence = self.next_control_sequence();
        let command = SocketCommand::Connect {
            facade: Arc::clone(self),
            sequence,
            generation: self.generation(),
            peer,
            interface,
            options,
            nonblocking,
        };
        if socket_runtime()?.submit_control(command).is_err() {
            self.connect_pending.store(false, Ordering::Release);
            return Err(SocketError::RuntimeBusy);
        }
        if nonblocking && self.kind == SocketKind::Stream {
            return Err(SocketError::InProgress);
        }
        let result = self.wait_control(sequence);
        self.connect_pending.store(false, Ordering::Release);
        result
    }

    pub fn listen(self: &Arc<Self>, backlog: u32) -> Result<(), SocketError> {
        let sequence = self.begin_listen(backlog)?;
        self.wait_control(sequence)
    }

    pub fn begin_listen(self: &Arc<Self>, backlog: u32) -> Result<u64, SocketError> {
        self.ensure_stack_attached()?;
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        let _control = self.control_lock.lock();
        if self.control_pending.swap(true, Ordering::AcqRel) {
            return Err(SocketError::AlreadyInProgress);
        }
        let sequence = self.next_control_sequence();
        let command = SocketCommand::Listen {
            facade: Arc::clone(self),
            sequence,
            generation: self.generation(),
            backlog,
        };
        if socket_runtime()?.submit_control(command).is_err() {
            self.control_pending.store(false, Ordering::Release);
            return Err(SocketError::RuntimeBusy);
        }
        Ok(sequence)
    }

    pub fn accept(
        self: &Arc<Self>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<Arc<SocketFacade>, SocketError> {
        self.ensure_stack_attached()?;
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        loop {
            self.ensure_stack_attached()?;
            let group = self.listen_group.lock().as_ref().cloned();
            let Some(group) = group else {
                return Err(if self.closing.load(Ordering::Acquire) {
                    SocketError::Closed
                } else {
                    SocketError::InvalidState
                });
            };
            if let Some(child) = group.accept() {
                self.refresh_accept_readiness();
                return Ok(child);
            }
            if self.closing.load(Ordering::Acquire) {
                return Err(SocketError::Closed);
            }
            if nonblocking {
                return Err(SocketError::WouldBlock);
            }
            self.wait_accept(deadline_ns)?;
        }
    }

    pub fn install_listen_group(&self, group: Arc<ListenGroup>) {
        *self.listen_group.lock() = Some(group);
    }

    pub fn listener_backlog(&self) -> usize {
        self.listen_group
            .lock()
            .as_ref()
            .map_or(1, |group| group.accept_limit())
    }

    pub fn listen_group(&self) -> Option<Arc<ListenGroup>> {
        self.listen_group.lock().as_ref().cloned()
    }

    pub(crate) fn notify_accept_ready(&self, cpu_hint: usize) {
        self.set_ready(Readiness::ACCEPTABLE | Readiness::READABLE);
        self.accept_wait.wake_one_with(|task| {
            let _ = sched::activate_task_with_cpu_hint(task, cpu_hint);
        });
        self.read_wait.wake_one_with(|task| {
            let _ = sched::activate_task_with_cpu_hint(task, cpu_hint);
        });
    }

    pub(crate) fn install_local_datagram_route(&self, route: LocalDatagramRoute) {
        if self.kind == SocketKind::Datagram && local_transport_fast_path_eligible() {
            if let Ok(_resident) = enter_resident_allocation_scope() {
                route
                    .receiver
                    .rx
                    .lock()
                    .as_mut()
                    .expect("UDP facade 必须拥有 RX ring")
                    .prepare_local_lane();
            }
            *self.local_datagram_route.lock() = Some(route);
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::UdpLocalRouteInstalls, 1);
        }
    }

    pub(crate) fn remember_local_datagram_route(
        &self,
        receiver: Arc<SocketFacade>,
        destination: Endpoint,
        source: Endpoint,
        delivered_to: Endpoint,
        interface: InterfaceId,
        dont_route: bool,
        confirm: bool,
        mark: u32,
        hop_limit: u8,
        traffic_class: u8,
        route_mtu: u32,
    ) {
        let route = LocalDatagramRoute {
            epoch: local_datagram_route_epoch(),
            stack_generation: self.stack_generation(),
            sender_generation: self.generation(),
            receiver_generation: receiver.generation(),
            destination,
            source,
            delivered_to,
            interface,
            dont_route,
            confirm,
            mark,
            hop_limit,
            traffic_class,
            route_mtu,
            receiver,
        };
        self.install_local_datagram_route(route);
    }

    fn clear_local_datagram_route(&self) {
        self.local_datagram_route.lock().take();
    }

    fn local_datagram_route_matches(
        &self,
        route: &LocalDatagramRoute,
        destination: Endpoint,
        payload_len: usize,
        dont_route: bool,
        confirm: bool,
    ) -> bool {
        let epoch_matches = route.epoch == local_datagram_route_epoch();
        #[cfg(test)]
        let epoch_matches = epoch_matches || route.epoch == u64::MAX;
        epoch_matches
            && local_transport_fast_path_eligible()
            && route.stack_generation == self.stack_generation()
            && route.sender_generation == self.generation()
            && route.receiver_generation == route.receiver.generation()
            && route.receiver.stack_generation() == route.stack_generation
            && !route.receiver.is_closing()
            && route.destination == destination
            && crate::transport::local_udp_payload_fits_route(
                destination.addr,
                payload_len,
                route.route_mtu,
            )
            && route.dont_route == dont_route
            && route.confirm == confirm
            && route.mark == self.socket_mark()
            && route.hop_limit == self.ip_hop_limit()
            && route.traffic_class == self.ip_traffic_class()
            && route.interface == self.interface().unwrap_or(route.interface)
    }

    pub fn send(
        self: &Arc<Self>,
        payload: &[u8],
        destination: Option<Endpoint>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        self.send_datagram(payload, destination, nonblocking, deadline_ns, false, false)
    }

    pub fn send_datagram(
        self: &Arc<Self>,
        payload: &[u8],
        destination: Option<Endpoint>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        dont_route: bool,
        confirm: bool,
    ) -> Result<usize, SocketError> {
        match self.send_datagram_from(
            payload.len(),
            destination,
            nonblocking,
            deadline_ns,
            dont_route,
            confirm,
            |offset, output| {
                output.copy_from_slice(&payload[offset..offset + output.len()]);
                Ok::<(), core::convert::Infallible>(())
            },
        ) {
            Ok(len) => Ok(len),
            Err(DatagramCopyError::Socket(error)) => Err(error),
            Err(DatagramCopyError::Copy(error)) => match error {},
        }
    }

    /// 把外部复制源直接写入预留的数据报槽，避免在 syscall 层建立中间缓冲。
    ///
    /// copy 失败时槽位和 chunk 全部回滚，数据报不会对协议层或接收端可见。
    pub fn send_datagram_from<E>(
        self: &Arc<Self>,
        payload_len: usize,
        destination: Option<Endpoint>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        dont_route: bool,
        confirm: bool,
        mut copy: impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<usize, DatagramCopyError<E>> {
        self.ensure_stack_attached()
            .map_err(DatagramCopyError::Socket)?;
        if self.closing.load(Ordering::Acquire) {
            return Err(DatagramCopyError::Socket(SocketError::Closed));
        }
        if self.write_shutdown.load(Ordering::Acquire) {
            return Err(DatagramCopyError::Socket(SocketError::WriteShutdown));
        }
        let destination = destination
            .or_else(|| self.peer_endpoint())
            .ok_or(DatagramCopyError::Socket(SocketError::DestinationRequired))?;
        let max_payload = match (self.kind, self.family) {
            (SocketKind::Raw, _) => u16::MAX as usize,
            (SocketKind::Datagram, AddressFamily::Ipv4) => MAX_UDP4_PAYLOAD,
            (SocketKind::Datagram, AddressFamily::Ipv6) => MAX_UDP6_PAYLOAD,
            (SocketKind::Stream, _) => {
                return Err(DatagramCopyError::Socket(SocketError::InvalidState));
            }
        };
        if payload_len > max_payload {
            return Err(DatagramCopyError::Socket(SocketError::MessageTooLarge));
        }
        let cached_route = self.local_datagram_route.lock().clone();
        if let Some(route) = cached_route {
            if self.local_datagram_route_matches(
                &route,
                destination,
                payload_len,
                dont_route,
                confirm,
            ) {
                #[cfg(feature = "performance-profile")]
                let direct_start = profiling::read_counter();
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::UdpLocalRouteMatches, 1);
                let direct_result = route.receiver.push_local_udp_from(
                    payload_len,
                    route.source,
                    route.delivered_to,
                    route.hop_limit,
                    route.traffic_class,
                    route.interface,
                    sched::now_ns_public(),
                    &mut copy,
                );
                #[cfg(feature = "performance-profile")]
                profiling::observe(
                    profiling::Metric::UdpLocalDirectCycles,
                    profiling::read_counter().wrapping_sub(direct_start),
                );
                match direct_result {
                    Ok(()) => {
                        #[cfg(feature = "performance-profile")]
                        {
                            profiling::observe(profiling::Metric::UdpLocalRouteDeliveries, 1);
                            profiling::observe(
                                profiling::Metric::UdpLocalDirectBytes,
                                payload_len as u64,
                            );
                        }
                        return Ok(payload_len);
                    }
                    Err(DatagramCopyError::Copy(error)) => {
                        return Err(DatagramCopyError::Copy(error));
                    }
                    Err(DatagramCopyError::Socket(SocketError::WouldBlock)) => {
                        #[cfg(feature = "performance-profile")]
                        profiling::observe(profiling::Metric::UdpLocalRouteReceiverRejects, 1);
                        route.receiver.request_local_udp_consumer_handoff();
                        return Ok(payload_len);
                    }
                    Err(DatagramCopyError::Socket(_)) => {
                        #[cfg(feature = "performance-profile")]
                        profiling::observe(profiling::Metric::UdpLocalRouteReceiverRejects, 1);
                    }
                }
            } else {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::UdpLocalRouteInvalid, 1);
            }
            self.clear_local_datagram_route();
        } else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::UdpLocalRouteAbsent, 1);
        }
        loop {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::UdpLocalFallbackDatagrams, 1);
            let pushed = {
                let mut tx_guard = self.tx.lock();
                let tx = tx_guard.as_mut().expect("UDP facade 必须拥有 TX ring");
                let was_empty = tx.is_empty();
                match tx.push_from(payload_len, destination, dont_route, confirm, &mut copy) {
                    Ok(()) => Ok(was_empty),
                    Err(DatagramCopyError::Socket(error)) => Err((error, tx.exhausted_pool_key())),
                    Err(DatagramCopyError::Copy(error)) => {
                        return Err(DatagramCopyError::Copy(error));
                    }
                }
            };
            let was_empty = match pushed {
                Ok(was_empty) => was_empty,
                Err((SocketError::WouldBlock, pool_key)) => {
                    if let Some(pool_key) = pool_key {
                        queue_dma_tx_pool_waiter(self, pool_key);
                    }
                    self.refresh_tx_readiness();
                    if nonblocking {
                        return Err(DatagramCopyError::Socket(SocketError::WouldBlock));
                    }
                    self.wait_datagram_write(payload_len, deadline_ns)
                        .map_err(DatagramCopyError::Socket)?;
                    continue;
                }
                Err((error, _)) => return Err(DatagramCopyError::Socket(error)),
            };
            self.refresh_tx_readiness();
            if was_empty && !self.tx_notified.swap(true, Ordering::AcqRel) {
                socket_runtime()
                    .map_err(DatagramCopyError::Socket)?
                    .notify_tx(Arc::clone(self), SocketTxCause::Datagram);
            }
            return Ok(payload_len);
        }
    }

    pub fn take_tx(self: &Arc<Self>) -> Option<UdpTxLease> {
        self.tx
            .lock()
            .as_mut()
            .expect("UDP facade 必须拥有 TX ring")
            .take(Arc::clone(self))
    }

    pub fn finish_tx_drain(self: &Arc<Self>) {
        self.tx_notified.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        let pending = match self.kind {
            SocketKind::Datagram | SocketKind::Raw => !self
                .tx
                .lock()
                .as_ref()
                .expect("UDP facade 必须拥有 TX ring")
                .is_empty(),
            SocketKind::Stream => {
                let tx = self.stream_tx.lock();
                tx.sent < tx.bytes.len()
            }
        };
        if pending && !self.tx_notified.swap(true, Ordering::AcqRel) {
            socket_runtime()
                .expect("socket runtime 必须保持安装")
                .notify_tx(Arc::clone(self), SocketTxCause::DrainRecheck);
        }
    }

    pub fn has_pending_datagram_tx(&self) -> bool {
        matches!(self.kind, SocketKind::Datagram | SocketKind::Raw)
            && !self
                .tx
                .lock()
                .as_ref()
                .expect("datagram facade 必须拥有 TX ring")
                .is_empty()
    }

    pub fn next_datagram_destination(&self) -> Option<Endpoint> {
        matches!(self.kind, SocketKind::Datagram | SocketKind::Raw)
            .then(|| self.tx.lock().as_ref().and_then(TxRing::next_destination))
            .flatten()
    }

    pub fn stream_tx_generation(&self) -> u64 {
        self.tx_generation.load(Ordering::Acquire)
    }

    pub fn finish_stream_tx_drain(self: &Arc<Self>, observed_generation: u64) {
        self.tx_completed_generation
            .store(observed_generation, Ordering::Release);
        self.tx_notified.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        if self.tx_generation.load(Ordering::Acquire) != observed_generation
            && !self.tx_notified.swap(true, Ordering::AcqRel)
        {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpTxNotifyDrainRecheck, 1);
            socket_runtime()
                .expect("socket runtime 必须保持安装")
                .notify_tx(Arc::clone(self), SocketTxCause::DrainRecheck);
        }
    }

    pub fn tcp_nodelay(&self) -> bool {
        self.tcp_nodelay.load(Ordering::Acquire)
    }

    pub fn set_tcp_nodelay(self: &Arc<Self>, enabled: bool) {
        let changed = self.tcp_nodelay.swap(enabled, Ordering::AcqRel) != enabled;
        if changed && enabled {
            self.request_stream_recheck();
        }
    }

    pub fn tcp_cork(&self) -> bool {
        self.tcp_cork.load(Ordering::Acquire)
    }

    pub fn set_tcp_cork(self: &Arc<Self>, enabled: bool) {
        let changed = self.tcp_cork.swap(enabled, Ordering::AcqRel) != enabled;
        if changed && !enabled {
            self.request_stream_recheck();
        }
    }

    pub fn tcp_more(&self) -> bool {
        self.tcp_more.load(Ordering::Acquire)
    }

    pub fn set_tcp_more(self: &Arc<Self>, enabled: bool) {
        let changed = self.tcp_more.swap(enabled, Ordering::AcqRel) != enabled;
        if changed && !enabled {
            let generation = self.tx_generation.load(Ordering::Acquire);
            let completed = self.tx_completed_generation.load(Ordering::Acquire);
            let unsent = self.stream_unsent_len();
            let send_mss = usize::try_from(self.tcp_send_mss.load(Ordering::Acquire))
                .unwrap_or(usize::MAX)
                .max(1);
            // 已完成的同一代大块数据只能等待 ACK/window 继续推进；清除 MSG_MORE
            // 不会改变它的可发送性。只有小于 MSS 的尾包需要强制解除 cork。
            if generation == completed && unsent != 0 && unsent < send_mss {
                self.tx_generation.fetch_add(1, Ordering::Release);
            }
            self.notify_stream_pending();
        }
    }

    fn notify_stream_pending(self: &Arc<Self>) {
        let _ = self.publish_stream_pending();
    }

    fn request_stream_recheck(self: &Arc<Self>) {
        self.tx_generation.fetch_add(1, Ordering::Release);
        self.notify_stream_pending();
    }

    fn publish_stream_pending(self: &Arc<Self>) -> Result<(), SocketError> {
        let direct = self.try_deliver_local_tcp_direct();
        let direct_reconcile =
            direct != 0 && (self.local_tcp_direct_reconcile_due() || self.stream_unsent_len() != 0);
        if direct_reconcile && !self.tx_notified.swap(true, Ordering::AcqRel) {
            match socket_runtime() {
                Ok(runtime) => {
                    runtime.notify_tx(Arc::clone(self), SocketTxCause::StreamLocalDirect)
                }
                Err(error) => {
                    self.tx_notified.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if self.stream_unsent_len() == 0 {
            return Ok(());
        }
        if self.tx_generation.load(Ordering::Acquire)
            == self.tx_completed_generation.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if !self.tx_notified.swap(true, Ordering::AcqRel) {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpTxNotifyPayload, 1);
            match socket_runtime() {
                Ok(runtime) => runtime.notify_tx(Arc::clone(self), SocketTxCause::StreamPayload),
                Err(error) => {
                    self.tx_notified.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub fn request_quick_ack(&self) {
        self.tcp_quick_ack.store(true, Ordering::Release);
    }

    pub fn take_quick_ack(&self) -> bool {
        self.tcp_quick_ack.swap(false, Ordering::AcqRel)
    }

    pub fn tcp_defer_accept_ns(&self) -> u64 {
        self.tcp_defer_accept_ns.load(Ordering::Acquire)
    }

    pub fn set_tcp_defer_accept_ns(&self, timeout_ns: u64) {
        self.tcp_defer_accept_ns
            .store(timeout_ns, Ordering::Release);
    }

    pub fn tcp_notsent_lowat(&self) -> u32 {
        self.tcp_notsent_lowat.load(Ordering::Acquire)
    }

    pub fn set_tcp_notsent_lowat(&self, value: u32) {
        self.tcp_notsent_lowat.store(value, Ordering::Release);
        self.refresh_tx_readiness();
    }

    pub fn tcp_user_timeout_ns(&self) -> u64 {
        self.tcp_user_timeout_ns.load(Ordering::Acquire)
    }

    pub fn set_tcp_user_timeout_ns(&self, timeout_ns: u64) {
        self.tcp_user_timeout_ns
            .store(timeout_ns, Ordering::Release);
    }

    pub fn tcp_keepalive_enabled(&self) -> bool {
        self.tcp_keepalive.load(Ordering::Acquire)
    }

    pub fn set_tcp_keepalive(self: &Arc<Self>, enabled: bool) {
        if self.tcp_keepalive.swap(enabled, Ordering::AcqRel) != enabled {
            self.notify_tcp_state_change();
        }
    }

    pub fn tcp_keepidle_ns(&self) -> u64 {
        self.tcp_keepidle_ns.load(Ordering::Acquire)
    }

    pub fn set_tcp_keepidle_ns(self: &Arc<Self>, value: u64) {
        self.tcp_keepidle_ns
            .store(value.max(1_000_000_000), Ordering::Release);
        self.notify_tcp_state_change();
    }

    pub fn tcp_keepintvl_ns(&self) -> u64 {
        self.tcp_keepintvl_ns.load(Ordering::Acquire)
    }

    pub fn set_tcp_keepintvl_ns(self: &Arc<Self>, value: u64) {
        self.tcp_keepintvl_ns
            .store(value.max(1_000_000_000), Ordering::Release);
        self.notify_tcp_state_change();
    }

    pub fn tcp_keepcount(&self) -> u8 {
        self.tcp_keepcount
            .load(Ordering::Acquire)
            .min(u16::from(u8::MAX)) as u8
    }

    pub fn tcp_maxseg(&self) -> u16 {
        self.tcp_maxseg.load(Ordering::Acquire)
    }

    pub fn set_tcp_maxseg(&self, value: u16) {
        self.tcp_maxseg.store(value, Ordering::Release);
    }

    pub fn update_tcp_info(
        &self,
        state: u8,
        rto_us: u32,
        rtt_us: u32,
        rtt_variance_us: u32,
        send_mss: u32,
        congestion_window: u32,
        send_ssthresh: u32,
        unacknowledged: u32,
        retransmitted: u32,
    ) {
        self.tcp_state.store(u16::from(state), Ordering::Release);
        self.tcp_rto_us.store(rto_us, Ordering::Release);
        self.tcp_rtt_us.store(rtt_us, Ordering::Release);
        self.tcp_rtt_variance_us
            .store(rtt_variance_us, Ordering::Release);
        self.tcp_send_mss.store(send_mss, Ordering::Release);
        self.tcp_congestion_window
            .store(congestion_window, Ordering::Release);
        self.tcp_send_ssthresh
            .store(send_ssthresh, Ordering::Release);
        self.tcp_unacknowledged
            .store(unacknowledged, Ordering::Release);
        self.tcp_retransmitted
            .store(retransmitted, Ordering::Release);
    }

    pub fn record_tcp_retransmission(&self) {
        self.tcp_total_retransmitted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn tcp_info(&self) -> TcpInfoSnapshot {
        TcpInfoSnapshot {
            state: self.tcp_state.load(Ordering::Acquire) as u8,
            rto_us: self.tcp_rto_us.load(Ordering::Acquire),
            rtt_us: self.tcp_rtt_us.load(Ordering::Acquire),
            rtt_variance_us: self.tcp_rtt_variance_us.load(Ordering::Acquire),
            send_mss: self.tcp_send_mss.load(Ordering::Acquire),
            congestion_window: self.tcp_congestion_window.load(Ordering::Acquire),
            send_ssthresh: self.tcp_send_ssthresh.load(Ordering::Acquire),
            unacknowledged: self.tcp_unacknowledged.load(Ordering::Acquire),
            retransmitted: self.tcp_retransmitted.load(Ordering::Acquire),
            total_retransmitted: self.tcp_total_retransmitted.load(Ordering::Acquire),
            receive_space: self.stream_receive_window().min(u32::MAX as usize) as u32,
            bytes_sent: self.tcp_bytes_sent.load(Ordering::Acquire),
            bytes_received: self.tcp_bytes_received.load(Ordering::Acquire),
        }
    }

    #[cfg(feature = "performance-profile")]
    pub(crate) fn tcp_profile_trace_due(&self) -> bool {
        self.tcp_profile_updates.fetch_add(1, Ordering::Relaxed) & 0xff == 0
    }

    pub fn set_tcp_keepcount(self: &Arc<Self>, value: u16) {
        self.tcp_keepcount.store(value.max(1), Ordering::Release);
        self.notify_tcp_state_change();
    }

    pub fn take_receive_window_update(&self) -> bool {
        self.receive_window_update.swap(false, Ordering::AcqRel)
    }

    pub fn finish_stream_receive(self: &Arc<Self>) {
        if self.receive_window_update.load(Ordering::Acquire) {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpReceiveWindowNotifications, 1);
            self.notify_tcp_state_change();
        }
    }

    fn notify_tcp_state_change(self: &Arc<Self>) {
        self.notify_tcp_state_change_with_cause(SocketTxCause::StreamState);
    }

    fn notify_tcp_state_change_with_cause(self: &Arc<Self>, cause: SocketTxCause) {
        if matches!(self.owner(), OwnerRef::Flow { .. }) {
            self.tx_generation.fetch_add(1, Ordering::Release);
            if !self.tx_notified.swap(true, Ordering::AcqRel)
                && let Ok(runtime) = socket_runtime()
            {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::TcpTxNotifyState, 1);
                runtime.notify_tx(Arc::clone(self), cause);
            }
        }
    }

    /// 为已确认的本地 TCP 发送方向启用按需扩容。
    ///
    /// 这里只改变内部容量；用户通过 SO_SNDBUF 设置的上限始终优先。
    pub fn prepare_local_stream_send(&self) {
        if self.kind == SocketKind::Stream && !self.tcp_send_buffer_explicit.load(Ordering::Acquire)
        {
            self.stream_tx.lock().bytes.enable_local_autotune();
        }
    }

    pub(crate) fn install_local_tcp_direct_peer(self: &Arc<Self>, peer: &Arc<SocketFacade>) {
        let local_owner = self.owner();
        let peer_owner = peer.owner();
        if self.kind != SocketKind::Stream
            || peer.kind != SocketKind::Stream
            || self.stack_generation() == 0
            || self.stack_generation() != peer.stack_generation()
            || !matches!(local_owner, OwnerRef::Flow { .. })
            || !matches!(peer_owner, OwnerRef::Flow { .. })
        {
            return;
        }
        let mut route = self.local_tcp_direct_route.lock();
        if self.stream_tx.lock().bytes.len() != 0 {
            return;
        }
        *route = Some(LocalTcpDirectRoute {
            local_generation: self.generation(),
            peer_generation: peer.generation(),
            stack_generation: self.stack_generation(),
            local_owner,
            peer_owner,
            peer: Arc::downgrade(peer),
        });
    }

    pub(crate) fn clear_local_tcp_direct_route(&self) {
        self.local_tcp_direct_route.lock().take();
    }

    fn mark_local_tcp_bulk_active(&self) {
        if !self.local_tcp_bulk_active.swap(true, Ordering::AcqRel) {
            LOCAL_TCP_BULK_SENDERS.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn clear_local_tcp_bulk_active(&self) {
        if self.local_tcp_bulk_active.swap(false, Ordering::AcqRel) {
            LOCAL_TCP_BULK_SENDERS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn local_tcp_direct_peer_for_route(
        &self,
        route: &LocalTcpDirectRoute,
    ) -> Option<Arc<SocketFacade>> {
        let peer = route.peer.upgrade()?;
        (route.local_generation == self.generation()
            && route.peer_generation == peer.generation()
            && route.stack_generation == self.stack_generation()
            && route.stack_generation == peer.stack_generation()
            && route.local_owner == self.owner()
            && route.peer_owner == peer.owner()
            && !self.is_closing()
            && !peer.is_closing())
        .then_some(peer)
    }

    fn try_deliver_local_tcp_direct(self: &Arc<Self>) -> usize {
        #[cfg(feature = "performance-profile")]
        let direct_start = profiling::read_counter();
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::TcpLocalDirectAttempts, 1);
        if self.tcp_cork()
            || self.tcp_more()
            || !self.local_tcp_tx_prepared.load(Ordering::Acquire)
            || !local_transport_fast_path_eligible()
        {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpLocalDirectPolicyRejects, 1);
            return 0;
        }
        let route = self.local_tcp_direct_route.lock();
        let Some(peer) = route
            .as_ref()
            .and_then(|route| self.local_tcp_direct_peer_for_route(route))
        else {
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::TcpLocalDirectRouteMisses, 1);
            return 0;
        };
        let mut delivered = 0usize;
        loop {
            let available = peer.stream_receive_window();
            let max_len = available.min(u16::MAX as usize);
            if max_len == 0 {
                if self.stream_unsent_len() != 0 {
                    peer.mark_local_stream_window_blocked();
                    #[cfg(feature = "performance-profile")]
                    profiling::observe(profiling::Metric::TcpLocalDirectWindowBlocks, 1);
                }
                break;
            }
            let Some((lease, flush)) = self.take_stream_tx_direct_deferred(max_len) else {
                break;
            };
            let lease_len = usize::from(lease.len);
            if peer
                .push_stream_rx_lease(&lease, 0, lease_len, flush)
                .is_err()
            {
                self.stream_tx.lock().rewind_unsent(lease_len);
                peer.mark_local_stream_window_blocked();
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::TcpLocalDirectWindowBlocks, 1);
                break;
            }
            self.finish_stream_tx_batch(lease_len);
            self.acknowledge_stream(lease_len);
            delivered += lease_len;
        }
        if delivered != 0 {
            self.local_tcp_direct_pending
                .fetch_add(delivered as u64, Ordering::AcqRel);
            self.local_tcp_direct_events.fetch_add(1, Ordering::AcqRel);
            #[cfg(feature = "performance-profile")]
            {
                profiling::observe(profiling::Metric::TcpLocalDirectDeliveries, 1);
                profiling::observe(profiling::Metric::TcpLocalDirectBytes, delivered as u64);
                profiling::observe(
                    profiling::Metric::TcpLocalDirectCycles,
                    profiling::read_counter().wrapping_sub(direct_start),
                );
            }
        }
        delivered
    }

    fn try_send_local_tcp_direct_from(
        self: &Arc<Self>,
        payload_len: usize,
        copy: &mut impl FnMut(usize, &mut [u8]),
    ) -> Result<Option<usize>, SocketError> {
        if payload_len == 0
            || payload_len > TCP_LOCAL_DIRECT_COPY_BYTES
            || self.tcp_cork()
            || self.tcp_more()
            || !local_transport_fast_path_eligible()
        {
            return Ok(None);
        }
        let route = self.local_tcp_direct_route.lock();
        let Some(peer) = route
            .as_ref()
            .and_then(|route| self.local_tcp_direct_peer_for_route(route))
        else {
            return Ok(None);
        };
        if self.stream_tx.lock().bytes.len() != 0 {
            return Ok(None);
        }
        if payload_len > TCP_LOCAL_IMMEDIATE_HANDOFF_BYTES {
            self.mark_local_tcp_bulk_active();
        }
        if peer.stream_receive_window() < payload_len {
            peer.mark_local_stream_window_blocked();
            return Ok(None);
        }
        #[cfg(feature = "performance-profile")]
        let direct_start = profiling::read_counter();
        match peer.push_stream_rx_local_from(payload_len, copy) {
            Ok(_) => {}
            Err(SocketError::WouldBlock) => {
                peer.mark_local_stream_window_blocked();
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        self.finish_stream_tx_batch(payload_len);
        self.local_tcp_direct_pending
            .fetch_add(payload_len as u64, Ordering::AcqRel);
        self.local_tcp_direct_events.fetch_add(1, Ordering::AcqRel);
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(profiling::Metric::TcpLocalDirectDeliveries, 1);
            profiling::observe(profiling::Metric::TcpLocalDirectBytes, payload_len as u64);
            profiling::observe(
                profiling::Metric::TcpLocalDirectCycles,
                profiling::read_counter().wrapping_sub(direct_start),
            );
        }
        self.request_local_tcp_direct_reconcile(false)?;
        Ok(Some(payload_len))
    }

    fn request_local_tcp_direct_reconcile(
        self: &Arc<Self>,
        force: bool,
    ) -> Result<(), SocketError> {
        if !force && !self.local_tcp_direct_reconcile_due() {
            return Ok(());
        }
        if !self.tx_notified.swap(true, Ordering::AcqRel) {
            match socket_runtime() {
                Ok(runtime) => {
                    runtime.notify_tx(Arc::clone(self), SocketTxCause::StreamLocalDirect)
                }
                Err(error) => {
                    self.tx_notified.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn local_tcp_direct_reconcile_due(&self) -> bool {
        self.local_tcp_direct_pending.load(Ordering::Acquire) >= TCP_LOCAL_DIRECT_RECONCILE_BYTES
            || self.local_tcp_direct_events.load(Ordering::Acquire)
                >= TCP_LOCAL_DIRECT_RECONCILE_EVENTS
    }

    pub(crate) fn take_local_tcp_direct_pending(&self) -> u32 {
        let mut observed = self.local_tcp_direct_pending.load(Ordering::Acquire);
        loop {
            let taken = observed.min(u64::from(u32::MAX));
            if taken == 0 {
                return 0;
            }
            match self.local_tcp_direct_pending.compare_exchange_weak(
                observed,
                observed - taken,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.local_tcp_direct_events.store(0, Ordering::Release);
                    return taken as u32;
                }
                Err(current) => observed = current,
            }
        }
    }

    pub(crate) fn restore_local_tcp_direct_pending(&self, bytes: u32) {
        if bytes != 0 {
            self.local_tcp_direct_pending
                .fetch_add(u64::from(bytes), Ordering::AcqRel);
        }
    }

    pub fn mark_local_stream_tx_prepared(&self) {
        self.local_tcp_tx_prepared.store(true, Ordering::Release);
    }

    fn prepare_stream_tx_storage(self: &Arc<Self>) {
        if self.kind != SocketKind::Stream || self.local_tcp_tx_prepared.load(Ordering::Acquire) {
            return;
        }
        if let Ok(runtime) = socket_runtime() {
            runtime.prepare_stream_tx(self);
        }
    }

    /// 连续流量交给 owner worker 聚合，短消息保留同步 local turn 的低延迟。
    pub fn local_stream_prefers_worker_batch(&self) -> bool {
        self.kind == SocketKind::Stream && self.stream_unsent_len() > SOCKET_CHUNK_BYTES
    }

    pub(crate) fn mark_local_stream_window_blocked(&self) {
        self.local_tcp_fast_path_active
            .store(true, Ordering::Release);
        self.local_tcp_window_blocked.store(true, Ordering::Release);
    }

    pub(crate) fn receive_window_scale_limit(&self) -> usize {
        if self.tcp_receive_buffer_explicit.load(Ordering::Acquire) {
            self.stream_rx.lock().bytes.limit
        } else {
            TCP_LOCAL_AUTOTUNE_LIMIT
        }
    }

    pub fn send_stream(
        self: &Arc<Self>,
        payload: &[u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
        more: bool,
    ) -> Result<usize, SocketError> {
        self.send_stream_from(
            payload.len(),
            nonblocking,
            deadline_ns,
            more,
            |offset, output| {
                output.copy_from_slice(&payload[offset..offset + output.len()]);
            },
        )
    }

    /// 从已固定、不会缺页的外部窗口批量复制 TCP 字节流。
    ///
    /// copy 可能在持有 socket TX 锁时调用，因此只能做有界内存复制，不能阻塞。
    pub fn send_stream_from(
        self: &Arc<Self>,
        payload_len: usize,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        more: bool,
        mut copy: impl FnMut(usize, &mut [u8]),
    ) -> Result<usize, SocketError> {
        // 用户页可能被内存管理层拆成多个短借用窗口。more 为 true 时只积累发送数据，
        // 最后一个窗口再统一发布；容量不足时提前发布，并把已接受的部分交还调用方。
        self.tcp_more.store(more, Ordering::Release);
        let result =
            self.send_stream_buffered_from(payload_len, nonblocking, deadline_ns, &mut copy);
        if more {
            return result;
        }
        let publish = self.publish_stream_pending();
        match (result, publish) {
            (Ok(accepted), Err(error)) if accepted == 0 => Err(error),
            (result, _) => result,
        }
    }

    /// send(MSG_OOB)：把单字节作为紧急数据发送（Linux 语义：只发送缓冲
    /// 的最后一个字节，返回值 1）。
    ///
    /// 紧急字节必须由引擎以带 URG 标志与紧急指针的段发出，因此先拆除本地
    /// 直连路由，避免字节绕过引擎被直接投递。
    pub fn send_urgent(
        self: &Arc<Self>,
        byte: u8,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        self.ensure_stack_attached()?;
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(SocketError::Closed);
        }
        if self.write_shutdown.load(Ordering::Acquire) {
            return Err(SocketError::WriteShutdown);
        }
        let owner = self.owner();
        if !matches!(owner, OwnerRef::Flow { .. }) || !self.stream_connected.load(Ordering::Acquire)
        {
            return if matches!(owner, OwnerRef::Closed { .. }) && self.peer_endpoint().is_some() {
                Err(SocketError::WriteShutdown)
            } else {
                Err(SocketError::NotConnected)
            };
        }
        self.clear_local_tcp_direct_route();
        let accepted = loop {
            if let Some(error) = self.backend_error() {
                break Err(error);
            }
            let copied = self
                .stream_tx
                .lock()
                .push_with(1, &mut |_, output| output[0] = byte);
            if copied != 0 {
                break Ok(1usize);
            }
            if nonblocking {
                break Err(SocketError::WouldBlock);
            }
            if let Err(error) = self.wait_write(deadline_ns) {
                break Err(error);
            }
        }?;
        self.urgent_tx_pending.store(true, Ordering::Release);
        self.tx_generation.fetch_add(1, Ordering::Release);
        let _ = self.publish_stream_pending();
        Ok(accepted)
    }

    fn send_stream_buffered_from(
        self: &Arc<Self>,
        payload_len: usize,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        copy: &mut impl FnMut(usize, &mut [u8]),
    ) -> Result<usize, SocketError> {
        self.ensure_stack_attached()?;
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(SocketError::Closed);
        }
        if self.write_shutdown.load(Ordering::Acquire) {
            return Err(SocketError::WriteShutdown);
        }
        let owner = self.owner();
        if !matches!(owner, OwnerRef::Flow { .. }) || !self.stream_connected.load(Ordering::Acquire)
        {
            return if matches!(owner, OwnerRef::Closed { .. }) && self.peer_endpoint().is_some() {
                Err(SocketError::WriteShutdown)
            } else {
                Err(SocketError::NotConnected)
            };
        }
        if payload_len == 0 {
            return Ok(0);
        }
        if let Some(written) = self.try_send_local_tcp_direct_from(payload_len, copy)? {
            return Ok(written);
        }
        // 本地流在第一次用户复制前安装共享 pool，避免先写 heap、发布时再迁移
        // 整个发送窗口。非本地流保持原有的 egress 安装路径。
        self.prepare_stream_tx_storage();
        let mut accepted = 0usize;
        loop {
            if let Some(error) = self.backend_error() {
                return if accepted == 0 {
                    Err(error)
                } else {
                    Ok(accepted)
                };
            }
            if self.closing.load(Ordering::Acquire) {
                return if accepted == 0 {
                    Err(SocketError::Closed)
                } else {
                    Ok(accepted)
                };
            }
            if self.write_shutdown.load(Ordering::Acquire) {
                return if accepted == 0 {
                    Err(SocketError::WriteShutdown)
                } else {
                    Ok(accepted)
                };
            }
            let owner = self.owner();
            if !matches!(owner, OwnerRef::Flow { .. })
                || !self.stream_connected.load(Ordering::Acquire)
            {
                let error =
                    if matches!(owner, OwnerRef::Closed { .. }) && self.peer_endpoint().is_some() {
                        SocketError::WriteShutdown
                    } else {
                        SocketError::NotConnected
                    };
                return if accepted == 0 {
                    Err(error)
                } else {
                    Ok(accepted)
                };
            }

            let copied = {
                let base = accepted;
                self.stream_tx
                    .lock()
                    .push_with(payload_len - accepted, &mut |offset, output| {
                        copy(base + offset, output)
                    })
            };
            if copied != 0 {
                accepted += copied;
                #[cfg(feature = "performance-profile")]
                if accepted != payload_len {
                    profiling::observe(profiling::Metric::TcpSendPartialCapacity, 1);
                }
                self.refresh_tx_readiness();
                self.tx_generation.fetch_add(1, Ordering::Release);
                if accepted == payload_len || nonblocking {
                    if accepted != payload_len {
                        let _ = self.publish_stream_pending();
                    }
                    return Ok(accepted);
                }
                match self.publish_stream_pending() {
                    Ok(()) => {}
                    Err(error) if accepted == 0 => return Err(error),
                    Err(_) => return Ok(accepted),
                }
                // 阻塞流写入在没有信号、超时或连接错误时继续等待容量，行为与
                // Linux TCP sendmsg 一致。协议调用预算耗尽后由 worker 继续推进，
                // 当前任务只在可写通知到达后重试，不额外发起同步 ELM 调用。
                continue;
            }
            #[cfg(feature = "performance-profile")]
            profiling::observe(
                if self.stream_tx.lock().exhausted_pool_key().is_some() {
                    profiling::Metric::TcpSendBlockedPool
                } else {
                    profiling::Metric::TcpSendBlockedBufferLimit
                },
                1,
            );
            match self.publish_stream_pending() {
                Ok(()) => {}
                Err(error) => return Err(error),
            }
            if copied == 0
                && let Some(pool_key) = self.stream_tx.lock().exhausted_pool_key()
            {
                queue_dma_tx_pool_waiter(self, pool_key);
            }
            self.refresh_tx_readiness();
            if nonblocking {
                return if accepted == 0 {
                    Err(SocketError::WouldBlock)
                } else {
                    Ok(accepted)
                };
            }
            if let Err(error) = self.wait_write(deadline_ns) {
                return if accepted == 0 {
                    Err(error)
                } else {
                    Ok(accepted)
                };
            }
        }
    }

    pub fn take_stream_tx(self: &Arc<Self>, max_len: usize) -> Option<TcpTxLease> {
        let lease = self.take_stream_tx_deferred(max_len)?;
        self.finish_stream_tx_batch(usize::from(lease.len));
        Some(lease)
    }

    pub(crate) fn take_stream_tx_deferred(self: &Arc<Self>, max_len: usize) -> Option<TcpTxLease> {
        let (start, len) = self.stream_tx.lock().take_unsent(max_len)?;
        Some(TcpTxLease {
            facade: Arc::clone(self),
            start,
            len: len as u16,
        })
    }

    fn take_stream_tx_direct_deferred(
        self: &Arc<Self>,
        max_len: usize,
    ) -> Option<(TcpTxLease, bool)> {
        let (start, len, flush) = {
            let mut tx = self.stream_tx.lock();
            let (start, len) = tx.take_unsent_without_inflight(max_len)?;
            let flush = tx.bytes.len().saturating_sub(tx.sent) == 0;
            (start, len, flush)
        };
        Some((
            TcpTxLease {
                facade: Arc::clone(self),
                start,
                len: len as u16,
            },
            flush,
        ))
    }

    pub(crate) fn finish_stream_tx_batch(&self, bytes: usize) {
        self.tcp_bytes_sent
            .fetch_add(bytes as u64, Ordering::Relaxed);
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::TcpBytesSent, bytes as u64);
        self.refresh_tx_readiness();
    }

    pub fn stream_unsent_len(&self) -> usize {
        let tx = self.stream_tx.lock();
        tx.bytes.len().saturating_sub(tx.sent)
    }

    #[cfg(test)]
    pub(crate) fn test_push_stream_tx(&self, payload: &[u8]) -> usize {
        self.stream_tx.lock().push(payload)
    }

    #[cfg(test)]
    pub(crate) fn test_udp_tx_lease(
        self: &Arc<Self>,
        payload: &[u8],
        destination: Endpoint,
    ) -> UdpTxLease {
        self.tx
            .lock()
            .as_mut()
            .expect("datagram facade 必须拥有 TX ring")
            .push(payload, destination, false, false)
            .unwrap();
        self.take_tx().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn test_udp_tx_used_bytes(&self) -> usize {
        self.tx
            .lock()
            .as_ref()
            .expect("datagram facade 必须拥有 TX ring")
            .used_bytes
    }

    #[cfg(test)]
    pub(crate) fn test_stream_tx_len(&self) -> usize {
        self.stream_tx.lock().bytes.len()
    }

    pub fn retransmit_stream(self: &Arc<Self>, start: u64, len: usize) -> Option<TcpTxLease> {
        let tx = self.stream_tx.lock();
        if len > u16::MAX as usize || !tx.contains(start, len) {
            return None;
        }
        drop(tx);
        Some(TcpTxLease {
            facade: Arc::clone(self),
            start,
            len: len as u16,
        })
    }

    pub fn acknowledge_stream(&self, len: usize) -> usize {
        let (consumed, pool_key) = {
            let mut tx = self.stream_tx.lock();
            let consumed = tx.acknowledge(len);
            (consumed, tx.pool_key())
        };
        if consumed != 0 {
            pool_key.inspect(|pool_key| {
                wake_dma_tx_pool_waiter(*pool_key, core::ptr::from_ref(self));
            });
        }
        if self.stream_is_writable() {
            if self.set_ready(Readiness::WRITABLE) {
                wake_one_socket_writer(&self.write_wait, self.id);
            }
        } else {
            self.clear_ready(Readiness::WRITABLE);
        }
        consumed
    }

    pub fn abort_stream_tx(&self) {
        self.stream_tx.lock().abort();
        self.refresh_tx_readiness();
    }

    pub fn push_stream_rx(&self, payload: &[u8]) -> Result<usize, SocketError> {
        self.push_stream_rx_compact(payload)
            .map(|commit| commit.len)
    }

    pub(crate) fn push_stream_rx_compact(
        &self,
        payload: &[u8],
    ) -> Result<StreamRxCommit, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Ok(StreamRxCommit {
                len: payload.len(),
                storage: StreamRxStorageKind::Discarded,
                low_water_fallback: false,
            });
        }
        self.local_tcp_fast_path_active
            .store(false, Ordering::Release);
        self.local_tcp_window_blocked
            .store(false, Ordering::Release);
        let (was_empty, copied) = {
            let mut rx = self.stream_rx.lock();
            if rx.bytes.available() < payload.len() {
                return Err(SocketError::WouldBlock);
            }
            let was_empty = rx.bytes.len == 0;
            let copied = rx.bytes.push_compact(payload)?;
            (was_empty, copied)
        };
        debug_assert_eq!(copied, payload.len());
        self.finish_stream_rx_commit(was_empty, copied, false, true);
        Ok(StreamRxCommit {
            len: copied,
            storage: StreamRxStorageKind::Compact,
            low_water_fallback: false,
        })
    }

    pub(crate) fn push_stream_rx_packet(
        &self,
        packet: &mut PacketChain,
        offset: usize,
        len: usize,
        pressure: RxPoolPressure,
    ) -> Result<StreamRxCommit, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Ok(StreamRxCommit {
                len,
                storage: StreamRxStorageKind::Discarded,
                low_water_fallback: false,
            });
        }
        self.local_tcp_fast_path_active
            .store(false, Ordering::Release);
        self.local_tcp_window_blocked
            .store(false, Ordering::Release);
        let should_pin =
            pressure == RxPoolPressure::Normal && len >= crate::tuning::TCP_RX_PIN_MIN_BYTES;
        let low_water_fallback =
            matches!(pressure, RxPoolPressure::Low | RxPoolPressure::Emergency)
                && len >= crate::tuning::TCP_RX_PIN_MIN_BYTES;
        let (was_empty, storage) = {
            let mut rx = self.stream_rx.lock();
            if rx.bytes.available() < len {
                return Err(SocketError::WouldBlock);
            }
            let was_empty = rx.bytes.len == 0;
            if should_pin
                && let Ok(shared) = packet.pin_range_shared(offset, len)
                && rx.bytes.push_shared_chain(shared).is_ok()
            {
                (was_empty, StreamRxStorageKind::PhysicalPinned)
            } else {
                let copied = rx.bytes.push_compact_with(len, |copied, output| {
                    packet
                        .copy_out(offset + copied, output)
                        .map_err(|_| SocketError::Buffer)
                })?;
                debug_assert_eq!(copied, len);
                (was_empty, StreamRxStorageKind::Compact)
            }
        };
        self.finish_stream_rx_commit(was_empty, len, false, true);
        Ok(StreamRxCommit {
            len,
            storage,
            low_water_fallback,
        })
    }

    pub(crate) fn push_stream_rx_lease(
        &self,
        lease: &TcpTxLease,
        offset: usize,
        len: usize,
        flush: bool,
    ) -> Result<StreamRxCommit, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Ok(StreamRxCommit {
                len,
                storage: StreamRxStorageKind::Discarded,
                low_water_fallback: false,
            });
        }
        self.local_tcp_fast_path_active
            .store(true, Ordering::Release);
        let (was_empty, storage, buffered, available) = {
            let mut rx = self.stream_rx.lock();
            if !self.tcp_receive_buffer_explicit.load(Ordering::Acquire) {
                rx.bytes.enable_local_autotune();
            }
            if rx.bytes.available() < len {
                self.local_tcp_window_blocked.store(true, Ordering::Release);
                return Err(SocketError::WouldBlock);
            }
            let was_empty = rx.bytes.len == 0;
            let shared = lease
                .packet_chain()?
                .and_then(|chain| chain.pin_shared_range(offset, len).ok());
            if let Some(shared) = shared {
                rx.bytes.push_shared_chain(shared)?;
                (
                    was_empty,
                    StreamRxStorageKind::LoopbackShared,
                    rx.bytes.len,
                    rx.bytes.available(),
                )
            } else {
                let copied = rx.bytes.push_compact_with(len, |copied, output| {
                    lease.copy_range(offset + copied, output)
                })?;
                debug_assert_eq!(copied, len);
                (
                    was_empty,
                    StreamRxStorageKind::Compact,
                    rx.bytes.len,
                    rx.bytes.available(),
                )
            }
        };
        // flush 标记发送批次尾部；在尾段到达前只发布 READABLE，不跨任务
        // 交接，避免一次性 read 在本地 TCP 请求分段中途过早返回。
        let handoff_ready = flush
            || buffered >= TCP_LOCAL_SHARED_READ_BATCH_BYTES
            || available < TCP_LOCAL_PRESSURE_BYTES;
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(profiling::Metric::TcpLocalRxBufferedBytes, buffered as u64);
            profiling::observe(
                profiling::Metric::TcpLocalRxAvailableBytes,
                available as u64,
            );
            if flush {
                profiling::observe(profiling::Metric::TcpLocalHandoffFlush, 1);
            }
            if buffered >= TCP_LOCAL_SHARED_READ_BATCH_BYTES {
                profiling::observe(profiling::Metric::TcpLocalHandoffBatch, 1);
            }
            if available < TCP_LOCAL_PRESSURE_BYTES {
                profiling::observe(profiling::Metric::TcpLocalHandoffPressure, 1);
            }
        }
        self.finish_stream_rx_commit(was_empty, len, true, handoff_ready);
        Ok(StreamRxCommit {
            len,
            storage,
            low_water_fallback: false,
        })
    }

    fn push_stream_rx_local_from(
        &self,
        len: usize,
        copy: &mut impl FnMut(usize, &mut [u8]),
    ) -> Result<StreamRxCommit, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Ok(StreamRxCommit {
                len,
                storage: StreamRxStorageKind::Discarded,
                low_water_fallback: false,
            });
        }
        self.local_tcp_fast_path_active
            .store(true, Ordering::Release);
        let (was_empty, buffered, available) = {
            let mut rx = self.stream_rx.lock();
            if !self.tcp_receive_buffer_explicit.load(Ordering::Acquire) {
                rx.bytes.enable_local_autotune();
            }
            if rx.bytes.available() < len {
                self.local_tcp_window_blocked.store(true, Ordering::Release);
                return Err(SocketError::WouldBlock);
            }
            let was_empty = rx.bytes.len == 0;
            let copied = rx.bytes.push_local_with(len, copy);
            debug_assert_eq!(copied, len);
            (was_empty, rx.bytes.len, rx.bytes.available())
        };
        let read_batch = local_tcp_read_batch_bytes();
        let handoff_ready = (was_empty && len <= TCP_LOCAL_IMMEDIATE_HANDOFF_BYTES)
            || buffered >= read_batch
            || available < TCP_LOCAL_PRESSURE_BYTES;
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(profiling::Metric::TcpLocalRxBufferedBytes, buffered as u64);
            profiling::observe(
                profiling::Metric::TcpLocalRxAvailableBytes,
                available as u64,
            );
            if buffered >= read_batch {
                profiling::observe(profiling::Metric::TcpLocalHandoffBatch, 1);
            }
            if available < TCP_LOCAL_PRESSURE_BYTES {
                profiling::observe(profiling::Metric::TcpLocalHandoffPressure, 1);
            }
        }
        self.finish_stream_rx_commit(was_empty, len, true, handoff_ready);
        Ok(StreamRxCommit {
            len,
            storage: StreamRxStorageKind::Compact,
            low_water_fallback: false,
        })
    }

    fn finish_stream_rx_commit(
        &self,
        was_empty: bool,
        len: usize,
        local: bool,
        handoff_ready: bool,
    ) {
        self.tcp_bytes_received
            .fetch_add(len as u64, Ordering::Relaxed);
        self.stream_pushed.fetch_add(len as u64, Ordering::Relaxed);
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::TcpBytesReceived, len as u64);
        if was_empty && len != 0 {
            self.set_ready(Readiness::READABLE);
        }
        if local {
            if !handoff_ready {
                return;
            }
            if let Some(target) = wake_one_local_socket_reader(&self.read_wait, self.id, true) {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::TcpLocalConsumerHandoffs, 1);
                sched::request_post_syscall_handoff_to(target);
            } else {
                self.request_local_tcp_consumer_handoff();
            }
        } else if was_empty && len != 0 {
            wake_one_socket_reader(&self.read_wait, self.id);
        }
    }

    pub fn publish_stream_eof(&self) {
        self.clear_local_tcp_direct_route();
        self.local_tcp_fast_path_active
            .store(false, Ordering::Release);
        self.local_tcp_window_blocked
            .store(false, Ordering::Release);
        self.stream_rx.lock().eof = true;
        self.set_ready(Readiness::READABLE | Readiness::READ_HANGUP);
        self.read_wait.wake_all();
        self.state_wait.wake_all();
    }

    // ── 紧急数据（MSG_OOB / SO_OOBINLINE / SIGURG）──────────────────────────

    pub fn set_oob_inline(&self, inline: bool) {
        self.oob_inline.store(inline, Ordering::Release);
    }

    pub fn oob_inline(&self) -> bool {
        self.oob_inline.load(Ordering::Acquire)
    }

    /// F_SETOWN 注册的紧急数据信号接收者（owner_type, owner_pid）。
    pub fn set_urgent_owner(&self, owner_type: i32, owner_pid: i32) {
        *self.urgent_owner.lock() = Some((owner_type, owner_pid));
    }

    /// 是否存在尚未读取的紧急数据（recv(MSG_OOB) 可立即返回）。
    pub fn oob_pending(&self) -> bool {
        self.oob_pending.load(Ordering::Acquire)
    }

    /// SIOCATMARK：读指针是否正位于 OOB 标记处。
    pub fn at_oob_mark(&self) -> bool {
        let Some(offset) = *self.oob_seq.lock() else {
            return false;
        };
        self.oob_pending.load(Ordering::Acquire)
            && self.stream_consumed.load(Ordering::Acquire) == offset
    }

    /// 引擎在流建立时记录首个流字节的绝对序列号（仅记录一次）。
    pub(crate) fn set_stream_base_seq(&self, seq: u32) {
        let mut base = self.stream_base_seq.lock();
        if base.is_none() {
            *base = Some(seq);
        }
    }

    /// URG 标记到达：记录紧急字节位置、唤醒读取者并触发 SIGURG。
    ///
    /// `byte_abs_seq` 是紧急字节的绝对序列号（非内联 = seq + ptr - 1，
    /// 内联 = seq + ptr，对应 Linux tcp_check_urg 的 ptr 折算）。字节本身由
    /// 递送路径经 [`SocketFacade::stash_oob_byte`] 填充；若字节已先进入流
    /// （迟到的标记），这里直接从流中取回副本并让普通读跳过它。
    pub(crate) fn mark_urgent(&self, byte_abs_seq: u32) {
        let Some(base) = *self.stream_base_seq.lock() else {
            return;
        };
        let offset = u64::from(byte_abs_seq.wrapping_sub(base));
        let pushed = self.stream_pushed.load(Ordering::Acquire);
        let consumed = self.stream_consumed.load(Ordering::Acquire);
        if offset < consumed {
            // 紧急字节已被用户读取：忽略迟到的标记（对应 Linux after(copied_seq, ptr)）。
            self.oob_pending.store(false, Ordering::Release);
            *self.oob_byte.lock() = None;
            return;
        }
        *self.oob_seq.lock() = Some(offset);
        if self.oob_inline() {
            // 内联模式：字节留在流中，oob_byte 只作 MSG_OOB 读取的镜像。
            self.oob_pending.store(true, Ordering::Release);
        } else if offset < pushed {
            // 非内联且字节已进入流（标记迟到）：取回副本，普通读跳过该字节。
            let byte = self.peek_stream_byte(offset);
            *self.oob_byte.lock() = byte;
            self.oob_skip.store(true, Ordering::Release);
            self.oob_pending.store(byte.is_some(), Ordering::Release);
        } else {
            // 字节尚未递送：递送路径负责剔除并填充副本。
            *self.oob_byte.lock() = None;
            self.oob_pending.store(false, Ordering::Release);
        }
        self.read_wait.wake_one_default();
        self.notify_urgent_signal();
    }

    /// 递送路径填充紧急字节：非内联时该字节已从流中剔除；内联时字节留在流中，
    /// 此处只保存镜像。
    pub(crate) fn stash_oob_byte(&self, byte: u8, byte_abs_seq: u32) {
        let Some(base) = *self.stream_base_seq.lock() else {
            return;
        };
        let offset = u64::from(byte_abs_seq.wrapping_sub(base));
        *self.oob_seq.lock() = Some(offset);
        *self.oob_byte.lock() = Some(byte);
        self.oob_skip.store(false, Ordering::Release);
        self.oob_pending.store(true, Ordering::Release);
        self.read_wait.wake_one_default();
        self.notify_urgent_signal();
    }

    /// 从流中按流偏移读取一个字节（迟到标记恢复紧急字节用）。
    fn peek_stream_byte(&self, offset: u64) -> Option<u8> {
        let consumed = self.stream_consumed.load(Ordering::Acquire);
        if offset < consumed {
            return None;
        }
        let rx = self.stream_rx.lock();
        let mut byte = [0u8; 1];
        rx.bytes
            .copy_range_with((offset - consumed) as usize, 1, &mut |_, input| {
                byte[..input.len()].copy_from_slice(input);
            })
            .then_some(byte[0])
    }

    /// 向 F_SETOWN 注册的接收者投递 SIGURG（尽力而为，对应 kill_fasync）。
    fn notify_urgent_signal(&self) {
        let Some((owner_type, owner_pid)) = *self.urgent_owner.lock() else {
            return;
        };
        // 宿主测试等尚无调度运行时的环境不投递信号。
        if sched::try_current_task_ref().is_none() {
            return;
        }
        // kernel fs.rs 的 F_OWNER_PGRP = 2：进程组接收者按 kill(-pgid) 投递。
        let pid = if owner_type == 2 {
            -owner_pid
        } else {
            owner_pid
        };
        let _ = sched::operation::kill(pid, Some(sched::SignalNumber::SIGURG));
    }

    /// 取走/窥视紧急字节（recv(MSG_OOB) 内部）。
    ///
    /// 非内联：返回并移除缓存（peek 不移除）；内联：返回镜像副本，字节始终
    /// 留在流中，仅当流消费越过其位置后失效。
    fn take_oob_byte(&self, peek: bool) -> Option<u8> {
        if !self.oob_pending.load(Ordering::Acquire) {
            return None;
        }
        if self.oob_inline() {
            let Some(offset) = *self.oob_seq.lock() else {
                return None;
            };
            if self.stream_consumed.load(Ordering::Acquire) >= offset {
                return None;
            }
            return *self.oob_byte.lock();
        }
        if peek {
            return *self.oob_byte.lock();
        }
        let byte = self.oob_byte.lock().take();
        self.oob_pending.store(false, Ordering::Release);
        self.oob_skip.store(false, Ordering::Release);
        byte
    }

    /// recv(MSG_OOB)：读取紧急字节（单字节）。
    ///
    /// Linux 语义：非内联且紧急数据未到达时阻塞等待；内联模式或非阻塞且
    /// 无可用紧急数据时返回 EINVAL（映射为 InvalidState）。
    pub fn recv_oob(
        self: &Arc<Self>,
        peek: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<u8, SocketError> {
        self.ensure_stack_attached()?;
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        loop {
            if let Some(error) = self.backend_error() {
                return Err(error);
            }
            if let Some(byte) = self.take_oob_byte(peek) {
                return Ok(byte);
            }
            let pending = self.oob_pending.load(Ordering::Acquire);
            let stashed = self.oob_byte.lock().is_some();
            if self.oob_inline() {
                // Linux：内联模式没有可用紧急数据时立即 EINVAL，不阻塞等待；
                // 仅当标记已到而字节尚未落位（同一 worker turn 内马上补齐）时等待。
                if !pending || stashed || nonblocking {
                    return Err(SocketError::InvalidState);
                }
            } else if nonblocking {
                return Err(SocketError::InvalidState);
            }
            if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
                return Err(SocketError::Closed);
            }
            // 阻塞等待紧急数据到达（mark_urgent / stash_oob_byte 唤醒 read_wait）。
            self.wait_io_until(&self.read_wait, deadline_ns, |facade| {
                let (current, _) = facade.readiness();
                socket_wait_terminal(current) || facade.oob_pending()
            })?;
        }
    }

    /// 发送侧待发紧急字节的标记（drain_send 据此设置 URG + 紧急指针）。
    pub(crate) fn urgent_tx_pending(&self) -> bool {
        self.urgent_tx_pending.load(Ordering::Acquire)
    }

    pub(crate) fn clear_urgent_tx_pending(&self) {
        self.urgent_tx_pending.store(false, Ordering::Release);
    }
    pub fn recv_stream(
        self: &Arc<Self>,
        output: &mut [u8],
        peek: bool,
        wait_all: bool,
        defer_window_update: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        let output_len = output.len();
        self.recv_stream_to(
            output_len,
            peek,
            wait_all,
            defer_window_update,
            nonblocking,
            deadline_ns,
            |offset, input| {
                output[offset..offset + input.len()].copy_from_slice(input);
            },
        )
    }

    /// 把 TCP 字节流批量复制到已固定、不会缺页的外部窗口。
    ///
    /// copy 在 RX 锁内执行，只能进行有界内存复制，不能阻塞。
    pub fn recv_stream_to(
        self: &Arc<Self>,
        output_len: usize,
        peek: bool,
        wait_all: bool,
        defer_window_update: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
        mut copy: impl FnMut(usize, &[u8]),
    ) -> Result<usize, SocketError> {
        self.ensure_stack_attached()?;
        let mut total = 0usize;
        loop {
            if let Some(error) = self.backend_error() {
                return if total == 0 { Err(error) } else { Ok(total) };
            }
            let (copied, eof) = {
                let mut rx = self.stream_rx.lock();
                let want = (output_len - total).min(rx.bytes.len);
                // 非内联模式下紧急字节已先于 URG 标记进入流（迟到标记）：普通读
                // 必须跳过该字节，语义等同 Linux tcp_recvmsg 对 urg_seq 的剔除。
                let consumed_before = self.stream_consumed.load(Ordering::Acquire);
                let skip = self.oob_skip.load(Ordering::Acquire)
                    && !self.oob_inline()
                    && self.oob_seq.lock().is_some_and(|offset| {
                        offset >= consumed_before && ((offset - consumed_before) as usize) < want
                    });
                let (copied, consumed) = if skip {
                    let offset = self.oob_seq.lock().expect("skip 前置检查已确认位置") as usize;
                    // 环内偏移相对未消费区起点。
                    let relative = offset - consumed_before as usize;
                    if relative != 0
                        && !rx.bytes.copy_range_with(0, relative, &mut |at, input| {
                            copy(total + at, input);
                        })
                    {
                        return Err(SocketError::Buffer);
                    }
                    let tail = want - relative - 1;
                    if tail != 0
                        && !rx
                            .bytes
                            .copy_range_with(relative + 1, tail, &mut |at, input| {
                                copy(total + at - relative - 1, input);
                            })
                    {
                        return Err(SocketError::Buffer);
                    }
                    // 读指针越过紧急位置：紧急数据失效（Linux after(copied_seq, urg_seq)）。
                    self.oob_pending.store(false, Ordering::Release);
                    self.oob_skip.store(false, Ordering::Release);
                    (relative + tail, want)
                } else {
                    if want != 0
                        && !rx.bytes.copy_range_with(0, want, &mut |at, input| {
                            copy(total + at, input);
                        })
                    {
                        return Err(SocketError::Buffer);
                    }
                    (want, want)
                };
                if copied != 0 && !peek {
                    rx.bytes.consume(consumed);
                    self.stream_consumed
                        .fetch_add(consumed as u64, Ordering::Relaxed);
                }
                (copied, rx.eof)
            };
            total += copied;
            if copied != 0 && !peek {
                self.remember_local_tcp_consumer();
                let local_fast_path = self.local_tcp_fast_path_active.load(Ordering::Acquire)
                    && local_transport_fast_path_eligible();
                let blocked = self.local_tcp_window_blocked.swap(false, Ordering::AcqRel);
                if !local_fast_path || blocked {
                    self.receive_window_update.store(true, Ordering::Release);
                    if !defer_window_update {
                        self.notify_tcp_state_change();
                    }
                }
            }
            self.refresh_rx_readiness();
            if total != 0 && (!wait_all || total == output_len || peek || eof) {
                return Ok(total);
            }
            if total == 0
                && let Some(error) = self.take_pending_error()
            {
                self.refresh_rx_readiness();
                return Err(error);
            }
            if eof || self.read_shutdown.load(Ordering::Acquire) {
                return Ok(total);
            }
            if nonblocking {
                return if total == 0 {
                    Err(SocketError::WouldBlock)
                } else {
                    Ok(total)
                };
            }
            self.wait_read(deadline_ns)?;
        }
    }

    pub fn push_rx(
        &self,
        datagram: crate::transport::UdpDatagram,
    ) -> Result<(), crate::transport::UdpDatagram> {
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Err(datagram);
        }
        let was_empty = {
            let mut rx = self.rx.lock();
            let rx = rx.as_mut().expect("UDP facade 必须拥有 RX ring");
            let was_empty = rx.is_empty();
            if let Err(datagram) = rx.push(datagram) {
                self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                return Err(datagram);
            }
            was_empty
        };
        if was_empty {
            self.set_ready(Readiness::READABLE);
            wake_one_socket_reader(&self.read_wait, self.id);
        }
        Ok(())
    }

    pub(crate) fn push_local_udp(
        &self,
        payload: &UdpTxLease,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
    ) -> Result<(), SocketError> {
        if let Ok(Some(shared)) = payload.packet_chain() {
            return self.push_local_udp_shared(
                shared,
                source,
                destination,
                hop_limit,
                traffic_class,
                ingress_interface,
                rx_timestamp_ns,
            );
        }
        match self.push_local_udp_from(
            usize::from(payload.len),
            source,
            destination,
            hop_limit,
            traffic_class,
            ingress_interface,
            rx_timestamp_ns,
            &mut |offset, output| payload.copy_range(offset, output),
        ) {
            Ok(()) => Ok(()),
            Err(DatagramCopyError::Socket(error)) => Err(error),
            Err(DatagramCopyError::Copy(error)) => Err(error),
        }
    }

    fn push_local_udp_shared(
        &self,
        payload: PacketChain,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
    ) -> Result<(), SocketError> {
        if self.kind != SocketKind::Datagram {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Err(SocketError::Closed);
        }
        let payload_len = payload.total_len();
        let (was_empty, handoff_ready) = {
            let mut rx = self.rx.lock();
            let rx = rx.as_mut().expect("UDP facade 必须拥有 RX ring");
            let was_empty = rx.is_empty();
            if let Err(error) = rx.push_local_shared(
                payload,
                source,
                destination,
                hop_limit,
                traffic_class,
                ingress_interface,
                rx_timestamp_ns,
            ) {
                if error == SocketError::WouldBlock {
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                }
                return Err(error);
            }
            #[cfg(feature = "performance-profile")]
            profiling::observe(profiling::Metric::UdpLocalSharedReferences, 1);
            let handoff_ready = payload_len <= SOCKET_CHUNK_BYTES
                || rx.len() >= LOCAL_DATAGRAM_BATCH_LIMIT
                || !rx.can_push_local_len(payload_len);
            (was_empty, handoff_ready)
        };
        self.finish_local_udp_push(was_empty, handoff_ready);
        Ok(())
    }

    fn push_local_udp_from<E>(
        &self,
        payload_len: usize,
        source: Endpoint,
        destination: Endpoint,
        hop_limit: u8,
        traffic_class: u8,
        ingress_interface: InterfaceId,
        rx_timestamp_ns: u64,
        copy: &mut impl FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), DatagramCopyError<E>> {
        if self.kind != SocketKind::Datagram {
            return Err(DatagramCopyError::Socket(SocketError::InvalidState));
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Err(DatagramCopyError::Socket(SocketError::Closed));
        }
        let (was_empty, handoff_ready) = {
            let mut rx = self.rx.lock();
            let rx = rx.as_mut().expect("UDP facade 必须拥有 RX ring");
            let was_empty = rx.is_empty();
            if let Err(error) = rx.push_local_from(
                payload_len,
                source,
                destination,
                hop_limit,
                traffic_class,
                ingress_interface,
                rx_timestamp_ns,
                copy,
            ) {
                if matches!(error, DatagramCopyError::Socket(SocketError::WouldBlock)) {
                    self.rx_dropped.fetch_add(1, Ordering::Relaxed);
                }
                return Err(error);
            }
            let handoff_ready = payload_len <= SOCKET_CHUNK_BYTES
                || rx.len() >= LOCAL_DATAGRAM_BATCH_LIMIT
                || !rx.can_push_local_len(payload_len);
            (was_empty, handoff_ready)
        };
        self.finish_local_udp_push(was_empty, handoff_ready);
        Ok(())
    }

    fn finish_local_udp_push(&self, was_empty: bool, handoff_ready: bool) {
        #[cfg(feature = "performance-profile")]
        let publish_start = was_empty.then(profiling::read_counter);
        let deferred_target = if was_empty {
            self.set_ready(Readiness::READABLE);
            // 大数据报先保留精确消费者身份，达到 RX 批次边界时再请求调度。
            // 跨 CPU 目标会在下面立即通知，只有同 CPU 才允许继续填充有界批次。
            wake_one_local_socket_reader(&self.read_wait, self.id, handoff_ready)
        } else {
            None
        };
        let in_syscall = sched::is_ready()
            && sched::current_task_fast().execution_scope_kind()
                == Some(sched::ExecutionScopeKind::Syscall);
        if handoff_ready && in_syscall {
            if let Some(target) = deferred_target.or_else(|| self.local_read_handoff.lock().clone())
            {
                #[cfg(feature = "performance-profile")]
                profiling::observe(profiling::Metric::UdpLocalConsumerHandoffs, 1);
                sched::request_post_syscall_handoff_to(target);
            }
        } else if in_syscall && let Some(target) = deferred_target {
            if target.preferred_cpu() != sched::current_cpu_id() {
                // 远端任务已经具备运行资格，立即通知其 CPU，不能在 socket 中保留
                // 可能先于当前 syscall 返回就已经运行过的任务身份。
                sched::request_post_syscall_handoff_to(target);
            } else {
                // 大报文可以先唤醒、后续 datagram 才达到批次边界；仅同 CPU 情况
                // 需要跨调用保留精确任务身份，小报文即时交接不进入 socket 槽锁。
                *self.local_read_handoff.lock() = Some(target);
            }
        }
        #[cfg(feature = "performance-profile")]
        if let Some(start) = publish_start {
            profiling::observe(
                profiling::Metric::UdpLocalSendPublishCycles,
                profiling::read_counter().wrapping_sub(start),
            );
        }
    }

    fn request_local_udp_consumer_handoff(&self) {
        if !sched::is_ready()
            || sched::current_task_fast().execution_scope_kind()
                != Some(sched::ExecutionScopeKind::Syscall)
        {
            return;
        }
        let Some(target) = self.local_read_handoff.lock().clone() else {
            return;
        };
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::UdpLocalConsumerHandoffs, 1);
        sched::request_post_syscall_handoff_to(target);
    }

    fn remember_local_udp_consumer(&self) {
        if !sched::is_ready()
            || sched::current_task_fast().execution_scope_kind()
                != Some(sched::ExecutionScopeKind::Syscall)
        {
            return;
        }
        let Some(target) =
            sched::current_task_handoff_target(sched::HandoffReason::SocketReadContinuation)
        else {
            return;
        };
        *self.local_read_handoff.lock() = Some(target);
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::UdpLocalConsumerTargets, 1);
    }

    fn request_local_tcp_consumer_handoff(&self) {
        if !sched::is_ready()
            || sched::current_task_fast().execution_scope_kind()
                != Some(sched::ExecutionScopeKind::Syscall)
        {
            return;
        }
        let Some(target) = self.local_read_handoff.lock().clone() else {
            return;
        };
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::TcpLocalConsumerHandoffs, 1);
        sched::request_post_syscall_handoff_to(target);
    }

    fn remember_local_tcp_consumer(&self) {
        if !sched::is_ready()
            || sched::current_task_fast().execution_scope_kind()
                != Some(sched::ExecutionScopeKind::Syscall)
        {
            return;
        }
        let Some(target) =
            sched::current_task_handoff_target(sched::HandoffReason::SocketReadContinuation)
        else {
            return;
        };
        *self.local_read_handoff.lock() = Some(target);
        #[cfg(feature = "performance-profile")]
        profiling::observe(profiling::Metric::TcpLocalConsumerTargets, 1);
    }

    pub fn recv(
        &self,
        output: &mut [u8],
        peek: bool,
        report_original_len: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<UdpReceive, SocketError> {
        self.ensure_stack_attached()?;
        loop {
            self.ensure_stack_attached()?;
            if let Some(result) = self.try_recv(output, peek, report_original_len)? {
                return Ok(result);
            }
            if self.read_shutdown.load(Ordering::Acquire) {
                return Ok(UdpReceive {
                    len: 0,
                    original_len: 0,
                    source: Endpoint {
                        addr: match self.family {
                            AddressFamily::Ipv4 => crate::IpAddr::V4(crate::Ipv4Addr::UNSPECIFIED),
                            AddressFamily::Ipv6 => crate::IpAddr::V6(crate::Ipv6Addr::UNSPECIFIED),
                        },
                        port: 0,
                    },
                    destination: self.local_endpoint().unwrap_or(Endpoint {
                        addr: match self.family {
                            AddressFamily::Ipv4 => crate::IpAddr::V4(crate::Ipv4Addr::UNSPECIFIED),
                            AddressFamily::Ipv6 => crate::IpAddr::V6(crate::Ipv6Addr::UNSPECIFIED),
                        },
                        port: 0,
                    }),
                    ingress_interface: InterfaceId(0),
                    hop_limit: 0,
                    traffic_class: 0,
                    rx_timestamp_ns: 0,
                    truncated: false,
                });
            }
            if nonblocking {
                return Err(SocketError::WouldBlock);
            }
            self.wait_read(deadline_ns)?;
        }
    }

    /// 等待至少一个 UDP 数据报可读，但不复制或消费它。
    ///
    /// 调用方可在本函数返回后 fault-in 用户目标页，再用
    /// [`Self::recv_local_datagram_from`] 完成不会缺页的直接复制。共享 fd 的其它
    /// reader 可能在两步之间消费数据，因此后一步仍必须允许返回 `None`。
    pub fn wait_datagram_readable(
        &self,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<Option<usize>, SocketError> {
        self.ensure_stack_attached()?;
        loop {
            self.ensure_stack_attached()?;
            if let Some(len) = self
                .rx
                .lock()
                .as_ref()
                .expect("UDP facade 必须拥有 RX ring")
                .front_len()
            {
                return Ok(Some(len));
            }
            if self.read_shutdown.load(Ordering::Acquire) {
                return Ok(None);
            }
            if nonblocking {
                return Err(SocketError::WouldBlock);
            }
            self.wait_read(deadline_ns)?;
        }
    }

    /// 将队首本地 UDP 数据报直接复制到已经预校验的外部目标。
    ///
    /// 复制失败时数据报保持在队首；非本地 packet 或共享 fd 竞态导致队列为空时
    /// 返回 `Ok(None)`，调用方必须回到通用接收路径。成功后才消费完整数据报。
    pub fn recv_local_datagram_from<E>(
        &self,
        output_len: usize,
        copy_capacity: usize,
        report_original_len: bool,
        mut copy: impl FnMut(usize, &[u8]) -> Result<(), E>,
    ) -> Result<Option<UdpReceive>, DatagramCopyError<E>> {
        self.ensure_stack_attached()
            .map_err(DatagramCopyError::Socket)?;
        #[cfg(feature = "performance-profile")]
        let receive_start = profiling::read_counter();
        let mut rx_guard = self.rx.lock();
        let rx = rx_guard.as_mut().expect("UDP facade 必须拥有 RX ring");
        let Some((
            original_len,
            source,
            destination,
            ingress_interface,
            hop_limit,
            traffic_class,
            rx_timestamp_ns,
        )) = rx.front_metadata()
        else {
            return Ok(None);
        };
        if !rx.front_local() {
            return Ok(None);
        }
        // wait 与消费之间允许共享 fd 的其它 reader 先取走队首。若新队首需要
        // 复制的字节超过此前固定的用户页容量，保持报文不动并回到通用路径。
        if output_len.min(original_len) > copy_capacity {
            return Ok(None);
        }
        #[cfg(feature = "performance-profile")]
        let copy_start = profiling::read_counter();
        let copied = rx
            .copy_local_front_to(output_len, &mut copy)?
            .expect("本地 payload 已在复制前确认");
        #[cfg(feature = "performance-profile")]
        {
            let copy_end = profiling::read_counter();
            record_payload_copy(copy_start, copied);
            profiling::observe(
                profiling::Metric::UdpLocalReceiveCopyCycles,
                copy_end.wrapping_sub(copy_start),
            );
        }
        let result = UdpReceive {
            len: if report_original_len {
                original_len
            } else {
                copied
            },
            original_len,
            source,
            destination,
            ingress_interface,
            hop_limit,
            traffic_class,
            rx_timestamp_ns,
            truncated: copied < original_len,
        };
        #[cfg(feature = "performance-profile")]
        let pop_start = profiling::read_counter();
        rx.pop().expect("已复制的数据报必须仍在队首");
        let empty = rx.is_empty();
        drop(rx_guard);
        self.remember_local_udp_consumer();
        #[cfg(feature = "performance-profile")]
        let readiness_start = {
            let now = profiling::read_counter();
            profiling::observe(
                profiling::Metric::UdpLocalReceivePopCycles,
                now.wrapping_sub(pop_start),
            );
            now
        };
        if empty {
            self.refresh_rx_readiness();
        } else {
            wake_one_socket_waiter(&self.read_wait);
        }
        #[cfg(feature = "performance-profile")]
        {
            let receive_end = profiling::read_counter();
            profiling::observe(
                profiling::Metric::UdpLocalReceiveReadinessCycles,
                receive_end.wrapping_sub(readiness_start),
            );
            profiling::observe(profiling::Metric::UdpLocalDirectReceives, 1);
            profiling::observe(profiling::Metric::UdpLocalDirectReceiveBytes, copied as u64);
            profiling::observe(
                profiling::Metric::UdpLocalDirectReceiveCycles,
                receive_end.wrapping_sub(receive_start),
            );
        }
        Ok(Some(result))
    }

    fn try_recv(
        &self,
        output: &mut [u8],
        peek: bool,
        report_original_len: bool,
    ) -> Result<Option<UdpReceive>, SocketError> {
        let mut rx_guard = self.rx.lock();
        let rx = rx_guard.as_mut().expect("UDP facade 必须拥有 RX ring");
        let Some((
            original_len,
            source,
            destination,
            ingress_interface,
            hop_limit,
            traffic_class,
            rx_timestamp_ns,
        )) = rx.front_metadata()
        else {
            return Ok(None);
        };
        let copied = original_len.min(output.len());
        rx.copy_front(&mut output[..copied])?;
        let result = UdpReceive {
            len: if report_original_len {
                original_len
            } else {
                copied
            },
            original_len,
            source,
            destination,
            ingress_interface,
            hop_limit,
            traffic_class,
            rx_timestamp_ns,
            truncated: copied < original_len,
        };
        if !peek {
            rx.pop().unwrap();
            let empty = rx.is_empty();
            drop(rx_guard);
            self.local_read_handoff.lock().take();
            if empty {
                self.refresh_rx_readiness();
            } else {
                wake_one_socket_waiter(&self.read_wait);
            }
        }
        Ok(Some(result))
    }

    pub fn shutdown(self: &Arc<Self>, read: bool, write: bool) -> Result<(), SocketError> {
        self.ensure_stack_attached()?;
        if !read && !write {
            return Err(SocketError::InvalidState);
        }
        if read {
            self.read_shutdown.store(true, Ordering::Release);
            self.local_read_handoff.lock().take();
            self.local_tcp_fast_path_active
                .store(false, Ordering::Release);
            self.local_tcp_window_blocked
                .store(false, Ordering::Release);
            match self.kind {
                SocketKind::Datagram | SocketKind::Raw => {
                    let mut rx = self.rx.lock();
                    let rx = rx.as_mut().expect("UDP facade 必须拥有 RX ring");
                    while rx.pop().is_some() {}
                }
                SocketKind::Stream => {
                    self.stream_rx.lock().bytes.clear();
                    // 读侧关闭后紧急数据一并失效。
                    self.oob_pending.store(false, Ordering::Release);
                    self.oob_skip.store(false, Ordering::Release);
                    *self.oob_byte.lock() = None;
                    *self.oob_seq.lock() = None;
                }
            }
            self.set_ready(Readiness::READ_HANGUP);
        }
        if write {
            self.write_shutdown.store(true, Ordering::Release);
            self.clear_local_tcp_direct_route();
            self.clear_local_tcp_bulk_active();
            self.clear_ready(Readiness::WRITABLE);
            if self.kind == SocketKind::Stream
                && let Ok(runtime) = socket_runtime()
                && !self.lifecycle_notified.swap(true, Ordering::AcqRel)
            {
                runtime.notify_lifecycle(Arc::clone(self));
            }
        }
        self.state_wait.wake_all();
        self.read_wait.wake_all();
        self.write_wait.wake_all();
        Ok(())
    }

    pub fn close(self: &Arc<Self>) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        self.deactivate_packet_observer();
        self.clear_local_datagram_route();
        self.clear_local_tcp_direct_route();
        self.local_read_handoff.lock().take();
        self.local_tcp_tx_prepared.store(false, Ordering::Release);
        self.clear_local_tcp_bulk_active();
        self.local_tcp_fast_path_active
            .store(false, Ordering::Release);
        self.local_tcp_window_blocked
            .store(false, Ordering::Release);
        if self.kind != SocketKind::Stream {
            invalidate_local_datagram_routes();
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.set_ready(Readiness::HANGUP | Readiness::READ_HANGUP);
        self.read_wait.wake_all();
        self.write_wait.wake_all();
        self.accept_wait.wake_all();
        self.state_wait.wake_all();
        if self.kind == SocketKind::Stream {
            if let Some(group) = self.listen_group.lock().take() {
                for child in group.close() {
                    child.close();
                }
            }
        }
        if self.stack_detached.load(Ordering::Acquire) {
            *self.owner.lock() = OwnerRef::Closed { generation };
            return;
        }
        match socket_runtime() {
            Ok(runtime) => {
                if !self.lifecycle_notified.swap(true, Ordering::AcqRel) {
                    runtime.notify_lifecycle(Arc::clone(self));
                }
            }
            Err(_) => *self.owner.lock() = OwnerRef::Closed { generation },
        }
    }

    pub fn complete_control(&self, sequence: u64, result: Result<(), SocketError>) {
        self.control_pending.store(false, Ordering::Release);
        if let Err(error) = result
            && self.kind == SocketKind::Stream
            && self.connect_pending.swap(false, Ordering::AcqRel)
        {
            self.publish_connection_error(error);
        }
        *self.control_result.lock() = Some((sequence, result));
        self.state_wait.wake_all();
        let (readiness, generation) = self.readiness();
        let observer = {
            let observer = self.observer.lock();
            observer
                .as_ref()
                .filter(|observer| observer.readiness_updates_required())
                .cloned()
        };
        if let Some(observer) = observer {
            observer.readiness_changed(readiness, generation);
        }
    }

    pub fn begin_lifecycle_drain(&self) {
        self.lifecycle_notified.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
    }

    pub fn retry_lifecycle(self: &Arc<Self>) {
        if self.closing.load(Ordering::Acquire)
            && !self.lifecycle_notified.swap(true, Ordering::AcqRel)
        {
            socket_runtime()
                .expect("socket runtime 必须保持安装")
                .notify_lifecycle(Arc::clone(self));
        }
    }

    pub fn publish_binding(
        &self,
        owner: OwnerRef,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
    ) {
        self.clear_local_datagram_route();
        if self.kind != SocketKind::Stream {
            invalidate_local_datagram_routes();
        }
        *self.local.lock() = Some(local);
        *self.peer.lock() = peer;
        *self.interface.lock() = interface;
        *self.owner.lock() = owner;
    }

    pub fn publish_connecting(&self) {
        self.stream_connected.store(false, Ordering::Release);
        self.clear_ready(Readiness::WRITABLE);
    }

    pub fn publish_connected(&self) {
        self.connect_pending.store(false, Ordering::Release);
        self.stream_connected.store(true, Ordering::Release);
        self.set_ready(Readiness::WRITABLE);
        self.write_wait.wake_all();
        self.state_wait.wake_all();
    }

    pub fn publish_connection_error(&self, error: SocketError) {
        self.connect_pending.store(false, Ordering::Release);
        self.stream_connected.store(false, Ordering::Release);
        self.clear_local_tcp_direct_route();
        self.stream_rx.lock().eof = true;
        self.set_pending_error(error);
        self.set_ready(
            Readiness::READABLE | Readiness::WRITABLE | Readiness::HANGUP | Readiness::READ_HANGUP,
        );
        self.read_wait.wake_all();
        self.write_wait.wake_all();
        self.state_wait.wake_all();
    }

    pub fn stream_receive_window(&self) -> usize {
        self.stream_rx.lock().bytes.available()
    }

    pub fn publish_closed(&self) {
        self.clear_local_tcp_direct_route();
        *self.owner.lock() = OwnerRef::Closed {
            generation: self.generation(),
        };
        self.state_wait.wake_all();
    }

    pub fn wait_closed(&self, deadline_ns: u64) -> Result<(), SocketError> {
        loop {
            if matches!(self.owner(), OwnerRef::Closed { .. }) {
                return Ok(());
            }
            if sched::now_ns_public() >= deadline_ns {
                return Err(SocketError::TimedOut);
            }
            let task = sched::current_task();
            let entry = self.state_wait.prepare_to_wait(&task, TaskState::Sleeping);
            if matches!(self.owner(), OwnerRef::Closed { .. }) {
                self.state_wait.finish_wait(&entry);
                return Ok(());
            }
            if sched::operation::has_interrupting_signal(&task) {
                self.state_wait.finish_wait(&entry);
                return Err(SocketError::Interrupted);
            }
            let armed = sched::register_sleep_deadline(&task, deadline_ns);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
            let task = sched::current_task();
            self.state_wait.finish_wait(&entry);
            if armed {
                sched::cancel_sleep_deadline(&task);
            }
        }
    }

    pub fn set_pending_error(&self, error: SocketError) {
        self.queue_error(SocketErrorRecord {
            sequence: 0,
            generation: self.generation(),
            error,
            origin: SocketErrorOrigin::Local,
            kind: 0,
            code: 0,
            info: 0,
            offender: self.peer_endpoint(),
        });
    }

    pub fn set_transport_error(
        &self,
        error: crate::transport::TransportControlError,
        offender: Option<Endpoint>,
    ) {
        let (socket_error, origin, kind, code, info) = match (self.family, error) {
            (_, crate::transport::TransportControlError::NetworkUnreachable) => (
                SocketError::NetworkUnreachable,
                if self.family == AddressFamily::Ipv4 {
                    SocketErrorOrigin::Icmp
                } else {
                    SocketErrorOrigin::Icmpv6
                },
                if self.family == AddressFamily::Ipv4 {
                    3
                } else {
                    1
                },
                0,
                0,
            ),
            (_, crate::transport::TransportControlError::HostUnreachable) => (
                SocketError::HostUnreachable,
                if self.family == AddressFamily::Ipv4 {
                    SocketErrorOrigin::Icmp
                } else {
                    SocketErrorOrigin::Icmpv6
                },
                if self.family == AddressFamily::Ipv4 {
                    3
                } else {
                    1
                },
                if self.family == AddressFamily::Ipv4 {
                    1
                } else {
                    3
                },
                0,
            ),
            (_, crate::transport::TransportControlError::PortUnreachable) => (
                SocketError::ConnectionRefused,
                if self.family == AddressFamily::Ipv4 {
                    SocketErrorOrigin::Icmp
                } else {
                    SocketErrorOrigin::Icmpv6
                },
                if self.family == AddressFamily::Ipv4 {
                    3
                } else {
                    1
                },
                if self.family == AddressFamily::Ipv4 {
                    3
                } else {
                    4
                },
                0,
            ),
            (_, crate::transport::TransportControlError::PacketTooBig { mtu }) => (
                SocketError::MessageTooLarge,
                if self.family == AddressFamily::Ipv4 {
                    SocketErrorOrigin::Icmp
                } else {
                    SocketErrorOrigin::Icmpv6
                },
                if self.family == AddressFamily::Ipv4 {
                    3
                } else {
                    2
                },
                if self.family == AddressFamily::Ipv4 {
                    4
                } else {
                    0
                },
                mtu,
            ),
            (_, crate::transport::TransportControlError::TimeExceeded) => (
                SocketError::HostUnreachable,
                if self.family == AddressFamily::Ipv4 {
                    SocketErrorOrigin::Icmp
                } else {
                    SocketErrorOrigin::Icmpv6
                },
                if self.family == AddressFamily::Ipv4 {
                    11
                } else {
                    3
                },
                0,
                0,
            ),
            (_, crate::transport::TransportControlError::ParameterProblem) => (
                SocketError::HostUnreachable,
                if self.family == AddressFamily::Ipv4 {
                    SocketErrorOrigin::Icmp
                } else {
                    SocketErrorOrigin::Icmpv6
                },
                if self.family == AddressFamily::Ipv4 {
                    12
                } else {
                    4
                },
                0,
                0,
            ),
        };
        self.queue_error(SocketErrorRecord {
            sequence: 0,
            generation: self.generation(),
            error: socket_error,
            origin,
            kind,
            code,
            info,
            offender,
        });
    }

    fn queue_error(&self, mut record: SocketErrorRecord) {
        let sequence = self.next_error_sequence.fetch_add(1, Ordering::Relaxed);
        record.sequence = sequence.max(1);
        let code = error_code(record.error);
        let mut queue = self.error_queue.lock();
        if queue.len() == 32 {
            self.error_queue_overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let was_empty = queue.is_empty();
        queue.push_back(record);
        if was_empty {
            self.pending_error.store(code, Ordering::Release);
        }
        drop(queue);
        self.set_ready(Readiness::ERROR);
        self.state_wait.wake_all();
    }

    pub fn error_queue_overflow(&self) -> u64 {
        self.error_queue_overflow.load(Ordering::Acquire)
    }

    pub fn take_pending_error(&self) -> Option<SocketError> {
        self.take_error_record().map(|record| record.error)
    }

    pub fn take_error_record(&self) -> Option<SocketErrorRecord> {
        let mut queue = self.error_queue.lock();
        let record = queue.pop_front()?;
        let next = queue.front().map_or(0, |record| error_code(record.error));
        self.pending_error.store(next, Ordering::Release);
        drop(queue);
        if next == 0 {
            self.clear_ready(Readiness::ERROR);
        } else {
            self.set_ready(Readiness::ERROR);
        }
        Some(record)
    }

    pub fn set_buffer_limits(&self, send: Option<usize>, receive: Option<usize>) {
        if let Some(limit) = send {
            if self.kind == SocketKind::Stream {
                self.tcp_send_buffer_explicit.store(true, Ordering::Release);
            }
            match self.kind {
                SocketKind::Datagram | SocketKind::Raw => self
                    .tx
                    .lock()
                    .as_mut()
                    .expect("UDP facade 必须拥有 TX ring")
                    .set_limit(limit),
                SocketKind::Stream => self.stream_tx.lock().bytes.set_limit(limit),
            }
            self.refresh_tx_readiness();
        }
        if let Some(limit) = receive {
            if self.kind == SocketKind::Stream {
                self.tcp_receive_buffer_explicit
                    .store(true, Ordering::Release);
            }
            match self.kind {
                SocketKind::Datagram | SocketKind::Raw => self
                    .rx
                    .lock()
                    .as_mut()
                    .expect("UDP facade 必须拥有 RX ring")
                    .set_limit(limit),
                SocketKind::Stream => self.stream_rx.lock().bytes.set_limit(limit),
            }
        }
    }

    pub fn buffer_limits(&self) -> (usize, usize) {
        match self.kind {
            SocketKind::Datagram | SocketKind::Raw => (
                self.tx
                    .lock()
                    .as_ref()
                    .expect("UDP facade 必须拥有 TX ring")
                    .limit,
                self.rx
                    .lock()
                    .as_ref()
                    .expect("UDP facade 必须拥有 RX ring")
                    .limit(),
            ),
            SocketKind::Stream => (
                self.stream_tx.lock().bytes.limit(),
                self.stream_rx.lock().bytes.limit,
            ),
        }
    }

    fn refresh_tx_readiness(&self) {
        if self.write_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            self.clear_ready(Readiness::WRITABLE);
            return;
        }
        let writable = match self.kind {
            SocketKind::Datagram | SocketKind::Raw => self
                .tx
                .lock()
                .as_ref()
                .expect("UDP facade 必须拥有 TX ring")
                .writable(),
            SocketKind::Stream => self.stream_is_writable(),
        };
        if writable {
            self.set_ready(Readiness::WRITABLE);
            return;
        }
        self.clear_ready(Readiness::WRITABLE);
        fence(Ordering::SeqCst);
        let writable = match self.kind {
            SocketKind::Datagram | SocketKind::Raw => self
                .tx
                .lock()
                .as_ref()
                .expect("UDP facade 必须拥有 TX ring")
                .writable(),
            SocketKind::Stream => self.stream_is_writable(),
        };
        if writable {
            self.set_ready(Readiness::WRITABLE);
        }
    }

    fn stream_is_writable(&self) -> bool {
        let tx = self.stream_tx.lock();
        if tx.bytes.len().saturating_sub(tx.sent)
            >= self.tcp_notsent_lowat.load(Ordering::Acquire) as usize
        {
            return false;
        }
        tx.writable()
    }

    fn reserve_dma_tx_capacity(&self, pool_key: usize) -> bool {
        if self.write_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return false;
        }
        let reserved = match self.kind {
            SocketKind::Stream => {
                let mut tx = self.stream_tx.lock();
                tx.bytes.len().saturating_sub(tx.sent)
                    < self.tcp_notsent_lowat.load(Ordering::Acquire) as usize
                    && tx.reserve_pool_chunk(pool_key)
            }
            SocketKind::Datagram | SocketKind::Raw => self
                .tx
                .lock()
                .as_ref()
                .is_some_and(|tx| tx.pool_key() == Some(pool_key) && tx.writable()),
        };
        if reserved {
            if self.set_ready(Readiness::WRITABLE) {
                wake_one_socket_writer(&self.write_wait, self.id);
            }
        }
        reserved
    }

    fn exhausted_dma_tx_pool_key(&self) -> Option<usize> {
        match self.kind {
            SocketKind::Stream => self.stream_tx.lock().exhausted_pool_key(),
            SocketKind::Datagram | SocketKind::Raw => {
                self.tx.lock().as_ref().and_then(TxRing::exhausted_pool_key)
            }
        }
    }

    fn refresh_rx_readiness(&self) {
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            self.clear_ready(Readiness::READABLE);
            return;
        }
        let readable = match self.kind {
            SocketKind::Datagram | SocketKind::Raw => !self
                .rx
                .lock()
                .as_ref()
                .expect("UDP facade 必须拥有 RX ring")
                .is_empty(),
            SocketKind::Stream => {
                let rx = self.stream_rx.lock();
                rx.bytes.len != 0 || rx.eof
            }
        };
        if readable {
            self.set_ready(Readiness::READABLE);
            return;
        }
        self.clear_ready(Readiness::READABLE);
        fence(Ordering::SeqCst);
        let readable = match self.kind {
            SocketKind::Datagram | SocketKind::Raw => !self
                .rx
                .lock()
                .as_ref()
                .expect("UDP facade 必须拥有 RX ring")
                .is_empty(),
            SocketKind::Stream => {
                let rx = self.stream_rx.lock();
                rx.bytes.len != 0 || rx.eof
            }
        };
        if readable {
            self.set_ready(Readiness::READABLE);
        }
    }

    fn refresh_accept_readiness(&self) {
        if self
            .listen_group
            .lock()
            .as_ref()
            .is_none_or(|group| !group.has_ready())
        {
            self.clear_ready(Readiness::ACCEPTABLE | Readiness::READABLE);
        } else {
            self.set_ready(Readiness::ACCEPTABLE | Readiness::READABLE);
            wake_one_socket_waiter(&self.accept_wait);
        }
    }

    fn next_control_sequence(&self) -> u64 {
        self.control_sequence.fetch_add(1, Ordering::Relaxed)
    }

    fn wait_control(&self, sequence: u64) -> Result<(), SocketError> {
        loop {
            self.ensure_stack_attached()?;
            if let Some(result) = self.take_control_result(sequence) {
                return result;
            }
            let task = sched::current_task();
            let entry = self.state_wait.prepare_to_wait(&task, TaskState::Sleeping);
            if let Err(error) = self.ensure_stack_attached() {
                self.state_wait.finish_wait(&entry);
                return Err(error);
            }
            if let Some(result) = self.take_control_result(sequence) {
                self.state_wait.finish_wait(&entry);
                return result;
            }
            if sched::operation::has_interrupting_signal(&task) {
                self.state_wait.finish_wait(&entry);
                return Err(SocketError::Interrupted);
            }
            drop(task);
            sched::schedule_once(sched::now_ns_public());
            self.state_wait.finish_wait(&entry);
        }
    }

    pub fn take_control_result(&self, sequence: u64) -> Option<Result<(), SocketError>> {
        let mut result = self.control_result.lock();
        if result.as_ref().is_some_and(|entry| entry.0 == sequence) {
            result.take().map(|entry| entry.1)
        } else {
            None
        }
    }

    fn wait_read(&self, deadline_ns: Option<u64>) -> Result<(), SocketError> {
        self.wait_io(&self.read_wait, Readiness::READABLE, deadline_ns)
    }

    fn wait_write(&self, deadline_ns: Option<u64>) -> Result<(), SocketError> {
        self.wait_io(&self.write_wait, Readiness::WRITABLE, deadline_ns)
    }

    fn wait_datagram_write(
        &self,
        payload_len: usize,
        deadline_ns: Option<u64>,
    ) -> Result<(), SocketError> {
        self.wait_io_until(&self.write_wait, deadline_ns, |facade| {
            let (current, _) = facade.readiness();
            socket_wait_terminal(current)
                || facade
                    .tx
                    .lock()
                    .as_ref()
                    .is_some_and(|tx| tx.can_push_len(payload_len))
        })
    }

    fn wait_accept(&self, deadline_ns: Option<u64>) -> Result<(), SocketError> {
        self.wait_io(&self.accept_wait, Readiness::ACCEPTABLE, deadline_ns)
    }

    fn wait_io(
        &self,
        queue: &WaitQueue,
        readiness: Readiness,
        deadline_ns: Option<u64>,
    ) -> Result<(), SocketError> {
        self.wait_io_until(queue, deadline_ns, |facade| {
            let (current, _) = facade.readiness();
            socket_wait_observable(current, readiness)
        })
    }

    fn wait_io_until(
        &self,
        queue: &WaitQueue,
        deadline_ns: Option<u64>,
        observable: impl Fn(&Self) -> bool,
    ) -> Result<(), SocketError> {
        // Timer IRQ 若发生在内核态，只会记录 deferred tick。持续背压可能在
        // readiness generation 变化与重试之间长期停留在同一个 syscall，先在
        // 无锁边界推进 ITIMER_REAL，避免 alarm 被高频 socket wait 饿死。
        sched::drain_deferred_timer_tick();
        self.ensure_stack_attached()?;
        let task = sched::current_task();
        let entry = queue.prepare_to_wait(&task, TaskState::Sleeping);
        if let Err(error) = self.ensure_stack_attached() {
            queue.finish_wait(&entry);
            return Err(error);
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.finish_wait(&entry);
            return Err(SocketError::Interrupted);
        }
        if observable(self) {
            queue.finish_wait(&entry);
            return Ok(());
        }
        if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
            queue.finish_wait(&entry);
            return Err(SocketError::TimedOut);
        }
        let armed =
            deadline_ns.is_some_and(|deadline| sched::register_sleep_deadline(&task, deadline));
        drop(task);
        sched::schedule_once(sched::now_ns_public());
        let task = sched::current_task();
        queue.finish_wait(&entry);
        if armed {
            sched::cancel_sleep_deadline(&task);
        }
        self.ensure_stack_attached()?;
        if sched::operation::has_interrupting_signal(&task) {
            return Err(SocketError::Interrupted);
        }
        if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
            return Err(SocketError::TimedOut);
        }
        Ok(())
    }

    fn set_ready(&self, bits: Readiness) -> bool {
        self.update_ready(bits.0, 0)
    }

    fn clear_ready(&self, bits: Readiness) {
        self.update_ready(0, bits.0);
    }

    fn update_ready(&self, set: u16, clear: u16) -> bool {
        let mut current = self.readiness.load(Ordering::Acquire);
        loop {
            let next = (current | set) & !clear;
            if next == current {
                return false;
            }
            match self.readiness.compare_exchange_weak(
                current,
                next,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let generation = self
                        .readiness_generation
                        .fetch_add(1, Ordering::AcqRel)
                        .wrapping_add(1)
                        .max(1);
                    let observer = {
                        let observer = self.observer.lock();
                        observer
                            .as_ref()
                            .filter(|observer| observer.readiness_updates_required())
                            .cloned()
                    };
                    if let Some(observer) = observer {
                        observer.readiness_changed(Readiness(next), generation);
                    }
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for SocketFacade {
    fn drop(&mut self) {
        self.clear_local_tcp_bulk_active();
        self.deactivate_packet_observer();
    }
}

fn socket_wait_observable(current: Readiness, requested: Readiness) -> bool {
    current.contains(requested) || socket_wait_terminal(current)
}

fn socket_wait_terminal(current: Readiness) -> bool {
    current.intersects(Readiness::ERROR | Readiness::HANGUP | Readiness::READ_HANGUP)
}

fn error_code(error: SocketError) -> u32 {
    error as u32 + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::NetBufPool;
    use crate::{IpAddr, Ipv4Addr};

    #[test]
    fn resident_allocation_scope_restores_outer_elm_context() {
        assert!(elm_model::current_context().is_none());
        let outer = elm_model::ElmContext::new(
            elm_model::ElmId(7),
            None,
            elm_model::Generation::FIRST,
            elm_model::ElmState::Active,
            elm_model::ElmLifecyclePhase::Initialize,
            0,
        );
        let _outer = elm_model::enter_current_context(&outer).expect("进入外层 ELM 上下文");
        assert_eq!(elm_model::current_cell(), Some(elm_model::ElmId(7)));

        let resident = enter_resident_allocation_scope()
            .expect("建立 resident allocation scope")
            .expect("ELM 内必须建立嵌套 scope");
        assert!(elm_model::current_cell().is_none());
        drop(resident);

        assert_eq!(elm_model::current_cell(), Some(elm_model::ElmId(7)));
    }

    #[test]
    fn stack_generation_detach_publishes_stable_network_down() {
        let counter = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed).max(1);
        let generation = u64::MAX.saturating_sub(counter);
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ));
        facade.stack_generation.store(generation, Ordering::Release);
        SOCKET_REGISTRY.write().push(Arc::downgrade(&facade));

        assert_eq!(detach_socket_generation(generation), 1);
        assert_eq!(facade.backend_error(), Some(SocketError::NetworkDown));
        let readiness = facade.readiness().0;
        assert!(readiness.contains(Readiness::ERROR));
        assert!(readiness.contains(Readiness::HANGUP));
        assert!(readiness.contains(Readiness::READ_HANGUP));
        assert!(!readiness.contains(Readiness::WRITABLE));
        assert_eq!(facade.take_pending_error(), Some(SocketError::NetworkDown));
        assert_eq!(facade.backend_error(), Some(SocketError::NetworkDown));
        assert_eq!(
            facade.send(&[1], None, true, None),
            Err(SocketError::NetworkDown)
        );
        facade.close();
        assert!(matches!(facade.owner(), OwnerRef::Closed { .. }));
    }

    fn facade() -> Arc<SocketFacade> {
        Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 1,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        ))
    }

    fn stream_facade(counter: u64) -> Arc<SocketFacade> {
        Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ))
    }

    fn sleeping_task() -> Arc<sched::Task> {
        let session = sched::Session::new();
        let process_group = sched::ProcessGroup::new(&session);
        session.register_group(&process_group);
        sched::Task::new(
            sched::SchedParams::default_fair(),
            Weak::new(),
            sched::ThreadGroup::new(),
            process_group,
        )
    }

    #[test]
    fn local_stream_lease_handoff_waits_for_flush() {
        let sender = stream_facade(2);
        let receiver = stream_facade(3);
        let mut payload = alloc::vec![0x31; 1024];
        payload[512..].fill(0xa7);
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        let first = sender.take_stream_tx(512).unwrap();
        let second = sender.take_stream_tx(512).unwrap();

        let task = sleeping_task();
        let entry = receiver
            .read_wait
            .prepare_to_wait(&task, TaskState::Sleeping);

        receiver
            .push_stream_rx_lease(&first, 0, 512, false)
            .unwrap();

        assert_eq!(task.state(), TaskState::Sleeping);
        assert_eq!(receiver.read_wait.len_hint(), 1);
        assert!(receiver.readiness().0.contains(Readiness::READABLE));

        receiver
            .push_stream_rx_lease(&second, 0, 512, true)
            .unwrap();

        assert_eq!(task.state(), TaskState::Runnable);
        assert_eq!(receiver.read_wait.len_hint(), 0);
        receiver.read_wait.finish_wait(&entry);

        let mut output = alloc::vec![0; payload.len()];
        assert_eq!(
            receiver.recv_stream(&mut output, false, true, false, true, None),
            Ok(payload.len())
        );
        assert_eq!(output, payload);
    }

    fn install_test_local_tcp_direct_pair(
        sender: &Arc<SocketFacade>,
        receiver: &Arc<SocketFacade>,
    ) {
        *sender.owner.lock() = OwnerRef::Flow {
            shard: ShardId(0),
            flow: FlowId(1),
            generation: sender.generation(),
        };
        *receiver.owner.lock() = OwnerRef::Flow {
            shard: ShardId(0),
            flow: FlowId(2),
            generation: receiver.generation(),
        };
        sender.install_local_tcp_direct_peer(receiver);
    }

    fn install_test_local_datagram_route(
        sender: &Arc<SocketFacade>,
        receiver: Arc<SocketFacade>,
        destination: Endpoint,
    ) {
        receiver
            .rx
            .lock()
            .as_mut()
            .expect("UDP facade 必须拥有 RX ring")
            .prepare_local_lane();
        *sender.local_datagram_route.lock() = Some(LocalDatagramRoute {
            // 并行单测会通过其它 socket 的 bind/close 推进全局 epoch；保留值只在
            // cfg(test) 中有效，避免测试 helper 与无关用例共享可变时序。
            epoch: u64::MAX,
            stack_generation: sender.stack_generation(),
            sender_generation: sender.generation(),
            receiver_generation: receiver.generation(),
            destination,
            source: Endpoint {
                addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 8000,
            },
            delivered_to: destination,
            interface: InterfaceId(1),
            dont_route: false,
            confirm: false,
            mark: sender.socket_mark(),
            hop_limit: sender.ip_hop_limit(),
            traffic_class: sender.ip_traffic_class(),
            route_mtu: u16::MAX as u32,
            receiver,
        });
    }

    #[test]
    fn stream_facade_does_not_allocate_udp_rings() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 2,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        assert!(facade.tx.lock().is_none());
        assert!(facade.rx.lock().is_none());
        assert_eq!(
            facade.stream_tx.lock().bytes.allocated_capacity(),
            2 * SOCKET_CHUNK_BYTES
        );
        assert_eq!(facade.stream_rx.lock().bytes.allocated_capacity(), 0);
    }

    #[test]
    fn local_stream_autotune_preserves_explicit_buffer_limits() {
        let automatic = stream_facade(60);
        assert_eq!(
            automatic.buffer_limits(),
            (TCP_BUFFER_BYTES, TCP_BUFFER_BYTES)
        );
        automatic.prepare_local_stream_send();
        assert_eq!(automatic.buffer_limits().0, TCP_LOCAL_AUTOTUNE_LIMIT);

        let explicit = stream_facade(61);
        explicit.set_buffer_limits(Some(32 * 1024), Some(48 * 1024));
        explicit.prepare_local_stream_send();
        assert_eq!(explicit.buffer_limits(), (32 * 1024, 48 * 1024));

        let sender = stream_facade(62);
        sender.prepare_local_stream_send();
        let payload = alloc::vec![0x4a; 60 * 1024];
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        let lease = sender.take_stream_tx(payload.len()).unwrap();
        automatic
            .push_stream_rx_lease(&lease, 0, payload.len(), true)
            .unwrap();
        assert_eq!(automatic.buffer_limits().1, TCP_LOCAL_AUTOTUNE_LIMIT);
        assert_eq!(explicit.buffer_limits().1, 48 * 1024);
    }

    #[test]
    fn local_stream_autotune_stays_within_socket_hard_limit() {
        let facade = stream_facade(65);
        facade.prepare_local_stream_send();
        assert_eq!(
            facade.buffer_limits(),
            (TCP_BUFFER_HARD_LIMIT, TCP_BUFFER_BYTES)
        );
    }

    #[test]
    fn local_stream_window_updates_only_after_backpressure() {
        let sender = stream_facade(63);
        let receiver = stream_facade(64);
        sender.prepare_local_stream_send();
        receiver.set_buffer_limits(None, Some(16 * 1024));
        let payload = alloc::vec![0x57; 16 * 1024];
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        let first = sender.take_stream_tx(payload.len()).unwrap();
        receiver
            .push_stream_rx_lease(&first, 0, payload.len(), true)
            .unwrap();

        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        let blocked = sender.take_stream_tx(payload.len()).unwrap();
        assert_eq!(
            receiver.push_stream_rx_lease(&blocked, 0, payload.len(), true),
            Err(SocketError::WouldBlock)
        );
        assert!(receiver.local_tcp_window_blocked.load(Ordering::Acquire));

        let mut output = [0u8; SOCKET_CHUNK_BYTES];
        assert_eq!(
            receiver.recv_stream(&mut output, false, false, true, true, None),
            Ok(output.len())
        );
        assert!(receiver.receive_window_update.load(Ordering::Acquire));
        assert!(!receiver.local_tcp_window_blocked.load(Ordering::Acquire));
    }

    #[test]
    fn uncongested_local_stream_consumption_skips_state_notification() {
        let sender = stream_facade(65);
        let receiver = stream_facade(66);
        sender.prepare_local_stream_send();
        let payload = [0x31; SOCKET_CHUNK_BYTES];
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        let lease = sender.take_stream_tx(payload.len()).unwrap();
        receiver
            .push_stream_rx_lease(&lease, 0, payload.len(), true)
            .unwrap();

        let mut output = [0u8; SOCKET_CHUNK_BYTES];
        assert_eq!(
            receiver.recv_stream(&mut output, false, false, true, true, None),
            Ok(output.len())
        );
        assert_eq!(output, payload);
        assert!(!receiver.receive_window_update.load(Ordering::Acquire));
    }

    #[test]
    fn local_tcp_direct_lane_moves_shared_bytes_and_tracks_reconciliation() {
        let sender = stream_facade(67);
        let receiver = stream_facade(68);
        sender.stack_generation.store(9, Ordering::Release);
        receiver.stack_generation.store(9, Ordering::Release);
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(4, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        assert!(sender.install_stream_tx_pool(pool));
        sender.mark_local_stream_tx_prepared();
        install_test_local_tcp_direct_pair(&sender, &receiver);

        let payload = alloc::vec![0x6a; 2 * SOCKET_CHUNK_BYTES];
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        assert_eq!(sender.try_deliver_local_tcp_direct(), payload.len());
        assert_eq!(sender.stream_unsent_len(), 0);
        assert_eq!(sender.take_local_tcp_direct_pending(), payload.len() as u32);

        let mut output = alloc::vec![0; payload.len()];
        assert_eq!(
            receiver.recv_stream(&mut output, false, false, true, true, None),
            Ok(payload.len())
        );
        assert_eq!(output, payload);
    }

    #[test]
    fn local_tcp_direct_multi_lease_wakes_only_for_final_lease() {
        let sender = stream_facade(73);
        let receiver = stream_facade(74);
        sender.stack_generation.store(12, Ordering::Release);
        receiver.stack_generation.store(12, Ordering::Release);
        sender.mark_local_stream_tx_prepared();
        install_test_local_tcp_direct_pair(&sender, &receiver);

        let first_task = sleeping_task();
        let first_entry = receiver
            .read_wait
            .prepare_to_wait(&first_task, TaskState::Sleeping);
        let second_task = sleeping_task();
        let second_entry = receiver
            .read_wait
            .prepare_to_wait(&second_task, TaskState::Sleeping);

        let payload = alloc::vec![0x5cu8; u16::MAX as usize + 512];
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        assert_eq!(sender.try_deliver_local_tcp_direct(), payload.len());

        let runnable = usize::from(first_task.state() == TaskState::Runnable)
            + usize::from(second_task.state() == TaskState::Runnable);
        assert_eq!(runnable, 1, "只有最终 lease 可以唤醒一个 reader");
        assert_eq!(receiver.read_wait.len_hint(), 1);
        receiver.read_wait.finish_wait(&first_entry);
        receiver.read_wait.finish_wait(&second_entry);

        let mut output = alloc::vec![0; payload.len()];
        assert_eq!(
            receiver.recv_stream(&mut output, false, true, false, true, None),
            Ok(payload.len())
        );
        assert_eq!(output, payload);
    }

    #[test]
    fn local_tcp_user_window_copies_directly_without_sender_pool() {
        let sender = stream_facade(69);
        let receiver = stream_facade(70);
        sender.stack_generation.store(10, Ordering::Release);
        receiver.stack_generation.store(10, Ordering::Release);
        install_test_local_tcp_direct_pair(&sender, &receiver);

        let payload = alloc::vec![0x4du8; 128 * 1024];
        assert_eq!(
            sender
                .try_send_local_tcp_direct_from(payload.len(), &mut |offset, output| {
                    output.copy_from_slice(&payload[offset..offset + output.len()]);
                })
                .unwrap(),
            Some(payload.len())
        );
        assert_eq!(sender.test_stream_tx_len(), 0);
        assert_eq!(sender.take_local_tcp_direct_pending(), payload.len() as u32);

        let mut output = alloc::vec![0u8; payload.len()];
        assert_eq!(
            receiver.recv_stream(&mut output, false, false, true, true, None),
            Ok(payload.len())
        );
        assert_eq!(output, payload);
    }

    #[test]
    fn local_tcp_direct_copy_never_overtakes_buffered_stream_data() {
        let sender = stream_facade(71);
        let receiver = stream_facade(72);
        sender.stack_generation.store(11, Ordering::Release);
        receiver.stack_generation.store(11, Ordering::Release);
        install_test_local_tcp_direct_pair(&sender, &receiver);

        let buffered = [0x31u8; 32];
        assert_eq!(sender.test_push_stream_tx(&buffered), buffered.len());
        let direct = [0x42u8; 16];
        assert_eq!(
            sender
                .try_send_local_tcp_direct_from(direct.len(), &mut |offset, output| {
                    output.copy_from_slice(&direct[offset..offset + output.len()]);
                })
                .unwrap(),
            None
        );
        assert_eq!(sender.test_stream_tx_len(), buffered.len());
        assert_eq!(receiver.stream_rx.lock().bytes.len, 0);
    }

    #[test]
    fn raw_socket_observer_predicate_tracks_lifecycle() {
        let raw = SocketFacade::new_with_protocol(
            SocketId {
                boot_nonce: 7,
                counter: 90,
            },
            AddressFamily::Ipv4,
            SocketKind::Raw,
            17,
        );
        assert!(facade_requires_packet_observation(&raw));
        raw.closing.store(true, Ordering::Release);
        assert!(!facade_requires_packet_observation(&raw));

        let datagram = SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 91,
            },
            AddressFamily::Ipv4,
            SocketKind::Datagram,
        );
        assert!(!facade_requires_packet_observation(&datagram));
    }

    #[test]
    fn packet_observer_registration_is_balanced_and_idempotent() {
        let active = AtomicBool::new(false);
        let observers = AtomicU32::new(0);

        assert!(packet_observers_allow_local_transport(&observers));
        assert!(register_packet_observer(&active, &observers));
        assert!(!register_packet_observer(&active, &observers));
        assert_eq!(observers.load(Ordering::Acquire), 1);
        assert!(!packet_observers_allow_local_transport(&observers));

        assert!(unregister_packet_observer(&active, &observers));
        assert!(!unregister_packet_observer(&active, &observers));
        assert_eq!(observers.load(Ordering::Acquire), 0);
        assert!(packet_observers_allow_local_transport(&observers));
    }

    #[test]
    fn stream_rx_rejects_whole_segment_when_space_is_insufficient() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 3,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        facade.set_buffer_limits(None, Some(16 * 1024));
        assert_eq!(facade.push_stream_rx(&[1; 15 * 1024]), Ok(15 * 1024));
        assert_eq!(
            facade.push_stream_rx(&[2; 2 * 1024]),
            Err(SocketError::WouldBlock)
        );
        assert_eq!(facade.stream_rx.lock().bytes.len, 15 * 1024);
    }

    #[test]
    fn stream_rx_pins_healthy_physical_chunk_until_user_consumes_it() {
        let mut owner = NetBufPool::new_heap(2, SOCKET_CHUNK_BYTES).unwrap();
        let mut lease = owner.lease(0, 1024, PacketMetadata::default()).unwrap();
        lease.as_mut_slice().unwrap().fill(0x5a);
        let mut packet = PacketChain::from_lease(lease);
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 51,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));

        let commit = facade
            .push_stream_rx_packet(&mut packet, 0, 1024, RxPoolPressure::Normal)
            .unwrap();
        assert_eq!(commit.storage, StreamRxStorageKind::PhysicalPinned);
        drop(packet);
        owner.drain_remote();
        assert_eq!(owner.available(), 1);

        let mut output = alloc::vec![0; 1024];
        assert_eq!(
            facade
                .recv_stream(&mut output, false, true, false, true, None)
                .unwrap(),
            output.len()
        );
        assert!(output.iter().all(|byte| *byte == 0x5a));
        owner.drain_remote();
        assert_eq!(owner.available(), 2);
    }

    #[test]
    fn stream_rx_low_water_compacts_and_returns_dma_chunk_immediately() {
        let mut owner = NetBufPool::new_heap(1, SOCKET_CHUNK_BYTES).unwrap();
        let mut lease = owner.lease(0, 1024, PacketMetadata::default()).unwrap();
        lease.as_mut_slice().unwrap().fill(0x6b);
        let mut packet = PacketChain::from_lease(lease);
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 52,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));

        let commit = facade
            .push_stream_rx_packet(&mut packet, 0, 1024, RxPoolPressure::Low)
            .unwrap();
        assert_eq!(commit.storage, StreamRxStorageKind::Compact);
        assert!(commit.low_water_fallback);
        drop(packet);
        owner.drain_remote();
        assert_eq!(owner.available(), 1);

        let rx = facade.stream_rx.lock();
        assert_eq!(rx.bytes.allocated_capacity(), 1024);
    }

    #[test]
    fn stream_reset_is_delivered_after_buffered_data_then_eof() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 4,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        facade.push_stream_rx(b"data").unwrap();
        facade.publish_connection_error(SocketError::ConnectionReset);
        let mut output = [0u8; 8];
        assert_eq!(
            facade.recv_stream(&mut output, false, false, false, true, None),
            Ok(4)
        );
        assert_eq!(&output[..4], b"data");
        assert_eq!(
            facade.recv_stream(&mut output, false, false, false, true, None),
            Err(SocketError::ConnectionReset)
        );
        assert_eq!(
            facade.recv_stream(&mut output, false, false, false, true, None),
            Ok(0)
        );
    }

    #[test]
    fn udp_tx_ring_preserves_datagram_boundaries_and_reclaims_chunks() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        facade
            .tx
            .lock()
            .as_mut()
            .unwrap()
            .push(b"first", destination, false, false)
            .unwrap();
        facade
            .tx
            .lock()
            .as_mut()
            .unwrap()
            .push(b"second", destination, false, false)
            .unwrap();
        let first = facade.take_tx().unwrap();
        let second = facade.take_tx().unwrap();
        let mut output = [0u8; 8];
        assert_eq!(first.copy_out(&mut output).unwrap(), 5);
        assert_eq!(&output[..5], b"first");
        output.fill(0);
        assert_eq!(second.copy_out(&mut output).unwrap(), 6);
        assert_eq!(&output[..6], b"second");
        first.complete();
        second.complete();
        let tx = facade.tx.lock();
        let tx = tx.as_ref().unwrap();
        assert_eq!(tx.used_bytes, 0);
        assert_eq!(tx.free_chunk_count(), UDP_BUFFER_BYTES / SOCKET_CHUNK_BYTES);
    }

    #[test]
    fn udp_external_copy_failure_rolls_back_the_whole_datagram() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let mut tx = facade.tx.lock();
        let tx = tx.as_mut().unwrap();
        let free_chunks = tx.free_chunk_count();
        let free_slots = tx.free_slots.len();
        let result = tx.push_from(
            SOCKET_CHUNK_BYTES + 16,
            destination,
            false,
            false,
            &mut |offset, output| {
                if offset != 0 {
                    return Err(7u8);
                }
                output.fill(0x5a);
                Ok(())
            },
        );
        assert!(matches!(result, Err(DatagramCopyError::Copy(7))));
        assert!(tx.is_empty());
        assert_eq!(tx.used_bytes, 0);
        assert_eq!(tx.free_chunk_count(), free_chunks);
        assert_eq!(tx.free_slots.len(), free_slots);
    }

    #[test]
    fn cached_local_datagram_bypasses_sender_tx_ring() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        let payload = alloc::vec![0x6d; SOCKET_CHUNK_BYTES + 19];

        let result = sender.send_datagram_from(
            payload.len(),
            Some(destination),
            true,
            None,
            false,
            false,
            |offset, output| {
                output.copy_from_slice(&payload[offset..offset + output.len()]);
                Ok::<(), u8>(())
            },
        );

        assert!(matches!(result, Ok(written) if written == payload.len()));
        assert!(sender.tx.lock().as_ref().unwrap().is_empty());
        let mut output = alloc::vec![0; payload.len()];
        {
            let rx_guard = receiver.rx.lock();
            let rx = rx_guard.as_ref().unwrap();
            assert_eq!(rx.len(), 1);
            assert_eq!(rx.bytes(), payload.len());
            rx.copy_front(&mut output).unwrap();
            assert_eq!(output, payload);
        }

        output.fill(0);
        let received = receiver
            .recv_local_datagram_from(output.len(), output.len(), false, |offset, input| {
                output[offset..offset + input.len()].copy_from_slice(input);
                Ok::<(), u8>(())
            })
            .ok()
            .flatten()
            .expect("缓存直达的数据报必须使用本地存储");
        assert_eq!(received.len, payload.len());
        assert_eq!(output, payload);
        assert!(receiver.rx.lock().as_ref().unwrap().is_empty());
    }

    #[test]
    fn cached_local_route_falls_back_when_payload_exceeds_route_mtu() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9015,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        sender
            .local_datagram_route
            .lock()
            .as_mut()
            .expect("测试路由必须存在")
            .route_mtu = 64;
        sender.tx_notified.store(true, Ordering::Release);

        assert_eq!(
            sender.send(&[0x52; 64], Some(destination), true, None),
            Ok(64)
        );
        assert!(sender.has_pending_datagram_tx());
        assert!(receiver.rx.lock().as_ref().unwrap().is_empty());
        assert!(sender.local_datagram_route.lock().is_none());
    }

    #[test]
    fn small_local_datagram_uses_reusable_lane_without_rx_chunks() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9010,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        let payload = [0x61; 32];
        let free_chunks = receiver.rx.lock().as_ref().unwrap().ring.free_chunk_count();

        assert!(matches!(
            sender.send_datagram_from(
                payload.len(),
                Some(destination),
                true,
                None,
                false,
                false,
                |offset, output| {
                    output.copy_from_slice(&payload[offset..offset + output.len()]);
                    Ok::<(), u8>(())
                },
            ),
            Ok(written) if written == payload.len()
        ));
        {
            let rx = receiver.rx.lock();
            let rx = rx.as_ref().unwrap();
            assert!(rx.local_occupied());
            assert!(rx.ring.is_empty());
            assert_eq!(rx.ring.free_chunk_count(), free_chunks);
        }

        let mut output = [0; 32];
        let received = receiver
            .recv_local_datagram_from(output.len(), output.len(), false, |offset, input| {
                output[offset..offset + input.len()].copy_from_slice(input);
                Ok::<(), u8>(())
            })
            .ok()
            .flatten()
            .expect("lane 中的数据报必须可读");
        assert_eq!(received.len, payload.len());
        assert_eq!(output, payload);
        let rx = receiver.rx.lock();
        let rx = rx.as_ref().unwrap();
        assert!(rx.is_empty());
        assert_eq!(rx.ring.free_chunk_count(), free_chunks);
    }

    #[test]
    fn occupied_local_lane_keeps_order_before_ring_fallback() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9011,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);

        for value in [0x21, 0x42] {
            assert_eq!(
                sender
                    .send_datagram_from(
                        1,
                        Some(destination),
                        true,
                        None,
                        false,
                        false,
                        |_, output| {
                            output[0] = value;
                            Ok::<(), u8>(())
                        },
                    )
                    .ok()
                    .expect("本地数据报必须写入成功"),
                1
            );
        }
        {
            let rx = receiver.rx.lock();
            let rx = rx.as_ref().unwrap();
            assert!(rx.local_occupied());
            assert_eq!(rx.ring.len, 1);
        }

        for expected in [0x21, 0x42] {
            let mut output = [0];
            receiver
                .recv_local_datagram_from(1, 1, false, |_, input| {
                    output.copy_from_slice(input);
                    Ok::<(), u8>(())
                })
                .ok()
                .flatten()
                .expect("本地数据报必须按顺序可读");
            assert_eq!(output, [expected]);
        }
        assert!(receiver.rx.lock().as_ref().unwrap().is_empty());
    }

    #[test]
    fn local_lane_copy_failure_keeps_the_datagram() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9012,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        assert_eq!(
            sender
                .send_datagram_from(
                    1,
                    Some(destination),
                    true,
                    None,
                    false,
                    false,
                    |_, output| {
                        output[0] = 0x73;
                        Ok::<(), u8>(())
                    },
                )
                .ok()
                .expect("本地数据报必须写入成功"),
            1
        );

        let failed = receiver.recv_local_datagram_from(1, 1, false, |_, _| Err(13u8));
        assert!(matches!(failed, Err(DatagramCopyError::Copy(13))));
        assert!(receiver.rx.lock().as_ref().unwrap().local_occupied());

        let mut output = [0];
        receiver
            .recv_local_datagram_from(1, 1, false, |_, input| {
                output.copy_from_slice(input);
                Ok::<(), u8>(())
            })
            .ok()
            .flatten()
            .expect("复制失败后数据报必须仍在 lane 中");
        assert_eq!(output, [0x73]);
    }

    #[test]
    fn cached_local_datagram_copy_failure_rolls_back_receiver_ring() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9001,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        let (free_chunks, entries, bytes) = {
            let rx = receiver.rx.lock();
            let rx = rx.as_ref().unwrap();
            (rx.ring.free_chunk_count(), rx.len(), rx.bytes())
        };

        let result = sender.send_datagram_from(
            SOCKET_CHUNK_BYTES + 19,
            Some(destination),
            true,
            None,
            false,
            false,
            |offset, output| {
                if offset != 0 {
                    return Err(9u8);
                }
                output.fill(0x5a);
                Ok(())
            },
        );

        assert!(matches!(result, Err(DatagramCopyError::Copy(9))));
        assert!(sender.tx.lock().as_ref().unwrap().is_empty());
        let rx = receiver.rx.lock();
        let rx = rx.as_ref().unwrap();
        assert_eq!(rx.len(), entries);
        assert_eq!(rx.bytes(), bytes);
        assert_eq!(rx.ring.free_chunk_count(), free_chunks);
    }

    #[test]
    fn local_datagram_receive_copy_failure_keeps_datagram_queued() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9002,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        let payload = alloc::vec![0x37; SOCKET_CHUNK_BYTES + 23];
        assert!(matches!(
            sender.send_datagram_from(
                payload.len(),
                Some(destination),
                true,
                None,
                false,
                false,
                |offset, output| {
                    output.copy_from_slice(&payload[offset..offset + output.len()]);
                    Ok::<(), u8>(())
                },
            ),
            Ok(written) if written == payload.len()
        ));

        let failed =
            receiver.recv_local_datagram_from(payload.len(), payload.len(), false, |offset, _| {
                if offset == 0 { Ok(()) } else { Err(11u8) }
            });
        assert!(matches!(failed, Err(DatagramCopyError::Copy(11))));
        assert_eq!(receiver.rx.lock().as_ref().unwrap().len(), 1);

        let mut output = alloc::vec![0; payload.len()];
        let received = receiver
            .recv_local_datagram_from(output.len(), output.len(), false, |offset, input| {
                output[offset..offset + input.len()].copy_from_slice(input);
                Ok::<(), u8>(())
            })
            .ok()
            .flatten()
            .expect("复制失败后数据报必须保持在队首");
        assert_eq!(received.len, payload.len());
        assert_eq!(output, payload);
        assert!(receiver.rx.lock().as_ref().unwrap().is_empty());
    }

    #[test]
    fn local_datagram_capacity_change_keeps_datagram_queued() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9007,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        let payload = [0x5a; 32];
        assert_eq!(
            sender.send(&payload, Some(destination), true, None),
            Ok(payload.len())
        );

        let mut copied = false;
        let result = receiver.recv_local_datagram_from(payload.len(), 1, false, |_, _| {
            copied = true;
            Ok::<(), u8>(())
        });
        assert!(matches!(result, Ok(None)));
        assert!(!copied);
        assert_eq!(receiver.rx.lock().as_ref().unwrap().len(), 1);

        let mut output = [0; 32];
        let received = receiver
            .recv_local_datagram_from(output.len(), output.len(), false, |offset, input| {
                output[offset..offset + input.len()].copy_from_slice(input);
                Ok::<(), u8>(())
            })
            .ok()
            .flatten()
            .expect("容量充足后队首数据报必须仍可读取");
        assert_eq!(received.len, payload.len());
        assert_eq!(output, payload);
    }

    #[test]
    fn pooled_local_datagram_keeps_one_shared_reference_until_receive() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(2, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let sender = facade();
        let receiver = facade();
        sender.install_datagram_tx_pool(Arc::clone(&pool));
        let source = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40000,
        };
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9003,
        };
        let payload = alloc::vec![0x6b; 1024];
        let lease = sender.test_udp_tx_lease(&payload, destination);
        assert_eq!(pool.lock().available(), 1);

        receiver
            .push_local_udp(&lease, source, destination, 64, 0, InterfaceId(1), 123)
            .unwrap();
        lease.complete();
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 1);
        assert!(matches!(
            receiver
                .rx
                .lock()
                .as_ref()
                .unwrap()
                .ring
                .front()
                .map(|datagram| &datagram.payload),
            Some(RxPayload::Shared(_))
        ));

        let mut output = alloc::vec![0; payload.len()];
        let received = receiver
            .recv_local_datagram_from(output.len(), output.len(), false, |offset, input| {
                output[offset..offset + input.len()].copy_from_slice(input);
                Ok::<(), u8>(())
            })
            .ok()
            .flatten()
            .expect("共享数据报必须可以完整读取");
        assert_eq!(received.len, payload.len());
        assert_eq!(received.source, source);
        assert_eq!(received.destination, destination);
        assert_eq!(received.ingress_interface, InterfaceId(1));
        assert_eq!(received.hop_limit, 64);
        assert_eq!(received.traffic_class, 0);
        assert_eq!(received.rx_timestamp_ns, 123);
        assert_eq!(output, payload);
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 2);
    }

    #[test]
    fn stale_shared_datagram_returns_buffer_error_without_consuming() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(1, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let sender = facade();
        let receiver = facade();
        sender.install_datagram_tx_pool(Arc::clone(&pool));
        let source = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 40003,
        };
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9006,
        };
        let payload = sender.test_udp_tx_lease(&[0x63; 32], destination);
        receiver
            .push_local_udp(&payload, source, destination, 64, 0, InterfaceId(1), 1)
            .unwrap();
        payload.complete();
        pool.lock().begin_dying();

        let result = receiver.recv_local_datagram_from(32, 32, false, |_, _| Ok::<(), u8>(()));
        assert!(matches!(
            result,
            Err(DatagramCopyError::Socket(SocketError::Buffer))
        ));
        assert_eq!(receiver.rx.lock().as_ref().unwrap().len(), 1);
    }

    #[test]
    fn closed_cached_receiver_falls_back_to_sender_queue() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9004,
        };
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        receiver.close();
        sender.tx_notified.store(true, Ordering::Release);

        assert_eq!(sender.send(&[0x71], Some(destination), true, None), Ok(1));
        assert!(sender.has_pending_datagram_tx());
        assert!(receiver.rx.lock().as_ref().unwrap().is_empty());
    }

    #[test]
    fn full_cached_receiver_drops_datagram_without_worker_fallback() {
        let sender = facade();
        let receiver = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9005,
        };
        receiver.set_buffer_limits(None, Some(16 * 1024));
        install_test_local_datagram_route(&sender, Arc::clone(&receiver), destination);
        sender.tx_notified.store(true, Ordering::Release);
        let payload = alloc::vec![0x27; SOCKET_CHUNK_BYTES];
        for _ in 0..4 {
            assert_eq!(
                sender.send(&payload, Some(destination), true, None),
                Ok(payload.len())
            );
        }
        let before = receiver.rx.lock().as_ref().unwrap().bytes();
        assert_eq!(before, 16 * 1024);

        assert_eq!(
            sender.send(&payload, Some(destination), true, None),
            Ok(payload.len())
        );
        assert_eq!(receiver.rx.lock().as_ref().unwrap().bytes(), before);
        assert!(!sender.has_pending_datagram_tx());
        assert_eq!(receiver.take_rx_overflow(), 1);
    }

    #[test]
    fn udp_tx_lease_copies_ranges_across_chunks() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let payload = (0usize..5000)
            .map(|index| index.wrapping_mul(17) as u8)
            .collect::<Vec<_>>();
        let lease = facade.test_udp_tx_lease(&payload, destination);
        let mut output = [0u8; 1000];
        lease.copy_range(3500, &mut output).unwrap();
        assert_eq!(&output, &payload[3500..4500]);
    }

    #[test]
    fn tcp_tx_lease_copies_subranges() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 10,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        let payload = (0usize..8192)
            .map(|index| index.wrapping_mul(29) as u8)
            .collect::<Vec<_>>();
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());
        let lease = facade.take_stream_tx(payload.len()).unwrap();
        let mut output = [0u8; 700];
        lease.copy_range(3900, &mut output).unwrap();
        assert_eq!(&output, &payload[3900..4600]);
    }

    #[test]
    fn dma_tcp_ring_pins_retransmits_and_conserves_pool() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(4, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 12,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        assert!(facade.install_stream_tx_pool(Arc::clone(&pool)));
        let payload = alloc::vec![0x5a; SOCKET_CHUNK_BYTES + 300];
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());
        assert_eq!(pool.lock().available(), 2);

        let first = facade.take_stream_tx(payload.len()).unwrap();
        let first_chain = first.packet_chain().unwrap().unwrap();
        let retransmit = facade
            .retransmit_stream(first.start, usize::from(first.len))
            .unwrap();
        let retransmit_chain = retransmit.packet_chain().unwrap().unwrap();
        assert_eq!(first_chain.fragment_count(), 2);
        assert_eq!(retransmit_chain.fragment_count(), 2);
        assert_eq!(facade.acknowledge_stream(payload.len()), payload.len());
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 2);

        drop(first_chain);
        drop(retransmit_chain);
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 4);

        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());
        let aborted = facade.take_stream_tx(payload.len()).unwrap();
        let completion = aborted.packet_chain().unwrap().unwrap();
        facade.abort_stream_tx();
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 2);
        drop(completion);
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 4);
    }

    #[test]
    fn loopback_tcp_rx_keeps_an_independent_shared_tx_reference() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(2, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let sender = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 53,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        let receiver = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 54,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        assert!(sender.install_stream_tx_pool(Arc::clone(&pool)));
        let payload = alloc::vec![0x7c; 1024];
        assert_eq!(sender.test_push_stream_tx(&payload), payload.len());
        let lease = sender.take_stream_tx(payload.len()).unwrap();

        let commit = receiver
            .push_stream_rx_lease(&lease, 0, payload.len(), true)
            .unwrap();
        assert_eq!(commit.storage, StreamRxStorageKind::LoopbackShared);
        assert_eq!(sender.acknowledge_stream(payload.len()), payload.len());
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 1);

        let mut output = alloc::vec![0; payload.len()];
        assert_eq!(
            receiver
                .recv_stream(&mut output, false, true, false, true, None)
                .unwrap(),
            payload.len()
        );
        assert_eq!(output, payload);
        pool.lock().drain_remote();
        assert_eq!(pool.lock().available(), 2);
    }

    #[test]
    fn dma_tcp_writability_tracks_shared_pool_capacity() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(1, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 13,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        assert!(facade.install_stream_tx_pool(Arc::clone(&pool)));
        assert!(facade.stream_is_writable());

        let payload = alloc::vec![0x5a; SOCKET_CHUNK_BYTES];
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());
        assert!(!facade.stream_is_writable());

        let lease = facade.take_stream_tx(payload.len()).unwrap();
        assert_eq!(facade.acknowledge_stream(payload.len()), payload.len());
        assert!(facade.stream_is_writable());
        assert_eq!(pool.lock().available(), 1);
        assert_eq!(facade.test_push_stream_tx(&payload), payload.len());
        drop(lease);
    }

    #[test]
    fn dma_tcp_pool_capacity_is_handed_to_waiting_socket() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(1, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let first = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 14,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        let second = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 15,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        assert!(first.install_stream_tx_pool(Arc::clone(&pool)));
        assert!(second.install_stream_tx_pool(Arc::clone(&pool)));
        assert!(first.stream_is_writable());

        let payload = alloc::vec![0x5a; SOCKET_CHUNK_BYTES];
        assert_eq!(first.test_push_stream_tx(&payload), payload.len());
        assert!(!second.stream_is_writable());
        let lease = first.take_stream_tx(payload.len()).unwrap();
        let pool_key = first.stream_tx.lock().pool_key().unwrap();
        queue_dma_tx_pool_waiter(&second, pool_key);
        queue_dma_tx_pool_waiter(&second, pool_key);
        assert_eq!(
            DMA_TX_POOL_WAITERS
                .lock()
                .iter()
                .filter(|waiter| waiter
                    .facade
                    .upgrade()
                    .is_some_and(|facade| Arc::ptr_eq(&facade, &second)))
                .count(),
            1
        );
        assert_eq!(first.acknowledge_stream(payload.len()), payload.len());

        assert!(second.stream_is_writable());
        assert!(!first.stream_is_writable());
        assert_eq!(second.test_push_stream_tx(&payload), payload.len());
        drop(lease);
    }

    #[test]
    fn dma_tcp_pool_handoff_keeps_remaining_capacity_work_conserving() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(2, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let first = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 16,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        let second = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 17,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        assert!(first.install_stream_tx_pool(Arc::clone(&pool)));
        assert!(second.install_stream_tx_pool(Arc::clone(&pool)));

        let payload = alloc::vec![0x5a; 2 * SOCKET_CHUNK_BYTES];
        assert_eq!(first.test_push_stream_tx(&payload), payload.len());
        assert!(!second.stream_is_writable());
        let lease = first.take_stream_tx(payload.len()).unwrap();
        let pool_key = first.stream_tx.lock().pool_key().unwrap();
        queue_dma_tx_pool_waiter(&second, pool_key);
        assert_eq!(first.acknowledge_stream(payload.len()), payload.len());

        assert!(first.stream_is_writable());
        assert!(second.stream_is_writable());
        assert_eq!(
            second.test_push_stream_tx(&payload[..SOCKET_CHUNK_BYTES]),
            SOCKET_CHUNK_BYTES
        );
        assert_eq!(
            first.test_push_stream_tx(&payload[..SOCKET_CHUNK_BYTES]),
            SOCKET_CHUNK_BYTES
        );
        drop(lease);
    }

    #[test]
    fn dma_udp_completion_keeps_pool_alive_after_socket_close() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(2, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let weak = Arc::downgrade(&pool);
        let facade = facade();
        facade.install_datagram_tx_pool(Arc::clone(&pool));
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let payload = alloc::vec![0x33; SOCKET_CHUNK_BYTES + 1];
        {
            let mut tx = facade.tx.lock();
            let tx = tx.as_mut().unwrap();
            assert!(tx.push(&payload, destination, false, false).is_ok());
            assert_eq!(
                tx.push(&[1], destination, false, false),
                Err(SocketError::WouldBlock)
            );
        }
        let lease = facade.take_tx().unwrap();
        let completion = lease.packet_chain().unwrap().unwrap();
        lease.complete();
        facade.close();
        drop(facade);
        drop(pool);
        assert!(weak.upgrade().is_some());
        drop(completion);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn dma_udp_readiness_tracks_pool_exhaustion() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(1, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let facade = facade();
        facade.install_datagram_tx_pool(Arc::clone(&pool));
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        facade
            .tx
            .lock()
            .as_mut()
            .unwrap()
            .push(&[0x5a; SOCKET_CHUNK_BYTES], destination, false, false)
            .unwrap();
        facade.refresh_tx_readiness();
        assert!(!facade.readiness().0.contains(Readiness::WRITABLE));

        facade.take_tx().unwrap().complete();
        assert!(facade.readiness().0.contains(Readiness::WRITABLE));
    }

    #[test]
    fn socket_wait_only_observes_requested_or_terminal_readiness() {
        assert!(socket_wait_observable(
            Readiness::WRITABLE,
            Readiness::WRITABLE
        ));
        assert!(socket_wait_observable(
            Readiness::ERROR,
            Readiness::WRITABLE
        ));
        assert!(socket_wait_observable(
            Readiness::HANGUP,
            Readiness::READABLE
        ));
        assert!(!socket_wait_observable(
            Readiness::READABLE,
            Readiness::WRITABLE
        ));
    }

    #[test]
    fn dma_udp_completion_wakes_another_pool_waiter() {
        let pool = Arc::new(Mutex::new(
            NetBufPool::new_heap(1, SOCKET_CHUNK_BYTES).unwrap(),
        ));
        let first = facade();
        let second = facade();
        first.install_datagram_tx_pool(Arc::clone(&pool));
        second.install_datagram_tx_pool(Arc::clone(&pool));
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        first
            .tx
            .lock()
            .as_mut()
            .unwrap()
            .push(&[0x5a; SOCKET_CHUNK_BYTES], destination, false, false)
            .unwrap();
        second.refresh_tx_readiness();
        assert!(!second.readiness().0.contains(Readiness::WRITABLE));
        let pool_key = first.tx.lock().as_ref().unwrap().pool_key().unwrap();
        queue_dma_tx_pool_waiter(&second, pool_key);

        first.take_tx().unwrap().complete();
        assert!(second.readiness().0.contains(Readiness::WRITABLE));
    }

    #[test]
    fn full_tx_ring_rejects_whole_datagram() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        facade.tx.lock().as_mut().unwrap().limit = 16 * 1024;
        assert!(
            facade
                .tx
                .lock()
                .as_mut()
                .unwrap()
                .push(&[1; 16 * 1024], destination, false, false)
                .is_ok()
        );
        assert_eq!(
            facade
                .tx
                .lock()
                .as_mut()
                .unwrap()
                .push(&[2], destination, false, false),
            Err(SocketError::WouldBlock)
        );
    }

    #[test]
    fn udp_capacity_is_checked_for_the_whole_datagram() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        let mut tx = facade.tx.lock();
        let tx = tx.as_mut().unwrap();
        tx.limit = 16 * 1024;
        for _ in 0..16 {
            tx.push(&[1; 1000], destination, false, false).unwrap();
        }
        assert!(tx.can_push_len(384));
        assert!(!tx.can_push_len(1000));
    }

    #[test]
    fn udp_send_buffer_grows_to_hard_limit_without_partial_datagram() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        facade.set_buffer_limits(Some(UDP_BUFFER_HARD_LIMIT), None);
        assert_eq!(facade.buffer_limits().0, UDP_BUFFER_HARD_LIMIT);
        for _ in 0..8 {
            assert!(
                facade
                    .tx
                    .lock()
                    .as_mut()
                    .unwrap()
                    .push(&[7; MAX_UDP4_PAYLOAD], destination, false, false)
                    .is_ok()
            );
        }
        assert_eq!(
            facade.tx.lock().as_mut().unwrap().push(
                &[7; MAX_UDP4_PAYLOAD],
                destination,
                false,
                false
            ),
            Err(SocketError::WouldBlock)
        );
    }

    #[test]
    fn error_queue_is_fifo_and_drops_newest_when_full() {
        let facade = facade();
        for index in 0..33 {
            facade.set_transport_error(
                crate::transport::TransportControlError::PacketTooBig { mtu: 1200 + index },
                None,
            );
        }
        assert_eq!(facade.error_queue_overflow(), 1);
        let first = facade.take_error_record().unwrap();
        assert_eq!(first.info, 1200);
        assert_eq!(first.sequence, 1);
        for expected in 1201..1232 {
            assert_eq!(facade.take_error_record().unwrap().info, expected);
        }
        assert!(facade.take_error_record().is_none());
        assert!(!facade.readiness().0.contains(Readiness::ERROR));
    }

    #[test]
    fn notsent_lowat_controls_stream_writable_readiness() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 9,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        facade.set_tcp_notsent_lowat(4);
        assert_eq!(facade.test_push_stream_tx(b"abcdefgh"), 8);
        facade.refresh_tx_readiness();
        assert!(!facade.readiness().0.contains(Readiness::WRITABLE));
        assert_eq!(facade.take_stream_tx(5).unwrap().len, 5);
        assert!(facade.readiness().0.contains(Readiness::WRITABLE));
    }

    #[test]
    fn clearing_tcp_more_retags_a_completed_stream_tail() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 11,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        facade.update_tcp_info(1, 0, 0, 0, 1460, 0, 0, 0, 0);
        facade.set_tcp_more(true);
        assert_eq!(facade.test_push_stream_tx(b"pending"), 7);
        facade.tx_generation.store(7, Ordering::Release);
        facade.tx_notified.store(true, Ordering::Release);
        facade.finish_stream_tx_drain(7);

        facade.set_tcp_more(false);

        assert_eq!(facade.stream_tx_generation(), 8);
    }

    #[test]
    fn clearing_tcp_more_does_not_republish_completed_full_segments() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 12,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        facade.update_tcp_info(1, 0, 0, 0, 1460, 0, 0, 0, 0);
        facade.set_tcp_more(true);
        assert_eq!(facade.test_push_stream_tx(&[1; 2048]), 2048);
        facade.tx_generation.store(9, Ordering::Release);
        facade.tx_notified.store(true, Ordering::Release);
        facade.finish_stream_tx_drain(9);

        facade.set_tcp_more(false);

        assert_eq!(facade.stream_tx_generation(), 9);
        assert!(!facade.tx_notified.load(Ordering::Acquire));
    }

    #[test]
    fn completed_stream_generation_does_not_publish_duplicate_work() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter: 99,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        facade.tx_generation.store(7, Ordering::Release);
        facade.tx_notified.store(true, Ordering::Release);

        facade.finish_stream_tx_drain(7);

        assert!(!facade.tx_notified.load(Ordering::Acquire));
        assert_eq!(facade.stream_tx_generation(), 7);
        assert_eq!(facade.tx_completed_generation.load(Ordering::Acquire), 7);
    }
}
