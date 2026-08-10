#include "../guest/bench-platform.h"

#include <stdio.h>
#include <string.h>

static unsigned clock_calls;
static unsigned stream_calls;
static unsigned heap_calls;
static unsigned map_calls;

static uint64_t host_counter(void) {
    static uint64_t value;
    value += 10;
    return value;
}

static int host_clock_read(void) {
    ++clock_calls;
    printf("HOST clock\n");
    return 0;
}

static int host_stream_write(const void *buffer, size_t length) {
    if (length >= strlen("BENCH_") && memcmp(buffer, "BENCH_", strlen("BENCH_")) == 0) {
        (void)fwrite(buffer, 1, length, stdout);
    } else {
        ++stream_calls;
        printf("HOST stream length=%zu\n", length);
    }
    return 0;
}

static int host_heap_cycle(size_t size, unsigned count) {
    ++heap_calls;
    printf("HOST heap size=%zu count=%u\n", size, count);
    return 0;
}

static int host_map_cycle(size_t size, int touch_pages) {
    ++map_calls;
    printf("HOST map size=%zu touch=%d\n", size, touch_pages);
    return 0;
}

const struct bench_platform bench_platform = {
    .counter = host_counter,
    .clock_read = host_clock_read,
    .stream_write = host_stream_write,
    .heap_cycle = host_heap_cycle,
    .map_cycle = host_map_cycle,
};
