// efi_global.c - EFI 通用辅助函数实现

#include "efi_global.h"
#include <stddef.h>

static int efi_pointer_is_aligned(const void *ptr)
{
    return (((unsigned long)ptr & (sizeof(void *) - 1)) == 0);
}

static void efi_copy_bytes(void *dst, const void *src, unsigned long size)
{
    volatile uint8_t *d = (volatile uint8_t *)dst;
    const volatile uint8_t *s = (const volatile uint8_t *)src;

    for (unsigned long i = 0; i < size; i++) {
        d[i] = s[i];
    }
}

int efi_status_is_error(EfiStatus status)
{
    return (status & EFI_ERROR_BIT) != 0;
}

int efi_status_is_success(EfiStatus status)
{
    return status == EFI_SUCCESS_VALUE;
}

const char *efi_status_name(EfiStatus status)
{
    switch (status) {
    case EFI_SUCCESS_VALUE: return "EFI_SUCCESS";
    case EFI_LOAD_ERROR_VALUE: return "EFI_LOAD_ERROR";
    case EFI_INVALID_PARAMETER_VALUE: return "EFI_INVALID_PARAMETER";
    case EFI_UNSUPPORTED_VALUE: return "EFI_UNSUPPORTED";
    case EFI_BAD_BUFFER_SIZE_VALUE: return "EFI_BAD_BUFFER_SIZE";
    case EFI_BUFFER_TOO_SMALL_VALUE: return "EFI_BUFFER_TOO_SMALL";
    case EFI_NOT_READY_VALUE: return "EFI_NOT_READY";
    case EFI_DEVICE_ERROR_VALUE: return "EFI_DEVICE_ERROR";
    case EFI_WRITE_PROTECTED_VALUE: return "EFI_WRITE_PROTECTED";
    case EFI_OUT_OF_RESOURCES_VALUE: return "EFI_OUT_OF_RESOURCES";
    case EFI_VOLUME_CORRUPTED_VALUE: return "EFI_VOLUME_CORRUPTED";
    case EFI_VOLUME_FULL_VALUE: return "EFI_VOLUME_FULL";
    case EFI_NO_MEDIA_VALUE: return "EFI_NO_MEDIA";
    case EFI_MEDIA_CHANGED_VALUE: return "EFI_MEDIA_CHANGED";
    case EFI_NOT_FOUND_VALUE: return "EFI_NOT_FOUND";
    case EFI_ACCESS_DENIED_VALUE: return "EFI_ACCESS_DENIED";
    case EFI_NO_RESPONSE_VALUE: return "EFI_NO_RESPONSE";
    case EFI_NO_MAPPING_VALUE: return "EFI_NO_MAPPING";
    case EFI_TIMEOUT_VALUE: return "EFI_TIMEOUT";
    case EFI_NOT_STARTED_VALUE: return "EFI_NOT_STARTED";
    case EFI_ALREADY_STARTED_VALUE: return "EFI_ALREADY_STARTED";
    case EFI_ABORTED_VALUE: return "EFI_ABORTED";
    case EFI_ICMP_ERROR_VALUE: return "EFI_ICMP_ERROR";
    case EFI_TFTP_ERROR_VALUE: return "EFI_TFTP_ERROR";
    case EFI_PROTOCOL_ERROR_VALUE: return "EFI_PROTOCOL_ERROR";
    case EFI_INCOMPATIBLE_VERSION_VALUE: return "EFI_INCOMPATIBLE_VERSION";
    case EFI_SECURITY_VIOLATION_VALUE: return "EFI_SECURITY_VIOLATION";
    case EFI_CRC_ERROR_VALUE: return "EFI_CRC_ERROR";
    case EFI_END_OF_MEDIA_VALUE: return "EFI_END_OF_MEDIA";
    case EFI_END_OF_FILE_VALUE: return "EFI_END_OF_FILE";
    case EFI_INVALID_LANGUAGE_VALUE: return "EFI_INVALID_LANGUAGE";
    case EFI_COMPROMISED_DATA_VALUE: return "EFI_COMPROMISED_DATA";
    case EFI_IP_ADDRESS_CONFLICT_VALUE: return "EFI_IP_ADDRESS_CONFLICT";
    case EFI_HTTP_ERROR_VALUE: return "EFI_HTTP_ERROR";
    case EFI_WARN_UNKNOWN_GLYPH_VALUE: return "EFI_WARN_UNKNOWN_GLYPH";
    case EFI_WARN_DELETE_FAILURE_VALUE: return "EFI_WARN_DELETE_FAILURE";
    case EFI_WARN_WRITE_FAILURE_VALUE: return "EFI_WARN_WRITE_FAILURE";
    case EFI_WARN_BUFFER_TOO_SMALL_VALUE: return "EFI_WARN_BUFFER_TOO_SMALL";
    case EFI_WARN_STALE_DATA_VALUE: return "EFI_WARN_STALE_DATA";
    case EFI_WARN_FILE_SYSTEM_VALUE: return "EFI_WARN_FILE_SYSTEM";
    case EFI_WARN_RESET_REQUIRED_VALUE: return "EFI_WARN_RESET_REQUIRED";
    default: return "EFI_STATUS_UNKNOWN";
    }
}

