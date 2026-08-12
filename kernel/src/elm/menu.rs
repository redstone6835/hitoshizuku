//! `elm-mgr` 菜单运行时。

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use elm_model::{
    ActionId, ElmId, ElmMenuItemKind, ElmMenuItemSnapshot, ElmMenuSnapshotHeader, Generation,
};

#[derive(Debug, Clone)]
pub(crate) struct MenuItemRuntime {
    pub id: u64,
    pub owner: ElmId,
    pub action: ActionId,
    pub kind: ElmMenuItemKind,
    pub flags: u32,
    pub label: String,
    pub description: String,
    pub route: String,
}

impl MenuItemRuntime {
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
        Self {
            id,
            owner,
            action,
            kind,
            flags,
            label: label.to_string(),
            description: description.to_string(),
            route: route.to_string(),
        }
    }
}

pub(crate) fn menu_snapshot_bytes(items: &[MenuItemRuntime], generation: Generation) -> Vec<u8> {
    let header = ElmMenuSnapshotHeader::new(items.len() as u32, generation.0);
    let mut out = Vec::new();
    push_plain(&mut out, &header);
    for item in items {
        let entry = ElmMenuItemSnapshot::new(
            item.id,
            item.owner,
            item.action,
            item.kind,
            item.flags,
            &item.label,
            &item.description,
            &item.route,
        );
        push_plain(&mut out, &entry);
    }
    out
}

fn push_plain<T>(out: &mut Vec<u8>, value: &T) {
    let bytes = plain_bytes(value);
    out.extend_from_slice(bytes);
}

fn plain_bytes<T>(value: &T) -> &[u8] {
    // 安全性：菜单快照结构均为 `#[repr(C)]` 固定布局，不包含内核指针。
    unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    }
}
