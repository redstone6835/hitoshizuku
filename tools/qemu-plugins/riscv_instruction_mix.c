#define _GNU_SOURCE
#include <qemu-plugin.h>

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#include "riscv_instruction_seen_table.h"

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

enum {
    MAX_VCPUS = 8,
    MAX_DESCRIPTORS = 512,
    DESCRIPTOR_HASH_SLOTS = 1024,
    MAX_MNEMONIC_BYTES = 96,
    MAX_INSN_BYTES = 32,
    DEFAULT_EPOCH_MS = 1000,
    MIN_EPOCH_MS = 10,
    MAX_EPOCH_MS = 60000,
    CATALOG_BUFFER_BYTES = 4 * 1024 * 1024,
    CATALOG_FLUSH_RECORDS = 4096,
    CATALOG_SEEN_INITIAL_BITS = 20,
    CATALOG_SEEN_INITIAL_SLOTS = 1 << CATALOG_SEEN_INITIAL_BITS,
};

enum execution_domain {
    DOMAIN_USER,
    DOMAIN_KERNEL,
    DOMAIN_COUNT,
};

struct instruction_descriptor {
    uint32_t size;
    char mnemonic[MAX_MNEMONIC_BYTES];
};

struct vcpu_counters {
    uint64_t mix[DOMAIN_COUNT][MAX_DESCRIPTORS];
    uint64_t blocks[DOMAIN_COUNT];
    uint64_t instructions[DOMAIN_COUNT];
};

struct tb_mix_entry {
    uint16_t descriptor_id;
    uint64_t count;
};

struct catalog_instruction {
    uint64_t pc;
    uint32_t size;
    uint32_t copied;
    int descriptor_id;
    bool bytes_complete;
    unsigned char bytes[MAX_INSN_BYTES];
    char mnemonic[MAX_MNEMONIC_BYTES];
};

struct counter_snapshot {
    uint64_t mix[DOMAIN_COUNT][MAX_DESCRIPTORS];
    uint64_t blocks[DOMAIN_COUNT];
    uint64_t instructions[DOMAIN_COUNT];
    uint64_t translated_blocks;
    uint64_t translated_instructions;
    uint64_t max_tb_instructions;
    size_t descriptor_count;
};

struct control_result {
    bool valid;
    bool active;
    int error_number;
    const char *error_kind;
};

static struct qemu_plugin_scoreboard *scoreboard;
static struct instruction_descriptor descriptors[MAX_DESCRIPTORS];
static uint16_t descriptor_hash[DESCRIPTOR_HASH_SLOTS];
static size_t descriptor_count;
static pthread_mutex_t descriptor_mutex = PTHREAD_MUTEX_INITIALIZER;

static FILE *output_file;
static FILE *catalog_file;
static char *catalog_buffer;
static char *output_path;
static char *control_path;
static char *catalog_path;
static char target_name[64];
static unsigned int configured_vcpus;
static unsigned int epoch_ms = DEFAULT_EPOCH_MS;

static struct riscv_seen_table seen_pcs;
static struct riscv_seen_table seen_fingerprints;
static pthread_mutex_t catalog_mutex = PTHREAD_MUTEX_INITIALIZER;

static pthread_t sampler_thread;
static bool sampler_started;
static pthread_mutex_t sampler_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t sampler_condition;
static bool sampler_condition_initialized;
static atomic_bool stop_sampler;
static atomic_bool plugin_exiting;

static atomic_uint_fast64_t translated_blocks;
static atomic_uint_fast64_t translated_instructions;
static atomic_uint_fast64_t max_tb_instructions;
static atomic_uint_fast64_t descriptor_overflow_instructions;
static atomic_uint_fast64_t descriptor_overflow_blocks;
static atomic_uint_fast64_t disassembly_errors;
static atomic_uint_fast64_t mnemonic_truncations;
static atomic_uint_fast64_t invalid_instruction_sizes;
static atomic_uint_fast64_t instruction_data_errors;
static atomic_uint_fast64_t catalog_allocation_failures;
static atomic_uint_fast64_t catalog_records;
static atomic_uint_fast64_t catalog_write_errors;
static atomic_uint_fast64_t catalog_dropped_blocks;
static uint64_t catalog_pending_records;
static uint64_t catalog_flushes;
static atomic_uint_fast64_t duplicate_pc_translations;
static atomic_uint_fast64_t duplicate_exact_translations;
static atomic_uint_fast64_t duplicate_tracking_drops;
static atomic_uint_fast64_t output_write_errors;
static atomic_uint_fast64_t control_read_errors;
static atomic_uint_fast64_t counter_regressions;
static atomic_uint_fast64_t unsupported_vcpu_events;
static atomic_uint_fast64_t late_translation_drops;
static atomic_uint_fast64_t sampler_wait_errors;
static atomic_uint_fast64_t start_detections;
static atomic_uint_fast64_t stop_detections;
static atomic_uint_fast64_t exit_stops;
static atomic_uint_fast64_t emitted_samples;
static atomic_uint_fast64_t emitted_windows;

static uint64_t monotonic_ns(void)
{
    struct timespec value;

    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) +
           (uint64_t)value.tv_nsec;
}

static long host_tid(void)
{
    return syscall(SYS_gettid);
}

static uint64_t fnv1a_bytes(uint64_t hash, const void *raw, size_t size)
{
    const unsigned char *bytes = raw;

    for (size_t index = 0; index < size; ++index) {
        hash ^= bytes[index];
        hash *= UINT64_C(1099511628211);
    }
    return hash;
}

static uint64_t descriptor_key_hash(const char *mnemonic, uint32_t size)
{
    uint64_t hash = fnv1a_bytes(UINT64_C(1469598103934665603), &size,
                                sizeof(size));
    return fnv1a_bytes(hash, mnemonic, strlen(mnemonic));
}

static qemu_plugin_u64 scoreboard_entry(size_t offset)
{
    return (qemu_plugin_u64){.score = scoreboard, .offset = offset};
}

static qemu_plugin_u64 mix_entry(enum execution_domain domain, size_t id)
{
    return scoreboard_entry(offsetof(struct vcpu_counters, mix) +
                            ((size_t)domain * MAX_DESCRIPTORS + id) *
                                sizeof(uint64_t));
}

