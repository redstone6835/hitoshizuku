//! ELM 单元角色的稳定线类型。
//!
//! kind 用于策略默认值、管理权限和可观测分类，不把单元锁死为某种实现方式。一个 Service
//! 仍可导出设备能力，一个 Driver 也可提供工具 export；真正可调用能力由契约、provider、
//! import/export 和 per-cell policy 决定。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// ELM 在运行时中的主要角色。
pub enum ElmKind {
    /// 管理型单元；只有额外通过信任与策略检查后才能取得 management 命名空间。
    Manager = 1,
    /// 向其他单元提供通用服务或工具能力的单元。
    Service = 2,
    /// 驱动或设备协议实现单元。
    Driver = 3,
    /// 主要附着到其他单元补缀点或扩展接口的单元。
    Extension = 4,
    /// 文件系统、VFS 扩展或存储命名空间相关单元。
    Filesystem = 5,
    /// 网络协议栈、网络设备服务或网络策略相关单元。
    Network = 6,
    /// 调试、诊断、追踪或验证用途单元。
    Debug = 7,
    /// 不适合以上分类的普通单元，不获得任何额外隐式权限。
    Other = 8,
}

impl ElmKind {
    /// 把稳定线格式判别值转换为 kind；未知值返回 `None`。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Manager),
            2 => Some(Self::Service),
            3 => Some(Self::Driver),
            4 => Some(Self::Extension),
            5 => Some(Self::Filesystem),
            6 => Some(Self::Network),
            7 => Some(Self::Debug),
            8 => Some(Self::Other),
            _ => None,
        }
    }

    /// 返回写入固定线格式的稳定 `u32` 判别值。
    pub const fn as_raw(self) -> u32 {
        self as u32
    }
}
