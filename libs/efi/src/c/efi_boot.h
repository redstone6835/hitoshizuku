// efi_boot.h - EFI Boot Services 包装层
// 包装固件提供的 Boot Services 函数，提供内存映射获取、Boot Services 退出、
// 看门狗禁用、Stall、HandleProtocol 等功能。

#ifndef EFI_BOOT_H
#define EFI_BOOT_H

#ifdef __cplusplus
extern "C" {
#endif

#include "efi_types.h"
#include "efi_global.h"

typedef struct EfiBootHandoff {
    const EfiSystemTable *system_table;
    EfiHandle image_handle;
    const char *cmdline;
    unsigned long cmdline_size;
    unsigned long memory_map_size;
    unsigned long map_key;
    unsigned long descriptor_size;
    uint32_t descriptor_version;
} EfiBootHandoff;

// ────────────── 内存映射 ──────────────────────────────

/// 获取 EFI 内存映射。
///
/// 首次调用时 memory_map 应为 NULL，函数会填充 *size 为所需大小。
/// 调用者分配足够内存后再次调用以获取实际数据。
///
/// @param system_table  EFI System Table 指针
/// @param size          输入：缓冲大小；输出：实际大小
/// @param memory_map    内存映射缓冲区
/// @param map_key       输出：内存映射键值
/// @param descriptor_size 输出：每个描述符的大小
/// @param descriptor_version 输出：描述符版本
///
/// @return EFI_SUCCESS 或错误码
EfiStatus efi_get_memory_map(
    EfiSystemTable        *system_table,
    unsigned long         *size,
    EfiMemoryDescriptor   *memory_map,
    unsigned long         *map_key,
    unsigned long         *descriptor_size,
    uint32_t              *descriptor_version);

EfiStatus efi_get_memory_map_retry(
    EfiSystemTable        *system_table,
    unsigned long         *size,
    EfiMemoryDescriptor   *memory_map,
    unsigned long         *map_key,
    unsigned long         *descriptor_size,
    uint32_t              *descriptor_version);

// ────────────── Boot Services 退出 ────────────────────────

/// 退出 EFI Boot Services。
///
/// 调用后所有 Boot Services 将不可用，固件释放对硬件的独占控制。
///
/// @param system_table  EFI System Table 指针
/// @param image_handle  当前映像句柄
/// @param map_key       内存映射键值
/// @return EFI_SUCCESS 或错误码
EfiStatus efi_exit_boot_services(
    EfiSystemTable *system_table,
    EfiHandle        image_handle,
    unsigned long    map_key);

EfiStatus efi_exit_boot_services_with_memory_map(
    EfiSystemTable       *system_table,
    EfiHandle             image_handle,
    EfiMemoryDescriptor  *memory_map,
    unsigned long         memory_map_capacity,
    EfiBootHandoff       *out_handoff);

// ────────────── 看门狗 ────────────────────────────────

/// 禁用 UEFI 看门狗定时器。
///
/// 在内核接管后，应该禁用固件的看门狗以避免系统在启动过程中被强制重启。
///
/// @param system_table  EFI System Table 指针
/// @return EFI_SUCCESS 或错误码
EfiStatus efi_disable_watchdog(EfiSystemTable *system_table);

// ────────────── 配置表查找 ──────────────────────────────

/// 在 System Table 的配置表中查找指定 GUID。
///
/// @param system_table  EFI System Table 指针
/// @param guid          要查找的 GUID
/// @return 找到的表指针，NULL 表示未找到
void *efi_locate_config_table(
    const EfiSystemTable *system_table,
    const EfiGuid        *guid);

// ────────────── 延迟 ──────────────────────────────

/// 使用 EFI Boot Services Stall 进行微秒级延迟。
///
/// @param system_table   EFI System Table 指针
/// @param microseconds   延迟的微秒数
/// @return EFI_SUCCESS 或错误码
EfiStatus efi_stall(
    EfiSystemTable *system_table,
    unsigned long    microseconds);

// ────────────── Handle Protocol ──────────────────────────────

/// 通过 HandleProtocol 获取协议接口。
///
/// @param system_table  EFI System Table 指针
/// @param handle        句柄
/// @param protocol      协议 GUID
/// @param interface     输出：协议接口指针
/// @return EFI_SUCCESS 或错误码
EfiStatus efi_handle_protocol(
    EfiSystemTable *system_table,
    EfiHandle        handle,
    const EfiGuid   *protocol,
    void           **interface);

EfiStatus efi_loaded_image_protocol(
    EfiSystemTable *system_table,
    EfiHandle       image_handle,
    EfiLoadedImageProtocol **loaded_image);

EfiStatus efi_copy_loaded_image_options_ascii(
    EfiLoadedImageProtocol *loaded_image,
    char                   *buffer,
    unsigned long           buffer_len,
    unsigned long          *out_len);

EfiStatus efi_prepare_boot_handoff(
    EfiSystemTable       *system_table,
    EfiHandle             image_handle,
    char                 *cmdline_buffer,
    unsigned long         cmdline_buffer_len,
    EfiSystemTable       *system_table_copy,
    EfiConfigTable       *config_table_copy,
    unsigned long         config_table_capacity,
    EfiRuntimeServices   *runtime_services_copy,
    EfiBootServices      *boot_services_copy,
    EfiChar16            *firmware_vendor_copy,
    unsigned long         firmware_vendor_capacity,
    EfiMemoryDescriptor  *memory_map,
    unsigned long         memory_map_capacity,
    EfiBootHandoff       *out_handoff);

#ifdef __cplusplus
}
#endif

#endif // EFI_BOOT_H
