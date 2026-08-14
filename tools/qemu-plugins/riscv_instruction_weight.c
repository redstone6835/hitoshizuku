#define _GNU_SOURCE
#include <qemu-plugin.h>

#include <errno.h>
#include <glib.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

enum {
    MAX_INSN_BYTES = 16,
};

enum plugin_mode {
    MODE_VALIDATION,
    MODE_TIMING,
};

struct vcpu_state {
    uint64_t active;
};

struct instruction_descriptor {
    uint8_t size;
    uint8_t bytes[MAX_INSN_BYTES];
    char *lookup_key;
    char *mnemonic;
    uint64_t generation;
    uint64_t count;
};

static struct qemu_plugin_scoreboard *scoreboard;
static GHashTable *descriptor_by_encoding;
static GPtrArray *descriptors;
static GPtrArray *touched;
static FILE *output;
static char *output_path;
static uint64_t start_pc;
static uint64_t stop_pc;
static uint64_t trap_entry_pc;
static uint64_t user_min_pc;
static uint64_t user_max_pc;
static uint64_t window_generation;
static uint64_t emitted_windows;
static uint64_t warmup_windows = 1;
static uint64_t window_start_thread_cpu_ns;
static uint64_t window_start_monotonic_ns;
static uint64_t window_translations;
static uint64_t window_scoped_translations;
static uint64_t window_guest_trap_entries;
static uint64_t translations_while_active;
static uint64_t start_events;
static uint64_t stop_events;
static uint64_t nested_start_events;
static uint64_t inactive_stop_events;
static uint64_t translation_failures;
static uint64_t timer_failures;
static enum plugin_mode mode = MODE_VALIDATION;
static bool have_user_range;
static bool window_active;
static bool window_thread_timer_valid;
static bool window_monotonic_timer_valid;

static const char *mode_name(void)
{
    return mode == MODE_TIMING ? "timing" : "validation";
}

static const char *count_scope_name(void)
{
    if (mode == MODE_TIMING) {
        return "unavailable";
    }
    return have_user_range ? "user-pc-range" : "all-guest-pcs";
}

static bool pc_in_validation_scope(uint64_t pc)
{
    return !have_user_range || (pc >= user_min_pc && pc < user_max_pc);
}

static qemu_plugin_u64 active_entry(void)
{
    return (qemu_plugin_u64){
        .score = scoreboard,
        .offset = offsetof(struct vcpu_state, active),
    };
}

static bool parse_u64(const char *text, uint64_t *result)
{
    char *end = NULL;

    errno = 0;
    unsigned long long value = strtoull(text, &end, 0);
    if (errno != 0 || !text[0] || !end || *end) {
        return false;
    }
    *result = (uint64_t)value;
    return true;
}

static bool parse_mode(const char *text, enum plugin_mode *result)
{
    if (strcmp(text, "validation") == 0) {
        *result = MODE_VALIDATION;
        return true;
    }
    if (strcmp(text, "timing") == 0) {
        *result = MODE_TIMING;
        return true;
    }
    return false;
}

static uint64_t clock_ns(clockid_t clock_id, bool *valid)
{
    struct timespec value;

    if (clock_gettime(clock_id, &value) != 0) {
        ++timer_failures;
        *valid = false;
        return 0;
    }
    *valid = true;
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) +
           (uint64_t)value.tv_nsec;
}

static char *first_token(const char *disassembly)
{
    if (!disassembly) {
        return g_strdup("unknown");
    }
    while (*disassembly == ' ' || *disassembly == '\t') {
        ++disassembly;
    }
    size_t length = strcspn(disassembly, " \t");
    if (length == 0) {
        return g_strdup("unknown");
    }
    return g_strndup(disassembly, length);
}

static char *encoding_key(const uint8_t *bytes, size_t size)
{
    static const char digits[] = "0123456789abcdef";
    char *key = g_malloc(4 + size * 2);

    if (!key) {
        return NULL;
    }
    int prefix = snprintf(key, 4, "%zu:", size);
    if (prefix < 0 || prefix >= 4) {
        g_free(key);
        return NULL;
    }
    for (size_t index = 0; index < size; ++index) {
        key[prefix + index * 2] = digits[bytes[index] >> 4];
        key[prefix + index * 2 + 1] = digits[bytes[index] & 15];
    }
    key[prefix + size * 2] = '\0';
    return key;
}

