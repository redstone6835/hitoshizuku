#include "bench-platform.h"

#include <limits.h>

#if BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_1 || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_64 || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_256
static const unsigned char stream_payload[256] =
    "three-platform-payload-0123456789abcdefghijklmnopqrstuvwxyz0123\n";
#endif

static uint64_t measured_ticks[BENCH_ROUNDS * BENCH_SAMPLES];

static size_t append_text(char *buffer, size_t offset, const char *text) {
    while (*text != '\0') {
        buffer[offset++] = *text++;
    }
    return offset;
}

static size_t append_u64(char *buffer, size_t offset, uint64_t value) {
    char digits[20];
    size_t count = 0;
    do {
        digits[count++] = (char)('0' + value % 10);
        value /= 10;
    } while (value != 0);
    while (count != 0) {
        buffer[offset++] = digits[--count];
    }
    return offset;
}

static int text_equal(const char *left, const char *right) {
    while (*left != '\0' && *right != '\0' && *left == *right) {
        ++left;
        ++right;
    }
    return *left == *right;
}

static int valid_system(const char *system) {
    return text_equal(system, "linux") || text_equal(system, "mygo-tomori") ||
        text_equal(system, "mygo-native");
}

static int parse_unsigned(const char *text, unsigned *value) {
    uint64_t parsed = 0;
    if (text == 0 || *text == '\0') {
        return 0;
    }
    while (*text != '\0') {
        if (*text < '0' || *text > '9') {
            return 0;
        }
        parsed = parsed * 10 + (uint64_t)(*text - '0');
        if (parsed > UINT_MAX) {
            return 0;
        }
        ++text;
    }
    *value = (unsigned)parsed;
    return 1;
}

static int write_line(
    const struct bench_platform *platform,
    const char *prefix,
    const char *system,
    const char *workload,
    unsigned boot,
    const char *mode,
    const char *suffix) {
    char line[256];
    size_t length = 0;
    length = append_text(line, length, prefix);
    length = append_text(line, length, " system=");
    length = append_text(line, length, system);
    length = append_text(line, length, " workload=");
    length = append_text(line, length, workload);
    length = append_text(line, length, " boot=");
    length = append_u64(line, length, boot);
    length = append_text(line, length, " mode=");
    length = append_text(line, length, mode);
    length = append_text(line, length, suffix);
    line[length++] = '\n';
    return platform->stream_write(line, length);
}

static int emit_meta(
    const struct bench_platform *platform,
    const char *system,
    const char *workload,
    unsigned boot,
    const char *mode) {
    char suffix[96];
    size_t length = 0;
    length = append_text(suffix, length, " counter=rdtime counter_hz=");
    length = append_u64(suffix, length, BENCH_COUNTER_HZ);
    suffix[length] = '\0';
    return write_line(platform, "BENCH_META", system, workload, boot, mode, suffix);
}

static int emit_sample(
    const struct bench_platform *platform,
    const char *system,
    const char *workload,
    unsigned boot,
    unsigned round,
    uint64_t ticks,
    const char *mode) {
    char suffix[128];
    size_t length = 0;
    length = append_text(suffix, length, " round=");
    length = append_u64(suffix, length, round);
    length = append_text(suffix, length, " sample_ticks=");
    length = append_u64(suffix, length, ticks);
    length = append_text(suffix, length, " status=ok");
    suffix[length] = '\0';
    return write_line(platform, "BENCH_SAMPLE", system, workload, boot, mode, suffix);
}

static int emit_done(
    const struct bench_platform *platform,
    const char *system,
    const char *workload,
    unsigned boot,
    const char *status,
    const char *detail,
    const char *mode) {
    char suffix[128];
    size_t length = 0;
    length = append_text(suffix, length, " status=");
    length = append_text(suffix, length, status);
    if (detail != 0) {
        length = append_text(suffix, length, " detail=");
        length = append_text(suffix, length, detail);
    }
    suffix[length] = '\0';
    return write_line(platform, "BENCH_DONE", system, workload, boot, mode, suffix);
}

static const char *mode_name(void) {
#if BENCH_MODE == BENCH_MODE_WARM
    return "warm";
#elif BENCH_MODE == BENCH_MODE_COLD
    return "cold";
#else
#error "unsupported BENCH_MODE"
#endif
}