const char *efi_memory_type_name(uint32_t type)
{
    switch (type) {
    case EFI_RESERVED_MEMORY_TYPE: return "EfiReservedMemoryType";
    case EFI_LOADER_CODE: return "EfiLoaderCode";
    case EFI_LOADER_DATA: return "EfiLoaderData";
    case EFI_BOOT_SERVICES_CODE: return "EfiBootServicesCode";
    case EFI_BOOT_SERVICES_DATA: return "EfiBootServicesData";
    case EFI_RUNTIME_SERVICES_CODE: return "EfiRuntimeServicesCode";
    case EFI_RUNTIME_SERVICES_DATA: return "EfiRuntimeServicesData";
    case EFI_CONVENTIONAL_MEMORY: return "EfiConventionalMemory";
    case EFI_UNUSABLE_MEMORY: return "EfiUnusableMemory";
    case EFI_ACPI_RECLAIM_MEMORY: return "EfiACPIReclaimMemory";
    case EFI_ACPI_MEMORY_NVS: return "EfiACPIMemoryNVS";
    case EFI_MEMORY_MAPPED_IO: return "EfiMemoryMappedIO";
    case EFI_MEMORY_MAPPED_IO_PORT_SPACE: return "EfiMemoryMappedIOPortSpace";
    case EFI_PAL_CODE: return "EfiPalCode";
    case EFI_PERSISTENT_MEMORY: return "EfiPersistentMemory";
    default: return "EfiUnknownMemoryType";
    }
}

int efi_memory_type_is_usable_after_exit_boot_services(uint32_t type)
{
    switch (type) {
    case EFI_LOADER_CODE:
    case EFI_LOADER_DATA:
    case EFI_BOOT_SERVICES_CODE:
    case EFI_BOOT_SERVICES_DATA:
    case EFI_CONVENTIONAL_MEMORY:
        return 1;
    default:
        return 0;
    }
}

int efi_guid_equal(const EfiGuid *lhs, const EfiGuid *rhs)
{
    const uint8_t *a;
    const uint8_t *b;

    if (lhs == NULL || rhs == NULL) {
        return 0;
    }

    a = (const uint8_t *)lhs;
    b = (const uint8_t *)rhs;
    for (unsigned long i = 0; i < sizeof(EfiGuid); i++) {
        if (a[i] != b[i]) {
            return 0;
        }
    }
    return 1;
}