static void free_descriptor(void *raw)
{
    struct instruction_descriptor *descriptor = raw;

    if (!descriptor) {
        return;
    }
    g_free(descriptor->lookup_key);
    g_free(descriptor->mnemonic);
    g_free(descriptor);
}

static struct instruction_descriptor *intern_instruction(
    struct qemu_plugin_insn *instruction)
{
    size_t size = qemu_plugin_insn_size(instruction);
    uint8_t bytes[MAX_INSN_BYTES];

    if (size == 0 || size > sizeof(bytes) ||
        qemu_plugin_insn_data(instruction, bytes, size) != size) {
        ++translation_failures;
        return NULL;
    }
    char *key = encoding_key(bytes, size);
    if (!key) {
        ++translation_failures;
        return NULL;
    }
    struct instruction_descriptor *descriptor =
        g_hash_table_lookup(descriptor_by_encoding, key);
    if (descriptor) {
        g_free(key);
        return descriptor;
    }

    descriptor = g_new0(struct instruction_descriptor, 1);
    char *disassembly = qemu_plugin_insn_disas(instruction);
    if (!descriptor) {
        g_free(disassembly);
        g_free(key);
        ++translation_failures;
        return NULL;
    }
    descriptor->size = (uint8_t)size;
    memcpy(descriptor->bytes, bytes, size);
    descriptor->lookup_key = key;
    descriptor->mnemonic = first_token(disassembly);
    g_free(disassembly);
    if (!descriptor->mnemonic) {
        free_descriptor(descriptor);
        ++translation_failures;
        return NULL;
    }
    g_hash_table_insert(descriptor_by_encoding, descriptor->lookup_key, descriptor);
    g_ptr_array_add(descriptors, descriptor);
    return descriptor;
}

static void count_instruction(unsigned int vcpu_index, void *userdata)
{
    (void)vcpu_index;
    struct instruction_descriptor *descriptor = userdata;

    if (descriptor->generation != window_generation) {
        descriptor->generation = window_generation;
        descriptor->count = 0;
        g_ptr_array_add(touched, descriptor);
    }
    ++descriptor->count;
}

static void count_guest_trap(unsigned int vcpu_index, void *userdata)
{
    (void)vcpu_index;
    (void)userdata;
    ++window_guest_trap_entries;
}

static void write_json_string(FILE *stream, const char *text)
{
    fputc('"', stream);
    for (const unsigned char *cursor = (const unsigned char *)text; *cursor;
         ++cursor) {
        switch (*cursor) {
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
            if (*cursor < 0x20) {
                fprintf(stream, "\\u%04x", *cursor);
            } else {
                fputc(*cursor, stream);
            }
            break;
        }
    }
    fputc('"', stream);
}

static void start_window(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    ++start_events;
    if (window_active) {
        ++nested_start_events;
        return;
    }
    window_active = true;
    window_translations = 0;
    window_scoped_translations = 0;
    window_guest_trap_entries = 0;
    if (mode == MODE_VALIDATION) {
        ++window_generation;
        g_ptr_array_set_size(touched, 0);
    }
    window_start_monotonic_ns =
        clock_ns(CLOCK_MONOTONIC_RAW, &window_monotonic_timer_valid);
    window_start_thread_cpu_ns =
        clock_ns(CLOCK_THREAD_CPUTIME_ID, &window_thread_timer_valid);
    qemu_plugin_u64_set(active_entry(), vcpu_index, 1);
}

