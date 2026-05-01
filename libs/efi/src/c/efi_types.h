// efi_types.h - EFI 基础类型定义

#ifndef EFI_TYPES_H
#define EFI_TYPES_H

#ifdef __cplusplus
extern "C" {
#endif

#include <stddef.h>
#include <stdint.h>

typedef uintptr_t EfiStatus;
typedef uintptr_t EfiUintn;
typedef void *EfiHandle;
typedef void *EfiEvent;
typedef uint64_t EfiLba;
typedef uint64_t EfiTpl;
typedef uint64_t EfiPhysicalAddress;
typedef uint64_t EfiVirtualAddress;
typedef uint16_t EfiChar16;
typedef uint8_t EfiBoolean;

#define EFI_ERROR_BIT ((EfiStatus)(1ULL << (sizeof(EfiStatus) * 8 - 1)))
#define EFIERR(code)  (EFI_ERROR_BIT | ((EfiStatus)(code)))

#define EFI_SUCCESS_VALUE             ((EfiStatus)0)
#define EFI_LOAD_ERROR_VALUE          EFIERR(1)
#define EFI_INVALID_PARAMETER_VALUE   EFIERR(2)
#define EFI_UNSUPPORTED_VALUE         EFIERR(3)
#define EFI_BAD_BUFFER_SIZE_VALUE     EFIERR(4)
#define EFI_BUFFER_TOO_SMALL_VALUE    EFIERR(5)
#define EFI_NOT_READY_VALUE           EFIERR(6)
#define EFI_DEVICE_ERROR_VALUE        EFIERR(7)
#define EFI_WRITE_PROTECTED_VALUE     EFIERR(8)
#define EFI_OUT_OF_RESOURCES_VALUE    EFIERR(9)
#define EFI_VOLUME_CORRUPTED_VALUE    EFIERR(10)
#define EFI_VOLUME_FULL_VALUE         EFIERR(11)
#define EFI_NO_MEDIA_VALUE            EFIERR(12)
#define EFI_MEDIA_CHANGED_VALUE       EFIERR(13)
#define EFI_NOT_FOUND_VALUE           EFIERR(14)
#define EFI_ACCESS_DENIED_VALUE       EFIERR(15)
#define EFI_NO_RESPONSE_VALUE         EFIERR(16)
#define EFI_NO_MAPPING_VALUE          EFIERR(17)
#define EFI_TIMEOUT_VALUE             EFIERR(18)
#define EFI_NOT_STARTED_VALUE         EFIERR(19)
#define EFI_ALREADY_STARTED_VALUE     EFIERR(20)
#define EFI_ABORTED_VALUE             EFIERR(21)
#define EFI_ICMP_ERROR_VALUE          EFIERR(22)
#define EFI_TFTP_ERROR_VALUE          EFIERR(23)
#define EFI_PROTOCOL_ERROR_VALUE      EFIERR(24)
#define EFI_INCOMPATIBLE_VERSION_VALUE EFIERR(25)
#define EFI_SECURITY_VIOLATION_VALUE  EFIERR(26)
#define EFI_CRC_ERROR_VALUE           EFIERR(27)
#define EFI_END_OF_MEDIA_VALUE        EFIERR(28)
#define EFI_END_OF_FILE_VALUE         EFIERR(31)
#define EFI_INVALID_LANGUAGE_VALUE    EFIERR(32)
#define EFI_COMPROMISED_DATA_VALUE    EFIERR(33)
#define EFI_IP_ADDRESS_CONFLICT_VALUE EFIERR(34)
#define EFI_HTTP_ERROR_VALUE          EFIERR(35)

#define EFI_WARN_UNKNOWN_GLYPH_VALUE  ((EfiStatus)1)
#define EFI_WARN_DELETE_FAILURE_VALUE ((EfiStatus)2)
#define EFI_WARN_WRITE_FAILURE_VALUE  ((EfiStatus)3)
#define EFI_WARN_BUFFER_TOO_SMALL_VALUE ((EfiStatus)4)
#define EFI_WARN_STALE_DATA_VALUE     ((EfiStatus)5)
#define EFI_WARN_FILE_SYSTEM_VALUE    ((EfiStatus)6)
#define EFI_WARN_RESET_REQUIRED_VALUE ((EfiStatus)7)

#define EFI_SYSTEM_TABLE_SIGNATURE_VALUE 0x5453595320494249ULL
#define EFI_BOOT_SERVICES_SIGNATURE_VALUE 0x56524553544f4f42ULL
#define EFI_RUNTIME_SERVICES_SIGNATURE_VALUE 0x56524553544e5552ULL

typedef struct EfiTableHeader {
    uint64_t signature;
    uint32_t revision;
    uint32_t header_size;
    uint32_t crc32;
    uint32_t reserved;
} EfiTableHeader;

typedef struct EfiGuid {
    uint32_t data1;
    uint16_t data2;
    uint16_t data3;
    uint8_t data4[8];
} EfiGuid;

typedef struct EfiConfigTable {
    EfiGuid vendor_guid;
    const void *vendor_table;
} EfiConfigTable;

typedef enum EfiMemoryType {
    EFI_RESERVED_MEMORY_TYPE = 0,
    EFI_LOADER_CODE = 1,
    EFI_LOADER_DATA = 2,
    EFI_BOOT_SERVICES_CODE = 3,
    EFI_BOOT_SERVICES_DATA = 4,
    EFI_RUNTIME_SERVICES_CODE = 5,
    EFI_RUNTIME_SERVICES_DATA = 6,
    EFI_CONVENTIONAL_MEMORY = 7,
    EFI_UNUSABLE_MEMORY = 8,
    EFI_ACPI_RECLAIM_MEMORY = 9,
    EFI_ACPI_MEMORY_NVS = 10,
    EFI_MEMORY_MAPPED_IO = 11,
    EFI_MEMORY_MAPPED_IO_PORT_SPACE = 12,
    EFI_PAL_CODE = 13,
    EFI_PERSISTENT_MEMORY = 14,
    EFI_MAX_MEMORY_TYPE = 15,
} EfiMemoryType;

typedef struct EfiMemoryDescriptor {
    uint32_t type_;
    EfiPhysicalAddress physical_start;
    EfiVirtualAddress virtual_start;
    uint64_t number_of_pages;
    uint64_t attribute;
} EfiMemoryDescriptor;

typedef struct EfiMemoryDescriptorExtended {
    uint32_t type_;
    uint32_t pad;
    EfiPhysicalAddress physical_start;
    EfiVirtualAddress virtual_start;
    uint64_t number_of_pages;
    uint64_t attribute;
} EfiMemoryDescriptorExtended;

#define EFI_MEMORY_UC  0x0000000000000001ULL
#define EFI_MEMORY_WC  0x0000000000000002ULL
#define EFI_MEMORY_WT  0x0000000000000004ULL
#define EFI_MEMORY_WB  0x0000000000000008ULL
#define EFI_MEMORY_UCE 0x0000000000000010ULL
#define EFI_MEMORY_WP  0x0000000000001000ULL
#define EFI_MEMORY_RP  0x0000000000002000ULL
#define EFI_MEMORY_XP  0x0000000000004000ULL
#define EFI_MEMORY_NV  0x0000000000008000ULL
#define EFI_MEMORY_MORE_RELIABLE 0x0000000000010000ULL
#define EFI_MEMORY_RO  0x0000000000020000ULL
#define EFI_MEMORY_SP  0x0000000000040000ULL
#define EFI_MEMORY_CPU_CRYPTO 0x0000000000080000ULL
#define EFI_MEMORY_RUNTIME 0x8000000000000000ULL

typedef enum EfiAllocateType {
    EFI_ALLOCATE_ANY_PAGES = 0,
    EFI_ALLOCATE_MAX_ADDRESS = 1,
    EFI_ALLOCATE_ADDRESS = 2,
    EFI_MAX_ALLOCATE_TYPE = 3,
} EfiAllocateType;

typedef enum EfiTimerDelay {
    EFI_TIMER_CANCEL = 0,
    EFI_TIMER_PERIODIC = 1,
    EFI_TIMER_RELATIVE = 2,
} EfiTimerDelay;

typedef enum EfiInterfaceType {
    EFI_NATIVE_INTERFACE = 0,
} EfiInterfaceType;

typedef enum EfiLocateSearchType {
    EFI_ALL_HANDLES = 0,
    EFI_BY_REGISTER_NOTIFY = 1,
    EFI_BY_PROTOCOL = 2,
} EfiLocateSearchType;

typedef struct EfiInputKey {
    uint16_t scan_code;
    uint16_t unicode_char;
} EfiInputKey;

typedef struct EfiSimpleTextInputProtocol EfiSimpleTextInputProtocol;
typedef EfiStatus (*EfiInputResetFn)(EfiSimpleTextInputProtocol *this,
                                     EfiBoolean extended_verification);
typedef EfiStatus (*EfiInputReadKeyFn)(EfiSimpleTextInputProtocol *this,
                                       EfiInputKey *key);
struct EfiSimpleTextInputProtocol {
    EfiInputResetFn reset;
    EfiInputReadKeyFn read_key_stroke;
    EfiEvent wait_for_key;
};

typedef struct EfiSimpleTextOutputMode {
    int32_t max_mode;
    int32_t mode;
    int32_t attribute;
    int32_t cursor_column;
    int32_t cursor_row;
    EfiBoolean cursor_visible;
} EfiSimpleTextOutputMode;

typedef struct EfiSimpleTextOutputProtocol EfiSimpleTextOutputProtocol;
typedef EfiStatus (*EfiTextResetFn)(EfiSimpleTextOutputProtocol *this,
                                    EfiBoolean extended_verification);
typedef EfiStatus (*EfiTextOutputStringFn)(EfiSimpleTextOutputProtocol *this,
                                           const EfiChar16 *string);
typedef EfiStatus (*EfiTextTestStringFn)(EfiSimpleTextOutputProtocol *this,
                                         const EfiChar16 *string);
typedef EfiStatus (*EfiTextQueryModeFn)(EfiSimpleTextOutputProtocol *this,
                                        EfiUintn mode_number,
                                        EfiUintn *columns,
                                        EfiUintn *rows);
typedef EfiStatus (*EfiTextSetModeFn)(EfiSimpleTextOutputProtocol *this,
                                      EfiUintn mode_number);
typedef EfiStatus (*EfiTextSetAttributeFn)(EfiSimpleTextOutputProtocol *this,
                                           EfiUintn attribute);
typedef EfiStatus (*EfiTextClearScreenFn)(EfiSimpleTextOutputProtocol *this);
typedef EfiStatus (*EfiTextSetCursorPositionFn)(EfiSimpleTextOutputProtocol *this,
                                                EfiUintn column,
                                                EfiUintn row);
typedef EfiStatus (*EfiTextEnableCursorFn)(EfiSimpleTextOutputProtocol *this,
                                           EfiBoolean visible);
struct EfiSimpleTextOutputProtocol {
    EfiTextResetFn reset;
    EfiTextOutputStringFn output_string;
    EfiTextTestStringFn test_string;
    EfiTextQueryModeFn query_mode;
    EfiTextSetModeFn set_mode;
    EfiTextSetAttributeFn set_attribute;
    EfiTextClearScreenFn clear_screen;
    EfiTextSetCursorPositionFn set_cursor_position;
    EfiTextEnableCursorFn enable_cursor;
    EfiSimpleTextOutputMode *mode;
};

typedef struct EfiSimplePointerState {
    int32_t relative_movement_x;
    int32_t relative_movement_y;
    int32_t relative_movement_z;
    EfiBoolean left_button;
    EfiBoolean right_button;
} EfiSimplePointerState;

typedef struct EfiSimplePointerMode {
    uint64_t resolution_x;
    uint64_t resolution_y;
    uint64_t resolution_z;
    EfiBoolean left_button;
    EfiBoolean right_button;
} EfiSimplePointerMode;

typedef struct EfiSimplePointerProtocol EfiSimplePointerProtocol;
typedef EfiStatus (*EfiPointerResetFn)(EfiSimplePointerProtocol *this,
                                       EfiBoolean extended_verification);
typedef EfiStatus (*EfiPointerGetStateFn)(EfiSimplePointerProtocol *this,
                                          EfiSimplePointerState *state);
struct EfiSimplePointerProtocol {
    EfiPointerResetFn reset;
    EfiPointerGetStateFn get_state;
    EfiEvent wait_for_input;
    EfiSimplePointerMode *mode;
};

typedef struct EfiSystemTable EfiSystemTable;

typedef struct EfiLoadedImageProtocol {
    uint32_t revision;
    EfiHandle parent_handle;
    EfiSystemTable *system_table;
    EfiHandle device_handle;
    void *file_path;
    void *reserved;
    uint32_t load_options_size;
    void *load_options;
    void *image_base;
    uint64_t image_size;
    EfiMemoryType image_code_type;
    EfiMemoryType image_data_type;
    EfiStatus (*unload)(EfiHandle image_handle);
} EfiLoadedImageProtocol;

typedef struct EfiCapsuleHeader EfiCapsuleHeader;
typedef struct EfiTime EfiTime;
typedef struct EfiTimeCapabilities EfiTimeCapabilities;
typedef struct EfiMemoryRangeCapsule EfiMemoryRangeCapsule;

typedef struct EfiRuntimeServices {
    EfiTableHeader hdr;
    void *get_time;
    void *set_time;
    void *get_wakeup_time;
    void *set_wakeup_time;
    void *set_virtual_address_map;
    void *convert_pointer;
    void *get_variable;
    void *get_next_variable_name;
    void *set_variable;
    void *get_next_high_mono_count;
    void *reset_system;
    void *update_capsule;
    void *query_capsule_capabilities;
    void *query_variable_info;
} EfiRuntimeServices;

typedef struct EfiDevicePathProtocol {
    uint8_t type;
    uint8_t sub_type;
    uint8_t length[2];
} EfiDevicePathProtocol;

typedef struct EfiOpenProtocolInformationEntry {
    EfiHandle agent_handle;
    EfiHandle controller_handle;
    uint32_t attributes;
    uint32_t open_count;
} EfiOpenProtocolInformationEntry;

typedef EfiStatus (*EfiRaiseTplFn)(EfiTpl new_tpl);
typedef void (*EfiRestoreTplFn)(EfiTpl old_tpl);
typedef EfiStatus (*EfiAllocatePagesFn)(EfiAllocateType type,
                                        EfiMemoryType memory_type,
                                        EfiUintn pages,
                                        EfiPhysicalAddress *memory);
typedef EfiStatus (*EfiFreePagesFn)(EfiPhysicalAddress memory, EfiUintn pages);
typedef EfiStatus (*EfiGetMemoryMapFn)(EfiUintn *memory_map_size,
                                       EfiMemoryDescriptor *memory_map,
                                       EfiUintn *map_key,
                                       EfiUintn *descriptor_size,
                                       uint32_t *descriptor_version);
typedef EfiStatus (*EfiAllocatePoolFn)(EfiMemoryType pool_type,
                                       EfiUintn size,
                                       void **buffer);
typedef EfiStatus (*EfiFreePoolFn)(void *buffer);
typedef EfiStatus (*EfiCreateEventFn)(uint32_t type,
                                      EfiTpl notify_tpl,
                                      void *notify_function,
                                      void *notify_context,
                                      EfiEvent *event);
typedef EfiStatus (*EfiSetTimerFn)(EfiEvent event,
                                   EfiTimerDelay type,
                                   uint64_t trigger_time);
typedef EfiStatus (*EfiWaitForEventFn)(EfiUintn number_of_events,
                                       EfiEvent *event,
                                       EfiUintn *index);
typedef EfiStatus (*EfiSignalEventFn)(EfiEvent event);
typedef EfiStatus (*EfiCloseEventFn)(EfiEvent event);
typedef EfiStatus (*EfiCheckEventFn)(EfiEvent event);
typedef EfiStatus (*EfiInstallProtocolInterfaceFn)(EfiHandle *handle,
                                                   const EfiGuid *protocol,
                                                   EfiInterfaceType interface_type,
                                                   void *interface);
typedef EfiStatus (*EfiReinstallProtocolInterfaceFn)(EfiHandle handle,
                                                     const EfiGuid *protocol,
                                                     void *old_interface,
                                                     void *new_interface);
typedef EfiStatus (*EfiUninstallProtocolInterfaceFn)(EfiHandle handle,
                                                     const EfiGuid *protocol,
                                                     void *interface);
typedef EfiStatus (*EfiHandleProtocolFn)(EfiHandle handle,
                                         const EfiGuid *protocol,
                                         void **interface);
typedef EfiStatus (*EfiRegisterProtocolNotifyFn)(const EfiGuid *protocol,
                                                 EfiEvent event,
                                                 void **registration);
typedef EfiStatus (*EfiLocateHandleFn)(EfiLocateSearchType search_type,
                                       const EfiGuid *protocol,
                                       void *search_key,
                                       EfiUintn *buffer_size,
                                       EfiHandle *buffer);
typedef EfiStatus (*EfiLocateDevicePathFn)(const EfiGuid *protocol,
                                           EfiDevicePathProtocol **device_path,
                                           EfiHandle *device);
typedef EfiStatus (*EfiInstallConfigurationTableFn)(const EfiGuid *guid,
                                                    void *table);
typedef EfiStatus (*EfiLoadImageFn)(EfiBoolean boot_policy,
                                    EfiHandle parent_image_handle,
                                    EfiDevicePathProtocol *device_path,
                                    void *source_buffer,
                                    EfiUintn source_size,
                                    EfiHandle *image_handle);
typedef EfiStatus (*EfiStartImageFn)(EfiHandle image_handle,
                                     EfiUintn *exit_data_size,
                                     EfiChar16 **exit_data);
typedef EfiStatus (*EfiExitFn)(EfiHandle image_handle,
                               EfiStatus exit_status,
                               EfiUintn exit_data_size,
                               EfiChar16 *exit_data);
typedef EfiStatus (*EfiUnloadImageFn)(EfiHandle image_handle);
typedef EfiStatus (*EfiExitBootServicesFn)(EfiHandle image_handle,
                                           EfiUintn map_key);
typedef EfiStatus (*EfiGetNextMonotonicCountFn)(uint64_t *count);
typedef EfiStatus (*EfiStallFn)(EfiUintn microseconds);
typedef EfiStatus (*EfiSetWatchdogTimerFn)(EfiUintn timeout,
                                           uint64_t watchdog_code,
                                           EfiUintn data_size,
                                           const EfiChar16 *watchdog_data);
typedef EfiStatus (*EfiConnectControllerFn)(EfiHandle controller_handle,
                                            EfiHandle *driver_image_handle,
                                            EfiDevicePathProtocol *remaining_device_path,
                                            EfiBoolean recursive);
typedef EfiStatus (*EfiDisconnectControllerFn)(EfiHandle controller_handle,
                                               EfiHandle driver_image_handle,
                                               EfiHandle child_handle);
typedef EfiStatus (*EfiOpenProtocolFn)(EfiHandle handle,
                                       const EfiGuid *protocol,
                                       void **interface,
                                       EfiHandle agent_handle,
                                       EfiHandle controller_handle,
                                       uint32_t attributes);
typedef EfiStatus (*EfiCloseProtocolFn)(EfiHandle handle,
                                        const EfiGuid *protocol,
                                        EfiHandle agent_handle,
                                        EfiHandle controller_handle);
typedef EfiStatus (*EfiOpenProtocolInformationFn)(
    EfiHandle handle,
    const EfiGuid *protocol,
    EfiOpenProtocolInformationEntry **entry_buffer,
    EfiUintn *entry_count);
typedef EfiStatus (*EfiProtocolsPerHandleFn)(EfiHandle handle,
                                             EfiGuid ***protocol_buffer,
                                             EfiUintn *protocol_buffer_count);
typedef EfiStatus (*EfiLocateHandleBufferFn)(EfiLocateSearchType search_type,
                                             const EfiGuid *protocol,
                                             void *search_key,
                                             EfiUintn *no_handles,
                                             EfiHandle **buffer);
typedef EfiStatus (*EfiLocateProtocolFn)(const EfiGuid *protocol,
                                         void *registration,
                                         void **interface);
typedef EfiStatus (*EfiInstallMultipleProtocolInterfacesFn)(EfiHandle *handle, ...);
typedef EfiStatus (*EfiUninstallMultipleProtocolInterfacesFn)(EfiHandle handle, ...);
typedef EfiStatus (*EfiCalculateCrc32Fn)(void *data,
                                         EfiUintn data_size,
                                         uint32_t *crc32);
typedef void (*EfiCopyMemFn)(void *destination, const void *source, EfiUintn length);
typedef void (*EfiSetMemFn)(void *buffer, EfiUintn size, uint8_t value);
typedef EfiStatus (*EfiCreateEventExFn)(uint32_t type,
                                        EfiTpl notify_tpl,
                                        void *notify_function,
                                        const void *notify_context,
                                        const EfiGuid *event_group,
                                        EfiEvent *event);

typedef struct EfiBootServices {
    EfiTableHeader hdr;
    EfiRaiseTplFn raise_tpl;
    EfiRestoreTplFn restore_tpl;
    EfiAllocatePagesFn allocate_pages;
    EfiFreePagesFn free_pages;
    EfiGetMemoryMapFn get_memory_map;
    EfiAllocatePoolFn allocate_pool;
    EfiFreePoolFn free_pool;
    EfiCreateEventFn create_event;
    EfiSetTimerFn set_timer;
    EfiWaitForEventFn wait_for_event;
    EfiSignalEventFn signal_event;
    EfiCloseEventFn close_event;
    EfiCheckEventFn check_event;
    EfiInstallProtocolInterfaceFn install_protocol_interface;
    EfiReinstallProtocolInterfaceFn reinstall_protocol_interface;
    EfiUninstallProtocolInterfaceFn uninstall_protocol_interface;
    EfiHandleProtocolFn handle_protocol;
    void *reserved;
    EfiRegisterProtocolNotifyFn register_protocol_notify;
    EfiLocateHandleFn locate_handle;
    EfiLocateDevicePathFn locate_device_path;
    EfiInstallConfigurationTableFn install_configuration_table;
    EfiLoadImageFn load_image;
    EfiStartImageFn start_image;
    EfiExitFn exit;
    EfiUnloadImageFn unload_image;
    EfiExitBootServicesFn exit_boot_services;
    EfiGetNextMonotonicCountFn get_next_monotonic_count;
    EfiStallFn stall;
    EfiSetWatchdogTimerFn set_watchdog_timer;
    EfiConnectControllerFn connect_controller;
    EfiDisconnectControllerFn disconnect_controller;
    EfiOpenProtocolFn open_protocol;
    EfiCloseProtocolFn close_protocol;
    EfiOpenProtocolInformationFn open_protocol_information;
    EfiProtocolsPerHandleFn protocols_per_handle;
    EfiLocateHandleBufferFn locate_handle_buffer;
    EfiLocateProtocolFn locate_protocol;
    EfiInstallMultipleProtocolInterfacesFn install_multiple_protocol_interfaces;
    EfiUninstallMultipleProtocolInterfacesFn uninstall_multiple_protocol_interfaces;
    EfiCalculateCrc32Fn calculate_crc32;
    EfiCopyMemFn copy_mem;
    EfiSetMemFn set_mem;
    EfiCreateEventExFn create_event_ex;
} EfiBootServices;

struct EfiSystemTable {
    EfiTableHeader hdr;
    const EfiChar16 *firmware_vendor;
    uint32_t firmware_revision;
    EfiHandle console_in_handle;
    EfiSimpleTextInputProtocol *con_in;
    EfiHandle console_out_handle;
    EfiSimpleTextOutputProtocol *con_out;
    EfiHandle standard_error_handle;
    EfiSimpleTextOutputProtocol *std_err;
    EfiRuntimeServices *runtime_services;
    EfiBootServices *boot_services;
    EfiUintn number_of_table_entries;
    EfiConfigTable *configuration_table;
};

extern const EfiStatus EFI_STATUS_SUCCESS;
extern const EfiStatus EFI_STATUS_LOAD_ERROR;
extern const EfiStatus EFI_STATUS_INVALID_PARAMETER;
extern const EfiStatus EFI_STATUS_UNSUPPORTED;
extern const EfiStatus EFI_STATUS_BAD_BUFFER_SIZE;
extern const EfiStatus EFI_STATUS_BUFFER_TOO_SMALL;
extern const EfiStatus EFI_STATUS_NOT_READY;
extern const EfiStatus EFI_STATUS_DEVICE_ERROR;
extern const EfiStatus EFI_STATUS_WRITE_PROTECTED;
extern const EfiStatus EFI_STATUS_NOT_FOUND;
extern const EfiStatus EFI_STATUS_ACCESS_DENIED;
extern const EfiStatus EFI_STATUS_NO_RESPONSE;
extern const EfiStatus EFI_STATUS_NO_MAPPING;
extern const EfiStatus EFI_STATUS_TIMEOUT;
extern const EfiStatus EFI_STATUS_NOT_STARTED;
extern const EfiStatus EFI_STATUS_ALREADY_STARTED;
extern const EfiStatus EFI_STATUS_ABORTED;
extern const EfiStatus EFI_STATUS_ICMP_ERROR;
extern const EfiStatus EFI_STATUS_TFTP_ERROR;
extern const EfiStatus EFI_STATUS_PROTOCOL_ERROR;
extern const EfiStatus EFI_STATUS_INCOMPATIBLE_VERSION;
extern const EfiStatus EFI_STATUS_SECURITY_VIOLATION;
extern const EfiStatus EFI_STATUS_CRC_ERROR;
extern const EfiStatus EFI_STATUS_END_OF_MEDIA;
extern const EfiStatus EFI_STATUS_END_OF_FILE;
extern const EfiStatus EFI_STATUS_INVALID_LANGUAGE;
extern const EfiStatus EFI_STATUS_COMPROMISED_DATA;
extern const EfiStatus EFI_STATUS_IP_ADDRESS_CONFLICT;
extern const EfiStatus EFI_STATUS_HTTP_ERROR;
extern const EfiStatus EFI_STATUS_OUT_OF_RESOURCES;
extern const EfiStatus EFI_STATUS_VOLUME_CORRUPTED;
extern const EfiStatus EFI_STATUS_VOLUME_FULL;
extern const EfiStatus EFI_STATUS_NO_MEDIA;
extern const EfiStatus EFI_STATUS_MEDIA_CHANGED;
extern const EfiStatus EFI_STATUS_WARN_UNKNOWN_GLYPH;
extern const EfiStatus EFI_STATUS_WARN_DELETE_FAILURE;
extern const EfiStatus EFI_STATUS_WARN_WRITE_FAILURE;
extern const EfiStatus EFI_STATUS_WARN_BUFFER_TOO_SMALL;
extern const EfiStatus EFI_STATUS_WARN_STALE_DATA;
extern const EfiStatus EFI_STATUS_WARN_FILE_SYSTEM;
extern const EfiStatus EFI_STATUS_WARN_RESET_REQUIRED;

extern const uint32_t EFI_MEMORY_TYPE_RESERVED_MEMORY_TYPE;
extern const uint32_t EFI_MEMORY_TYPE_LOADER_CODE;
extern const uint32_t EFI_MEMORY_TYPE_LOADER_DATA;
extern const uint32_t EFI_MEMORY_TYPE_BOOT_SERVICES_CODE;
extern const uint32_t EFI_MEMORY_TYPE_BOOT_SERVICES_DATA;
extern const uint32_t EFI_MEMORY_TYPE_RUNTIME_SERVICES_CODE;
extern const uint32_t EFI_MEMORY_TYPE_RUNTIME_SERVICES_DATA;
extern const uint32_t EFI_MEMORY_TYPE_CONVENTIONAL_MEMORY;
extern const uint32_t EFI_MEMORY_TYPE_UNUSABLE_MEMORY;
extern const uint32_t EFI_MEMORY_TYPE_ACPI_RECLAIM_MEMORY;
extern const uint32_t EFI_MEMORY_TYPE_ACPI_MEMORY_NVS;
extern const uint32_t EFI_MEMORY_TYPE_MEMORY_MAPPED_IO;
extern const uint32_t EFI_MEMORY_TYPE_MEMORY_MAPPED_IO_PORT_SPACE;
extern const uint32_t EFI_MEMORY_TYPE_PAL_CODE;
extern const uint32_t EFI_MEMORY_TYPE_PERSISTENT_MEMORY;
extern const uint32_t EFI_MEMORY_TYPE_MAX_MEMORY_TYPE;

extern const EfiGuid ACPI_20_TABLE_GUID;
extern const EfiGuid FDT_TABLE_GUID;
extern const EfiGuid SMBIOS3_TABLE_GUID;
extern const EfiGuid ACPI_TABLE_GUID;
extern const EfiGuid LOADED_IMAGE_PROTOCOL_GUID;
extern const EfiGuid SIMPLE_POINTER_PROTOCOL_GUID;
extern const EfiGuid SIMPLE_TEXT_INPUT_PROTOCOL_GUID;
extern const EfiGuid SIMPLE_TEXT_OUTPUT_PROTOCOL_GUID;

#ifdef __cplusplus
}
#endif

#endif // EFI_TYPES_H
