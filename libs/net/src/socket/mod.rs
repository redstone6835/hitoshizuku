//! 跨 CPU 稳定 socket facade、协议数据环与精确 readiness。

mod listen_group;

pub use listen_group::ListenGroup;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};

use sched::{TaskState, WaitQueue};
use spin::{Mutex, RwLock};

use crate::IpAddr;
use crate::buf::PacketChain;
use crate::control::BindOptions;
use crate::device::boot_config;
use crate::{AddressFamily, Endpoint, FlowId, InterfaceId, ListenGroupId, ShardId, SocketId};

const UDP_RING_ENTRIES: usize = 256;
const UDP_BUFFER_BYTES: usize = 128 * 1024;
const UDP_BUFFER_HARD_LIMIT: usize = 512 * 1024;
const SOCKET_CHUNK_BYTES: usize = 4096;
const MAX_DATAGRAM_CHUNKS: usize = 17;
const MAX_UDP4_PAYLOAD: usize = 65_507;
const MAX_UDP6_PAYLOAD: usize = 65_527;
const TCP_BUFFER_BYTES: usize = 256 * 1024;
const TCP_BUFFER_HARD_LIMIT: usize = 1024 * 1024;
const TCP_INITIAL_CHUNKS: usize = 2;
const TCP_KEEPIDLE_DEFAULT_NS: u64 = 7_200_000_000_000;
const TCP_KEEPINTVL_DEFAULT_NS: u64 = 75_000_000_000;
const TCP_KEEPCNT_DEFAULT: u16 = 9;

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);
static SOCKET_RUNTIME: RwLock<Option<&'static dyn SocketRuntime>> = RwLock::new(None);

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
}

impl core::ops::BitOr for Readiness {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

pub trait ReadinessObserver: Send + Sync {
    fn readiness_changed(&self, readiness: Readiness, generation: u64);
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
    fn notify_tx(&self, facade: Arc<SocketFacade>);
    fn notify_lifecycle(&self, facade: Arc<SocketFacade>);
    fn update_multicast(
        &self,
        facade: Arc<SocketFacade>,
        membership: MulticastMembership,
        joined: bool,
    ) -> Result<(), SocketError>;
    fn interface_by_name(&self, name: &[u8]) -> Option<InterfaceId>;
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
    let boot = boot_config().ok_or(SocketError::RuntimeUnavailable)?;
    let boot_nonce = u64::from_le_bytes(boot.generation_nonce()[..8].try_into().unwrap());
    let counter = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    assert!(counter != 0, "SocketId 已耗尽");
    Ok(Arc::new(SocketFacade::new_with_protocol(
        SocketId {
            boot_nonce,
            counter,
        },
        family,
        SocketKind::Raw,
        protocol,
    )))
}

fn new_facade(family: AddressFamily, kind: SocketKind) -> Result<Arc<SocketFacade>, SocketError> {
    let boot = boot_config().ok_or(SocketError::RuntimeUnavailable)?;
    let boot_nonce = u64::from_le_bytes(boot.generation_nonce()[..8].try_into().unwrap());
    let counter = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    assert!(counter != 0, "SocketId 已耗尽");
    Ok(Arc::new(SocketFacade::new(
        SocketId {
            boot_nonce,
            counter,
        },
        family,
        kind,
    )))
}

struct ByteRing {
    arena: Box<[u8]>,
    head: usize,
    len: usize,
    limit: usize,
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
            .min(self.limit);
        let mut arena = alloc::vec![0; capacity].into_boxed_slice();
        self.copy_range(0, &mut arena[..self.len]);
        self.arena = arena;
        self.head = 0;
    }

    fn push(&mut self, input: &[u8]) -> usize {
        let len = input.len().min(self.available());
        self.grow_for(len);
        if len == 0 {
            return 0;
        }
        let tail = (self.head + self.len) % self.arena.len();
        let first = len.min(self.arena.len() - tail);
        self.arena[tail..tail + first].copy_from_slice(&input[..first]);
        self.arena[..len - first].copy_from_slice(&input[first..len]);
        self.len += len;
        len
    }

