//! Native Socket：不经过 fd table 的网络 endpoint capability。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use general::syscall::NativeCallOutcome;
use native_abi::wire::{NetworkAddress, SocketCreateRequest, SocketInfo};
use native_abi::{NativeHandle, ObjectInterface, Rights, status, wire};
use sched::Task;
use vfs::file::FileOps;
use vfs::net_socket::{InetRecvOptions, InetSendOptions, NetSocketFileOps};
use vfs::poll_source::PollSource;

use super::dispatch::native_return;
use super::memory::MemoryObject;
use super::operations::insert_native_handle;
use super::{
    KernelNativeObject, NativeProcessState, copy_user_value, copy_user_value_out, task_vm,
};

pub(crate) struct SocketObject {
    backend: Arc<NetSocketFileOps>,
    generation: AtomicU64,
    family: u16,
    kind: u16,
    protocol: u16,
}

impl SocketObject {
    pub(crate) fn poll_source(&self) -> Option<&PollSource> {
        self.backend.poll_source()
    }
}

pub(super) fn socket_create(
    task: &Arc<Task>,
    state: &NativeProcessState,
    object: &KernelNativeObject,
    user: u64,
) -> NativeCallOutcome {
    if !matches!(object, KernelNativeObject::SelfProcess) {
        return native_return(status::HANDLE_WRONG_INTERFACE, 0, 0);
    }
    let request = match copy_user_value::<SocketCreateRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    if request.flags != 0 || request.reserved != [0; 3] {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let family = match request.family {
        wire::NETWORK_FAMILY_IPV4 => vfs::addr::AF_INET,
        wire::NETWORK_FAMILY_IPV6 => vfs::addr::AF_INET6,
        _ => return native_return(status::SOCKET_INVALID_ADDRESS, 0, 0),
    };
    let kind = match request.kind {
        wire::SOCKET_KIND_STREAM => vfs::net_socket::SOCK_STREAM_PUB,
        wire::SOCKET_KIND_DATAGRAM => vfs::net_socket::SOCK_DGRAM_PUB,
        _ => return native_return(status::CORE_INVALID_ARGUMENT, 0, 0),
    };
    let backend = match vfs::net_socket::create_net_socket(family, kind, request.protocol, false) {
        Ok(backend) => Arc::new(backend),
        Err(error) => return map_socket_error(error),
    };
    let socket = Arc::new(SocketObject {
        backend,
        generation: AtomicU64::new(1),
        family: request.family,
        kind: request.kind,
        protocol: request.protocol,
    });
    insert_native_handle(
        state,
        KernelNativeObject::Socket(socket),
        ObjectInterface::Socket,
        Rights::BIND
            | Rights::CREATE
            | Rights::SEND
            | Rights::RECEIVE
            | Rights::MODIFY
            | Rights::OBSERVE
            | Rights::INSPECT
            | Rights::DUPLICATE,
    )
}

pub(super) fn socket_bind(task: &Arc<Task>, socket: &SocketObject, user: u64) -> NativeCallOutcome {
    let address = match read_address(task, user, socket.family) {
        Ok(address) => address,
        Err(error) => return native_return(error, 0, 0),
    };
    match socket.backend.bind(address.as_slice(), false) {
        Ok(()) => {
            socket.generation.fetch_add(1, Ordering::AcqRel);
            native_return(status::OK, 0, 0)
        }
        Err(error) => map_socket_error(error),
    }
}

pub(super) fn socket_connect(
    task: &Arc<Task>,
    socket: &SocketObject,
    user: u64,
) -> NativeCallOutcome {
    let address = match read_address(task, user, socket.family) {
        Ok(address) => address,
        Err(error) => return native_return(error, 0, 0),
    };
    match socket.backend.connect(address.as_slice(), false) {
        Ok(()) => {
            socket.generation.fetch_add(1, Ordering::AcqRel);
            native_return(status::OK, 0, 0)
        }
        Err(error) => map_socket_error(error),
    }
}

pub(super) fn socket_listen(socket: &SocketObject, backlog: u64) -> NativeCallOutcome {
    let Ok(backlog) = u32::try_from(backlog) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    match socket.backend.listen(backlog) {
        Ok(()) => {
            socket.generation.fetch_add(1, Ordering::AcqRel);
            native_return(status::OK, 0, 0)
        }
        Err(error) => map_socket_error(error),
    }
}

pub(super) fn socket_accept(
    task: &Arc<Task>,
    state: &NativeProcessState,
    socket: &SocketObject,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let accepted = loop {
        match socket.backend.accept(true, false) {
            Ok(accepted) => break accepted,
            Err(errno::Errno::EAGAIN) => {}
            Err(error) => return map_socket_error(error),
        }
        match wait_for_socket_event(task, socket, vfs::file::PollEvents::POLLIN, deadline_ns) {
            Ok(()) => {}
            Err(SocketWaitError::Timeout) => {
                return native_return(status::SOCKET_TIMEOUT, 0, 0);
            }
            Err(SocketWaitError::ExternalControl) => {
                return NativeCallOutcome::RetryExternalControl;
            }
            Err(SocketWaitError::Unavailable) => {
                return native_return(status::SOCKET_ERROR, 0, 0);
            }
        }
    };
    let object = Arc::new(SocketObject {
        family: socket.family,
        kind: socket.kind,
        protocol: socket.protocol,
        backend: Arc::new(accepted),
        generation: AtomicU64::new(1),
    });
    insert_native_handle(
        state,
        KernelNativeObject::Socket(object),
        ObjectInterface::Socket,
        Rights::SEND
            | Rights::RECEIVE
            | Rights::MODIFY
            | Rights::OBSERVE
            | Rights::INSPECT
            | Rights::DUPLICATE,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketWaitError {
    Timeout,
    ExternalControl,
    Unavailable,
}

fn wait_for_socket_event(
    task: &Arc<Task>,
    socket: &SocketObject,
    interest: vfs::file::PollEvents,
    deadline_ns: u64,
) -> Result<(), SocketWaitError> {
    const FALLBACK_RECHECK_NS: u64 = 10_000_000;

    if !socket.backend.poll(interest).is_empty() {
        return Ok(());
    }
    if super::operations::has_native_external_control(task) {
        return Err(SocketWaitError::ExternalControl);
    }
    let now = sched::now_ns_public();
    if deadline_ns != 0 && now >= deadline_ns {
        return Err(SocketWaitError::Timeout);
    }
    let sleeping = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping)
        || task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);
    if !sleeping {
        return if super::operations::has_native_external_control(task) {
            Err(SocketWaitError::ExternalControl)
        } else {
            Err(SocketWaitError::Unavailable)
        };
    }
    let registered = socket.backend.poll_add_waiter(task, interest);
    let wake_deadline = if deadline_ns != 0 {
        deadline_ns
    } else {
        now.saturating_add(FALLBACK_RECHECK_NS)
    };
    let deadline_armed = sched::register_sleep_deadline(task, wake_deadline);
    if !socket.backend.poll(interest).is_empty() {
        finish_socket_wait(task, socket, registered, deadline_armed);
        return Ok(());
    }
    if super::operations::has_native_external_control(task) {
        finish_socket_wait(task, socket, registered, deadline_armed);
        return Err(SocketWaitError::ExternalControl);
    }
    if registered || deadline_armed {
        sched::schedule_once(now);
    }
    finish_socket_wait(task, socket, registered, deadline_armed);
    if super::operations::has_native_external_control(task) {
        Err(SocketWaitError::ExternalControl)
    } else if deadline_ns != 0 && sched::now_ns_public() >= deadline_ns {
        Err(SocketWaitError::Timeout)
    } else {
        Ok(())
    }
}

fn finish_socket_wait(
    task: &Arc<Task>,
    socket: &SocketObject,
    registered: bool,
    deadline_armed: bool,
) {
    if registered {
        socket.backend.poll_remove_waiter(task);
    }
    if deadline_armed {
        sched::cancel_sleep_deadline(task);
    }
    super::operations::restore_native_task_after_wait(task);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn socket_send(
    task: &Arc<Task>,
    state: &NativeProcessState,
    socket: &SocketObject,
    memory_raw: u64,
    offset: u64,
    length: u64,
    address_user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let memory = match lookup_memory(state, memory_raw, Rights::READ) {
        Ok(memory) => memory,
        Err(error) => return native_return(error, 0, 0),
    };
    socket_send_memory(
        task,
        state,
        socket,
        &memory,
        offset,
        length,
        address_user,
        deadline_ns,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn socket_send_memory(
    task: &Arc<Task>,
    state: &NativeProcessState,
    socket: &SocketObject,
    memory: &Arc<MemoryObject>,
    offset: u64,
    length: u64,
    address_user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let _memory_access = match memory.begin_access() {
        Ok(access) => access,
        Err(error) => return native_return(error, 0, 0),
    };
    let address = if address_user == 0 {
        None
    } else {
        match read_address(task, address_user, socket.family) {
            Ok(address) => Some(address),
            Err(error) => return native_return(error, 0, 0),
        }
    };
    let user = match super::memory::resolve_mapped_range(task, state, memory, offset, length) {
        Ok(user) => user,
        Err(error) => return native_return(error, 0, 0),
    };
    let Ok(vm) = task_vm(task) else {
        return native_return(status::STREAM_FAULT, 0, 0);
    };
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    let result = unsafe {
        vm.with_user_read_slice(user, length, |buffer| {
            socket.backend.sendto(
                buffer,
                address.as_ref().map(|address| address.as_slice()),
                InetSendOptions {
                    nonblocking: false,
                    more: false,
                    dont_route: false,
                    confirm: false,
                    deadline_ns: (deadline_ns != 0).then_some(deadline_ns),
                },
            )
        })
    };
    match result {
        Ok(Ok(count)) => native_return(status::OK, count as u64, 0),
        Ok(Err(error)) => map_socket_error(error),
        Err(_) => native_return(status::STREAM_FAULT, 0, 0),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn socket_receive(
    task: &Arc<Task>,
    state: &NativeProcessState,
    socket: &SocketObject,
    memory_raw: u64,
    offset: u64,
    length: u64,
    address_user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let memory = match lookup_memory(state, memory_raw, Rights::WRITE) {
        Ok(memory) => memory,
        Err(error) => return native_return(error, 0, 0),
    };
    socket_receive_memory(
        task,
        state,
        socket,
        &memory,
        offset,
        length,
        address_user,
        deadline_ns,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn socket_receive_memory(
    task: &Arc<Task>,
    state: &NativeProcessState,
    socket: &SocketObject,
    memory: &Arc<MemoryObject>,
    offset: u64,
    length: u64,
    address_user: u64,
    deadline_ns: u64,
) -> NativeCallOutcome {
    let _memory_access = match memory.begin_access() {
        Ok(access) => access,
        Err(error) => return native_return(error, 0, 0),
    };
    let user = match super::memory::resolve_mapped_range(task, state, memory, offset, length) {
        Ok(user) => user,
        Err(error) => return native_return(error, 0, 0),
    };
    let Ok(vm) = task_vm(task) else {
        return native_return(status::STREAM_FAULT, 0, 0);
    };
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    let address_windows = if address_user == 0 {
        None
    } else {
        let Ok(address_user) = usize::try_from(address_user) else {
            return native_return(status::STREAM_FAULT, 0, 0);
        };
        match vm.pin_user_write_windows::<2>(address_user, core::mem::size_of::<NetworkAddress>()) {
            Ok(windows) => Some(windows),
            Err(_) => return native_return(status::STREAM_FAULT, 0, 0),
        }
    };
    let result = unsafe {
        vm.with_user_write_slice(user, length, |buffer| {
            socket.backend.recvfrom(
                buffer,
                InetRecvOptions {
                    nonblocking: false,
                    peek: false,
                    wait_all: false,
                    trunc: false,
                    defer_window_update: false,
                    deadline_ns: (deadline_ns != 0).then_some(deadline_ns),
                },
            )
        })
    };
    match result {
        Ok(Ok(received)) => {
            if let (Some(windows), Some(remote)) = (address_windows.as_ref(), received.remote) {
                let address = endpoint_to_native(remote);
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&address as *const NetworkAddress).cast::<u8>(),
                        core::mem::size_of::<NetworkAddress>(),
                    )
                };
                if windows.copy_from(0, bytes).is_err() {
                    return native_return(status::STREAM_FAULT, 0, 0);
                }
            }
            native_return(status::OK, received.len as u64, 0)
        }
        Ok(Err(error)) => map_socket_error(error),
        Err(_) => native_return(status::STREAM_FAULT, 0, 0),
    }
}

pub(super) fn socket_send_memory_nonblocking(
    socket: &SocketObject,
    memory: &MemoryObject,
    offset: u64,
    length: u64,
    address: Option<(&MemoryObject, u64)>,
) -> NativeCallOutcome {
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    let mut buffer = Vec::new();
    if buffer.try_reserve_exact(length).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    buffer.resize(length, 0);
    socket_send_memory_buffered(socket, memory, offset, address, &mut buffer)
}

pub(super) fn socket_send_memory_buffered(
    socket: &SocketObject,
    memory: &MemoryObject,
    offset: u64,
    address: Option<(&MemoryObject, u64)>,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    let address = match address {
        Some((memory, offset)) => match read_memory_address(memory, offset, socket.family) {
            Ok(address) => Some(address),
            Err(error) => return native_return(error, 0, 0),
        },
        None => None,
    };
    if let Err(error) = memory.read_into(offset, buffer) {
        return native_return(error, 0, 0);
    }
    match socket.backend.sendto(
        &buffer,
        address.as_ref().map(EncodedAddress::as_slice),
        InetSendOptions {
            nonblocking: true,
            more: false,
            dont_route: false,
            confirm: false,
            deadline_ns: None,
        },
    ) {
        Ok(count) if count <= buffer.len() => native_return(status::OK, count as u64, 0),
        Ok(_) => native_return(status::SOCKET_ERROR, 0, 0),
        Err(error) => map_socket_error(error),
    }
}

pub(super) fn socket_receive_memory_nonblocking(
    socket: &SocketObject,
    memory: &MemoryObject,
    offset: u64,
    length: u64,
    address: Option<(&MemoryObject, u64)>,
) -> NativeCallOutcome {
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    let mut buffer = Vec::new();
    if buffer.try_reserve_exact(length).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    buffer.resize(length, 0);
    socket_receive_memory_buffered(socket, memory, offset, address, &mut buffer)
}

pub(super) fn socket_receive_memory_buffered(
    socket: &SocketObject,
    memory: &MemoryObject,
    offset: u64,
    address: Option<(&MemoryObject, u64)>,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    let memory_access = match memory.begin_access() {
        Ok(access) => access,
        Err(error) => return native_return(error, 0, 0),
    };
    if let Err(error) = memory.validate_transfer(offset, buffer.len()) {
        return native_return(error, 0, 0);
    }
    let address_access = match address {
        Some((address_memory, address_offset)) => {
            let access = match address_memory.begin_access() {
                Ok(access) => access,
                Err(error) => return native_return(error, 0, 0),
            };
            if let Err(error) = address_memory
                .validate_transfer(address_offset, core::mem::size_of::<NetworkAddress>())
            {
                return native_return(error, 0, 0);
            }
            Some((access, address_offset))
        }
        None => None,
    };
    let received = match socket.backend.recvfrom(
        buffer,
        InetRecvOptions {
            nonblocking: true,
            peek: false,
            wait_all: false,
            trunc: false,
            defer_window_update: false,
            deadline_ns: None,
        },
    ) {
        Ok(received) if received.len <= buffer.len() => received,
        Ok(_) => return native_return(status::SOCKET_ERROR, 0, 0),
        Err(error) => return map_socket_error(error),
    };
    if let Err(error) = memory_access.write_from(offset, &buffer[..received.len]) {
        return native_return(error, 0, 0);
    }
    if let (Some(remote), Some((address_access, address_offset))) =
        (received.remote, address_access.as_ref())
    {
        let address = endpoint_to_native(remote);
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&address as *const NetworkAddress).cast::<u8>(),
                core::mem::size_of::<NetworkAddress>(),
            )
        };
        if let Err(error) = address_access.write_from(*address_offset, bytes) {
            return native_return(error, 0, 0);
        }
    }
    native_return(status::OK, received.len as u64, 0)
}

pub(super) fn socket_shutdown(socket: &SocketObject, direction: u64) -> NativeCallOutcome {
    let how = match direction as u32 {
        wire::SOCKET_SHUTDOWN_READ => 0,
        wire::SOCKET_SHUTDOWN_WRITE => 1,
        wire::SOCKET_SHUTDOWN_BOTH => 2,
        _ => return native_return(status::CORE_INVALID_ARGUMENT, 0, 0),
    };
    match socket.backend.shutdown(how) {
        Ok(()) => {
            socket.generation.fetch_add(1, Ordering::AcqRel);
            native_return(status::OK, 0, 0)
        }
        Err(error) => map_socket_error(error),
    }
}

pub(super) fn socket_query(
    task: &Arc<Task>,
    socket: &SocketObject,
    user: u64,
) -> NativeCallOutcome {
    let mut local = [0u8; 28];
    let local = socket
        .backend
        .getsockname(&mut local)
        .ok()
        .and_then(|length| sockaddr_to_native(&local[..length]).ok())
        .unwrap_or_default();
    let mut peer = [0u8; 28];
    let peer = socket
        .backend
        .getpeername(&mut peer)
        .ok()
        .and_then(|length| sockaddr_to_native(&peer[..length]).ok())
        .unwrap_or_default();
    let state = if peer.family != 0 {
        4
    } else if local.port != 0 {
        2
    } else {
        1
    };
    let info = SocketInfo {
        family: socket.family,
        kind: socket.kind,
        protocol: socket.protocol,
        state,
        flags: 0,
        reserved0: 0,
        local,
        peer,
        generation: socket.generation.load(Ordering::Acquire),
        reserved: [0; 2],
    };
    match copy_user_value_out(task, user, &info) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

fn lookup_memory(
    state: &NativeProcessState,
    memory_raw: u64,
    rights: Rights,
) -> Result<Arc<MemoryObject>, u32> {
    let memory = NativeHandle::from_raw(memory_raw);
    let object = {
        let handles = state.handles.lock();
        let entry = handles.lookup(memory, Some(ObjectInterface::MemoryObject), rights)?;
        let KernelNativeObject::MemoryObject(object) = entry.object else {
            return Err(status::HANDLE_WRONG_INTERFACE);
        };
        Arc::clone(object)
    };
    Ok(object)
}

fn read_address(task: &Arc<Task>, user: u64, family: u16) -> Result<EncodedAddress, u32> {
    let address = copy_user_value::<NetworkAddress>(task, user)?;
    encode_address(&address, family)
}

fn read_memory_address(
    memory: &MemoryObject,
    offset: u64,
    family: u16,
) -> Result<EncodedAddress, u32> {
    let mut address = NetworkAddress::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (&mut address as *mut NetworkAddress).cast::<u8>(),
            core::mem::size_of::<NetworkAddress>(),
        )
    };
    memory.read_into(offset, bytes)?;
    encode_address(&address, family)
}

fn encode_address(address: &NetworkAddress, family: u16) -> Result<EncodedAddress, u32> {
    if address.family != family
        || address.flags != 0
        || address.reserved0 != 0
        || address.reserved1 != 0
        || (family == wire::NETWORK_FAMILY_IPV4
            && (address.scope_id != 0 || address.address[4..].iter().any(|byte| *byte != 0)))
    {
        return Err(status::SOCKET_INVALID_ADDRESS);
    }
    let mut bytes = [0u8; 28];
    let length = if family == wire::NETWORK_FAMILY_IPV4 {
        bytes[0..2].copy_from_slice(&vfs::addr::AF_INET.to_ne_bytes());
        bytes[2..4].copy_from_slice(&address.port.to_be_bytes());
        bytes[4..8].copy_from_slice(&address.address[..4]);
        16
    } else {
        bytes[0..2].copy_from_slice(&vfs::addr::AF_INET6.to_ne_bytes());
        bytes[2..4].copy_from_slice(&address.port.to_be_bytes());
        bytes[8..24].copy_from_slice(&address.address);
        bytes[24..28].copy_from_slice(&address.scope_id.to_ne_bytes());
        28
    };
    Ok(EncodedAddress { bytes, length })
}

struct EncodedAddress {
    bytes: [u8; 28],
    length: usize,
}

impl EncodedAddress {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

fn endpoint_to_native(endpoint: net::Endpoint) -> NetworkAddress {
    match endpoint.addr {
        net::IpAddr::V4(address) => {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&address.0);
            NetworkAddress {
                family: wire::NETWORK_FAMILY_IPV4,
                port: endpoint.port,
                address: bytes,
                ..NetworkAddress::default()
            }
        }
        net::IpAddr::V6(address) => NetworkAddress {
            family: wire::NETWORK_FAMILY_IPV6,
            port: endpoint.port,
            address: address.0,
            ..NetworkAddress::default()
        },
    }
}

fn sockaddr_to_native(bytes: &[u8]) -> Result<NetworkAddress, u32> {
    vfs::addr::parse_inet_sockaddr(bytes)
        .map(endpoint_to_native)
        .map_err(|_| status::SOCKET_INVALID_ADDRESS)
}

fn map_socket_error(error: errno::Errno) -> NativeCallOutcome {
    let status = match error {
        errno::Errno::EAGAIN => status::SOCKET_WOULD_BLOCK,
        errno::Errno::ETIMEDOUT => status::SOCKET_TIMEOUT,
        errno::Errno::EADDRINUSE => status::SOCKET_ADDRESS_IN_USE,
        errno::Errno::ECONNREFUSED => status::SOCKET_CONNECTION_REFUSED,
        errno::Errno::ENETUNREACH | errno::Errno::EHOSTUNREACH => {
            status::SOCKET_NETWORK_UNREACHABLE
        }
        errno::Errno::EPIPE | errno::Errno::ECONNRESET | errno::Errno::ENOTCONN => {
            status::SOCKET_PEER_CLOSED
        }
        errno::Errno::EINVAL | errno::Errno::EAFNOSUPPORT => status::SOCKET_INVALID_ADDRESS,
        errno::Errno::EACCES | errno::Errno::EPERM => status::SECURITY_RIGHTS_DENIED,
        _ => status::SOCKET_ERROR,
    };
    native_return(status, 0, 0)
}
