//! POSIX `dev_t` projection for device-like VFS nodes.
//!
//! This registry is intentionally part of the VFS compatibility layer.  The
//! core device model remains keyed by PnP identity and typed device objects,
//! not by major/minor numbers.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vfs::stat::DevId;
use vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosixDeviceKind {
    Char,
    Block,
}

#[derive(Clone, Debug)]
pub struct PosixDeviceRecord {
    pub kind: PosixDeviceKind,
    pub node_name: String,
    pub display_name: String,
    pub rdev: DevId,
}

struct PosixDeviceRegistry {
    next_char_minor: u32,
    next_block_minor: u32,
    records: Vec<PosixDeviceRecord>,
}

impl PosixDeviceRegistry {
    const fn new() -> Self {
        Self {
            next_char_minor: 1,
            next_block_minor: 1,
            records: Vec::new(),
        }
    }
}

static POSIX_DEVICES: Spinlock<PosixDeviceRegistry> = Spinlock::new(PosixDeviceRegistry::new());

const CHAR_MAJOR: u32 = 240;
const BLOCK_MAJOR: u32 = 241;

pub fn register_char(node_name: &str, display_name: &str) -> DevId {
    register(PosixDeviceKind::Char, node_name, display_name)
}

pub fn register_block(node_name: &str, display_name: &str) -> DevId {
    register(PosixDeviceKind::Block, node_name, display_name)
}

fn register(kind: PosixDeviceKind, node_name: &str, display_name: &str) -> DevId {
    let mut registry = POSIX_DEVICES.lock();
    if let Some(record) = registry
        .records
        .iter()
        .find(|record| record.kind == kind && record.node_name == node_name)
    {
        return record.rdev;
    }

    let rdev = match kind {
        PosixDeviceKind::Char => {
            let minor = registry.next_char_minor;
            registry.next_char_minor = registry.next_char_minor.saturating_add(1);
            DevId::new(CHAR_MAJOR, minor)
        }
        PosixDeviceKind::Block => {
            let minor = registry.next_block_minor;
            registry.next_block_minor = registry.next_block_minor.saturating_add(1);
            DevId::new(BLOCK_MAJOR, minor)
        }
    };
    registry.records.push(PosixDeviceRecord {
        kind,
        node_name: node_name.to_string(),
        display_name: display_name.to_string(),
        rdev,
    });
    rdev
}

pub fn unregister_node(node_name: &str) {
    POSIX_DEVICES
        .lock()
        .records
        .retain(|record| record.node_name != node_name);
}

pub fn lookup_node(node_name: &str) -> Option<PosixDeviceRecord> {
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.node_name == node_name)
        .cloned()
}

pub fn lookup_char(display_name: &str) -> Option<DevId> {
    lookup(PosixDeviceKind::Char, display_name)
}

pub fn lookup_block(display_name: &str) -> Option<DevId> {
    lookup(PosixDeviceKind::Block, display_name)
}

fn lookup(kind: PosixDeviceKind, display_name: &str) -> Option<DevId> {
    POSIX_DEVICES
        .lock()
        .records
        .iter()
        .find(|record| record.kind == kind && record.display_name == display_name)
        .map(|record| record.rdev)
}

pub fn records() -> Vec<PosixDeviceRecord> {
    POSIX_DEVICES.lock().records.clone()
}
