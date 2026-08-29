//! x86_64 UEFI EFI-stub 适配器的协议边界。
//!
//! EFI firmware enters an x86_64 image in long mode with `(ImageHandle,
//! SystemTable)`. The actual PE/COFF entry and `ExitBootServices` sequence must
//! be supplied by the platform loader. This module deliberately exposes only
//! validated, integer metadata and conversion helpers; it never calls firmware
//! through an unchecked function pointer.

use general::{StartMemoryRegion, StartMemoryRegionKind};

use super::boot_protocol::{
    BootProtocolError, EfiMemoryDescriptor, EfiMemoryMap, EfiStubArguments, X86BootProtocol,
};

#[cfg(target_os = "none")]
use core::mem::align_of;

/// UEFI system-table signature (`"IBI SYST"` little endian).
pub const EFI_SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
/// UEFI memory descriptor version currently understood by this adapter.
pub const EFI_MEMORY_DESCRIPTOR_VERSION: u32 = 1;

/// Firmware call sequence state. `ExitBootServices` must be the final Boot
/// Services operation before a context is handed to the kernel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EfiStubPhase {
    Entry,
    TableValidated,
    MemoryMapCaptured,
    BootServicesExited,
}

impl EfiStubPhase {
    pub const fn can_advance(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Entry, Self::TableValidated)
                | (Self::TableValidated, Self::MemoryMapCaptured)
                | (Self::MemoryMapCaptured, Self::BootServicesExited)
        )
    }
}

/// Errors raised when an EFI handoff tries to skip a required phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EfiStateError {
    /// The requested transition is not part of the firmware handoff protocol.
    InvalidTransition {
        from: EfiStubPhase,
        to: EfiStubPhase,
    },
    /// `ExitBootServices` returned `EFI_INVALID_PARAMETER` too many times.
    ExitRetryLimit,
}

/// Explicit state for the `GetMemoryMap`/`ExitBootServices` sequence.
///
/// The firmware invalidates a map key when an allocation changes the map.  A
/// failed `ExitBootServices` therefore has to return to `TableValidated`, take
/// a fresh map, and only then try the exit again.  Keeping this state separate
/// from the raw EFI structs makes the ordering testable on a hosted target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EfiHandoffState {
    phase: EfiStubPhase,
    exit_retries: u8,
}

impl EfiHandoffState {
    /// Maximum number of map-key refreshes attempted by the checked wrapper.
    pub const MAX_EXIT_RETRIES: u8 = 5;

    pub const fn new() -> Self {
        Self {
            phase: EfiStubPhase::Entry,
            exit_retries: 0,
        }
    }

    pub const fn phase(self) -> EfiStubPhase {
        self.phase
    }

    pub const fn exit_retries(self) -> u8 {
        self.exit_retries
    }

    pub fn advance(&mut self, next: EfiStubPhase) -> Result<(), EfiStateError> {
        if self.phase.can_advance(next) {
            self.phase = next;
            Ok(())
        } else {
            Err(EfiStateError::InvalidTransition {
                from: self.phase,
                to: next,
            })
        }
    }

    /// Re-arm the state machine after `EFI_INVALID_PARAMETER` from exit.
    pub fn retry_after_invalid_parameter(&mut self) -> Result<(), EfiStateError> {
        if self.phase != EfiStubPhase::MemoryMapCaptured {
            return Err(EfiStateError::InvalidTransition {
                from: self.phase,
                to: EfiStubPhase::TableValidated,
            });
        }
        if self.exit_retries >= Self::MAX_EXIT_RETRIES {
            return Err(EfiStateError::ExitRetryLimit);
        }
        self.exit_retries += 1;
        self.phase = EfiStubPhase::TableValidated;
        Ok(())
    }
}

impl Default for EfiHandoffState {
    fn default() -> Self {
        Self::new()
    }
}

/// 已验证的 EFI table header 字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EfiTableHeaderView {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
}

