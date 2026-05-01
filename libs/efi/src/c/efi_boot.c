// efi_boot.c – Boot Services 包装函数实现

#include "efi_boot.h"

#define EFI_FIELD_END(type, field) \
    (offsetof(type, field) + sizeof(((type *)0)->field))

#define EFI_BOOT_HANDOFF_EXIT_RETRIES 5UL

static int efi_boot_services_has_field(const EfiBootServices *bs,
                                       unsigned long field_end)
{
    if (bs == NULL) {
        return 0;
    }
    if (((unsigned long)bs & (sizeof(void *) - 1)) != 0) {
        return 0;
    }
    return efi_table_header_is_valid(&bs->hdr,
                                     EFI_BOOT_SERVICES_SIGNATURE_VALUE,
                                     field_end);
}

static int efi_memory_map_result_is_valid(unsigned long map_size,
                                          unsigned long descriptor_size,
                                          unsigned long capacity)
{
    if (map_size == 0 || map_size > capacity) {
        return 0;
    }
    if (descriptor_size < sizeof(EfiMemoryDescriptor)) {
        return 0;
    }
    if ((map_size % descriptor_size) != 0) {
        return 0;
    }
    return 1;
}

static EfiStatus efi_get_memory_map_for_exit(
    EfiSystemTable        *st,
    EfiMemoryDescriptor   *memory_map,
    unsigned long          memory_map_capacity,
    unsigned long         *out_map_size,
    unsigned long         *out_map_key,
    unsigned long         *out_descriptor_size,
    uint32_t              *out_descriptor_version)
{
    EfiStatus status;
    unsigned long map_size;
    unsigned long map_key = 0;
    unsigned long descriptor_size = 0;
    uint32_t descriptor_version = 0;

    if (!memory_map || memory_map_capacity < sizeof(EfiMemoryDescriptor) ||
        !out_map_size || !out_map_key || !out_descriptor_size ||
        !out_descriptor_version) {
        return EFI_INVALID_PARAMETER_VALUE;
    }

    map_size = memory_map_capacity;
    status = efi_get_memory_map(st, &map_size, memory_map, &map_key,
                                &descriptor_size, &descriptor_version);
    if (status != EFI_SUCCESS_VALUE) {
        return status;
    }
    if (!efi_memory_map_result_is_valid(map_size, descriptor_size,
                                        memory_map_capacity)) {
        return EFI_DEVICE_ERROR_VALUE;
    }

    *out_map_size = map_size;
    *out_map_key = map_key;
    *out_descriptor_size = descriptor_size;
    *out_descriptor_version = descriptor_version;
    return EFI_SUCCESS_VALUE;
}

// ────────────── efi_locate_config_table ───────────────────────

void *efi_locate_config_table(
    const EfiSystemTable *st,
    const EfiGuid *guid)
{
    uint64_t i;

    if (!efi_system_table_is_valid(st) || !guid || !st->configuration_table)
        return NULL;

    for (i = 0; i < st->number_of_table_entries; i++) {
        if (efi_guid_equal(&st->configuration_table[i].vendor_guid, guid)) {
            // vendor_table 是 const void *，但调用方可能需要可变指针
            return (void *)st->configuration_table[i].vendor_table;
        }
    }
    return NULL;
}

// ────────────── efi_get_memory_map ────────────────────────────

EfiStatus efi_get_memory_map(
    EfiSystemTable        *st,
    unsigned long         *size,
    EfiMemoryDescriptor   *memory_map,
    unsigned long         *map_key,
    unsigned long         *descriptor_size,
    uint32_t              *descriptor_version)
{
    if (!efi_system_table_is_valid(st))
        return EFI_INVALID_PARAMETER_VALUE;
    if (!st->boot_services)
        return EFI_UNSUPPORTED_VALUE;
    if (!size || !map_key || !descriptor_size || !descriptor_version)
        return EFI_INVALID_PARAMETER_VALUE;
    if (!efi_boot_services_has_field(st->boot_services,
                                     EFI_FIELD_END(EfiBootServices,
                                                   get_memory_map)) ||
        !st->boot_services->get_memory_map)
        return EFI_UNSUPPORTED_VALUE;

    return st->boot_services->get_memory_map(
        size, memory_map, map_key, descriptor_size, descriptor_version);
}

EfiStatus efi_get_memory_map_retry(
    EfiSystemTable        *st,
    unsigned long         *size,
    EfiMemoryDescriptor   *memory_map,
    unsigned long         *map_key,
    unsigned long         *descriptor_size,
    uint32_t              *descriptor_version)
{
    EfiStatus status;
    unsigned long capacity;

    if (!size) {
        return EFI_INVALID_PARAMETER_VALUE;
    }
    capacity = *size;

    status = efi_get_memory_map(st, size, memory_map, map_key,
                                descriptor_size, descriptor_version);
    if (status != EFI_SUCCESS_VALUE) {
        return status;
    }
    if (!descriptor_size ||
        !efi_memory_map_result_is_valid(*size, *descriptor_size, capacity)) {
        return EFI_DEVICE_ERROR_VALUE;
    }
    return EFI_SUCCESS_VALUE;
}

