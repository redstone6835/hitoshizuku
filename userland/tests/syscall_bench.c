#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#if !defined(__riscv) || __riscv_xlen != 64
#error "syscall_bench only supports RISC-V64"
#endif

enum {
    SYS_READ = 63,
    SYS_WRITE = 64,
    SYS_OPENAT = 56,
    SYS_CLOSE = 57,
    SYS_FUTEX = 98,
    SYS_CLOCK_GETTIME = 113,
    SYS_SCHED_YIELD = 124,
    SYS_GETTIMEOFDAY = 169,
    SYS_GETPID = 172,
    SYS_GETPPID = 173,
    SYS_GETUID = 174,
    SYS_GETTID = 178,
    CLOCK_MONOTONIC_RAW_ID = 4,
    FUTEX_WAKE_PRIVATE = 129,
    AT_FDCWD = -100,
    O_RDONLY = 0,
    O_WRONLY = 1,
    RW_BUFFER_SIZE = 4096,
};

struct timeval64 {
    int64_t tv_sec;
    int64_t tv_usec;
};

typedef long (*bench_op_t)(void *context);

struct bench_case {
    const char *name;
    long syscall_nr;
    bench_op_t op;
    void *context;
};

struct rw_context {
    long fd;
    unsigned char *buffer;
    size_t length;
};

struct result {
    uint64_t total_ns;
    uint64_t empty_ns;
    uint64_t net_ns;
    uint64_t errors;
    uint64_t checksum;
};

/* QEMU plugin 以这两个固定地址严格门控动态指令采样窗口。 */
__attribute__((noinline, used, externally_visible)) void syscall_profile_start(void)
{
    __asm__ volatile("" : : : "memory");
}

__attribute__((noinline, used, externally_visible)) void syscall_profile_stop(void)
{
    __asm__ volatile("" : : : "memory");
}

static inline long raw_syscall0(long nr)
{
    register long a0 __asm__("a0");
    register long a7 __asm__("a7") = nr;
    __asm__ volatile("ecall" : "=r"(a0) : "r"(a7) : "memory");
    return a0;
}

static inline long raw_syscall1(long nr, long arg0)
{
    register long a0 __asm__("a0") = arg0;
    register long a7 __asm__("a7") = nr;
    __asm__ volatile("ecall" : "+r"(a0) : "r"(a7) : "memory");
    return a0;
}

static inline long raw_syscall2(long nr, long arg0, long arg1)
{
    register long a0 __asm__("a0") = arg0;
    register long a1 __asm__("a1") = arg1;
    register long a7 __asm__("a7") = nr;
    __asm__ volatile("ecall" : "+r"(a0) : "r"(a1), "r"(a7) : "memory");
    return a0;
}

static inline long raw_syscall3(long nr, long arg0, long arg1, long arg2)
{
    register long a0 __asm__("a0") = arg0;
    register long a1 __asm__("a1") = arg1;
    register long a2 __asm__("a2") = arg2;
    register long a7 __asm__("a7") = nr;
    __asm__ volatile("ecall" : "+r"(a0) : "r"(a1), "r"(a2), "r"(a7) : "memory");
    return a0;
}

static inline long raw_syscall4(long nr, long arg0, long arg1, long arg2, long arg3)
{
    register long a0 __asm__("a0") = arg0;
    register long a1 __asm__("a1") = arg1;
    register long a2 __asm__("a2") = arg2;
    register long a3 __asm__("a3") = arg3;
    register long a7 __asm__("a7") = nr;
    __asm__ volatile("ecall" : "+r"(a0) : "r"(a1), "r"(a2), "r"(a3), "r"(a7) : "memory");
    return a0;
}

static uint64_t timespec_ns(const struct timespec *value)
{
    return (uint64_t)value->tv_sec * UINT64_C(1000000000) + (uint64_t)value->tv_nsec;
}

static int monotonic_raw(struct timespec *value)
{
    long result = raw_syscall2(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC_RAW_ID, (long)value);
    return result < 0 ? (int)-result : 0;
}

static long op_empty(void *context)
{
    uintptr_t value = (uintptr_t)context;
    __asm__ volatile("" : "+r"(value) : : "memory");
    return (long)value;
}

static long op_syscall0(void *context)
{
    return raw_syscall0((long)(uintptr_t)context);
}

static long op_clock_gettime(void *context)
{
    return raw_syscall2(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC_RAW_ID, (long)context);
}

static long op_gettimeofday(void *context)
{
    return raw_syscall2(SYS_GETTIMEOFDAY, (long)context, 0);
}