static qemu_plugin_u64 block_entry(enum execution_domain domain)
{
    return scoreboard_entry(offsetof(struct vcpu_counters, blocks) +
                            (size_t)domain * sizeof(uint64_t));
}

static qemu_plugin_u64 instruction_entry(enum execution_domain domain)
{
    return scoreboard_entry(offsetof(struct vcpu_counters, instructions) +
                            (size_t)domain * sizeof(uint64_t));
}

static void update_max(atomic_uint_fast64_t *destination, uint64_t value)
{
    uint64_t observed = atomic_load_explicit(destination, memory_order_relaxed);

    while (observed < value &&
           !atomic_compare_exchange_weak_explicit(
               destination, &observed, value, memory_order_relaxed,
               memory_order_relaxed)) {
    }
}

static void json_string(FILE *stream, const char *value)
{
    const unsigned char *cursor = (const unsigned char *)value;

    fputc('"', stream);
    while (*cursor) {
        unsigned char character = *cursor++;

        switch (character) {
        case '"':
            fputs("\\\"", stream);
            break;
        case '\\':
            fputs("\\\\", stream);
            break;
        case '\b':
            fputs("\\b", stream);
            break;
        case '\f':
            fputs("\\f", stream);
            break;
        case '\n':
            fputs("\\n", stream);
            break;
        case '\r':
            fputs("\\r", stream);
            break;
        case '\t':
            fputs("\\t", stream);
            break;
        default:
            if (character < 0x20) {
                fprintf(stream, "\\u%04x", character);
            } else {
                fputc(character, stream);
            }
            break;
        }
    }
    fputc('"', stream);
}

static bool finish_record(FILE *stream, atomic_uint_fast64_t *errors)
{
    fputc('\n', stream);
    if (fflush(stream) != 0 || ferror(stream)) {
        atomic_fetch_add_explicit(errors, 1, memory_order_relaxed);
        clearerr(stream);
        return false;
    }
    return true;
}

/*
 * Catalog records are orders of magnitude more frequent than mix epochs.
 * Keep their normal-exit evidence complete without issuing one fflush syscall
 * per translated TB. A crash can lose only the current bounded batch; such a
 * file has no final quality record and is rejected by the validator.
 */
static bool flush_catalog(void)
{
    if (fflush(catalog_file) != 0 || ferror(catalog_file)) {
        atomic_fetch_add_explicit(&catalog_write_errors, 1,
                                  memory_order_relaxed);
        clearerr(catalog_file);
        return false;
    }
    catalog_pending_records = 0;
    ++catalog_flushes;
    return true;
}

static bool finish_catalog_record(bool translation_record, bool force_flush)
{
    fputc('\n', catalog_file);
    if (ferror(catalog_file)) {
        atomic_fetch_add_explicit(&catalog_write_errors, 1,
                                  memory_order_relaxed);
        clearerr(catalog_file);
        return false;
    }
    if (translation_record) {
        ++catalog_pending_records;
    }
    if (force_flush || catalog_pending_records >= CATALOG_FLUSH_RECORDS) {
        return flush_catalog();
    }
    return true;
}

static bool extract_mnemonic(const char *disassembly, char *destination,
                             size_t capacity)
{
    const unsigned char *cursor = (const unsigned char *)disassembly;
    size_t length = 0;
    bool complete = true;

    while (*cursor == ' ' || *cursor == '\t') {
        ++cursor;
    }
    while (*cursor && *cursor != ' ' && *cursor != '\t' && *cursor != ',') {
        if (length + 1 < capacity) {
            destination[length++] = (char)*cursor;
        } else {
            complete = false;
        }
        ++cursor;
    }
    if (length == 0) {
        snprintf(destination, capacity, "%s", "<invalid>");
        return false;
    }
    destination[length] = '\0';
    return complete;
}

static int descriptor_for(const char *mnemonic, uint32_t size)
{
    uint64_t hash = descriptor_key_hash(mnemonic, size);
    size_t slot = (size_t)hash & (DESCRIPTOR_HASH_SLOTS - 1);
    int result = -1;

    pthread_mutex_lock(&descriptor_mutex);
    for (size_t probe = 0; probe < DESCRIPTOR_HASH_SLOTS; ++probe) {
        uint16_t encoded = descriptor_hash[slot];

        if (encoded == 0) {
            if (descriptor_count == MAX_DESCRIPTORS) {
                break;
            }
            size_t id = descriptor_count++;
            descriptors[id].size = size;
            snprintf(descriptors[id].mnemonic,
                     sizeof(descriptors[id].mnemonic), "%s", mnemonic);
            descriptor_hash[slot] = (uint16_t)(id + 1);
            result = (int)id;
            break;
        }
        size_t id = (size_t)encoded - 1;
        if (descriptors[id].size == size &&
            strcmp(descriptors[id].mnemonic, mnemonic) == 0) {
            result = (int)id;
            break;
        }
        slot = (slot + 1) & (DESCRIPTOR_HASH_SLOTS - 1);
    }
    pthread_mutex_unlock(&descriptor_mutex);
    return result;
}

static enum execution_domain domain_for_pc(uint64_t pc)
{
    return (pc >> 63) != 0 ? DOMAIN_KERNEL : DOMAIN_USER;
}

static uint64_t catalog_fingerprint(uint64_t guest_pc,
                                    const struct catalog_instruction *insns,
                                    size_t count)
{
    uint64_t hash = fnv1a_bytes(UINT64_C(1469598103934665603), &guest_pc,
                                sizeof(guest_pc));
    hash = fnv1a_bytes(hash, &count, sizeof(count));
    for (size_t index = 0; index < count; ++index) {
        hash = fnv1a_bytes(hash, &insns[index].pc, sizeof(insns[index].pc));
        hash = fnv1a_bytes(hash, &insns[index].size,
                           sizeof(insns[index].size));
        hash = fnv1a_bytes(hash, insns[index].bytes, insns[index].copied);
    }
    return hash == 0 ? UINT64_C(1) : hash;
}