    fn copy_range(&self, offset: usize, output: &mut [u8]) -> bool {
        if offset.saturating_add(output.len()) > self.len {
            return false;
        }
        if output.is_empty() {
            return true;
        }
        let output_len = output.len();
        let start = (self.head + offset) % self.arena.len();
        let first = output_len.min(self.arena.len() - start);
        output[..first].copy_from_slice(&self.arena[start..start + first]);
        output[first..].copy_from_slice(&self.arena[..output_len - first]);
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
    bytes: ByteRing,
    base: u64,
    sent: usize,
}

impl StreamTxRing {
    fn new() -> Self {
        Self {
            bytes: ByteRing::new(),
            base: 0,
            sent: 0,
        }
    }

    fn push(&mut self, input: &[u8]) -> usize {
        self.bytes.push(input)
    }

    fn take_unsent(&mut self, max_len: usize) -> Option<(u64, usize)> {
        let len = self.bytes.len.saturating_sub(self.sent).min(max_len);
        if len == 0 {
            return None;
        }
        let start = self.base + self.sent as u64;
        self.sent += len;
        Some((start, len))
    }

    fn copy_absolute(&self, start: u64, output: &mut [u8]) -> bool {
        let Some(offset) = start.checked_sub(self.base) else {
            return false;
        };
        self.bytes.copy_range(offset as usize, output)
    }

    fn contains(&self, start: u64, len: usize) -> bool {
        start
            .checked_sub(self.base)
            .and_then(|offset| (offset as usize).checked_add(len))
            .is_some_and(|end| end <= self.bytes.len)
    }

    fn acknowledge(&mut self, len: usize) -> usize {
        let consumed = self.bytes.consume(len.min(self.sent));
        self.base = self.base.saturating_add(consumed as u64);
        self.sent -= consumed;
        consumed
    }

    fn abort(&mut self) {
        self.bytes.clear();
        self.base = self.base.saturating_add(self.sent as u64);
        self.sent = 0;
    }
}

struct StreamRxRing {
    bytes: ByteRing,
    eof: bool,
}

impl StreamRxRing {
    fn new() -> Self {
        Self {
            bytes: ByteRing::new(),
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
}

struct TxEntry {
    generation: u32,
    chunks: [u8; MAX_DATAGRAM_CHUNKS],
    chunk_count: u8,
    len: u16,
    destination: Endpoint,
    dont_route: bool,
    confirm: bool,
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
        }
    }

    fn writable(&self) -> bool {
        !self.free_slots.is_empty()
            && self.free_chunks.iter().any(|word| *word != 0)
            && self.used_bytes < self.limit
    }

    fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    fn push(
        &mut self,
        payload: &[u8],
        destination: Endpoint,
        dont_route: bool,
        confirm: bool,
    ) -> Result<(), SocketError> {
        let chunk_count = payload.len().div_ceil(SOCKET_CHUNK_BYTES);
        if chunk_count > MAX_DATAGRAM_CHUNKS
            || self.used_bytes.saturating_add(payload.len()) > self.limit
            || self.free_slots.is_empty()
            || self.free_chunk_count() < chunk_count
        {
            return Err(SocketError::WouldBlock);
        }
        let slot = self.free_slots.pop().unwrap();
        let mut chunks = [0u8; MAX_DATAGRAM_CHUNKS];
        for chunk in chunks.iter_mut().take(chunk_count) {
            *chunk = self.take_free_chunk().expect("TX chunk 计数失配");
        }
        for (part, chunk) in payload.chunks(SOCKET_CHUNK_BYTES).zip(chunks.iter()) {
            let offset = usize::from(*chunk) * SOCKET_CHUNK_BYTES;
            self.arena[offset..offset + part.len()].copy_from_slice(part);
        }
        let generation = self.generations[usize::from(slot)].wrapping_add(1).max(1);
        self.generations[usize::from(slot)] = generation;
        self.entries[usize::from(slot)] = Some(TxEntry {
            generation,
            chunks,
            chunk_count: chunk_count as u8,
            len: payload.len() as u16,
            destination,
            dont_route,
            confirm,
        });
        self.queued.push_back(slot);
        self.used_bytes += payload.len();
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

struct RxRing {
    entries: Box<[Option<crate::transport::UdpDatagram>]>,
    head: u16,
    tail: u16,
    len: u16,
    bytes: usize,
    limit: usize,
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
        }
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
            return Err(datagram);
        }
        self.bytes += usize::from(datagram.payload_len);
        self.entries[usize::from(self.tail)] = Some(datagram);
        self.tail = (self.tail + 1) % self.entries.len() as u16;
        self.len += 1;
        Ok(())
    }

