//! EFI (UEFI) C wrapper crate.
//!
//! The ABI definitions and protocol logic live in `src/c`. Rust keeps only
//! `repr(C)` mirrors required for FFI plus small, typed wrapper functions.

#![no_std]

/// 强制链接器保留 EFI 查询直接符号目录。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    status_success as usize ^ guid_equal as usize
}

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr::addr_of_mut;

pub type EfiStatus = usize;
pub type EfiHandle = *mut c_void;
pub type EfiEvent = *mut c_void;
pub type EfiPhysicalAddress = u64;
pub type EfiVirtualAddress = u64;
pub type EfiChar16 = u16;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiTableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EfiGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiConfigTable {
    pub vendor_guid: EfiGuid,
    pub vendor_table: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiInputKey {
    pub scan_code: u16,
    pub unicode_char: u16,
}

#[repr(C)]
pub struct EfiSimpleTextInputProtocol {
    pub reset: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextInputProtocol,
            extended_verification: u8,
        ) -> EfiStatus,
    >,
    pub read_key_stroke: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextInputProtocol,
            key: *mut EfiInputKey,
        ) -> EfiStatus,
    >,
    pub wait_for_key: EfiEvent,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiSimpleTextOutputMode {
    pub max_mode: i32,
    pub mode: i32,
    pub attribute: i32,
    pub cursor_column: i32,
    pub cursor_row: i32,
    pub cursor_visible: u8,
}

impl EfiSimpleTextOutputMode {
    pub fn is_cursor_visible(&self) -> bool {
        self.cursor_visible != 0
    }
}

#[repr(C)]
pub struct EfiSimpleTextOutputProtocol {
    pub reset: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextOutputProtocol,
            extended_verification: u8,
        ) -> EfiStatus,
    >,
    pub output_string: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextOutputProtocol,
            string: *const EfiChar16,
        ) -> EfiStatus,
    >,
    pub test_string: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextOutputProtocol,
            string: *const EfiChar16,
        ) -> EfiStatus,
    >,
    pub query_mode: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextOutputProtocol,
            mode_number: usize,
            columns: *mut usize,
            rows: *mut usize,
        ) -> EfiStatus,
    >,
    pub set_mode: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextOutputProtocol,
            mode_number: usize,
        ) -> EfiStatus,
    >,
    pub set_attribute: Option<
        unsafe extern "C" fn(this: *mut EfiSimpleTextOutputProtocol, attribute: usize) -> EfiStatus,
    >,
    pub clear_screen:
        Option<unsafe extern "C" fn(this: *mut EfiSimpleTextOutputProtocol) -> EfiStatus>,
    pub set_cursor_position: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimpleTextOutputProtocol,
            column: usize,
            row: usize,
        ) -> EfiStatus,
    >,
    pub enable_cursor: Option<
        unsafe extern "C" fn(this: *mut EfiSimpleTextOutputProtocol, visible: u8) -> EfiStatus,
    >,
    pub mode: *mut EfiSimpleTextOutputMode,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiSimplePointerState {
    pub relative_movement_x: i32,
    pub relative_movement_y: i32,
    pub relative_movement_z: i32,
    pub left_button: u8,
    pub right_button: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiSimplePointerMode {
    pub resolution_x: u64,
    pub resolution_y: u64,
    pub resolution_z: u64,
    pub left_button: u8,
    pub right_button: u8,
}

#[repr(C)]
pub struct EfiSimplePointerProtocol {
    pub reset: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimplePointerProtocol,
            extended_verification: u8,
        ) -> EfiStatus,
    >,
    pub get_state: Option<
        unsafe extern "C" fn(
            this: *mut EfiSimplePointerProtocol,
            state: *mut EfiSimplePointerState,
        ) -> EfiStatus,
    >,
    pub wait_for_input: EfiEvent,
    pub mode: *mut EfiSimplePointerMode,
}

