//! 任务迁移事务的通用上下文。

use crate::cpu::CpuId;
use crate::placement::PlacementSnapshot;

/// 一次任务迁移从 source placement 到目标 CPU 所需的不可变上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationContext {
    pub source: PlacementSnapshot,
    pub target_cpu: CpuId,
    pub target_domain: usize,
    pub topology_generation: u64,
}
