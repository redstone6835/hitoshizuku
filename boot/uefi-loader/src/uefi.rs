//! Minimal UEFI 2.x ABI declarations owned by the standalone loader.
//!
//! Firmware function pointers use the Windows x64 calling convention on
//! x86_64.  Keeping that convention in these definitions is mandatory: the
//! kernel's freestanding EFI query crate deliberately uses C ABI wrappers and
//! is not an application-side UEFI protocol binding.

use core::ffi::c_void;

pub type EfiStatus = usize;
pub type Handle = *mut c_void;
pub type PhysicalAddress = u64;

pub const EFI_SUCCESS: EfiStatus = 0;
pub const EFI_LOAD_ERROR: EfiStatus = error(1);
pub const EFI_INVALID_PARAMETER: EfiStatus = error(2);
pub const EFI_UNSUPPORTED: EfiStatus = error(3);
pub const EFI_BAD_BUFFER_SIZE: EfiStatus = error(4);
pub const EFI_BUFFER_TOO_SMALL: EfiStatus = error(5);
pub const EFI_DEVICE_ERROR: EfiStatus = error(7);
pub const EFI_OUT_OF_RESOURCES: EfiStatus = error(9);
pub const EFI_NOT_FOUND: EfiStatus = error(14);

pub const EFI_SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
pub const EFI_BOOT_SERVICES_SIGNATURE: u64 = 0x5652_4553_544f_4f42;
pub const EFI_OPEN_PROTOCOL_GET_PROTOCOL: u32 = 0x0000_0002;
pub const EFI_FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;
pub const EFI_ALLOCATE_ANY_PAGES: u32 = 0;
pub const EFI_ALLOCATE_MAX_ADDRESS: u32 = 1;
pub const EFI_ALLOCATE_ADDRESS: u32 = 2;
pub const EFI_LOADER_CODE: u32 = 1;
pub const EFI_LOADER_DATA: u32 = 2;
pub const EFI_BOOT_SERVICES_CODE: u32 = 3;
pub const EFI_BOOT_SERVICES_DATA: u32 = 4;
pub const EFI_CONVENTIONAL_MEMORY: u32 = 7;
pub const EFI_ACPI_RECLAIM_MEMORY: u32 = 9;
pub const EFI_ACPI_MEMORY_NVS: u32 = 10;