#[repr(C)]
pub struct EfiRuntimeServices {
    pub hdr: EfiTableHeader,
    pub get_time: usize,
    pub set_time: usize,
    pub get_wakeup_time: usize,
    pub set_wakeup_time: usize,
    pub set_virtual_address_map: usize,
    pub convert_pointer: usize,
    pub get_variable: usize,
    pub get_next_variable_name: usize,
    pub set_variable: usize,
    pub get_next_high_mono_count: usize,
    pub reset_system: usize,
    pub update_capsule: usize,
    pub query_capsule_capabilities: usize,
    pub query_variable_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiMemoryDescriptor {
    pub type_: u32,
    pub physical_start: EfiPhysicalAddress,
    pub virtual_start: EfiVirtualAddress,
    pub number_of_pages: u64,
    pub attribute: u64,
}

pub type EfiMemoryType = u32;

pub type EfiGetMemoryMapFn = unsafe extern "C" fn(
    memory_map_size: *mut usize,
    memory_map: *mut EfiMemoryDescriptor,
    map_key: *mut usize,
    descriptor_size: *mut usize,
    descriptor_version: *mut u32,
) -> EfiStatus;
pub type EfiExitBootServicesFn =
    unsafe extern "C" fn(image_handle: EfiHandle, map_key: usize) -> EfiStatus;
pub type EfiHandleProtocolFn = unsafe extern "C" fn(
    handle: EfiHandle,
    protocol: *const EfiGuid,
    interface: *mut *mut c_void,
) -> EfiStatus;
pub type EfiStallFn = unsafe extern "C" fn(microseconds: usize) -> EfiStatus;
pub type EfiSetWatchdogTimerFn = unsafe extern "C" fn(
    timeout: usize,
    watchdog_code: u64,
    data_size: usize,
    watchdog_data: *const EfiChar16,
) -> EfiStatus;

#[repr(C)]
pub struct EfiBootServices {
    pub hdr: EfiTableHeader,
    pub raise_tpl: usize,
    pub restore_tpl: usize,
    pub allocate_pages: usize,
    pub free_pages: usize,
    pub get_memory_map: Option<EfiGetMemoryMapFn>,
    pub allocate_pool: usize,
    pub free_pool: usize,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol: Option<EfiHandleProtocolFn>,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path: usize,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services: Option<EfiExitBootServicesFn>,
    pub get_next_monotonic_count: usize,
    pub stall: Option<EfiStallFn>,
    pub set_watchdog_timer: Option<EfiSetWatchdogTimerFn>,
    pub connect_controller: usize,
    pub disconnect_controller: usize,
    pub open_protocol: usize,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer: usize,
    pub locate_protocol: usize,
    pub install_multiple_protocol_interfaces: usize,
    pub uninstall_multiple_protocol_interfaces: usize,
    pub calculate_crc32: usize,
    pub copy_mem: usize,
    pub set_mem: usize,
    pub create_event_ex: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiSystemTable {
    pub hdr: EfiTableHeader,
    pub firmware_vendor: *const EfiChar16,
    pub firmware_revision: u32,
    pub console_in_handle: EfiHandle,
    pub con_in: *mut EfiSimpleTextInputProtocol,
    pub console_out_handle: EfiHandle,
    pub con_out: *mut EfiSimpleTextOutputProtocol,
    pub standard_error_handle: EfiHandle,
    pub std_err: *mut EfiSimpleTextOutputProtocol,
    pub runtime_services: *mut EfiRuntimeServices,
    pub boot_services: *mut EfiBootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut EfiConfigTable,
}

impl EfiSystemTable {
    pub unsafe fn config_tables(&self) -> Option<&'static [EfiConfigTable]> {
        unsafe { config_tables(self as *const EfiSystemTable) }
    }

    pub unsafe fn find_config_table(&self, guid: &EfiGuid) -> Option<*mut c_void> {
        unsafe { find_config_table(self as *const EfiSystemTable, guid) }
    }

    pub unsafe fn firmware_vendor_cstr16(&self, max_len: usize) -> Option<&'static [EfiChar16]> {
        unsafe { firmware_vendor(self as *const EfiSystemTable, max_len) }
    }

    pub unsafe fn find_acpi_rsdp(&self) -> Option<*mut c_void> {
        unsafe { find_acpi_rsdp(self as *const EfiSystemTable) }
    }

