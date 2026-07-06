//! 单元管理器菜单固定布局模型。

use crate::ids::{ActionId, ElmId};

pub const ELM_MENU_LABEL_LEN: usize = 64;
pub const ELM_MENU_DESCRIPTION_LEN: usize = 128;
pub const ELM_MENU_ROUTE_LEN: usize = 64;

pub const ELM_MENU_FLAG_TODO: u32 = 1 << 0;
pub const ELM_MENU_FLAG_DISABLED: u32 = 1 << 1;
pub const ELM_MENU_FLAG_REQUIRES_SYS_ADMIN: u32 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmMenuItemKind {
    Group = 1,
    Action = 2,
    Toggle = 3,
    Status = 4,
}

impl ElmMenuItemKind {
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
pub struct ElmMenuSnapshotHeader {
    pub abi_version: u16,
    pub item_entry_size: u16,
    pub item_count: u32,
    pub generation: u64,
}

impl ElmMenuSnapshotHeader {
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
pub struct ElmMenuItemSnapshot {
    pub id: u64,
    pub owner: u64,
    pub action: u64,
    pub kind: u32,
    pub flags: u32,
    pub label_len: u16,
    pub description_len: u16,
    pub route_len: u16,
    pub reserved: u16,
    pub label: [u8; ELM_MENU_LABEL_LEN],
    pub description: [u8; ELM_MENU_DESCRIPTION_LEN],
    pub route: [u8; ELM_MENU_ROUTE_LEN],
}

impl ElmMenuItemSnapshot {
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
