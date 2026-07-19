//! `net.stack` socket 调用 ABI 与代际内套接字表。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use crate::control::BindOptions;
use crate::{AddressFamily, Endpoint, InterfaceId, IpAddr, Readiness, SocketId, SocketKind};

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

pub const NET_STACK_SOCKET_FAMILY_IPV4: u8 = 4;
pub const NET_STACK_SOCKET_FAMILY_IPV6: u8 = 6;
pub const NET_STACK_SOCKET_KIND_DATAGRAM: u8 = 1;
pub const NET_STACK_SOCKET_KIND_STREAM: u8 = 2;
pub const NET_STACK_SOCKET_KIND_RAW: u8 = 3;

const EPHEMERAL_START: u16 = 49_152;
const EPHEMERAL_COUNT: usize = 16_384;
const DATAGRAM_BUFFER_DEFAULT: usize = 128 * 1024;
const STREAM_BUFFER_DEFAULT: usize = 256 * 1024;
const DATAGRAM_MAX: usize = 65_527;

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
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        }
    }

    pub const fn allowed_while_quiesced(&self) -> bool {
        matches!(
            self,
            Self::Close { .. } | Self::Recv { .. } | Self::GetOption { .. } | Self::Query { .. }
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
struct SocketPayload {
    bytes: Vec<u8>,
    source: Option<Endpoint>,
    destination: Option<Endpoint>,
    interface: Option<InterfaceId>,
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
            _ => return Err(NetStackSocketErrorV1::InvalidArgument),
        }
        Ok(())
    }

    fn bind_options(&self) -> BindOptions {
        BindOptions {
            reuse_address: self.reuse_address,
            reuse_port: self.reuse_port,
            v6_only: self.v6_only,
            multicast_or_broadcast: false,
            free_bind: self.free_bind,
        }
    }
}

struct SocketEntry {
    socket: NetStackSocketRefV1,
    family: AddressFamily,
    kind: SocketKind,
    protocol: u8,
    state: NetStackSocketStateV1,
    local: Option<Endpoint>,
    peer: Option<Endpoint>,
    interface: Option<InterfaceId>,
    backlog: u32,
    accepted: VecDeque<NetStackSocketRefV1>,
    tx: VecDeque<SocketPayload>,
    rx: VecDeque<SocketPayload>,
    tx_queued_bytes: usize,
    rx_queued_bytes: usize,
    read_shutdown: bool,
    write_shutdown: bool,
    readiness: Readiness,
    readiness_generation: u64,
    options: SocketOptions,
}

impl SocketEntry {
    fn descriptor(&self) -> NetStackSocketDescriptorV1 {
        NetStackSocketDescriptorV1 {
            socket: self.socket,
            family: family_raw(self.family),
            kind: kind_raw(self.kind),
            protocol: self.protocol,
            state: self.state,
            readiness: self.readiness.raw(),
            readiness_generation: self.readiness_generation,
        }
    }

    fn snapshot(&self) -> NetStackSocketSnapshotV1 {
        NetStackSocketSnapshotV1 {
            descriptor: self.descriptor(),
            local: self.local,
            peer: self.peer,
            interface: self.interface,
            read_shutdown: self.read_shutdown,
            write_shutdown: self.write_shutdown,
            tx_queued_bytes: self.tx_queued_bytes.min(u32::MAX as usize) as u32,
            rx_queued_bytes: self.rx_queued_bytes.min(u32::MAX as usize) as u32,
        }
    }

    fn publish_readiness(&mut self, readiness: Readiness) {
        if self.readiness != readiness {
            self.readiness = readiness;
            self.readiness_generation = self.readiness_generation.saturating_add(1);
        }
    }

    fn refresh_readiness(&mut self) {
        let mut readiness = Readiness::default();
        if !self.rx.is_empty() || self.read_shutdown {
            readiness = readiness | Readiness::READABLE;
        }
        let connected_stream =
            self.kind != SocketKind::Stream || self.state == NetStackSocketStateV1::Connected;
        if !self.write_shutdown
            && connected_stream
            && self.tx_queued_bytes < self.options.send_buffer
        {
            readiness = readiness | Readiness::WRITABLE;
        }
        if self.state == NetStackSocketStateV1::Listening && !self.accepted.is_empty() {
            readiness = readiness | Readiness::ACCEPTABLE | Readiness::READABLE;
        }
        if self.read_shutdown && self.write_shutdown {
            readiness = readiness | Readiness::HANGUP | Readiness::READ_HANGUP;
        } else if self.read_shutdown {
            readiness = readiness | Readiness::READ_HANGUP;
        }
        self.publish_readiness(readiness);
    }
}