static void write_catalog_block(uint64_t translation_index,
                                uint64_t translation_begin_ns,
                                uint64_t guest_pc, long tid,
                                enum execution_domain domain,
                                const struct catalog_instruction *insns,
                                size_t count, uint64_t overflow_count,
                                uint64_t decode_error_count)
{
    uint64_t fingerprint = catalog_fingerprint(guest_pc, insns, count);
    bool duplicate_pc = false;
    bool duplicate_exact = false;
    uint64_t timestamp = monotonic_ns();

    pthread_mutex_lock(&catalog_mutex);
    enum riscv_seen_result pc_result =
        riscv_seen_table_insert(&seen_pcs, guest_pc + 1);
    enum riscv_seen_result exact_result =
        riscv_seen_table_insert(&seen_fingerprints, fingerprint);
    if (pc_result == RISCV_SEEN_DUPLICATE) {
        duplicate_pc = true;
        atomic_fetch_add_explicit(&duplicate_pc_translations, 1,
                                  memory_order_relaxed);
    } else if (pc_result == RISCV_SEEN_DROPPED) {
        atomic_fetch_add_explicit(&duplicate_tracking_drops, 1,
                                  memory_order_relaxed);
    }
    if (exact_result == RISCV_SEEN_DUPLICATE) {
        duplicate_exact = true;
        atomic_fetch_add_explicit(&duplicate_exact_translations, 1,
                                  memory_order_relaxed);
    } else if (exact_result == RISCV_SEEN_DROPPED) {
        atomic_fetch_add_explicit(&duplicate_tracking_drops, 1,
                                  memory_order_relaxed);
    }

    fprintf(catalog_file,
            "{\"schema\":\"mygo.riscv-tb-catalog.v1\",\"type\":\"tb\","
            "\"monotonic_ns\":%" PRIu64 ",\"translation_begin_ns\":%" PRIu64
            ",\"host_tid\":%ld,\"translation_index\":%" PRIu64
            ",\"guest_pc\":\"0x%016" PRIx64 "\",\"mode\":\"%s\","
            "\"instruction_count\":%zu,\"duplicate_pc\":%s,"
            "\"duplicate_exact\":%s,\"descriptor_overflow\":%" PRIu64
            ",\"decode_errors\":%" PRIu64 ",\"instructions\":[",
            timestamp, translation_begin_ns, tid, translation_index, guest_pc,
            domain == DOMAIN_KERNEL ? "kernel" : "user", count,
            duplicate_pc ? "true" : "false",
            duplicate_exact ? "true" : "false", overflow_count,
            decode_error_count);
    for (size_t index = 0; index < count; ++index) {
        const struct catalog_instruction *insn = &insns[index];
        if (index != 0) {
            fputc(',', catalog_file);
        }
        fprintf(catalog_file,
                "{\"pc\":\"0x%016" PRIx64 "\",\"size\":%u,\"bytes\":\"",
                insn->pc, insn->size);
        for (uint32_t byte = 0; byte < insn->copied; ++byte) {
            fprintf(catalog_file, "%02x", insn->bytes[byte]);
        }
        fprintf(catalog_file, "\",\"bytes_complete\":%s,\"descriptor_id\":",
                insn->bytes_complete ? "true" : "false");
        if (insn->descriptor_id >= 0) {
            fprintf(catalog_file, "%d", insn->descriptor_id);
        } else {
            fputs("null", catalog_file);
        }
        fputs(",\"mnemonic\":", catalog_file);
        json_string(catalog_file, insn->mnemonic);
        fputc('}', catalog_file);
    }
    fputs("]}", catalog_file);
    if (finish_catalog_record(true, false)) {
        atomic_fetch_add_explicit(&catalog_records, 1, memory_order_relaxed);
    } else {
        atomic_fetch_add_explicit(&catalog_dropped_blocks, 1,
                                  memory_order_relaxed);
    }
    pthread_mutex_unlock(&catalog_mutex);
}

