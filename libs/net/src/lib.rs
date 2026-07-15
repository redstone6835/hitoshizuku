//! 网络栈的架构无关核心。
//!
//! 当前提供 buffer 所有权、批量 queue 契约和设备注册边界。

#![no_std]

extern crate alloc;

pub mod address;
pub mod buf;
pub mod device;
pub mod id;
pub mod queue;
pub mod tuning;

pub use address::{Endpoint, IpAddr, Ipv4Addr, Ipv6Addr};
pub use id::{NetDeviceId, QueuePairId};