static long op_futex_wake(void *context)
{
    return raw_syscall3(SYS_FUTEX, (long)context, FUTEX_WAKE_PRIVATE, 1);
}

static long op_read(void *context)
{
    const struct rw_context *rw = context;
    return raw_syscall3(SYS_READ, rw->fd, (long)rw->buffer, (long)rw->length);
}

static long op_write(void *context)
{
    const struct rw_context *rw = context;
    return raw_syscall3(SYS_WRITE, rw->fd, (long)rw->buffer, (long)rw->length);
}

static unsigned char read_buffer[RW_BUFFER_SIZE] __attribute__((aligned(16)));
static unsigned char write_buffer[RW_BUFFER_SIZE] __attribute__((aligned(16)));
static struct rw_context read_context = {
    .fd = -1,
    .buffer = read_buffer,
    .length = sizeof(read_buffer),
};
static struct rw_context write_context = {
    .fd = -1,
    .buffer = write_buffer,
    .length = sizeof(write_buffer),
};

static int prepare_case(const struct bench_case *bench)
{
    struct rw_context *rw;
    const char *path;
    long flags;

    if (strcmp(bench->name, "read") == 0) {
        rw = bench->context;
        path = "/dev/zero";
        flags = O_RDONLY;
    } else if (strcmp(bench->name, "write") == 0) {
        rw = bench->context;
        path = "/dev/null";
        flags = O_WRONLY;
        for (size_t index = 0; index < rw->length; ++index) {
            rw->buffer[index] = (unsigned char)(index * 17U + 3U);
        }
    } else {
        return 0;
    }
    rw->fd = raw_syscall4(SYS_OPENAT, AT_FDCWD, (long)path, flags, 0);
    return rw->fd < 0 ? -1 : 0;
}

static void cleanup_case(const struct bench_case *bench)
{
    if (strcmp(bench->name, "read") == 0 || strcmp(bench->name, "write") == 0) {
        struct rw_context *rw = bench->context;
        if (rw->fd >= 0) {
            (void)raw_syscall1(SYS_CLOSE, rw->fd);
            rw->fd = -1;
        }
    }
}

static int timespec_is_valid(const struct timespec *value)
{
    return value->tv_sec >= 0 && value->tv_nsec >= 0 && value->tv_nsec < 1000000000L;
}

static int timeval_is_valid(const struct timeval64 *value)
{
    return value->tv_sec >= 0 && value->tv_usec >= 0 && value->tv_usec < 1000000;
}

/* 正确性探针位于计时和 TCG marker 窗口之外，不改变被测热循环。 */
static int validate_case(const struct bench_case *bench)
{
    long first;
    long second;

    if (strcmp(bench->name, "clock_gettime") == 0) {
        first = bench->op(bench->context);
        struct timespec first_value = *(const struct timespec *)bench->context;
        second = bench->op(bench->context);
        const struct timespec *second_value = (const struct timespec *)bench->context;
        return first == 0 && second == 0 && timespec_is_valid(&first_value)
               && timespec_is_valid(second_value)
               && timespec_ns(second_value) >= timespec_ns(&first_value)
                   ? 0
                   : -1;
    }
    if (strcmp(bench->name, "gettimeofday") == 0) {
        first = bench->op(bench->context);
        struct timeval64 first_value = *(const struct timeval64 *)bench->context;
        second = bench->op(bench->context);
        const struct timeval64 *second_value = (const struct timeval64 *)bench->context;
        int ordered = second_value->tv_sec > first_value.tv_sec
                      || (second_value->tv_sec == first_value.tv_sec
                          && second_value->tv_usec >= first_value.tv_usec);
        return first == 0 && second == 0 && timeval_is_valid(&first_value)
               && timeval_is_valid(second_value) && ordered
                   ? 0
                   : -1;
    }

    first = bench->op(bench->context);
    second = bench->op(bench->context);
    if (strcmp(bench->name, "read") == 0 || strcmp(bench->name, "write") == 0) {
        const struct rw_context *rw = bench->context;
        return first == (long)rw->length && second == (long)rw->length ? 0 : -1;
    }
    if (strcmp(bench->name, "getpid") == 0 || strcmp(bench->name, "getppid") == 0
        || strcmp(bench->name, "gettid") == 0) {
        return first > 0 && second == first ? 0 : -1;
    }
    return first == 0 && second == 0 ? 0 : -1;
}

