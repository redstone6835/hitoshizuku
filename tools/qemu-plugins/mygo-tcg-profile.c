#define _GNU_SOURCE
#include <qemu-plugin.h>

#include <errno.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

enum {
    DEFAULT_TABLE_BITS = 23,
    MIN_TABLE_BITS = 12,
    MAX_TABLE_BITS = 23,
    TABLE_PROBES = 128,
    MAX_REPORT_ROWS = 4096,
};

struct hot_slot {
    atomic_uint_fast64_t key;
};

struct hot_count {
    uint64_t blocks;
    uint64_t instructions;
};

struct vcpu_counters {
    uint64_t blocks;
    uint64_t instructions;
    uint64_t active;
};

struct report_row {
    uint64_t pc;
    uint64_t blocks;
    uint64_t instructions;
};

static struct hot_slot *hot_table;
static struct qemu_plugin_scoreboard *counters;
static size_t table_slots;
static size_t table_mask;
static size_t counter_bytes_per_vcpu;
static atomic_uint_fast64_t translated_blocks;
static atomic_uint_fast64_t occupied_slots;
static atomic_uint_fast64_t dropped_blocks;
static atomic_uint_fast64_t collision_probes;
static atomic_uint_fast64_t max_probe;
static atomic_uint_fast64_t start_events;
static atomic_uint_fast64_t stop_events;
static char *output_path;
static char target_name[64];
static int configured_vcpus;
static unsigned int configured_table_bits;
static uint64_t profile_start_pc;
static uint64_t profile_stop_pc;
static bool windowed;

