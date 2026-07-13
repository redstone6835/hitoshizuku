//! 任务调度归属的一致原子快照。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::cpu::{CpuId, ROOT_SCHED_DOMAIN_ID};

const CPU_NONE: u64 = u8::MAX as u64;
const BYTE_MASK: u64 = u8::MAX as u64;
const CPU_SHIFT: u32 = 0;
const DOMAIN_SHIFT: u32 = 8;
const STATE_SHIFT: u32 = 16;
const GENERATION_SHIFT: u32 = 24;
const GENERATION_MASK: u64 = (1u64 << 40) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PlacementState {
    Unbound = 0,
    Bound = 1,
    Migrating = 2,
    OfflineRepair = 3,
}

impl PlacementState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Bound,
            2 => Self::Migrating,
            3 => Self::OfflineRepair,
            _ => Self::Unbound,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementSnapshot {
    pub cpu: Option<CpuId>,
    pub domain_id: usize,
    pub topology_generation: u64,
    pub state: PlacementState,
}

impl PlacementSnapshot {
    pub const fn unbound() -> Self {
        Self {
            cpu: None,
            domain_id: ROOT_SCHED_DOMAIN_ID,
            topology_generation: 0,
            state: PlacementState::Unbound,
        }
    }
}

pub struct TaskPlacement {
    encoded: AtomicU64,
}

impl TaskPlacement {
    pub const fn unbound() -> Self {
        Self {
            encoded: AtomicU64::new(encode(PlacementSnapshot::unbound())),
        }
    }

    pub fn snapshot(&self) -> PlacementSnapshot {
        decode(self.encoded.load(Ordering::Acquire))
    }

    pub(crate) fn bind(&self, cpu: CpuId, domain_id: usize, topology_generation: u64) {
        self.encoded.store(
            encode(PlacementSnapshot {
                cpu: Some(cpu),
                domain_id,
                topology_generation,
                state: PlacementState::Bound,
            }),
            Ordering::Release,
        );
    }
}

const fn encode(snapshot: PlacementSnapshot) -> u64 {
    let cpu = match snapshot.cpu {
        Some(cpu) => cpu.get() as u64,
        None => CPU_NONE,
    };
    (cpu << CPU_SHIFT)
        | (((snapshot.domain_id as u64) & BYTE_MASK) << DOMAIN_SHIFT)
        | ((snapshot.state as u64) << STATE_SHIFT)
        | ((snapshot.topology_generation & GENERATION_MASK) << GENERATION_SHIFT)
}

fn decode(encoded: u64) -> PlacementSnapshot {
    let cpu_raw = (encoded >> CPU_SHIFT) & BYTE_MASK;
    PlacementSnapshot {
        cpu: if cpu_raw == CPU_NONE {
            None
        } else {
            CpuId::new(cpu_raw as usize)
        },
        domain_id: ((encoded >> DOMAIN_SHIFT) & BYTE_MASK) as usize,
        topology_generation: (encoded >> GENERATION_SHIFT) & GENERATION_MASK,
        state: PlacementState::from_raw(((encoded >> STATE_SHIFT) & BYTE_MASK) as u8),
    }
}