__attribute__((noinline)) static uint64_t run_loop(bench_op_t op, void *context,
                                                   uint64_t iterations,
                                                   uint64_t *errors)
{
    uint64_t checksum = 0;
    uint64_t failed = 0;

    for (uint64_t index = 0; index < iterations; ++index) {
        long value = op(context);
        checksum += (uint64_t)value;
        failed += value < 0;
    }
    *errors = failed;
    return checksum;
}

static int measure(const struct bench_case *bench, uint64_t iterations,
                   struct result *result)
{
    struct timespec start;
    struct timespec end;
    uint64_t errors = 0;
    uint64_t empty_before_errors = 0;
    uint64_t empty_after_errors = 0;
    uint64_t empty_before_ns;
    uint64_t empty_after_ns;
    uint64_t checksum;

    if (monotonic_raw(&start) != 0) {
        return -1;
    }
    checksum = run_loop(op_empty, bench->context, iterations, &empty_before_errors);
    if (monotonic_raw(&end) != 0) {
        return -1;
    }
    empty_before_ns = timespec_ns(&end) - timespec_ns(&start);

    if (monotonic_raw(&start) != 0) {
        return -1;
    }
    syscall_profile_start();
    checksum ^= run_loop(bench->op, bench->context, iterations, &errors);
    syscall_profile_stop();
    if (monotonic_raw(&end) != 0) {
        return -1;
    }
    result->total_ns = timespec_ns(&end) - timespec_ns(&start);

    if (monotonic_raw(&start) != 0) {
        return -1;
    }
    checksum ^= run_loop(op_empty, bench->context, iterations, &empty_after_errors);
    if (monotonic_raw(&end) != 0) {
        return -1;
    }
    empty_after_ns = timespec_ns(&end) - timespec_ns(&start);
    result->empty_ns = empty_before_ns / 2 + empty_after_ns / 2
                       + (empty_before_ns % 2 + empty_after_ns % 2) / 2;
    result->net_ns = result->total_ns > result->empty_ns
                         ? result->total_ns - result->empty_ns
                         : 0;
    result->errors = errors + empty_before_errors + empty_after_errors;
    result->checksum = checksum;
    return 0;
}

static int compare_u64(const void *left, const void *right)
{
    uint64_t a = *(const uint64_t *)left;
    uint64_t b = *(const uint64_t *)right;
    return (a > b) - (a < b);
}

static int parse_count(const char *text, uint64_t minimum, uint64_t maximum,
                       uint64_t *value)
{
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (errno != 0 || !text[0] || !end || *end || parsed < minimum || parsed > maximum) {
        return -1;
    }
    *value = (uint64_t)parsed;
    return 0;
}

static int selected(const char *filter, const char *name)
{
    return strcmp(filter, "all") == 0 || strcmp(filter, name) == 0;
}

static void print_ns_per_call(uint64_t nanoseconds, uint64_t iterations)
{
    uint64_t whole = nanoseconds / iterations;
    uint64_t fraction = nanoseconds % iterations * UINT64_C(1000) / iterations;
    printf("%" PRIu64 ".%03" PRIu64, whole, fraction);
}