static void stop_window(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    ++stop_events;
    if (!window_active) {
        ++inactive_stop_events;
        return;
    }
    qemu_plugin_u64_set(active_entry(), vcpu_index, 0);
    bool monotonic_valid;
    bool thread_valid;
    uint64_t stop_thread_cpu_ns = clock_ns(CLOCK_THREAD_CPUTIME_ID, &thread_valid);
    uint64_t stop_monotonic_ns = clock_ns(CLOCK_MONOTONIC_RAW, &monotonic_valid);
    uint64_t total = 0;
    bool thread_delta_valid =
        window_thread_timer_valid && thread_valid &&
        stop_thread_cpu_ns >= window_start_thread_cpu_ns;
    bool monotonic_delta_valid =
        window_monotonic_timer_valid && monotonic_valid &&
        stop_monotonic_ns >= window_start_monotonic_ns;
    if (mode == MODE_VALIDATION) {
        for (guint index = 0; index < touched->len; ++index) {
            const struct instruction_descriptor *descriptor = touched->pdata[index];
            total += descriptor->count;
        }
    }

    if (warmup_windows != 0) {
        --warmup_windows;
        window_active = false;
        return;
    }
    ++emitted_windows;

    fprintf(output,
            "{\"schema\":\"mygo.riscv-instruction-weight-window.v2\","
            "\"type\":\"window\",\"sequence\":%" PRIu64
            ",\"mode\":\"%s\",\"cpu_scope\":\"full-vcpu-thread\","
            "\"count_scope\":\"%s\",\"plugin_thread_cpu_ns\":",
            emitted_windows, mode_name(), count_scope_name());
    if (thread_delta_valid) {
        fprintf(output, "%" PRIu64,
                stop_thread_cpu_ns - window_start_thread_cpu_ns);
    } else {
        fputs("null", output);
    }
    fputs(",\"plugin_monotonic_ns\":", output);
    if (monotonic_delta_valid) {
        fprintf(output, "%" PRIu64, stop_monotonic_ns - window_start_monotonic_ns);
    } else {
        fputs("null", output);
    }
    fprintf(output, ",\"translations_during_window\":%" PRIu64,
            window_translations);
    fputs(",\"scoped_translations_during_window\":", output);
    if (have_user_range) {
        fprintf(output, "%" PRIu64, window_scoped_translations);
    } else {
        fputs("null", output);
    }
    fprintf(output, ",\"guest_trap_entries_during_window\":%" PRIu64,
            window_guest_trap_entries);
    if (mode == MODE_TIMING) {
        fputs(",\"counts_available\":false,\"instruction_count\":null,"
              "\"counts\":null}\n",
              output);
    } else {
        fprintf(output,
                ",\"counts_available\":true,\"instruction_count\":%" PRIu64
                ",\"counts\":[",
                total);
        for (guint index = 0; index < touched->len; ++index) {
            const struct instruction_descriptor *descriptor = touched->pdata[index];
            if (index != 0) {
                fputc(',', output);
            }
            fputs("{\"size\":", output);
            fprintf(output, "%u,\"bytes\":\"", descriptor->size);
            for (uint8_t byte = 0; byte < descriptor->size; ++byte) {
                fprintf(output, "%02x", descriptor->bytes[byte]);
            }
            fputs("\",\"mnemonic\":", output);
            write_json_string(output, descriptor->mnemonic);
            fprintf(output, ",\"count\":%" PRIu64 "}", descriptor->count);
        }
        fputs("]}\n", output);
    }
    fflush(output);
    window_active = false;
}