static void translate_block(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    (void)id;
    if (atomic_load_explicit(&plugin_exiting, memory_order_relaxed)) {
        atomic_fetch_add_explicit(&late_translation_drops, 1,
                                  memory_order_relaxed);
        return;
    }

    uint64_t translation_begin_ns = monotonic_ns();
    long tid = host_tid();
    uint64_t guest_pc = qemu_plugin_tb_vaddr(tb);
    enum execution_domain domain = domain_for_pc(guest_pc);
    size_t instruction_count = qemu_plugin_tb_n_insns(tb);
    uint64_t translation_index =
        atomic_fetch_add_explicit(&translated_blocks, 1, memory_order_relaxed) +
        1;
    atomic_fetch_add_explicit(&translated_instructions, instruction_count,
                              memory_order_relaxed);
    update_max(&max_tb_instructions, instruction_count);

    struct catalog_instruction *catalog_insns = NULL;
    if (catalog_file && instruction_count != 0) {
        if (instruction_count <= SIZE_MAX / sizeof(*catalog_insns)) {
            catalog_insns = calloc(instruction_count, sizeof(*catalog_insns));
        }
        if (!catalog_insns) {
            atomic_fetch_add_explicit(&catalog_allocation_failures, 1,
                                      memory_order_relaxed);
            atomic_fetch_add_explicit(&catalog_dropped_blocks, 1,
                                      memory_order_relaxed);
        }
    }

    struct tb_mix_entry aggregate[MAX_DESCRIPTORS];
    size_t aggregate_count = 0;
    uint64_t overflow_count = 0;
    uint64_t decode_error_count = 0;

    for (size_t index = 0; index < instruction_count; ++index) {
        struct qemu_plugin_insn *insn = qemu_plugin_tb_get_insn(tb, index);
        size_t raw_size = qemu_plugin_insn_size(insn);
        uint32_t size = raw_size <= UINT32_MAX ? (uint32_t)raw_size : 0;
        char *disassembly = qemu_plugin_insn_disas(insn);
        char mnemonic[MAX_MNEMONIC_BYTES];
        bool mnemonic_complete = false;

        if (raw_size == 0 || raw_size > UINT32_MAX) {
            atomic_fetch_add_explicit(&invalid_instruction_sizes, 1,
                                      memory_order_relaxed);
            ++decode_error_count;
        }
        if (disassembly) {
            mnemonic_complete =
                extract_mnemonic(disassembly, mnemonic, sizeof(mnemonic));
            g_free(disassembly);
            if (!mnemonic_complete) {
                if (strcmp(mnemonic, "<invalid>") == 0) {
                    atomic_fetch_add_explicit(&disassembly_errors, 1,
                                              memory_order_relaxed);
                    ++decode_error_count;
                } else {
                    atomic_fetch_add_explicit(&mnemonic_truncations, 1,
                                              memory_order_relaxed);
                }
            }
        } else {
            snprintf(mnemonic, sizeof(mnemonic), "%s", "<invalid>");
            atomic_fetch_add_explicit(&disassembly_errors, 1,
                                      memory_order_relaxed);
            ++decode_error_count;
        }

        int descriptor_id = descriptor_for(mnemonic, size);
        if (descriptor_id < 0) {
            ++overflow_count;
            atomic_fetch_add_explicit(&descriptor_overflow_instructions, 1,
                                      memory_order_relaxed);
        } else {
            size_t aggregate_index;
            for (aggregate_index = 0; aggregate_index < aggregate_count;
                 ++aggregate_index) {
                if (aggregate[aggregate_index].descriptor_id ==
                    (uint16_t)descriptor_id) {
                    ++aggregate[aggregate_index].count;
                    break;
                }
            }
            if (aggregate_index == aggregate_count) {
                aggregate[aggregate_count].descriptor_id =
                    (uint16_t)descriptor_id;
                aggregate[aggregate_count].count = 1;
                ++aggregate_count;
            }
        }

        if (catalog_insns) {
            struct catalog_instruction *record = &catalog_insns[index];
            record->pc = qemu_plugin_insn_vaddr(insn);
            record->size = size;
            record->descriptor_id = descriptor_id;
            snprintf(record->mnemonic, sizeof(record->mnemonic), "%s",
                     mnemonic);
            size_t requested = raw_size < MAX_INSN_BYTES ? raw_size
                                                         : MAX_INSN_BYTES;
            size_t copied =
                qemu_plugin_insn_data(insn, record->bytes, requested);
            record->copied = copied <= UINT32_MAX ? (uint32_t)copied : 0;
            record->bytes_complete = copied == raw_size;
            if (!record->bytes_complete) {
                atomic_fetch_add_explicit(&instruction_data_errors, 1,
                                          memory_order_relaxed);
                ++decode_error_count;
            }
        }
    }

    if (overflow_count != 0) {
        atomic_fetch_add_explicit(&descriptor_overflow_blocks, 1,
                                  memory_order_relaxed);
    }
    for (size_t index = 0; index < aggregate_count; ++index) {
        qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
            tb, QEMU_PLUGIN_INLINE_ADD_U64,
            mix_entry(domain, aggregate[index].descriptor_id),
            aggregate[index].count);
    }
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64, block_entry(domain), 1);
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64, instruction_entry(domain),
        instruction_count);

    if (catalog_insns) {
        write_catalog_block(translation_index, translation_begin_ns, guest_pc,
                            tid, domain, catalog_insns, instruction_count,
                            overflow_count, decode_error_count);
        free(catalog_insns);
    } else if (catalog_file && instruction_count == 0) {
        write_catalog_block(translation_index, translation_begin_ns, guest_pc,
                            tid, domain, NULL, 0, overflow_count,
                            decode_error_count);
    }
}

static void vcpu_initialized(qemu_plugin_id_t id, unsigned int vcpu_index)
{
    (void)id;
    if (vcpu_index >= MAX_VCPUS) {
        atomic_fetch_add_explicit(&unsupported_vcpu_events, 1,
                                  memory_order_relaxed);
    }
}

static struct control_result read_control(void)
{
    struct control_result result = {
        .valid = false,
        .active = false,
        .error_number = 0,
        .error_kind = "malformed",
    };
    char buffer[32];
    int descriptor = open(control_path, O_RDONLY | O_CLOEXEC);

    if (descriptor < 0) {
        result.error_number = errno;
        result.error_kind = "open";
        return result;
    }
    ssize_t length = read(descriptor, buffer, sizeof(buffer));
    int read_error = errno;
    close(descriptor);
    if (length < 0) {
        result.error_number = read_error;
        result.error_kind = "read";
        return result;
    }
    if (length == (ssize_t)sizeof(buffer)) {
        return result;
    }
    size_t begin = 0;
    size_t end = (size_t)length;
    while (begin < end &&
           (buffer[begin] == ' ' || buffer[begin] == '\t' ||
            buffer[begin] == '\r' || buffer[begin] == '\n')) {
        ++begin;
    }
    while (end > begin &&
           (buffer[end - 1] == ' ' || buffer[end - 1] == '\t' ||
            buffer[end - 1] == '\r' || buffer[end - 1] == '\n')) {
        --end;
    }
    if (end - begin != 1 || (buffer[begin] != '0' && buffer[begin] != '1')) {
        return result;
    }
    result.valid = true;
    result.active = buffer[begin] == '1';
    result.error_kind = NULL;
    return result;
}

static void take_snapshot(struct counter_snapshot *snapshot)
{
    memset(snapshot, 0, sizeof(*snapshot));
    pthread_mutex_lock(&descriptor_mutex);
    snapshot->descriptor_count = descriptor_count;
    pthread_mutex_unlock(&descriptor_mutex);

    for (size_t domain = 0; domain < DOMAIN_COUNT; ++domain) {
        for (size_t id = 0; id < snapshot->descriptor_count; ++id) {
            snapshot->mix[domain][id] =
                qemu_plugin_u64_sum(mix_entry((enum execution_domain)domain,
                                              id));
        }
        snapshot->blocks[domain] = qemu_plugin_u64_sum(
            block_entry((enum execution_domain)domain));
        snapshot->instructions[domain] = qemu_plugin_u64_sum(
            instruction_entry((enum execution_domain)domain));
    }
    snapshot->translated_blocks = atomic_load_explicit(
        &translated_blocks, memory_order_relaxed);
    snapshot->translated_instructions = atomic_load_explicit(
        &translated_instructions, memory_order_relaxed);
    snapshot->max_tb_instructions = atomic_load_explicit(
        &max_tb_instructions, memory_order_relaxed);
}

static uint64_t counter_delta(uint64_t current, uint64_t previous,
                              bool *regressed)
{
    if (current < previous) {
        *regressed = true;
        atomic_fetch_add_explicit(&counter_regressions, 1,
                                  memory_order_relaxed);
        return 0;
    }
    return current - previous;
}