    pub unsafe fn find_fdt(&self) -> Option<*mut c_void> {
        unsafe { find_fdt(self as *const EfiSystemTable) }
    }
}

#[repr(C)]
pub struct EfiLoadedImageProtocol {
    pub revision: u32,
    pub parent_handle: EfiHandle,
    pub system_table: *mut EfiSystemTable,
    pub device_handle: EfiHandle,
    pub file_path: *mut c_void,
    pub reserved: *mut c_void,
    pub load_options_size: u32,
    pub load_options: *mut c_void,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: EfiMemoryType,
    pub image_data_type: EfiMemoryType,
    pub unload: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EfiBootHandoff {
    pub system_table: *const EfiSystemTable,
    pub image_handle: EfiHandle,
    pub cmdline: *const u8,
    pub cmdline_size: usize,
    pub memory_map_size: usize,
    pub map_key: usize,
    pub descriptor_size: usize,
    pub descriptor_version: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct EfiSystemTableView {
    ptr: *const EfiSystemTable,
}

impl EfiSystemTableView {
    pub unsafe fn from_ptr(ptr: usize) -> Option<Self> {
        if ptr == 0 {
            return None;
        }
        let st = ptr as *const EfiSystemTable;
        if unsafe { efi_system_table_is_valid(st) } == 0 {
            return None;
        }
        Some(EfiSystemTableView { ptr: st })
    }

    pub unsafe fn table(&self) -> &EfiSystemTable {
        unsafe { &*self.ptr }
    }

    pub fn as_ptr(&self) -> *const EfiSystemTable {
        self.ptr
    }
}

const EFI_SYSTEM_TABLE_SNAPSHOT_CONFIG_CAPACITY: usize = 64;
const EFI_SYSTEM_TABLE_SNAPSHOT_VENDOR_CAPACITY: usize = 128;

static mut SNAPSHOT_EFI_SYSTEM_TABLE: MaybeUninit<EfiSystemTable> = MaybeUninit::uninit();
static mut SNAPSHOT_EFI_RUNTIME_SERVICES: MaybeUninit<EfiRuntimeServices> = MaybeUninit::uninit();
static mut SNAPSHOT_EFI_BOOT_SERVICES: MaybeUninit<EfiBootServices> = MaybeUninit::uninit();
static mut SNAPSHOT_EFI_CONFIG_TABLE: MaybeUninit<
    [EfiConfigTable; EFI_SYSTEM_TABLE_SNAPSHOT_CONFIG_CAPACITY],
> = MaybeUninit::uninit();
static mut SNAPSHOT_EFI_FIRMWARE_VENDOR: MaybeUninit<
    [EfiChar16; EFI_SYSTEM_TABLE_SNAPSHOT_VENDOR_CAPACITY],
> = MaybeUninit::uninit();

pub fn snapshot_system_table_static(
    view: EfiSystemTableView,
) -> Result<&'static EfiSystemTable, &'static str> {
    let system_table_ptr = addr_of_mut!(SNAPSHOT_EFI_SYSTEM_TABLE).cast::<EfiSystemTable>();
    let config_table_ptr = addr_of_mut!(SNAPSHOT_EFI_CONFIG_TABLE).cast::<EfiConfigTable>();
    let runtime_services_ptr =
        addr_of_mut!(SNAPSHOT_EFI_RUNTIME_SERVICES).cast::<EfiRuntimeServices>();
    let boot_services_ptr = addr_of_mut!(SNAPSHOT_EFI_BOOT_SERVICES).cast::<EfiBootServices>();
    let firmware_vendor_ptr = addr_of_mut!(SNAPSHOT_EFI_FIRMWARE_VENDOR).cast::<EfiChar16>();
    let mut config_table_count = 0usize;

    let ok = unsafe {
        snapshot_system_table(
            view.as_ptr(),
            system_table_ptr,
            config_table_ptr,
            EFI_SYSTEM_TABLE_SNAPSHOT_CONFIG_CAPACITY,
            runtime_services_ptr,
            boot_services_ptr,
            firmware_vendor_ptr,
            EFI_SYSTEM_TABLE_SNAPSHOT_VENDOR_CAPACITY,
            Some(&mut config_table_count),
        )
    };
    if !ok {
        return Err("[efi] failed to snapshot EFI system table");
    }

    let table = unsafe { (*addr_of_mut!(SNAPSHOT_EFI_SYSTEM_TABLE)).assume_init_ref() };
    let _snapshot_view = unsafe { EfiSystemTableView::from_ptr(table as *const _ as usize) }
        .ok_or("[efi] kernel EFI table snapshot validation failed")?;

    let _ = config_table_count;
    Ok(table)
}

unsafe extern "C" {
    static EFI_STATUS_SUCCESS: EfiStatus;
    static EFI_STATUS_LOAD_ERROR: EfiStatus;
    static EFI_STATUS_INVALID_PARAMETER: EfiStatus;
    static EFI_STATUS_UNSUPPORTED: EfiStatus;
    static EFI_STATUS_BUFFER_TOO_SMALL: EfiStatus;
    static EFI_STATUS_NOT_FOUND: EfiStatus;
    static EFI_STATUS_OUT_OF_RESOURCES: EfiStatus;

    static EFI_MEMORY_TYPE_LOADER_CODE: u32;
    static EFI_MEMORY_TYPE_LOADER_DATA: u32;
    static EFI_MEMORY_TYPE_BOOT_SERVICES_CODE: u32;
    static EFI_MEMORY_TYPE_BOOT_SERVICES_DATA: u32;
    static EFI_MEMORY_TYPE_CONVENTIONAL_MEMORY: u32;

    static ACPI_20_TABLE_GUID: EfiGuid;
    static ACPI_TABLE_GUID: EfiGuid;
    static FDT_TABLE_GUID: EfiGuid;
    static SMBIOS3_TABLE_GUID: EfiGuid;
    static LOADED_IMAGE_PROTOCOL_GUID: EfiGuid;
    static SIMPLE_POINTER_PROTOCOL_GUID: EfiGuid;
    static SIMPLE_TEXT_INPUT_PROTOCOL_GUID: EfiGuid;
    static SIMPLE_TEXT_OUTPUT_PROTOCOL_GUID: EfiGuid;

    fn efi_status_is_success(status: EfiStatus) -> i32;
    fn efi_status_is_error(status: EfiStatus) -> i32;
    fn efi_status_name(status: EfiStatus) -> *const u8;
    fn efi_memory_type_name(type_: u32) -> *const u8;
    fn efi_memory_type_is_usable_after_exit_boot_services(type_: u32) -> i32;
    fn efi_guid_equal(lhs: *const EfiGuid, rhs: *const EfiGuid) -> i32;
    fn efi_system_table_is_valid(st: *const EfiSystemTable) -> i32;
    fn efi_system_table_snapshot(
        src: *const EfiSystemTable,
        dst: *mut EfiSystemTable,
        config_table_copy: *mut EfiConfigTable,
        config_table_capacity: usize,
        runtime_services_copy: *mut EfiRuntimeServices,
        boot_services_copy: *mut EfiBootServices,
        firmware_vendor_copy: *mut EfiChar16,
        firmware_vendor_capacity: usize,
        out_config_table_count: *mut usize,
    ) -> i32;
    fn efi_ascii_strlen(ptr: *const u8, max_len: usize) -> usize;
    fn efi_known_config_table_name(guid: *const EfiGuid) -> *const u8;

    fn efi_system_table_config_tables(
        st: *const EfiSystemTable,
        out_entries: *mut *const EfiConfigTable,
        out_count: *mut usize,
    ) -> i32;
    fn efi_system_table_find_config_table(
        st: *const EfiSystemTable,
        guid: *const EfiGuid,
    ) -> *mut c_void;
    fn efi_system_table_find_acpi_rsdp(st: *const EfiSystemTable) -> *mut c_void;
    fn efi_system_table_find_fdt(st: *const EfiSystemTable) -> *mut c_void;
    fn efi_system_table_firmware_vendor(
        st: *const EfiSystemTable,
        out_ptr: *mut *const EfiChar16,
        out_len: *mut usize,
        max_len: usize,
    ) -> i32;

    fn efi_locate_config_table(
        system_table: *const EfiSystemTable,
        guid: *const EfiGuid,
    ) -> *mut c_void;
    fn efi_get_memory_map(
        system_table: *mut EfiSystemTable,
        size: *mut usize,
        memory_map: *mut EfiMemoryDescriptor,
        map_key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> EfiStatus;
    fn efi_get_memory_map_retry(
        system_table: *mut EfiSystemTable,
        size: *mut usize,
        memory_map: *mut EfiMemoryDescriptor,
        map_key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> EfiStatus;
    fn efi_exit_boot_services(
        system_table: *mut EfiSystemTable,
        image_handle: EfiHandle,
        map_key: usize,
    ) -> EfiStatus;
    fn efi_exit_boot_services_with_memory_map(
        system_table: *mut EfiSystemTable,
        image_handle: EfiHandle,
        memory_map: *mut EfiMemoryDescriptor,
        memory_map_capacity: usize,
        out_handoff: *mut EfiBootHandoff,
    ) -> EfiStatus;
    fn efi_disable_watchdog(system_table: *mut EfiSystemTable) -> EfiStatus;
    fn efi_stall(system_table: *mut EfiSystemTable, microseconds: usize) -> EfiStatus;
    fn efi_handle_protocol(
        system_table: *mut EfiSystemTable,
        handle: EfiHandle,
        protocol: *const EfiGuid,
        interface: *mut *mut c_void,
    ) -> EfiStatus;
    fn efi_loaded_image_protocol(
        system_table: *mut EfiSystemTable,
        image_handle: EfiHandle,
        loaded_image: *mut *mut EfiLoadedImageProtocol,
    ) -> EfiStatus;
    fn efi_copy_loaded_image_options_ascii(
        loaded_image: *mut EfiLoadedImageProtocol,
        buffer: *mut u8,
        buffer_len: usize,
        out_len: *mut usize,
    ) -> EfiStatus;
    fn efi_prepare_boot_handoff(
        system_table: *mut EfiSystemTable,
        image_handle: EfiHandle,
        cmdline_buffer: *mut u8,
        cmdline_buffer_len: usize,
        system_table_copy: *mut EfiSystemTable,
        config_table_copy: *mut EfiConfigTable,
        config_table_capacity: usize,
        runtime_services_copy: *mut EfiRuntimeServices,
        boot_services_copy: *mut EfiBootServices,
        firmware_vendor_copy: *mut EfiChar16,
        firmware_vendor_capacity: usize,
        memory_map: *mut EfiMemoryDescriptor,
        memory_map_capacity: usize,
        out_handoff: *mut EfiBootHandoff,
    ) -> EfiStatus;
}

fn c_ascii_str(ptr: *const u8, max_len: usize) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    let len = unsafe { efi_ascii_strlen(ptr, max_len) };
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    core::str::from_utf8(bytes).unwrap_or("")
}

#[kernel_symbols::export(name = "efi.status_success", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_success() -> EfiStatus {
    unsafe { EFI_STATUS_SUCCESS }
}

#[kernel_symbols::export(name = "efi.status_load_error", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_load_error() -> EfiStatus {
    unsafe { EFI_STATUS_LOAD_ERROR }
}

#[kernel_symbols::export(name = "efi.status_invalid_parameter", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_invalid_parameter() -> EfiStatus {
    unsafe { EFI_STATUS_INVALID_PARAMETER }
}