int main(int argc, char **argv)
{
    uint64_t iterations = UINT64_C(1000000);
    uint64_t warmup = UINT64_C(100000);
    uint64_t repeats_value = 5;
    const char *filter = "all";
    struct timespec clock_value = {0};
    struct timeval64 time_value = {0};
    uint32_t futex_word = 0;
    struct bench_case cases[] = {
        {"getpid", SYS_GETPID, op_syscall0, (void *)(uintptr_t)SYS_GETPID},
        {"getppid", SYS_GETPPID, op_syscall0, (void *)(uintptr_t)SYS_GETPPID},
        {"getuid", SYS_GETUID, op_syscall0, (void *)(uintptr_t)SYS_GETUID},
        {"gettid", SYS_GETTID, op_syscall0, (void *)(uintptr_t)SYS_GETTID},
        {"clock_gettime", SYS_CLOCK_GETTIME, op_clock_gettime, &clock_value},
        {"gettimeofday", SYS_GETTIMEOFDAY, op_gettimeofday, &time_value},
        {"futex_wake", SYS_FUTEX, op_futex_wake, &futex_word},
        {"sched_yield", SYS_SCHED_YIELD, op_syscall0, (void *)(uintptr_t)SYS_SCHED_YIELD},
        {"read", SYS_READ, op_read, &read_context},
        {"write", SYS_WRITE, op_write, &write_context},
    };

    if (argc > 1 && parse_count(argv[1], 1, UINT64_C(1000000000), &iterations) != 0) {
        fprintf(stderr, "usage: %s [iterations [repeats [case|all [warmup]]]]\n", argv[0]);
        return 2;
    }
    if (argc > 2 && parse_count(argv[2], 1, 31, &repeats_value) != 0) {
        fprintf(stderr, "usage: %s [iterations [repeats [case|all [warmup]]]]\n", argv[0]);
        return 2;
    }
    if (argc > 3) {
        filter = argv[3];
    }
    if (argc > 4 && parse_count(argv[4], 0, UINT64_C(1000000000), &warmup) != 0) {
        fprintf(stderr, "usage: %s [iterations [repeats [case|all [warmup]]]]\n", argv[0]);
        return 2;
    }
    if (argc > 5) {
        fprintf(stderr, "usage: %s [iterations [repeats [case|all [warmup]]]]\n", argv[0]);
        return 2;
    }
    if ((repeats_value & 1U) == 0) {
        fprintf(stderr, "repeats must be odd so the median is unambiguous\n");
        return 2;
    }

    size_t repeats = (size_t)repeats_value;
    uint64_t medians[31];
    unsigned int selected_cases = 0;
    unsigned int failures = 0;
    printf("SYSCALL_BENCH version=1 arch=riscv64 iterations=%" PRIu64
           " warmup=%" PRIu64 " repeats=%zu filter=%s\n",
           iterations, warmup, repeats, filter);

    for (size_t case_index = 0; case_index < sizeof(cases) / sizeof(cases[0]); ++case_index) {
        const struct bench_case *bench = &cases[case_index];
        if (!selected(filter, bench->name)) {
            continue;
        }
        ++selected_cases;
        if (prepare_case(bench) != 0) {
            fprintf(stderr, "SYSCALL_ERROR case=%s phase=prepare\n", bench->name);
            ++failures;
            continue;
        }
        if (validate_case(bench) != 0) {
            fprintf(stderr, "SYSCALL_ERROR case=%s phase=validate\n", bench->name);
            ++failures;
            cleanup_case(bench);
            continue;
        }
        uint64_t warmup_errors = 0;
        uint64_t empty_warmup_errors = 0;
        (void)run_loop(bench->op, bench->context, warmup, &warmup_errors);
        (void)run_loop(op_empty, bench->context, warmup, &empty_warmup_errors);
        if (warmup_errors != 0 || empty_warmup_errors != 0) {
            fprintf(stderr, "SYSCALL_ERROR case=%s phase=warmup errors=%" PRIu64 "\n",
                    bench->name, warmup_errors + empty_warmup_errors);
            ++failures;
            cleanup_case(bench);
            continue;
        }

        size_t completed = 0;
        for (size_t round = 0; round < repeats; ++round) {
            struct result value = {0};
            if (measure(bench, iterations, &value) != 0) {
                fprintf(stderr, "SYSCALL_ERROR case=%s phase=measure round=%zu\n",
                        bench->name, round + 1);
                ++failures;
                break;
            }
            medians[round] = value.net_ns;
            ++completed;
            printf("SYSCALL_RESULT case=%s syscall=%ld round=%zu iterations=%" PRIu64
                   " total_ns=%" PRIu64 " empty_ns=%" PRIu64 " net_ns=%" PRIu64
                   " avg_ns=",
                   bench->name, bench->syscall_nr, round + 1, iterations, value.total_ns,
                   value.empty_ns, value.net_ns);
            print_ns_per_call(value.net_ns, iterations);
            printf(" errors=%" PRIu64 " checksum=%" PRIu64 "\n", value.errors,
                   value.checksum);
            if (value.errors != 0) {
                ++failures;
            }
        }
        if (completed != repeats) {
            cleanup_case(bench);
            continue;
        }
        qsort(medians, completed, sizeof(medians[0]), compare_u64);
        uint64_t median = medians[completed / 2];
        printf("SYSCALL_SUMMARY case=%s syscall=%ld iterations=%" PRIu64
               " repeats=%zu median_net_ns=%" PRIu64 " median_avg_ns=",
               bench->name, bench->syscall_nr, iterations, repeats, median);
        print_ns_per_call(median, iterations);
        putchar('\n');
        cleanup_case(bench);
    }

    if (selected_cases == 0) {
        fprintf(stderr, "SYSCALL_ERROR unknown_case=%s\n", filter);
        return 2;
    }
    printf("SYSCALL_BENCH_DONE status=%u cases=%u\n", failures ? 1U : 0U,
           selected_cases);
    return failures ? 1 : 0;
}