static void emit_descriptor(size_t id, bool mapped[MAX_DESCRIPTORS])
{
    struct instruction_descriptor descriptor;

    if (mapped[id]) {
        return;
    }
    pthread_mutex_lock(&descriptor_mutex);
    descriptor = descriptors[id];
    pthread_mutex_unlock(&descriptor_mutex);
    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-instruction-mix.v1\","
            "\"type\":\"descriptor\",\"monotonic_ns\":%" PRIu64
            ",\"id\":%zu,\"mnemonic\":",
            monotonic_ns(), id);
    json_string(output_file, descriptor.mnemonic);
    fprintf(output_file, ",\"size\":%u}", descriptor.size);
    finish_record(output_file, &output_write_errors);
    mapped[id] = true;
}

static void emit_control_error(const struct control_result *result)
{
    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-instruction-mix.v1\","
            "\"type\":\"control_error\",\"monotonic_ns\":%" PRIu64
            ",\"kind\":",
            monotonic_ns());
    json_string(output_file, result->error_kind);
    fprintf(output_file, ",\"errno\":%d}", result->error_number);
    finish_record(output_file, &output_write_errors);
}

static void emit_window_event(const char *type, uint64_t window_id,
                              bool detected_from_control)
{
    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-instruction-mix.v1\",\"type\":");
    json_string(output_file, type);
    fprintf(output_file,
            ",\"monotonic_ns\":%" PRIu64 ",\"window_id\":%" PRIu64
            ",\"detected_from_control\":%s}",
            monotonic_ns(), window_id,
            detected_from_control ? "true" : "false");
    finish_record(output_file, &output_write_errors);
}

static void emit_sample(const struct counter_snapshot *current,
                        const struct counter_snapshot *previous,
                        bool mapped[MAX_DESCRIPTORS], uint64_t window_id,
                        uint64_t epoch_index, const char *reason)
{
    uint64_t mix_delta[DOMAIN_COUNT][MAX_DESCRIPTORS];
    uint64_t mix_total[DOMAIN_COUNT] = {0};
    uint64_t block_delta[DOMAIN_COUNT];
    uint64_t instruction_delta[DOMAIN_COUNT];
    size_t count = current->descriptor_count > previous->descriptor_count
                       ? current->descriptor_count
                       : previous->descriptor_count;
    bool regressed = false;

    memset(mix_delta, 0, sizeof(mix_delta));
    for (size_t domain = 0; domain < DOMAIN_COUNT; ++domain) {
        for (size_t id = 0; id < count; ++id) {
            mix_delta[domain][id] =
                counter_delta(current->mix[domain][id],
                              previous->mix[domain][id], &regressed);
            mix_total[domain] += mix_delta[domain][id];
            if (mix_delta[domain][id] != 0) {
                emit_descriptor(id, mapped);
            }
        }
        block_delta[domain] =
            counter_delta(current->blocks[domain], previous->blocks[domain],
                          &regressed);
        instruction_delta[domain] = counter_delta(
            current->instructions[domain], previous->instructions[domain],
            &regressed);
    }
    uint64_t translated_block_delta = counter_delta(
        current->translated_blocks, previous->translated_blocks, &regressed);
    uint64_t translated_instruction_delta =
        counter_delta(current->translated_instructions,
                      previous->translated_instructions, &regressed);

    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-instruction-mix.v1\","
            "\"type\":\"sample\",\"monotonic_ns\":%" PRIu64
            ",\"window_id\":%" PRIu64 ",\"epoch\":%" PRIu64
            ",\"reason\":",
            monotonic_ns(), window_id, epoch_index);
    json_string(output_file, reason);
    fprintf(output_file,
            ",\"tb_delta\":{\"user\":%" PRIu64
            ",\"kernel\":%" PRIu64 "},\"instruction_delta\":{"
            "\"user\":%" PRIu64 ",\"kernel\":%" PRIu64
            "},\"mix_instruction_delta\":{\"user\":%" PRIu64
            ",\"kernel\":%" PRIu64 "},\"translated\":{"
            "\"tb\":%" PRIu64 ",\"instructions\":%" PRIu64
            ",\"tb_delta\":%" PRIu64 ",\"instruction_delta\":%" PRIu64
            ",\"max_tb_instructions\":%" PRIu64
            "},\"counter_regression\":%s,\"mix\":[",
            block_delta[DOMAIN_USER], block_delta[DOMAIN_KERNEL],
            instruction_delta[DOMAIN_USER], instruction_delta[DOMAIN_KERNEL],
            mix_total[DOMAIN_USER], mix_total[DOMAIN_KERNEL],
            current->translated_blocks, current->translated_instructions,
            translated_block_delta, translated_instruction_delta,
            current->max_tb_instructions, regressed ? "true" : "false");
    bool first = true;
    for (size_t id = 0; id < count; ++id) {
        if (mix_delta[DOMAIN_USER][id] == 0 &&
            mix_delta[DOMAIN_KERNEL][id] == 0) {
            continue;
        }
        if (!first) {
            fputc(',', output_file);
        }
        first = false;
        fprintf(output_file,
                "{\"id\":%zu,\"user\":%" PRIu64
                ",\"kernel\":%" PRIu64 "}",
                id, mix_delta[DOMAIN_USER][id],
                mix_delta[DOMAIN_KERNEL][id]);
    }
    fputs("]}", output_file);
    finish_record(output_file, &output_write_errors);
    atomic_fetch_add_explicit(&emitted_samples, 1, memory_order_relaxed);
}

static bool wait_for_next_epoch(void)
{
    struct timespec deadline;

    if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
        return false;
    }
    deadline.tv_sec += epoch_ms / 1000;
    deadline.tv_nsec += (long)(epoch_ms % 1000) * 1000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        ++deadline.tv_sec;
        deadline.tv_nsec -= 1000000000L;
    }

    pthread_mutex_lock(&sampler_mutex);
    while (!atomic_load_explicit(&stop_sampler, memory_order_acquire)) {
        int status = pthread_cond_timedwait(&sampler_condition, &sampler_mutex,
                                            &deadline);
        if (status == ETIMEDOUT) {
            break;
        }
        if (status != 0) {
            atomic_fetch_add_explicit(&sampler_wait_errors, 1,
                                      memory_order_relaxed);
            atomic_store_explicit(&stop_sampler, true, memory_order_release);
            pthread_mutex_unlock(&sampler_mutex);
            return false;
        }
    }
    bool keep_running =
        !atomic_load_explicit(&stop_sampler, memory_order_acquire);
    pthread_mutex_unlock(&sampler_mutex);
    return keep_running;
}

