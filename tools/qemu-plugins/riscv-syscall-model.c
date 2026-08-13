// SPDX-License-Identifier: GPL-2.0-or-later
/* 按系统调用聚合 RISC-V 内核 TB 指令，供离线成本模型使用。 */

#define _GNU_SOURCE
#include <qemu-plugin.h>

#include <errno.h>
#include <glib.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

enum {
    MAX_VCPUS = 8,
    MAX_SYSCALLS = 512,
    MAX_DESCRIPTORS = 512,
    DESCRIPTOR_HASH_SLOTS = 1024,
    MAX_MNEMONIC_BYTES = 96,
    MAX_INSN_BYTES = 32,
};

struct raw_encoding {
    struct raw_encoding *next;
    uint16_t descriptor_id;
    uint32_t size;
    uint8_t bytes[MAX_INSN_BYTES];
};

struct instruction_descriptor {
    struct raw_encoding *encodings;
    uint64_t encoding_count;
    uint32_t size;
    char mnemonic[MAX_MNEMONIC_BYTES];
};

struct tb_descriptor_count {
    uint16_t descriptor_id;
    uint64_t count;
};

struct tb_profile {
    struct tb_profile *next;
    uint64_t instructions;
    uint64_t unattributed;
    uint32_t pair_count;
    struct tb_descriptor_count pairs[];
};

struct scoreboard_state {
    uint64_t active_syscall;
};

struct vcpu_state {
    struct qemu_plugin_register *a0;
    struct qemu_plugin_register *a1;
    struct qemu_plugin_register *a2;
    GByteArray *register_buffer;
    uint64_t *mix;
    uint64_t entries[MAX_SYSCALLS];
    uint64_t exits[MAX_SYSCALLS];
    uint64_t blocks[MAX_SYSCALLS];
    uint64_t instructions[MAX_SYSCALLS];
    uint64_t unattributed[MAX_SYSCALLS];
    uint64_t current_session;
    uint64_t current_task;
    int32_t active_nr;
    bool current_running;
    uint64_t enter_markers;
    uint64_t exit_markers;
    uint64_t switch_markers;
    uint64_t register_read_errors;
    uint64_t invalid_syscall_numbers;
    uint64_t invalid_switch_values;
    uint64_t duplicate_enters;
    uint64_t unmatched_exits;
    uint64_t exit_nr_mismatches;
    uint64_t exit_task_mismatches;
    uint64_t switch_out_mismatches;
    uint64_t active_state_mismatches;
    uint64_t counter_saturations;
};

struct task_key {
    uint64_t session;
    uint64_t task;
};

static struct qemu_plugin_scoreboard *scoreboard;
static struct vcpu_state *vcpu_states;
static unsigned int configured_vcpus;

static struct instruction_descriptor descriptors[MAX_DESCRIPTORS];
static uint16_t descriptor_hash[DESCRIPTOR_HASH_SLOTS];
static size_t descriptor_count;
static GHashTable *encoding_table;
static pthread_mutex_t descriptor_mutex = PTHREAD_MUTEX_INITIALIZER;

static struct tb_profile *profiles;
static pthread_mutex_t profile_mutex = PTHREAD_MUTEX_INITIALIZER;

static GHashTable *task_bindings;
static pthread_mutex_t task_mutex = PTHREAD_MUTEX_INITIALIZER;

static char *output_path;
static FILE *output_file;
static uint64_t enter_pc;
static uint64_t exit_pc;
static uint64_t switch_pc;

static atomic_uint_fast64_t unsupported_vcpu_events;
static atomic_uint_fast64_t register_lookup_errors;
static atomic_uint_fast64_t descriptor_overflow_instructions;
static atomic_uint_fast64_t descriptor_overflow_blocks;
static atomic_uint_fast64_t disassembly_errors;
static atomic_uint_fast64_t mnemonic_truncations;
static atomic_uint_fast64_t invalid_instruction_sizes;
static atomic_uint_fast64_t instruction_data_errors;
static atomic_uint_fast64_t encoding_allocation_errors;
static atomic_uint_fast64_t task_allocation_errors;
static atomic_uint_fast64_t profile_allocation_errors;
static atomic_uint_fast64_t dropped_profile_blocks;
static atomic_uint_fast64_t dropped_profile_instructions;
static atomic_uint_fast64_t translated_kernel_blocks;
static atomic_uint_fast64_t translated_kernel_instructions;
static atomic_uint_fast64_t translated_marker_blocks;
static atomic_uint_fast64_t output_write_errors;

static qemu_plugin_u64 active_syscall_entry(void)
{
    return (qemu_plugin_u64){
        .score = scoreboard,
        .offset = offsetof(struct scoreboard_state, active_syscall),
    };
}

