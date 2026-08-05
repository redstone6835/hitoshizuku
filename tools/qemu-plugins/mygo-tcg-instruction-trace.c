#define _GNU_SOURCE
#include <qemu-plugin.h>

#include <errno.h>
#include <glib.h>
#include <inttypes.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

enum {
    MAX_INSN_BYTES = 16,
    DEFAULT_MAX_INSTRUCTIONS = 100000,
    MAX_INSTRUCTIONS_LIMIT = 10000000,
};

struct vcpu_state {
    uint64_t active;
};

struct instruction_descriptor {
    uint64_t pc;
    uint8_t size;
    uint8_t bytes[MAX_INSN_BYTES];
    char *disassembly;
};

struct trace_record {
    const struct instruction_descriptor *instruction;
    unsigned int cpu;
};

static struct qemu_plugin_scoreboard *state;
static GPtrArray *descriptors;
static struct trace_record *records;
static FILE *output;
static char *output_path;
static char target_name[64];
static uint64_t start_pc;
static uint64_t stop_pc;
static uint64_t max_instructions = DEFAULT_MAX_INSTRUCTIONS;
static atomic_uint_fast64_t attempted_instructions;
static atomic_uint_fast64_t dropped_instructions;
static atomic_uint_fast64_t start_events;
static atomic_uint_fast64_t stop_events;
static atomic_uint_fast64_t translation_failures;

