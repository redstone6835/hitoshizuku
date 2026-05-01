// efi_global.h - EFI 通用辅助函数头文件
// 命名风格跟随 Rust（函数 snake_case，类型 CamelCase）

#ifndef EFI_GLOBAL_H
#define EFI_GLOBAL_H

#include "efi_types.h"

#ifdef __cplusplus
extern "C" {
#endif

int efi_status_is_error(EfiStatus status);

int efi_status_is_success(EfiStatus status);

const char *efi_status_name(EfiStatus status);

const char *efi_memory_type_name(uint32_t type);

int efi_memory_type_is_usable_after_exit_boot_services(uint32_t type);

int efi_guid_equal(const EfiGuid *lhs, const EfiGuid *rhs);

int efi_table_header_is_valid(const EfiTableHeader *hdr,
                              uint64_t expected_signature,
                              unsigned long minimum_size);

int efi_boot_services_is_valid(const EfiBootServices *bs);

int efi_runtime_services_is_valid(const EfiRuntimeServices *rt);

int efi_system_table_is_valid(const EfiSystemTable *st);

int efi_system_table_copy(const EfiSystemTable *src, EfiSystemTable *dst);

int efi_system_table_snapshot(const EfiSystemTable *src,
                              EfiSystemTable *dst,
                              EfiConfigTable *config_table_copy,
                              unsigned long config_table_capacity,
                              EfiRuntimeServices *runtime_services_copy,
                              EfiBootServices *boot_services_copy,
                              EfiChar16 *firmware_vendor_copy,
                              unsigned long firmware_vendor_capacity,
                              unsigned long *out_config_table_count);

unsigned long efi_ascii_strlen(const char *ptr, unsigned long max_len);

const char *efi_known_config_table_name(const EfiGuid *guid);

// ─────────────────────────── System Table 方法 ─────────────────

/// 获取配置表数组的指针和长度。
///
/// st: 非空的 system_table 指针
/// out_entries: 输出配置表数组首地址
/// out_count: 输出配置表条目数量
/// 返回值: 若 st 有效且配置表存在，返回 0；否则返回 -1
int efi_system_table_config_tables(const EfiSystemTable *st,
                                   const EfiConfigTable **out_entries,
                                   unsigned long *out_count);

/// 在配置表中查找匹配指定 GUID 的条目。
///
/// st: 非空的 system_table 指针
/// guid: 要查找的 GUID
/// 返回值: 若找到，返回对应的 vendor_table 指针；否则返回 NULL
const void *efi_system_table_find_config_table(const EfiSystemTable *st,
                                               const EfiGuid *guid);

const void *efi_system_table_find_acpi_rsdp(const EfiSystemTable *st);

const void *efi_system_table_find_fdt(const EfiSystemTable *st);

/// 获取固件厂商字符串（UTF-16，不含结尾 NUL）。
///
/// st: 非空的 system_table 指针
/// out_ptr: 输出字符串首地址
/// out_len: 输出字符串长度（以 EfiChar16 为单位）
/// max_len: 最大扫描长度（防止无界扫描）
/// 返回值: 若成功获取，返回 0；若 firmware_vendor 为空或未找到终止符，返回 -1
int efi_system_table_firmware_vendor(const EfiSystemTable *st,
                                     const EfiChar16 **out_ptr,
                                     unsigned long *out_len,
                                     unsigned long max_len);

#ifdef __cplusplus
}
#endif

#endif // EFI_GLOBAL_H