impl EfiTableHeaderView {
    pub fn validate(self) -> Result<(), BootProtocolError> {
        if self.signature != EFI_SYSTEM_TABLE_SIGNATURE {
            return Err(BootProtocolError::Invalid("EFI system table signature"));
        }
        if self.header_size < 24 {
            return Err(BootProtocolError::Invalid("EFI system table header size"));
        }
        if self.revision == 0 {
            return Err(BootProtocolError::Invalid("EFI system table revision"));
        }
        Ok(())
    }
}

/// EFI stub 交接中的稳定元数据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EfiStubHandoff {
    pub arguments: EfiStubArguments,
    pub system_table: EfiTableHeaderView,
    pub descriptor_size: usize,
    pub descriptor_version: u32,
}

impl EfiStubHandoff {
    pub fn validate(&self) -> Result<(), BootProtocolError> {
        self.arguments.validate()?;
        self.system_table.validate()?;
        if self.descriptor_version < EFI_MEMORY_DESCRIPTOR_VERSION {
            return Err(BootProtocolError::Unsupported(
                "EFI memory descriptor version",
            ));
        }
        if self.descriptor_size < 40 || !self.descriptor_size.is_multiple_of(8) {
            return Err(BootProtocolError::Invalid("EFI memory descriptor size"));
        }
        Ok(())
    }

    pub const fn protocol(&self) -> X86BootProtocol {
        X86BootProtocol::Efi
    }
}

/// Errors returned by the checked firmware wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EfiHandoffError {
    InvalidArguments,
    InvalidBuffer,
    InvalidTable,
    TableSnapshot,
    Protocol(BootProtocolError),
    FirmwareStatus(efi::EfiStatus),
    State(EfiStateError),
}

impl EfiHandoffError {
    /// Convert a local validation failure to the status an EFI image entry can
    /// return to firmware.  Firmware statuses are preserved verbatim.
    #[cfg(target_os = "none")]
    pub fn status(self) -> efi::EfiStatus {
        match self {
            Self::FirmwareStatus(status) => status,
            Self::InvalidArguments | Self::InvalidBuffer | Self::InvalidTable => {
                efi::status_invalid_parameter()
            }
            Self::TableSnapshot | Self::Protocol(BootProtocolError::Overflow(_)) => {
                efi::status_load_error()
            }
            Self::Protocol(BootProtocolError::Unsupported(_)) => efi::status_unsupported(),
            Self::Protocol(BootProtocolError::Invalid(_))
            | Self::Protocol(BootProtocolError::Truncated(_))
            | Self::State(_) => efi::status_invalid_parameter(),
        }
    }
}

/// 将 `GetMemoryMap` 返回的描述符数组转换为 `StartContext` 区域。
///
/// 该函数要求调用者在 `ExitBootServices` 成功后再交出结果；它不会自行
/// 改变 EFI 状态，也不会回收任何 firmware memory。
pub fn normalize_memory_map(
    bytes: &[u8],
    descriptor_size: usize,
    descriptor_version: u32,
) -> Result<alloc::vec::Vec<StartMemoryRegion>, BootProtocolError> {
    let map = EfiMemoryMap::new(bytes, descriptor_size, descriptor_version)?;
    map.regions()
}

/// Allocation-free variant used before the kernel allocator is online.
///
/// The descriptor count is checked before writing the destination, so a small
/// output slice never receives a misleading partial memory map.
pub fn normalize_memory_map_into(
    bytes: &[u8],
    descriptor_size: usize,
    descriptor_version: u32,
    output: &mut [StartMemoryRegion],
) -> Result<usize, BootProtocolError> {
    let map = EfiMemoryMap::new(bytes, descriptor_size, descriptor_version)?;
    let count = map.iter().count();
    if output.len() < count {
        return Err(BootProtocolError::Unsupported(
            "EFI memory-map output buffer capacity",
        ));
    }
    for (slot, descriptor) in output.iter_mut().zip(map.iter()) {
        *slot = descriptor.to_start_region()?;
    }
    Ok(count)
}

/// A validated memory-map snapshot.  The byte slice must remain stable until
/// all consumers have finished constructing `StartContext`.
#[derive(Clone, Copy, Debug)]
pub struct EfiMemoryMapSnapshot<'a> {
    pub bytes: &'a [u8],
    pub map_key: usize,
    pub descriptor_size: usize,
    pub descriptor_version: u32,
}