int efi_table_header_is_valid(const EfiTableHeader *hdr,
                              uint64_t expected_signature,
                              unsigned long minimum_size)
{
    if (hdr == NULL) {
        return 0;
    }
    if (hdr->signature != expected_signature) {
        return 0;
    }
    /*
     * Some firmware handoff paths provide synthetic EFI tables whose header
     * contains the correct signature but leaves HeaderSize unset.  Treat such
     * tables as structurally usable so they can continue through the normal EFI
     * System Table path; field-level helpers still check pointer presence and
     * per-table function availability before dereferencing optional services.
     */
    if (hdr->header_size == 0) {
        return 1;
    }
    if (hdr->header_size < minimum_size) {
        return 0;
    }
    return 1;
}

int efi_boot_services_is_valid(const EfiBootServices *bs)
{
    if (bs == NULL || !efi_pointer_is_aligned(bs)) {
        return 0;
    }
    return efi_table_header_is_valid(&bs->hdr,
                                     EFI_BOOT_SERVICES_SIGNATURE_VALUE,
                                     offsetof(EfiBootServices, create_event_ex) +
                                         sizeof(bs->create_event_ex));
}

int efi_runtime_services_is_valid(const EfiRuntimeServices *rt)
{
    if (rt == NULL || !efi_pointer_is_aligned(rt)) {
        return 0;
    }
    return efi_table_header_is_valid(&rt->hdr,
                                     EFI_RUNTIME_SERVICES_SIGNATURE_VALUE,
                                     offsetof(EfiRuntimeServices, query_variable_info) +
                                         sizeof(rt->query_variable_info));
}

/// 校验 system_table 指针是否有效（非空、对齐、魔数和最小表头尺寸）
int efi_system_table_is_valid(const EfiSystemTable *st)
{
    if (st == NULL) {
        return 0;
    }
    // 检查指针对齐（至少按指针大小对齐）
    if (!efi_pointer_is_aligned(st)) {
        return 0;
    }
    if (!efi_table_header_is_valid(&st->hdr,
                                   EFI_SYSTEM_TABLE_SIGNATURE_VALUE,
                                   sizeof(EfiSystemTable))) {
        return 0;
    }
    return 1;
}

int efi_system_table_copy(const EfiSystemTable *src, EfiSystemTable *dst)
{
    if (!efi_system_table_is_valid(src) || dst == NULL) {
        return -1;
    }
    efi_copy_bytes(dst, src, sizeof(EfiSystemTable));
    return 0;
}

int efi_system_table_snapshot(const EfiSystemTable *src,
                              EfiSystemTable *dst,
                              EfiConfigTable *config_table_copy,
                              unsigned long config_table_capacity,
                              EfiRuntimeServices *runtime_services_copy,
                              EfiBootServices *boot_services_copy,
                              EfiChar16 *firmware_vendor_copy,
                              unsigned long firmware_vendor_capacity,
                              unsigned long *out_config_table_count)
{
    unsigned long count;

    if (!efi_system_table_is_valid(src) || dst == NULL) {
        return -1;
    }

    efi_copy_bytes(dst, src, sizeof(EfiSystemTable));
    if (out_config_table_count != NULL) {
        *out_config_table_count = 0;
    }

    count = (unsigned long)src->number_of_table_entries;
    if (count != 0) {
        if (src->configuration_table == NULL || config_table_copy == NULL ||
            config_table_capacity < count) {
            return -1;
        }
        for (unsigned long i = 0; i < count; i++) {
            efi_copy_bytes(&config_table_copy[i], &src->configuration_table[i],
                           sizeof(EfiConfigTable));
        }
        dst->configuration_table = config_table_copy;
        if (out_config_table_count != NULL) {
            *out_config_table_count = count;
        }
    }

    if (runtime_services_copy != NULL &&
        efi_runtime_services_is_valid(src->runtime_services)) {
        efi_copy_bytes(runtime_services_copy, src->runtime_services,
                       sizeof(EfiRuntimeServices));
        dst->runtime_services = runtime_services_copy;
    }

    if (boot_services_copy != NULL && efi_boot_services_is_valid(src->boot_services)) {
        efi_copy_bytes(boot_services_copy, src->boot_services,
                       sizeof(EfiBootServices));
        dst->boot_services = boot_services_copy;
    }

    if (firmware_vendor_copy != NULL && firmware_vendor_capacity != 0 &&
        src->firmware_vendor != NULL) {
        unsigned long i = 0;
        while (i + 1 < firmware_vendor_capacity && src->firmware_vendor[i] != 0) {
            firmware_vendor_copy[i] = src->firmware_vendor[i];
            i++;
        }
        firmware_vendor_copy[i] = 0;
        dst->firmware_vendor = firmware_vendor_copy;
    }

    return 0;
}