// ────────────── efi_exit_boot_services ────────────────────────

EfiStatus efi_exit_boot_services(
    EfiSystemTable *st,
    EfiHandle       image_handle,
    unsigned long   map_key)
{
    if (!efi_system_table_is_valid(st))
        return EFI_INVALID_PARAMETER_VALUE;
    if (!st->boot_services)
        return EFI_UNSUPPORTED_VALUE;

    if (!efi_boot_services_has_field(st->boot_services,
                                     EFI_FIELD_END(EfiBootServices,
                                                   exit_boot_services)) ||
        !st->boot_services->exit_boot_services)
        return EFI_UNSUPPORTED_VALUE;

    return st->boot_services->exit_boot_services(image_handle, map_key);
}

EfiStatus efi_exit_boot_services_with_memory_map(
    EfiSystemTable *st,
    EfiHandle image_handle,
    EfiMemoryDescriptor *memory_map,
    unsigned long memory_map_capacity,
    EfiBootHandoff *out)
{
    EfiStatus status;
    unsigned long attempt;
    unsigned long map_size = 0;
    unsigned long map_key = 0;
    unsigned long descriptor_size = 0;
    uint32_t descriptor_version = 0;

    if (out == NULL) {
        return EFI_INVALID_PARAMETER_VALUE;
    }
    out->system_table = st;
    out->image_handle = image_handle;
    out->cmdline = NULL;
    out->cmdline_size = 0;
    out->memory_map_size = 0;
    out->map_key = 0;
    out->descriptor_size = 0;
    out->descriptor_version = 0;

    if (!efi_system_table_is_valid(st)) {
        return EFI_INVALID_PARAMETER_VALUE;
    }
    if (!st->boot_services) {
        return EFI_UNSUPPORTED_VALUE;
    }
    if (!efi_boot_services_has_field(st->boot_services,
                                     EFI_FIELD_END(EfiBootServices,
                                                   exit_boot_services)) ||
        !st->boot_services->exit_boot_services) {
        return EFI_UNSUPPORTED_VALUE;
    }
    if (image_handle == NULL || !memory_map || memory_map_capacity == 0) {
        return EFI_INVALID_PARAMETER_VALUE;
    }

    status = EFI_DEVICE_ERROR_VALUE;
    for (attempt = 0; attempt < EFI_BOOT_HANDOFF_EXIT_RETRIES; attempt++) {
        status = efi_get_memory_map_for_exit(st, memory_map, memory_map_capacity,
                                             &map_size, &map_key,
                                             &descriptor_size,
                                             &descriptor_version);
        out->memory_map_size = map_size;
        out->map_key = map_key;
        out->descriptor_size = descriptor_size;
        out->descriptor_version = descriptor_version;
        if (status != EFI_SUCCESS_VALUE) {
            return status;
        }
        status = efi_exit_boot_services(st, image_handle, map_key);
        if (status == EFI_SUCCESS_VALUE) {
            break;
        }
        if (status != EFI_INVALID_PARAMETER_VALUE) {
            return status;
        }
    }
    if (status != EFI_SUCCESS_VALUE) {
        return status;
    }

    out->memory_map_size = map_size;
    out->map_key = map_key;
    out->descriptor_size = descriptor_size;
    out->descriptor_version = descriptor_version;
    return EFI_SUCCESS_VALUE;
}

// ────────────── efi_disable_watchdog ──────────────────────────

EfiStatus efi_disable_watchdog(EfiSystemTable *st)
{
    if (!efi_system_table_is_valid(st))
        return EFI_INVALID_PARAMETER_VALUE;
    if (!st->boot_services)
        return EFI_UNSUPPORTED_VALUE;

    if (!efi_boot_services_has_field(st->boot_services,
                                     EFI_FIELD_END(EfiBootServices,
                                                   set_watchdog_timer)) ||
        !st->boot_services->set_watchdog_timer)
        return EFI_UNSUPPORTED_VALUE;

    // timeout == 0 禁用看门狗（UEFI Spec § 7.5）
    return st->boot_services->set_watchdog_timer(0, 0, 0, NULL);
}

// ────────────── efi_stall ─────────────────────────────────────

EfiStatus efi_stall(EfiSystemTable *st, unsigned long microseconds)
{
    if (!efi_system_table_is_valid(st))
        return EFI_INVALID_PARAMETER_VALUE;
    if (!st->boot_services)
        return EFI_UNSUPPORTED_VALUE;

    if (!efi_boot_services_has_field(st->boot_services,
                                     EFI_FIELD_END(EfiBootServices, stall)) ||
        !st->boot_services->stall)
        return EFI_UNSUPPORTED_VALUE;

    return st->boot_services->stall(microseconds);
}

// ────────────── efi_handle_protocol ───────────────────────────