impl<'a> EfiMemoryMapSnapshot<'a> {
    pub fn validate(
        bytes: &'a [u8],
        map_key: usize,
        descriptor_size: usize,
        descriptor_version: u32,
    ) -> Result<Self, EfiHandoffError> {
        EfiMemoryMap::new(bytes, descriptor_size, descriptor_version)
            .map_err(EfiHandoffError::Protocol)?;
        Ok(Self {
            bytes,
            map_key,
            descriptor_size,
            descriptor_version,
        })
    }

    pub fn regions_into(
        self,
        output: &mut [StartMemoryRegion],
    ) -> Result<usize, BootProtocolError> {
        normalize_memory_map_into(
            self.bytes,
            self.descriptor_size,
            self.descriptor_version,
            output,
        )
    }
}

// The following wrappers are only emitted for the freestanding image.  Host
// tests exercise the state machine and parsers above without dereferencing a
// synthetic firmware pointer.
#[cfg(target_os = "none")]
#[derive(Clone, Copy)]
pub struct EfiPreflightSnapshot {
    pub system_table: &'static efi::EfiSystemTable,
    pub arguments: EfiStubArguments,
    pub memory_map: EfiMemoryMapSnapshot<'static>,
}

#[cfg(target_os = "none")]
#[derive(Clone, Copy)]
pub struct EfiBootSnapshot {
    pub system_table: &'static efi::EfiSystemTable,
    pub arguments: EfiStubArguments,
    pub memory_map: EfiMemoryMapSnapshot<'static>,
}

#[cfg(target_os = "none")]
fn validate_efi_entry(
    image_handle: usize,
    system_table: usize,
) -> Result<
    (
        EfiStubArguments,
        efi::EfiSystemTableView,
        &'static efi::EfiSystemTable,
    ),
    EfiHandoffError,
> {
    let arguments = EfiStubArguments::efi(image_handle, system_table, 0);
    arguments.validate().map_err(EfiHandoffError::Protocol)?;
    // `from_ptr` delegates pointer/signature/header checks to the C wrapper;
    // no raw firmware pointer is dereferenced until it has passed that gate.
    let view = unsafe { efi::EfiSystemTableView::from_ptr(system_table) }
        .ok_or(EfiHandoffError::InvalidTable)?;
    let table =
        efi::snapshot_system_table_static(view).map_err(|_| EfiHandoffError::TableSnapshot)?;
    let header = EfiTableHeaderView {
        signature: table.hdr.signature,
        revision: table.hdr.revision,
        header_size: table.hdr.header_size,
    };
    header.validate().map_err(EfiHandoffError::Protocol)?;
    Ok((arguments, view, table))
}

/// Capture a checked map while Boot Services are still active.
///
/// This is the operation used by the exported EFI entry when the current ELF
/// image cannot yet perform its own higher-half transition.  It deliberately
/// does not call `ExitBootServices`; callers that own the final page-table and
/// PE/COFF transition should use [`exit_boot_services_checked`].
#[cfg(target_os = "none")]
pub unsafe fn preflight_memory_map(
    image_handle: usize,
    system_table: usize,
    memory_map: &'static mut [u8],
) -> Result<EfiPreflightSnapshot, EfiHandoffError> {
    if memory_map.is_empty()
        || (memory_map.as_ptr() as usize) % align_of::<efi::EfiMemoryDescriptor>() != 0
    {
        return Err(EfiHandoffError::InvalidBuffer);
    }
    let (arguments, view, table) = validate_efi_entry(image_handle, system_table)?;
    let mut state = EfiHandoffState::new();
    state
        .advance(EfiStubPhase::TableValidated)
        .map_err(EfiHandoffError::State)?;
    let mut map_size = memory_map.len();
    let mut map_key = 0usize;
    let mut descriptor_size = 0usize;
    let mut descriptor_version = 0u32;
    let status = unsafe {
        efi::get_memory_map_retry(
            view.as_ptr() as *mut efi::EfiSystemTable,
            &mut map_size,
            memory_map.as_mut_ptr().cast(),
            &mut map_key,
            &mut descriptor_size,
            &mut descriptor_version,
        )
    };
    if !efi::status_is_success(status) {
        return Err(EfiHandoffError::FirmwareStatus(status));
    }
    let bytes = unsafe { core::slice::from_raw_parts(memory_map.as_ptr(), map_size) };
    let snapshot =
        EfiMemoryMapSnapshot::validate(bytes, map_key, descriptor_size, descriptor_version)?;
    state
        .advance(EfiStubPhase::MemoryMapCaptured)
        .map_err(EfiHandoffError::State)?;
    Ok(EfiPreflightSnapshot {
        system_table: table,
        arguments,
        memory_map: snapshot,
    })
}

