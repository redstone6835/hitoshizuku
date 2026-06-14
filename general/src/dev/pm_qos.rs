//! Power-management QoS constraints.
//!
//! Linux exposes `/dev/cpu_dma_latency` as a userspace handle over the PM QoS
//! CPU DMA latency class. The device file ABI lives in `vfs::device_files`;
//! this module only keeps typed latency requests and the aggregated effective
//! value, so future idle/cpuidle code can consume the same state without
//! knowing about device paths or ioctl/read/write ABI details.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vfs::sync::Spinlock;

/// Linux PM QoS default CPU DMA latency value in microseconds.
///
/// This represents "no active userspace constraint" for the compatibility
/// device. The value is intentionally named and centralized instead of being
/// duplicated in device-file code.
pub const CPU_DMA_LATENCY_DEFAULT_US: i32 = 2_000_000_000;

/// Latency QoS classes currently tracked by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyQosClass {
    CpuDmaLatency,
}

/// PM QoS state-management errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyQosError {
    Invalid,
    NoDevice,
    NoMemory,
}

#[derive(Clone, Copy)]
struct RequestSlot {
    generation: u64,
    value_us: i32,
    active: bool,
}

impl RequestSlot {
    const fn inactive() -> Self {
        Self {
            generation: 0,
            value_us: CPU_DMA_LATENCY_DEFAULT_US,
            active: false,
        }
    }
}

struct LatencyQosState {
    next_generation: u64,
    effective_us: i32,
    slots: Vec<RequestSlot>,
}

impl LatencyQosState {
    const fn new() -> Self {
        Self {
            next_generation: 1,
            effective_us: CPU_DMA_LATENCY_DEFAULT_US,
            slots: Vec::new(),
        }
    }

    fn alloc_slot(&mut self) -> Result<(usize, u64), LatencyQosError> {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);

        if let Some((index, slot)) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| !slot.active)
        {
            *slot = RequestSlot {
                generation,
                value_us: CPU_DMA_LATENCY_DEFAULT_US,
                active: true,
            };
            return Ok((index, generation));
        }

        self.slots
            .try_reserve(1)
            .map_err(|_| LatencyQosError::NoMemory)?;
        let index = self.slots.len();
        self.slots.push(RequestSlot {
            generation,
            value_us: CPU_DMA_LATENCY_DEFAULT_US,
            active: true,
        });
        Ok((index, generation))
    }

    fn update_slot(
        &mut self,
        slot_id: usize,
        generation: u64,
        value_us: i32,
    ) -> Result<(), LatencyQosError> {
        let Some(slot) = self.slots.get_mut(slot_id) else {
            return Err(LatencyQosError::NoDevice);
        };
        if !slot.active || slot.generation != generation {
            return Err(LatencyQosError::NoDevice);
        }
        slot.value_us = value_us;
        self.recompute_effective();
        Ok(())
    }

    fn remove_slot(&mut self, slot_id: usize, generation: u64) {
        let Some(slot) = self.slots.get_mut(slot_id) else {
            return;
        };
        if !slot.active || slot.generation != generation {
            return;
        }
        *slot = RequestSlot::inactive();
        self.recompute_effective();
    }

    fn recompute_effective(&mut self) {
        self.effective_us = self
            .slots
            .iter()
            .filter(|slot| slot.active)
            .map(|slot| slot.value_us)
            .min()
            .unwrap_or(CPU_DMA_LATENCY_DEFAULT_US);
    }
}

static CPU_DMA_LATENCY_QOS: Spinlock<LatencyQosState> = Spinlock::new(LatencyQosState::new());

fn state_for(class: LatencyQosClass) -> &'static Spinlock<LatencyQosState> {
    match class {
        LatencyQosClass::CpuDmaLatency => &CPU_DMA_LATENCY_QOS,
    }
}

/// One open PM QoS latency request.
///
/// The handle owns a single request slot. Dropping or explicitly releasing it
/// removes the request, which matches the `/dev/cpu_dma_latency` file lifetime.
pub struct LatencyConstraintHandle {
    class: LatencyQosClass,
    slot_id: usize,
    generation: u64,
    released: AtomicBool,
}

impl LatencyConstraintHandle {
    /// Update this request's latency value in microseconds.
    pub fn update_us(&self, value_us: i32) -> Result<(), LatencyQosError> {
        if value_us < 0 {
            return Err(LatencyQosError::Invalid);
        }
        if self.released.load(Ordering::Acquire) {
            return Err(LatencyQosError::NoDevice);
        }
        state_for(self.class)
            .lock()
            .update_slot(self.slot_id, self.generation, value_us)
    }

    /// Remove this request if it is still active.
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        state_for(self.class)
            .lock()
            .remove_slot(self.slot_id, self.generation);
    }
}

impl Drop for LatencyConstraintHandle {
    fn drop(&mut self) {
        self.release();
    }
}

/// Open a new latency request for `class`.
pub fn open_latency_request(
    class: LatencyQosClass,
) -> Result<LatencyConstraintHandle, LatencyQosError> {
    let (slot_id, generation) = state_for(class).lock().alloc_slot()?;
    Ok(LatencyConstraintHandle {
        class,
        slot_id,
        generation,
        released: AtomicBool::new(false),
    })
}

/// Return the effective latency constraint for `class`.
pub fn effective_latency_us(class: LatencyQosClass) -> i32 {
    state_for(class).lock().effective_us
}

/// Convenience accessor for the Linux `/dev/cpu_dma_latency` class.
pub fn cpu_dma_latency_effective_us() -> i32 {
    effective_latency_us(LatencyQosClass::CpuDmaLatency)
}
