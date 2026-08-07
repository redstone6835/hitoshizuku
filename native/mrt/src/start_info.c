#include <mrt/mrt.h>

_Static_assert(MYGO_CAPABILITY_COUNT == 2u, "mrt bootstrap capability set");
_Static_assert(MYGO_CAP_SELF_PROCESS_REQUIRED == 1u, "self process is required");
_Static_assert(MYGO_CAP_STDOUT_REQUIRED == 1u, "stdout is required");

static void clear_view(struct mrt_start_view *view) {
    if (view != 0) {
        view->info = 0;
        view->self_process = 0;
        view->stdout_stream = 0;
    }
}

static int bytes_are_zero(const void *address, uint64_t size) {
    const unsigned char *bytes = (const unsigned char *)address;
    for (uint64_t index = 0; index < size; ++index) {
        if (bytes[index] != 0) {
            return 0;
        }
    }
    return 1;
}

static int has_nonzero_byte(const void *address, uint64_t size) {
    return !bytes_are_zero(address, size);
}

static int valid_total_size(const struct mygo_start_info *info, uint64_t entry_size) {
    return entry_size == info->total_size && entry_size >= MYGO_START_INFO_SIZE &&
           entry_size <= MYGO_START_INFO_MAX_SIZE && (entry_size & 7u) == 0;
}

static int checked_array_end(
    uint64_t offset,
    uint64_t count,
    uint64_t record_size,
    uint64_t total_size,
    uint64_t *end) {
    if ((offset & 7u) != 0 || count > (UINT64_MAX - offset) / record_size) {
        return 0;
    }
    uint64_t result = offset + count * record_size;
    if (result > total_size) {
        return 0;
    }
    *end = result;
    return 1;
}

static int validate_layout(const struct mygo_start_info *info) {
    uint64_t cursor = MYGO_START_INFO_SIZE;
    uint64_t end = 0;

    if (info->argc == 0) {
        if (info->argv_offset != 0) {
            return 0;
        }
    } else {
        if (info->argv_offset != cursor ||
            !checked_array_end(cursor, info->argc, MYGO_STRING_REF_SIZE, info->total_size, &end)) {
            return 0;
        }
        cursor = end;
    }

    if (info->envc == 0) {
        if (info->env_offset != 0) {
            return 0;
        }
    } else {
        if (info->env_offset != cursor ||
            !checked_array_end(cursor, info->envc, MYGO_STRING_REF_SIZE, info->total_size, &end)) {
            return 0;
        }
        cursor = end;
    }

    if (info->initial_handle_count == 0) {
        if (info->initial_handle_offset != 0) {
            return 0;
        }
    } else {
        if (info->initial_handle_record_size != MYGO_INITIAL_HANDLE_SIZE ||
            info->initial_handle_offset != cursor ||
            !checked_array_end(
                cursor,
                info->initial_handle_count,
                MYGO_INITIAL_HANDLE_SIZE,
                info->total_size,
                &end)) {
            return 0;
        }
        cursor = end;
    }

    if (info->string_bytes_size == 0) {
        if (info->string_bytes_offset != 0) {
            return 0;
        }
    } else {
        if (info->string_bytes_offset != cursor || info->string_bytes_size > UINT64_MAX - cursor) {
            return 0;
        }
        cursor += info->string_bytes_size;
        if (cursor > info->total_size) {
            return 0;
        }
    }

    uint64_t aligned_end = (cursor + 7u) & ~UINT64_C(7);
    return aligned_end >= cursor && aligned_end == info->total_size;
}

static int validate_string_array(
    const unsigned char *base,
    uint32_t array_offset,
    uint32_t count,
    uint64_t string_end,
    uint64_t *expected_string_offset) {
    const struct mygo_string_ref *refs =
        (const struct mygo_string_ref *)(base + array_offset);
    for (uint32_t index = 0; index < count; ++index) {
        uint64_t offset = refs[index].offset;
        uint64_t length = refs[index].length;
        if (offset != *expected_string_offset || length >= string_end - offset) {
            return 0;
        }
        for (uint64_t cursor = 0; cursor < length; ++cursor) {
            if (base[offset + cursor] == 0) {
                return 0;
            }
        }
        if (base[offset + length] != 0) {
            return 0;
        }
        *expected_string_offset = offset + length + 1;
    }
    return 1;
}

static int validate_strings(const struct mygo_start_info *info) {
    if (info->argc == 0 && info->envc == 0) {
        return info->string_bytes_size == 0;
    }
    if (info->string_bytes_size == 0) {
        return 0;
    }
    const unsigned char *base = (const unsigned char *)info;
    uint64_t string_end =
        (uint64_t)info->string_bytes_offset + (uint64_t)info->string_bytes_size;
    uint64_t expected = info->string_bytes_offset;
    if (!validate_string_array(base, info->argv_offset, info->argc, string_end, &expected) ||
        !validate_string_array(base, info->env_offset, info->envc, string_end, &expected)) {
        return 0;
    }
    return expected == string_end;
}

