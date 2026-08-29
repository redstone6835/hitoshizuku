#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! Independent x86_64 UEFI loader for the fixed-address Hitoshizuku kernel.
//!
//! It reads `\\EFI\\HITOSHI\\KERNEL.ELF`, allocates every PT_LOAD at its
//! declared physical address, creates a bounded Multiboot2 handoff, and never
//! calls firmware after ExitBootServices.  A copied low-memory trampoline
//! installs an identity map, returns to 32-bit protected mode, and enters the
//! existing Multiboot2 `_start` without changing the kernel boot assembly.

use core::ffi::c_void;
use core::mem::{size_of, transmute};
use core::ptr::{copy_nonoverlapping, write_bytes};

use uefi::{BootServices, FileProtocol, Guid, MemoryDescriptor, PhysicalAddress};

pub mod elf;
pub mod state;
pub mod uefi;

pub use elf::{ElfError, ElfImage, ElfLoadSegment};
pub use state::{HandoffError, LoaderPhase, LoaderState};
pub use uefi::{EfiStatus, Handle, SystemTable, TableError};

const PAGE_SIZE: usize = 4096;
const LOW_4G_MAX: u64 = 0xffff_ffff;
const LOW_4G_LIMIT: u64 = LOW_4G_MAX + 1;
const MAX_DEFERRED_LOADS: usize = 8;
const KERNEL_ENTRY_PHYS: u64 = 0x0180_0000;
const KERNEL_ENTRY_VIRT: u64 = 0xffff_ffff_8180_0000;
const KERNEL_PATH: &[u16] = &[
    92, 69, 70, 73, 92, 72, 73, 84, 79, 83, 72, 73, 92, 75, 69, 82, 78, 69, 76, 46, 69, 76, 70, 0,
];
const KERNEL_CMDLINE: &[u8] = b"console=ttyS0\0";
const MAX_KERNEL_FILE_BYTES: usize = 128 * 1024 * 1024;
const MEMORY_MAP_BYTES: usize = 512 * 1024;
const MULTIBOOT_INFO_BYTES: usize = 512 * 1024;
const RSDP_COPY_BYTES: usize = 4096;
const LOW_STACK_BYTES: usize = 64 * 1024;
const IDENTITY_PAGE_TABLE_BYTES: usize = 8 * PAGE_SIZE;
const MAX_MB2_MEMORY_ENTRIES: usize = 256;
const MB2_TAG_END: u32 = 0;
const MB2_TAG_CMDLINE: u32 = 1;
const MB2_TAG_MEMORY_MAP: u32 = 6;
const MB2_TAG_ACPI_NEW: u32 = 15;
const MB2_MEMORY_AVAILABLE: u32 = 1;
const MB2_MEMORY_RESERVED: u32 = 2;
const MB2_MEMORY_ACPI_RECLAIMABLE: u32 = 3;
const MB2_MEMORY_NVS: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct TrampolineDescriptor {
    source: u64,
    target: u64,
    file_size: u64,
    memory_size: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TrampolineParams {
    page_table_root: u64,
    deferred_count: u64,
    deferred: [TrampolineDescriptor; MAX_DEFERRED_LOADS],
    stack_top: u32,
    multiboot_info: u32,
    entry: u32,
}

const TRAMPOLINE_STACK_OFFSET: usize = core::mem::offset_of!(TrampolineParams, stack_top);
const TRAMPOLINE_MB2_OFFSET: usize = core::mem::offset_of!(TrampolineParams, multiboot_info);
const TRAMPOLINE_ENTRY_OFFSET: usize = core::mem::offset_of!(TrampolineParams, entry);
const _: () = assert!(
    TRAMPOLINE_STACK_OFFSET == 272
        && TRAMPOLINE_MB2_OFFSET == 276
        && TRAMPOLINE_ENTRY_OFFSET == 280
);

#[derive(Clone, Copy)]
struct DeferredLoad {
    source: u64,
    target: u64,
    file_size: u64,
    memory_size: u64,
    is_ap_trampoline: bool,
}

#[derive(Clone, Copy)]
struct DeferredLoads {
    entries: [DeferredLoad; MAX_DEFERRED_LOADS],
    count: usize,
}

impl DeferredLoads {
    const EMPTY: DeferredLoad = DeferredLoad {
        source: 0,
        target: 0,
        file_size: 0,
        memory_size: 0,
        is_ap_trampoline: false,
    };

    const fn new() -> Self {
        Self {
            entries: [Self::EMPTY; MAX_DEFERRED_LOADS],
            count: 0,
        }
    }

    fn push(&mut self, entry: DeferredLoad) -> Result<(), LoaderError> {
        if self.count == MAX_DEFERRED_LOADS {
            return Err(LoaderError::Invalid("too many deferred PT_LOAD segments"));
        }
        self.entries[self.count] = entry;
        self.count += 1;
        Ok(())
    }
}

const fn deferred_target_memory_is_reusable(load: DeferredLoad, memory_type: u32) -> bool {
    matches!(
        memory_type,
        uefi::EFI_LOADER_CODE
            | uefi::EFI_LOADER_DATA
            | uefi::EFI_BOOT_SERVICES_CODE
            | uefi::EFI_BOOT_SERVICES_DATA
            | uefi::EFI_CONVENTIONAL_MEMORY
    ) || (load.is_ap_trampoline && memory_type == 0)
}

fn validate_handoff_ranges(
    deferred: DeferredLoads,
    ranges: &[(u64, usize)],
) -> Result<(), LoaderError> {
    for load in &deferred.entries[..deferred.count] {
        let end = load
            .target
            .checked_add(load.memory_size)
            .ok_or(LoaderError::Overflow("deferred target range"))?;
        for &(start, size) in ranges {
            let range_end = start
                .checked_add(size as u64)
                .ok_or(LoaderError::Overflow("handoff range"))?;
            if start < end && load.target < range_end {
                return Err(LoaderError::Invalid(
                    "handoff allocation overlaps deferred PT_LOAD target",
                ));
            }
        }
        for source in &deferred.entries[..deferred.count] {
            let source_end = source
                .source
                .checked_add(source.memory_size)
                .ok_or(LoaderError::Overflow("staging source range"))?;
            if source.source < end && load.target < source_end {
                return Err(LoaderError::Invalid(
                    "deferred PT_LOAD target overlaps staging source",
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoaderError {
    Firmware(EfiStatus),
    Elf(ElfError),
    Invalid(&'static str),
    Overflow(&'static str),
}

impl LoaderError {
    const fn status(self) -> EfiStatus {
        match self {
            Self::Firmware(status) => status,
            Self::Elf(ElfError::Truncated(_)) | Self::Elf(ElfError::Invalid(_)) => {
                uefi::EFI_LOAD_ERROR
            }
            Self::Elf(ElfError::Unsupported(_)) | Self::Invalid(_) => uefi::EFI_UNSUPPORTED,
            Self::Elf(ElfError::Overflow(_)) | Self::Overflow(_) => uefi::EFI_BAD_BUFFER_SIZE,
        }
    }
}

#[inline]
fn firmware(status: EfiStatus) -> Result<(), LoaderError> {
    if uefi::is_error(status) {
        Err(LoaderError::Firmware(status))
    } else {
        Ok(())
    }
}

#[inline]
fn pages_for(bytes: usize) -> Result<usize, LoaderError> {
    bytes
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value / PAGE_SIZE)
        .filter(|pages| *pages != 0)
        .ok_or(LoaderError::Overflow("page allocation size"))
}

unsafe fn boot_services(table: *mut SystemTable) -> Result<&'static BootServices, LoaderError> {
    let table = unsafe { table.as_ref() }.ok_or(LoaderError::Invalid("null system table"))?;
    unsafe { table.boot_services() }.map_err(|_| LoaderError::Invalid("invalid system table"))
}

unsafe fn handle_protocol<T>(
    boot: &BootServices,
    handle: Handle,
    guid: Guid,
) -> Result<*mut T, LoaderError> {
    let protocol = boot
        .handle_protocol
        .ok_or(LoaderError::Invalid("HandleProtocol missing"))?;
    let mut interface: *mut c_void = core::ptr::null_mut();
    firmware(unsafe { protocol(handle, &guid, &mut interface) })?;
    if interface.is_null() {
        return Err(LoaderError::Invalid(
            "HandleProtocol returned null interface",
        ));
    }
    Ok(interface.cast())
}

unsafe fn allocate_pages_at(
    boot: &BootServices,
    address: u64,
    bytes: usize,
    memory_type: u32,
) -> Result<PhysicalAddress, LoaderError> {
    if address & (PAGE_SIZE as u64 - 1) != 0 {
        return Err(LoaderError::Invalid("fixed allocation is not page aligned"));
    }
    let allocate = boot
        .allocate_pages
        .ok_or(LoaderError::Invalid("AllocatePages missing"))?;
    let mut output = address;
    firmware(unsafe {
        allocate(
            uefi::EFI_ALLOCATE_ADDRESS,
            memory_type,
            pages_for(bytes)?,
            &mut output,
        )
    })?;
    if output != address {
        return Err(LoaderError::Invalid(
            "AllocateAddress returned a different address",
        ));
    }
    Ok(output)
}

unsafe fn allocate_low_pages(
    boot: &BootServices,
    bytes: usize,
    memory_type: u32,
) -> Result<PhysicalAddress, LoaderError> {
    let allocate = boot
        .allocate_pages
        .ok_or(LoaderError::Invalid("AllocatePages missing"))?;
    let pages = pages_for(bytes)?;
    let allocated_bytes = pages
        .checked_mul(PAGE_SIZE)
        .ok_or(LoaderError::Overflow("low allocation size"))?;
    let mut output = LOW_4G_MAX;
    firmware(unsafe {
        allocate(
            uefi::EFI_ALLOCATE_MAX_ADDRESS,
            memory_type,
            pages,
            &mut output,
        )
    })?;
    validate_low_allocation(output, allocated_bytes)?;
    Ok(output)
}

/// Validate the complete page range returned by `AllocatePages`.
///
/// The handoff ABI stores these addresses in 32-bit fields.  An exclusive
/// endpoint at exactly 4 GiB would therefore wrap to zero when it is used as
/// a stack top or Multiboot pointer, so the endpoint is intentionally kept
/// strictly below the 4 GiB limit.  Page zero is rejected because the
/// trampoline treats null physical addresses as invalid.
fn validate_low_allocation(output: u64, allocated_bytes: usize) -> Result<(), LoaderError> {
    if output == 0 || output % PAGE_SIZE as u64 != 0 {
        return Err(LoaderError::Invalid("invalid low allocation address"));
    }
    let end = output
        .checked_add(allocated_bytes as u64)
        .ok_or(LoaderError::Overflow("low allocation"))?;
    if end >= LOW_4G_LIMIT {
        return Err(LoaderError::Invalid("low allocation exceeds 4 GiB"));
    }
    Ok(())
}

unsafe fn allocate_pool(boot: &BootServices, bytes: usize) -> Result<*mut u8, LoaderError> {
    let allocate = boot
        .allocate_pool
        .ok_or(LoaderError::Invalid("AllocatePool missing"))?;
    let mut output: *mut c_void = core::ptr::null_mut();
    firmware(unsafe { allocate(uefi::EFI_LOADER_DATA, bytes, &mut output) })?;
    if output.is_null() {
        return Err(LoaderError::Invalid("AllocatePool returned null"));
    }
    Ok(output.cast())
}

unsafe fn close_file(file: *mut FileProtocol) {
    if let Some(close) = unsafe { file.as_ref() }.and_then(|value| value.close) {
        let _ = unsafe { close(file) };
    }
}

unsafe fn read_kernel_file(
    boot: &BootServices,
    image: Handle,
    _system_table: *mut SystemTable,
) -> Result<(*mut u8, usize), LoaderError> {
    let loaded = unsafe {
        handle_protocol::<uefi::LoadedImageProtocol>(boot, image, uefi::LOADED_IMAGE_PROTOCOL_GUID)?
    };
    let loaded = unsafe { loaded.as_ref() }.ok_or(LoaderError::Invalid("null LoadedImage"))?;
    if loaded.device_handle.is_null() {
        return Err(LoaderError::Invalid("LoadedImage has no device handle"));
    }
    let fs = unsafe {
        handle_protocol::<uefi::SimpleFileSystemProtocol>(
            boot,
            loaded.device_handle,
            uefi::SIMPLE_FILE_SYSTEM_PROTOCOL_GUID,
        )?
    };
    let open_volume = unsafe { fs.as_ref() }
        .and_then(|value| value.open_volume)
        .ok_or(LoaderError::Invalid("OpenVolume missing"))?;
    let mut root = core::ptr::null_mut();
    firmware(unsafe { open_volume(fs, &mut root) })?;
    if root.is_null() {
        return Err(LoaderError::Invalid("OpenVolume returned null"));
    }
    let open = unsafe { root.as_ref() }
        .and_then(|value| value.open)
        .ok_or(LoaderError::Invalid("File Open missing"))?;
    let mut kernel = core::ptr::null_mut();
    let status = unsafe {
        open(
            root,
            &mut kernel,
            KERNEL_PATH.as_ptr(),
            uefi::EFI_FILE_MODE_READ,
            0,
        )
    };
    unsafe { close_file(root) };
    firmware(status)?;
    if kernel.is_null() {
        return Err(LoaderError::Invalid("File Open returned null"));
    }
    let result = (|| unsafe {
        let get_info = kernel
            .as_ref()
            .and_then(|value| value.get_info)
            .ok_or(LoaderError::Invalid("File GetInfo missing"))?;
        let mut info_size = 0usize;
        let status = get_info(
            kernel,
            &uefi::FILE_INFO_GUID,
            &mut info_size,
            core::ptr::null_mut(),
        );
        if status != uefi::EFI_BUFFER_TOO_SMALL || info_size < 16 {
            return Err(LoaderError::Firmware(status));
        }
        let info = allocate_pool(boot, info_size)?;
        let status = get_info(kernel, &uefi::FILE_INFO_GUID, &mut info_size, info.cast());
        if let Err(error) = firmware(status) {
            if let Some(free) = boot.free_pool {
                let _ = free(info.cast());
            }
            return Err(error);
        }
        let file_size = core::ptr::read_unaligned(info.add(8).cast::<u64>()) as usize;
        if let Some(free) = boot.free_pool {
            let _ = free(info.cast());
        }
        if file_size == 0 || file_size > MAX_KERNEL_FILE_BYTES {
            return Err(LoaderError::Invalid("kernel file size"));
        }
        let bytes = allocate_pool(boot, file_size)?;
        let read = kernel
            .as_ref()
            .and_then(|value| value.read)
            .ok_or(LoaderError::Invalid("File Read missing"))?;
        let mut read_size = file_size;
        let status = read(kernel, &mut read_size, bytes.cast());
        if let Err(error) = firmware(status) {
            if let Some(free) = boot.free_pool {
                let _ = free(bytes.cast());
            }
            return Err(error);
        }
        if read_size != file_size {
            if let Some(free) = boot.free_pool {
                let _ = free(bytes.cast());
            }
            return Err(LoaderError::Invalid("short kernel file read"));
        }
        Ok((bytes, file_size))
    })();
    unsafe { close_file(kernel) };
    result
}

unsafe fn copy_kernel_segments(
    boot: &BootServices,
    image: ElfImage<'_>,
) -> Result<DeferredLoads, LoaderError> {
    if image.entry() != KERNEL_ENTRY_VIRT {
        return Err(LoaderError::Invalid("kernel entry virtual address"));
    }
    let mut entry_segment = false;
    let mut deferred = DeferredLoads::new();
    for segment in image.segments() {
        let segment = segment.map_err(LoaderError::Elf)?;
        let memory_size = usize::try_from(segment.memory_size)
            .map_err(|_| LoaderError::Overflow("PT_LOAD memory size"))?;
        let end = segment
            .physical_address
            .checked_add(segment.memory_size)
            .ok_or(LoaderError::Overflow("PT_LOAD physical range"))?;
        if end > LOW_4G_MAX + 1 {
            return Err(LoaderError::Invalid("PT_LOAD exceeds low 4 GiB"));
        }
        let source = &image.bytes()[segment.file_range.clone()];
        match unsafe {
            allocate_pages_at(
                boot,
                segment.physical_address,
                memory_size,
                uefi::EFI_LOADER_CODE,
            )
        } {
            Ok(_) => unsafe {
                copy_nonoverlapping(
                    source.as_ptr(),
                    segment.physical_address as *mut u8,
                    source.len(),
                );
                write_bytes(
                    (segment.physical_address as *mut u8).add(source.len()),
                    0,
                    memory_size - source.len(),
                );
            },
            Err(LoaderError::Firmware(uefi::EFI_NOT_FOUND)) => {
                let staging =
                    unsafe { allocate_low_pages(boot, memory_size, uefi::EFI_LOADER_DATA)? };
                unsafe {
                    copy_nonoverlapping(source.as_ptr(), staging as *mut u8, source.len());
                    write_bytes(
                        (staging as *mut u8).add(source.len()),
                        0,
                        memory_size - source.len(),
                    );
                }
                deferred.push(DeferredLoad {
                    source: staging,
                    target: segment.physical_address,
                    file_size: source.len() as u64,
                    memory_size: memory_size as u64,
                    is_ap_trampoline: segment.physical_address == 0x8000
                        && end <= 0x9000
                        && segment.executable()
                        && memory_size <= PAGE_SIZE,
                })?;
            }
            Err(error) => return Err(error),
        }
        if segment.virtual_address == KERNEL_ENTRY_VIRT
            && segment.physical_address == KERNEL_ENTRY_PHYS
            && segment.executable()
        {
            entry_segment = true;
        }
    }
    if !entry_segment {
        return Err(LoaderError::Invalid("kernel entry PT_LOAD layout"));
    }
    Ok(deferred)
}

fn checksum_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

unsafe fn find_and_copy_rsdp(table: &SystemTable, output: *mut u8) -> Result<usize, LoaderError> {
    let rsdp = unsafe { table.find_config_table(uefi::ACPI_20_TABLE_GUID) }
        .or_else(|| unsafe { table.find_config_table(uefi::ACPI_TABLE_GUID) })
        .ok_or(LoaderError::Invalid("ACPI RSDP unavailable"))?;
    if rsdp.is_null() {
        return Err(LoaderError::Invalid("null ACPI RSDP"));
    }
    let source = rsdp.cast::<u8>();
    let v1 = unsafe { core::slice::from_raw_parts(source, 20) };
    if v1.get(..8) != Some(b"RSD PTR ") || !checksum_valid(v1) {
        return Err(LoaderError::Invalid("ACPI RSDP v1 checksum"));
    }
    let length = match v1[15] {
        0 => 20,
        2..=u8::MAX => {
            let prefix = unsafe { core::slice::from_raw_parts(source, 36) };
            let length = u32::from_le_bytes(prefix[20..24].try_into().unwrap()) as usize;
            if !(36..=RSDP_COPY_BYTES).contains(&length) {
                return Err(LoaderError::Invalid("ACPI RSDP length"));
            }
            if !checksum_valid(unsafe { core::slice::from_raw_parts(source, length) }) {
                return Err(LoaderError::Invalid("ACPI RSDP extended checksum"));
            }
            length
        }
        _ => return Err(LoaderError::Invalid("reserved ACPI RSDP revision")),
    };
    unsafe {
        copy_nonoverlapping(source, output, length);
    }
    Ok(length)
}

unsafe fn build_identity_page_tables(root: u64) {
    unsafe {
        write_bytes(root as *mut u8, 0, IDENTITY_PAGE_TABLE_BYTES);
    }
    let tables = root as *mut u64;
    unsafe {
        *tables.add(0) = root + 0x1000 | 0x3;
        for pdpt in 0..4usize {
            *tables.add(512 + pdpt) = root + 0x2000 + (pdpt as u64) * 0x1000 | 0x3;
        }
        for leaf in 0..2048usize {
            *tables.add(1024 + leaf) = (leaf as u64 * 0x20_0000) | 0x83;
        }
    }
}

struct MultibootWriter {
    start: *mut u8,
    capacity: usize,
    offset: usize,
}
impl MultibootWriter {
    unsafe fn new(start: *mut u8, capacity: usize) -> Result<Self, LoaderError> {
        if start.is_null() || capacity < 16 {
            return Err(LoaderError::Invalid("Multiboot buffer"));
        }
        unsafe {
            write_bytes(start, 0, capacity);
        }
        Ok(Self {
            start,
            capacity,
            offset: 8,
        })
    }
    unsafe fn tag(&mut self, kind: u32, payload: &[u8]) -> Result<(), LoaderError> {
        let size = 8usize
            .checked_add(payload.len())
            .ok_or(LoaderError::Overflow("Multiboot tag size"))?;
        let aligned = size
            .checked_add(7)
            .map(|value| value & !7)
            .ok_or(LoaderError::Overflow("Multiboot tag alignment"))?;
        let end = self
            .offset
            .checked_add(aligned)
            .ok_or(LoaderError::Overflow("Multiboot tag range"))?;
        if end > self.capacity {
            return Err(LoaderError::Invalid("Multiboot handoff buffer exhausted"));
        }
        unsafe {
            write_u32(self.start.add(self.offset), kind);
            write_u32(self.start.add(self.offset + 4), size as u32);
            copy_nonoverlapping(
                payload.as_ptr(),
                self.start.add(self.offset + 8),
                payload.len(),
            );
        }
        self.offset = end;
        Ok(())
    }
    unsafe fn finish(mut self) -> Result<(), LoaderError> {
        unsafe {
            self.tag(MB2_TAG_END, &[])?;
        }
        if self.offset > u32::MAX as usize {
            return Err(LoaderError::Overflow("Multiboot total size"));
        }
        unsafe {
            write_u32(self.start, self.offset as u32);
            write_u32(self.start.add(4), 0);
        }
        Ok(())
    }
}

const fn multiboot_memory_type(memory_type: u32) -> u32 {
    match memory_type {
        uefi::EFI_CONVENTIONAL_MEMORY => MB2_MEMORY_AVAILABLE,
        uefi::EFI_ACPI_RECLAIM_MEMORY => MB2_MEMORY_ACPI_RECLAIMABLE,
        uefi::EFI_ACPI_MEMORY_NVS => MB2_MEMORY_NVS,
        _ => MB2_MEMORY_RESERVED,
    }
}

unsafe fn build_multiboot_info(
    output: u64,
    map: *const u8,
    map_size: usize,
    descriptor_size: usize,
    rsdp: *const u8,
    rsdp_length: usize,
    deferred: DeferredLoads,
    _system_table: *mut SystemTable,
) -> Result<(), LoaderError> {
    if descriptor_size < size_of::<MemoryDescriptor>() || map_size % descriptor_size != 0 {
        return Err(LoaderError::Invalid("UEFI memory map descriptor layout"));
    }
    let count = map_size / descriptor_size;
    if count > MAX_MB2_MEMORY_ENTRIES {
        return Err(LoaderError::Invalid("too many UEFI memory map entries"));
    }
    let mut writer = unsafe { MultibootWriter::new(output as *mut u8, MULTIBOOT_INFO_BYTES) }?;
    unsafe {
        writer.tag(MB2_TAG_CMDLINE, KERNEL_CMDLINE)?;
    }
    let mut memory_map = [0u8; 8 + (MAX_MB2_MEMORY_ENTRIES + MAX_DEFERRED_LOADS * 2) * 24];
    unsafe {
        write_u32(memory_map.as_mut_ptr(), 24);
        write_u32(memory_map.as_mut_ptr().add(4), 0);
    }
    for load in &deferred.entries[..deferred.count] {
        let mut cursor = load.target & !(PAGE_SIZE as u64 - 1);
        let target_end =
            (load.target + load.memory_size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        while cursor < target_end {
            let mut found = false;
            for index in 0..count {
                let source = unsafe { map.add(index * descriptor_size).cast::<MemoryDescriptor>() };
                let descriptor = unsafe { core::ptr::read_unaligned(source) };
                let end = descriptor
                    .physical_start
                    .saturating_add(descriptor.number_of_pages.saturating_mul(PAGE_SIZE as u64));
                if descriptor.physical_start <= cursor && cursor < end {
                    if !deferred_target_memory_is_reusable(*load, descriptor.memory_type) {
                        return Err(LoaderError::Invalid(
                            "deferred PT_LOAD overlaps permanent firmware memory",
                        ));
                    }
                    cursor = end.min(target_end);
                    found = true;
                    break;
                }
            }
            if !found && !load.is_ap_trampoline {
                return Err(LoaderError::Invalid(
                    "deferred PT_LOAD is outside the UEFI memory map",
                ));
            }
            if !found {
                cursor = target_end;
            }
        }
    }
    let mut output_count = 0usize;
    for index in 0..count {
        let source = unsafe { map.add(index * descriptor_size).cast::<MemoryDescriptor>() };
        let descriptor = unsafe { core::ptr::read_unaligned(source) };
        let descriptor_end = descriptor
            .physical_start
            .saturating_add(descriptor.number_of_pages.saturating_mul(PAGE_SIZE as u64));
        let mut emit = |start: u64,
                        length: u64,
                        memory_type: u32,
                        output_count: &mut usize|
         -> Result<(), LoaderError> {
            if length == 0 || *output_count >= MAX_MB2_MEMORY_ENTRIES + MAX_DEFERRED_LOADS * 2 {
                return if length == 0 {
                    Ok(())
                } else {
                    Err(LoaderError::Invalid("too many Multiboot memory entries"))
                };
            }
            let offset = 8 + *output_count * 24;
            unsafe {
                write_u64(memory_map.as_mut_ptr().add(offset), start);
                write_u64(memory_map.as_mut_ptr().add(offset + 8), length);
                write_u32(memory_map.as_mut_ptr().add(offset + 16), memory_type);
                write_u32(memory_map.as_mut_ptr().add(offset + 20), 0);
            }
            *output_count += 1;
            Ok(())
        };
        let mut bounds = [0u64; MAX_DEFERRED_LOADS * 2 + 2];
        let mut bound_count = 2;
        bounds[0] = descriptor.physical_start;
        bounds[1] = descriptor_end;
        for load in &deferred.entries[..deferred.count] {
            let start = load.target & !(PAGE_SIZE as u64 - 1);
            let end =
                (load.target + load.memory_size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
            for boundary in [start, end] {
                if descriptor.physical_start < boundary && boundary < descriptor_end {
                    bounds[bound_count] = boundary;
                    bound_count += 1;
                }
            }
        }
        bounds[..bound_count].sort_unstable();
        for pair in bounds[..bound_count].windows(2) {
            let is_reserved = deferred.entries[..deferred.count].iter().any(|load| {
                let start = load.target & !(PAGE_SIZE as u64 - 1);
                let end = (load.target + load.memory_size + PAGE_SIZE as u64 - 1)
                    & !(PAGE_SIZE as u64 - 1);
                start <= pair[0] && pair[0] < end
            });
            emit(
                pair[0],
                pair[1] - pair[0],
                if is_reserved {
                    MB2_MEMORY_RESERVED
                } else {
                    multiboot_memory_type(descriptor.memory_type)
                },
                &mut output_count,
            )?;
        }
    }
    unsafe {
        writer.tag(MB2_TAG_MEMORY_MAP, &memory_map[..8 + output_count * 24])?;
        writer.tag(
            MB2_TAG_ACPI_NEW,
            core::slice::from_raw_parts(rsdp, rsdp_length),
        )?;
        writer.finish()
    }
}

unsafe fn capture_memory_map(
    boot: &BootServices,
    buffer: *mut u8,
) -> Result<(usize, usize, usize), LoaderError> {
    let get = boot
        .get_memory_map
        .ok_or(LoaderError::Invalid("GetMemoryMap missing"))?;
    let mut size = MEMORY_MAP_BYTES;
    let mut key = 0usize;
    let mut descriptor_size = 0usize;
    let mut descriptor_version = 0u32;
    firmware(unsafe {
        get(
            &mut size,
            buffer.cast(),
            &mut key,
            &mut descriptor_size,
            &mut descriptor_version,
        )
    })?;
    if size == 0 || size > MEMORY_MAP_BYTES {
        return Err(LoaderError::Invalid("UEFI memory map size"));
    }
    Ok((size, key, descriptor_size))
}

unsafe fn copy_trampoline(destination: u64, params: TrampolineParams) -> Result<(), LoaderError> {
    unsafe extern "C" {
        static __uefi_handoff_trampoline_start: u8;
        static __uefi_handoff_trampoline_end: u8;
        static __uefi_handoff_params: u8;
        static __uefi_handoff_gdt_ptr: u8;
        static __uefi_handoff_gdt: u8;
    }
    let start = core::ptr::addr_of!(__uefi_handoff_trampoline_start) as usize;
    let end = core::ptr::addr_of!(__uefi_handoff_trampoline_end) as usize;
    let params_source = core::ptr::addr_of!(__uefi_handoff_params) as usize;
    let gdt_ptr_source = core::ptr::addr_of!(__uefi_handoff_gdt_ptr) as usize;
    let gdt_source = core::ptr::addr_of!(__uefi_handoff_gdt) as usize;
    let length = end
        .checked_sub(start)
        .ok_or(LoaderError::Invalid("trampoline symbols"))?;
    let params_offset = params_source
        .checked_sub(start)
        .filter(|offset| *offset + size_of::<TrampolineParams>() <= length)
        .ok_or(LoaderError::Invalid("trampoline params offset"))?;
    let gdt_ptr_offset = gdt_ptr_source
        .checked_sub(start)
        .filter(|offset| *offset + 10 <= length)
        .ok_or(LoaderError::Invalid("trampoline GDT pointer offset"))?;
    let gdt_offset = gdt_source
        .checked_sub(start)
        .filter(|offset| *offset < length)
        .ok_or(LoaderError::Invalid("trampoline GDT offset"))?;
    unsafe {
        copy_nonoverlapping(start as *const u8, destination as *mut u8, length);
        core::ptr::write_unaligned(
            (destination as *mut u8)
                .add(params_offset)
                .cast::<TrampolineParams>(),
            params,
        );
        core::ptr::write_unaligned(
            (destination as *mut u8)
                .add(gdt_ptr_offset + 2)
                .cast::<u64>(),
            destination + gdt_offset as u64,
        );
    }
    Ok(())
}

unsafe fn exit_boot_services_and_jump(
    boot: &BootServices,
    image: Handle,
    memory_map: u64,
    multiboot: u64,
    rsdp: u64,
    rsdp_length: usize,
    trampoline: u64,
    deferred: DeferredLoads,
    _system_table: *mut SystemTable,
) -> Result<(), LoaderError> {
    let exit = boot
        .exit_boot_services
        .ok_or(LoaderError::Invalid("ExitBootServices missing"))?;
    for _ in 0..=5 {
        let (map_size, map_key, descriptor_size) =
            unsafe { capture_memory_map(boot, memory_map as *mut u8) }?;
        unsafe {
            build_multiboot_info(
                multiboot,
                memory_map as *const u8,
                map_size,
                descriptor_size,
                rsdp as *const u8,
                rsdp_length,
                deferred,
                _system_table,
            )?;
        }
        let status = unsafe { exit(image, map_key) };
        if status == uefi::EFI_SUCCESS {
            let entry: unsafe extern "C" fn() -> ! = unsafe { transmute(trampoline as usize) };
            unsafe { entry() };
        }
        if status != uefi::EFI_INVALID_PARAMETER {
            return Err(LoaderError::Firmware(status));
        }
    }
    Err(LoaderError::Firmware(uefi::EFI_INVALID_PARAMETER))
}

/// UEFI application body. It returns only while Boot Services remain active.
pub unsafe fn run(image: Handle, system_table: *mut SystemTable) -> EfiStatus {
    let result = (|| unsafe {
        let table = system_table
            .as_ref()
            .ok_or(LoaderError::Invalid("null system table"))?;
        let boot = boot_services(system_table)?;
        if let Some(disable_watchdog) = boot.set_watchdog_timer {
            firmware(disable_watchdog(0, 0, 0, core::ptr::null()))?;
        }
        let (kernel_file, kernel_size) = read_kernel_file(boot, image, system_table)?;
        let elf = ElfImage::parse(core::slice::from_raw_parts(kernel_file, kernel_size))
            .map_err(LoaderError::Elf)?;
        let deferred = copy_kernel_segments(boot, elf)?;
        if let Some(free) = boot.free_pool {
            let _ = free(kernel_file.cast());
        }
        let memory_map = allocate_low_pages(boot, MEMORY_MAP_BYTES, uefi::EFI_LOADER_DATA)?;
        let multiboot = allocate_low_pages(boot, MULTIBOOT_INFO_BYTES, uefi::EFI_LOADER_DATA)?;
        let rsdp = allocate_low_pages(boot, RSDP_COPY_BYTES, uefi::EFI_LOADER_DATA)?;
        let page_tables =
            allocate_low_pages(boot, IDENTITY_PAGE_TABLE_BYTES, uefi::EFI_LOADER_DATA)?;
        let stack = allocate_low_pages(boot, LOW_STACK_BYTES, uefi::EFI_LOADER_DATA)?;
        let trampoline = allocate_low_pages(boot, PAGE_SIZE, uefi::EFI_LOADER_CODE)?;
        validate_handoff_ranges(
            deferred,
            &[
                (memory_map, MEMORY_MAP_BYTES),
                (multiboot, MULTIBOOT_INFO_BYTES),
                (rsdp, RSDP_COPY_BYTES),
                (page_tables, IDENTITY_PAGE_TABLE_BYTES),
                (stack, LOW_STACK_BYTES),
                (trampoline, PAGE_SIZE),
            ],
        )?;
        let rsdp_length = find_and_copy_rsdp(table, rsdp as *mut u8)?;
        build_identity_page_tables(page_tables);
        copy_trampoline(
            trampoline,
            TrampolineParams {
                page_table_root: page_tables,
                deferred_count: deferred.count as u64,
                deferred: core::array::from_fn(|index| TrampolineDescriptor {
                    source: deferred.entries[index].source,
                    target: deferred.entries[index].target,
                    file_size: deferred.entries[index].file_size,
                    memory_size: deferred.entries[index].memory_size,
                }),
                stack_top: (stack + LOW_STACK_BYTES as u64) as u32,
                multiboot_info: multiboot as u32,
                entry: KERNEL_ENTRY_PHYS as u32,
            },
        )?;
        exit_boot_services_and_jump(
            boot,
            image,
            memory_map,
            multiboot,
            rsdp,
            rsdp_length,
            trampoline,
            deferred,
            system_table,
        )
    })();
    match result {
        Ok(()) => uefi::EFI_SUCCESS,
        Err(error) => error.status(),
    }
}

#[cfg(target_os = "uefi")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

unsafe fn write_u32(destination: *mut u8, value: u32) {
    unsafe { core::ptr::write_unaligned(destination.cast::<u32>(), value.to_le()) };
}
unsafe fn write_u64(destination: *mut u8, value: u64) {
    unsafe { core::ptr::write_unaligned(destination.cast::<u64>(), value.to_le()) };
}

#[cfg(target_os = "uefi")]
core::arch::global_asm!(
    r#"
    .section .text$uefi_handoff,"xr"
    .balign 16
    .globl __uefi_handoff_trampoline_start
__uefi_handoff_trampoline_start:
    .code64
    cli
    leaq __uefi_handoff_params(%rip), %rbx
    movq 8(%rbx), %r13
    leaq 16(%rbx), %r12
.Luefi_copy_loop:
    testq %r13, %r13
    jz .Luefi_copy_done
    movq 0(%r12), %rsi
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    rep movsb
    movq 24(%r12), %rcx
    subq 16(%r12), %rcx
    xorl %eax, %eax
    rep stosb
    addq $32, %r12
    decq %r13
    jmp .Luefi_copy_loop
.Luefi_copy_done:
    movl {stack_offset}(%rbx), %esp
    movq 0(%rbx), %rax
    movq %rax, %cr3
    leaq __uefi_handoff_gdt_ptr(%rip), %rax
    lgdt (%rax)
    pushq $0x08
    leaq .Luefi_compat(%rip), %rax
    pushq %rax
    lretq
    .code32
.Luefi_compat:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    call .Luefi_base
.Luefi_base:
    popl %esi
    subl $(.Luefi_base - __uefi_handoff_trampoline_start), %esi
    movl %cr0, %eax
    andl $0x7fffffff, %eax
    movl %eax, %cr0
    movl $0xc0000080, %ecx
    rdmsr
    andl $0xfffffeff, %eax
    wrmsr
    movl __uefi_handoff_params - __uefi_handoff_trampoline_start + {stack_offset}(%esi), %esp
    movl $0x36d76289, %eax
    movl __uefi_handoff_params - __uefi_handoff_trampoline_start + {mb2_offset}(%esi), %ebx
    movl __uefi_handoff_params - __uefi_handoff_trampoline_start + {entry_offset}(%esi), %edx
    jmp *%edx
    .balign 8
    .globl __uefi_handoff_params
__uefi_handoff_params:
    .quad 0
    .quad 0
    .rept 32
    .quad 0
    .endr
    .long 0
    .long 0
    .long 0
    .long 0
    .balign 8
    .globl __uefi_handoff_gdt
__uefi_handoff_gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
    .globl __uefi_handoff_gdt_ptr
__uefi_handoff_gdt_ptr:
    .word __uefi_handoff_gdt_ptr - __uefi_handoff_gdt - 1
    .quad 0
    .balign 16
    .globl __uefi_handoff_trampoline_end
__uefi_handoff_trampoline_end:
"#,
    stack_offset = const TRAMPOLINE_STACK_OFFSET,
    mb2_offset = const TRAMPOLINE_MB2_OFFSET,
    entry_offset = const TRAMPOLINE_ENTRY_OFFSET,
    options(att_syntax)
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiboot_memory_types_preserve_acpi_classes() {
        assert_eq!(
            multiboot_memory_type(uefi::EFI_CONVENTIONAL_MEMORY),
            MB2_MEMORY_AVAILABLE
        );
        assert_eq!(
            multiboot_memory_type(uefi::EFI_ACPI_RECLAIM_MEMORY),
            MB2_MEMORY_ACPI_RECLAIMABLE
        );
        assert_eq!(
            multiboot_memory_type(uefi::EFI_ACPI_MEMORY_NVS),
            MB2_MEMORY_NVS
        );
        assert_eq!(
            multiboot_memory_type(uefi::EFI_LOADER_DATA),
            MB2_MEMORY_RESERVED
        );
    }
    #[test]
    fn low_page_calculation_is_checked() {
        assert_eq!(pages_for(1), Ok(1));
        assert_eq!(pages_for(PAGE_SIZE), Ok(1));
        assert_eq!(pages_for(PAGE_SIZE + 1), Ok(2));
        assert!(validate_low_allocation(PAGE_SIZE as u64, PAGE_SIZE).is_ok());
        assert!(validate_low_allocation(0, PAGE_SIZE).is_err());
        assert!(validate_low_allocation(0xffff_f000, PAGE_SIZE).is_err());
        assert!(validate_low_allocation(0xffff_e000, PAGE_SIZE).is_ok());
    }
}
