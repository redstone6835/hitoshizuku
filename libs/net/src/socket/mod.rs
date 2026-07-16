//! 跨 CPU 稳定 socket facade、UDP 数据环与精确 readiness。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};

use sched::{TaskState, WaitQueue};
use spin::{Mutex, RwLock};

use crate::control::BindOptions;
use crate::device::boot_config;
use crate::{AddressFamily, Endpoint, FlowId, InterfaceId, ShardId, SocketId};

const UDP_RING_ENTRIES: usize = 256;
const UDP_BUFFER_BYTES: usize = 128 * 1024;
const UDP_BUFFER_HARD_LIMIT: usize = 512 * 1024;
const SOCKET_CHUNK_BYTES: usize = 4096;
const MAX_DATAGRAM_CHUNKS: usize = 17;
const MAX_UDP_PAYLOAD: usize = 65_507;

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);
static SOCKET_RUNTIME: RwLock<Option<&'static dyn SocketRuntime>> = RwLock::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerRef {
    Unassigned,
    Flow {
        shard: ShardId,
        flow: FlowId,
        generation: u32,
    },
    Closed {
        generation: u32,
    },
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
    },
}

pub trait SocketRuntime: Send + Sync {
    fn submit_control(&self, command: SocketCommand) -> Result<(), SocketCommand>;
    fn notify_tx(&self, facade: Arc<SocketFacade>);
    fn notify_lifecycle(&self, facade: Arc<SocketFacade>);
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

pub fn new_socket_facade(family: AddressFamily) -> Result<Arc<SocketFacade>, SocketError> {
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
    )))
}

struct TxEntry {
    generation: u32,
    chunks: [u8; MAX_DATAGRAM_CHUNKS],
    chunk_count: u8,
    len: u16,
    destination: Endpoint,
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

    fn push(&mut self, payload: &[u8], destination: Endpoint) -> Result<(), SocketError> {
        if payload.len() > MAX_UDP_PAYLOAD {
            return Err(SocketError::MessageTooLarge);
        }
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
            completed: false,
        })
    }

    fn copy_out(
        &self,
        slot: u16,
        generation: u32,
        output: &mut [u8],
    ) -> Result<usize, SocketError> {
        let entry = self.entries[usize::from(slot)]
            .as_ref()
            .filter(|entry| entry.generation == generation)
            .ok_or(SocketError::Closed)?;
        if output.len() < usize::from(entry.len) {
            return Err(SocketError::Buffer);
        }
        let mut copied = 0usize;
        for chunk in entry.chunks.iter().take(usize::from(entry.chunk_count)) {
            let len = (usize::from(entry.len) - copied).min(SOCKET_CHUNK_BYTES);
            let offset = usize::from(*chunk) * SOCKET_CHUNK_BYTES;
            output[copied..copied + len].copy_from_slice(&self.arena[offset..offset + len]);
            copied += len;
        }
        Ok(copied)
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
            .copy_out(self.slot, self.generation, output)
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
    pub rx_timestamp_ns: u64,
    pub truncated: bool,
}

pub struct SocketFacade {
    id: SocketId,
    family: AddressFamily,
    generation: AtomicU32,
    owner: Mutex<OwnerRef>,
    local: Mutex<Option<Endpoint>>,
    peer: Mutex<Option<Endpoint>>,
    tx: Mutex<TxRing>,
    rx: Mutex<RxRing>,
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
    closing: AtomicBool,
    read_shutdown: AtomicBool,
    write_shutdown: AtomicBool,
    pending_error: AtomicU32,
    interface: Mutex<Option<InterfaceId>>,
}

impl SocketFacade {
    fn new(id: SocketId, family: AddressFamily) -> Self {
        Self {
            id,
            family,
            generation: AtomicU32::new(1),
            owner: Mutex::new(OwnerRef::Unassigned),
            local: Mutex::new(None),
            peer: Mutex::new(None),
            tx: Mutex::new(TxRing::new()),
            rx: Mutex::new(RxRing::new()),
            readiness: AtomicU16::new(Readiness::WRITABLE.0),
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
            closing: AtomicBool::new(false),
            read_shutdown: AtomicBool::new(false),
            write_shutdown: AtomicBool::new(false),
            pending_error: AtomicU32::new(0),
            interface: Mutex::new(None),
        }
    }

    pub const fn id(&self) -> SocketId {
        self.id
    }