static const char *workload_name(void) {
#if BENCH_WORKLOAD == BENCH_WORKLOAD_CLOCK_READ
    return "clock-read";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE
    return "stream-write";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_1
    return "stream-write-1";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_64
    return "stream-write-64";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_256
    return "stream-write-256";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_SMALL
    return "heap-small";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_BATCH
    return "heap-batch";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_MAP_LARGE
    return "map-large";
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_PAGE_TOUCH
    return "page-touch";
#else
#error "unsupported BENCH_WORKLOAD"
#endif
}

#if BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_1 || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_64 || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_256
static size_t stream_length(void) {
#if BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_64
    return 64;
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_1
    return 1;
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_256
    return 256;
#else
    return 0;
#endif
}
#endif

static int run_operation(const struct bench_platform *platform) {
#if BENCH_WORKLOAD == BENCH_WORKLOAD_CLOCK_READ
    return platform->clock_read();
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_1 || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_64 || \
    BENCH_WORKLOAD == BENCH_WORKLOAD_STREAM_WRITE_256
    return platform->stream_write(stream_payload, stream_length());
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_SMALL
    return platform->heap_cycle(32, 1);
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_HEAP_BATCH
    return platform->heap_cycle(32, 64);
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_MAP_LARGE
    return platform->map_cycle(65536, 0);
#elif BENCH_WORKLOAD == BENCH_WORKLOAD_PAGE_TOUCH
    return platform->map_cycle(1048576, 1);
#else
#error "unsupported BENCH_WORKLOAD"
#endif
}

int bench_run(
    const struct bench_platform *platform,
    const char *system_id,
    const char *workload_id,
    unsigned boot,
    unsigned rounds,
    unsigned samples,
    unsigned warmup) {
    if (platform == 0 || platform->counter == 0 || platform->clock_read == 0 ||
        platform->stream_write == 0 || platform->heap_cycle == 0 ||
        platform->map_cycle == 0 || rounds > BENCH_ROUNDS || samples > BENCH_SAMPLES) {
        return 2;
    }

    for (unsigned index = 0; index < warmup; ++index) {
        if (run_operation(platform) != 0) {
            (void)emit_done(
                platform, system_id, workload_id, boot, "error", "warmup_failed", mode_name());
            return 3;
        }
    }

    for (unsigned round = 0; round < rounds; ++round) {
        for (unsigned sample = 0; sample < samples; ++sample) {
            uint64_t begin = platform->counter();
            if (run_operation(platform) != 0) {
                (void)emit_done(
                    platform,
                    system_id,
                    workload_id,
                    boot,
                    "error",
                    "operation_failed",
                    mode_name());
                return 4;
            }
            uint64_t end = platform->counter();
            if (end < begin) {
                (void)emit_done(
                    platform,
                    system_id,
                    workload_id,
                    boot,
                    "error",
                    "counter_regressed",
                    mode_name());
                return 5;
            }
            measured_ticks[round * BENCH_SAMPLES + sample] = end - begin;
        }
    }

    if (emit_meta(platform, system_id, workload_id, boot, mode_name()) != 0) {
        return 6;
    }
    for (unsigned round = 0; round < rounds; ++round) {
        for (unsigned sample = 0; sample < samples; ++sample) {
            if (emit_sample(
                    platform,
                    system_id,
                    workload_id,
                    boot,
                    round,
                    measured_ticks[round * BENCH_SAMPLES + sample],
                    mode_name()) != 0) {
                return 7;
            }
        }
    }
    return emit_done(platform, system_id, workload_id, boot, "ok", 0, mode_name()) == 0 ? 0 : 8;
}

int main(int argc, char **argv, char **envp) {
    (void)envp;
    unsigned boot;
    if (argc != 4 || !valid_system(argv[1]) || !parse_unsigned(argv[2], &boot) ||
        !text_equal(argv[3], mode_name())) {
        return 9;
    }
    return bench_run(
        &bench_platform,
        argv[1],
        workload_name(),
        boot,
        BENCH_ROUNDS,
        BENCH_SAMPLES,
        BENCH_WARMUP);
}
