//! ELM 运行拓扑快照导出。

use alloc::vec::Vec;

use elm_model::{ElmCellSnapshot, ElmPortSnapshot, ElmSnapshotHeader};

use super::core::ElmCore;

pub(crate) fn snapshot_bytes(core: &ElmCore) -> Vec<u8> {
    let header = ElmSnapshotHeader::new(
        core.cells().len() as u32,
        core.ports().len() as u32,
        core.lease_count() as u32,
        core.last_event_sequence(),
    );
    let mut out = Vec::new();
    push_plain(&mut out, &header);
    for cell in core.cells() {
        let entry = ElmCellSnapshot::new(
            cell.id,
            cell.parent,
            cell.state,
            cell.kind,
            cell.generation,
            cell.name.as_str(),
            cell.ebi_arch,
            cell.ebi_status,
            cell.has_native_code,
        );
        push_plain(&mut out, &entry);
    }
    for port in core.ports() {
        let desc = port.desc;
        let entry = ElmPortSnapshot::new(
            desc.id,
            desc.owner,
            desc.contract,
            desc.direction,
            desc.mode,
            desc.implemented,
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
    // 安全性：调用点只传入 ELM 控制面 `#[repr(C)]` 固定布局结构，
    // 这些结构不包含指针引用，按原始字节导出给用户态工具。
    unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    }
}
