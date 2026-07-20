//! `net.stack` socket 调用 ABI 与代际内套接字表。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use crate::control::BindOptions;
use crate::socket::{attach_socket_facade_generation, new_socket_readiness_relay};
use crate::{
    AddressFamily, Endpoint, InterfaceId, MulticastMembership, OwnerRef, ReadinessObserver,
    SocketError, SocketErrorRecord, SocketFacade, SocketId, SocketKind, TcpInfoSnapshot,
    new_raw_socket_facade, new_socket_facade, new_tcp_socket_facade,
};

pub const NET_STACK_SOCKET_CALL_ABI_VERSION: u16 = 1;
pub const NET_STACK_SOCKET_REQUEST_ABI_VERSION: u16 = 1;
pub const NET_STACK_SOCKET_CALL_RUST_ABI: &str = "fn(&mutnet::stack::NetStackSocketCallV1)->i32";

pub const NET_STACK_SOCKET_OP_PROBE: u32 = 1;
pub const NET_STACK_SOCKET_OP_CREATE: u32 = 2;
pub const NET_STACK_SOCKET_OP_CLOSE: u32 = 3;
pub const NET_STACK_SOCKET_OP_BIND: u32 = 4;
pub const NET_STACK_SOCKET_OP_CONNECT: u32 = 5;
pub const NET_STACK_SOCKET_OP_LISTEN: u32 = 6;
pub const NET_STACK_SOCKET_OP_ACCEPT: u32 = 7;
pub const NET_STACK_SOCKET_OP_SEND: u32 = 8;
pub const NET_STACK_SOCKET_OP_RECV: u32 = 9;
pub const NET_STACK_SOCKET_OP_GET_OPTION: u32 = 10;
pub const NET_STACK_SOCKET_OP_SET_OPTION: u32 = 11;
pub const NET_STACK_SOCKET_OP_QUERY: u32 = 12;
pub const NET_STACK_SOCKET_OP_SHUTDOWN: u32 = 13;
pub const NET_STACK_SOCKET_OP_TAKE_ERROR: u32 = 14;
pub const NET_STACK_SOCKET_OP_TAKE_ERROR_RECORD: u32 = 15;
pub const NET_STACK_SOCKET_OP_TCP_INFO: u32 = 16;
pub const NET_STACK_SOCKET_OP_TAKE_RX_OVERFLOW: u32 = 17;
pub const NET_STACK_SOCKET_OP_MULTICAST: u32 = 18;

pub const NET_STACK_SOCKET_FAMILY_IPV4: u8 = 4;
pub const NET_STACK_SOCKET_FAMILY_IPV6: u8 = 6;
pub const NET_STACK_SOCKET_KIND_DATAGRAM: u8 = 1;
pub const NET_STACK_SOCKET_KIND_STREAM: u8 = 2;
pub const NET_STACK_SOCKET_KIND_RAW: u8 = 3;

const DATAGRAM_BUFFER_DEFAULT: usize = 128 * 1024;
const STREAM_BUFFER_DEFAULT: usize = 256 * 1024;

/// 常驻 VFS socket 宿主与 `net.stack` 间一次同步调用的固定帧。
#[repr(C)]
pub struct NetStackSocketCallV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub opcode: u32,
    pub stack_generation: u64,
    pub ready: u8,
    pub quiesced: u8,
    pub committed: u8,
    pub reserved0: [u8; 5],
    pub request: *mut NetStackSocketRequestV1,
    pub reserved1: [u64; 1],
}
#[kernel_symbols::export]
impl NetStackSocketCallV1 {
    pub fn new(opcode: u32, stack_generation: u64) -> Self {
        Self {
            abi_version: NET_STACK_SOCKET_CALL_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            opcode,
            stack_generation,
            ready: 0,
            quiesced: 0,
            committed: 0,
            reserved0: [0; 5],
            request: core::ptr::null_mut(),
            reserved1: [0; 1],
        }
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackSocketCallV1.valid",
        contract = "kernel.net.stack-socket-call-frame@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn valid(
        &self,
        opcode: u32,
        stack_generation: u64,
        request: *mut NetStackSocketRequestV1,
    ) -> bool {
        self.abi_version == NET_STACK_SOCKET_CALL_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.opcode == opcode
            && self.stack_generation == stack_generation
            && stack_generation != 0
            && socket_opcode_valid(opcode)
            && self.ready <= 1
            && self.quiesced <= 1
            && self.committed <= 1
            && !(self.ready == 1 && self.quiesced == 1)
            && self.reserved0 == [0; 5]
            && self.reserved1 == [0; 1]
            && self.request == request
            && if opcode == NET_STACK_SOCKET_OP_PROBE {
                request.is_null()
            } else {
                !request.is_null()
            }
    }
}

