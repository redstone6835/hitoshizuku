#ifndef BENCH_PLATFORM_H
#define BENCH_PLATFORM_H

#include <stddef.h>
#include <stdint.h>

#define BENCH_WORKLOAD_CLOCK_READ 1
#define BENCH_WORKLOAD_STREAM_WRITE 2
#define BENCH_WORKLOAD_STREAM_WRITE_1 3
#define BENCH_WORKLOAD_STREAM_WRITE_64 4
#define BENCH_WORKLOAD_STREAM_WRITE_256 5
#define BENCH_WORKLOAD_HEAP_SMALL 6
#define BENCH_WORKLOAD_HEAP_BATCH 7
#define BENCH_WORKLOAD_MAP_LARGE 8
#define BENCH_WORKLOAD_PAGE_TOUCH 9

#define BENCH_MODE_WARM 1
#define BENCH_MODE_COLD 2

#ifndef BENCH_WORKLOAD
#define BENCH_WORKLOAD BENCH_WORKLOAD_CLOCK_READ
#endif

#ifndef BENCH_MODE
#define BENCH_MODE BENCH_MODE_WARM
#endif

#ifndef BENCH_SAMPLES
#define BENCH_SAMPLES 1000
#endif

#ifndef BENCH_ROUNDS
#define BENCH_ROUNDS 5
#endif

#ifndef BENCH_WARMUP
#define BENCH_WARMUP 1000
#endif

#ifndef BENCH_COUNTER_HZ
#define BENCH_COUNTER_HZ UINT64_C(10000000)
#endif

struct bench_platform {
    uint64_t (*counter)(void);
    int (*clock_read)(void);
    int (*stream_write)(const void *buffer, size_t length);
    int (*heap_cycle)(size_t size, unsigned count);
    int (*map_cycle)(size_t size, int touch_pages);
};

extern const struct bench_platform bench_platform;

int bench_run(
    const struct bench_platform *platform,
    const char *system_id,
    const char *workload_id,
    unsigned boot,
    unsigned rounds,
    unsigned samples,
    unsigned warmup);

#endif
