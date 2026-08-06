#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#if !defined(__riscv) || __riscv_xlen != 64
#error "mm_fault_bench only supports RISC-V64"
#endif

enum fault_case {
    FAULT_ANON_READ,
    FAULT_ANON_WRITE,
    FAULT_RESIDENT_WRITE,
};

struct shared_state {
    atomic_uint ready;
    atomic_uint start;
    atomic_uint done;
    atomic_uint finish;
    atomic_uint errors;
};

struct worker {
    struct shared_state *shared;
    volatile unsigned char *mapping;
    size_t first_page;
    size_t pages;
    size_t page_size;
    unsigned int id;
    enum fault_case kind;
    uint64_t checksum;
};

/* QEMU 插件以两个固定 PC 严格门控动态指令窗口。 */
__attribute__((noinline, used, externally_visible)) void mm_profile_start(void)
{
    __asm__ volatile("" : : : "memory");
}

__attribute__((noinline, used, externally_visible)) void mm_profile_stop(void)
{
    __asm__ volatile("" : : : "memory");
}

static void spin_until_nonzero(const atomic_uint *value)
{
    while (atomic_load_explicit(value, memory_order_acquire) == 0) {
        __asm__ volatile("nop" : : : "memory");
    }
}

static uint64_t touch_pages(struct worker *worker)
{
    uint64_t checksum = 0;
    size_t end = worker->first_page + worker->pages;

    for (size_t page = worker->first_page; page < end; ++page) {
        volatile unsigned char *byte = worker->mapping + page * worker->page_size;
        if (worker->kind == FAULT_ANON_READ) {
            checksum += *byte;
        } else {
            unsigned char value = (unsigned char)((page + worker->id + 1U) | 1U);
            *byte = value;
            checksum += value;
        }
    }
    return checksum;
}

static void *worker_main(void *opaque)
{
    struct worker *worker = opaque;
    atomic_fetch_add_explicit(&worker->shared->ready, 1, memory_order_release);
    spin_until_nonzero(&worker->shared->start);
    worker->checksum = touch_pages(worker);
    atomic_fetch_add_explicit(&worker->shared->done, 1, memory_order_release);
    spin_until_nonzero(&worker->shared->finish);
    return NULL;
}

static uint64_t timespec_ns(const struct timespec *value)
{
    return (uint64_t)value->tv_sec * UINT64_C(1000000000) + (uint64_t)value->tv_nsec;
}

static int parse_count(const char *text, uint64_t minimum, uint64_t maximum,
                       uint64_t *result)
{
    char *end = NULL;
    errno = 0;
    unsigned long long value = strtoull(text, &end, 10);
    if (errno != 0 || !text[0] || !end || *end || value < minimum || value > maximum) {
        return -1;
    }
    *result = (uint64_t)value;
    return 0;
}

static const char *case_name(enum fault_case kind)
{
    switch (kind) {
    case FAULT_ANON_READ:
        return "anon-read";
    case FAULT_ANON_WRITE:
        return "anon-write";
    case FAULT_RESIDENT_WRITE:
        return "resident-write";
    }
    return "invalid";
}

static int parse_case(const char *name, enum fault_case *kind)
{
    if (strcmp(name, "anon-read") == 0) {
        *kind = FAULT_ANON_READ;
    } else if (strcmp(name, "anon-write") == 0) {
        *kind = FAULT_ANON_WRITE;
    } else if (strcmp(name, "resident-write") == 0) {
        *kind = FAULT_RESIDENT_WRITE;
    } else {
        return -1;
    }
    return 0;
}

static int validate_mapping(const volatile unsigned char *mapping, size_t pages,
                            size_t page_size, enum fault_case kind,
                            uint64_t expected_checksum)
{
    uint64_t checksum = 0;
    for (size_t page = 0; page < pages; ++page) {
        const volatile unsigned char *base = mapping + page * page_size;
        unsigned char value = base[0];
        if (kind == FAULT_ANON_READ) {
            if (value != 0) {
                return -1;
            }
        } else if (value == 0) {
            return -1;
        }
        checksum += value;
        for (size_t offset = 1; offset < page_size; ++offset) {
            if (base[offset] != 0) {
                return -1;
            }
        }
    }
    return checksum == expected_checksum ? 0 : -1;
}