/// 一个 `net.stack` 代际独占的 socket 数据与控制状态。
pub struct NetStackSocketTable {
    stack_generation: u64,
    boot_nonce: u64,
    next_counter: u64,
    next_ephemeral: u16,
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
        let mut entry = SocketEntry {
            socket,
            family,
            kind,
            protocol,
            state: NetStackSocketStateV1::Unbound,
            local: None,
            peer: None,
            interface: None,
            backlog: 0,
            accepted: VecDeque::new(),
            tx: VecDeque::new(),
            rx: VecDeque::new(),
            tx_queued_bytes: 0,
            rx_queued_bytes: 0,
            read_shutdown: false,
            write_shutdown: false,
            readiness: Readiness::default(),
            readiness_generation: 1,
            options: SocketOptions::new(kind),
        };
        entry.refresh_readiness();
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
        self.sockets.remove(&socket.id);
        for entry in self.sockets.values_mut() {
            entry.accepted.retain(|candidate| candidate.id != socket.id);
            entry.refresh_readiness();
        }
        Ok(())
    }

    fn bind(
        &mut self,
        socket: NetStackSocketRefV1,
        mut local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<Endpoint, NetStackSocketErrorV1> {
        let (family, protocol, state) = {
            let entry = self.entry(socket)?;
            (entry.family, entry.protocol, entry.state)
        };
        if state != NetStackSocketStateV1::Unbound
            || !address_allowed(family, local.addr, options.v6_only)
        {
            return Err(NetStackSocketErrorV1::InvalidState);
        }
        if local.port == 0 {
            local.port =
                self.allocate_ephemeral(family, protocol, local.addr, interface, options)?;
        } else if self.binding_conflicts(family, protocol, local, interface, options, None) {
            return Err(NetStackSocketErrorV1::AddressInUse);
        }
        let entry = self.entry_mut(socket)?;
        entry.options.reuse_address = options.reuse_address;
        entry.options.reuse_port = options.reuse_port;
        entry.options.v6_only = options.v6_only;
        entry.options.free_bind = options.free_bind;
        entry.local = Some(local);
        entry.interface = interface;
        entry.state = NetStackSocketStateV1::Bound;
        entry.refresh_readiness();
        Ok(local)
    }

    fn connect(
        &mut self,
        socket: NetStackSocketRefV1,
        peer: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1> {
        let (family, state) = {
            let entry = self.entry(socket)?;
            (entry.family, entry.state)
        };
        if !address_allowed(family, peer.addr, options.v6_only) {
            return Err(NetStackSocketErrorV1::AddressUnavailable);
        }
        if matches!(
            state,
            NetStackSocketStateV1::Connected | NetStackSocketStateV1::Listening
        ) {
            return Err(NetStackSocketErrorV1::AlreadyConnected);
        }
        if state == NetStackSocketStateV1::Unbound {
            let local = Endpoint {
                addr: unspecified(family),
                port: 0,
            };
            self.bind(socket, local, interface, options)?;
        }
        let entry = self.entry_mut(socket)?;
        entry.peer = Some(peer);
        entry.interface = interface.or(entry.interface);
        entry.state = NetStackSocketStateV1::Connected;
        entry.refresh_readiness();
        Ok(entry.snapshot())
    }

    fn listen(
        &mut self,
        socket: NetStackSocketRefV1,
        backlog: u32,
    ) -> Result<NetStackSocketSnapshotV1, NetStackSocketErrorV1> {
        let (family, kind, state) = {
            let entry = self.entry(socket)?;
            (entry.family, entry.kind, entry.state)
        };
        if kind != SocketKind::Stream {
            return Err(NetStackSocketErrorV1::NotSupported);
        }
        if state == NetStackSocketStateV1::Unbound {
            self.bind(
                socket,
                Endpoint {
                    addr: unspecified(family),
                    port: 0,
                },
                None,
                BindOptions::default(),
            )?;
        }
        let entry = self.entry_mut(socket)?;
        if !matches!(
            entry.state,
            NetStackSocketStateV1::Bound | NetStackSocketStateV1::Listening
        ) {
            return Err(NetStackSocketErrorV1::InvalidState);
        }
        entry.state = NetStackSocketStateV1::Listening;
        entry.backlog = backlog.max(1);
        entry.refresh_readiness();
        Ok(entry.snapshot())
    }

    fn accept(
        &mut self,
        socket: NetStackSocketRefV1,
    ) -> Result<NetStackSocketDescriptorV1, NetStackSocketErrorV1> {
        let child = {
            let entry = self.entry_mut(socket)?;
            if entry.state != NetStackSocketStateV1::Listening {
                return Err(NetStackSocketErrorV1::InvalidState);
            }
            let child = entry
                .accepted
                .pop_front()
                .ok_or(NetStackSocketErrorV1::WouldBlock)?;
            entry.refresh_readiness();
            child
        };
        Ok(self.entry(child)?.descriptor())
    }

    fn send(
        &mut self,
        socket: NetStackSocketRefV1,
        data: &[u8],
        destination: Option<Endpoint>,
    ) -> Result<u32, NetStackSocketErrorV1> {
        let (kind, family, state, interface, bind_options) = {
            let entry = self.entry(socket)?;
            (
                entry.kind,
                entry.family,
                entry.state,
                entry.interface,
                entry.options.bind_options(),
            )
        };
        if kind != SocketKind::Stream && state == NetStackSocketStateV1::Unbound {
            self.bind(
                socket,
                Endpoint {
                    addr: unspecified(family),
                    port: 0,
                },
                interface,
                bind_options,
            )?;
        }
        let entry = self.entry_mut(socket)?;
        if entry.write_shutdown {
            return Err(NetStackSocketErrorV1::WriteShutdown);
        }
        let destination = match entry.kind {
            SocketKind::Stream => {
                if destination.is_some() {
                    return Err(NetStackSocketErrorV1::AlreadyConnected);
                }
                entry.peer.ok_or(NetStackSocketErrorV1::NotConnected)?
            }
            SocketKind::Datagram | SocketKind::Raw => destination
                .or(entry.peer)
                .ok_or(NetStackSocketErrorV1::DestinationRequired)?,
        };
        if entry.kind != SocketKind::Stream && data.len() > DATAGRAM_MAX {
            return Err(NetStackSocketErrorV1::MessageTooLarge);
        }
        if entry
            .tx_queued_bytes
            .checked_add(data.len())
            .is_none_or(|total| total > entry.options.send_buffer)
        {
            entry.refresh_readiness();
            return Err(NetStackSocketErrorV1::WouldBlock);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(data.len())
            .map_err(|_| NetStackSocketErrorV1::BufferFull)?;
        bytes.extend_from_slice(data);
        entry.tx_queued_bytes += bytes.len();
        entry.tx.push_back(SocketPayload {
            bytes,
            source: entry.local,
            destination: Some(destination),
            interface: entry.interface,
        });
        entry.refresh_readiness();
        Ok(data.len() as u32)
    }

    fn recv(
        &mut self,
        socket: NetStackSocketRefV1,
        output: &mut [u8],
        peek: bool,
        truncate: bool,
    ) -> Result<NetStackSocketRecvV1, NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        let Some(payload) = entry.rx.front_mut() else {
            if entry.read_shutdown {
                return Ok(NetStackSocketRecvV1 {
                    len: 0,
                    original_len: 0,
                    source: entry.peer,
                    destination: entry.local,
                    interface: entry.interface,
                    truncated: false,
                });
            }
            return Err(NetStackSocketErrorV1::WouldBlock);
        };
        let original_len = payload.bytes.len();
        let copied = original_len.min(output.len());
        output[..copied].copy_from_slice(&payload.bytes[..copied]);
        let result = NetStackSocketRecvV1 {
            len: if truncate { original_len } else { copied }.min(u32::MAX as usize) as u32,
            original_len: original_len.min(u32::MAX as usize) as u32,
            source: payload.source,
            destination: payload.destination,
            interface: payload.interface,
            truncated: copied != original_len,
        };
        if !peek {
            if entry.kind == SocketKind::Stream && copied < original_len {
                payload.bytes.drain(..copied);
                entry.rx_queued_bytes -= copied;
            } else {
                entry.rx.pop_front();
                entry.rx_queued_bytes -= original_len;
            }
            entry.refresh_readiness();
        }
        Ok(result)
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
        entry.read_shutdown |= read;
        entry.write_shutdown |= write;
        if read {
            entry.rx.clear();
            entry.rx_queued_bytes = 0;
        }
        entry.refresh_readiness();
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
                && entry.state != NetStackSocketStateV1::Unbound)
            || (option == NetStackSocketOptionV1::TcpMaxSegment
                && entry.state == NetStackSocketStateV1::Connected)
        {
            return Err(NetStackSocketErrorV1::NotSupported);
        }
        entry.options.set(option, value)?;
        entry.refresh_readiness();
        Ok(())
    }

    fn allocate_ephemeral(
        &mut self,
        family: AddressFamily,
        protocol: u8,
        address: IpAddr,
        interface: Option<InterfaceId>,
        options: BindOptions,
    ) -> Result<u16, NetStackSocketErrorV1> {
        for _ in 0..EPHEMERAL_COUNT {
            let port = self.next_ephemeral;
            self.next_ephemeral = if port == u16::MAX {
                EPHEMERAL_START
            } else {
                port + 1
            };
            let local = Endpoint {
                addr: address,
                port,
            };
            if !self.binding_conflicts(family, protocol, local, interface, options, None) {
                return Ok(port);
            }
        }
        Err(NetStackSocketErrorV1::AddressInUse)
    }

    fn binding_conflicts(
        &self,
        family: AddressFamily,
        protocol: u8,
        local: Endpoint,
        interface: Option<InterfaceId>,
        options: BindOptions,
        exclude: Option<SocketId>,
    ) -> bool {
        self.sockets.values().any(|entry| {
            if Some(entry.socket.id) == exclude
                || entry.family != family
                || entry.protocol != protocol
                || entry.interface != interface
            {
                return false;
            }
            let Some(existing) = entry.local else {
                return false;
            };
            if existing.port != local.port
                || !(existing.addr == local.addr
                    || existing.addr.is_unspecified()
                    || local.addr.is_unspecified())
            {
                return false;
            }
            !((options.reuse_port && entry.options.reuse_port)
                || (options.reuse_address && entry.options.reuse_address))
        })
    }

    #[cfg(test)]
    fn push_received(
        &mut self,
        socket: NetStackSocketRefV1,
        bytes: &[u8],
        source: Option<Endpoint>,
        destination: Option<Endpoint>,
    ) -> Result<(), NetStackSocketErrorV1> {
        let entry = self.entry_mut(socket)?;
        if entry
            .rx_queued_bytes
            .checked_add(bytes.len())
            .is_none_or(|total| total > entry.options.receive_buffer)
        {
            return Err(NetStackSocketErrorV1::BufferFull);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| NetStackSocketErrorV1::BufferFull)?;
        owned.extend_from_slice(bytes);
        entry.rx_queued_bytes += owned.len();
        entry.rx.push_back(SocketPayload {
            bytes: owned,
            source,
            destination,
            interface: entry.interface,
        });
        entry.refresh_readiness();
        Ok(())
    }

    #[cfg(test)]
    fn enqueue_accepted(
        &mut self,
        listener: NetStackSocketRefV1,
        child: NetStackSocketRefV1,
    ) -> Result<(), NetStackSocketErrorV1> {
        self.entry(child)?;
        let entry = self.entry_mut(listener)?;
        if entry.state != NetStackSocketStateV1::Listening {
            return Err(NetStackSocketErrorV1::InvalidState);
        }
        if entry.accepted.len() >= entry.backlog as usize {
            return Err(NetStackSocketErrorV1::BufferFull);
        }
        entry.accepted.push_back(child);
        entry.refresh_readiness();
        Ok(())
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
        next_ephemeral: EPHEMERAL_START,
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
pub fn destroy_socket_table(table: NetStackSocketTable) {
    drop(table);
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
            output,
        } => {
            let length = *len as usize;
            if data.is_null() || length > isize::MAX as usize {
                *output = Some(Err(NetStackSocketErrorV1::InvalidArgument));
            } else {
                // Safety: 宿主将载荷声明为本次 pinned call 的只读范围；长度已校验，
                // 切片只在同步调用期间使用，套接字表复制后才返回。
                let input = unsafe { core::slice::from_raw_parts(*data, length) };
                *output = Some(table.send(*socket, input, *destination));
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

fn address_allowed(family: AddressFamily, address: IpAddr, v6_only: bool) -> bool {
    match (family, address) {
        (AddressFamily::Ipv4, IpAddr::V4(_)) | (AddressFamily::Ipv6, IpAddr::V6(_)) => true,
        (AddressFamily::Ipv6, IpAddr::V4(_)) => !v6_only,
        _ => false,
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
        | OptionId::TcpMaxSegment => entry.kind == SocketKind::Stream,
        _ => true,
    }
}

const fn unspecified(family: AddressFamily) -> IpAddr {
    match family {
        AddressFamily::Ipv4 => IpAddr::V4(crate::Ipv4Addr::UNSPECIFIED),
        AddressFamily::Ipv6 => IpAddr::V6(crate::Ipv6Addr::UNSPECIFIED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ipv4Addr, Ipv6Addr};

    fn request(
        table: &mut NetStackSocketTable,
        id: u64,
        command: NetStackSocketCommandV1,
    ) -> NetStackSocketCommandV1 {
        let mut request = NetStackSocketRequestV1::new(table.stack_generation, id, command);
        assert!(dispatch_socket_table_call(table, &mut request, false));
        assert_eq!(request.committed, 1);
        request.command
    }

    fn create(
        table: &mut NetStackSocketTable,
        id: u64,
        family: u8,
        kind: u8,
        protocol: u8,
    ) -> NetStackSocketDescriptorV1 {
        match request(
            table,
            id,
            NetStackSocketCommandV1::Create {
                family,
                kind,
                protocol,
                output: None,
            },
        ) {
            NetStackSocketCommandV1::Create {
                output: Some(Ok(descriptor)),
                ..
            } => descriptor,
            _ => panic!("socket create 未返回 descriptor"),
        }
    }

    #[test]
    fn socket_table_runs_datagram_lifecycle_and_payload_calls() {
        let mut table = create_socket_table(9, 7).unwrap();
        let descriptor = create(
            &mut table,
            1,
            NET_STACK_SOCKET_FAMILY_IPV4,
            NET_STACK_SOCKET_KIND_DATAGRAM,
            0,
        );
        let local = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
        };
        let bound = match request(
            &mut table,
            2,
            NetStackSocketCommandV1::Bind {
                socket: descriptor.socket,
                local,
                interface: None,
                options: BindOptions::default(),
                output: None,
            },
        ) {
            NetStackSocketCommandV1::Bind {
                output: Some(Ok(bound)),
                ..
            } => bound,
            _ => panic!("socket bind 未返回 endpoint"),
        };
        assert_ne!(bound.port, 0);
        let peer = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
            port: 9000,
        };
        assert!(matches!(
            request(
                &mut table,
                3,
                NetStackSocketCommandV1::Connect {
                    socket: descriptor.socket,
                    peer,
                    interface: None,
                    options: BindOptions::default(),
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Connect {
                output: Some(Ok(_)),
                ..
            }
        ));
        let payload = b"elm socket";
        assert!(matches!(
            request(
                &mut table,
                4,
                NetStackSocketCommandV1::Send {
                    socket: descriptor.socket,
                    data: payload.as_ptr(),
                    len: payload.len() as u32,
                    destination: None,
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Send {
                output: Some(Ok(10)),
                ..
            }
        ));
        table
            .push_received(descriptor.socket, payload, Some(peer), Some(bound))
            .unwrap();
        let mut output = [0u8; 32];
        assert!(matches!(
            request(
                &mut table,
                5,
                NetStackSocketCommandV1::Recv {
                    socket: descriptor.socket,
                    data: output.as_mut_ptr(),
                    capacity: output.len() as u32,
                    peek: false,
                    truncate: false,
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Recv {
                output: Some(Ok(NetStackSocketRecvV1 { len: 10, .. })),
                ..
            }
        ));
        assert_eq!(&output[..payload.len()], payload);
    }

    #[test]
    fn socket_table_handles_options_listener_accept_and_close() {
        let mut table = create_socket_table(12, 4).unwrap();
        let listener = create(
            &mut table,
            1,
            NET_STACK_SOCKET_FAMILY_IPV6,
            NET_STACK_SOCKET_KIND_STREAM,
            6,
        );
        assert_eq!(listener.readiness & Readiness::WRITABLE.raw(), 0);
        assert!(matches!(
            request(
                &mut table,
                2,
                NetStackSocketCommandV1::SetOption {
                    socket: listener.socket,
                    option: NetStackSocketOptionV1::V6Only,
                    value: NetStackSocketOptionValueV1::Bool(true),
                    output: None,
                },
            ),
            NetStackSocketCommandV1::SetOption {
                output: Some(Ok(())),
                ..
            }
        ));
        let local = Endpoint {
            addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            port: 8080,
        };
        let mut bind_options = BindOptions::default();
        bind_options.v6_only = true;
        let _ = request(
            &mut table,
            3,
            NetStackSocketCommandV1::Bind {
                socket: listener.socket,
                local,
                interface: None,
                options: bind_options,
                output: None,
            },
        );
        assert!(matches!(
            request(
                &mut table,
                4,
                NetStackSocketCommandV1::Listen {
                    socket: listener.socket,
                    backlog: 8,
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Listen {
                output: Some(Ok(_)),
                ..
            }
        ));
        let child = create(
            &mut table,
            5,
            NET_STACK_SOCKET_FAMILY_IPV6,
            NET_STACK_SOCKET_KIND_STREAM,
            6,
        );
        table
            .enqueue_accepted(listener.socket, child.socket)
            .unwrap();
        assert!(matches!(
            request(
                &mut table,
                6,
                NetStackSocketCommandV1::Accept {
                    socket: listener.socket,
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Accept {
                output: Some(Ok(descriptor)),
                ..
            } if descriptor.socket == child.socket
        ));
        assert!(matches!(
            request(
                &mut table,
                7,
                NetStackSocketCommandV1::Close {
                    socket: child.socket,
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Close {
                output: Some(Ok(())),
                ..
            }
        ));
    }

    #[test]
    fn socket_table_rejects_quiesced_mutation_and_stale_generation() {
        let mut table = create_socket_table(3, 5).unwrap();
        let descriptor = create(
            &mut table,
            1,
            NET_STACK_SOCKET_FAMILY_IPV4,
            NET_STACK_SOCKET_KIND_DATAGRAM,
            17,
        );
        let mut quiesced = NetStackSocketRequestV1::new(
            5,
            2,
            NetStackSocketCommandV1::SetOption {
                socket: descriptor.socket,
                option: NetStackSocketOptionV1::Broadcast,
                value: NetStackSocketOptionValueV1::Bool(true),
                output: None,
            },
        );
        assert!(dispatch_socket_table_call(&mut table, &mut quiesced, true));
        assert!(matches!(
            quiesced.command,
            NetStackSocketCommandV1::SetOption {
                output: Some(Err(NetStackSocketErrorV1::Quiesced)),
                ..
            }
        ));
        let stale = NetStackSocketRefV1 {
            generation: descriptor.socket.generation + 1,
            ..descriptor.socket
        };
        assert!(matches!(
            request(
                &mut table,
                3,
                NetStackSocketCommandV1::Query {
                    socket: stale,
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Query {
                output: Some(Err(NetStackSocketErrorV1::StaleGeneration)),
                ..
            }
        ));

        let auto_bound = create(
            &mut table,
            4,
            NET_STACK_SOCKET_FAMILY_IPV4,
            NET_STACK_SOCKET_KIND_DATAGRAM,
            17,
        );
        let payload = b"auto-bind";
        let destination = Endpoint {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 9000,
        };
        assert!(matches!(
            request(
                &mut table,
                5,
                NetStackSocketCommandV1::Send {
                    socket: auto_bound.socket,
                    data: payload.as_ptr(),
                    len: payload.len() as u32,
                    destination: Some(destination),
                    output: None,
                },
            ),
            NetStackSocketCommandV1::Send {
                output: Some(Ok(9)),
                ..
            }
        ));
        assert!(
            table
                .entry(auto_bound.socket)
                .unwrap()
                .local
                .is_some_and(|local| local.port != 0)
        );
    }
}
