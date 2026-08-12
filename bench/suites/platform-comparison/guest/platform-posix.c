#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include "bench-platform.h"

#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static volatile uint64_t clock_sink;
static volatile unsigned char memory_sink;

static uint64_t posix_counter(void) {
    uint64_t value;
    __asm__ volatile("rdtime %0" : "=r"(value));
    return value;
}

static int posix_clock_read(void) {
    struct timespec value;
    long status = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &value);
    if (status != 0) {
        return -1;
    }
    clock_sink = (uint64_t)value.tv_sec ^ (uint64_t)value.tv_nsec;
    return 0;
}

static int posix_stream_write(const void *buffer, size_t length) {
    return syscall(SYS_write, STDOUT_FILENO, buffer, length) == (long)length ? 0 : -1;
}

static int posix_heap_cycle(size_t size, unsigned count) {
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
}

static int posix_map_cycle(size_t size, int touch_pages) {
    void *mapping = (void *)syscall(
        SYS_mmap,
        0,
        size,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0);
    if (mapping == MAP_FAILED) {
        return -1;
    }
    if (touch_pages != 0) {
        for (size_t offset = 0; offset < size; offset += 4096) {
            ((volatile unsigned char *)mapping)[offset] = (unsigned char)offset;
        }
    }
    return syscall(SYS_munmap, mapping, size) == 0 ? 0 : -1;
}

const struct bench_platform bench_platform = {
    .counter = posix_counter,
    .clock_read = posix_clock_read,
    .stream_write = posix_stream_write,
    .heap_cycle = posix_heap_cycle,
    .map_cycle = posix_map_cycle,
};