#[kernel_symbols::export(name = "efi.status_unsupported", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_unsupported() -> EfiStatus {
    unsafe { EFI_STATUS_UNSUPPORTED }
}

pub fn status_buffer_too_small() -> EfiStatus {
    unsafe { EFI_STATUS_BUFFER_TOO_SMALL }
}

pub fn status_not_found() -> EfiStatus {
    unsafe { EFI_STATUS_NOT_FOUND }
}

pub fn status_out_of_resources() -> EfiStatus {
    unsafe { EFI_STATUS_OUT_OF_RESOURCES }
}

#[kernel_symbols::export(name = "efi.status_is_error", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_is_error(status: EfiStatus) -> bool {
    unsafe { efi_status_is_error(status) != 0 }
}

#[kernel_symbols::export(name = "efi.status_is_success", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_is_success(status: EfiStatus) -> bool {
    unsafe { efi_status_is_success(status) != 0 }
}

#[kernel_symbols::export(name = "efi.status_name", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn status_name(status: EfiStatus) -> &'static str {
    let ptr = unsafe { efi_status_name(status) };
    c_ascii_str(ptr, 96)
}

#[kernel_symbols::export(name = "efi.memory_type_name", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn memory_type_name(type_: u32) -> &'static str {
    let ptr = unsafe { efi_memory_type_name(type_) };
    c_ascii_str(ptr, 96)
}

