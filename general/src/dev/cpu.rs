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
    /// CPU 直接子节点中启用的 interrupt-controller provider phandle。
    pub interrupt_controller_phandles: Box<[u32]>,
    pub compatible: Vec<Box<str>>,
    pub socket_id: Option<u32>,
    /// 从最外层到最内层的 `clusterN` 编号。
    ///
    /// DT CPU topology 的编号只在同一父节点内唯一，因此 core 的稳定身份必须
    /// 与完整 cluster ancestry 一起解释，不能把不同 cluster 下的 `core0`
    /// 合并。
    pub cluster_path: Box<[u32]>,
    pub core_id: Option<u32>,
    pub thread_id: Option<u32>,
    /// 固件 `capacity-dmips-mhz`；仅当所有 CPU 都提供有效值时才保留。
    pub capacity_dmips_mhz: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuNumaEntry {
    pub logical_id: u32,
    pub node_id: u32,
}

static CPU_TOPOLOGY: Spinlock<Vec<CpuTopologyEntry>> = Spinlock::new(Vec::new());
static CPU_NUMA_TOPOLOGY: Spinlock<Vec<CpuNumaEntry>> = Spinlock::new(Vec::new());

pub fn install_topology(entries: Vec<CpuTopologyEntry>) {
    *CPU_TOPOLOGY.lock() = entries;
}

pub fn snapshot_topology() -> Vec<CpuTopologyEntry> {
    CPU_TOPOLOGY.lock().clone()
}

pub fn install_numa_topology(entries: Vec<CpuNumaEntry>) {
    *CPU_NUMA_TOPOLOGY.lock() = entries;
}

pub fn snapshot_numa_topology() -> Vec<CpuNumaEntry> {
    CPU_NUMA_TOPOLOGY.lock().clone()
}

pub fn numa_node_for_logical_cpu(logical_id: u32) -> Option<u32> {
    CPU_NUMA_TOPOLOGY
        .lock()
        .iter()
        .find(|entry| entry.logical_id == logical_id)
        .map(|entry| entry.node_id)
}

fn cpu_reg_for_interrupt_controller_in(
    entries: &[CpuTopologyEntry],
    controller: u32,
) -> Option<u64> {
    cpu_for_interrupt_controller_in(entries, controller).map(|entry| entry.reg)
}

fn cpu_for_interrupt_controller_in(
    entries: &[CpuTopologyEntry],
    controller: u32,
) -> Option<&CpuTopologyEntry> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.interrupt_controller_phandles.contains(&controller));
    let entry = matches.next()?;
    matches.next().is_none().then_some(entry)
}

/// 把 CPU 本地 interrupt-controller provider 精确映射回物理 CPU/hart `reg`。
///
/// PLIC 等按 `interrupts-extended` 列举 per-CPU context 的 ELM 驱动必须比较
/// provider phandle，而不能把 context 序号误当成稀疏的物理 hart ID。
#[kernel_symbols::export(
    name = "general.dev.cpu.cpu_reg_for_interrupt_controller",
    contract = "kernel.general.cpu-topology@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn cpu_reg_for_interrupt_controller(controller: u32) -> Option<u64> {
    cpu_reg_for_interrupt_controller_in(&CPU_TOPOLOGY.lock(), controller)
}

/// 把 CPU 本地 interrupt-controller provider 映射为调度器逻辑 CPU 编号。
///
/// 级联控制器在每个 CPU 上选择不同 context 时必须使用逻辑编号索引运行期状态；
/// 物理 hart `reg` 可能稀疏，不能直接作为数组下标。
#[kernel_symbols::export(
    name = "general.dev.cpu.cpu_logical_id_for_interrupt_controller",
    contract = "kernel.general.cpu-topology@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn cpu_logical_id_for_interrupt_controller(controller: u32) -> Option<usize> {
    let logical_id = cpu_for_interrupt_controller_in(&CPU_TOPOLOGY.lock(), controller)?.logical_id;
    usize::try_from(logical_id).ok()
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn cpu(reg: u64, controllers: &[u32]) -> CpuTopologyEntry {
        CpuTopologyEntry {
            logical_id: reg as u32,
            reg,
            phandle: None,
            interrupt_controller_phandles: controllers.into(),
            compatible: vec![],
            socket_id: None,
            cluster_path: Box::new([]),
            core_id: None,
            thread_id: None,
            capacity_dmips_mhz: None,
        }
    }

    #[test]
    fn interrupt_controller_lookup_uses_provider_identity_not_cpu_ordinal() {
        let entries = [cpu(7, &[0x20]), cpu(42, &[0x30, 0x31])];
        assert_eq!(cpu_reg_for_interrupt_controller_in(&entries, 0x20), Some(7));
        assert_eq!(
            cpu_reg_for_interrupt_controller_in(&entries, 0x31),
            Some(42)
        );
        assert_eq!(
            cpu_for_interrupt_controller_in(&entries, 0x31).map(|entry| entry.logical_id),
            Some(42)
        );
        assert_eq!(cpu_reg_for_interrupt_controller_in(&entries, 0x99), None);
    }

    #[test]
    fn ambiguous_provider_identity_fails_closed() {
        let entries = [cpu(0, &[0x20]), cpu(1, &[0x20])];
        assert_eq!(cpu_reg_for_interrupt_controller_in(&entries, 0x20), None);
        assert!(cpu_for_interrupt_controller_in(&entries, 0x20).is_none());
    }
}