static uint64_t mix64(uint64_t value)
{
    value ^= value >> 33;
    value *= UINT64_C(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value *= UINT64_C(0xc4ceb9fe1a85ec53);
    return value ^ (value >> 33);
}

static qemu_plugin_u64 counter_entry(size_t offset)
{
    return (qemu_plugin_u64){.score = counters, .offset = offset};
}

static size_t hot_offset(size_t index)
{
    return sizeof(struct vcpu_counters) + index * sizeof(struct hot_count);
}

static void update_max_probe(uint64_t value)
{
    uint64_t observed = atomic_load_explicit(&max_probe, memory_order_relaxed);
    while (observed < value &&
           !atomic_compare_exchange_weak_explicit(&max_probe, &observed, value,
                                                  memory_order_relaxed,
                                                  memory_order_relaxed)) {
    }
}

static void release_resources(void)
{
    if (counters) {
        qemu_plugin_scoreboard_free(counters);
        counters = NULL;
    }
    free(hot_table);
    hot_table = NULL;
    free(output_path);
    output_path = NULL;
}

static bool configure_table(unsigned int table_bits)
{
    if (table_bits < MIN_TABLE_BITS || table_bits > MAX_TABLE_BITS) {
        return false;
    }
    table_slots = (size_t)1 << table_bits;
    if (table_slots >
        (SIZE_MAX - sizeof(struct vcpu_counters)) / sizeof(struct hot_count)) {
        return false;
    }
    counter_bytes_per_vcpu =
        sizeof(struct vcpu_counters) + table_slots * sizeof(struct hot_count);
    hot_table = calloc(table_slots, sizeof(*hot_table));
    if (!hot_table) {
        return false;
    }
    counters = qemu_plugin_scoreboard_new(counter_bytes_per_vcpu);
    if (!counters) {
        release_resources();
        return false;
    }
    table_mask = table_slots - 1;
    configured_table_bits = table_bits;
    return true;
}

static bool parse_table_bits(const char *value, unsigned int *result)
{
    char *end = NULL;
    errno = 0;
    unsigned long parsed = strtoul(value, &end, 10);
    if (errno != 0 || !value[0] || !end || *end || parsed > UINT32_MAX ||
        parsed < MIN_TABLE_BITS || parsed > MAX_TABLE_BITS) {
        return false;
    }
    *result = (unsigned int)parsed;
    return true;
}

static bool parse_u64(const char *value, uint64_t *result)
{
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(value, &end, 0);
    if (errno != 0 || !value[0] || !end || *end) {
        return false;
    }
    *result = (uint64_t)parsed;
    return true;
}

static void start_window(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    qemu_plugin_u64_set(counter_entry(offsetof(struct vcpu_counters, active)),
                        vcpu_index, 1);
    atomic_fetch_add_explicit(&start_events, 1, memory_order_relaxed);
}

static void stop_window(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    qemu_plugin_u64_set(counter_entry(offsetof(struct vcpu_counters, active)),
                        vcpu_index, 0);
    atomic_fetch_add_explicit(&stop_events, 1, memory_order_relaxed);
}

static void record_windowed_block(unsigned int vcpu_index, void *userdata)
{
    uintptr_t packed = (uintptr_t)userdata - 1;
    size_t index = (size_t)(packed >> 16);
    uint64_t instruction_count = packed & UINT64_C(0xffff);
    size_t offset = hot_offset(index);

    qemu_plugin_u64_add(counter_entry(offset + offsetof(struct hot_count, blocks)),
                        vcpu_index, 1);
    qemu_plugin_u64_add(
        counter_entry(offset + offsetof(struct hot_count, instructions)),
        vcpu_index, instruction_count);
    qemu_plugin_u64_add(counter_entry(offsetof(struct vcpu_counters, blocks)),
                        vcpu_index, 1);
    qemu_plugin_u64_add(counter_entry(offsetof(struct vcpu_counters, instructions)),
                        vcpu_index, instruction_count);
}

static void translate_block(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    (void)id;
    uint64_t pc = qemu_plugin_tb_vaddr(tb);
    uint64_t key = pc + 1;
    if (key == 0) {
        atomic_fetch_add_explicit(&dropped_blocks, 1, memory_order_relaxed);
        return;
    }

    size_t index = mix64(pc) & table_mask;
    size_t probes = 0;
    bool found = false;
    for (size_t probe = 0; probe < TABLE_PROBES; ++probe) {
        probes = probe + 1;
        uint64_t observed = atomic_load_explicit(&hot_table[index].key, memory_order_acquire);
        if (observed == key) {
            found = true;
            break;
        }
        if (observed == 0 && atomic_compare_exchange_strong_explicit(
                                 &hot_table[index].key, &observed, key,
                                 memory_order_acq_rel, memory_order_acquire)) {
            atomic_fetch_add_explicit(&occupied_slots, 1, memory_order_relaxed);
            found = true;
            break;
        }
        index = (index + 1) & table_mask;
    }
    atomic_fetch_add_explicit(&collision_probes, probes, memory_order_relaxed);
    update_max_probe(probes);
    if (!found) {
        atomic_fetch_add_explicit(&dropped_blocks, 1, memory_order_relaxed);
        return;
    }

    size_t offset = hot_offset(index);
    uint64_t instruction_count = qemu_plugin_tb_n_insns(tb);
    atomic_fetch_add_explicit(&translated_blocks, 1, memory_order_relaxed);
    if (windowed) {
        if (pc == profile_start_pc) {
            qemu_plugin_register_vcpu_tb_exec_cb(tb, start_window,
                                                 QEMU_PLUGIN_CB_NO_REGS, NULL);
            return;
        }
        if (pc == profile_stop_pc) {
            qemu_plugin_register_vcpu_tb_exec_cb(tb, stop_window,
                                                 QEMU_PLUGIN_CB_NO_REGS, NULL);
            return;
        }
        if (instruction_count > UINT16_MAX) {
            atomic_fetch_add_explicit(&dropped_blocks, 1, memory_order_relaxed);
            return;
        }
        uintptr_t packed = ((uintptr_t)index << 16) | (uintptr_t)instruction_count;
        qemu_plugin_register_vcpu_tb_exec_cond_cb(
            tb, record_windowed_block, QEMU_PLUGIN_CB_NO_REGS, QEMU_PLUGIN_COND_NE,
            counter_entry(offsetof(struct vcpu_counters, active)), 0,
            (void *)(packed + 1));
        return;
    }
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64,
        counter_entry(offset + offsetof(struct hot_count, blocks)), 1);
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64,
        counter_entry(offset + offsetof(struct hot_count, instructions)), instruction_count);
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64,
        counter_entry(offsetof(struct vcpu_counters, blocks)), 1);
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64,
        counter_entry(offsetof(struct vcpu_counters, instructions)), instruction_count);
}

static bool row_is_lower_priority(const struct report_row *left,
                                  const struct report_row *right)
{
    if (left->instructions != right->instructions) {
        return left->instructions < right->instructions;
    }
    if (left->blocks != right->blocks) {
        return left->blocks < right->blocks;
    }
    return left->pc > right->pc;
}

