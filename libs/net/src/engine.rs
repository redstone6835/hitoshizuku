//! 当前协议引擎的私有适配类型。
//!
//! `libs/net` 的公共 API 不直接暴露 smoltcp 类型。需要暂时穿过 stack 和
//! interface 的协议引擎句柄，都先封装在本模块，后续替换协议栈时只需保留
//! 同等语义的轻量标识符。

/// 协议引擎 socket 表中的槽位句柄。
///
/// 这是 crate 内部的 opaque handle。生命周期校验仍由
/// [`crate::socket::NetSocketHandle`] 上的 generation/type 字段完成。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProtocolSocketHandle {
    inner: smoltcp::iface::SocketHandle,
}

impl core::fmt::Debug for ProtocolSocketHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ProtocolSocketHandle(..)")
    }
}

impl ProtocolSocketHandle {
    pub(crate) fn from_smoltcp(inner: smoltcp::iface::SocketHandle) -> Self {
        Self { inner }
    }

    pub(crate) fn into_smoltcp(self) -> smoltcp::iface::SocketHandle {
        self.inner
    }
}