static void translate_block(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    (void)id;
    uint64_t pc = qemu_plugin_tb_vaddr(tb);

    if (window_active) {
        ++window_translations;
        ++translations_while_active;
        if (have_user_range && pc_in_validation_scope(pc)) {
            ++window_scoped_translations;
        }
    }

    if (pc == start_pc) {
        qemu_plugin_register_vcpu_tb_exec_cb(tb, start_window,
                                             QEMU_PLUGIN_CB_NO_REGS, NULL);
        return;
    }
    if (pc == stop_pc) {
        qemu_plugin_register_vcpu_tb_exec_cb(tb, stop_window,
                                             QEMU_PLUGIN_CB_NO_REGS, NULL);
        return;
    }
    if (pc == trap_entry_pc) {
        qemu_plugin_register_vcpu_tb_exec_cond_cb(
            tb, count_guest_trap, QEMU_PLUGIN_CB_NO_REGS,
            QEMU_PLUGIN_COND_NE, active_entry(), 0, NULL);
    }
    if (mode == MODE_TIMING) {
        return;
    }
    size_t instruction_count = qemu_plugin_tb_n_insns(tb);
    for (size_t index = 0; index < instruction_count; ++index) {
        struct qemu_plugin_insn *instruction = qemu_plugin_tb_get_insn(tb, index);
        if (!pc_in_validation_scope(qemu_plugin_insn_vaddr(instruction))) {
            continue;
        }
        struct instruction_descriptor *descriptor = intern_instruction(instruction);
        if (!descriptor) {
            continue;
        }
        qemu_plugin_register_vcpu_insn_exec_cond_cb(
            instruction, count_instruction, QEMU_PLUGIN_CB_NO_REGS,
            QEMU_PLUGIN_COND_NE, active_entry(), 0, descriptor);
    }
}

static void release_resources(void)
{
    if (output) {
        fclose(output);
        output = NULL;
    }
    if (scoreboard) {
        qemu_plugin_scoreboard_free(scoreboard);
        scoreboard = NULL;
    }
    if (descriptor_by_encoding) {
        g_hash_table_destroy(descriptor_by_encoding);
        descriptor_by_encoding = NULL;
    }
    if (descriptors) {
        g_ptr_array_free(descriptors, TRUE);
        descriptors = NULL;
    }
    if (touched) {
        g_ptr_array_free(touched, TRUE);
        touched = NULL;
    }
    g_free(output_path);
    output_path = NULL;
}