static void heap_sift_up(struct report_row *rows, size_t index)
{
    while (index != 0) {
        size_t parent = (index - 1) / 2;
        if (!row_is_lower_priority(&rows[index], &rows[parent])) {
            break;
        }
        struct report_row value = rows[index];
        rows[index] = rows[parent];
        rows[parent] = value;
        index = parent;
    }
}

static void heap_sift_down(struct report_row *rows, size_t count, size_t index)
{
    for (;;) {
        size_t left = index * 2 + 1;
        if (left >= count) {
            return;
        }
        size_t child = left;
        size_t right = left + 1;
        if (right < count && row_is_lower_priority(&rows[right], &rows[left])) {
            child = right;
        }
        if (!row_is_lower_priority(&rows[child], &rows[index])) {
            return;
        }
        struct report_row value = rows[index];
        rows[index] = rows[child];
        rows[child] = value;
        index = child;
    }
}

static int compare_rows(const void *left_raw, const void *right_raw)
{
    const struct report_row *left = left_raw;
    const struct report_row *right = right_raw;
    if (left->instructions < right->instructions) {
        return 1;
    }
    if (left->instructions > right->instructions) {
        return -1;
    }
    if (left->blocks < right->blocks) {
        return 1;
    }
    if (left->blocks > right->blocks) {
        return -1;
    }
    return left->pc < right->pc ? -1 : left->pc != right->pc;
}