    pub const fn family(&self) -> AddressFamily {
        self.family
    }

    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
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
        let _control = self.control_lock.lock();
        if self.peer_endpoint().is_some() {
            return Err(SocketError::AlreadyConnected);
        }
        let sequence = self.next_control_sequence();
        let command = SocketCommand::Connect {
            facade: Arc::clone(self),
            sequence,
            generation: self.generation(),
            peer,
            interface,
            options,
        };
        socket_runtime()?
            .submit_control(command)
            .map_err(|_| SocketError::RuntimeBusy)?;
        self.wait_control(sequence)
    }

    pub fn send(
        self: &Arc<Self>,
        payload: &[u8],
        destination: Option<Endpoint>,
        nonblocking: bool,
        deadline_ns: Option<u64>,
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
        loop {
            let was_empty = {
                let mut tx = self.tx.lock();
                let was_empty = tx.is_empty();
                match tx.push(payload, destination) {
                    Ok(()) => was_empty,
                    Err(SocketError::WouldBlock) if !nonblocking => {
                        drop(tx);
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
        self.tx.lock().take(Arc::clone(self))
    }

    pub fn finish_tx_drain(self: &Arc<Self>) {
        self.tx_notified.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        if !self.tx.lock().is_empty() && !self.tx_notified.swap(true, Ordering::AcqRel) {
            socket_runtime()
                .expect("socket runtime 必须保持安装")
                .notify_tx(Arc::clone(self));
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
            let was_empty = rx.is_empty();
            rx.push(datagram)?;
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
        let mut rx = self.rx.lock();
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
            rx_timestamp_ns: datagram.rx_timestamp_ns,
            truncated: copied < original_len,
        };
        if !peek {
            let datagram = rx.pop().unwrap();
            let empty = rx.is_empty();
            drop(rx);
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
            let mut rx = self.rx.lock();
            while let Some(datagram) = rx.pop() {
                drop(datagram);
            }
            drop(rx);
            self.clear_ready(Readiness::READABLE);
            self.set_ready(Readiness::READ_HANGUP);
        }
        if write {
            self.write_shutdown.store(true, Ordering::Release);
            self.clear_ready(Readiness::WRITABLE);
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
        if let Ok(runtime) = socket_runtime() {
            runtime.notify_lifecycle(Arc::clone(self));
        } else {
            *self.owner.lock() = OwnerRef::Closed { generation };
        }
    }

    pub fn complete_control(&self, sequence: u64, result: Result<(), SocketError>) {
        *self.control_result.lock() = Some((sequence, result));
        self.state_wait.wake_all();
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

    pub fn publish_closed(&self) {
        *self.owner.lock() = OwnerRef::Closed {
            generation: self.generation(),
        };
    }

    pub fn set_pending_error(&self, error: SocketError) {
        let code = error_code(error);
        let _ = self
            .pending_error
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
        self.set_ready(Readiness::ERROR);
        self.state_wait.wake_all();
    }

    pub fn take_pending_error(&self) -> Option<SocketError> {
        let code = self.pending_error.swap(0, Ordering::AcqRel);
        if code == 0 {
            return None;
        }
        self.clear_ready(Readiness::ERROR);
        if self.pending_error.load(Ordering::Acquire) != 0 {
            self.set_ready(Readiness::ERROR);
        }
        error_from_code(code)
    }

    pub fn set_buffer_limits(&self, send: Option<usize>, receive: Option<usize>) {
        if let Some(limit) = send {
            self.tx.lock().set_limit(limit);
            self.refresh_tx_readiness();
        }
        if let Some(limit) = receive {
            self.rx.lock().limit = limit.clamp(16 * 1024, UDP_BUFFER_HARD_LIMIT);
        }
    }

    pub fn buffer_limits(&self) -> (usize, usize) {
        (self.tx.lock().limit, self.rx.lock().limit)
    }

    fn refresh_tx_readiness(&self) {
        if self.write_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            self.clear_ready(Readiness::WRITABLE);
            return;
        }
        if self.tx.lock().writable() {
            self.set_ready(Readiness::WRITABLE);
            return;
        }
        self.clear_ready(Readiness::WRITABLE);
        fence(Ordering::SeqCst);
        if self.tx.lock().writable() {
            self.set_ready(Readiness::WRITABLE);
        }
    }

    fn refresh_rx_readiness(&self) {
        if self.read_shutdown.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            self.clear_ready(Readiness::READABLE);
            return;
        }
        if !self.rx.lock().is_empty() {
            self.set_ready(Readiness::READABLE);
            return;
        }
        self.clear_ready(Readiness::READABLE);
        fence(Ordering::SeqCst);
        if !self.rx.lock().is_empty() {
            self.set_ready(Readiness::READABLE);
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

fn error_from_code(code: u32) -> Option<SocketError> {
    let raw = code.checked_sub(1)?;
    const ALL: [SocketError; 18] = [
        SocketError::RuntimeUnavailable,
        SocketError::RuntimeBusy,
        SocketError::InvalidState,
        SocketError::AddressInUse,
        SocketError::AddressUnavailable,
        SocketError::NotConnected,
        SocketError::DestinationRequired,
        SocketError::AlreadyConnected,
        SocketError::WouldBlock,
        SocketError::Interrupted,
        SocketError::TimedOut,
        SocketError::MessageTooLarge,
        SocketError::ReadShutdown,
        SocketError::WriteShutdown,
        SocketError::Closed,
        SocketError::NetworkUnreachable,
        SocketError::HostUnreachable,
        SocketError::Buffer,
    ];
    ALL.get(raw as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IpAddr, Ipv4Addr};

    fn facade() -> Arc<SocketFacade> {
        Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 7,
                counter: 1,
            },
            AddressFamily::Ipv4,
        ))
    }

    #[test]
    fn udp_tx_ring_preserves_datagram_boundaries_and_reclaims_chunks() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        facade.tx.lock().push(b"first", destination).unwrap();
        facade.tx.lock().push(b"second", destination).unwrap();
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
        assert_eq!(tx.used_bytes, 0);
        assert_eq!(tx.free_chunk_count(), UDP_BUFFER_BYTES / SOCKET_CHUNK_BYTES);
    }

    #[test]
    fn full_tx_ring_rejects_whole_datagram() {
        let facade = facade();
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        facade.tx.lock().limit = 16 * 1024;
        assert!(facade.tx.lock().push(&[1; 16 * 1024], destination).is_ok());
        assert_eq!(
            facade.tx.lock().push(&[2], destination),
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
                    .push(&[7; MAX_UDP_PAYLOAD], destination)
                    .is_ok()
            );
        }
        assert_eq!(
            facade.tx.lock().push(&[7; MAX_UDP_PAYLOAD], destination),
            Err(SocketError::WouldBlock)
        );
    }
}