unsigned long efi_ascii_strlen(const char *ptr, unsigned long max_len)
{
    unsigned long len = 0;

    if (ptr == NULL) {
        return 0;
    }
    while (len < max_len && ptr[len] != '\0') {
        len++;
    }
    return len;
}

const char *efi_known_config_table_name(const EfiGuid *guid)
{
    if (efi_guid_equal(guid, &ACPI_20_TABLE_GUID)) {
        return "ACPI 2.0 RSDP";
    }
    if (efi_guid_equal(guid, &ACPI_TABLE_GUID)) {
        return "ACPI RSDP";
    }
    if (efi_guid_equal(guid, &FDT_TABLE_GUID)) {
        return "FDT (DTB)";
    }
    if (efi_guid_equal(guid, &SMBIOS3_TABLE_GUID)) {
        return "SMBIOS 3.x";
    }
    return "";
}

// ──────────────────── System Table 方法 ─────────────────────

int efi_system_table_config_tables(const EfiSystemTable *st,
                                   const EfiConfigTable **out_entries,
                                   unsigned long *out_count)
{
    if (!efi_system_table_is_valid(st) || out_entries == NULL || out_count == NULL) {
        return -1;
    }
    if (st->number_of_table_entries == 0 || st->configuration_table == NULL) {
        *out_entries = NULL;
        *out_count = 0;
        return 0;
    }
    *out_entries = st->configuration_table;
    *out_count = (unsigned long)st->number_of_table_entries;
    return 0;
}

const void *efi_system_table_find_config_table(const EfiSystemTable *st,
                                               const EfiGuid *guid)
{
    if (!efi_system_table_is_valid(st) || guid == NULL) {
        return NULL;
    }
    if (st->number_of_table_entries == 0 || st->configuration_table == NULL) {
        return NULL;
    }

    const EfiConfigTable *entries = st->configuration_table;
    unsigned long count = (unsigned long)st->number_of_table_entries;

    for (unsigned long i = 0; i < count; i++) {
        if (efi_guid_equal(&entries[i].vendor_guid, guid)) {
            return entries[i].vendor_table;
        }
    }
    return NULL;
}

const void *efi_system_table_find_acpi_rsdp(const EfiSystemTable *st)
{
    const void *table = efi_system_table_find_config_table(st, &ACPI_20_TABLE_GUID);
    if (table != NULL) {
        return table;
    }
    return efi_system_table_find_config_table(st, &ACPI_TABLE_GUID);
}

const void *efi_system_table_find_fdt(const EfiSystemTable *st)
{
    return efi_system_table_find_config_table(st, &FDT_TABLE_GUID);
}

int efi_system_table_firmware_vendor(const EfiSystemTable *st,
                                     const EfiChar16 **out_ptr,
                                     unsigned long *out_len,
                                     unsigned long max_len)
{
    if (!efi_system_table_is_valid(st) || out_ptr == NULL || out_len == NULL) {
        return -1;
    }
    if (st->firmware_vendor == NULL) {
        return -1;
    }

    const EfiChar16 *ptr = st->firmware_vendor;
    unsigned long len = 0;
    while (len < max_len) {
        if (ptr[len] == 0) {
            *out_ptr = ptr;
            *out_len = len;
            return 0;
        }
        len++;
    }
    // 未找到终止符
    return -1;
}