const fn socket_opcode_valid(opcode: u32) -> bool {
    matches!(
        opcode,
        NET_STACK_SOCKET_OP_PROBE
            | NET_STACK_SOCKET_OP_CREATE
            | NET_STACK_SOCKET_OP_CLOSE
            | NET_STACK_SOCKET_OP_BIND
            | NET_STACK_SOCKET_OP_CONNECT
            | NET_STACK_SOCKET_OP_LISTEN
            | NET_STACK_SOCKET_OP_ACCEPT
            | NET_STACK_SOCKET_OP_SEND
            | NET_STACK_SOCKET_OP_RECV
            | NET_STACK_SOCKET_OP_GET_OPTION
            | NET_STACK_SOCKET_OP_SET_OPTION
            | NET_STACK_SOCKET_OP_QUERY
            | NET_STACK_SOCKET_OP_SHUTDOWN
            | NET_STACK_SOCKET_OP_TAKE_ERROR
            | NET_STACK_SOCKET_OP_TAKE_ERROR_RECORD
            | NET_STACK_SOCKET_OP_TCP_INFO
            | NET_STACK_SOCKET_OP_TAKE_RX_OVERFLOW
            | NET_STACK_SOCKET_OP_MULTICAST
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetStackSocketRefV1 {
    pub id: SocketId,
    pub generation: u32,
}

impl NetStackSocketRefV1 {
    pub const fn invalid() -> Self {
        Self {
            id: SocketId {
                boot_nonce: 0,
                counter: 0,
            },
            generation: 0,
        }
    }

    pub const fn valid(self) -> bool {
        self.id.boot_nonce != 0 && self.id.counter != 0 && self.generation != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NetStackSocketStateV1 {
    Unbound = 1,
    Bound = 2,
    Connected = 3,
    Listening = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStackSocketDescriptorV1 {
    pub socket: NetStackSocketRefV1,
    pub family: u8,
    pub kind: u8,
    pub protocol: u8,
    pub state: NetStackSocketStateV1,
    pub readiness: u16,
    pub readiness_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStackSocketSnapshotV1 {
    pub descriptor: NetStackSocketDescriptorV1,
    pub owner: OwnerRef,
    pub local: Option<Endpoint>,
    pub peer: Option<Endpoint>,
    pub interface: Option<InterfaceId>,
    pub read_shutdown: bool,
    pub write_shutdown: bool,
    pub tx_queued_bytes: u32,
    pub rx_queued_bytes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStackSocketRecvV1 {
    pub len: u32,
    pub original_len: u32,
    pub source: Option<Endpoint>,
    pub destination: Option<Endpoint>,
    pub interface: Option<InterfaceId>,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub rx_timestamp_ns: u64,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackSocketErrorV1 {
    InvalidArgument,
    NotSupported,
    NotFound,
    StaleGeneration,
    InvalidState,
    AddressInUse,
    AddressUnavailable,
    NotConnected,
    DestinationRequired,
    AlreadyConnected,
    InProgress,
    AlreadyInProgress,
    WouldBlock,
    MessageTooLarge,
    BufferFull,
    ReadShutdown,
    WriteShutdown,
    Quiesced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackSocketOptionV1 {
    ReuseAddress,
    ReusePort,
    V6Only,
    Broadcast,
    DontRoute,
    FreeBind,
    RawHeaderIncluded,
    ReceiveErrorsV4,
    ReceiveErrorsV6,
    TcpNoDelay,
    TcpCork,
    TcpQuickAck,
    TcpKeepAlive,
    IpHopLimit,
    IpTrafficClass,
    MulticastHops,
    MulticastLoop,
    MulticastInterface,
    SocketMark,
    SocketPriority,
    SendBuffer,
    ReceiveBuffer,
    TcpDeferAcceptNs,
    TcpNotSentLowat,
    TcpUserTimeoutNs,
    TcpKeepIdleNs,
    TcpKeepIntervalNs,
    TcpKeepCount,
    TcpMaxSegment,
    TcpMore,
    AbortiveClose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackSocketMulticastActionV1 {
    Add,
    Drop,
    Query,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackSocketOptionValueV1 {
    Bool(bool),
    U32(u32),
    U64(u64),
    I32(i32),
    Interface(Option<InterfaceId>),
}

pub enum NetStackSocketCommandV1 {
    Create {
        family: u8,
        kind: u8,
        protocol: u8,
        output: Option<Result<NetStackSocketDescriptorV1, NetStackSocketErrorV1>>,
    },
    Close {
        socket: NetStackSocketRefV1,
        output: Option<Result<(), NetStackSocketErrorV1>>,
    },
    Bind {
        socket: NetStackSocketRefV1,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        output: Option<Result<Endpoint, NetStackSocketErrorV1>>,
    },
    Connect {
        socket: NetStackSocketRefV1,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        output: Option<Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1>>,
    },
    Listen {
        socket: NetStackSocketRefV1,
        backlog: u32,
        output: Option<Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1>>,
    },
    Accept {
        socket: NetStackSocketRefV1,
        output: Option<Result<NetStackSocketDescriptorV1, NetStackSocketErrorV1>>,
    },
    Send {
        socket: NetStackSocketRefV1,
        data: *const u8,
        len: u32,
        destination: Option<Endpoint>,
        dont_route: bool,
        confirm: bool,
        output: Option<Result<u32, NetStackSocketErrorV1>>,
    },
    Recv {
        socket: NetStackSocketRefV1,
        data: *mut u8,
        capacity: u32,
        peek: bool,
        truncate: bool,
        output: Option<Result<NetStackSocketRecvV1, NetStackSocketErrorV1>>,
    },
    GetOption {
        socket: NetStackSocketRefV1,
        option: NetStackSocketOptionV1,
        output: Option<Result<NetStackSocketOptionValueV1, NetStackSocketErrorV1>>,
    },
    SetOption {
        socket: NetStackSocketRefV1,
        option: NetStackSocketOptionV1,
        value: NetStackSocketOptionValueV1,
        output: Option<Result<(), NetStackSocketErrorV1>>,
    },
    Query {
        socket: NetStackSocketRefV1,
        output: Option<Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1>>,
    },
    Shutdown {
        socket: NetStackSocketRefV1,
        read: bool,
        write: bool,
        output: Option<Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1>>,
    },
    TakeError {
        socket: NetStackSocketRefV1,
        output: Option<Result<Option<SocketError>, NetStackSocketErrorV1>>,
    },
    TakeErrorRecord {
        socket: NetStackSocketRefV1,
        output: Option<Result<Option<SocketErrorRecord>, NetStackSocketErrorV1>>,
    },
    TcpInfo {
        socket: NetStackSocketRefV1,
        output: Option<Result<TcpInfoSnapshot, NetStackSocketErrorV1>>,
    },
    TakeRxOverflow {
        socket: NetStackSocketRefV1,
        output: Option<Result<u32, NetStackSocketErrorV1>>,
    },
    Multicast {
        socket: NetStackSocketRefV1,
        action: NetStackSocketMulticastActionV1,
        membership: Option<MulticastMembership>,
        output: Option<Result<bool, NetStackSocketErrorV1>>,
    },
}

impl NetStackSocketCommandV1 {
    pub const fn opcode(&self) -> u32 {
        match self {
            Self::Create { .. } => NET_STACK_SOCKET_OP_CREATE,
            Self::Close { .. } => NET_STACK_SOCKET_OP_CLOSE,
            Self::Bind { .. } => NET_STACK_SOCKET_OP_BIND,
            Self::Connect { .. } => NET_STACK_SOCKET_OP_CONNECT,
            Self::Listen { .. } => NET_STACK_SOCKET_OP_LISTEN,
            Self::Accept { .. } => NET_STACK_SOCKET_OP_ACCEPT,
            Self::Send { .. } => NET_STACK_SOCKET_OP_SEND,
            Self::Recv { .. } => NET_STACK_SOCKET_OP_RECV,
            Self::GetOption { .. } => NET_STACK_SOCKET_OP_GET_OPTION,
            Self::SetOption { .. } => NET_STACK_SOCKET_OP_SET_OPTION,
            Self::Query { .. } => NET_STACK_SOCKET_OP_QUERY,
            Self::Shutdown { .. } => NET_STACK_SOCKET_OP_SHUTDOWN,
            Self::TakeError { .. } => NET_STACK_SOCKET_OP_TAKE_ERROR,
            Self::TakeErrorRecord { .. } => NET_STACK_SOCKET_OP_TAKE_ERROR_RECORD,
            Self::TcpInfo { .. } => NET_STACK_SOCKET_OP_TCP_INFO,
            Self::TakeRxOverflow { .. } => NET_STACK_SOCKET_OP_TAKE_RX_OVERFLOW,
            Self::Multicast { .. } => NET_STACK_SOCKET_OP_MULTICAST,
        }
    }

    pub const fn allowed_while_quiesced(&self) -> bool {
        matches!(
            self,
            Self::Close { .. }
                | Self::Recv { .. }
                | Self::GetOption { .. }
                | Self::Query { .. }
                | Self::TakeError { .. }
                | Self::TakeErrorRecord { .. }
                | Self::TcpInfo { .. }
                | Self::TakeRxOverflow { .. }
        )
    }

    pub fn payload_binding(&self) -> Option<(usize, usize, bool)> {
        match self {
            Self::Send { data, len, .. } => Some((*data as usize, *len as usize, false)),
            Self::Recv { data, capacity, .. } => Some((*data as usize, *capacity as usize, true)),
            _ => None,
        }
    }

    fn complete_error(&mut self, error: NetStackSocketErrorV1) {
        match self {
            Self::Create { output, .. } => *output = Some(Err(error)),
            Self::Close { output, .. } => *output = Some(Err(error)),
            Self::Bind { output, .. } => *output = Some(Err(error)),
            Self::Connect { output, .. } => *output = Some(Err(error)),
            Self::Listen { output, .. } => *output = Some(Err(error)),
            Self::Accept { output, .. } => *output = Some(Err(error)),
            Self::Send { output, .. } => *output = Some(Err(error)),
            Self::Recv { output, .. } => *output = Some(Err(error)),
            Self::GetOption { output, .. } => *output = Some(Err(error)),
            Self::SetOption { output, .. } => *output = Some(Err(error)),
            Self::Query { output, .. } => *output = Some(Err(error)),
            Self::Shutdown { output, .. } => *output = Some(Err(error)),
            Self::TakeError { output, .. } => *output = Some(Err(error)),
            Self::TakeErrorRecord { output, .. } => *output = Some(Err(error)),
            Self::TcpInfo { output, .. } => *output = Some(Err(error)),
            Self::TakeRxOverflow { output, .. } => *output = Some(Err(error)),
            Self::Multicast { output, .. } => *output = Some(Err(error)),
        }
    }
}

#[repr(C)]
pub struct NetStackSocketRequestV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub opcode: u32,
    pub stack_generation: u64,
    pub request_id: u64,
    pub committed: u8,
    pub reserved0: [u8; 7],
    pub command: NetStackSocketCommandV1,
}

#[kernel_symbols::export]
impl NetStackSocketRequestV1 {
    pub fn new(stack_generation: u64, request_id: u64, command: NetStackSocketCommandV1) -> Self {
        Self {
            abi_version: NET_STACK_SOCKET_REQUEST_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            opcode: command.opcode(),
            stack_generation,
            request_id,
            committed: 0,
            reserved0: [0; 7],
            command,
        }
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackSocketRequestV1.valid_header",
        contract = "kernel.net.stack-socket-request@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn valid_header(&self, opcode: u32, stack_generation: u64, request_id: u64) -> bool {
        self.abi_version == NET_STACK_SOCKET_REQUEST_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.opcode == opcode
            && self.command.opcode() == opcode
            && self.stack_generation == stack_generation
            && stack_generation != 0
            && self.request_id == request_id
            && request_id != 0
            && self.committed <= 1
            && self.reserved0 == [0; 7]
    }
}

#[derive(Clone)]
struct SocketOptions {
    reuse_address: bool,
    reuse_port: bool,
    v6_only: bool,
    broadcast: bool,
    dont_route: bool,
    free_bind: bool,
    raw_header_included: bool,
    receive_errors_v4: bool,
    receive_errors_v6: bool,
    tcp_nodelay: bool,
    tcp_cork: bool,
    tcp_quick_ack: bool,
    tcp_keepalive: bool,
    ip_hop_limit: u32,
    ip_traffic_class: u32,
    multicast_hops: u32,
    multicast_loop: bool,
    multicast_interface: Option<InterfaceId>,
    socket_mark: u32,
    socket_priority: i32,
    send_buffer: usize,
    receive_buffer: usize,
    tcp_defer_accept_ns: u64,
    tcp_notsent_lowat: u32,
    tcp_user_timeout_ns: u64,
    tcp_keepidle_ns: u64,
    tcp_keepintvl_ns: u64,
    tcp_keepcount: u32,
    tcp_maxseg: u32,
    tcp_more: bool,
    abortive_close: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingControl {
    Bind {
        sequence: u64,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    },
    Listen {
        sequence: u64,
        backlog: u32,
    },
}

impl SocketOptions {
    fn new(kind: SocketKind) -> Self {
        let buffer = if kind == SocketKind::Stream {
            STREAM_BUFFER_DEFAULT
        } else {
            DATAGRAM_BUFFER_DEFAULT
        };
        Self {
            reuse_address: false,
            reuse_port: false,
            v6_only: false,
            broadcast: false,
            dont_route: false,
            free_bind: false,
            raw_header_included: false,
            receive_errors_v4: false,
            receive_errors_v6: false,
            tcp_nodelay: false,
            tcp_cork: false,
            tcp_quick_ack: false,
            tcp_keepalive: false,
            ip_hop_limit: 64,
            ip_traffic_class: 0,
            multicast_hops: 1,
            multicast_loop: true,
            multicast_interface: None,
            socket_mark: 0,
            socket_priority: 0,
            send_buffer: buffer,
            receive_buffer: buffer,
            tcp_defer_accept_ns: 0,
            tcp_notsent_lowat: u32::MAX,
            tcp_user_timeout_ns: 0,
            tcp_keepidle_ns: 7_200_000_000_000,
            tcp_keepintvl_ns: 75_000_000_000,
            tcp_keepcount: 9,
            tcp_maxseg: 0,
            tcp_more: false,
            abortive_close: false,
        }
    }

    fn get(&self, option: NetStackSocketOptionV1) -> NetStackSocketOptionValueV1 {
        use NetStackSocketOptionV1 as OptionId;
        match option {
            OptionId::ReuseAddress => NetStackSocketOptionValueV1::Bool(self.reuse_address),
            OptionId::ReusePort => NetStackSocketOptionValueV1::Bool(self.reuse_port),
            OptionId::V6Only => NetStackSocketOptionValueV1::Bool(self.v6_only),
            OptionId::Broadcast => NetStackSocketOptionValueV1::Bool(self.broadcast),
            OptionId::DontRoute => NetStackSocketOptionValueV1::Bool(self.dont_route),
            OptionId::FreeBind => NetStackSocketOptionValueV1::Bool(self.free_bind),
            OptionId::RawHeaderIncluded => {
                NetStackSocketOptionValueV1::Bool(self.raw_header_included)
            }
            OptionId::ReceiveErrorsV4 => NetStackSocketOptionValueV1::Bool(self.receive_errors_v4),
            OptionId::ReceiveErrorsV6 => NetStackSocketOptionValueV1::Bool(self.receive_errors_v6),
            OptionId::TcpNoDelay => NetStackSocketOptionValueV1::Bool(self.tcp_nodelay),
            OptionId::TcpCork => NetStackSocketOptionValueV1::Bool(self.tcp_cork),
            OptionId::TcpQuickAck => NetStackSocketOptionValueV1::Bool(self.tcp_quick_ack),
            OptionId::TcpKeepAlive => NetStackSocketOptionValueV1::Bool(self.tcp_keepalive),
            OptionId::IpHopLimit => NetStackSocketOptionValueV1::U32(self.ip_hop_limit),
            OptionId::IpTrafficClass => NetStackSocketOptionValueV1::U32(self.ip_traffic_class),
            OptionId::MulticastHops => NetStackSocketOptionValueV1::U32(self.multicast_hops),
            OptionId::MulticastLoop => NetStackSocketOptionValueV1::Bool(self.multicast_loop),
            OptionId::MulticastInterface => {
                NetStackSocketOptionValueV1::Interface(self.multicast_interface)
            }
            OptionId::SocketMark => NetStackSocketOptionValueV1::U32(self.socket_mark),
            OptionId::SocketPriority => NetStackSocketOptionValueV1::I32(self.socket_priority),
            OptionId::SendBuffer => NetStackSocketOptionValueV1::U32(self.send_buffer as u32),
            OptionId::ReceiveBuffer => NetStackSocketOptionValueV1::U32(self.receive_buffer as u32),
            OptionId::TcpDeferAcceptNs => {
                NetStackSocketOptionValueV1::U64(self.tcp_defer_accept_ns)
            }
            OptionId::TcpNotSentLowat => NetStackSocketOptionValueV1::U32(self.tcp_notsent_lowat),
            OptionId::TcpUserTimeoutNs => {
                NetStackSocketOptionValueV1::U64(self.tcp_user_timeout_ns)
            }
            OptionId::TcpKeepIdleNs => NetStackSocketOptionValueV1::U64(self.tcp_keepidle_ns),
            OptionId::TcpKeepIntervalNs => NetStackSocketOptionValueV1::U64(self.tcp_keepintvl_ns),
            OptionId::TcpKeepCount => NetStackSocketOptionValueV1::U32(self.tcp_keepcount),
            OptionId::TcpMaxSegment => NetStackSocketOptionValueV1::U32(self.tcp_maxseg),
            OptionId::TcpMore => NetStackSocketOptionValueV1::Bool(self.tcp_more),
            OptionId::AbortiveClose => NetStackSocketOptionValueV1::Bool(self.abortive_close),
        }
    }

    fn set(
        &mut self,
        option: NetStackSocketOptionV1,
        value: NetStackSocketOptionValueV1,
    ) -> Result<(), NetStackSocketErrorV1> {
        use NetStackSocketOptionV1 as OptionId;
        use NetStackSocketOptionValueV1::{Bool, I32, Interface, U32, U64};
        match (option, value) {
            (OptionId::ReuseAddress, Bool(value)) => self.reuse_address = value,
            (OptionId::ReusePort, Bool(value)) => self.reuse_port = value,
            (OptionId::V6Only, Bool(value)) => self.v6_only = value,
            (OptionId::Broadcast, Bool(value)) => self.broadcast = value,
            (OptionId::DontRoute, Bool(value)) => self.dont_route = value,
            (OptionId::FreeBind, Bool(value)) => self.free_bind = value,
            (OptionId::RawHeaderIncluded, Bool(value)) => self.raw_header_included = value,
            (OptionId::ReceiveErrorsV4, Bool(value)) => self.receive_errors_v4 = value,
            (OptionId::ReceiveErrorsV6, Bool(value)) => self.receive_errors_v6 = value,
            (OptionId::TcpNoDelay, Bool(value)) => self.tcp_nodelay = value,
            (OptionId::TcpCork, Bool(value)) => self.tcp_cork = value,
            (OptionId::TcpQuickAck, Bool(value)) => self.tcp_quick_ack = value,
            (OptionId::TcpKeepAlive, Bool(value)) => self.tcp_keepalive = value,
            (OptionId::IpHopLimit, U32(value)) if value <= u8::MAX as u32 => {
                self.ip_hop_limit = value
            }
            (OptionId::IpTrafficClass, U32(value)) if value <= u8::MAX as u32 => {
                self.ip_traffic_class = value
            }
            (OptionId::MulticastHops, U32(value)) if value <= u8::MAX as u32 => {
                self.multicast_hops = value
            }
            (OptionId::MulticastLoop, Bool(value)) => self.multicast_loop = value,
            (OptionId::MulticastInterface, Interface(value)) => self.multicast_interface = value,
            (OptionId::SocketMark, U32(value)) => self.socket_mark = value,
            (OptionId::SocketPriority, I32(value)) => self.socket_priority = value,
            (OptionId::SendBuffer, U32(value)) if value != 0 => self.send_buffer = value as usize,
            (OptionId::ReceiveBuffer, U32(value)) if value != 0 => {
                self.receive_buffer = value as usize
            }
            (OptionId::TcpDeferAcceptNs, U64(value)) => self.tcp_defer_accept_ns = value,
            (OptionId::TcpNotSentLowat, U32(value)) => self.tcp_notsent_lowat = value,
            (OptionId::TcpUserTimeoutNs, U64(value)) => self.tcp_user_timeout_ns = value,
            (OptionId::TcpKeepIdleNs, U64(value)) => self.tcp_keepidle_ns = value,
            (OptionId::TcpKeepIntervalNs, U64(value)) => self.tcp_keepintvl_ns = value,
            (OptionId::TcpKeepCount, U32(value)) if value != 0 && value <= u8::MAX as u32 => {
                self.tcp_keepcount = value
            }
            (OptionId::TcpMaxSegment, U32(value)) if value <= u16::MAX as u32 => {
                self.tcp_maxseg = value
            }
            (OptionId::TcpMore, Bool(value)) => self.tcp_more = value,
            (OptionId::AbortiveClose, Bool(value)) => self.abortive_close = value,
            _ => return Err(NetStackSocketErrorV1::InvalidArgument),
        }
        Ok(())
    }
}

struct SocketEntry {
    socket: NetStackSocketRefV1,
    family: AddressFamily,
    kind: SocketKind,
    protocol: u8,
    facade: Arc<SocketFacade>,
    _readiness_relay: Arc<dyn ReadinessObserver>,
    read_shutdown: bool,
    write_shutdown: bool,
    pending_control: Option<PendingControl>,
    options: SocketOptions,
}

impl SocketEntry {
    fn descriptor(&self) -> NetStackSocketDescriptorV1 {
        let (readiness, readiness_generation) = self.facade.readiness();
        NetStackSocketDescriptorV1 {
            socket: self.socket,
            family: family_raw(self.family),
            kind: kind_raw(self.kind),
            protocol: self.protocol,
            state: state_from_owner(self.facade.owner()),
            readiness: readiness.raw(),
            readiness_generation,
        }
    }

    fn snapshot(&self) -> NetStackSocketSnapshotV1 {
        NetStackSocketSnapshotV1 {
            descriptor: self.descriptor(),
            owner: self.facade.owner(),
            local: self.facade.local_endpoint(),
            peer: self.facade.peer_endpoint(),
            interface: self.facade.interface(),
            read_shutdown: self.read_shutdown,
            write_shutdown: self.write_shutdown,
            tx_queued_bytes: 0,
            rx_queued_bytes: 0,
        }
    }
}

/// 一个 `net.stack` 代际独占的 socket 数据与控制状态。
pub struct NetStackSocketTable {
    stack_generation: u64,
    boot_nonce: u64,
    next_counter: u64,
    sockets: BTreeMap<SocketId, SocketEntry>,
}

impl NetStackSocketTable {
    fn create(
        &mut self,
        family: u8,
        kind: u8,
        protocol: u8,
    ) -> Result<NetStackSocketDescriptorV1, NetStackSocketErrorV1> {
        let family = parse_family(family).ok_or(NetStackSocketErrorV1::InvalidArgument)?;
        let kind = parse_kind(kind).ok_or(NetStackSocketErrorV1::InvalidArgument)?;
        let protocol = normalize_protocol(kind, protocol)?;
        let facade = match kind {
            SocketKind::Datagram => new_socket_facade(family),
            SocketKind::Stream => new_tcp_socket_facade(family),
            SocketKind::Raw => new_raw_socket_facade(family, protocol),
        }
        .map_err(map_socket_error)?;
        self.insert_facade(family, kind, protocol, facade)
    }

    fn insert_facade(
        &mut self,
        family: AddressFamily,
        kind: SocketKind,
        protocol: u8,
        facade: Arc<SocketFacade>,
    ) -> Result<NetStackSocketDescriptorV1, NetStackSocketErrorV1> {
        let counter = self.next_counter;
        self.next_counter = self
            .next_counter
            .checked_add(1)
            .ok_or(NetStackSocketErrorV1::BufferFull)?;
        let id = SocketId {
            boot_nonce: self.boot_nonce,
            counter,
        };
        let socket = NetStackSocketRefV1 { id, generation: 1 };
        attach_socket_facade_generation(&facade, self.stack_generation);
        let relay = new_socket_readiness_relay(socket, self.stack_generation);
        facade.set_observer(Arc::downgrade(&relay));
        let entry = SocketEntry {
            socket,
            family,
            kind,
            protocol,
            facade,
            _readiness_relay: relay,
            read_shutdown: false,
            write_shutdown: false,
            pending_control: None,
            options: SocketOptions::new(kind),
        };
        let descriptor = entry.descriptor();
        self.sockets.insert(id, entry);
        Ok(descriptor)
    }

    fn entry(&self, socket: NetStackSocketRefV1) -> Result<&SocketEntry, NetStackSocketErrorV1> {
        if !socket.valid() || socket.id.boot_nonce != self.boot_nonce {
            return Err(NetStackSocketErrorV1::NotFound);
        }
        let entry = self
            .sockets
            .get(&socket.id)
            .ok_or(NetStackSocketErrorV1::NotFound)?;
        if entry.socket.generation != socket.generation {
            return Err(NetStackSocketErrorV1::StaleGeneration);
        }
        Ok(entry)
    }

    fn entry_mut(
        &mut self,
        socket: NetStackSocketRefV1,
    ) -> Result<&mut SocketEntry, NetStackSocketErrorV1> {
        if !socket.valid() || socket.id.boot_nonce != self.boot_nonce {
            return Err(NetStackSocketErrorV1::NotFound);
        }
        let entry = self
            .sockets
            .get_mut(&socket.id)
            .ok_or(NetStackSocketErrorV1::NotFound)?;
        if entry.socket.generation != socket.generation {
            return Err(NetStackSocketErrorV1::StaleGeneration);
        }
        Ok(entry)
    }

    fn close(&mut self, socket: NetStackSocketRefV1) -> Result<(), NetStackSocketErrorV1> {
        self.entry(socket)?;
        if let Some(entry) = self.sockets.remove(&socket.id) {
            if entry.options.abortive_close {
                entry.facade.request_abortive_close();
            }
            entry.facade.close();
        }
        Ok(())
    }

    fn bind(
        &mut self,
        socket: NetStackSocketRefV1,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<Endpoint, NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        if let Some(pending) = entry.pending_control {
            let PendingControl::Bind {
                sequence,
                local: pending_local,
                interface: pending_interface,
                options: pending_options,
            } = pending
            else {
                return Err(NetStackSocketErrorV1::InvalidState);
            };
            if pending_local != local
                || pending_interface != interface
                || pending_options != options
            {
                return Err(NetStackSocketErrorV1::InvalidState);
            }
            let Some(result) = entry.facade.take_control_result(sequence) else {
                return Err(NetStackSocketErrorV1::InProgress);
            };
            entry.pending_control = None;
            result.map_err(map_socket_error)?;
            return Ok(entry.facade.local_endpoint().unwrap_or(local));
        }
        let sequence = entry
            .facade
            .begin_bind(local, interface, options)
            .map_err(map_socket_error)?;
        entry.pending_control = Some(PendingControl::Bind {
            sequence,
            local,
            interface,
            options,
        });
        entry.options.reuse_address = options.reuse_address;
        entry.options.reuse_port = options.reuse_port;
        entry.options.v6_only = options.v6_only;
        entry.options.free_bind = options.free_bind;
        Err(NetStackSocketErrorV1::InProgress)
    }

    fn connect(
        &mut self,
        socket: NetStackSocketRefV1,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        let result = entry
            .facade
            .connect_with_mode(peer, interface, options, true);
        if let Err(error) = result {
            return Err(map_socket_error(error));
        }
        Ok(entry.snapshot())
    }

    fn listen(
        &mut self,
        socket: NetStackSocketRefV1,
        backlog: u32,
    ) -> Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        if let Some(pending) = entry.pending_control {
            let PendingControl::Listen {
                sequence,
                backlog: pending_backlog,
            } = pending
            else {
                return Err(NetStackSocketErrorV1::InvalidState);
            };
            if pending_backlog != backlog {
                return Err(NetStackSocketErrorV1::InvalidState);
            }
            let Some(result) = entry.facade.take_control_result(sequence) else {
                return Err(NetStackSocketErrorV1::InProgress);
            };
            entry.pending_control = None;
            result.map_err(map_socket_error)?;
            return Ok(entry.snapshot());
        }
        let sequence = entry
            .facade
            .begin_listen(backlog)
            .map_err(map_socket_error)?;
        entry.pending_control = Some(PendingControl::Listen { sequence, backlog });
        Err(NetStackSocketErrorV1::InProgress)
    }

    fn accept(
        &mut self,
        socket: NetStackSocketRefV1,
    ) -> Result<NetStackSocketDescriptorV1, NetStackSocketErrorV1> {
        let child = self
            .entry(socket)?
            .facade
            .accept(true, None)
            .map_err(map_socket_error)?;
        let family = child.family();
        let kind = child.kind();
        let protocol = child.protocol();
        self.insert_facade(family, kind, protocol, child)
    }

    fn send(
        &mut self,
        socket: NetStackSocketRefV1,
        data: &[u8],
        destination: Option<Endpoint>,
        dont_route: bool,
        confirm: bool,
    ) -> Result<u32, NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        let result = match entry.kind {
            SocketKind::Stream => {
                if destination.is_some() {
                    return Err(NetStackSocketErrorV1::AlreadyConnected);
                }
                entry.facade.send_stream(data, true, None)
            }
            SocketKind::Datagram | SocketKind::Raw => {
                entry
                    .facade
                    .send_datagram(data, destination, true, None, dont_route, confirm)
            }
        };
        result
            .map(|length| length.min(u32::MAX as usize) as u32)
            .map_err(map_socket_error)
    }

    fn recv(
        &mut self,
        socket: NetStackSocketRefV1,
        output: &mut [u8],
        peek: bool,
        truncate: bool,
    ) -> Result<NetStackSocketRecvV1, NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        if entry.kind == SocketKind::Stream {
            let len = entry
                .facade
                .recv_stream(output, peek, false, true, None)
                .map_err(map_socket_error)?;
            return Ok(NetStackSocketRecvV1 {
                len: len.min(u32::MAX as usize) as u32,
                original_len: len.min(u32::MAX as usize) as u32,
                source: entry.facade.peer_endpoint(),
                destination: entry.facade.local_endpoint(),
                interface: entry.facade.interface(),
                hop_limit: 0,
                traffic_class: 0,
                rx_timestamp_ns: 0,
                truncated: false,
            });
        }
        let received = entry
            .facade
            .recv(output, peek, truncate, true, None)
            .map_err(map_socket_error)?;
        Ok(NetStackSocketRecvV1 {
            len: received.len.min(u32::MAX as usize) as u32,
            original_len: received.original_len.min(u32::MAX as usize) as u32,
            source: Some(received.source),
            destination: Some(received.destination),
            interface: Some(received.ingress_interface),
            hop_limit: received.hop_limit,
            traffic_class: received.traffic_class,
            rx_timestamp_ns: received.rx_timestamp_ns,
            truncated: received.truncated,
        })
    }

    fn shutdown(
        &mut self,
        socket: NetStackSocketRefV1,
        read: bool,
        write: bool,
    ) -> Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1> {
        if !read && !write {
            return Err(NetStackSocketErrorV1::InvalidArgument);
        }
        let entry = self.entry_mut(socket)?;
        entry
            .facade
            .shutdown(read, write)
            .map_err(map_socket_error)?;
        entry.read_shutdown |= read;
        entry.write_shutdown |= write;
        Ok(entry.snapshot())
    }

    fn get_option(
        &self,
        socket: NetStackSocketRefV1,
        option: NetStackSocketOptionV1,
    ) -> Result<NetStackSocketOptionValueV1, NetStackSocketErrorV1> {
        let entry = self.entry(socket)?;
        if !option_supported(entry, option) {
            return Err(NetStackSocketErrorV1::NotSupported);
        }
        Ok(entry.options.get(option))
    }

    fn set_option(
        &mut self,
        socket: NetStackSocketRefV1,
        option: NetStackSocketOptionV1,
        value: NetStackSocketOptionValueV1,
    ) -> Result<(), NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        if !option_supported(entry, option)
            || (option == NetStackSocketOptionV1::V6Only
                && !matches!(entry.facade.owner(), OwnerRef::Unassigned))
            || (option == NetStackSocketOptionV1::TcpMaxSegment
                && matches!(entry.facade.owner(), OwnerRef::Flow { .. }))
        {
            return Err(NetStackSocketErrorV1::NotSupported);
        }
        entry.options.set(option, value)?;
        apply_facade_option(&entry.facade, option, value);
        let (send_limit, receive_limit) = entry.facade.buffer_limits();
        if option == NetStackSocketOptionV1::SendBuffer {
            entry.options.send_buffer = send_limit;
        }
        if option == NetStackSocketOptionV1::ReceiveBuffer {
            entry.options.receive_buffer = receive_limit;
        }
        Ok(())
    }

    fn take_error(
        &self,
        socket: NetStackSocketRefV1,
    ) -> Result<Option<SocketError>, NetStackSocketErrorV1> {
        Ok(self.entry(socket)?.facade.take_pending_error())
    }

    fn take_error_record(
        &self,
        socket: NetStackSocketRefV1,
    ) -> Result<Option<SocketErrorRecord>, NetStackSocketErrorV1> {
        Ok(self.entry(socket)?.facade.take_error_record())
    }

    fn tcp_info(
        &self,
        socket: NetStackSocketRefV1,
    ) -> Result<TcpInfoSnapshot, NetStackSocketErrorV1> {
        Ok(self.entry(socket)?.facade.tcp_info())
    }

    fn take_rx_overflow(&self, socket: NetStackSocketRefV1) -> Result<u32, NetStackSocketErrorV1> {
        Ok(self.entry(socket)?.facade.take_rx_overflow())
    }

    fn multicast(
        &self,
        socket: NetStackSocketRefV1,
        action: NetStackSocketMulticastActionV1,
        membership: Option<MulticastMembership>,
    ) -> Result<bool, NetStackSocketErrorV1> {
        let facade = &self.entry(socket)?.facade;
        match action {
            NetStackSocketMulticastActionV1::Add => facade
                .add_multicast_membership(membership.ok_or(NetStackSocketErrorV1::InvalidArgument)?)
                .map(|()| true)
                .map_err(map_socket_error),
            NetStackSocketMulticastActionV1::Drop => facade
                .drop_multicast_membership(
                    membership.ok_or(NetStackSocketErrorV1::InvalidArgument)?,
                )
                .map(|()| true)
                .map_err(map_socket_error),
            NetStackSocketMulticastActionV1::Query => Ok(facade.has_multicast_memberships()),
        }
    }
}

#[kernel_symbols::export(
    name = "net.stack.create_socket_table",
    contract = "kernel.net.stack-socket-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn create_socket_table(boot_nonce: u64, stack_generation: u64) -> Option<NetStackSocketTable> {
    if boot_nonce == 0 || stack_generation == 0 {
        return None;
    }
    Some(NetStackSocketTable {
        stack_generation,
        boot_nonce,
        next_counter: 1,
        sockets: BTreeMap::new(),
    })
}

#[kernel_symbols::export(
    name = "net.stack.destroy_socket_table",
    contract = "kernel.net.stack-socket-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn destroy_socket_table(mut table: NetStackSocketTable) {
    for (_, entry) in core::mem::take(&mut table.sockets) {
        entry.facade.detach_stack_for_generation();
        entry.facade.close();
    }
}

#[kernel_symbols::export(
    name = "net.stack.dispatch_socket_table_call",
    contract = "kernel.net.stack-socket-state@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn dispatch_socket_table_call(
    table: &mut NetStackSocketTable,
    request: &mut NetStackSocketRequestV1,
    quiesced: bool,
) -> bool {
    if !request.valid_header(request.opcode, table.stack_generation, request.request_id)
        || request.committed != 0
    {
        return false;
    }
    if quiesced && !request.command.allowed_while_quiesced() {
        request
            .command
            .complete_error(NetStackSocketErrorV1::Quiesced);
        request.committed = 1;
        return true;
    }
    match &mut request.command {
        NetStackSocketCommandV1::Create {
            family,
            kind,
            protocol,
            output,
        } => *output = Some(table.create(*family, *kind, *protocol)),
        NetStackSocketCommandV1::Close { socket, output } => {
            *output = Some(table.close(*socket));
        }
        NetStackSocketCommandV1::Bind {
            socket,
            local,
            interface,
            options,
            output,
        } => *output = Some(table.bind(*socket, *local, *interface, *options)),
        NetStackSocketCommandV1::Connect {
            socket,
            peer,
            interface,
            options,
            output,
        } => *output = Some(table.connect(*socket, *peer, *interface, *options)),
        NetStackSocketCommandV1::Listen {
            socket,
            backlog,
            output,
        } => *output = Some(table.listen(*socket, *backlog)),
        NetStackSocketCommandV1::Accept { socket, output } => {
            *output = Some(table.accept(*socket));
        }
        NetStackSocketCommandV1::Send {
            socket,
            data,
            len,
            destination,
            dont_route,
            confirm,
            output,
        } => {
            let length = *len as usize;
            if data.is_null() || length > isize::MAX as usize {
                *output = Some(Err(NetStackSocketErrorV1::InvalidArgument));
            } else {
                // Safety: 宿主将载荷声明为本次 pinned call 的只读范围；长度已校验，
                // 切片只在同步调用期间使用，套接字表复制后才返回。
                let input = unsafe { core::slice::from_raw_parts(*data, length) };
                *output = Some(table.send(*socket, input, *destination, *dont_route, *confirm));
            }
        }
        NetStackSocketCommandV1::Recv {
            socket,
            data,
            capacity,
            peek,
            truncate,
            output,
        } => {
            let length = *capacity as usize;
            if data.is_null() || length > isize::MAX as usize {
                *output = Some(Err(NetStackSocketErrorV1::InvalidArgument));
            } else {
                // Safety: 宿主将输出缓冲区声明为本次 pinned call 的可写范围；长度已
                // 校验，切片只在同步调用期间使用，ELM 不保存该指针。
                let output_buffer = unsafe { core::slice::from_raw_parts_mut(*data, length) };
                *output = Some(table.recv(*socket, output_buffer, *peek, *truncate));
            }
        }
        NetStackSocketCommandV1::GetOption {
            socket,
            option,
            output,
        } => {
            *output = Some(table.get_option(*socket, *option));
        }
        NetStackSocketCommandV1::SetOption {
            socket,
            option,
            value,
            output,
        } => {
            *output = Some(table.set_option(*socket, *option, *value));
        }
        NetStackSocketCommandV1::Query { socket, output } => {
            *output = Some(table.entry(*socket).map(SocketEntry::snapshot));
        }
        NetStackSocketCommandV1::Shutdown {
            socket,
            read,
            write,
            output,
        } => *output = Some(table.shutdown(*socket, *read, *write)),
        NetStackSocketCommandV1::TakeError { socket, output } => {
            *output = Some(table.take_error(*socket));
        }
        NetStackSocketCommandV1::TakeErrorRecord { socket, output } => {
            *output = Some(table.take_error_record(*socket));
        }
        NetStackSocketCommandV1::TcpInfo { socket, output } => {
            *output = Some(table.tcp_info(*socket));
        }
        NetStackSocketCommandV1::TakeRxOverflow { socket, output } => {
            *output = Some(table.take_rx_overflow(*socket));
        }
        NetStackSocketCommandV1::Multicast {
            socket,
            action,
            membership,
            output,
        } => *output = Some(table.multicast(*socket, *action, *membership)),
    }
    request.committed = 1;
    true
}

const fn family_raw(family: AddressFamily) -> u8 {
    match family {
        AddressFamily::Ipv4 => NET_STACK_SOCKET_FAMILY_IPV4,
        AddressFamily::Ipv6 => NET_STACK_SOCKET_FAMILY_IPV6,
    }
}

const fn kind_raw(kind: SocketKind) -> u8 {
    match kind {
        SocketKind::Datagram => NET_STACK_SOCKET_KIND_DATAGRAM,
        SocketKind::Stream => NET_STACK_SOCKET_KIND_STREAM,
        SocketKind::Raw => NET_STACK_SOCKET_KIND_RAW,
    }
}

const fn parse_family(family: u8) -> Option<AddressFamily> {
    match family {
        NET_STACK_SOCKET_FAMILY_IPV4 => Some(AddressFamily::Ipv4),
        NET_STACK_SOCKET_FAMILY_IPV6 => Some(AddressFamily::Ipv6),
        _ => None,
    }
}

const fn parse_kind(kind: u8) -> Option<SocketKind> {
    match kind {
        NET_STACK_SOCKET_KIND_DATAGRAM => Some(SocketKind::Datagram),
        NET_STACK_SOCKET_KIND_STREAM => Some(SocketKind::Stream),
        NET_STACK_SOCKET_KIND_RAW => Some(SocketKind::Raw),
        _ => None,
    }
}

fn normalize_protocol(kind: SocketKind, protocol: u8) -> Result<u8, NetStackSocketErrorV1> {
    match (kind, protocol) {
        (SocketKind::Datagram, 0 | 17) => Ok(17),
        (SocketKind::Stream, 0 | 6) => Ok(6),
        (SocketKind::Raw, 1..=u8::MAX) => Ok(protocol),
        _ => Err(NetStackSocketErrorV1::NotSupported),
    }
}

fn option_supported(entry: &SocketEntry, option: NetStackSocketOptionV1) -> bool {
    use NetStackSocketOptionV1 as OptionId;
    match option {
        OptionId::V6Only | OptionId::ReceiveErrorsV6 => entry.family == AddressFamily::Ipv6,
        OptionId::RawHeaderIncluded => entry.kind == SocketKind::Raw,
        OptionId::TcpNoDelay
        | OptionId::TcpCork
        | OptionId::TcpQuickAck
        | OptionId::TcpKeepAlive
        | OptionId::TcpDeferAcceptNs
        | OptionId::TcpNotSentLowat
        | OptionId::TcpUserTimeoutNs
        | OptionId::TcpKeepIdleNs
        | OptionId::TcpKeepIntervalNs
        | OptionId::TcpKeepCount
        | OptionId::TcpMaxSegment
        | OptionId::TcpMore
        | OptionId::AbortiveClose => entry.kind == SocketKind::Stream,
        _ => true,
    }
}

fn apply_facade_option(
    facade: &Arc<SocketFacade>,
    option: NetStackSocketOptionV1,
    value: NetStackSocketOptionValueV1,
) {
    use NetStackSocketOptionV1 as OptionId;
    use NetStackSocketOptionValueV1::{Bool, I32, Interface, U32, U64};
    match (option, value) {
        (OptionId::V6Only, Bool(value)) => facade.set_v6_only(value),
        (OptionId::FreeBind, Bool(value)) => facade.set_free_bind(value),
        (OptionId::RawHeaderIncluded, Bool(value)) => facade.set_raw_header_included(value),
        (OptionId::TcpNoDelay, Bool(value)) => facade.set_tcp_nodelay(value),
        (OptionId::TcpCork, Bool(value)) => facade.set_tcp_cork(value),
        (OptionId::TcpQuickAck, Bool(true)) => facade.request_quick_ack(),
        (OptionId::TcpKeepAlive, Bool(value)) => facade.set_tcp_keepalive(value),
        (OptionId::IpHopLimit, U32(value)) => facade.set_ip_hop_limit(value as u8),
        (OptionId::IpTrafficClass, U32(value)) => facade.set_ip_traffic_class(value as u8),
        (OptionId::MulticastHops, U32(value)) => facade.set_multicast_hops(value as u8),
        (OptionId::MulticastLoop, Bool(value)) => facade.set_multicast_loop(value),
        (OptionId::MulticastInterface, Interface(value)) => facade.set_multicast_interface(value),
        (OptionId::SocketMark, U32(value)) => facade.set_socket_mark(value),
        (OptionId::SocketPriority, I32(value)) => facade.set_socket_priority(value),
        (OptionId::SendBuffer, U32(value)) => facade.set_buffer_limits(Some(value as usize), None),
        (OptionId::ReceiveBuffer, U32(value)) => {
            facade.set_buffer_limits(None, Some(value as usize))
        }
        (OptionId::TcpDeferAcceptNs, U64(value)) => facade.set_tcp_defer_accept_ns(value),
        (OptionId::TcpNotSentLowat, U32(value)) => facade.set_tcp_notsent_lowat(value),
        (OptionId::TcpUserTimeoutNs, U64(value)) => facade.set_tcp_user_timeout_ns(value),
        (OptionId::TcpKeepIdleNs, U64(value)) => facade.set_tcp_keepidle_ns(value),
        (OptionId::TcpKeepIntervalNs, U64(value)) => facade.set_tcp_keepintvl_ns(value),
        (OptionId::TcpKeepCount, U32(value)) => facade.set_tcp_keepcount(value as u16),
        (OptionId::TcpMaxSegment, U32(value)) => facade.set_tcp_maxseg(value as u16),
        (OptionId::TcpMore, Bool(value)) => facade.set_tcp_more(value),
        (OptionId::AbortiveClose, Bool(true)) => facade.request_abortive_close(),
        _ => {}
    }
}

const fn state_from_owner(owner: OwnerRef) -> NetStackSocketStateV1 {
    match owner {
        OwnerRef::Unassigned => NetStackSocketStateV1::Unbound,
        OwnerRef::Bound { .. } => NetStackSocketStateV1::Bound,
        OwnerRef::Listener { .. } => NetStackSocketStateV1::Listening,
        OwnerRef::Flow { .. } | OwnerRef::Closed { .. } => NetStackSocketStateV1::Connected,
    }
}

const fn map_socket_error(error: SocketError) -> NetStackSocketErrorV1 {
    match error {
        SocketError::RuntimeUnavailable | SocketError::NetworkDown => {
            NetStackSocketErrorV1::Quiesced
        }
        SocketError::RuntimeBusy | SocketError::Buffer => NetStackSocketErrorV1::BufferFull,
        SocketError::InvalidState => NetStackSocketErrorV1::InvalidState,
        SocketError::AddressInUse => NetStackSocketErrorV1::AddressInUse,
        SocketError::AddressUnavailable => NetStackSocketErrorV1::AddressUnavailable,
        SocketError::NotConnected => NetStackSocketErrorV1::NotConnected,
        SocketError::DestinationRequired => NetStackSocketErrorV1::DestinationRequired,
        SocketError::AlreadyConnected => NetStackSocketErrorV1::AlreadyConnected,
        SocketError::AlreadyInProgress => NetStackSocketErrorV1::AlreadyInProgress,
        SocketError::InProgress => NetStackSocketErrorV1::InProgress,
        SocketError::WouldBlock | SocketError::TimedOut | SocketError::Interrupted => {
            NetStackSocketErrorV1::WouldBlock
        }
        SocketError::MessageTooLarge => NetStackSocketErrorV1::MessageTooLarge,
        SocketError::ReadShutdown => NetStackSocketErrorV1::ReadShutdown,
        SocketError::WriteShutdown => NetStackSocketErrorV1::WriteShutdown,
        SocketError::Closed => NetStackSocketErrorV1::NotFound,
        SocketError::NetworkUnreachable
        | SocketError::HostUnreachable
        | SocketError::ConnectionRefused
        | SocketError::ConnectionReset => NetStackSocketErrorV1::NotConnected,
    }
}