static int valid_handle_encoding(uint64_t handle) {
    return (uint32_t)handle != 0 && (uint32_t)(handle >> 32) != 0;
}

static int validate_handles(
    const struct mygo_start_info *info,
    uint64_t *self_process,
    uint64_t *stdout_stream) {
    if (info->initial_handle_count != MYGO_CAPABILITY_COUNT) {
        return 0;
    }
    const unsigned char *base = (const unsigned char *)info;
    const struct mygo_initial_handle *handles =
        (const struct mygo_initial_handle *)(base + info->initial_handle_offset);
    uint32_t previous_requirement = 0;

    for (uint32_t index = 0; index < info->initial_handle_count; ++index) {
        const struct mygo_initial_handle *handle = &handles[index];
        if (handle->requirement_id <= previous_requirement || handle->flags != 0 ||
            handle->reserved != 0 || !valid_handle_encoding(handle->handle)) {
            return 0;
        }
        previous_requirement = handle->requirement_id;
        if (handle->requirement_id == MYGO_REQUIREMENT_SELF_PROCESS) {
            if (handle->object_interface != MYGO_INTERFACE_PROCESS ||
                handle->granted_rights != MYGO_CAP_SELF_PROCESS_RIGHTS) {
                return 0;
            }
            *self_process = handle->handle;
        } else if (handle->requirement_id == MYGO_REQUIREMENT_STDOUT) {
            if (handle->object_interface != MYGO_INTERFACE_STREAM ||
                handle->granted_rights != MYGO_CAP_STDOUT_RIGHTS) {
                return 0;
            }
            *stdout_stream = handle->handle;
        } else {
            return 0;
        }
    }
    return *self_process != 0 && *stdout_stream != 0;
}

enum mrt_start_error mrt_validate_start_info(
    const struct mygo_start_info *info,
    uint64_t entry_size,
    uint64_t entry_image_base,
    uint64_t entry_thread_pointer,
    struct mrt_start_view *out) {
    clear_view(out);
    if (info == 0 || out == 0 || info->magic[0] != 's' || info->magic[1] != 'y' ||
        info->magic[2] != 's' || info->magic[3] != 't' || info->version != 1 ||
        info->header_size != MYGO_START_INFO_SIZE) {
        return MRT_START_BAD_HEADER;
    }
    if (!valid_total_size(info, entry_size) || !validate_layout(info)) {
        return MRT_START_BAD_RANGE;
    }
    if (info->flags != 0 || info->reserved0 != 0 || info->reserved1 != 0 ||
        !bytes_are_zero(info->reserved2, sizeof(info->reserved2))) {
        return MRT_START_BAD_RESERVED;
    }
    if (info->abi_epoch != MYGO_ABI_EPOCH || info->target_arch != MYGO_TARGET_ARCH ||
        (info->enabled_features & ~MYGO_FEATURE_STATIC_TLS) != 0 ||
        info->image_base != entry_image_base ||
        info->image_base == 0 || info->image_base % MYGO_PAGE_SIZE != 0 ||
        info->page_size != MYGO_PAGE_SIZE || info->call_slot_count != MYGO_CALL_SLOT_COUNT ||
        info->runtime_flags != 0) {
        return MRT_START_BAD_CONTRACT;
    }
    if (info->initial_tls_size == 0) {
        if (info->enabled_features != 0 || info->initial_tls_base != 0 ||
            info->initial_thread_pointer != 0 || entry_thread_pointer != 0) {
            return MRT_START_BAD_TLS;
        }
    } else if (info->enabled_features != MYGO_FEATURE_STATIC_TLS ||
               info->initial_tls_base == 0 ||
               info->initial_thread_pointer != info->initial_tls_base ||
               entry_thread_pointer != info->initial_thread_pointer ||
               info->initial_tls_base > UINT64_MAX - info->initial_tls_size) {
        return MRT_START_BAD_TLS;
    }
    if (!has_nonzero_byte(info->random_seed, sizeof(info->random_seed))) {
        return MRT_START_BAD_RANDOM;
    }
    if (!validate_strings(info)) {
        return MRT_START_BAD_STRINGS;
    }

    uint64_t self_process = 0;
    uint64_t stdout_stream = 0;
    if (!validate_handles(info, &self_process, &stdout_stream)) {
        return MRT_START_BAD_HANDLES;
    }
    out->info = info;
    out->self_process = self_process;
    out->stdout_stream = stdout_stream;
    return MRT_START_OK;
}
