//! 固件来源无关的 NUMA 拓扑快照。
//!
//! DTB/ACPI 启动层把已经校验的 CPU、内存与距离关系安装到这里。allocator、驱动和
//! sysfs 只消费稳定的本机范围，不需要重新解释固件表。

use alloc::vec::Vec;

use vfs::sync::Spinlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumaDistance {
    pub from: u32,
    pub to: u32,
    pub distance: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumaMemoryRange {
    pub start: usize,
    pub size: usize,
    pub node_id: u32,
}

impl NumaMemoryRange {
    pub const fn end(self) -> usize {
        self.start.saturating_add(self.size)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumaTopology {
    pub node_ids: Vec<u32>,
    pub distances: Vec<NumaDistance>,
    pub memory: Vec<NumaMemoryRange>,
}

impl NumaTopology {
    pub fn distance(&self, from: u32, to: u32) -> Option<u32> {
        self.distances
            .iter()
            .find(|entry| entry.from == from && entry.to == to)
            .or_else(|| {
                self.distances
                    .iter()
                    .find(|entry| entry.from == to && entry.to == from)
            })
            .map(|entry| entry.distance)
            .or_else(|| (from == to).then_some(fdt::NUMA_LOCAL_DISTANCE))
    }

    pub fn memory_node(&self, paddr: usize) -> Option<u32> {
        self.memory
            .iter()
            .find(|range| paddr >= range.start && paddr < range.end())
            .map(|range| range.node_id)
    }
}

static NUMA_TOPOLOGY: Spinlock<NumaTopology> = Spinlock::new(NumaTopology {
    node_ids: Vec::new(),
    distances: Vec::new(),
    memory: Vec::new(),
});

/// 安装启动固件 NUMA 拓扑。调用方必须先完成范围与距离校验。
pub fn install_topology(
    cpu_nodes: impl IntoIterator<Item = u32>,
    distances: Vec<NumaDistance>,
    memory: Vec<NumaMemoryRange>,
) {
    let mut node_ids = Vec::new();
    for node_id in cpu_nodes
        .into_iter()
        .chain(memory.iter().map(|range| range.node_id))
        .chain(distances.iter().flat_map(|entry| [entry.from, entry.to]))
    {
        if !node_ids.contains(&node_id) {
            node_ids.push(node_id);
        }
    }
    node_ids.sort_unstable();
    *NUMA_TOPOLOGY.lock() = NumaTopology {
        node_ids,
        distances,
        memory,
    };
}

pub fn snapshot_topology() -> NumaTopology {
    NUMA_TOPOLOGY.lock().clone()
}

#[kernel_symbols::export(
    name = "general.dev.numa.distance",
    contract = "kernel.general.numa-topology@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn distance(from: u32, to: u32) -> Option<u32> {
    NUMA_TOPOLOGY.lock().distance(from, to)
}

#[kernel_symbols::export(
    name = "general.dev.numa.memory_node",
    contract = "kernel.general.numa-topology@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn memory_node(paddr: usize) -> Option<u32> {
    NUMA_TOPOLOGY.lock().memory_node(paddr)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn topology_queries_symmetric_distance_and_memory_ranges() {
        let topology = NumaTopology {
            node_ids: vec![0, 1],
            distances: vec![NumaDistance {
                from: 0,
                to: 1,
                distance: 20,
            }],
            memory: vec![NumaMemoryRange {
                start: 0x1000,
                size: 0x2000,
                node_id: 1,
            }],
        };
        assert_eq!(topology.distance(1, 0), Some(20));
        assert_eq!(topology.distance(1, 1), Some(10));
        assert_eq!(topology.memory_node(0x2fff), Some(1));
        assert_eq!(topology.memory_node(0x3000), None);
    }
}