static void plugin_exit(qemu_plugin_id_t id, void *userdata)
{
    (void)id;
    (void)userdata;
    fprintf(output,
            "{\"schema\":\"mygo.riscv-instruction-weight-window.v2\","
            "\"type\":\"footer\",\"mode\":\"%s\","
            "\"cpu_scope\":\"full-vcpu-thread\",\"count_scope\":\"%s\","
            "\"counts_available\":%s,\"windows\":%" PRIu64
            ",\"start_events\":%" PRIu64 ",\"stop_events\":%" PRIu64
            ",\"nested_starts\":%" PRIu64 ",\"inactive_stops\":%" PRIu64
            ",\"translation_failures\":%" PRIu64
            ",\"timer_failures\":%" PRIu64
            ",\"translations_while_active\":%" PRIu64
            ",\"legacy_bit63_domain_heuristic\":false"
            ",\"active_at_exit\":%s}\n",
            mode_name(), count_scope_name(),
            mode == MODE_VALIDATION ? "true" : "false", emitted_windows,
            start_events, stop_events, nested_start_events, inactive_stop_events,
            translation_failures, timer_failures, translations_while_active,
            window_active ? "true" : "false");
    fflush(output);
    release_resources();
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info,
                                           int argc, char **argv)
{
    bool have_start = false;
    bool have_stop = false;
    bool have_trap_entry = false;
    bool have_mode = false;
    bool have_user_min = false;
    bool have_user_max = false;

    if (!info->system_emulation || info->system.smp_vcpus != 1 ||
        strcmp(info->target_name, "riscv64") != 0) {
        fprintf(stderr,
                "riscv instruction weight: riscv64 system emulation with one vCPU is required\n");
        return 1;
    }
    for (int index = 0; index < argc; ++index) {
        if (strncmp(argv[index], "output=", 7) == 0 && !output_path) {
            output_path = g_strdup(argv[index] + 7);
        } else if (strncmp(argv[index], "start_pc=", 9) == 0 && !have_start &&
                   parse_u64(argv[index] + 9, &start_pc)) {
            have_start = true;
        } else if (strncmp(argv[index], "stop_pc=", 8) == 0 && !have_stop &&
                   parse_u64(argv[index] + 8, &stop_pc)) {
            have_stop = true;
        } else if (strncmp(argv[index], "trap_entry_pc=", 14) == 0 &&
                   !have_trap_entry &&
                   parse_u64(argv[index] + 14, &trap_entry_pc)) {
            have_trap_entry = true;
        } else if (strncmp(argv[index], "mode=", 5) == 0 && !have_mode &&
                   parse_mode(argv[index] + 5, &mode)) {
            have_mode = true;
        } else if (strncmp(argv[index], "user_min_pc=", 12) == 0 &&
                   !have_user_min &&
                   parse_u64(argv[index] + 12, &user_min_pc)) {
            have_user_min = true;
        } else if (strncmp(argv[index], "user_max_pc=", 12) == 0 &&
                   !have_user_max &&
                   parse_u64(argv[index] + 12, &user_max_pc)) {
            have_user_max = true;
        } else if (strncmp(argv[index], "warmup_windows=", 15) == 0 &&
                   parse_u64(argv[index] + 15, &warmup_windows) &&
                   warmup_windows <= 1000) {
        } else {
            fprintf(stderr, "riscv instruction weight: invalid option: %s\n",
                    argv[index]);
            release_resources();
            return 1;
        }
    }
    if (!output_path || !output_path[0] || !have_start || !have_stop ||
        !have_trap_entry || start_pc == stop_pc || trap_entry_pc == 0) {
        fprintf(stderr,
                "riscv instruction weight: output, distinct start/stop PCs, and trap entry PC are required\n");
        release_resources();
        return 1;
    }
    if (have_user_min != have_user_max ||
        (have_user_min && user_min_pc >= user_max_pc)) {
        fprintf(stderr,
                "riscv instruction weight: user_min_pc/user_max_pc must form a non-empty half-open range\n");
        release_resources();
        return 1;
    }
    have_user_range = have_user_min;
    scoreboard = qemu_plugin_scoreboard_new(sizeof(struct vcpu_state));
    if (mode == MODE_VALIDATION) {
        descriptor_by_encoding = g_hash_table_new(g_str_hash, g_str_equal);
        descriptors = g_ptr_array_new_with_free_func(free_descriptor);
        touched = g_ptr_array_new();
    }
    output = fopen(output_path, "w");
    if (!output || !scoreboard ||
        (mode == MODE_VALIDATION &&
         (!descriptor_by_encoding || !descriptors || !touched))) {
        fprintf(stderr, "riscv instruction weight: resource allocation failed\n");
        release_resources();
        return 1;
    }
    setvbuf(output, NULL, _IOLBF, 0);
    fprintf(output,
            "{\"schema\":\"mygo.riscv-instruction-weight-window.v2\","
            "\"type\":\"header\",\"target\":\"riscv64\",\"mode\":\"%s\","
            "\"cpu_scope\":\"full-vcpu-thread\",\"count_scope\":\"%s\","
            "\"counts_available\":%s,\"configured_vcpus\":1,"
            "\"start_pc\":\"0x%" PRIx64
            "\",\"stop_pc\":\"0x%" PRIx64
            "\",\"trap_entry_pc\":\"0x%" PRIx64
            "\",\"warmup_windows\":%" PRIu64
            ",\"primary_clock\":\"CLOCK_THREAD_CPUTIME_ID\","
            "\"secondary_clock\":\"CLOCK_MONOTONIC_RAW\","
            "\"translation_accounting\":\"tb-translations-while-window-active\","
            "\"range_semantics\":\"min-inclusive,max-exclusive\","
            "\"legacy_bit63_domain_heuristic\":false,\"user_min_pc\":",
            mode_name(), count_scope_name(),
            mode == MODE_VALIDATION ? "true" : "false", start_pc, stop_pc,
            trap_entry_pc, warmup_windows);
    if (have_user_range) {
        fprintf(output, "\"0x%" PRIx64 "\"", user_min_pc);
    } else {
        fputs("null", output);
    }
    fputs(",\"user_max_pc\":", output);
    if (have_user_range) {
        fprintf(output, "\"0x%" PRIx64 "\"", user_max_pc);
    } else {
        fputs("null", output);
    }
    fputs("}\n", output);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
    qemu_plugin_register_atexit_cb(id, plugin_exit, NULL);
    return 0;
}
