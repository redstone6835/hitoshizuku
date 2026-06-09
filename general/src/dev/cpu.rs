//! 固件 CPU 拓扑登记。
//!
//! CPU 不通过 `/dev` 暴露，但它仍是固件拓扑的一部分。DTB/ACPI 启动路径把解析到
//! 的 socket/core/thread 关系安装到这里，sysfs、调度器或 NUMA 代码后续可以通过
//! typed snapshot 使用这些信息，而不是重新解析固件表。

use alloc::boxed::Box;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuTopologyEntry {
    pub logical_id: u32,
    pub reg: u64,
    pub phandle: Option<u32>,
    pub compatible: Vec<Box<str>>,
    pub socket_id: Option<u32>,
    pub core_id: Option<u32>,
    pub thread_id: Option<u32>,
}

static CPU_TOPOLOGY: Spinlock<Vec<CpuTopologyEntry>> = Spinlock::new(Vec::new());

pub fn install_topology(entries: Vec<CpuTopologyEntry>) {
    *CPU_TOPOLOGY.lock() = entries;
}

pub fn snapshot_topology() -> Vec<CpuTopologyEntry> {
    CPU_TOPOLOGY.lock().clone()
}
