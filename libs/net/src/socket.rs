//! 网络 Socket 类型定义。
//!
//! [`NetSocketHandle`] 是网络 socket 在协议栈中的唯一标识——持有它即可
//! 通过 [`NetStack`](crate::stack::NetStack) 的方法执行所有 socket 操作
//! （connect/send/recv/close 等）。
//!
//! 设计上 handle 是轻量值类型（Copy），可以自由传递和存储。实际的 socket
//! 状态和缓冲区由 smoltcp 的 `SocketSet` 内部管理——本 crate 只暴露
//! 操作接口，不暴露 smoltcp 内部类型。

use core::sync::atomic::{AtomicBool, Ordering};

use crate::device::InterfaceId;

// ── Socket 类型 ──────────────────────────────────────────────────────────────

/// 网络 socket 协议类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    /// TCP 流式 socket（面向连接，可靠有序字节流）。
    Tcp,
    /// UDP 数据报 socket（无连接，尽力而为消息传递）。
    Udp,
}

// ── Socket 状态 ──────────────────────────────────────────────────────────────

/// Socket 可观测状态（从 smoltcp 内部状态映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketState {
    /// Socket 已创建但未绑定/连接。
    Closed,
    /// TCP: 正在监听入站连接。
    Listen,
    /// TCP: 正在建立连接（SYN 已发送 / SYN-ACK 已收到等）。
    Connecting,
    /// TCP: 连接已建立，可收发数据。UDP: 已绑定，可收发。
    Established,
    /// TCP: 正在关闭连接（FIN 交换中）。
    Closing,
}

// ── Socket 元数据（内部用于 soft-close 和生命周期管理）────────────────────────

/// 每个 socket 的内部元数据。
///
/// 用于实现 soft-close 语义：`socket_remove` 时先标记 `removed = true`，
/// 后续所有操作检查此标志直接返回 `Closed`。实际从 `SocketSet` 中移除
/// 推迟到下一轮 `poll`，避免并发操作 use-after-free。
pub(crate) struct SocketMeta {
    /// socket 是否已被标记为移除。
    pub removed: AtomicBool,
}

impl SocketMeta {
    pub fn new() -> Self {
        Self {
            removed: AtomicBool::new(false),
        }
    }

    pub fn is_removed(&self) -> bool {
        self.removed.load(Ordering::Acquire)
    }

    pub fn mark_removed(&self) {
        self.removed.store(true, Ordering::Release);
    }
}

// ── Socket Handle ────────────────────────────────────────────────────────────

/// 网络 socket 句柄。
///
/// 轻量标识符（`Copy`），持有它即可通过 `NetStack` 操作 socket。
/// 内含接口 ID 以便 stack 直接路由到正确的 `ManagedInterface`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetSocketHandle {
    pub(crate) iface_id: InterfaceId,
    pub(crate) inner: smoltcp::iface::SocketHandle,
    pub(crate) sock_type: SocketType,
}

impl NetSocketHandle {
    /// 该 socket 所属的网络接口。
    pub fn interface_id(&self) -> InterfaceId {
        self.iface_id
    }

    /// socket 协议类型。
    pub fn socket_type(&self) -> SocketType {
        self.sock_type
    }
}
