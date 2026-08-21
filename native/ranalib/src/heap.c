#include <stddef.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdlib.h>
#include <ranalib/string.h>

#include <tlsf.h>

#define RANALIB_HEAP_MAGIC UINT64_C(0x52414e4148454150)
#define RANALIB_ARENA_SIZE (UINT64_C(1) << 20)
#define RANALIB_DIRECT_THRESHOLD (UINT64_C(1) << 18)

union allocation_header {
    struct {
        uint64_t magic;
        size_t requested_size;
        size_t mapping_size;
    } fields;
    max_align_t alignment;
};

static tlsf_t heap_allocator;
static unsigned char heap_lock;

static void lock_heap(void) {
    while (__atomic_test_and_set(&heap_lock, __ATOMIC_ACQUIRE)) {
    }
}

static void unlock_heap(void) {
    __atomic_clear(&heap_lock, __ATOMIC_RELEASE);
}

static int allocation_errno(uint32_t status) {
    if (status == MYGO_STATUS_memory_invalid_range ||
        status == MYGO_STATUS_memory_invalid_alignment) {
        return EINVAL;
    }
    return ENOMEM;
}

static union allocation_header *allocation_header(void *pointer) {
    return (union allocation_header *)pointer - 1;
}

static int checked_add(size_t left, size_t right, size_t *result) {
    if (left > SIZE_MAX - right) {
        return 0;
    }
    *result = left + right;
    return 1;
}

static int checked_page_size(size_t minimum, size_t *mapping_size) {
    const size_t page_size = (size_t)MYGO_PAGE_SIZE;
    if (minimum > SIZE_MAX - (page_size - 1)) {
        return 0;
    }
    *mapping_size = (minimum + page_size - 1) & ~(page_size - 1);
    return *mapping_size >= minimum;
}

static void *map_pages(size_t minimum, size_t *actual_size, uint32_t *status) {
    size_t requested = 0;
    if (!checked_page_size(minimum, &requested)) {
        *status = MYGO_STATUS_core_out_of_range;
        return NULL;
    }
    uint64_t address_space = mrt_initial_handle(MYGO_REQUIREMENT_current_address_space);
    if (address_space == 0) {
        *status = MYGO_STATUS_core_resource_exhausted;
        return NULL;
    }
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_memory_allocate,
        address_space,
        requested,
        MYGO_PAGE_SIZE,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_ok) {
        *status = result.status;
        return NULL;
    }
    if (result.value0 == 0 || result.value0 % MYGO_PAGE_SIZE != 0 ||
        result.value1 < requested || result.value1 > SIZE_MAX ||
        result.value1 % MYGO_PAGE_SIZE != 0) {
        mrt_abort();
    }
    *actual_size = (size_t)result.value1;
    *status = MYGO_STATUS_ok;
    return (void *)(uintptr_t)result.value0;
}

static void unmap_pages(void *address, size_t size) {
    uint64_t address_space = mrt_initial_handle(MYGO_REQUIREMENT_current_address_space);
    if (address_space == 0) {
        mrt_abort();
    }
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_memory_free,
        address_space,
        (uintptr_t)address,
        size,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_ok) {
        mrt_abort();
    }
}

static uint32_t grow_heap(size_t allocation_size) {
    size_t overhead = tlsf_pool_overhead();
    if (heap_allocator == NULL) {
        if (!checked_add(overhead, tlsf_size(), &overhead)) {
            return MYGO_STATUS_core_out_of_range;
        }
    }
    size_t minimum = 0;
    if (!checked_add(allocation_size, overhead, &minimum)) {
        return MYGO_STATUS_core_out_of_range;
    }
    if (minimum < RANALIB_ARENA_SIZE) {
        minimum = RANALIB_ARENA_SIZE;
    }

    size_t mapping_size = 0;
    uint32_t status = MYGO_STATUS_ok;
    void *mapping = map_pages(minimum, &mapping_size, &status);
    if (mapping == NULL) {
        return status;
    }

    if (heap_allocator == NULL) {
        heap_allocator = tlsf_create_with_pool(mapping, mapping_size);
        if (heap_allocator == NULL) {
            unmap_pages(mapping, mapping_size);
            return MYGO_STATUS_core_resource_exhausted;
        }
    } else if (tlsf_add_pool(heap_allocator, mapping, mapping_size) == NULL) {
        unmap_pages(mapping, mapping_size);
        return MYGO_STATUS_core_resource_exhausted;
    }
    return MYGO_STATUS_ok;
}

static void *allocate_direct(size_t requested_size, size_t total_size) {
    size_t mapping_size = 0;
    uint32_t status = MYGO_STATUS_ok;
    union allocation_header *header = map_pages(total_size, &mapping_size, &status);
    if (header == NULL) {
        errno = allocation_errno(status);
        return NULL;
    }
    header->fields.magic = RANALIB_HEAP_MAGIC;
    header->fields.requested_size = requested_size;
    header->fields.mapping_size = mapping_size;
    return header + 1;
}

void *malloc(size_t size) {
    size_t requested_size = size == 0 ? 1 : size;
    size_t total_size = 0;
    if (!checked_add(requested_size, sizeof(union allocation_header), &total_size)) {
        errno = ENOMEM;
        return NULL;
    }
    if (total_size >= RANALIB_DIRECT_THRESHOLD) {
        return allocate_direct(requested_size, total_size);
    }

    lock_heap();
    if (heap_allocator == NULL) {
        uint32_t status = grow_heap(total_size);
        if (status != MYGO_STATUS_ok) {
            unlock_heap();
            errno = allocation_errno(status);
            return NULL;
        }
    }
    union allocation_header *header = tlsf_memalign(
        heap_allocator, _Alignof(max_align_t), total_size);
    if (header == NULL) {
        uint32_t status = grow_heap(total_size);
        if (status == MYGO_STATUS_ok) {
            header = tlsf_memalign(
                heap_allocator, _Alignof(max_align_t), total_size);
        }
        if (header == NULL) {
            unlock_heap();
            errno = allocation_errno(status);
            return NULL;
        }
    }
    header->fields.magic = RANALIB_HEAP_MAGIC;
    header->fields.requested_size = requested_size;
    header->fields.mapping_size = 0;
    unlock_heap();
    return header + 1;
}

void *calloc(size_t count, size_t size) {
    if (count != 0 && size > SIZE_MAX / count) {
        errno = ENOMEM;
        return NULL;
    }
    size_t total = count * size;
    void *pointer = malloc(total);
    if (pointer != NULL) {
        memset(pointer, 0, total);
    }
    return pointer;
}

void free(void *pointer) {
    if (pointer == NULL) {
        return;
    }
    union allocation_header *header = allocation_header(pointer);
    if (header->fields.magic != RANALIB_HEAP_MAGIC) {
        mrt_abort();
    }
    size_t mapping_size = header->fields.mapping_size;
    header->fields.magic = 0;
    if (mapping_size != 0) {
        unmap_pages(header, mapping_size);
        return;
    }
    lock_heap();
    tlsf_free(heap_allocator, header);
    unlock_heap();
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

    void *replacement = malloc(size);
    if (replacement == NULL) {
        return NULL;
    }
    memcpy(replacement, pointer, old_size);
    free(pointer);
    return replacement;
}

#if defined(RANALIB_HEAP_TEST)
void ranalib_heap_reset_for_test(void) {
    heap_allocator = NULL;
    __atomic_clear(&heap_lock, __ATOMIC_RELEASE);
}
#endif