static qemu_plugin_u64 active_entry(void)
{
    return (qemu_plugin_u64){.score = state, .offset = offsetof(struct vcpu_state, active)};
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

static void free_descriptor(void *value)
{
    struct instruction_descriptor *descriptor = value;
    if (!descriptor) {
        return;
    }
    g_free(descriptor->disassembly);
    free(descriptor);
}

static void start_window(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    qemu_plugin_u64_set(active_entry(), vcpu_index, 1);
    atomic_fetch_add_explicit(&start_events, 1, memory_order_relaxed);
}

static void stop_window(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    qemu_plugin_u64_set(active_entry(), vcpu_index, 0);
    atomic_fetch_add_explicit(&stop_events, 1, memory_order_relaxed);
}

static void record_instruction(unsigned int vcpu_index, void *userdata)
{
    const struct instruction_descriptor *instruction = userdata;
    uint64_t sequence =
        atomic_fetch_add_explicit(&attempted_instructions, 1, memory_order_relaxed);
    if (sequence >= max_instructions) {
        atomic_fetch_add_explicit(&dropped_instructions, 1, memory_order_relaxed);
        return;
    }
    records[sequence] = (struct trace_record){
        .instruction = instruction,
        .cpu = vcpu_index,
    };
}

static void translate_block(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    (void)id;
    uint64_t pc = qemu_plugin_tb_vaddr(tb);
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

    size_t count = qemu_plugin_tb_n_insns(tb);
    for (size_t index = 0; index < count; ++index) {
        struct qemu_plugin_insn *insn = qemu_plugin_tb_get_insn(tb, index);
        size_t size = qemu_plugin_insn_size(insn);
        if (size == 0 || size > MAX_INSN_BYTES) {
            atomic_fetch_add_explicit(&translation_failures, 1, memory_order_relaxed);
            continue;
        }
        struct instruction_descriptor *descriptor = calloc(1, sizeof(*descriptor));
        if (!descriptor) {
            atomic_fetch_add_explicit(&translation_failures, 1, memory_order_relaxed);
            continue;
        }
        descriptor->pc = qemu_plugin_insn_vaddr(insn);
        descriptor->size = (uint8_t)size;
        descriptor->disassembly = qemu_plugin_insn_disas(insn);
        if (qemu_plugin_insn_data(insn, descriptor->bytes, size) != size) {
            free_descriptor(descriptor);
            atomic_fetch_add_explicit(&translation_failures, 1, memory_order_relaxed);
            continue;
        }
        if (!descriptor->disassembly || !descriptor->disassembly[0]) {
            free_descriptor(descriptor);
            atomic_fetch_add_explicit(&translation_failures, 1, memory_order_relaxed);
            continue;
        }
        g_ptr_array_add(descriptors, descriptor);
        qemu_plugin_register_vcpu_insn_exec_cond_cb(
            insn, record_instruction, QEMU_PLUGIN_CB_NO_REGS, QEMU_PLUGIN_COND_NE,
            active_entry(), 0, descriptor);
    }
}

static void release_resources(void)
{
    if (output) {
        fclose(output);
        output = NULL;
    }
    if (state) {
        qemu_plugin_scoreboard_free(state);
        state = NULL;
    }
    if (descriptors) {
        g_ptr_array_free(descriptors, TRUE);
        descriptors = NULL;
    }
    free(records);
    records = NULL;
    free(output_path);
    output_path = NULL;
}

static void write_footer(qemu_plugin_id_t id, void *userdata)
{
    (void)id;
    (void)userdata;
    uint64_t attempted =
        atomic_load_explicit(&attempted_instructions, memory_order_relaxed);
    uint64_t dropped =
        atomic_load_explicit(&dropped_instructions, memory_order_relaxed);
    uint64_t translated_failures =
        atomic_load_explicit(&translation_failures, memory_order_relaxed);
    uint64_t active_at_exit = qemu_plugin_u64_get(active_entry(), 0);
    uint64_t recorded = attempted - dropped;
    for (uint64_t sequence = 0; sequence < recorded; ++sequence) {
        const struct trace_record *record = &records[sequence];
        const struct instruction_descriptor *instruction = record->instruction;
        fprintf(output, "INSN sequence=%" PRIu64 " cpu=%u pc=0x%" PRIx64
                        " size=%u bytes=",
                sequence, record->cpu, instruction->pc, instruction->size);
        for (uint8_t index = 0; index < instruction->size; ++index) {
            fprintf(output, "%02x", instruction->bytes[index]);
        }
        fputs(" disas_hex=", output);
        for (const unsigned char *cursor =
                 (const unsigned char *)instruction->disassembly;
             *cursor; ++cursor) {
            fprintf(output, "%02x", *cursor);
        }
        fputc('\n', output);
    }
    fprintf(output,
            "TRACE_DONE instructions=%" PRIu64 " dropped=%" PRIu64
            " translation_failures=%" PRIu64 " start_events=%" PRIu64
            " stop_events=%" PRIu64 " active_at_exit=%" PRIu64 "\n",
            recorded, dropped, translated_failures,
            atomic_load_explicit(&start_events, memory_order_relaxed),
            atomic_load_explicit(&stop_events, memory_order_relaxed), active_at_exit);
    fflush(output);
    release_resources();
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info,
                                           int argc, char **argv)
{
    if (!info->system_emulation || info->system.smp_vcpus != 1) {
        fprintf(stderr, "mygo instruction trace: exactly one vCPU is required\n");
        return 1;
    }
    bool have_start = false;
    bool have_stop = false;
    snprintf(target_name, sizeof(target_name), "%s", info->target_name);
    for (int index = 0; index < argc; ++index) {
        if (strncmp(argv[index], "output=", 7) == 0 && !output_path) {
            output_path = strdup(argv[index] + 7);
            if (!output_path) {
                return 1;
            }
        } else if (strncmp(argv[index], "start_pc=", 9) == 0 && !have_start &&
                   parse_u64(argv[index] + 9, &start_pc)) {
            have_start = true;
        } else if (strncmp(argv[index], "stop_pc=", 8) == 0 && !have_stop &&
                   parse_u64(argv[index] + 8, &stop_pc)) {
            have_stop = true;
        } else if (strncmp(argv[index], "max_instructions=", 17) == 0 &&
                   parse_u64(argv[index] + 17, &max_instructions) &&
                   max_instructions != 0 &&
                   max_instructions <= MAX_INSTRUCTIONS_LIMIT) {
        } else {
            fprintf(stderr, "mygo instruction trace: invalid option: %s\n", argv[index]);
            release_resources();
            return 1;
        }
    }
    if (!output_path || !have_start || !have_stop || start_pc == stop_pc) {
        fprintf(stderr,
                "mygo instruction trace: output and distinct start/stop PCs are required\n");
        release_resources();
        return 1;
    }

    state = qemu_plugin_scoreboard_new(sizeof(struct vcpu_state));
    descriptors = g_ptr_array_new_with_free_func(free_descriptor);
    records = calloc((size_t)max_instructions, sizeof(*records));
    output = fopen(output_path, "w");
    if (!state || !descriptors || !records || !output) {
        fprintf(stderr, "mygo instruction trace: resource allocation failed\n");
        release_resources();
        return 1;
    }
    fprintf(output,
            "MYGO_INSN_TRACE version=1 target=%s configured_vcpus=1 start_pc=0x%" PRIx64
            " stop_pc=0x%" PRIx64 " max_instructions=%" PRIu64 "\n",
            target_name, start_pc, stop_pc, max_instructions);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
    qemu_plugin_register_atexit_cb(id, write_footer, NULL);
    return 0;
}
