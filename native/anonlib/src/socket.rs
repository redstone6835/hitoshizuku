//! 不暴露 sockaddr 的类型化网络 endpoint。

use super::memory::MemoryObject;
use super::{OwnedHandle, Process, Status, abi, mrt_call};

pub enum SocketObject {}

/// socket.create 的固定配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketConfig {
    family: u16,
    kind: u16,
    protocol: u16,
}

impl SocketConfig {
    pub const fn tcp_ipv4() -> Self {
        Self {
            family: abi::MYGO_NETWORK_FAMILY_IPV4,
            kind: abi::MYGO_SOCKET_KIND_STREAM,
            protocol: 6,
        }
    }

    pub const fn udp_ipv4() -> Self {
        Self {
            family: abi::MYGO_NETWORK_FAMILY_IPV4,
            kind: abi::MYGO_SOCKET_KIND_DATAGRAM,
            protocol: 17,
        }
    }

    pub const fn tcp_ipv6() -> Self {
        Self {
            family: abi::MYGO_NETWORK_FAMILY_IPV6,
            kind: abi::MYGO_SOCKET_KIND_STREAM,
            protocol: 6,
        }
    }

    pub const fn udp_ipv6() -> Self {
        Self {
            family: abi::MYGO_NETWORK_FAMILY_IPV6,
            kind: abi::MYGO_SOCKET_KIND_DATAGRAM,
            protocol: 17,
        }
    }
}

/// Native 固定网络地址，不接受 Linux sockaddr 字节串。
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkAddress {
    raw: abi::MygoNetworkAddress,
}

impl NetworkAddress {
    pub const fn ipv4(address: [u8; 4], port: u16) -> Self {
        let mut bytes = [0; 16];
        bytes[0] = address[0];
        bytes[1] = address[1];
        bytes[2] = address[2];
        bytes[3] = address[3];
        Self {
            raw: abi::MygoNetworkAddress {
                family: abi::MYGO_NETWORK_FAMILY_IPV4,
                flags: 0,
                port,
                reserved0: 0,
                address: bytes,
                scope_id: 0,
                reserved1: 0,
            },
        }
    }

    pub const fn ipv6(address: [u8; 16], port: u16, scope_id: u32) -> Self {
        Self {
            raw: abi::MygoNetworkAddress {
                family: abi::MYGO_NETWORK_FAMILY_IPV6,
                flags: 0,
                port,
                reserved0: 0,
                address,
                scope_id,
                reserved1: 0,
            },
        }
    }
}

/// Native Socket capability；所有 buffer 都是显式 MemoryObject 区间。
pub struct Socket {
    handle: OwnedHandle<SocketObject>,
}

impl Process {
    pub fn create_socket(&self, config: SocketConfig) -> Result<Socket, Status> {
        if !abi::MYGO_HAS_socket_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let request = abi::MygoSocketCreateRequest {
            family: config.family,
            kind: config.kind,
            protocol: config.protocol,
            flags: 0,
            reserved: [0; 3],
        };
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_socket_create,
                self.raw(),
                &request as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| Socket { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }
}

impl Socket {
    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    fn address_operation(&self, slot: u64, address: &NetworkAddress) -> Result<(), Status> {
        let result = unsafe {
            mrt_call(
                slot,
                self.raw(),
                &address.raw as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }

    pub fn bind(&self, address: &NetworkAddress) -> Result<(), Status> {
        if !abi::MYGO_HAS_socket_bind {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        self.address_operation(abi::MYGO_SLOT_socket_bind, address)
    }

    pub fn connect(&self, address: &NetworkAddress) -> Result<(), Status> {
        if !abi::MYGO_HAS_socket_connect {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        self.address_operation(abi::MYGO_SLOT_socket_connect, address)
    }

    pub fn listen(&self, backlog: u32) -> Result<(), Status> {
        if !abi::MYGO_HAS_socket_listen {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_socket_listen,
                self.raw(),
                u64::from(backlog),
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }

    pub fn accept(&self, deadline_ns: u64) -> Result<Socket, Status> {
        if !abi::MYGO_HAS_socket_accept {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_socket_accept,
                self.raw(),
                deadline_ns,
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| Socket { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }

    pub fn send(
        &self,
        memory: &MemoryObject,
        offset: u64,
        length: u64,
        address: Option<&NetworkAddress>,
        deadline_ns: u64,
    ) -> Result<usize, Status> {
        self.transfer(memory, offset, length, address, deadline_ns, true)
    }

    pub fn receive(
        &self,
        memory: &MemoryObject,
        offset: u64,
        length: u64,
        address: Option<&mut NetworkAddress>,
        deadline_ns: u64,
    ) -> Result<usize, Status> {
        let address_pointer = address.map_or(0, |address| &mut address.raw as *mut _ as usize as u64);
        self.transfer_raw(memory, offset, length, address_pointer, deadline_ns, false)
    }

    fn transfer(
        &self,
        memory: &MemoryObject,
        offset: u64,
        length: u64,
        address: Option<&NetworkAddress>,
        deadline_ns: u64,
        send: bool,
    ) -> Result<usize, Status> {
        let address_pointer = address.map_or(0, |address| &address.raw as *const _ as usize as u64);
        self.transfer_raw(memory, offset, length, address_pointer, deadline_ns, send)
    }

    fn transfer_raw(
        &self,
        memory: &MemoryObject,
        offset: u64,
        length: u64,
        address_pointer: u64,
        deadline_ns: u64,
        send: bool,
    ) -> Result<usize, Status> {
        let (available, slot) = if send {
            (abi::MYGO_HAS_socket_send, abi::MYGO_SLOT_socket_send)
        } else {
            (abi::MYGO_HAS_socket_receive, abi::MYGO_SLOT_socket_receive)
        };
        if !available {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                slot,
                self.raw(),
                memory.raw(),
                offset,
                length,
                address_pointer,
                deadline_ns,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        usize::try_from(result.value0).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))
    }

    pub fn shutdown(&self, direction: u32) -> Result<(), Status> {
        if !abi::MYGO_HAS_socket_shutdown {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_socket_shutdown,
                self.raw(),
                u64::from(direction),
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }

    pub fn query(&self) -> Result<abi::MygoSocketInfo, Status> {
        if !abi::MYGO_HAS_socket_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoSocketInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_socket_query,
                self.raw(),
                &mut info as *mut _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(info)
        } else {
            Err(Status(result.status))
        }
    }
}