#[kernel_symbols::export(name = "efi.memory_type_is_usable_after_exit_boot_services", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn memory_type_is_usable_after_exit_boot_services(type_: u32) -> bool {
    unsafe { efi_memory_type_is_usable_after_exit_boot_services(type_) != 0 }
}

pub fn memory_type_loader_code() -> u32 {
    unsafe { EFI_MEMORY_TYPE_LOADER_CODE }
}

pub fn memory_type_loader_data() -> u32 {
    unsafe { EFI_MEMORY_TYPE_LOADER_DATA }
}

pub fn memory_type_boot_services_code() -> u32 {
    unsafe { EFI_MEMORY_TYPE_BOOT_SERVICES_CODE }
}

pub fn memory_type_boot_services_data() -> u32 {
    unsafe { EFI_MEMORY_TYPE_BOOT_SERVICES_DATA }
}

pub fn memory_type_conventional_memory() -> u32 {
    unsafe { EFI_MEMORY_TYPE_CONVENTIONAL_MEMORY }
}

#[kernel_symbols::export(name = "efi.acpi_20_table_guid", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn acpi_20_table_guid() -> &'static EfiGuid {
    unsafe { &ACPI_20_TABLE_GUID }
}

#[kernel_symbols::export(name = "efi.acpi_table_guid", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn acpi_table_guid() -> &'static EfiGuid {
    unsafe { &ACPI_TABLE_GUID }
}