static uint64_t fnv1a_bytes(uint64_t hash, const void *raw, size_t size)
{
    const uint8_t *bytes = raw;

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

static guint task_key_hash(gconstpointer raw)
{
    const struct task_key *key = raw;
    uint64_t value = key->session ^ (key->task + UINT64_C(0x9e3779b97f4a7c15) +
                                     (key->session << 6) +
                                     (key->session >> 2));

    value ^= value >> 33;
    value *= UINT64_C(0xff51afd7ed558ccd);
    value ^= value >> 33;
    return (guint)(value ^ (value >> 32));
}

static gboolean task_key_equal(gconstpointer left_raw, gconstpointer right_raw)
{
    const struct task_key *left = left_raw;
    const struct task_key *right = right_raw;

    return left->session == right->session && left->task == right->task;
}

static guint raw_encoding_hash(gconstpointer raw)
{
    const struct raw_encoding *encoding = raw;
    uint64_t hash = fnv1a_bytes(UINT64_C(1469598103934665603),
                                &encoding->descriptor_id,
                                sizeof(encoding->descriptor_id));

    hash = fnv1a_bytes(hash, &encoding->size, sizeof(encoding->size));
    hash = fnv1a_bytes(hash, encoding->bytes, encoding->size);
    return (guint)(hash ^ (hash >> 32));
}

static gboolean raw_encoding_equal(gconstpointer left_raw,
                                   gconstpointer right_raw)
{
    const struct raw_encoding *left = left_raw;
    const struct raw_encoding *right = right_raw;

    return left->descriptor_id == right->descriptor_id &&
           left->size == right->size &&
           memcmp(left->bytes, right->bytes, left->size) == 0;
}

static bool parse_u64(const char *text, uint64_t *result)
{
    char *end = NULL;
    unsigned long long value;

    errno = 0;
    value = strtoull(text, &end, 0);
    if (errno != 0 || !text[0] || end == text || *end != '\0') {
        return false;
    }
    *result = (uint64_t)value;
    return true;
}

static void add_saturating(uint64_t *destination, uint64_t value,
                           uint64_t *saturations)
{
    uint64_t sum;

    if (__builtin_add_overflow(*destination, value, &sum)) {
        *destination = UINT64_MAX;
        ++*saturations;
    } else {
        *destination = sum;
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

static bool extract_mnemonic(const char *disassembly, char *destination,
                             size_t capacity)
{
    size_t length = 0;

    if (!disassembly) {
        snprintf(destination, capacity, "%s", "<invalid>");
        return false;
    }
    while (*disassembly == ' ' || *disassembly == '\t') {
        ++disassembly;
    }
    while (disassembly[length] && disassembly[length] != ' ' &&
           disassembly[length] != '\t') {
        ++length;
    }
    if (length == 0) {
        snprintf(destination, capacity, "%s", "<invalid>");
        return false;
    }
    if (length >= capacity) {
        memcpy(destination, disassembly, capacity - 1);
        destination[capacity - 1] = '\0';
        return false;
    }
    memcpy(destination, disassembly, length);
    destination[length] = '\0';
    return true;
}

static int find_descriptor_locked(const char *mnemonic, uint32_t size,
                                  bool create)
{
    size_t slot = descriptor_key_hash(mnemonic, size) &
                  (DESCRIPTOR_HASH_SLOTS - 1);

    for (size_t probe = 0; probe < DESCRIPTOR_HASH_SLOTS; ++probe) {
        uint16_t encoded_id = descriptor_hash[slot];

        if (encoded_id == 0) {
            if (!create || descriptor_count == MAX_DESCRIPTORS) {
                return -1;
            }
            size_t id = descriptor_count++;
            struct instruction_descriptor *descriptor = &descriptors[id];

            descriptor->size = size;
            snprintf(descriptor->mnemonic, sizeof(descriptor->mnemonic), "%s",
                     mnemonic);
            descriptor_hash[slot] = (uint16_t)(id + 1);
            return (int)id;
        }
        size_t id = (size_t)encoded_id - 1;
        if (descriptors[id].size == size &&
            strcmp(descriptors[id].mnemonic, mnemonic) == 0) {
            return (int)id;
        }
        slot = (slot + 1) & (DESCRIPTOR_HASH_SLOTS - 1);
    }
    return -1;
}

static void record_encoding_locked(uint16_t descriptor_id,
                                   struct instruction_descriptor *descriptor,
                                   const uint8_t *bytes, uint32_t size)
{
    struct raw_encoding lookup = {
        .descriptor_id = descriptor_id,
        .size = size,
    };

    memcpy(lookup.bytes, bytes, size);
    if (g_hash_table_contains(encoding_table, &lookup)) {
        return;
    }

    struct raw_encoding *encoding = malloc(sizeof(*encoding));
    if (!encoding) {
        atomic_fetch_add_explicit(&encoding_allocation_errors, 1,
                                  memory_order_relaxed);
        return;
    }
    encoding->descriptor_id = descriptor_id;
    encoding->size = size;
    memcpy(encoding->bytes, bytes, size);
    encoding->next = descriptor->encodings;
    descriptor->encodings = encoding;
    g_hash_table_add(encoding_table, encoding);
    ++descriptor->encoding_count;
}

static int intern_instruction(struct qemu_plugin_insn *instruction)
{
    size_t raw_size = qemu_plugin_insn_size(instruction);
    uint32_t descriptor_size =
        raw_size <= UINT32_MAX ? (uint32_t)raw_size : 0;
    uint8_t bytes[MAX_INSN_BYTES];
    bool bytes_valid = false;
    char mnemonic[MAX_MNEMONIC_BYTES];
    char *disassembly = qemu_plugin_insn_disas(instruction);
    bool mnemonic_complete =
        extract_mnemonic(disassembly, mnemonic, sizeof(mnemonic));

    g_free(disassembly);
    if (!mnemonic_complete) {
        if (strcmp(mnemonic, "<invalid>") == 0) {
            atomic_fetch_add_explicit(&disassembly_errors, 1,
                                      memory_order_relaxed);
        } else {
            atomic_fetch_add_explicit(&mnemonic_truncations, 1,
                                      memory_order_relaxed);
        }
    }
    if (raw_size == 0 || raw_size > UINT32_MAX) {
        atomic_fetch_add_explicit(&invalid_instruction_sizes, 1,
                                  memory_order_relaxed);
    } else if (raw_size > sizeof(bytes)) {
        atomic_fetch_add_explicit(&instruction_data_errors, 1,
                                  memory_order_relaxed);
    } else if (qemu_plugin_insn_data(instruction, bytes, raw_size) == raw_size) {
        bytes_valid = true;
    } else {
        atomic_fetch_add_explicit(&instruction_data_errors, 1,
                                  memory_order_relaxed);
    }

    pthread_mutex_lock(&descriptor_mutex);
    int id = find_descriptor_locked(mnemonic, descriptor_size, true);
    if (id >= 0 && bytes_valid) {
        record_encoding_locked((uint16_t)id, &descriptors[id], bytes,
                               descriptor_size);
    }
    pthread_mutex_unlock(&descriptor_mutex);
    if (id < 0) {
        atomic_fetch_add_explicit(&descriptor_overflow_instructions, 1,
                                  memory_order_relaxed);
    }
    return id;
}

static uint64_t load_target_u64(const uint8_t *bytes)
{
    uint64_t value = 0;

    for (unsigned int index = 0; index < sizeof(value); ++index) {
        value |= (uint64_t)bytes[index] << (index * 8);
    }
    return value;
}

static bool read_register_u64(struct qemu_plugin_register *handle,
                              GByteArray *buffer, uint64_t *value)
{
    int length;

    if (!handle || !buffer) {
        return false;
    }
    g_byte_array_set_size(buffer, 0);
    length = qemu_plugin_read_register(handle, buffer);
    if (length != (int)sizeof(uint64_t) || buffer->len < sizeof(uint64_t)) {
        return false;
    }
    *value = load_target_u64(buffer->data);
    return true;
}

static bool read_marker_arguments(struct vcpu_state *state, uint64_t *a0,
                                  uint64_t *a1, uint64_t *a2)
{
    if (read_register_u64(state->a0, state->register_buffer, a0) &&
        read_register_u64(state->a1, state->register_buffer, a1) &&
        read_register_u64(state->a2, state->register_buffer, a2)) {
        return true;
    }
    ++state->register_read_errors;
    return false;
}

static void set_active_syscall(struct vcpu_state *state,
                               unsigned int vcpu_index, int32_t nr)
{
    state->active_nr = nr;
    qemu_plugin_u64_set(active_syscall_entry(), vcpu_index,
                        nr >= 0 ? (uint64_t)nr + 1 : 0);
}

static bool lookup_binding_locked(uint64_t session, uint64_t task,
                                  uint32_t *nr)
{
    struct task_key lookup = {.session = session, .task = task};
    gpointer value = g_hash_table_lookup(task_bindings, &lookup);

    if (!value) {
        return false;
    }
    *nr = GPOINTER_TO_UINT(value) - 1;
    return true;
}

static bool install_binding_locked(uint64_t session, uint64_t task,
                                   uint32_t nr)
{
    struct task_key *key = g_try_new(struct task_key, 1);

    if (!key) {
        atomic_fetch_add_explicit(&task_allocation_errors, 1,
                                  memory_order_relaxed);
        return false;
    }
    key->session = session;
    key->task = task;
    g_hash_table_replace(task_bindings, key, GUINT_TO_POINTER(nr + 1));
    return true;
}

static void syscall_enter(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    if (vcpu_index >= configured_vcpus) {
        atomic_fetch_add_explicit(&unsupported_vcpu_events, 1,
                                  memory_order_relaxed);
        return;
    }
    struct vcpu_state *state = &vcpu_states[vcpu_index];
    uint64_t session;
    uint64_t task;
    uint64_t raw_nr;

    ++state->enter_markers;
    if (!read_marker_arguments(state, &session, &task, &raw_nr)) {
        return;
    }
    if (raw_nr >= MAX_SYSCALLS) {
        ++state->invalid_syscall_numbers;
        set_active_syscall(state, vcpu_index, -1);
        return;
    }
    uint32_t nr = (uint32_t)raw_nr;
    add_saturating(&state->entries[nr], 1, &state->counter_saturations);

    pthread_mutex_lock(&task_mutex);
    uint32_t previous_nr;
    if (lookup_binding_locked(session, task, &previous_nr)) {
        ++state->duplicate_enters;
    }
    bool installed = install_binding_locked(session, task, nr);
    pthread_mutex_unlock(&task_mutex);

    state->current_session = session;
    state->current_task = task;
    state->current_running = true;
    set_active_syscall(state, vcpu_index, installed ? (int32_t)nr : -1);
}

static void syscall_exit(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    if (vcpu_index >= configured_vcpus) {
        atomic_fetch_add_explicit(&unsupported_vcpu_events, 1,
                                  memory_order_relaxed);
        return;
    }
    struct vcpu_state *state = &vcpu_states[vcpu_index];
    uint64_t session;
    uint64_t task;
    uint64_t raw_nr;

    ++state->exit_markers;
    if (!read_marker_arguments(state, &session, &task, &raw_nr)) {
        return;
    }
    if (raw_nr >= MAX_SYSCALLS) {
        ++state->invalid_syscall_numbers;
        set_active_syscall(state, vcpu_index, -1);
        return;
    }
    uint32_t nr = (uint32_t)raw_nr;
    add_saturating(&state->exits[nr], 1, &state->counter_saturations);

    pthread_mutex_lock(&task_mutex);
    uint32_t bound_nr;
    if (!lookup_binding_locked(session, task, &bound_nr)) {
        ++state->unmatched_exits;
    } else {
        if (bound_nr != nr) {
            ++state->exit_nr_mismatches;
        }
        struct task_key lookup = {.session = session, .task = task};
        g_hash_table_remove(task_bindings, &lookup);
    }
    pthread_mutex_unlock(&task_mutex);

    if (!state->current_running || state->current_session != session ||
        state->current_task != task) {
        ++state->exit_task_mismatches;
    }
    state->current_session = session;
    state->current_task = task;
    state->current_running = true;
    set_active_syscall(state, vcpu_index, -1);
}

static void task_switch(unsigned int vcpu_index, void *userdata)
{
    (void)userdata;
    if (vcpu_index >= configured_vcpus) {
        atomic_fetch_add_explicit(&unsupported_vcpu_events, 1,
                                  memory_order_relaxed);
        return;
    }
    struct vcpu_state *state = &vcpu_states[vcpu_index];
    uint64_t session;
    uint64_t task;
    uint64_t running;

    ++state->switch_markers;
    if (!read_marker_arguments(state, &session, &task, &running)) {
        return;
    }
    if (running > 1) {
        ++state->invalid_switch_values;
        return;
    }
    if (running == 0) {
        /*
         * 新任务第一次被调度时不会从 switch_context 的旧栈返回，因此没有
         * 对应的 running=1 marker。只在确实有活跃 syscall 需要暂停时校验
         * vCPU 归属，避免把正常的首次任务切出记作模型错误。
         */
        if (state->active_nr >= 0 &&
            (!state->current_running || state->current_session != session ||
             state->current_task != task)) {
            ++state->switch_out_mismatches;
        }
        state->current_running = false;
        state->current_session = 0;
        state->current_task = 0;
        set_active_syscall(state, vcpu_index, -1);
        return;
    }

    uint32_t nr;
    pthread_mutex_lock(&task_mutex);
    bool active = lookup_binding_locked(session, task, &nr);
    pthread_mutex_unlock(&task_mutex);
    state->current_running = true;
    state->current_session = session;
    state->current_task = task;
    set_active_syscall(state, vcpu_index, active ? (int32_t)nr : -1);
}

static void count_tb(unsigned int vcpu_index, void *userdata)
{
    struct tb_profile *profile = userdata;

    if (vcpu_index >= configured_vcpus) {
        atomic_fetch_add_explicit(&unsupported_vcpu_events, 1,
                                  memory_order_relaxed);
        return;
    }
    struct vcpu_state *state = &vcpu_states[vcpu_index];
    int32_t nr = state->active_nr;

    if (nr < 0 || nr >= MAX_SYSCALLS || !state->current_running) {
        ++state->active_state_mismatches;
        qemu_plugin_u64_set(active_syscall_entry(), vcpu_index, 0);
        return;
    }
    add_saturating(&state->blocks[nr], 1, &state->counter_saturations);
    add_saturating(&state->instructions[nr], profile->instructions,
                   &state->counter_saturations);
    add_saturating(&state->unattributed[nr], profile->unattributed,
                   &state->counter_saturations);
    size_t base = (size_t)nr * MAX_DESCRIPTORS;
    for (uint32_t index = 0; index < profile->pair_count; ++index) {
        const struct tb_descriptor_count *pair = &profile->pairs[index];

        add_saturating(&state->mix[base + pair->descriptor_id], pair->count,
                       &state->counter_saturations);
    }
}

static void translate_block(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    (void)id;
    uint64_t pc = qemu_plugin_tb_vaddr(tb);

    if (pc == enter_pc) {
        atomic_fetch_add_explicit(&translated_marker_blocks, 1,
                                  memory_order_relaxed);
        qemu_plugin_register_vcpu_tb_exec_cb(tb, syscall_enter,
                                             QEMU_PLUGIN_CB_R_REGS, NULL);
        return;
    }
    if (pc == exit_pc) {
        atomic_fetch_add_explicit(&translated_marker_blocks, 1,
                                  memory_order_relaxed);
        qemu_plugin_register_vcpu_tb_exec_cb(tb, syscall_exit,
                                             QEMU_PLUGIN_CB_R_REGS, NULL);
        return;
    }
    if (pc == switch_pc) {
        atomic_fetch_add_explicit(&translated_marker_blocks, 1,
                                  memory_order_relaxed);
        qemu_plugin_register_vcpu_tb_exec_cb(tb, task_switch,
                                             QEMU_PLUGIN_CB_R_REGS, NULL);
        return;
    }
    if ((pc >> 63) == 0) {
        return;
    }

    size_t instruction_count = qemu_plugin_tb_n_insns(tb);
    struct tb_descriptor_count aggregate[MAX_DESCRIPTORS];
    size_t aggregate_count = 0;
    uint64_t unattributed = 0;

    atomic_fetch_add_explicit(&translated_kernel_blocks, 1,
                              memory_order_relaxed);
    atomic_fetch_add_explicit(&translated_kernel_instructions,
                              instruction_count, memory_order_relaxed);
    for (size_t index = 0; index < instruction_count; ++index) {
        struct qemu_plugin_insn *instruction =
            qemu_plugin_tb_get_insn(tb, index);
        int descriptor_id = intern_instruction(instruction);

        if (descriptor_id < 0) {
            ++unattributed;
            continue;
        }
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
    if (unattributed != 0) {
        atomic_fetch_add_explicit(&descriptor_overflow_blocks, 1,
                                  memory_order_relaxed);
    }
    if (aggregate_count > (SIZE_MAX - sizeof(struct tb_profile)) /
                              sizeof(struct tb_descriptor_count)) {
        atomic_fetch_add_explicit(&profile_allocation_errors, 1,
                                  memory_order_relaxed);
        atomic_fetch_add_explicit(&dropped_profile_blocks, 1,
                                  memory_order_relaxed);
        atomic_fetch_add_explicit(&dropped_profile_instructions,
                                  instruction_count, memory_order_relaxed);
        return;
    }
    size_t bytes = sizeof(struct tb_profile) +
                   aggregate_count * sizeof(struct tb_descriptor_count);
    struct tb_profile *profile = malloc(bytes);
    if (!profile) {
        atomic_fetch_add_explicit(&profile_allocation_errors, 1,
                                  memory_order_relaxed);
        atomic_fetch_add_explicit(&dropped_profile_blocks, 1,
                                  memory_order_relaxed);
        atomic_fetch_add_explicit(&dropped_profile_instructions,
                                  instruction_count, memory_order_relaxed);
        return;
    }
    profile->instructions = instruction_count;
    profile->unattributed = unattributed;
    profile->pair_count = (uint32_t)aggregate_count;
    memcpy(profile->pairs, aggregate,
           aggregate_count * sizeof(struct tb_descriptor_count));
    pthread_mutex_lock(&profile_mutex);
    profile->next = profiles;
    profiles = profile;
    pthread_mutex_unlock(&profile_mutex);

    qemu_plugin_register_vcpu_tb_exec_cond_cb(
        tb, count_tb, QEMU_PLUGIN_CB_NO_REGS, QEMU_PLUGIN_COND_NE,
        active_syscall_entry(), 0, profile);
}

static void initialize_vcpu(qemu_plugin_id_t id, unsigned int vcpu_index)
{
    (void)id;
    if (vcpu_index >= configured_vcpus) {
        atomic_fetch_add_explicit(&unsupported_vcpu_events, 1,
                                  memory_order_relaxed);
        return;
    }
    struct vcpu_state *state = &vcpu_states[vcpu_index];
    GArray *registers = qemu_plugin_get_registers();

    state->register_buffer = g_byte_array_sized_new(sizeof(uint64_t));
    for (guint index = 0; index < registers->len; ++index) {
        qemu_plugin_reg_descriptor *descriptor =
            &g_array_index(registers, qemu_plugin_reg_descriptor, index);
        const char *name = descriptor->name;

        if (!state->a0 &&
            (strcmp(name, "a0") == 0 || strcmp(name, "x10") == 0)) {
            state->a0 = descriptor->handle;
        } else if (!state->a1 &&
                   (strcmp(name, "a1") == 0 || strcmp(name, "x11") == 0)) {
            state->a1 = descriptor->handle;
        } else if (!state->a2 &&
                   (strcmp(name, "a2") == 0 || strcmp(name, "x12") == 0)) {
            state->a2 = descriptor->handle;
        }
    }
    g_array_free(registers, true);
    if (!state->a0 || !state->a1 || !state->a2 || !state->register_buffer) {
        atomic_fetch_add_explicit(&register_lookup_errors, 1,
                                  memory_order_relaxed);
    }
}

static uint64_t atomic_value(atomic_uint_fast64_t *value)
{
    return atomic_load_explicit(value, memory_order_relaxed);
}

static uint64_t sum_vcpu_counter(size_t offset, uint64_t *saturations)
{
    uint64_t total = 0;

    for (unsigned int vcpu = 0; vcpu < configured_vcpus; ++vcpu) {
        const uint64_t *counter =
            (const uint64_t *)((const uint8_t *)&vcpu_states[vcpu] + offset);
        add_saturating(&total, *counter, saturations);
    }
    return total;
}

static uint64_t sum_syscall_array(size_t offset, uint32_t nr,
                                  uint64_t *saturations)
{
    return sum_vcpu_counter(offset + (size_t)nr * sizeof(uint64_t),
                            saturations);
}

static uint64_t sum_descriptor_count(uint32_t nr, uint32_t descriptor_id,
                                     uint64_t *saturations)
{
    uint64_t total = 0;
    size_t index = (size_t)nr * MAX_DESCRIPTORS + descriptor_id;

    for (unsigned int vcpu = 0; vcpu < configured_vcpus; ++vcpu) {
        add_saturating(&total, vcpu_states[vcpu].mix[index], saturations);
    }
    return total;
}

static void write_signed_delta(FILE *stream, uint64_t left, uint64_t right)
{
    if (left >= right) {
        fprintf(stream, "%" PRIu64, left - right);
    } else {
        fprintf(stream, "-%" PRIu64, right - left);
    }
}

static void write_report(qemu_plugin_id_t id, void *userdata)
{
    (void)id;
    (void)userdata;
    uint64_t report_saturations = 0;
    uint64_t total_entries = 0;
    uint64_t total_exits = 0;
    uint64_t total_blocks = 0;
    uint64_t total_instructions = 0;
    uint64_t total_unattributed = 0;
    uint64_t total_descriptor_counts = 0;
    uint64_t completed_instances = 0;
    uint64_t observed_syscalls = 0;
    uint64_t active_vcpus = 0;

    pthread_mutex_lock(&task_mutex);
    guint active_tasks = task_bindings ? g_hash_table_size(task_bindings) : 0;
    pthread_mutex_unlock(&task_mutex);
    for (unsigned int vcpu = 0; vcpu < configured_vcpus; ++vcpu) {
        if (vcpu_states[vcpu].active_nr >= 0) {
            ++active_vcpus;
        }
    }

    fprintf(output_file,
            "{\"schema\":\"mygo.riscv-syscall-model.v1\","
            "\"target\":\"riscv64\",\"vcpus\":%u,"
            "\"scope\":\"active-task-kernel-tb\","
            "\"kernel_pc_heuristic\":\"bit63-set\","
            "\"config\":{\"enter_pc\":\"0x%016" PRIx64
            "\",\"exit_pc\":\"0x%016" PRIx64
            "\",\"switch_pc\":\"0x%016" PRIx64 "\"},"
            "\"descriptors\":[",
            configured_vcpus, enter_pc, exit_pc, switch_pc);

    pthread_mutex_lock(&descriptor_mutex);
    for (size_t id_index = 0; id_index < descriptor_count; ++id_index) {
        const struct instruction_descriptor *descriptor =
            &descriptors[id_index];

        if (id_index != 0) {
            fputc(',', output_file);
        }
        fprintf(output_file, "{\"id\":%zu,\"mnemonic\":", id_index);
        json_string(output_file, descriptor->mnemonic);
        fprintf(output_file, ",\"size\":%u,\"encodings\":[",
                descriptor->size);
        bool first_encoding = true;
        for (const struct raw_encoding *encoding = descriptor->encodings;
             encoding; encoding = encoding->next) {
            if (!first_encoding) {
                fputc(',', output_file);
            }
            first_encoding = false;
            fputc('"', output_file);
            for (uint32_t byte = 0; byte < encoding->size; ++byte) {
                fprintf(output_file, "%02x", encoding->bytes[byte]);
            }
            fputc('"', output_file);
        }
        fprintf(output_file, "],\"encoding_count\":%" PRIu64 "}",
                descriptor->encoding_count);
    }
    size_t report_descriptor_count = descriptor_count;
    pthread_mutex_unlock(&descriptor_mutex);

    fputs("],\"syscalls\":[", output_file);
    bool first_syscall = true;
    for (uint32_t nr = 0; nr < MAX_SYSCALLS; ++nr) {
        uint64_t entries = sum_syscall_array(offsetof(struct vcpu_state, entries),
                                             nr, &report_saturations);
        uint64_t exits = sum_syscall_array(offsetof(struct vcpu_state, exits), nr,
                                           &report_saturations);
        uint64_t blocks = sum_syscall_array(offsetof(struct vcpu_state, blocks),
                                            nr, &report_saturations);
        uint64_t instructions = sum_syscall_array(
            offsetof(struct vcpu_state, instructions), nr,
            &report_saturations);
        uint64_t unattributed = sum_syscall_array(
            offsetof(struct vcpu_state, unattributed), nr,
            &report_saturations);
        if (entries == 0 && exits == 0 && blocks == 0 && instructions == 0) {
            continue;
        }
        ++observed_syscalls;
        add_saturating(&total_entries, entries, &report_saturations);
        add_saturating(&total_exits, exits, &report_saturations);
        add_saturating(&total_blocks, blocks, &report_saturations);
        add_saturating(&total_instructions, instructions,
                       &report_saturations);
        add_saturating(&total_unattributed, unattributed,
                       &report_saturations);
        add_saturating(&completed_instances, entries < exits ? entries : exits,
                       &report_saturations);

        if (!first_syscall) {
            fputc(',', output_file);
        }
        first_syscall = false;
        fprintf(output_file,
                "{\"nr\":%u,\"entries\":%" PRIu64
                ",\"exits\":%" PRIu64 ",\"blocks\":%" PRIu64
                ",\"instructions\":%" PRIu64
                ",\"unattributed_instructions\":%" PRIu64
                ",\"descriptor_counts\":[",
                nr, entries, exits, blocks, instructions, unattributed);
        bool first_count = true;
        uint64_t syscall_descriptor_total = 0;
        for (uint32_t descriptor_id = 0;
             descriptor_id < report_descriptor_count; ++descriptor_id) {
            uint64_t count = sum_descriptor_count(nr, descriptor_id,
                                                  &report_saturations);
            if (count == 0) {
                continue;
            }
            add_saturating(&syscall_descriptor_total, count,
                           &report_saturations);
            if (!first_count) {
                fputc(',', output_file);
            }
            first_count = false;
            fprintf(output_file, "{\"id\":%u,\"count\":%" PRIu64 "}",
                    descriptor_id, count);
        }
        add_saturating(&total_descriptor_counts, syscall_descriptor_total,
                       &report_saturations);
        fprintf(output_file, "],\"descriptor_count_sum\":%" PRIu64 "}",
                syscall_descriptor_total);
    }

    uint64_t marker_enters =
        sum_vcpu_counter(offsetof(struct vcpu_state, enter_markers),
                         &report_saturations);
    uint64_t marker_exits =
        sum_vcpu_counter(offsetof(struct vcpu_state, exit_markers),
                         &report_saturations);
    uint64_t marker_switches =
        sum_vcpu_counter(offsetof(struct vcpu_state, switch_markers),
                         &report_saturations);
    uint64_t register_read_errors =
        sum_vcpu_counter(offsetof(struct vcpu_state, register_read_errors),
                         &report_saturations);
    uint64_t invalid_syscalls =
        sum_vcpu_counter(offsetof(struct vcpu_state, invalid_syscall_numbers),
                         &report_saturations);
    uint64_t invalid_switches =
        sum_vcpu_counter(offsetof(struct vcpu_state, invalid_switch_values),
                         &report_saturations);
    uint64_t duplicate_enters =
        sum_vcpu_counter(offsetof(struct vcpu_state, duplicate_enters),
                         &report_saturations);
    uint64_t unmatched_exits =
        sum_vcpu_counter(offsetof(struct vcpu_state, unmatched_exits),
                         &report_saturations);
    uint64_t exit_nr_mismatches =
        sum_vcpu_counter(offsetof(struct vcpu_state, exit_nr_mismatches),
                         &report_saturations);
    uint64_t exit_task_mismatches =
        sum_vcpu_counter(offsetof(struct vcpu_state, exit_task_mismatches),
                         &report_saturations);
    uint64_t switch_out_mismatches =
        sum_vcpu_counter(offsetof(struct vcpu_state, switch_out_mismatches),
                         &report_saturations);
    uint64_t active_state_mismatches =
        sum_vcpu_counter(offsetof(struct vcpu_state, active_state_mismatches),
                         &report_saturations);
    uint64_t runtime_saturations =
        sum_vcpu_counter(offsetof(struct vcpu_state, counter_saturations),
                         &report_saturations);
    uint64_t dropped_blocks = atomic_value(&dropped_profile_blocks);
    uint64_t dropped_instructions = atomic_value(&dropped_profile_instructions);
    bool instruction_closed =
        total_instructions == total_descriptor_counts + total_unattributed &&
        total_descriptor_counts <= total_instructions;
    bool fully_attributed = total_unattributed == 0;
    bool closed = total_entries == total_exits && active_tasks == 0 &&
                  active_vcpus == 0 && instruction_closed && fully_attributed &&
                  dropped_blocks == 0 && runtime_saturations == 0 &&
                  report_saturations == 0;

    fprintf(output_file,
            "],\"totals\":{\"entries\":%" PRIu64
            ",\"exits\":%" PRIu64 ",\"completed_instances\":%" PRIu64
            ",\"observed_syscall_numbers\":%" PRIu64
            ",\"blocks\":%" PRIu64 ",\"instructions\":%" PRIu64
            ",\"descriptor_count_sum\":%" PRIu64
            ",\"unattributed_instructions\":%" PRIu64
            ",\"enter_markers\":%" PRIu64
            ",\"exit_markers\":%" PRIu64
            ",\"switch_markers\":%" PRIu64 "},"
            "\"closure\":{\"entry_exit_delta\":",
            total_entries, total_exits, completed_instances, observed_syscalls,
            total_blocks, total_instructions, total_descriptor_counts,
            total_unattributed, marker_enters, marker_exits, marker_switches);
    write_signed_delta(output_file, total_entries, total_exits);
    fprintf(output_file,
            ",\"active_tasks_at_exit\":%u,\"active_vcpus_at_exit\":%" PRIu64
            ",\"instructions_minus_accounted\":",
            active_tasks, active_vcpus);
    write_signed_delta(output_file, total_instructions,
                       total_descriptor_counts + total_unattributed);
    fprintf(output_file,
            ",\"instructions_closed\":%s,\"fully_attributed\":%s,"
            "\"closed\":%s},"
            "\"overflow\":{\"unsupported_vcpu_events\":%" PRIu64
            ",\"invalid_syscall_numbers\":%" PRIu64
            ",\"descriptor_overflow_instructions\":%" PRIu64
            ",\"descriptor_overflow_blocks\":%" PRIu64
            ",\"instruction_size_or_data_errors\":%" PRIu64
            ",\"encoding_allocation_errors\":%" PRIu64
            ",\"task_allocation_errors\":%" PRIu64
            ",\"profile_allocation_errors\":%" PRIu64
            ",\"dropped_profile_blocks\":%" PRIu64
            ",\"dropped_profile_instructions\":%" PRIu64
            ",\"runtime_counter_saturations\":%" PRIu64
            ",\"report_counter_saturations\":%" PRIu64 "},"
            "\"errors\":{\"register_lookup_errors\":%" PRIu64
            ",\"register_read_errors\":%" PRIu64
            ",\"invalid_switch_values\":%" PRIu64
            ",\"duplicate_enters\":%" PRIu64
            ",\"unmatched_exits\":%" PRIu64
            ",\"exit_nr_mismatches\":%" PRIu64
            ",\"exit_task_mismatches\":%" PRIu64
            ",\"switch_out_mismatches\":%" PRIu64
            ",\"active_state_mismatches\":%" PRIu64
            ",\"disassembly_errors\":%" PRIu64
            ",\"mnemonic_truncations\":%" PRIu64
            ",\"output_write_errors\":%" PRIu64 "},"
            "\"translation\":{\"kernel_blocks\":%" PRIu64
            ",\"kernel_instructions\":%" PRIu64
            ",\"marker_blocks\":%" PRIu64
            ",\"descriptor_count\":%zu}}\n",
            instruction_closed ? "true" : "false",
            fully_attributed ? "true" : "false", closed ? "true" : "false",
            atomic_value(&unsupported_vcpu_events), invalid_syscalls,
            atomic_value(&descriptor_overflow_instructions),
            atomic_value(&descriptor_overflow_blocks),
            atomic_value(&invalid_instruction_sizes) +
                atomic_value(&instruction_data_errors),
            atomic_value(&encoding_allocation_errors),
            atomic_value(&task_allocation_errors),
            atomic_value(&profile_allocation_errors), dropped_blocks,
            dropped_instructions, runtime_saturations, report_saturations,
            atomic_value(&register_lookup_errors), register_read_errors,
            invalid_switches, duplicate_enters, unmatched_exits,
            exit_nr_mismatches, exit_task_mismatches, switch_out_mismatches,
            active_state_mismatches, atomic_value(&disassembly_errors),
            atomic_value(&mnemonic_truncations),
            atomic_value(&output_write_errors),
            atomic_value(&translated_kernel_blocks),
            atomic_value(&translated_kernel_instructions),
            atomic_value(&translated_marker_blocks), report_descriptor_count);

    if (fflush(output_file) != 0 || ferror(output_file)) {
        atomic_fetch_add_explicit(&output_write_errors, 1,
                                  memory_order_relaxed);
        fprintf(stderr, "riscv syscall model: cannot flush %s: %s\n",
                output_path, strerror(errno));
    }
}

static void release_resources(void)
{
    if (output_file) {
        fclose(output_file);
        output_file = NULL;
    }
    if (scoreboard) {
        qemu_plugin_scoreboard_free(scoreboard);
        scoreboard = NULL;
    }
    if (task_bindings) {
        g_hash_table_destroy(task_bindings);
        task_bindings = NULL;
    }
    if (encoding_table) {
        g_hash_table_destroy(encoding_table);
        encoding_table = NULL;
    }
    if (vcpu_states) {
        for (unsigned int vcpu = 0; vcpu < configured_vcpus; ++vcpu) {
            if (vcpu_states[vcpu].register_buffer) {
                g_byte_array_unref(vcpu_states[vcpu].register_buffer);
            }
            free(vcpu_states[vcpu].mix);
        }
        free(vcpu_states);
        vcpu_states = NULL;
    }
    while (profiles) {
        struct tb_profile *next = profiles->next;
        free(profiles);
        profiles = next;
    }
    for (size_t id = 0; id < descriptor_count; ++id) {
        struct raw_encoding *encoding = descriptors[id].encodings;
        while (encoding) {
            struct raw_encoding *next = encoding->next;
            free(encoding);
            encoding = next;
        }
    }
    free(output_path);
    output_path = NULL;
}

static void plugin_exit(qemu_plugin_id_t id, void *userdata)
{
    write_report(id, userdata);
    release_resources();
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info, int argc,
                                           char **argv)
{
    bool have_enter_pc = false;
    bool have_exit_pc = false;
    bool have_switch_pc = false;

    if (!info->system_emulation || strcmp(info->target_name, "riscv64") != 0 ||
        info->system.smp_vcpus <= 0 || info->system.smp_vcpus > MAX_VCPUS) {
        fprintf(stderr,
                "riscv syscall model: riscv64 system emulation with 1..%u "
                "vCPUs is required\n",
                MAX_VCPUS);
        return 1;
    }
    configured_vcpus = (unsigned int)info->system.smp_vcpus;
    for (int index = 0; index < argc; ++index) {
        if (strncmp(argv[index], "output=", 7) == 0 && !output_path &&
            argv[index][7]) {
            output_path = strdup(argv[index] + 7);
        } else if (strncmp(argv[index], "enter_pc=", 9) == 0 &&
                   !have_enter_pc && parse_u64(argv[index] + 9, &enter_pc)) {
            have_enter_pc = true;
        } else if (strncmp(argv[index], "exit_pc=", 8) == 0 && !have_exit_pc &&
                   parse_u64(argv[index] + 8, &exit_pc)) {
            have_exit_pc = true;
        } else if (strncmp(argv[index], "switch_pc=", 10) == 0 &&
                   !have_switch_pc &&
                   parse_u64(argv[index] + 10, &switch_pc)) {
            have_switch_pc = true;
        } else {
            fprintf(stderr, "riscv syscall model: invalid option: %s\n",
                    argv[index]);
            release_resources();
            return 1;
        }
    }
    if (!output_path || !have_enter_pc || !have_exit_pc || !have_switch_pc ||
        enter_pc == exit_pc || enter_pc == switch_pc || exit_pc == switch_pc) {
        fputs("riscv syscall model: output and three distinct marker PCs are "
              "required\n",
              stderr);
        release_resources();
        return 1;
    }

    output_file = fopen(output_path, "w");
    if (!output_file) {
        fprintf(stderr, "riscv syscall model: cannot open %s: %s\n",
                output_path, strerror(errno));
        release_resources();
        return 1;
    }
    vcpu_states = calloc(configured_vcpus, sizeof(*vcpu_states));
    if (!vcpu_states) {
        fputs("riscv syscall model: cannot allocate vCPU states\n", stderr);
        release_resources();
        return 1;
    }
    for (unsigned int vcpu = 0; vcpu < configured_vcpus; ++vcpu) {
        vcpu_states[vcpu].active_nr = -1;
        vcpu_states[vcpu].mix =
            calloc((size_t)MAX_SYSCALLS * MAX_DESCRIPTORS, sizeof(uint64_t));
        if (!vcpu_states[vcpu].mix) {
            fputs("riscv syscall model: cannot allocate vCPU counters\n",
                  stderr);
            release_resources();
            return 1;
        }
    }
    task_bindings =
        g_hash_table_new_full(task_key_hash, task_key_equal, g_free, NULL);
    encoding_table =
        g_hash_table_new(raw_encoding_hash, raw_encoding_equal);
    scoreboard = qemu_plugin_scoreboard_new(sizeof(struct scoreboard_state));
    if (!task_bindings || !encoding_table || !scoreboard) {
        fputs("riscv syscall model: cannot allocate plugin state\n", stderr);
        release_resources();
        return 1;
    }
    setvbuf(output_file, NULL, _IOLBF, 0);

    qemu_plugin_register_vcpu_init_cb(id, initialize_vcpu);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
    qemu_plugin_register_atexit_cb(id, plugin_exit, NULL);
    return 0;
}