static void *sample_counters(void *userdata)
{
    (void)userdata;
    struct counter_snapshot previous;
    struct counter_snapshot current;
    bool mapped[MAX_DESCRIPTORS] = {false};
    bool active = false;
    uint64_t window_id = 0;
    uint64_t epoch_index = 0;

    memset(&previous, 0, sizeof(previous));
    for (;;) {
        if (atomic_load_explicit(&stop_sampler, memory_order_acquire)) {
            if (active) {
                take_snapshot(&current);
                emit_sample(&current, &previous, mapped, window_id,
                            ++epoch_index, "exit");
                emit_window_event("window_stop", window_id, false);
                atomic_fetch_add_explicit(&exit_stops, 1,
                                          memory_order_relaxed);
            }
            break;
        }

        struct control_result control = read_control();
        if (!control.valid) {
            atomic_fetch_add_explicit(&control_read_errors, 1,
                                      memory_order_relaxed);
            emit_control_error(&control);
        } else if (!active && control.active) {
            take_snapshot(&previous);
            active = true;
            ++window_id;
            epoch_index = 0;
            atomic_fetch_add_explicit(&start_detections, 1,
                                      memory_order_relaxed);
            atomic_fetch_add_explicit(&emitted_windows, 1,
                                      memory_order_relaxed);
            emit_window_event("window_start", window_id, true);
        } else if (active) {
            take_snapshot(&current);
            emit_sample(&current, &previous, mapped, window_id,
                        ++epoch_index, control.active ? "epoch" : "stop");
            previous = current;
            if (!control.active) {
                active = false;
                atomic_fetch_add_explicit(&stop_detections, 1,
                                          memory_order_relaxed);
                emit_window_event("window_stop", window_id, true);
            }
        }

        if (!wait_for_next_epoch()) {
            continue;
        }
    }
    return NULL;
}

static bool parse_epoch_ms(const char *value, unsigned int *result)
{
    char *end = NULL;
    errno = 0;
    unsigned long parsed = strtoul(value, &end, 10);

    if (errno != 0 || !value[0] || !end || *end || parsed < MIN_EPOCH_MS ||
        parsed > MAX_EPOCH_MS) {
        return false;
    }
    *result = (unsigned int)parsed;
    return true;
}

static void release_resources(void)
{
    if (scoreboard) {
        qemu_plugin_scoreboard_free(scoreboard);
        scoreboard = NULL;
    }
    if (output_file) {
        fclose(output_file);
        output_file = NULL;
    }
    if (catalog_file) {
        fclose(catalog_file);
        catalog_file = NULL;
    }
    free(catalog_buffer);
    catalog_buffer = NULL;
    riscv_seen_table_release(&seen_pcs);
    riscv_seen_table_release(&seen_fingerprints);
    free(output_path);
    output_path = NULL;
    free(control_path);
    control_path = NULL;
    free(catalog_path);
    catalog_path = NULL;
    if (sampler_condition_initialized) {
        pthread_cond_destroy(&sampler_condition);
        sampler_condition_initialized = false;
    }
}