#[kernel_symbols::export(name = "efi.fdt_table_guid", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn fdt_table_guid() -> &'static EfiGuid {
    unsafe { &FDT_TABLE_GUID }
}

pub fn smbios3_table_guid() -> &'static EfiGuid {
    unsafe { &SMBIOS3_TABLE_GUID }
}

pub fn loaded_image_protocol_guid() -> &'static EfiGuid {
    unsafe { &LOADED_IMAGE_PROTOCOL_GUID }
}

pub fn simple_pointer_protocol_guid() -> &'static EfiGuid {
    unsafe { &SIMPLE_POINTER_PROTOCOL_GUID }
}

pub fn simple_text_input_protocol_guid() -> &'static EfiGuid {
    unsafe { &SIMPLE_TEXT_INPUT_PROTOCOL_GUID }
}

pub fn simple_text_output_protocol_guid() -> &'static EfiGuid {
    unsafe { &SIMPLE_TEXT_OUTPUT_PROTOCOL_GUID }
}

#[kernel_symbols::export(name = "efi.guid_equal", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn guid_equal(lhs: &EfiGuid, rhs: &EfiGuid) -> bool {
    unsafe { efi_guid_equal(lhs as *const EfiGuid, rhs as *const EfiGuid) != 0 }
}

#[kernel_symbols::export(name = "efi.known_config_table_name", contract = "kernel.firmware.efi-query@1", version = 1, capabilities = kernel_symbols::capability::FIRMWARE_QUERY)]
pub fn known_config_table_name(guid: &EfiGuid) -> &'static str {
    let ptr = unsafe { efi_known_config_table_name(guid as *const EfiGuid) };
    c_ascii_str(ptr, 128)
}