static int run_round(enum fault_case kind, size_t pages, unsigned int threads,
                     size_t page_size, unsigned int round)
{
    if (pages > SIZE_MAX / page_size) {
        return -1;
    }
    size_t bytes = pages * page_size;
    volatile unsigned char *mapping = mmap(NULL, bytes, PROT_READ | PROT_WRITE,
                                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        perror("mmap");
        return -1;
    }

    if (kind == FAULT_RESIDENT_WRITE) {
        for (size_t page = 0; page < pages; ++page) {
            mapping[page * page_size] = 1;
        }
    }

    struct shared_state shared = {
        .ready = ATOMIC_VAR_INIT(0),
        .start = ATOMIC_VAR_INIT(0),
        .done = ATOMIC_VAR_INIT(0),
        .finish = ATOMIC_VAR_INIT(0),
        .errors = ATOMIC_VAR_INIT(0),
    };
    struct worker *workers = calloc(threads, sizeof(*workers));
    pthread_t *thread_ids = threads > 1 ? calloc(threads, sizeof(*thread_ids)) : NULL;
    if (!workers || (threads > 1 && !thread_ids)) {
        free(thread_ids);
        free(workers);
        munmap((void *)mapping, bytes);
        return -1;
    }

    size_t assigned = 0;
    for (unsigned int index = 0; index < threads; ++index) {
        size_t count = pages / threads + (index < pages % threads ? 1U : 0U);
        workers[index] = (struct worker){
            .shared = &shared,
            .mapping = mapping,
            .first_page = assigned,
            .pages = count,
            .page_size = page_size,
            .id = index,
            .kind = kind,
            .checksum = 0,
        };
        assigned += count;
    }

    if (threads > 1) {
        for (unsigned int index = 0; index < threads; ++index) {
            int error = pthread_create(&thread_ids[index], NULL, worker_main, &workers[index]);
            if (error != 0) {
                fprintf(stderr, "pthread_create: %s\n", strerror(error));
                atomic_store_explicit(&shared.errors, 1, memory_order_release);
                atomic_store_explicit(&shared.start, 1, memory_order_release);
                atomic_store_explicit(&shared.finish, 1, memory_order_release);
                for (unsigned int joined = 0; joined < index; ++joined) {
                    pthread_join(thread_ids[joined], NULL);
                }
                free(thread_ids);
                free(workers);
                munmap((void *)mapping, bytes);
                return -1;
            }
        }
        while (atomic_load_explicit(&shared.ready, memory_order_acquire) != threads) {
            __asm__ volatile("nop" : : : "memory");
        }
    }

    struct timespec start;
    struct timespec stop;
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return -1;
    }
    mm_profile_start();
    if (threads == 1) {
        workers[0].checksum = touch_pages(&workers[0]);
    } else {
        atomic_store_explicit(&shared.start, 1, memory_order_release);
        while (atomic_load_explicit(&shared.done, memory_order_acquire) != threads) {
            __asm__ volatile("nop" : : : "memory");
        }
    }
    mm_profile_stop();
    if (clock_gettime(CLOCK_MONOTONIC, &stop) != 0) {
        return -1;
    }

    atomic_store_explicit(&shared.finish, 1, memory_order_release);
    if (threads > 1) {
        for (unsigned int index = 0; index < threads; ++index) {
            int error = pthread_join(thread_ids[index], NULL);
            if (error != 0) {
                fprintf(stderr, "pthread_join: %s\n", strerror(error));
                atomic_store_explicit(&shared.errors, 1, memory_order_release);
            }
        }
    }

    uint64_t checksum = 0;
    for (unsigned int index = 0; index < threads; ++index) {
        checksum += workers[index].checksum;
    }
    if (validate_mapping(mapping, pages, page_size, kind, checksum) != 0) {
        atomic_store_explicit(&shared.errors, 1, memory_order_release);
    }
    uint64_t elapsed = timespec_ns(&stop) - timespec_ns(&start);
    printf("MM_FAULT_RESULT round=%u pages=%zu threads=%u elapsed_ns=%" PRIu64
           " avg_ns=%" PRIu64 ".%03" PRIu64 " checksum=%" PRIu64
           " errors=%u\n",
           round, pages, threads, elapsed, elapsed / pages,
           (elapsed % pages) * UINT64_C(1000) / pages, checksum,
           atomic_load_explicit(&shared.errors, memory_order_acquire));

    int failed = atomic_load_explicit(&shared.errors, memory_order_acquire) != 0;
    free(thread_ids);
    free(workers);
    if (munmap((void *)mapping, bytes) != 0) {
        perror("munmap");
        failed = 1;
    }
    return failed ? -1 : 0;
}

int main(int argc, char **argv)
{
    enum fault_case kind = FAULT_ANON_WRITE;
    uint64_t pages_value = 1;
    uint64_t threads_value = 1;
    uint64_t repeats_value = 1;

    if (argc > 1 && parse_case(argv[1], &kind) != 0) {
        goto usage;
    }
    if (argc > 2 && parse_count(argv[2], 1, UINT64_C(1048576), &pages_value) != 0) {
        goto usage;
    }
    if (argc > 3 && parse_count(argv[3], 1, 256, &threads_value) != 0) {
        goto usage;
    }
    if (argc > 4 && parse_count(argv[4], 1, 31, &repeats_value) != 0) {
        goto usage;
    }
    if (argc > 5 || threads_value > pages_value) {
        goto usage;
    }

    long page_size_value = sysconf(_SC_PAGESIZE);
    if (page_size_value <= 0 || ((size_t)page_size_value & ((size_t)page_size_value - 1U))) {
        fprintf(stderr, "MM_FAULT_ERROR invalid_page_size=%ld\n", page_size_value);
        return 1;
    }
    size_t page_size = (size_t)page_size_value;
    size_t pages = (size_t)pages_value;
    unsigned int threads = (unsigned int)threads_value;
    unsigned int repeats = (unsigned int)repeats_value;

    printf("MM_FAULT_BENCH version=1 arch=riscv64 case=%s pages=%zu threads=%u"
           " repeats=%u page_size=%zu\n",
           case_name(kind), pages, threads, repeats, page_size);
    unsigned int failures = 0;
    for (unsigned int round = 1; round <= repeats; ++round) {
        if (run_round(kind, pages, threads, page_size, round) != 0) {
            ++failures;
        }
    }
    printf("MM_FAULT_BENCH_DONE status=%u rounds=%u\n", failures ? 1U : 0U,
           repeats);
    return failures ? 1 : 0;

usage:
    fprintf(stderr,
            "usage: %s [anon-read|anon-write|resident-write [pages [threads [repeats]]]]\n",
            argv[0]);
    return 2;
}
