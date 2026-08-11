#include "bench-platform.h"

#include <mrt/mrt.h>
#if BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_SMALL || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_BATCH
#include <ranalib/stdlib.h>
#endif

static volatile uint64_t clock_sink;
#if BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_SMALL || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_BATCH
static volatile unsigned char memory_sink;
#endif

static uint64_t native_counter(void) {
    uint64_t value;
    __asm__ volatile("rdtime %0" : "=r"(value));
    return value;
}

static int native_clock_read(void) {
    uint64_t clock = mrt_initial_handle(MYGO_REQUIREMENT_monotonic_clock);
    struct mygo_native_result result =
        mrt_call(MYGO_SLOT_clock_read, clock, 0, 0, 0, 0, 0);
    if (result.status != MYGO_STATUS_ok) {
        return -1;
    }
    clock_sink = result.value0;
    return 0;
}

static int native_stream_write(const void *buffer, size_t length) {
    uint64_t stream = mrt_initial_handle(MYGO_REQUIREMENT_stdout);
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_stream_write,
        stream,
        (uintptr_t)buffer,
        length,
        0,
        0,
        0);
    return result.status == MYGO_STATUS_ok && result.value0 == length ? 0 : -1;
}

static int native_heap_cycle(size_t size, unsigned count) {
#if BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_SMALL || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_BATCH
    void *pointers[64];
    if (count > sizeof(pointers) / sizeof(pointers[0])) {
        return -1;
    }
    for (unsigned index = 0; index < count; ++index) {
        pointers[index] = malloc(size);
        if (pointers[index] == NULL) {
            while (index != 0) {
                free(pointers[--index]);
            }
            return -1;
        }
        *(volatile unsigned char *)pointers[index] = (unsigned char)index;
        memory_sink ^= *(volatile unsigned char *)pointers[index];
    }
    while (count != 0) {
        free(pointers[--count]);
    }
    return 0;
#else
    (void)size;
    (void)count;
    return 0;
#endif
}

static int native_map_cycle(size_t size, int touch_pages) {
    uint64_t address_space = mrt_initial_handle(MYGO_REQUIREMENT_current_address_space);
    if (address_space == 0 || size == 0 || size % MYGO_PAGE_SIZE != 0) {
        return -1;
    }
    struct mygo_native_result allocated = mrt_call(
        MYGO_SLOT_memory_allocate,
        address_space,
        size,
        MYGO_PAGE_SIZE,
        0,
        0,
        0);
    if (allocated.status != MYGO_STATUS_ok || allocated.value0 == 0 ||
        allocated.value1 < size || allocated.value1 % MYGO_PAGE_SIZE != 0 ||
        allocated.value0 % MYGO_PAGE_SIZE != 0) {
        return -1;
    }
    if (touch_pages != 0) {
        for (uint64_t offset = 0; offset < allocated.value1; offset += MYGO_PAGE_SIZE) {
            ((volatile unsigned char *)(uintptr_t)allocated.value0)[offset] = (unsigned char)offset;
        }
    }
    struct mygo_native_result freed = mrt_call(
        MYGO_SLOT_memory_free,
        address_space,
        allocated.value0,
        allocated.value1,
        0,
        0,
        0);
    return freed.status == MYGO_STATUS_ok ? 0 : -1;
}

const struct bench_platform bench_platform = {
    .counter = native_counter,
    .clock_read = native_clock_read,
    .stream_write = native_stream_write,
    .heap_cycle = native_heap_cycle,
    .map_cycle = native_map_cycle,
};
