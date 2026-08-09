#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/stdlib.h>

enum {
    TEST_ARENA_SIZE = 1024 * 1024,
    TEST_ARENA_COUNT = 4,
};

struct captured_call {
    uint64_t slot;
    uint64_t handle;
    uint64_t args[5];
};

static _Alignas(4096) unsigned char arenas[TEST_ARENA_COUNT][TEST_ARENA_SIZE];
static struct captured_call calls[16];
static unsigned int call_count;
static unsigned int next_arena;
static uint32_t map_status;

void ranalib_heap_reset_for_test(void);

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
            assert(next_arena < TEST_ARENA_COUNT);
            assert(arg0 <= TEST_ARENA_SIZE);
            result.value0 = (uintptr_t)arenas[next_arena++];
            result.value1 = arg0;
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
    ranalib_heap_reset_for_test();
    memset(arenas, 0, sizeof(arenas));
    memset(calls, 0, sizeof(calls));
    call_count = 0;
    next_arena = 0;
    map_status = MYGO_STATUS_ok;
    errno = 0;
}

static void small_allocations_share_one_arena(void) {
    reset();
    unsigned char *first = malloc(32);
    unsigned char *second = malloc(48);

    assert(first != NULL && second != NULL && first != second);
    assert((uintptr_t)first % _Alignof(max_align_t) == 0);
    assert((uintptr_t)second % _Alignof(max_align_t) == 0);
    assert(call_count == 1);
    assert(calls[0].slot == MYGO_SLOT_memory_allocate);
    assert(calls[0].handle == UINT64_C(0x0000000100000004));
    assert(calls[0].args[0] == TEST_ARENA_SIZE);
    assert(calls[0].args[1] == MYGO_PAGE_SIZE);

    free(first);
    free(second);
    assert(call_count == 1);
}

static void calloc_clears_reused_storage(void) {
    reset();
    unsigned char *first = malloc(64);
    assert(first != NULL);
    memset(first, 0xa5, 64);
    free(first);

    unsigned char *cleared = calloc(16, 4);
    assert(cleared != NULL);
    for (unsigned int index = 0; index < 64; ++index) {
        assert(cleared[index] == 0);
    }
    assert(call_count == 1);
    free(cleared);
}

static void calloc_rejects_overflow_without_mapping(void) {
    reset();
    assert(calloc(SIZE_MAX, 2) == NULL);
    assert(errno == ENOMEM);
    assert(call_count == 0);
}

static void realloc_preserves_bytes_inside_the_arena(void) {
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
    assert(call_count == 1);
    free(grown);
}

static void large_allocations_are_returned_to_native_memory(void) {
    reset();
    void *large = malloc(300 * 1024);
    assert(large != NULL);
    assert(call_count == 1);
    assert(calls[0].slot == MYGO_SLOT_memory_allocate);
    assert(calls[0].args[0] > 300 * 1024);

    free(large);
    assert(call_count == 2);
    assert(calls[1].slot == MYGO_SLOT_memory_free);
    assert(calls[1].args[0] == (uintptr_t)arenas[0]);
    assert(calls[1].args[1] == calls[0].args[0]);
}

static void failed_mapping_reports_enomem(void) {
    reset();
    map_status = MYGO_STATUS_core_resource_exhausted;
    assert(malloc(1) == NULL);
    assert(errno == ENOMEM);
    assert(call_count == 1);
}

int main(void) {
    small_allocations_share_one_arena();
    calloc_clears_reused_storage();
    calloc_rejects_overflow_without_mapping();
    realloc_preserves_bytes_inside_the_arena();
    large_allocations_are_returned_to_native_memory();
    failed_mapping_reports_enomem();
    return 0;
}
