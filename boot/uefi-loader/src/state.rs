//! Explicit UEFI handoff ordering.

use crate::elf::{ElfError, ElfImage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderPhase {
    Entry,
    TableValidated,
    KernelRead,
    ElfValidated,
    MemoryMapCaptured,
    BootServicesExited,
}

impl LoaderPhase {
    pub const fn can_advance(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Entry, Self::TableValidated)
                | (Self::TableValidated, Self::KernelRead)
                | (Self::KernelRead, Self::ElfValidated)
                | (Self::ElfValidated, Self::MemoryMapCaptured)
                | (Self::MemoryMapCaptured, Self::BootServicesExited)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateError {
    InvalidTransition { from: LoaderPhase, to: LoaderPhase },
    ExitRetryLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoaderState {
    phase: LoaderPhase,
    exit_retries: u8,
}

impl LoaderState {
    pub const MAX_EXIT_RETRIES: u8 = 5;

    pub const fn new() -> Self {
        Self {
            phase: LoaderPhase::Entry,
            exit_retries: 0,
        }
    }

    pub const fn phase(self) -> LoaderPhase {
        self.phase
    }

    pub const fn exit_retries(self) -> u8 {
        self.exit_retries
    }

    pub fn advance(&mut self, next: LoaderPhase) -> Result<(), StateError> {
        if self.phase.can_advance(next) {
            self.phase = next;
            Ok(())
        } else {
            Err(StateError::InvalidTransition {
                from: self.phase,
                to: next,
            })
        }
    }

    /// A failed `ExitBootServices` invalidates the map key.  The caller must
    /// capture a fresh map before trying again.
    pub fn retry_after_invalid_parameter(&mut self) -> Result<(), StateError> {
        if self.phase != LoaderPhase::MemoryMapCaptured {
            return Err(StateError::InvalidTransition {
                from: self.phase,
                to: LoaderPhase::ElfValidated,
            });
        }
        if self.exit_retries >= Self::MAX_EXIT_RETRIES {
            return Err(StateError::ExitRetryLimit);
        }
        self.exit_retries += 1;
        self.phase = LoaderPhase::ElfValidated;
        Ok(())
    }
}

impl Default for LoaderState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffError {
    InvalidState(StateError),
    InvalidElf(ElfError),
}

/// Parse a kernel image before the firmware allocation and copy phase.
pub fn inspect_kernel<'a>(
    state: &mut LoaderState,
    bytes: &'a [u8],
) -> Result<ElfImage<'a>, HandoffError> {
    state
        .advance(LoaderPhase::TableValidated)
        .map_err(HandoffError::InvalidState)?;
    state
        .advance(LoaderPhase::KernelRead)
        .map_err(HandoffError::InvalidState)?;
    let image = ElfImage::parse(bytes).map_err(HandoffError::InvalidElf)?;
    state
        .advance(LoaderPhase::ElfValidated)
        .map_err(HandoffError::InvalidState)?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_requires_order_and_retries_stale_keys() {
        let mut state = LoaderState::new();
        assert!(state.advance(LoaderPhase::MemoryMapCaptured).is_err());
        state.advance(LoaderPhase::TableValidated).unwrap();
        state.advance(LoaderPhase::KernelRead).unwrap();
        state.advance(LoaderPhase::ElfValidated).unwrap();
        state.advance(LoaderPhase::MemoryMapCaptured).unwrap();
        state.retry_after_invalid_parameter().unwrap();
        assert_eq!(state.phase(), LoaderPhase::ElfValidated);
        assert_eq!(state.exit_retries(), 1);
    }
}