EfiStatus efi_handle_protocol(
    EfiSystemTable *st,
    EfiHandle        handle,
    const EfiGuid   *protocol,
    void           **interface)
{
    if (!efi_system_table_is_valid(st))
        return EFI_INVALID_PARAMETER_VALUE;
    if (!st->boot_services)
        return EFI_UNSUPPORTED_VALUE;
    if (!interface || !protocol)
        return EFI_INVALID_PARAMETER_VALUE;

    if (!efi_boot_services_has_field(st->boot_services,
                                     EFI_FIELD_END(EfiBootServices,
                                                   handle_protocol)) ||
        !st->boot_services->handle_protocol)
        return EFI_UNSUPPORTED_VALUE;

    return st->boot_services->handle_protocol(handle, protocol, interface);
}

EfiStatus efi_loaded_image_protocol(
    EfiSystemTable *st,
    EfiHandle image_handle,
    EfiLoadedImageProtocol **loaded_image)
{
    void *interface = NULL;
    EfiStatus status;

    if (!loaded_image) {
        return EFI_INVALID_PARAMETER_VALUE;
    }
    *loaded_image = NULL;

    status = efi_handle_protocol(st, image_handle,
                                 &LOADED_IMAGE_PROTOCOL_GUID, &interface);
    if (status != EFI_SUCCESS_VALUE) {
        return status;
    }
    if (interface == NULL) {
        return EFI_NOT_FOUND_VALUE;
    }

    *loaded_image = (EfiLoadedImageProtocol *)interface;
    return EFI_SUCCESS_VALUE;
}

EfiStatus efi_copy_loaded_image_options_ascii(
    EfiLoadedImageProtocol *loaded_image,
    char *buffer,
    unsigned long buffer_len,
    unsigned long *out_len)
{
    const EfiChar16 *src;
    unsigned long src_units;
    unsigned long copied = 0;

    if (!buffer || buffer_len == 0 || !out_len) {
        return EFI_INVALID_PARAMETER_VALUE;
    }

    buffer[0] = '\0';
    *out_len = 0;
    if (!loaded_image || !loaded_image->load_options ||
        loaded_image->load_options_size == 0) {
        return EFI_SUCCESS_VALUE;
    }

    src = (const EfiChar16 *)loaded_image->load_options;
    src_units = loaded_image->load_options_size / sizeof(EfiChar16);
    while (copied + 1 < buffer_len && copied < src_units) {
        EfiChar16 ch = src[copied];
        if (ch == 0) {
            break;
        }
        buffer[copied] = (ch < 0x80) ? (char)ch : '?';
        copied++;
    }
    buffer[copied] = '\0';
    *out_len = copied;

    if (copied < src_units && copied + 1 == buffer_len && src[copied] != 0) {
        return EFI_WARN_BUFFER_TOO_SMALL_VALUE;
    }

    return EFI_SUCCESS_VALUE;
}

EfiStatus efi_prepare_boot_handoff(
    EfiSystemTable *st,
    EfiHandle image_handle,
    char *cmdline_buffer,
    unsigned long cmdline_buffer_len,
    EfiSystemTable *system_table_copy,
    EfiConfigTable *config_table_copy,
    unsigned long config_table_capacity,
    EfiRuntimeServices *runtime_services_copy,
    EfiBootServices *boot_services_copy,
    EfiChar16 *firmware_vendor_copy,
    unsigned long firmware_vendor_capacity,
    EfiMemoryDescriptor *memory_map,
    unsigned long memory_map_capacity,
    EfiBootHandoff *out)
{
    EfiLoadedImageProtocol *loaded_image = NULL;
    EfiStatus status;
    unsigned long cmdline_len = 0;

    (void)memory_map;
    (void)memory_map_capacity;

    if (!efi_system_table_is_valid(st) || !out || !system_table_copy ||
        image_handle == NULL) {
        return EFI_INVALID_PARAMETER_VALUE;
    }

    out->system_table = NULL;
    out->image_handle = NULL;
    out->cmdline = NULL;
    out->cmdline_size = 0;
    out->memory_map_size = 0;
    out->map_key = 0;
    out->descriptor_size = 0;
    out->descriptor_version = 0;

    if (cmdline_buffer && cmdline_buffer_len != 0) {
        cmdline_buffer[0] = '\0';
    }

    if (efi_system_table_snapshot(st, system_table_copy, config_table_copy,
                                  config_table_capacity, runtime_services_copy,
                                  boot_services_copy, firmware_vendor_copy,
                                  firmware_vendor_capacity, NULL) != 0) {
        return EFI_OUT_OF_RESOURCES_VALUE;
    }

    // 尽力而为：缺少看门狗支持不应阻塞启动交接
    (void)efi_disable_watchdog(st);

    status = efi_loaded_image_protocol(st, image_handle, &loaded_image);
    if (status == EFI_SUCCESS_VALUE && cmdline_buffer && cmdline_buffer_len != 0) {
        status = efi_copy_loaded_image_options_ascii(
            loaded_image, cmdline_buffer, cmdline_buffer_len, &cmdline_len);
        if (efi_status_is_error(status)) {
            return status;
        }
    }

    out->system_table = system_table_copy;
    out->image_handle = image_handle;
    out->cmdline = cmdline_buffer;
    out->cmdline_size = cmdline_len;
    return EFI_SUCCESS_VALUE;
}
