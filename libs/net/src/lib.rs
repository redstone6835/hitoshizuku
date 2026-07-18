//! 网络子系统 crate。
//!
//! 本 crate 提供内核网络功能的核心抽象和协议栈集成：
//!
//! - [`driver`]：网络设备驱动 trait（[`NetDriver`](driver::NetDriver)）和
//!   缓冲区类型。任何网络硬件驱动只需实现此 trait 即可接入协议栈。
//! - [`device`]：网络设备对象（[`NetDevice`](device::NetDevice)），
//!   代表一个已注册的网络接口在内核中的身份。
//! - [`config`]：接口配置类型（IP 地址、网关、DHCP 等）。
//! - [`adapter`]：将 [`NetDriver`](driver::NetDriver) 适配为当前协议引擎的
//!   设备接口。
//! - [`interface`]：单个受管理接口的内部状态。
//! - [`stack`]：全局网络协议栈管理器
//!   （[`NetStack`](stack::NetStack)），负责接口生命周期和 poll 调度。
//! - [`time`]：网络层自有单调时间类型，隔离具体协议引擎的时间表示。
//! - `engine`：crate 内部协议引擎适配类型，不作为公共 API 暴露。
//! - [`error`]：统一错误类型。
//!
//! # 架构分层
//!
//! ```text
//! ┌──────────────────────────────────┐
//! │  syscall 层 / socket API         │
//! ├──────────────────────────────────┤
//! │  stack.rs (NetStack)             │  ← 协议栈调度
//! ├──────────────────────────────────┤
//! │  interface/adapter/time          │  ← 协议引擎适配边界
//! ├──────────────────────────────────┤
//! │  driver.rs (NetDriver trait)     │  ← 设备抽象
//! ├──────────────────────────────────┤
//! │  loopback / VirtIO-net / ...     │  ← 具体驱动（drivers 中的 ELM）
//! └──────────────────────────────────┘
//! ```
//!
//! # 扩展性
//!
//! - 新增网络驱动：实现 `NetDriver` trait，零 core 改动。
//! - 新增协议：在 `stack.rs` 暴露协议无关 handle 方法。
//! - 替换协议栈：重写协议引擎适配层，`driver.rs` 和公共时间/配置类型不动。
//! - IPv6：启用 `smoltcp/proto-ipv6`，`config.rs` 加地址变体。

#![no_std]

extern crate alloc;

pub mod adapter;
pub mod config;
pub mod device;
pub mod driver;
pub mod elm;
mod engine;
pub mod error;
pub mod interface;
pub mod route;
pub mod socket;
pub mod stack;
pub mod time;
pub mod tuning;

pub use config::{CidrAddress, Endpoint, Gateway, IfConfig, IfMode, IpAddr, Ipv4Addr, Ipv6Addr};
pub use device::{InterfaceId, NetDevice};
pub use driver::{Duplex, LinkMedium, LinkState, NetDriver, NetStats, RxBuf, TxBuf};
pub use error::NetError;
pub use route::{NextHop, RouteEntry, RouteLookup, RouteSource, RouteTable};
pub use socket::{NetSocketHandle, SocketState, SocketType, TcpConnSnapshot, UdpSockSnapshot};
pub use stack::stack;
pub use stack::{
    IFF_BROADCAST, IFF_MULTICAST, IFF_RUNNING, IFF_UP, InterfaceSnapshot, NeighborEntry,
};
pub use time::{NetDuration, NetInstant};
pub use tuning::{
    EphemeralPortRange, NetTuning, PacketBufferTuning, TcpBufferTuning, TcpListenTuning,
};

/// 强制链接器保留网络设备 ELM 直接符号所在的代码生成单元。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    stack::stack as usize
        ^ stack::NetStack::attach as usize
        ^ stack::NetStack::detach as usize
        ^ stack::NetStack::find_interface_by_name as usize
        ^ device::NetDevice::new as usize
        ^ device::NetDevice::id as usize
        ^ device::NetDevice::mark_gone as usize
        ^ driver::RxBuf::new as usize
        ^ driver::RxBuf::len as usize
        ^ driver::RxBuf::into_storage as usize
        ^ driver::TxBuf::new_heap as usize
        ^ driver::TxBuf::len as usize
        ^ driver::TxBuf::into_heap as usize
        ^ config::IfConfig::static_v4 as usize
}