static void write_quality(qemu_plugin_id_t id, void *userdata)
{
    (void)id;
    (void)userdata;
    atomic_store_explicit(&plugin_exiting, true, memory_order_release);
    atomic_store_explicit(&stop_sampler, true, memory_order_release);
    pthread_mutex_lock(&sampler_mutex);
    pthread_cond_broadcast(&sampler_condition);
    pthread_mutex_unlock(&sampler_mutex);
    if (sampler_started) {
        pthread_join(sampler_thread, NULL);
        sampler_started = false;
    }

    if (catalog_file) {
        pthread_mutex_lock(&catalog_mutex);
        flush_catalog();
        pthread_mutex_unlock(&catalog_mutex);
    }

    uint64_t starts =
        atomic_load_explicit(&start_detections, memory_order_relaxed);
    uint64_t stops =
        atomic_load_explicit(&stop_detections, memory_order_relaxed);
    uint64_t output_errors =
        atomic_load_explicit(&output_write_errors, memory_order_relaxed);
    uint64_t catalog_errors =
        atomic_load_explicit(&catalog_write_errors, memory_order_relaxed);
    uint64_t overflow = atomic_load_explicit(
        &descriptor_overflow_instructions, memory_order_relaxed);
    uint64_t control_errors =
        atomic_load_explicit(&control_read_errors, memory_order_relaxed);
    uint64_t regressions =
        atomic_load_explicit(&counter_regressions, memory_order_relaxed);
    uint64_t unsupported =
        atomic_load_explicit(&unsupported_vcpu_events, memory_order_relaxed);
    uint64_t data_errors =
        atomic_load_explicit(&instruction_data_errors, memory_order_relaxed);
    uint64_t decode_errors =
        atomic_load_explicit(&disassembly_errors, memory_order_relaxed) +
        atomic_load_explicit(&mnemonic_truncations, memory_order_relaxed) +
        atomic_load_explicit(&invalid_instruction_sizes, memory_order_relaxed);
    uint64_t catalog_drops =
        atomic_load_explicit(&catalog_dropped_blocks, memory_order_relaxed);
    uint64_t catalog_allocations = atomic_load_explicit(
        &catalog_allocation_failures, memory_order_relaxed);
    uint64_t late_drops =
        atomic_load_explicit(&late_translation_drops, memory_order_relaxed);
    uint64_t wait_errors =
        atomic_load_explicit(&sampler_wait_errors, memory_order_relaxed);
    bool complete = starts != 0 && stops != 0 && output_errors == 0 &&
                    catalog_errors == 0 && overflow == 0 &&
                    control_errors == 0 && regressions == 0 &&
                    unsupported == 0 && data_errors == 0 &&
                    decode_errors == 0 && late_drops == 0 &&
                    wait_errors == 0 &&
                    (!catalog_file ||
                     (catalog_drops == 0 && catalog_allocations == 0));

    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-instruction-mix.v1\","
            "\"type\":\"quality\",\"monotonic_ns\":%" PRIu64
            ",\"complete\":%s,\"configured_vcpus\":%u,"
            "\"max_supported_vcpus\":%u,\"descriptor_count\":%zu,"
            "\"descriptor_limit\":%u,\"translated_blocks\":%" PRIu64
            ",\"translated_instructions\":%" PRIu64
            ",\"max_tb_instructions\":%" PRIu64
            ",\"windows\":%" PRIu64 ",\"samples\":%" PRIu64
            ",\"start_detections\":%" PRIu64
            ",\"stop_detections\":%" PRIu64
            ",\"exit_stops\":%" PRIu64 ",\"errors\":{"
            "\"output_write\":%" PRIu64 ",\"control_read\":%" PRIu64
            ",\"counter_regression\":%" PRIu64
            ",\"descriptor_overflow_instructions\":%" PRIu64
            ",\"descriptor_overflow_blocks\":%" PRIu64
            ",\"disassembly\":%" PRIu64
            ",\"mnemonic_truncation\":%" PRIu64
            ",\"invalid_instruction_size\":%" PRIu64
            ",\"instruction_data\":%" PRIu64
            ",\"unsupported_vcpu\":%" PRIu64
            ",\"late_translation_drop\":%" PRIu64
            ",\"sampler_wait\":%" PRIu64
            "},\"catalog\":{\"enabled\":%s,\"records\":%" PRIu64
            ",\"write_errors\":%" PRIu64
            ",\"dropped_blocks\":%" PRIu64
            ",\"allocation_failures\":%" PRIu64
            ",\"duplicate_pc\":%" PRIu64
            ",\"duplicate_exact\":%" PRIu64
            ",\"tracking_drops\":%" PRIu64
            ",\"pc_seen_slots\":%zu,\"pc_seen_entries\":%zu"
            ",\"fingerprint_seen_slots\":%zu"
            ",\"fingerprint_seen_entries\":%zu"
            ",\"flushes\":%" PRIu64
            ",\"flush_records\":%u,\"buffer_bytes\":%u"
            ",\"flush_policy\":\"bounded-batch-v1\""
            ",\"tail_failure\":\"missing-final-quality-invalid\"}}",
            monotonic_ns(), complete ? "true" : "false", configured_vcpus,
            MAX_VCPUS, descriptor_count, MAX_DESCRIPTORS,
            atomic_load_explicit(&translated_blocks, memory_order_relaxed),
            atomic_load_explicit(&translated_instructions,
                                 memory_order_relaxed),
            atomic_load_explicit(&max_tb_instructions, memory_order_relaxed),
            atomic_load_explicit(&emitted_windows, memory_order_relaxed),
            atomic_load_explicit(&emitted_samples, memory_order_relaxed),
            starts, stops,
            atomic_load_explicit(&exit_stops, memory_order_relaxed),
            output_errors, control_errors, regressions, overflow,
            atomic_load_explicit(&descriptor_overflow_blocks,
                                 memory_order_relaxed),
            atomic_load_explicit(&disassembly_errors, memory_order_relaxed),
            atomic_load_explicit(&mnemonic_truncations, memory_order_relaxed),
            atomic_load_explicit(&invalid_instruction_sizes,
                                 memory_order_relaxed),
            data_errors, unsupported, late_drops, wait_errors,
            catalog_file ? "true" : "false",
            atomic_load_explicit(&catalog_records, memory_order_relaxed),
            catalog_errors,
            catalog_drops, catalog_allocations,
            atomic_load_explicit(&duplicate_pc_translations,
                                 memory_order_relaxed),
            atomic_load_explicit(&duplicate_exact_translations,
                                 memory_order_relaxed),
            atomic_load_explicit(&duplicate_tracking_drops,
                                 memory_order_relaxed),
            seen_pcs.capacity, seen_pcs.count,
            seen_fingerprints.capacity, seen_fingerprints.count,
            catalog_flushes, CATALOG_FLUSH_RECORDS, CATALOG_BUFFER_BYTES);
    finish_record(output_file, &output_write_errors);

    if (catalog_file) {
        pthread_mutex_lock(&catalog_mutex);
        fprintf(catalog_file,
                "{\"schema\":\"mygo.riscv-tb-catalog.v1\","
                "\"type\":\"quality\",\"monotonic_ns\":%" PRIu64
                ",\"translated_blocks\":%" PRIu64
                ",\"records\":%" PRIu64 ",\"write_errors\":%" PRIu64
                ",\"dropped_blocks\":%" PRIu64
                ",\"duplicate_pc\":%" PRIu64
                ",\"duplicate_exact\":%" PRIu64
                ",\"tracking_drops\":%" PRIu64
                ",\"pc_seen_slots\":%zu,\"pc_seen_entries\":%zu"
                ",\"fingerprint_seen_slots\":%zu"
                ",\"fingerprint_seen_entries\":%zu"
                ",\"flushes\":%" PRIu64
                ",\"flush_records\":%u,\"buffer_bytes\":%u"
                ",\"flush_policy\":\"bounded-batch-v1\""
                ",\"tail_failure\":\"missing-final-quality-invalid\"}",
                monotonic_ns(),
                atomic_load_explicit(&translated_blocks,
                                     memory_order_relaxed),
                atomic_load_explicit(&catalog_records, memory_order_relaxed),
                atomic_load_explicit(&catalog_write_errors,
                                     memory_order_relaxed),
                atomic_load_explicit(&catalog_dropped_blocks,
                                     memory_order_relaxed),
                atomic_load_explicit(&duplicate_pc_translations,
                                     memory_order_relaxed),
                atomic_load_explicit(&duplicate_exact_translations,
                                     memory_order_relaxed),
                atomic_load_explicit(&duplicate_tracking_drops,
                                     memory_order_relaxed),
                seen_pcs.capacity, seen_pcs.count,
                seen_fingerprints.capacity, seen_fingerprints.count,
                catalog_flushes, CATALOG_FLUSH_RECORDS,
                CATALOG_BUFFER_BYTES);
        finish_catalog_record(false, true);
        pthread_mutex_unlock(&catalog_mutex);
    }
    release_resources();
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info, int argc,
                                           char **argv)
{
    if (!info->system_emulation || strcmp(info->target_name, "riscv64") != 0 ||
        info->system.smp_vcpus <= 0 || info->system.smp_vcpus > MAX_VCPUS) {
        fprintf(stderr,
                "riscv instruction mix: requires riscv64 system emulation with "
                "1..%u vCPUs\n",
                MAX_VCPUS);
        return 1;
    }
    configured_vcpus = (unsigned int)info->system.smp_vcpus;
    snprintf(target_name, sizeof(target_name), "%s", info->target_name);

    for (int index = 0; index < argc; ++index) {
        if (strncmp(argv[index], "output=", 7) == 0 && !output_path &&
            argv[index][7]) {
            output_path = strdup(argv[index] + 7);
        } else if (strncmp(argv[index], "control=", 8) == 0 && !control_path &&
                   argv[index][8]) {
            control_path = strdup(argv[index] + 8);
        } else if (strncmp(argv[index], "catalog=", 8) == 0 && !catalog_path &&
                   argv[index][8]) {
            catalog_path = strdup(argv[index] + 8);
        } else if (strncmp(argv[index], "epoch-ms=", 9) == 0 &&
                   parse_epoch_ms(argv[index] + 9, &epoch_ms)) {
        } else {
            fprintf(stderr, "riscv instruction mix: invalid option: %s\n",
                    argv[index]);
            release_resources();
            return 1;
        }
    }
    if (!output_path || !control_path ||
        (catalog_path && strcmp(catalog_path, output_path) == 0) ||
        strcmp(control_path, output_path) == 0 ||
        (catalog_path && strcmp(control_path, catalog_path) == 0)) {
        fprintf(stderr,
                "riscv instruction mix: distinct output= and control= are "
                "required; catalog= must also be distinct\n");
        release_resources();
        return 1;
    }

    output_file = fopen(output_path, "w");
    if (!output_file) {
        fprintf(stderr, "riscv instruction mix: cannot open %s: %s\n",
                output_path, strerror(errno));
        release_resources();
        return 1;
    }
    setvbuf(output_file, NULL, _IOLBF, 0);
    if (catalog_path) {
        catalog_file = fopen(catalog_path, "w");
        catalog_buffer = malloc(CATALOG_BUFFER_BYTES);
        bool pc_table_ready = riscv_seen_table_init(
            &seen_pcs, CATALOG_SEEN_INITIAL_SLOTS);
        bool fingerprint_table_ready = riscv_seen_table_init(
            &seen_fingerprints, CATALOG_SEEN_INITIAL_SLOTS);
        bool catalog_buffer_ready =
            catalog_file && catalog_buffer &&
            setvbuf(catalog_file, catalog_buffer, _IOFBF,
                    CATALOG_BUFFER_BYTES) == 0;
        if (!catalog_buffer_ready || !pc_table_ready ||
            !fingerprint_table_ready) {
            fprintf(stderr,
                    "riscv instruction mix: cannot initialize catalog %s\n",
                    catalog_path);
            release_resources();
            return 1;
        }
    }
    scoreboard = qemu_plugin_scoreboard_new(sizeof(struct vcpu_counters));
    if (!scoreboard) {
        fprintf(stderr, "riscv instruction mix: cannot allocate scoreboard\n");
        release_resources();
        return 1;
    }

    pthread_condattr_t attributes;
    int condition_status = pthread_condattr_init(&attributes);
    bool attributes_initialized = condition_status == 0;
    if (condition_status == 0) {
        condition_status =
            pthread_condattr_setclock(&attributes, CLOCK_MONOTONIC);
    }
    if (condition_status == 0) {
        condition_status = pthread_cond_init(&sampler_condition, &attributes);
    }
    if (condition_status != 0) {
        if (attributes_initialized) {
            pthread_condattr_destroy(&attributes);
        }
        fprintf(stderr,
                "riscv instruction mix: cannot initialize sampler condition\n");
        release_resources();
        return 1;
    }
    if (attributes_initialized) {
        pthread_condattr_destroy(&attributes);
    }
    sampler_condition_initialized = true;

    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-instruction-mix.v1\","
            "\"type\":\"header\",\"monotonic_ns\":%" PRIu64
            ",\"target\":",
            monotonic_ns());
    json_string(output_file, target_name);
    fprintf(output_file,
            ",\"configured_vcpus\":%u,\"max_supported_vcpus\":%u,"
            "\"epoch_ms\":%u,\"descriptor_limit\":%u,"
            "\"mode_rule\":\"guest_pc_bit_63\",\"catalog_enabled\":%s}",
            configured_vcpus, MAX_VCPUS, epoch_ms, MAX_DESCRIPTORS,
            catalog_file ? "true" : "false");
    if (!finish_record(output_file, &output_write_errors)) {
        release_resources();
        return 1;
    }
    if (catalog_file) {
        fprintf(catalog_file,
                "{\"schema\":\"mygo.riscv-tb-catalog.v1\","
                "\"type\":\"header\",\"monotonic_ns\":%" PRIu64
                ",\"target\":",
                monotonic_ns());
        json_string(catalog_file, target_name);
        fprintf(catalog_file,
                ",\"configured_vcpus\":%u,\"seen_slots\":%u"
                ",\"flush_records\":%u,\"buffer_bytes\":%u"
                ",\"flush_policy\":\"bounded-batch-v1\""
                ",\"tail_failure\":\"missing-final-quality-invalid\"}",
                configured_vcpus, CATALOG_SEEN_INITIAL_SLOTS,
                CATALOG_FLUSH_RECORDS, CATALOG_BUFFER_BYTES);
        if (!finish_catalog_record(false, true)) {
            release_resources();
            return 1;
        }
    }

    if (pthread_create(&sampler_thread, NULL, sample_counters, NULL) != 0) {
        fprintf(stderr, "riscv instruction mix: cannot start sampler thread\n");
        release_resources();
        return 1;
    }
    sampler_started = true;
    qemu_plugin_register_vcpu_init_cb(id, vcpu_initialized);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
    qemu_plugin_register_atexit_cb(id, write_quality, NULL);
    return 0;
}