pub unsafe fn config_tables(
    system_table: *const EfiSystemTable,
) -> Option<&'static [EfiConfigTable]> {
    let mut entries: *const EfiConfigTable = core::ptr::null();
    let mut count = 0usize;
    let ret =
        unsafe { efi_system_table_config_tables(system_table, &raw mut entries, &raw mut count) };
    if ret != 0 || entries.is_null() || count == 0 {
        None
    } else {
        Some(unsafe { core::slice::from_raw_parts(entries, count) })
    }
}

pub unsafe fn find_config_table(
    system_table: *const EfiSystemTable,
    guid: &EfiGuid,
) -> Option<*mut c_void> {
    let ptr = unsafe { efi_system_table_find_config_table(system_table, guid as *const EfiGuid) };
    (!ptr.is_null()).then_some(ptr)
}

pub unsafe fn find_acpi_rsdp(system_table: *const EfiSystemTable) -> Option<*mut c_void> {
    let ptr = unsafe { efi_system_table_find_acpi_rsdp(system_table) };
    (!ptr.is_null()).then_some(ptr)
}

pub unsafe fn find_fdt(system_table: *const EfiSystemTable) -> Option<*mut c_void> {
    let ptr = unsafe { efi_system_table_find_fdt(system_table) };
    (!ptr.is_null()).then_some(ptr)
}

pub unsafe fn snapshot_system_table(
    src: *const EfiSystemTable,
    dst: *mut EfiSystemTable,
    config_table_copy: *mut EfiConfigTable,
    config_table_capacity: usize,
    runtime_services_copy: *mut EfiRuntimeServices,
    boot_services_copy: *mut EfiBootServices,
    firmware_vendor_copy: *mut EfiChar16,
    firmware_vendor_capacity: usize,
    out_config_table_count: Option<&mut usize>,
) -> bool {
    let out_count_ptr = out_config_table_count
        .map(|count| count as *mut usize)
        .unwrap_or(core::ptr::null_mut());
    unsafe {
        efi_system_table_snapshot(
            src,
            dst,
            config_table_copy,
            config_table_capacity,
            runtime_services_copy,
            boot_services_copy,
            firmware_vendor_copy,
            firmware_vendor_capacity,
            out_count_ptr,
        ) == 0
    }
}

pub unsafe fn firmware_vendor(
    system_table: *const EfiSystemTable,
    max_len: usize,
) -> Option<&'static [EfiChar16]> {
    let mut ptr: *const EfiChar16 = core::ptr::null();
    let mut len = 0usize;
    let ret = unsafe {
        efi_system_table_firmware_vendor(system_table, &raw mut ptr, &raw mut len, max_len)
    };
    if ret != 0 || ptr.is_null() {
        None
    } else {
        Some(unsafe { core::slice::from_raw_parts(ptr, len) })
    }
}

pub unsafe fn locate_config_table(
    system_table: *const EfiSystemTable,
    guid: &EfiGuid,
) -> Option<*mut c_void> {
    let ptr = unsafe { efi_locate_config_table(system_table, guid as *const EfiGuid) };
    (!ptr.is_null()).then_some(ptr)
}

pub unsafe fn get_memory_map(
    system_table: *mut EfiSystemTable,
    size: &mut usize,
    memory_map: *mut EfiMemoryDescriptor,
    map_key: &mut usize,
    descriptor_size: &mut usize,
    descriptor_version: &mut u32,
) -> EfiStatus {
    unsafe {
        efi_get_memory_map(
            system_table,
            size as *mut usize,
            memory_map,
            map_key as *mut usize,
            descriptor_size as *mut usize,
            descriptor_version as *mut u32,
        )
    }
}

pub unsafe fn get_memory_map_retry(
    system_table: *mut EfiSystemTable,
    size: &mut usize,
    memory_map: *mut EfiMemoryDescriptor,
    map_key: &mut usize,
    descriptor_size: &mut usize,
    descriptor_version: &mut u32,
) -> EfiStatus {
    unsafe {
        efi_get_memory_map_retry(
            system_table,
            size as *mut usize,
            memory_map,
            map_key as *mut usize,
            descriptor_size as *mut usize,
            descriptor_version as *mut u32,
        )
    }
}