/// Obtain the final memory map and perform the required `ExitBootServices`
/// map-key retry sequence.  The returned table is a static copy; all pointers
/// into Boot Services itself are considered invalid after this function returns.
#[cfg(target_os = "none")]
pub unsafe fn exit_boot_services_checked(
    image_handle: usize,
    system_table: usize,
    memory_map: &'static mut [u8],
) -> Result<EfiBootSnapshot, EfiHandoffError> {
    if memory_map.is_empty()
        || (memory_map.as_ptr() as usize) % align_of::<efi::EfiMemoryDescriptor>() != 0
    {
        return Err(EfiHandoffError::InvalidBuffer);
    }
    let (arguments, view, table) = validate_efi_entry(image_handle, system_table)?;
    let mut state = EfiHandoffState::new();
    state
        .advance(EfiStubPhase::TableValidated)
        .map_err(EfiHandoffError::State)?;
    let mut map_size: usize;
    let mut map_key = 0usize;
    let mut descriptor_size = 0usize;
    let mut descriptor_version = 0u32;
    let mut last_status = efi::status_load_error();
    let system_table_ptr = view.as_ptr() as *mut efi::EfiSystemTable;
    let image_handle_ptr = image_handle as efi::EfiHandle;

    for attempt in 0..=EfiHandoffState::MAX_EXIT_RETRIES {
        map_size = memory_map.len();
        let status = unsafe {
            efi::get_memory_map_retry(
                system_table_ptr,
                &mut map_size,
                memory_map.as_mut_ptr().cast(),
                &mut map_key,
                &mut descriptor_size,
                &mut descriptor_version,
            )
        };
        if !efi::status_is_success(status) {
            return Err(EfiHandoffError::FirmwareStatus(status));
        }
        let bytes = unsafe { core::slice::from_raw_parts(memory_map.as_ptr(), map_size) };
        // Validate the map before handing its key to firmware.
        EfiMemoryMap::new(bytes, descriptor_size, descriptor_version)
            .map_err(EfiHandoffError::Protocol)?;
        state
            .advance(EfiStubPhase::MemoryMapCaptured)
            .map_err(EfiHandoffError::State)?;

        last_status =
            unsafe { efi::exit_boot_services(system_table_ptr, image_handle_ptr, map_key) };
        if efi::status_is_success(last_status) {
            state
                .advance(EfiStubPhase::BootServicesExited)
                .map_err(EfiHandoffError::State)?;
            let snapshot = EfiMemoryMapSnapshot::validate(
                bytes,
                map_key,
                descriptor_size,
                descriptor_version,
            )?;
            return Ok(EfiBootSnapshot {
                system_table: table,
                arguments,
                memory_map: snapshot,
            });
        }
        if last_status != efi::status_invalid_parameter() {
            return Err(EfiHandoffError::FirmwareStatus(last_status));
        }
        if attempt == EfiHandoffState::MAX_EXIT_RETRIES {
            break;
        }
        state
            .retry_after_invalid_parameter()
            .map_err(EfiHandoffError::State)?;
    }
    Err(EfiHandoffError::FirmwareStatus(last_status))
}

/// 判断一段 EFI 区域是否能在 `ExitBootServices` 后交给物理分配器。
pub const fn usable_after_exit(kind: StartMemoryRegionKind) -> bool {
    kind.is_usable_after_handoff()
}

