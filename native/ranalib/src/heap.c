#include <stddef.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdlib.h>

#define RANALIB_HEAP_MAGIC UINT64_C(0x52414e4148454150)

union allocation_header {
    struct {
        uint64_t magic;
        size_t mapping_size;
        size_t requested_size;
    } fields;
    max_align_t alignment;
};

static int allocation_errno(uint32_t status) {
    if (status == MYGO_STATUS_memory_invalid_range) {
        return EINVAL;
    }
    return ENOMEM;
}

static union allocation_header *allocation_header(void *pointer) {
    return (union allocation_header *)pointer - 1;
}

static int checked_mapping_size(size_t requested, size_t *mapping_size) {
    const size_t page_size = (size_t)MYGO_PAGE_SIZE;
    if (requested == 0) {
        requested = 1;
    }
    if (requested > SIZE_MAX - sizeof(union allocation_header)) {
        return 0;
    }
    size_t total = requested + sizeof(union allocation_header);
    if (total > SIZE_MAX - (page_size - 1)) {
        return 0;
    }
    *mapping_size = (total + page_size - 1) & ~(page_size - 1);
    return *mapping_size >= total;
}

void *malloc(size_t size) {
    size_t mapping_size = 0;
    if (!checked_mapping_size(size, &mapping_size)) {
        errno = ENOMEM;
        return NULL;
    }
    uint64_t address_space = mrt_initial_handle(MYGO_REQUIREMENT_current_address_space);
    if (address_space == 0) {
        errno = ENOMEM;
        return NULL;
    }
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_memory_allocate,
        address_space,
        mapping_size,
        MYGO_PAGE_SIZE,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_ok) {
        errno = allocation_errno(result.status);
        return NULL;
    }
    if (result.value0 == 0 || result.value0 % MYGO_PAGE_SIZE != 0) {
        mrt_abort();
    }

    if (result.value1 == 0 || result.value1 % MYGO_PAGE_SIZE != 0 || result.value1 < mapping_size) {
        mrt_abort();
    }
    union allocation_header *header = (union allocation_header *)(uintptr_t)result.value0;
    header->fields.magic = RANALIB_HEAP_MAGIC;
    header->fields.mapping_size = (size_t)result.value1;
    header->fields.requested_size = size == 0 ? 1 : size;
    return header + 1;
}

void *calloc(size_t count, size_t size) {
    if (count != 0 && size > SIZE_MAX / count) {
        errno = ENOMEM;
        return NULL;
    }
    return malloc(count * size);
}

void free(void *pointer) {
    if (pointer == NULL) {
        return;
    }
    union allocation_header *header = allocation_header(pointer);
    if (header->fields.magic != RANALIB_HEAP_MAGIC ||
        header->fields.mapping_size == 0 ||
        header->fields.mapping_size % MYGO_PAGE_SIZE != 0) {
        mrt_abort();
    }
    uint64_t address_space = mrt_initial_handle(MYGO_REQUIREMENT_current_address_space);
    if (address_space == 0) {
        mrt_abort();
    }
    size_t mapping_size = header->fields.mapping_size;
    header->fields.magic = 0;
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_memory_free,
        address_space,
        (uintptr_t)header,
        mapping_size,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_ok) {
        mrt_abort();
    }
}

void *realloc(void *pointer, size_t size) {
    if (pointer == NULL) {
        return malloc(size);
    }
    if (size == 0) {
        free(pointer);
        return NULL;
    }
    union allocation_header *header = allocation_header(pointer);
    if (header->fields.magic != RANALIB_HEAP_MAGIC) {
        mrt_abort();
    }
    size_t old_size = header->fields.requested_size;
    if (size <= old_size) {
        header->fields.requested_size = size;
        return pointer;
    }

    unsigned char *replacement = malloc(size);
    if (replacement == NULL) {
        return NULL;
    }
    const unsigned char *source = pointer;
    for (size_t index = 0; index < old_size; ++index) {
        replacement[index] = source[index];
    }
    free(pointer);
    return replacement;
}