    fn front(&self) -> Option<&crate::transport::UdpDatagram> {
        self.entries[usize::from(self.head)].as_ref()
    }

    fn pop(&mut self) -> Option<crate::transport::UdpDatagram> {
        if self.len == 0 {
            return None;
        }
        let datagram = self.entries[usize::from(self.head)].take();
        self.head = (self.head + 1) % self.entries.len() as u16;
        self.len -= 1;
        if let Some(datagram) = datagram.as_ref() {
            self.bytes = self.bytes.saturating_sub(usize::from(datagram.payload_len));
        }
        datagram
    }
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

    pub fn complete(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        let writable = {
            let mut tx = self.facade.tx.lock();
            let tx = tx.as_mut().expect("UDP facade 必须拥有 TX ring");
            tx.complete(self.slot, self.generation) && tx.writable()
        };
        if writable {
            self.facade.set_ready(Readiness::WRITABLE);
            self.facade.write_wait.wake_one_default();
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
    generation: AtomicU32,
    owner: Mutex<OwnerRef>,
    local: Mutex<Option<Endpoint>>,
    peer: Mutex<Option<Endpoint>>,
    tx: Mutex<Option<TxRing>>,
    rx: Mutex<Option<RxRing>>,
    stream_tx: Mutex<StreamTxRing>,
    stream_rx: Mutex<StreamRxRing>,
    listen_group: Mutex<Option<Arc<ListenGroup>>>,
    readiness: AtomicU16,
    readiness_generation: AtomicU64,
    observer: Mutex<Option<Weak<dyn ReadinessObserver>>>,
    read_wait: WaitQueue,
    write_wait: WaitQueue,
    accept_wait: WaitQueue,
    state_wait: WaitQueue,
    control_lock: sched::mutex::Mutex<()>,
    control_sequence: AtomicU64,
    control_result: Mutex<Option<(u64, Result<(), SocketError>)>>,
    tx_notified: AtomicBool,
    tx_generation: AtomicU64,
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
    raw_header_included: AtomicBool,
    free_bind: AtomicBool,
    v6_only: AtomicBool,
    ip_hop_limit: AtomicU16,
    ip_traffic_class: AtomicU16,
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
            generation: AtomicU32::new(1),
            owner: Mutex::new(OwnerRef::Unassigned),
            local: Mutex::new(None),
            peer: Mutex::new(None),
            tx: Mutex::new((kind != SocketKind::Stream).then(TxRing::new)),
            rx: Mutex::new((kind != SocketKind::Stream).then(RxRing::new)),
            stream_tx: Mutex::new(StreamTxRing::new()),
            stream_rx: Mutex::new(StreamRxRing::new()),
            listen_group: Mutex::new(None),
            readiness: AtomicU16::new(if kind != SocketKind::Stream {
                Readiness::WRITABLE.0
            } else {
                0
            }),
            readiness_generation: AtomicU64::new(1),
            observer: Mutex::new(None),
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
            accept_wait: WaitQueue::new(),
            state_wait: WaitQueue::new(),
            control_lock: sched::mutex::Mutex::new(()),
            control_sequence: AtomicU64::new(1),
            control_result: Mutex::new(None),
            tx_notified: AtomicBool::new(false),
            tx_generation: AtomicU64::new(0),
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
            raw_header_included: AtomicBool::new(false),
            free_bind: AtomicBool::new(false),
            v6_only: AtomicBool::new(false),
            ip_hop_limit: AtomicU16::new(64),
            ip_traffic_class: AtomicU16::new(0),
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

    pub fn add_multicast_membership(
        self: &Arc<Self>,
        membership: MulticastMembership,
    ) -> Result<(), SocketError> {
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
        *self.observer.lock() = Some(observer);
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
        let _control = self.control_lock.lock();
        if !matches!(self.owner(), OwnerRef::Unassigned) {
            return Err(SocketError::InvalidState);
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
        socket_runtime()?
            .submit_control(command)
            .map_err(|_| SocketError::RuntimeBusy)?;
        self.wait_control(sequence)
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
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        let _control = self.control_lock.lock();
        let sequence = self.next_control_sequence();
        let command = SocketCommand::Listen {
            facade: Arc::clone(self),
            sequence,
            generation: self.generation(),
            backlog,
        };
        socket_runtime()?
            .submit_control(command)
            .map_err(|_| SocketError::RuntimeBusy)?;
        self.wait_control(sequence)
    }

    pub fn accept(
        self: &Arc<Self>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<Arc<SocketFacade>, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        loop {
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
        if self.closing.load(Ordering::Acquire) {
            return Err(SocketError::Closed);
        }
        if self.write_shutdown.load(Ordering::Acquire) {
            return Err(SocketError::WriteShutdown);
        }
        let destination = destination
            .or_else(|| self.peer_endpoint())
            .ok_or(SocketError::DestinationRequired)?;
        let max_payload = match (self.kind, self.family) {
            (SocketKind::Raw, _) => u16::MAX as usize,
            (SocketKind::Datagram, AddressFamily::Ipv4) => MAX_UDP4_PAYLOAD,
            (SocketKind::Datagram, AddressFamily::Ipv6) => MAX_UDP6_PAYLOAD,
            (SocketKind::Stream, _) => return Err(SocketError::InvalidState),
        };
        if payload.len() > max_payload {
            return Err(SocketError::MessageTooLarge);
        }
        loop {
            let was_empty = {
                let mut tx_guard = self.tx.lock();
                let tx = tx_guard.as_mut().expect("UDP facade 必须拥有 TX ring");
                let was_empty = tx.is_empty();
                match tx.push(payload, destination, dont_route, confirm) {
                    Ok(()) => was_empty,
                    Err(SocketError::WouldBlock) if !nonblocking => {
                        drop(tx_guard);
                        self.wait_write(deadline_ns)?;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            };
            self.refresh_tx_readiness();
            if was_empty && !self.tx_notified.swap(true, Ordering::AcqRel) {
                socket_runtime()?.notify_tx(Arc::clone(self));
            }
            return Ok(payload.len());
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
                tx.sent < tx.bytes.len
            }
        };
        if pending && !self.tx_notified.swap(true, Ordering::AcqRel) {
            socket_runtime()
                .expect("socket runtime 必须保持安装")
                .notify_tx(Arc::clone(self));
        }
    }

    pub fn stream_tx_generation(&self) -> u64 {
        self.tx_generation.load(Ordering::Acquire)
    }

    pub fn finish_stream_tx_drain(self: &Arc<Self>, observed_generation: u64) {
        self.tx_notified.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        if self.tx_generation.load(Ordering::Acquire) != observed_generation
            && !self.tx_notified.swap(true, Ordering::AcqRel)
        {
            socket_runtime()
                .expect("socket runtime 必须保持安装")
                .notify_tx(Arc::clone(self));
        }
    }

    pub fn tcp_nodelay(&self) -> bool {
        self.tcp_nodelay.load(Ordering::Acquire)
    }

    pub fn set_tcp_nodelay(self: &Arc<Self>, enabled: bool) {
        let changed = self.tcp_nodelay.swap(enabled, Ordering::AcqRel) != enabled;
        if changed && enabled {
            self.notify_stream_pending();
        }
    }

    pub fn tcp_cork(&self) -> bool {
        self.tcp_cork.load(Ordering::Acquire)
    }

    pub fn set_tcp_cork(self: &Arc<Self>, enabled: bool) {
        let changed = self.tcp_cork.swap(enabled, Ordering::AcqRel) != enabled;
        if changed && !enabled {
            self.notify_stream_pending();
        }
    }

    pub fn tcp_more(&self) -> bool {
        self.tcp_more.load(Ordering::Acquire)
    }

    pub fn set_tcp_more(&self, enabled: bool) {
        self.tcp_more.store(enabled, Ordering::Release);
    }

    fn notify_stream_pending(self: &Arc<Self>) {
        if self.stream_unsent_len() == 0 {
            return;
        }
        self.tx_generation.fetch_add(1, Ordering::Release);
        if !self.tx_notified.swap(true, Ordering::AcqRel) {
            if let Ok(runtime) = socket_runtime() {
                runtime.notify_tx(Arc::clone(self));
            }
        }
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

    pub fn set_tcp_keepcount(self: &Arc<Self>, value: u16) {
        self.tcp_keepcount.store(value.max(1), Ordering::Release);
        self.notify_tcp_state_change();
    }

    pub fn take_receive_window_update(&self) -> bool {
        self.receive_window_update.swap(false, Ordering::AcqRel)
    }

    fn notify_tcp_state_change(self: &Arc<Self>) {
        if matches!(self.owner(), OwnerRef::Flow { .. }) {
            self.tx_generation.fetch_add(1, Ordering::Release);
            if !self.tx_notified.swap(true, Ordering::AcqRel)
                && let Ok(runtime) = socket_runtime()
            {
                runtime.notify_tx(Arc::clone(self));
            }
        }
    }

    pub fn send_stream(
        self: &Arc<Self>,
        payload: &[u8],
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
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
        if payload.is_empty() {
            return Ok(0);
        }
        loop {
            let copied = self.stream_tx.lock().push(payload);
            if copied != 0 {
                self.refresh_tx_readiness();
                self.tx_generation.fetch_add(1, Ordering::Release);
                if !self.tx_notified.swap(true, Ordering::AcqRel) {
                    socket_runtime()?.notify_tx(Arc::clone(self));
                }
                return Ok(copied);
            }
            self.refresh_tx_readiness();
            if nonblocking {
                return Err(SocketError::WouldBlock);
            }
            self.wait_write(deadline_ns)?;
        }
    }

    pub fn take_stream_tx(self: &Arc<Self>, max_len: usize) -> Option<TcpTxLease> {
        let (start, len) = self.stream_tx.lock().take_unsent(max_len)?;
        self.tcp_bytes_sent.fetch_add(len as u64, Ordering::Relaxed);
        self.refresh_tx_readiness();
        Some(TcpTxLease {
            facade: Arc::clone(self),
            start,
            len: len as u16,
        })
    }

    pub fn stream_unsent_len(&self) -> usize {
        let tx = self.stream_tx.lock();
        tx.bytes.len.saturating_sub(tx.sent)
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
    pub(crate) fn test_stream_tx_len(&self) -> usize {
        self.stream_tx.lock().bytes.len
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
        let writable = {
            let mut tx = self.stream_tx.lock();
            let consumed = tx.acknowledge(len);
            let writable = tx.bytes.available() != 0;
            (consumed, writable)
        };
        if writable.1 {
            self.set_ready(Readiness::WRITABLE);
            self.write_wait.wake_one_default();
        }
        writable.0
    }

    pub fn abort_stream_tx(&self) {
        self.stream_tx.lock().abort();
        self.refresh_tx_readiness();
    }

    pub fn push_stream_rx(&self, payload: &[u8]) -> Result<usize, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Ok(payload.len());
        }
        let was_empty;
        let copied;
        {
            let mut rx = self.stream_rx.lock();
            if rx.bytes.available() < payload.len() {
                return Err(SocketError::WouldBlock);
            }
            was_empty = rx.bytes.len == 0;
            copied = rx.bytes.push(payload);
        }
        debug_assert_eq!(copied, payload.len());
        self.tcp_bytes_received
            .fetch_add(copied as u64, Ordering::Relaxed);
        if was_empty && copied != 0 {
            self.set_ready(Readiness::READABLE);
            self.read_wait.wake_one_default();
        }
        Ok(copied)
    }

    pub fn push_stream_rx_chain(
        &self,
        packet: &PacketChain,
        offset: usize,
        len: usize,
    ) -> Result<usize, SocketError> {
        if self.kind != SocketKind::Stream {
            return Err(SocketError::InvalidState);
        }
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return Ok(len);
        }
        let was_empty;
        {
            let mut rx = self.stream_rx.lock();
            if rx.bytes.available() < len {
                return Err(SocketError::WouldBlock);
            }
            was_empty = rx.bytes.len == 0;
            let mut copied = 0usize;
            packet
                .for_each_slice(offset, len, |payload| {
                    copied += rx.bytes.push(payload);
                    Ok::<_, ()>(())
                })
                .map_err(|_| SocketError::Buffer)?;
            debug_assert_eq!(copied, len);
        }
        self.tcp_bytes_received
            .fetch_add(len as u64, Ordering::Relaxed);
        if was_empty && len != 0 {
            self.set_ready(Readiness::READABLE);
            self.read_wait.wake_one_default();
        }
        Ok(len)
    }

    pub fn publish_stream_eof(&self) {
        self.stream_rx.lock().eof = true;
        self.set_ready(Readiness::READABLE | Readiness::READ_HANGUP);
        self.read_wait.wake_all();
        self.state_wait.wake_all();
    }

    pub fn recv_stream(
        self: &Arc<Self>,
        output: &mut [u8],
        peek: bool,
        wait_all: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<usize, SocketError> {
        let mut total = 0usize;
        loop {
            let (copied, eof) = {
                let mut rx = self.stream_rx.lock();
                let copied = (output.len() - total).min(rx.bytes.len);
                if copied != 0 && !rx.bytes.copy_range(0, &mut output[total..total + copied]) {
                    return Err(SocketError::Buffer);
                }
                if copied != 0 && !peek {
                    rx.bytes.consume(copied);
                }
                (copied, rx.eof)
            };
            total += copied;
            if copied != 0 && !peek {
                self.receive_window_update.store(true, Ordering::Release);
                self.notify_tcp_state_change();
            }
            self.refresh_rx_readiness();
            if total != 0 && (!wait_all || total == output.len() || peek || eof) {
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
            self.read_wait.wake_one_default();
        }
        Ok(())
    }

    pub fn recv(
        &self,
        output: &mut [u8],
        peek: bool,
        report_original_len: bool,
        nonblocking: bool,
        deadline_ns: Option<u64>,
    ) -> Result<UdpReceive, SocketError> {
        loop {
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

    fn try_recv(
        &self,
        output: &mut [u8],
        peek: bool,
        report_original_len: bool,
    ) -> Result<Option<UdpReceive>, SocketError> {
        let mut rx_guard = self.rx.lock();
        let rx = rx_guard.as_mut().expect("UDP facade 必须拥有 RX ring");
        let Some(datagram) = rx.front() else {
            return Ok(None);
        };
        let original_len = usize::from(datagram.payload_len);
        let copied = original_len.min(output.len());
        datagram
            .packet
            .copy_out(usize::from(datagram.payload_offset), &mut output[..copied])
            .map_err(|_| SocketError::Buffer)?;
        let result = UdpReceive {
            len: if report_original_len {
                original_len
            } else {
                copied
            },
            original_len,
            source: datagram.source,
            destination: datagram.destination,
            ingress_interface: datagram.ingress_interface,
            hop_limit: datagram.hop_limit,
            traffic_class: datagram.traffic_class,
            rx_timestamp_ns: datagram.rx_timestamp_ns,
            truncated: copied < original_len,
        };
        if !peek {
            let datagram = rx.pop().unwrap();
            let empty = rx.is_empty();
            drop(rx_guard);
            drop(datagram);
            if empty {
                self.refresh_rx_readiness();
            } else {
                self.read_wait.wake_one_default();
            }
        }
        Ok(Some(result))
    }

    pub fn shutdown(self: &Arc<Self>, read: bool, write: bool) -> Result<(), SocketError> {
        if !read && !write {
            return Err(SocketError::InvalidState);
        }
        if read {
            self.read_shutdown.store(true, Ordering::Release);
            match self.kind {
                SocketKind::Datagram | SocketKind::Raw => {
                    let mut rx = self.rx.lock();
                    let rx = rx.as_mut().expect("UDP facade 必须拥有 RX ring");
                    while let Some(datagram) = rx.pop() {
                        drop(datagram);
                    }
                }
                SocketKind::Stream => self.stream_rx.lock().bytes.clear(),
            }
            self.clear_ready(Readiness::READABLE);
            self.set_ready(Readiness::READ_HANGUP);
        }
        if write {
            self.write_shutdown.store(true, Ordering::Release);
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
        if let Err(error) = result
            && self.kind == SocketKind::Stream
            && self.connect_pending.swap(false, Ordering::AcqRel)
        {
            self.publish_connection_error(error);
        }
        *self.control_result.lock() = Some((sequence, result));
        self.state_wait.wake_all();
    }

    pub fn begin_lifecycle_drain(&self) {
        self.lifecycle_notified.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
    }

    pub fn publish_binding(
        &self,
        owner: OwnerRef,
        local: Endpoint,
        peer: Option<Endpoint>,
        interface: Option<InterfaceId>,
    ) {
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
            self.state_wait.prepare_to_wait(&task, TaskState::Sleeping);
            if matches!(self.owner(), OwnerRef::Closed { .. }) {
                self.state_wait.finish_wait(&task);
                return Ok(());
            }
            if sched::operation::has_interrupting_signal(&task) {
                self.state_wait.finish_wait(&task);
                return Err(SocketError::Interrupted);
            }
            let armed = sched::register_sleep_deadline(&task, deadline_ns);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
            let task = sched::current_task();
            self.state_wait.finish_wait(&task);
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
            match self.kind {
                SocketKind::Datagram | SocketKind::Raw => {
                    self.rx
                        .lock()
                        .as_mut()
                        .expect("UDP facade 必须拥有 RX ring")
                        .limit = limit.clamp(16 * 1024, UDP_BUFFER_HARD_LIMIT)
                }
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
                    .limit,
            ),
            SocketKind::Stream => (
                self.stream_tx.lock().bytes.limit,
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
        tx.bytes.available() != 0
            && tx.bytes.len.saturating_sub(tx.sent)
                < self.tcp_notsent_lowat.load(Ordering::Acquire) as usize
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
            self.accept_wait.wake_one_default();
        }
    }

    fn next_control_sequence(&self) -> u64 {
        self.control_sequence.fetch_add(1, Ordering::Relaxed)
    }

    fn wait_control(&self, sequence: u64) -> Result<(), SocketError> {
        loop {
            if let Some(result) = self.take_control_result(sequence) {
                return result;
            }
            let task = sched::current_task();
            self.state_wait.prepare_to_wait(&task, TaskState::Sleeping);
            if let Some(result) = self.take_control_result(sequence) {
                self.state_wait.finish_wait(&task);
                return result;
            }
            if sched::operation::has_interrupting_signal(&task) {
                self.state_wait.finish_wait(&task);
                return Err(SocketError::Interrupted);
            }
            drop(task);
            sched::schedule_once(sched::now_ns_public());
            let task = sched::current_task();
            self.state_wait.finish_wait(&task);
        }
    }

    fn take_control_result(&self, sequence: u64) -> Option<Result<(), SocketError>> {
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

    fn wait_accept(&self, deadline_ns: Option<u64>) -> Result<(), SocketError> {
        self.wait_io(&self.accept_wait, Readiness::ACCEPTABLE, deadline_ns)
    }

    fn wait_io(
        &self,
        queue: &WaitQueue,
        readiness: Readiness,
        deadline_ns: Option<u64>,
    ) -> Result<(), SocketError> {
        let task = sched::current_task();
        let (_, observed_generation) = self.readiness();
        queue.prepare_to_wait(&task, TaskState::Sleeping);
        let (current, generation) = self.readiness();
        if current.contains(readiness) || generation != observed_generation {
            queue.finish_wait(&task);
            return Ok(());
        }
        if sched::operation::has_interrupting_signal(&task) {
            queue.finish_wait(&task);
            return Err(SocketError::Interrupted);
        }
        if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
            queue.finish_wait(&task);
            return Err(SocketError::TimedOut);
        }
        let armed =
            deadline_ns.is_some_and(|deadline| sched::register_sleep_deadline(&task, deadline));
        drop(task);
        sched::schedule_once(sched::now_ns_public());
        let task = sched::current_task();
        queue.finish_wait(&task);
        if armed {
            sched::cancel_sleep_deadline(&task);
        }
        if sched::operation::has_interrupting_signal(&task) {
            return Err(SocketError::Interrupted);
        }
        if deadline_ns.is_some_and(|deadline| sched::now_ns_public() >= deadline) {
            return Err(SocketError::TimedOut);
        }
        Ok(())
    }

    fn set_ready(&self, bits: Readiness) {
        self.update_ready(bits.0, 0);
    }

    fn clear_ready(&self, bits: Readiness) {
        self.update_ready(0, bits.0);
    }

    fn update_ready(&self, set: u16, clear: u16) {
        let mut current = self.readiness.load(Ordering::Acquire);
        loop {
            let next = (current | set) & !clear;
            if next == current {
                return;
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
                    let observer = self.observer.lock().as_ref().and_then(Weak::upgrade);
                    if let Some(observer) = observer {
                        observer.readiness_changed(Readiness(next), generation);
                    }
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

fn error_code(error: SocketError) -> u32 {
    error as u32 + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::PacketFragment;
    use crate::{IpAddr, Ipv4Addr};

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
            facade.stream_tx.lock().bytes.arena.len(),
            2 * SOCKET_CHUNK_BYTES
        );
        assert_eq!(
            facade.stream_rx.lock().bytes.arena.len(),
            2 * SOCKET_CHUNK_BYTES
        );
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
    fn stream_rx_copies_packet_chain_without_linearizing() {
        let facade = Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 5,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ));
        let mut packet = PacketChain::from_owned(alloc::vec![0, 1, 2, 3]);
        packet
            .push(PacketFragment::Owned(
                alloc::vec![4, 5, 6, 7].into_boxed_slice(),
            ))
            .unwrap_or_else(|_| unreachable!());

        assert_eq!(facade.push_stream_rx_chain(&packet, 2, 5), Ok(5));
        let mut output = [0u8; 8];
        assert_eq!(
            facade
                .recv_stream(&mut output, false, false, true, None)
                .unwrap(),
            5
        );
        assert_eq!(&output[..5], &[2, 3, 4, 5, 6]);
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
            facade.recv_stream(&mut output, false, false, true, None),
            Ok(4)
        );
        assert_eq!(&output[..4], b"data");
        assert_eq!(
            facade.recv_stream(&mut output, false, false, true, None),
            Err(SocketError::ConnectionReset)
        );
        assert_eq!(
            facade.recv_stream(&mut output, false, false, true, None),
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
}
