//! 动态组件生命周期的格式无关状态机。

use alloc::vec::Vec;

use crate::{status, wire};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentTlsReservation {
    offset: usize,
    end: usize,
    identity: u64,
}

impl ComponentTlsReservation {
    pub const fn offset(self) -> usize {
        self.offset
    }

    pub const fn size(self) -> usize {
        self.end - self.offset
    }

    pub const fn identity(self) -> u64 {
        self.identity
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentTlsAllocator {
    capacity: usize,
    initial_used: usize,
    next_identity: u64,
    reservations: Vec<ComponentTlsReservation>,
}

impl ComponentTlsAllocator {
    pub fn new(capacity: usize, initial_used: usize) -> Option<Self> {
        if initial_used > capacity {
            return None;
        }
        Some(Self {
            capacity,
            initial_used,
            next_identity: 1,
            reservations: Vec::new(),
        })
    }

    pub fn reserve(
        &mut self,
        memory_size: usize,
        alignment: usize,
    ) -> Option<ComponentTlsReservation> {
        if memory_size == 0 || !alignment.is_power_of_two() {
            return None;
        }
        let page_size = crate::registry::PAGE_SIZE as usize;
        let effective_alignment = alignment.max(page_size);
        let size = align_up(memory_size, page_size)?;
        let mut cursor = self.initial_used;
        let mut insertion = self.reservations.len();
        let mut selected = None;
        for (index, reservation) in self.reservations.iter().enumerate() {
            let offset = align_up(cursor, effective_alignment)?;
            let end = offset.checked_add(size)?;
            if end <= reservation.offset {
                insertion = index;
                selected = Some((offset, end));
                break;
            }
            cursor = cursor.max(reservation.end);
        }
        let (offset, end) = selected.unwrap_or_else(|| {
            let offset = align_up(cursor, effective_alignment).unwrap_or(usize::MAX);
            let end = offset.checked_add(size).unwrap_or(usize::MAX);
            (offset, end)
        });
        if end > self.capacity {
            return None;
        }
        self.reservations.try_reserve(1).ok()?;
        let next_identity = self.next_identity.checked_add(1)?;
        let reservation = ComponentTlsReservation {
            offset,
            end,
            identity: self.next_identity,
        };
        self.reservations.insert(insertion, reservation);
        self.next_identity = next_identity;
        Some(reservation)
    }

    pub fn rollback(&mut self, reservation: ComponentTlsReservation) -> bool {
        let Some(index) = self.reservations.iter().position(|candidate| {
            candidate.identity == reservation.identity
                && candidate.offset == reservation.offset
                && candidate.end == reservation.end
        }) else {
            return false;
        };
        self.reservations.remove(index);
        if self.next_identity == reservation.identity.saturating_add(1) {
            self.next_identity = reservation.identity;
        }
        true
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment.checked_sub(1)?)
        .map(|value| value & !(alignment - 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ComponentState {
    Preparing = wire::COMPONENT_STATE_PREPARING,
    Initializing = wire::COMPONENT_STATE_INITIALIZING,
    Active = wire::COMPONENT_STATE_ACTIVE,
    Draining = wire::COMPONENT_STATE_DRAINING,
    Finalizing = wire::COMPONENT_STATE_FINALIZING,
    Unloaded = wire::COMPONENT_STATE_UNLOADED,
    Failed = wire::COMPONENT_STATE_FAILED,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentLifecycleMachine {
    state: ComponentState,
    generation: u64,
}

impl ComponentLifecycleMachine {
    pub const fn new() -> Self {
        Self {
            state: ComponentState::Preparing,
            generation: 0,
        }
    }

    pub const fn state(&self) -> ComponentState {
        self.state
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn begin_initialization(&mut self) -> Result<(), u32> {
        if self.state != ComponentState::Preparing {
            return Err(status::COMPONENT_INVALID_TRANSACTION);
        }
        self.state = ComponentState::Initializing;
        Ok(())
    }

    pub fn activate(&mut self, lifecycle_status: u32) -> Result<(), u32> {
        if self.state != ComponentState::Initializing {
            return Err(status::COMPONENT_INVALID_TRANSACTION);
        }
        if lifecycle_status != status::OK {
            self.state = ComponentState::Failed;
            return Err(status::COMPONENT_LIFECYCLE_FAILED);
        }
        self.state = ComponentState::Active;
        self.generation = self.generation.saturating_add(1);
        Ok(())
    }

    pub fn begin_unload(
        &mut self,
        dependent_count: u32,
        self_active: bool,
        active_calls: u64,
    ) -> Result<bool, u32> {
        match self.state {
            ComponentState::Unloaded => return Err(status::COMPONENT_UNLOADED),
            ComponentState::Draining => return Ok(active_calls == 0),
            ComponentState::Active => {}
            _ => return Err(status::COMPONENT_INVALID_TRANSACTION),
        }
        if dependent_count != 0 {
            return Err(status::COMPONENT_IN_USE);
        }
        if self_active {
            return Err(status::COMPONENT_SELF_UNLOAD);
        }
        self.state = if active_calls == 0 {
            ComponentState::Finalizing
        } else {
            ComponentState::Draining
        };
        Ok(active_calls == 0)
    }

    pub fn calls_drained(&mut self, active_calls: u64) -> Result<(), u32> {
        if self.state != ComponentState::Draining {
            return Err(status::COMPONENT_INVALID_TRANSACTION);
        }
        if active_calls != 0 {
            return Err(status::COMPONENT_DRAINING);
        }
        self.state = ComponentState::Finalizing;
        Ok(())
    }

    pub const fn timeout(&self) -> u32 {
        if matches!(self.state, ComponentState::Draining) {
            status::COMPONENT_TIMEOUT
        } else {
            status::COMPONENT_INVALID_TRANSACTION
        }
    }

    pub fn finish(&mut self, lifecycle_status: u32) -> u32 {
        if self.state != ComponentState::Finalizing {
            return status::COMPONENT_INVALID_TRANSACTION;
        }
        self.state = ComponentState::Unloaded;
        self.generation = self.generation.saturating_add(1);
        if lifecycle_status == status::OK {
            status::OK
        } else {
            status::COMPONENT_LIFECYCLE_FAILED
        }
    }
}

impl Default for ComponentLifecycleMachine {
    fn default() -> Self {
        Self::new()
    }
}
