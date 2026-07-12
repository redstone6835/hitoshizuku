//! elm-mgr “模组菜单”使用的固定布局模型。
//!
//! ELM 可以声明 action、toggle、status 或 group 菜单项，用户态工具通过菜单快照构建统一
//! 管理界面。菜单项只描述展示文本、路由、owner 和 action id；实际执行仍进入受鉴权的
//! action provider，不能把菜单可见性当作操作权限。
//!
//! 所有字符串使用固定长度零结尾缓冲区。生产者必须拒绝超长 UTF-8，而不是静默生成歧义
//! 路由；消费者只读取首个零字节之前的内容。

use crate::ids::{ActionId, ElmId};

/// `ELM_MENU_LABEL_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MENU_LABEL_LEN: usize = 64;
/// `ELM_MENU_DESCRIPTION_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MENU_DESCRIPTION_LEN: usize = 128;
/// `ELM_MENU_ROUTE_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_MENU_ROUTE_LEN: usize = 64;

/// `ELM_MENU_FLAG_TODO` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MENU_FLAG_TODO: u32 = 1 << 0;
/// `ELM_MENU_FLAG_DISABLED` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MENU_FLAG_DISABLED: u32 = 1 << 1;
/// `ELM_MENU_FLAG_REQUIRES_SYS_ADMIN` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_MENU_FLAG_REQUIRES_SYS_ADMIN: u32 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// `ElmMenuItemKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmMenuItemKind {
    /// `Group` 表示 `ElmMenuItemKind` 的对象类别：`group`。
    Group = 1,
    /// `Action` 表示 `ElmMenuItemKind` 的对象类别：`action`。
    Action = 2,
    /// `Toggle` 表示 `ElmMenuItemKind` 的对象类别：`toggle`。
    Toggle = 3,
    /// `Status` 表示 `ElmMenuItemKind` 的对象类别：`status`。
    Status = 4,
}

impl ElmMenuItemKind {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Group),
            2 => Some(Self::Action),
            3 => Some(Self::Toggle),
            4 => Some(Self::Status),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMenuSnapshotHeader` 描述后续可变长记录区的头部；记录数量、尺寸与总缓冲区长度必须相互一致。
pub struct ElmMenuSnapshotHeader {
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// `item_entry_size` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub item_entry_size: u16,
    /// `item_count` 对应记录或资源的数量；解析器必须验证它与实际缓冲区长度一致。
    pub item_count: u32,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
}

impl ElmMenuSnapshotHeader {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(item_count: u32, generation: u64) -> Self {
        Self {
            abi_version: crate::ctl::ELM_CTL_ABI_VERSION,
            item_entry_size: core::mem::size_of::<ElmMenuItemSnapshot>() as u16,
            item_count,
            generation,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmMenuItemSnapshot` 是某一时刻的只读快照表示，不授予对原对象的所有权或长期引用。
pub struct ElmMenuItemSnapshot {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: u64,
    /// 拥有该对象的 cell id；所有生命周期和权限检查都归属于该 owner。
    pub owner: u64,
    /// 请求执行或审计记录中的动作编号。
    pub action: u64,
    /// 该记录、资源或关系的类别编码。
    pub kind: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// `label_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub label_len: u16,
    /// `description_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub description_len: u16,
    /// `route_len` 对应区域或资源的字节数量；参与运算前必须检查整数溢出。
    pub route_len: u16,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u16,
    /// 供管理界面展示的短标签，实际长度受固定缓冲区限制。
    pub label: [u8; ELM_MENU_LABEL_LEN],
    /// 供管理和诊断界面展示的说明文本。
    pub description: [u8; ELM_MENU_DESCRIPTION_LEN],
    /// 菜单或管理入口使用的稳定路由 identifier。
    pub route: [u8; ELM_MENU_ROUTE_LEN],
}

impl ElmMenuItemSnapshot {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(
        id: u64,
        owner: ElmId,
        action: ActionId,
        kind: ElmMenuItemKind,
        flags: u32,
        label: &str,
        description: &str,
        route: &str,
    ) -> Self {
        let mut out = Self {
            id,
            owner: owner.0,
            action: action.0,
            kind: kind_code(kind),
            flags,
            label_len: 0,
            description_len: 0,
            route_len: 0,
            reserved: 0,
            label: [0; ELM_MENU_LABEL_LEN],
            description: [0; ELM_MENU_DESCRIPTION_LEN],
            route: [0; ELM_MENU_ROUTE_LEN],
        };
        out.label_len = copy_str(label, &mut out.label) as u16;
        out.description_len = copy_str(description, &mut out.description) as u16;
        out.route_len = copy_str(route, &mut out.route) as u16;
        out
    }
}

/// 执行 `kind_code` 定义的模型或协议操作；返回值反映校验后的结果。
pub const fn kind_code(kind: ElmMenuItemKind) -> u32 {
    match kind {
        ElmMenuItemKind::Group => 1,
        ElmMenuItemKind::Action => 2,
        ElmMenuItemKind::Toggle => 3,
        ElmMenuItemKind::Status => 4,
    }
}

fn copy_str(src: &str, dst: &mut [u8]) -> usize {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len());
    dst[..n].copy_from_slice(&bytes[..n]);
    n
}