pub const LOADED_IMAGE_PROTOCOL_GUID: Guid = Guid::new(
    0x5b1b_31a1,
    0x9562,
    0x11d2,
    [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
pub const SIMPLE_FILE_SYSTEM_PROTOCOL_GUID: Guid = Guid::new(
    0x964e_5b22,
    0x6459,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
pub const FILE_INFO_GUID: Guid = Guid::new(
    0x0957_6e92,
    0x6d3f,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);
pub const ACPI_20_TABLE_GUID: Guid = Guid::new(
    0x8868_e871,
    0xe4f1,
    0x11d3,
    [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81],
);
pub const ACPI_TABLE_GUID: Guid = Guid::new(
    0xeb9d_2d30,
    0x2d88,
    0x11d3,
    [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
);

const fn error(value: usize) -> EfiStatus {
    (1usize << (usize::BITS - 1)) | value
}

pub const fn is_error(status: EfiStatus) -> bool {
    status & (1usize << (usize::BITS - 1)) != 0
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryDescriptor {
    pub memory_type: u32,
    pub physical_start: PhysicalAddress,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

#[repr(C)]
pub struct LoadedImageProtocol {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *mut SystemTable,
    pub device_handle: Handle,
    pub file_path: *mut c_void,
    pub reserved: *mut c_void,
    pub load_options_size: u32,
    pub load_options: *mut c_void,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    pub unload: usize,
}

#[repr(C)]
pub struct SimpleFileSystemProtocol {
    pub revision: u64,
    pub open_volume:
        Option<unsafe extern "efiapi" fn(*mut Self, *mut *mut FileProtocol) -> EfiStatus>,
}

#[repr(C)]
pub struct FileProtocol {
    pub revision: u64,
    pub open: Option<
        unsafe extern "efiapi" fn(*mut Self, *mut *mut Self, *const u16, u64, u64) -> EfiStatus,
    >,
    pub close: Option<unsafe extern "efiapi" fn(*mut Self) -> EfiStatus>,
    pub delete: usize,
    pub read: Option<unsafe extern "efiapi" fn(*mut Self, *mut usize, *mut c_void) -> EfiStatus>,
    pub write: usize,
    pub get_position: usize,
    pub set_position: usize,
    pub get_info: Option<
        unsafe extern "efiapi" fn(*mut Self, *const Guid, *mut usize, *mut c_void) -> EfiStatus,
    >,
    pub set_info: usize,
    pub flush: usize,
    pub open_ex: usize,
    pub read_ex: usize,
    pub write_ex: usize,
    pub flush_ex: usize,
}

pub type AllocatePages =
    unsafe extern "efiapi" fn(u32, u32, usize, *mut PhysicalAddress) -> EfiStatus;
pub type FreePages = unsafe extern "efiapi" fn(PhysicalAddress, usize) -> EfiStatus;
pub type GetMemoryMap = unsafe extern "efiapi" fn(
    *mut usize,
    *mut MemoryDescriptor,
    *mut usize,
    *mut usize,
    *mut u32,
) -> EfiStatus;
pub type AllocatePool = unsafe extern "efiapi" fn(u32, usize, *mut *mut c_void) -> EfiStatus;
pub type FreePool = unsafe extern "efiapi" fn(*mut c_void) -> EfiStatus;
pub type OpenProtocol = unsafe extern "efiapi" fn(
    Handle,
    *const Guid,
    *mut *mut c_void,
    Handle,
    Handle,
    u32,
) -> EfiStatus;
pub type HandleProtocol =
    unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> EfiStatus;
pub type LocateProtocol =
    unsafe extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut c_void) -> EfiStatus;
pub type LocateDevicePath =
    unsafe extern "efiapi" fn(*const Guid, *mut *mut c_void, *mut Handle) -> EfiStatus;
pub type ExitBootServices = unsafe extern "efiapi" fn(Handle, usize) -> EfiStatus;
pub type SetWatchdogTimer = unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> EfiStatus;

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    pub raise_tpl: usize,
    pub restore_tpl: usize,
    pub allocate_pages: Option<AllocatePages>,
    pub free_pages: Option<FreePages>,
    pub get_memory_map: Option<GetMemoryMap>,
    pub allocate_pool: Option<AllocatePool>,
    pub free_pool: Option<FreePool>,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol: Option<HandleProtocol>,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path: Option<LocateDevicePath>,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services: Option<ExitBootServices>,
    pub get_next_monotonic_count: usize,
    pub stall: usize,
    pub set_watchdog_timer: Option<SetWatchdogTimer>,
    pub connect_controller: usize,
    pub disconnect_controller: usize,
    pub open_protocol: Option<OpenProtocol>,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer: usize,
    pub locate_protocol: Option<LocateProtocol>,
    pub install_multiple_protocol_interfaces: usize,
    pub uninstall_multiple_protocol_interfaces: usize,
    pub calculate_crc32: usize,
    pub copy_mem: usize,
    pub set_mem: usize,
    pub create_event_ex: usize,
}

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut c_void,
    pub standard_error_handle: Handle,
    pub std_err: *mut c_void,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut ConfigurationTable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableError {
    Null,
    Signature,
    HeaderSize,
    BootServices,
}

impl SystemTable {
    pub unsafe fn validate(&self) -> Result<(), TableError> {
        if self.hdr.signature != EFI_SYSTEM_TABLE_SIGNATURE {
            return Err(TableError::Signature);
        }
        if self.hdr.header_size < core::mem::size_of::<TableHeader>() as u32 {
            return Err(TableError::HeaderSize);
        }
        let boot = unsafe { self.boot_services.as_ref() }.ok_or(TableError::BootServices)?;
        if boot.hdr.signature != EFI_BOOT_SERVICES_SIGNATURE {
            return Err(TableError::BootServices);
        }
        Ok(())
    }

    pub unsafe fn boot_services(&self) -> Result<&BootServices, TableError> {
        unsafe { self.validate()? };
        unsafe { self.boot_services.as_ref() }.ok_or(TableError::BootServices)
    }

    pub unsafe fn find_config_table(&self, guid: Guid) -> Option<*const c_void> {
        let tables = unsafe {
            core::slice::from_raw_parts(self.configuration_table, self.number_of_table_entries)
        };
        tables
            .iter()
            .find(|table| table.vendor_guid == guid)
            .map(|table| table.vendor_table)
    }
}