static void write_report(qemu_plugin_id_t id, void *userdata)
{
    (void)id;
    (void)userdata;
    FILE *output = output_path ? fopen(output_path, "w") : stderr;
    if (!output) {
        output = stderr;
    }

    uint64_t total_blocks = 0;
    uint64_t total_instructions = 0;
    uint64_t active_at_exit = 0;
    unsigned int active_vcpus = 0;
    for (unsigned int cpu = 0; cpu < (unsigned int)configured_vcpus; ++cpu) {
        uint64_t blocks = qemu_plugin_u64_get(
            counter_entry(offsetof(struct vcpu_counters, blocks)), cpu);
        uint64_t instructions = qemu_plugin_u64_get(
            counter_entry(offsetof(struct vcpu_counters, instructions)), cpu);
        total_blocks += blocks;
        total_instructions += instructions;
        active_at_exit += qemu_plugin_u64_get(
            counter_entry(offsetof(struct vcpu_counters, active)), cpu);
        if (blocks || instructions) {
            ++active_vcpus;
        }
    }

    struct report_row *rows = calloc(MAX_REPORT_ROWS, sizeof(*rows));
    size_t count = 0;
    uint64_t omitted_hotspots = 0;
    if (!rows) {
        atomic_fetch_add_explicit(&dropped_blocks, 1, memory_order_relaxed);
    } else {
        for (size_t index = 0; index < table_slots; ++index) {
            size_t offset = hot_offset(index);
            uint64_t blocks = qemu_plugin_u64_sum(
                counter_entry(offset + offsetof(struct hot_count, blocks)));
            if (!blocks) {
                continue;
            }
            struct report_row row = {
                .pc = atomic_load_explicit(&hot_table[index].key, memory_order_relaxed) - 1,
                .blocks = blocks,
                .instructions = qemu_plugin_u64_sum(
                    counter_entry(offset + offsetof(struct hot_count, instructions))),
            };
            if (count < MAX_REPORT_ROWS) {
                rows[count] = row;
                heap_sift_up(rows, count);
                ++count;
            } else {
                /* 报告截断同样是不完整采样，必须通过 dropped 使消费者失败。 */
                ++omitted_hotspots;
                if (row_is_lower_priority(&rows[0], &row)) {
                    rows[0] = row;
                    heap_sift_down(rows, count, 0);
                }
            }
        }
        qsort(rows, count, sizeof(*rows), compare_rows);
    }

    uint64_t dropped =
        atomic_load_explicit(&dropped_blocks, memory_order_relaxed) + omitted_hotspots;
    fprintf(output,
            "MYGO_TCG_PROFILE version=2 target=%s configured_vcpus=%d active_vcpus=%u "
            "table_bits=%u table_slots=%zu table_probes=%u counter_bytes_per_vcpu=%zu "
            "translated_blocks=%llu occupied_slots=%llu dropped=%llu collision_probes=%llu "
            "max_probe=%llu total_blocks=%llu total_instructions=%llu reported_hotspots=%zu "
            "windowed=%u start_pc=0x%llx stop_pc=0x%llx start_events=%llu "
            "stop_events=%llu active_at_exit=%llu\n",
            target_name, configured_vcpus, active_vcpus, configured_table_bits, table_slots,
            TABLE_PROBES, counter_bytes_per_vcpu,
            (unsigned long long)atomic_load_explicit(&translated_blocks, memory_order_relaxed),
            (unsigned long long)atomic_load_explicit(&occupied_slots, memory_order_relaxed),
            (unsigned long long)dropped,
            (unsigned long long)atomic_load_explicit(&collision_probes, memory_order_relaxed),
            (unsigned long long)atomic_load_explicit(&max_probe, memory_order_relaxed),
            (unsigned long long)total_blocks, (unsigned long long)total_instructions, count,
            windowed ? 1U : 0U, (unsigned long long)profile_start_pc,
            (unsigned long long)profile_stop_pc,
            (unsigned long long)atomic_load_explicit(&start_events, memory_order_relaxed),
            (unsigned long long)atomic_load_explicit(&stop_events, memory_order_relaxed),
            (unsigned long long)active_at_exit);
    for (unsigned int cpu = 0; cpu < (unsigned int)configured_vcpus; ++cpu) {
        uint64_t blocks = qemu_plugin_u64_get(
            counter_entry(offsetof(struct vcpu_counters, blocks)), cpu);
        uint64_t instructions = qemu_plugin_u64_get(
            counter_entry(offsetof(struct vcpu_counters, instructions)), cpu);
        if (blocks || instructions) {
            fprintf(output, "VCPU cpu=%u blocks=%llu instructions=%llu\n", cpu,
                    (unsigned long long)blocks, (unsigned long long)instructions);
        }
    }
    for (size_t index = 0; index < count; ++index) {
        fprintf(output, "HOT rank=%zu pc=0x%llx blocks=%llu instructions=%llu\n", index + 1,
                (unsigned long long)rows[index].pc,
                (unsigned long long)rows[index].blocks,
                (unsigned long long)rows[index].instructions);
    }
    free(rows);
    if (output != stderr) {
        fclose(output);
    }
    release_resources();
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info,
                                           int argc, char **argv)
{
    if (!info->system_emulation || info->system.smp_vcpus <= 0) {
        return 1;
    }
    unsigned int table_bits = DEFAULT_TABLE_BITS;
    bool have_start_pc = false;
    bool have_stop_pc = false;
    snprintf(target_name, sizeof(target_name), "%s", info->target_name);
    configured_vcpus = info->system.smp_vcpus;
    for (int index = 0; index < argc; ++index) {
        if (strncmp(argv[index], "output=", 7) == 0 && !output_path) {
            output_path = strdup(argv[index] + 7);
            if (!output_path) {
                return 1;
            }
        } else if (strncmp(argv[index], "table_bits=", 11) == 0 &&
                   parse_table_bits(argv[index] + 11, &table_bits)) {
        } else if (strncmp(argv[index], "start_pc=", 9) == 0 && !have_start_pc &&
                   parse_u64(argv[index] + 9, &profile_start_pc)) {
            have_start_pc = true;
        } else if (strncmp(argv[index], "stop_pc=", 8) == 0 && !have_stop_pc &&
                   parse_u64(argv[index] + 8, &profile_stop_pc)) {
            have_stop_pc = true;
        } else {
            fprintf(stderr, "mygo tcg profile: invalid option: %s\n", argv[index]);
            release_resources();
            return 1;
        }
    }
    if (have_start_pc != have_stop_pc ||
        (have_start_pc && profile_start_pc == profile_stop_pc)) {
        fprintf(stderr, "mygo tcg profile: start_pc/stop_pc must be a distinct pair\n");
        release_resources();
        return 1;
    }
    windowed = have_start_pc;
    if (!configure_table(table_bits)) {
        fprintf(stderr, "mygo tcg profile: cannot allocate %u-bit hot table\n", table_bits);
        release_resources();
        return 1;
    }
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
    qemu_plugin_register_atexit_cb(id, write_report, NULL);
    return 0;
}