/// 以不变量检查的方式处理单个 EFI descriptor，供 loader 在需要逐条
/// 记录 provenance 时使用。
pub fn normalize_descriptor(
    descriptor: EfiMemoryDescriptor,
) -> Result<StartMemoryRegion, BootProtocolError> {
    descriptor.to_start_region()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_header_and_phase_requirements_are_strict() {
        let header = EfiTableHeaderView {
            signature: EFI_SYSTEM_TABLE_SIGNATURE,
            revision: 0x0002_0070,
            header_size: 24,
        };
        assert!(header.validate().is_ok());
        assert!(
            (EfiTableHeaderView {
                signature: 0,
                ..header
            })
            .validate()
            .is_err()
        );
        assert!(EfiStubPhase::Entry.can_advance(EfiStubPhase::TableValidated));
        assert!(!EfiStubPhase::Entry.can_advance(EfiStubPhase::BootServicesExited));
    }

    #[test]
    fn efi_handoff_rejects_bad_descriptor_metadata() {
        let handoff = EfiStubHandoff {
            arguments: EfiStubArguments::efi(1, 2, 0),
            system_table: EfiTableHeaderView {
                signature: EFI_SYSTEM_TABLE_SIGNATURE,
                revision: 1,
                header_size: 24,
            },
            descriptor_size: 40,
            descriptor_version: EFI_MEMORY_DESCRIPTOR_VERSION,
        };
        assert!(handoff.validate().is_ok());
        assert!(
            (EfiStubHandoff {
                descriptor_size: 41,
                ..handoff
            })
            .validate()
            .is_err()
        );
    }

    #[test]
    fn handoff_state_requires_a_fresh_map_after_key_invalidation() {
        let mut state = EfiHandoffState::new();
        assert_eq!(state.phase(), EfiStubPhase::Entry);
        state.advance(EfiStubPhase::TableValidated).unwrap();
        state.advance(EfiStubPhase::MemoryMapCaptured).unwrap();
        state.retry_after_invalid_parameter().unwrap();
        assert_eq!(state.phase(), EfiStubPhase::TableValidated);
        assert_eq!(state.exit_retries(), 1);
        assert!(state.advance(EfiStubPhase::BootServicesExited).is_err());
        state.advance(EfiStubPhase::MemoryMapCaptured).unwrap();
        state.advance(EfiStubPhase::BootServicesExited).unwrap();
        assert_eq!(state.phase(), EfiStubPhase::BootServicesExited);
    }

    #[test]
    fn handoff_state_has_a_bounded_retry_budget() {
        let mut state = EfiHandoffState::new();
        state.advance(EfiStubPhase::TableValidated).unwrap();
        for _ in 0..EfiHandoffState::MAX_EXIT_RETRIES {
            state.advance(EfiStubPhase::MemoryMapCaptured).unwrap();
            state.retry_after_invalid_parameter().unwrap();
        }
        state.advance(EfiStubPhase::MemoryMapCaptured).unwrap();
        assert_eq!(
            state.retry_after_invalid_parameter(),
            Err(EfiStateError::ExitRetryLimit)
        );
    }

    #[test]
    fn allocation_free_memory_map_normalization_checks_capacity() {
        let mut descriptor = [0u8; 40];
        descriptor[..4].copy_from_slice(
            &super::super::boot_protocol::efi_memory_type::CONVENTIONAL.to_le_bytes(),
        );
        descriptor[8..16].copy_from_slice(&0x20_0000u64.to_le_bytes());
        descriptor[24..32].copy_from_slice(&2u64.to_le_bytes());
        let mut output = [StartMemoryRegion::new(
            general::StartPhysRange::new(0, 1),
            StartMemoryRegionKind::Reserved,
            0,
        ); 1];
        assert_eq!(
            normalize_memory_map_into(&descriptor, 40, 1, &mut output),
            Ok(1)
        );
        assert_eq!(output[0].kind, StartMemoryRegionKind::UsableRam);
        assert_eq!(
            output[0].range,
            general::StartPhysRange::new(0x20_0000, 0x20_2000)
        );
        assert!(normalize_memory_map_into(&descriptor, 40, 1, &mut []).is_err());
    }
}
