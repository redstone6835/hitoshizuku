#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdlib.h>

enum {
    TEST_PAGE_COUNT = 4,
};

struct captured_call {
    uint64_t slot;
    uint64_t handle;
    uint64_t args[5];
};

static _Alignas(4096) unsigned char pages[TEST_PAGE_COUNT][4096];
static struct captured_call calls[8];
static unsigned int call_count;
static unsigned int next_page;
static uint32_t map_status;

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    assert(requirement_id == MYGO_REQUIREMENT_current_address_space);
    return UINT64_C(0x0000000100000004);
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    assert(call_count < sizeof(calls) / sizeof(calls[0]));
    calls[call_count].slot = slot;
    calls[call_count].handle = object_handle;
    calls[call_count].args[0] = arg0;
    calls[call_count].args[1] = arg1;
    calls[call_count].args[2] = arg2;
    calls[call_count].args[3] = arg3;
    calls[call_count].args[4] = arg4;
    ++call_count;

    struct mygo_native_result result = {0};
    if (slot == MYGO_SLOT_memory_allocate) {
        result.status = map_status;
        if (result.status == MYGO_STATUS_ok) {
            assert(next_page < TEST_PAGE_COUNT);
            result.value0 = (uintptr_t)pages[next_page++];
            result.value1 = MYGO_PAGE_SIZE;
        }
    } else {
        assert(slot == MYGO_SLOT_memory_free);
        result.status = MYGO_STATUS_ok;
    }
    return result;
}

_Noreturn void mrt_abort(void) {
    assert(!"heap invariant failure");
    __builtin_trap();
}

static void reset(void) {
    memset(pages, 0, sizeof(pages));
    memset(calls, 0, sizeof(calls));
    call_count = 0;
    next_page = 0;
    map_status = MYGO_STATUS_ok;
    errno = 0;
}

static void malloc_maps_zeroed_read_write_pages(void) {
    reset();
    unsigned char *pointer = malloc(32);

    assert(pointer != NULL);
    assert((uintptr_t)pointer % _Alignof(max_align_t) == 0);
    for (unsigned int index = 0; index < 32; ++index) {
        assert(pointer[index] == 0);
    }
    assert(call_count == 1);
    assert(calls[0].slot == MYGO_SLOT_memory_allocate);
    assert(calls[0].handle == UINT64_C(0x0000000100000004));
    assert(calls[0].args[0] == MYGO_PAGE_SIZE);
    assert(calls[0].args[1] == MYGO_PAGE_SIZE);
    assert(calls[0].args[2] == 0 && calls[0].args[3] == 0 && calls[0].args[4] == 0);

    free(pointer);
    assert(call_count == 2);
    assert(calls[1].slot == MYGO_SLOT_memory_free);
    assert(calls[1].args[0] == (uintptr_t)pages[0]);
    assert(calls[1].args[1] == MYGO_PAGE_SIZE);
}

static void calloc_rejects_overflow_without_mapping(void) {
    reset();
    assert(calloc(SIZE_MAX, 2) == NULL);
    assert(errno == ENOMEM);
    assert(call_count == 0);
}

static void realloc_preserves_bytes_and_releases_the_old_mapping(void) {
    reset();
    unsigned char *old = malloc(8);
    assert(old != NULL);
    for (unsigned int index = 0; index < 8; ++index) {
        old[index] = (unsigned char)(index + 1);
    }

    unsigned char *grown = realloc(old, 64);
    assert(grown != NULL);
    for (unsigned int index = 0; index < 8; ++index) {
        assert(grown[index] == (unsigned char)(index + 1));
    }
    assert(call_count == 3);
    assert(calls[2].slot == MYGO_SLOT_memory_free);
    assert(calls[2].args[0] == (uintptr_t)pages[0]);
    free(grown);
}

static void failed_mapping_reports_enomem(void) {
    reset();
    map_status = MYGO_STATUS_core_resource_exhausted;
    assert(malloc(1) == NULL);
    assert(errno == ENOMEM);
    assert(call_count == 1);
}

int main(void) {
    malloc_maps_zeroed_read_write_pages();
    calloc_rejects_overflow_without_mapping();
    realloc_preserves_bytes_and_releases_the_old_mapping();
    failed_mapping_reports_enomem();
    return 0;
}
