//! ELM 运行时对象使用的强类型标识符。
//!
//! 所有标识符在线格式中都是 `u64`，但不同种类不能互换。值零统一保留为“无对象”或
//! “未分配”，运行时分配器不得产生零。标识符只在当前启动实例和对应对象生命周期内有效，
//! 不能当作内核地址，也不保证跨启动稳定。

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 一个 ELM cell 的运行时标识符。
pub struct ElmId(pub u64);

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Nexus 或 provider 端口的运行时标识符。
pub struct PortId(pub u64);

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 两个能力端点之间已提交绑定的运行时标识符。
pub struct BindingId(pub u64);

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// elm-mgr 菜单或可调用管理动作的运行时标识符。
pub struct ActionId(pub u64);

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 保护调用、绑定或资源生命周期的租约标识符。
pub struct LeaseId(pub u64);

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 同一逻辑 cell 的热替换代际。
///
/// cell id 在热替换前后保持不变，generation 在每次成功提交时递增。句柄、import、lease
/// 和调用必须同时匹配 cell id 与 generation，才能拒绝旧镜像遗留的陈旧引用。
pub struct Generation(pub u64);

/// 启动期内建根 `elm-mgr` cell 的保留 id。
pub const ELM_MGR_BUILTIN_ID: ElmId = ElmId(1);
/// 提供 EKI projection source 的内建子 ELM 保留 id。
pub const ELM_EKI_BUILTIN_ID: ElmId = ElmId(2);

impl Generation {
    /// 动态 cell 首次成功装载时使用的第一代。
    pub const FIRST: Self = Self(1);

    /// 计算下一代，并在 `u64` 溢出或结果为保留零值时返回 `None`。
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) if value != 0 => Some(Self(value)),
            _ => None,
        }
    }
}