pub unsafe fn exit_boot_services(
    system_table: *mut EfiSystemTable,
    image_handle: EfiHandle,
    map_key: usize,
) -> EfiStatus {
    unsafe { efi_exit_boot_services(system_table, image_handle, map_key) }
}

pub unsafe fn exit_boot_services_with_memory_map(
    system_table: *mut EfiSystemTable,
    image_handle: EfiHandle,
    memory_map: &mut [u8],
    out_handoff: &mut EfiBootHandoff,
) -> EfiStatus {
    unsafe {
        efi_exit_boot_services_with_memory_map(
            system_table,
            image_handle,
            memory_map.as_mut_ptr().cast::<EfiMemoryDescriptor>(),
            memory_map.len(),
            out_handoff as *mut EfiBootHandoff,
        )
    }
}

pub unsafe fn disable_watchdog(system_table: *mut EfiSystemTable) -> EfiStatus {
    unsafe { efi_disable_watchdog(system_table) }
}

pub unsafe fn stall(system_table: *mut EfiSystemTable, microseconds: usize) -> EfiStatus {
    unsafe { efi_stall(system_table, microseconds) }
}

pub unsafe fn handle_protocol(
    system_table: *mut EfiSystemTable,
    handle: EfiHandle,
    protocol: &EfiGuid,
    interface: &mut *mut c_void,
) -> EfiStatus {
    unsafe { efi_handle_protocol(system_table, handle, protocol as *const EfiGuid, interface) }
}

pub unsafe fn loaded_image_protocol(
    system_table: *mut EfiSystemTable,
    image_handle: EfiHandle,
) -> Result<*mut EfiLoadedImageProtocol, EfiStatus> {
    let mut loaded_image: *mut EfiLoadedImageProtocol = core::ptr::null_mut();
    let status =
        unsafe { efi_loaded_image_protocol(system_table, image_handle, &raw mut loaded_image) };
    if status == status_success() {
        Ok(loaded_image)
    } else {
        Err(status)
    }
}

pub unsafe fn copy_loaded_image_options_ascii(
    loaded_image: *mut EfiLoadedImageProtocol,
    buffer: &mut [u8],
) -> Result<usize, EfiStatus> {
    let mut len = 0usize;
    let status = unsafe {
        efi_copy_loaded_image_options_ascii(
            loaded_image,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut len,
        )
    };
    if status_is_error(status) {
        Err(status)
    } else {
        Ok(len)
    }
}

pub unsafe fn prepare_boot_handoff(
    system_table: *mut EfiSystemTable,
    image_handle: EfiHandle,
    cmdline_buffer: &mut [u8],
    system_table_copy: *mut EfiSystemTable,
    config_table_copy: *mut EfiConfigTable,
    config_table_capacity: usize,
    runtime_services_copy: *mut EfiRuntimeServices,
    boot_services_copy: *mut EfiBootServices,
    firmware_vendor_copy: *mut EfiChar16,
    firmware_vendor_capacity: usize,
    memory_map: &mut [u8],
) -> Result<EfiBootHandoff, EfiStatus> {
    let mut handoff = EfiBootHandoff {
        system_table: core::ptr::null(),
        image_handle: core::ptr::null_mut(),
        cmdline: core::ptr::null(),
        cmdline_size: 0,
        memory_map_size: 0,
        map_key: 0,
        descriptor_size: 0,
        descriptor_version: 0,
    };
    let status = unsafe {
        efi_prepare_boot_handoff(
            system_table,
            image_handle,
            cmdline_buffer.as_mut_ptr(),
            cmdline_buffer.len(),
            system_table_copy,
            config_table_copy,
            config_table_capacity,
            runtime_services_copy,
            boot_services_copy,
            firmware_vendor_copy,
            firmware_vendor_capacity,
            memory_map.as_mut_ptr().cast::<EfiMemoryDescriptor>(),
            memory_map.len(),
            &raw mut handoff,
        )
    };
    if status == status_success() {
        Ok(handoff)
    } else {
        Err(status)
    }
}
