// SPDX-License-Identifier: GPL-2.0-or-later
/* 用于低频客机内核栈采样的 QEMU TCG plugin。 */

#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#include <glib.h>
#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

#define OBSERVER_MAGIC "MYGOBS1"
#define OBSERVER_VERSION 1U
#define DEFAULT_PERIOD_INSNS UINT64_C(50000000)
#define DEFAULT_STACK_BYTES 1024U
#define MAX_STACK_BYTES 4096U

enum record_flags {
    RECORD_KERNEL = 1U << 0,
    RECORD_REGISTERS_VALID = 1U << 1,
    RECORD_STACK_VALID = 1U << 2,
    RECORD_STACK_TRUNCATED = 1U << 3,
    RECORD_COUNTER_ONLY = 1U << 4,
};

struct __attribute__((packed)) observer_record {
    char magic[8];
    uint16_t version;
    uint16_t header_bytes;
    uint32_t record_bytes;
    uint32_t vcpu_index;
    uint32_t flags;
    uint64_t sequence;
    uint64_t monotonic_ns;
    uint64_t total_insns;
    uint64_t user_insns;
    uint64_t kernel_insns;
    uint64_t dropped;
    uint64_t pc;
    uint64_t sp;
    uint64_t ra;
    uint64_t fp;
    uint64_t tp;
    uint64_t percpu;
    uint32_t stack_bytes;
    uint32_t reserved;
    uint8_t stack[MAX_STACK_BYTES];
};

_Static_assert(offsetof(struct observer_record, stack) == 128,
               "observer record header layout changed");

struct vcpu_state {
    struct qemu_plugin_register *pc;
    struct qemu_plugin_register *sp;
    struct qemu_plugin_register *ra;
    struct qemu_plugin_register *fp;
    struct qemu_plugin_register *tp;
    struct qemu_plugin_register *percpu;
    GByteArray *register_buffer;
    GByteArray *stack_buffer;
    uint64_t completed_insns;
    uint64_t sequence;
    uint64_t dropped;
};

static qemu_plugin_u64 kernel_insns;
static qemu_plugin_u64 total_period;
static struct vcpu_state *vcpu_states;
static unsigned int max_vcpus;
static uint64_t period_insns = DEFAULT_PERIOD_INSNS;
static uint32_t stack_bytes = DEFAULT_STACK_BYTES;
static int output_socket = -1;
static struct sockaddr_un output_address;
static socklen_t output_address_len;
static char *summary_path;
static bool riscv_target;

/* ---- per-TB histogram ------------------------------------------- */
struct tb_entry {
    uint64_t execs;
    uint32_t insns;
    uint32_t _pad;
};

static GMutex histogram_mutex;
static GHashTable *tb_histogram;
static char *histogram_path;

static bool parse_u64(const char *value, uint64_t minimum, uint64_t maximum,
                      uint64_t *result)
{
    char *end = NULL;
    unsigned long long parsed;

    errno = 0;
    parsed = strtoull(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0' || parsed < minimum ||
        parsed > maximum) {
        return false;
    }
    *result = (uint64_t)parsed;
    return true;
}

static bool set_socket_path(const char *path)
{
    size_t length = strlen(path);

    if (length == 0 || length >= sizeof(output_address.sun_path)) {
        return false;
    }
    memset(&output_address, 0, sizeof(output_address));
    output_address.sun_family = AF_UNIX;
    memcpy(output_address.sun_path, path, length + 1);
    output_address_len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) +
                                     length + 1);
    return true;
}

static uint64_t load_target_u64(const uint8_t *bytes)
{
    uint64_t value = 0;

    for (unsigned int index = 0; index < sizeof(value); ++index) {
        value |= (uint64_t)bytes[index] << (index * CHAR_BIT);
    }
    return value;
}

static bool read_register_u64(struct qemu_plugin_register *handle,
                              GByteArray *buffer, uint64_t *value)
{
    int length;

    if (handle == NULL) {
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

static uint64_t monotonic_ns(void)
{
    struct timespec value;

    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) +
           (uint64_t)value.tv_nsec;
}

static void send_record(struct observer_record *record,
                        struct vcpu_state *state, unsigned int vcpu_index,
                        uint32_t flags, uint32_t copied)
{
    size_t wire_bytes = offsetof(struct observer_record, stack) + copied;
    ssize_t sent;

    memcpy(record->magic, OBSERVER_MAGIC, sizeof(record->magic));
    record->version = GUINT16_TO_LE(OBSERVER_VERSION);
    record->header_bytes =
        GUINT16_TO_LE((uint16_t)offsetof(struct observer_record, stack));
    record->record_bytes = GUINT32_TO_LE((uint32_t)wire_bytes);
    record->vcpu_index = GUINT32_TO_LE(vcpu_index);
    record->flags = GUINT32_TO_LE(flags);
    record->sequence = GUINT64_TO_LE(record->sequence);
    record->monotonic_ns = GUINT64_TO_LE(record->monotonic_ns);
    record->total_insns = GUINT64_TO_LE(record->total_insns);
    record->user_insns = GUINT64_TO_LE(record->user_insns);
    record->kernel_insns = GUINT64_TO_LE(record->kernel_insns);
    record->dropped = GUINT64_TO_LE(record->dropped);
    record->pc = GUINT64_TO_LE(record->pc);
    record->sp = GUINT64_TO_LE(record->sp);
    record->ra = GUINT64_TO_LE(record->ra);
    record->fp = GUINT64_TO_LE(record->fp);
    record->tp = GUINT64_TO_LE(record->tp);
    record->percpu = GUINT64_TO_LE(record->percpu);
    record->stack_bytes = GUINT32_TO_LE(copied);
    sent = sendto(output_socket, record, wire_bytes, MSG_DONTWAIT | MSG_NOSIGNAL,
                  (const struct sockaddr *)&output_address,
                  output_address_len);
    if (sent != (ssize_t)wire_bytes) {
        state->dropped++;
    }
}

static void sample_guest(unsigned int vcpu_index, void *userdata)
{
    struct observer_record record = {0};
    struct vcpu_state *state;
    uint64_t accumulated;
    uint64_t remainder;
    uint64_t pc;
    uint64_t sp;
    uint64_t ra;
    uint64_t fp;
    uint64_t tp;
    uint64_t percpu;
    uint32_t copied = 0;
    bool kernel = GPOINTER_TO_UINT(userdata) != 0;
    uint32_t flags = kernel ? RECORD_KERNEL : RECORD_COUNTER_ONLY;

    if (vcpu_index >= max_vcpus) {
        return;
    }
    accumulated = qemu_plugin_u64_get(total_period, vcpu_index);
    remainder = accumulated % period_insns;
    state = &vcpu_states[vcpu_index];
    state->completed_insns += accumulated - remainder;
    qemu_plugin_u64_set(total_period, vcpu_index, remainder);
    record.sequence = ++state->sequence;
    record.monotonic_ns = monotonic_ns();
    record.total_insns = state->completed_insns + remainder;
    record.kernel_insns = qemu_plugin_u64_get(kernel_insns, vcpu_index);
    record.user_insns = record.total_insns - record.kernel_insns;
    record.dropped = state->dropped;

    if (!kernel) {
        send_record(&record, state, vcpu_index, flags, 0);
        return;
    }
    if (read_register_u64(state->pc, state->register_buffer, &pc) &&
        read_register_u64(state->sp, state->register_buffer, &sp) &&
        read_register_u64(state->ra, state->register_buffer, &ra) &&
        read_register_u64(state->fp, state->register_buffer, &fp) &&
        read_register_u64(state->tp, state->register_buffer, &tp) &&
        read_register_u64(state->percpu, state->register_buffer, &percpu)) {
        record.pc = pc;
        record.sp = sp;
        record.ra = ra;
        record.fp = fp;
        record.tp = tp;
        record.percpu = percpu;
        flags |= RECORD_REGISTERS_VALID;
        while (copied < stack_bytes) {
            uint64_t address = sp + copied;
            uint32_t page_remaining = 4096U - (uint32_t)(address & 4095U);
            uint32_t chunk = MIN(stack_bytes - copied, MIN(page_remaining, 256U));

            g_byte_array_set_size(state->stack_buffer, 0);
            if (!qemu_plugin_read_memory_vaddr(address, state->stack_buffer,
                                               chunk) ||
                state->stack_buffer->len < chunk) {
                flags |= RECORD_STACK_TRUNCATED;
                break;
            }
            memcpy(record.stack + copied, state->stack_buffer->data, chunk);
            copied += chunk;
        }
        if (copied > 0) {
            flags |= RECORD_STACK_VALID;
        }
    }
    send_record(&record, state, vcpu_index, flags, copied);
}

static void count_tb_exec(unsigned int vcpu_index, void *userdata)
{
    (void)vcpu_index;
    __atomic_fetch_add(&((struct tb_entry *)userdata)->execs, 1ULL,
                       __ATOMIC_RELAXED);
}

static void translate_block(qemu_plugin_id_t id, struct qemu_plugin_tb *tb)
{
    uint64_t pc = qemu_plugin_tb_vaddr(tb);
    uint64_t instruction_count = (uint64_t)qemu_plugin_tb_n_insns(tb);
    bool kernel = (pc >> 63) != 0;

    (void)id;
    qemu_plugin_register_vcpu_tb_exec_cond_cb(
        tb, sample_guest,
        kernel ? QEMU_PLUGIN_CB_R_REGS : QEMU_PLUGIN_CB_NO_REGS,
        QEMU_PLUGIN_COND_GE, total_period, period_insns,
        GUINT_TO_POINTER(kernel));
    qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
        tb, QEMU_PLUGIN_INLINE_ADD_U64, total_period, instruction_count);
    if (kernel) {
        qemu_plugin_register_vcpu_tb_exec_inline_per_vcpu(
            tb, QEMU_PLUGIN_INLINE_ADD_U64, kernel_insns,
            instruction_count);
    }
    if (tb_histogram != NULL) {
        gpointer hkey = (gpointer)(uintptr_t)pc;
        struct tb_entry *entry;

        g_mutex_lock(&histogram_mutex);
        entry = g_hash_table_lookup(tb_histogram, hkey);
        if (entry == NULL) {
            entry = g_new0(struct tb_entry, 1);
            entry->insns = (uint32_t)instruction_count;
            g_hash_table_insert(tb_histogram, hkey, entry);
        }
        g_mutex_unlock(&histogram_mutex);
        qemu_plugin_register_vcpu_tb_exec_cb(tb, count_tb_exec,
                                             QEMU_PLUGIN_CB_NO_REGS, entry);
    }
}

static void initialize_vcpu(qemu_plugin_id_t id, unsigned int vcpu_index)
{
    GArray *registers;
    struct vcpu_state *state;

    (void)id;
    if (vcpu_index >= max_vcpus) {
        return;
    }
    state = &vcpu_states[vcpu_index];
    state->register_buffer = g_byte_array_sized_new(sizeof(uint64_t));
    state->stack_buffer = g_byte_array_sized_new(stack_bytes);
    registers = qemu_plugin_get_registers();
    for (guint index = 0; index < registers->len; ++index) {
        qemu_plugin_reg_descriptor *descriptor =
            &g_array_index(registers, qemu_plugin_reg_descriptor, index);

        const char *name = descriptor->name;

        if (strcmp(name, "pc") == 0) {
            state->pc = descriptor->handle;
        } else if (!riscv_target && strcmp(name, "r3") == 0) {
            state->sp = descriptor->handle;
        } else if (!riscv_target && strcmp(name, "r1") == 0) {
            state->ra = descriptor->handle;
        } else if (!riscv_target && strcmp(name, "r22") == 0) {
            state->fp = descriptor->handle;
        } else if (!riscv_target && strcmp(name, "r2") == 0) {
            state->tp = descriptor->handle;
        } else if (!riscv_target && strcmp(name, "r21") == 0) {
            state->percpu = descriptor->handle;
        } else if (riscv_target &&
                   (strcmp(name, "sp") == 0 || strcmp(name, "x2") == 0)) {
            state->sp = descriptor->handle;
        } else if (riscv_target &&
                   (strcmp(name, "ra") == 0 || strcmp(name, "x1") == 0)) {
            state->ra = descriptor->handle;
        } else if (riscv_target &&
                   (strcmp(name, "fp") == 0 || strcmp(name, "s0") == 0 ||
                    strcmp(name, "x8") == 0)) {
            state->fp = descriptor->handle;
        } else if (riscv_target &&
                   (strcmp(name, "tp") == 0 || strcmp(name, "x4") == 0)) {
            state->tp = descriptor->handle;
        }
    }
    g_array_free(registers, true);
    if (riscv_target) {
        state->percpu = state->tp;
    }
}

static void write_histogram(void)
{
    GHashTableIter iter;
    gpointer key, value;
    bool first = true;
    FILE *stream;

    if (histogram_path == NULL || tb_histogram == NULL) {
        return;
    }
    stream = fopen(histogram_path, "w");
    if (stream == NULL) {
        fprintf(stderr, "buildstorm_observer: open histogram %s failed: %s\n",
                histogram_path, strerror(errno));
        return;
    }
    fputs("{\n  \"schema\": \"mygo.qemu-observer-histogram.v1\",\n  \"tbs\": [\n",
          stream);
    g_mutex_lock(&histogram_mutex);
    g_hash_table_iter_init(&iter, tb_histogram);
    while (g_hash_table_iter_next(&iter, &key, &value)) {
        struct tb_entry *entry = value;
        uint64_t pc = (uint64_t)(uintptr_t)key;
        uint64_t execs =
            __atomic_load_n(&entry->execs, __ATOMIC_RELAXED);
        if (execs == 0) {
            continue;
        }
        if (!first) {
            fputs(",\n", stream);
        }
        fprintf(stream,
                "    {\"pc\": %" PRIu64 ", \"insns\": %" PRIu32
                ", \"execs\": %" PRIu64 "}",
                pc, entry->insns, execs);
        first = false;
    }
    g_mutex_unlock(&histogram_mutex);
    fputs("\n  ]\n}\n", stream);
    if (fclose(stream) != 0) {
        fprintf(stderr, "buildstorm_observer: close histogram %s failed: %s\n",
                histogram_path, strerror(errno));
    }
}

static void write_summary(void)
{
    FILE *stream;

    if (summary_path == NULL) {
        return;
    }
    stream = fopen(summary_path, "w");
    if (stream == NULL) {
        fprintf(stderr, "buildstorm_observer: open %s failed: %s\n",
                summary_path, strerror(errno));
        return;
    }
    fprintf(stream,
            "{\n  \"schema\": \"mygo.qemu-observer-plugin.v1\",\n"
            "  \"counter_granularity\": \"translation-block\",\n"
            "  \"period_insns\": %" PRIu64 ",\n"
            "  \"stack_bytes\": %u,\n  \"vcpus\": [\n",
            period_insns, stack_bytes);
    for (int cpu = 0, count = qemu_plugin_num_vcpus(); cpu < count; ++cpu) {
        struct vcpu_state *state = &vcpu_states[cpu];
        uint64_t total = state->completed_insns +
                         qemu_plugin_u64_get(total_period, cpu);
        uint64_t kernel = qemu_plugin_u64_get(kernel_insns, cpu);

        fprintf(stream,
                "    {\"cpu\": %d, \"total\": %" PRIu64
                ", \"user\": %" PRIu64 ", \"kernel\": %" PRIu64
                ", \"samples\": %" PRIu64 ", \"dropped\": %" PRIu64
                "}%s\n",
                cpu, total, total - kernel, kernel, state->sequence,
                state->dropped, cpu + 1 == count ? "" : ",");
    }
    fputs("  ]\n}\n", stream);
    if (fclose(stream) != 0) {
        fprintf(stderr, "buildstorm_observer: close %s failed: %s\n",
                summary_path, strerror(errno));
    }
}

static void plugin_exit(qemu_plugin_id_t id, void *userdata)
{
    (void)id;
    (void)userdata;
    write_summary();
    write_histogram();
    if (tb_histogram != NULL) {
        g_mutex_lock(&histogram_mutex);
        g_hash_table_destroy(tb_histogram);
        tb_histogram = NULL;
        g_mutex_unlock(&histogram_mutex);
        g_mutex_clear(&histogram_mutex);
    }
    g_free(histogram_path);
    for (unsigned int cpu = 0; cpu < max_vcpus; ++cpu) {
        if (vcpu_states[cpu].register_buffer != NULL) {
            g_byte_array_free(vcpu_states[cpu].register_buffer, true);
        }
        if (vcpu_states[cpu].stack_buffer != NULL) {
            g_byte_array_free(vcpu_states[cpu].stack_buffer, true);
        }
    }
    g_free(vcpu_states);
    qemu_plugin_scoreboard_free(kernel_insns.score);
    qemu_plugin_scoreboard_free(total_period.score);
    if (output_socket >= 0) {
        close(output_socket);
    }
    g_free(summary_path);
}

static int parse_options(int argc, char **argv)
{
    bool have_socket = false;

    for (int index = 0; index < argc; ++index) {
        const char *argument = argv[index];

        if (g_str_has_prefix(argument, "socket=")) {
            if (!set_socket_path(argument + strlen("socket="))) {
                fputs("buildstorm_observer: invalid socket path\n", stderr);
                return -1;
            }
            have_socket = true;
        } else if (g_str_has_prefix(argument, "period=")) {
            if (!parse_u64(argument + strlen("period="), 1000, UINT64_MAX,
                           &period_insns)) {
                fputs("buildstorm_observer: invalid period\n", stderr);
                return -1;
            }
        } else if (g_str_has_prefix(argument, "stack-bytes=")) {
            uint64_t value;

            if (!parse_u64(argument + strlen("stack-bytes="), 0,
                           MAX_STACK_BYTES, &value) ||
                value % sizeof(uint64_t) != 0) {
                fputs("buildstorm_observer: invalid stack-bytes\n", stderr);
                return -1;
            }
            stack_bytes = (uint32_t)value;
        } else if (g_str_has_prefix(argument, "summary=")) {
            g_free(summary_path);
            summary_path = g_strdup(argument + strlen("summary="));
        } else if (g_str_has_prefix(argument, "histogram=")) {
            g_free(histogram_path);
            histogram_path = g_strdup(argument + strlen("histogram="));
        } else {
            fprintf(stderr, "buildstorm_observer: unknown option: %s\n",
                    argument);
            return -1;
        }
    }
    if (!have_socket) {
        fputs("buildstorm_observer: socket option is required\n", stderr);
        return -1;
    }
    return 0;
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info,
                                           int argc, char **argv)
{
    struct qemu_plugin_scoreboard *scoreboard;

    if (!info->system_emulation ||
        (strcmp(info->target_name, "loongarch64") != 0 &&
         strcmp(info->target_name, "riscv64") != 0)) {
        fputs("buildstorm_observer: RISC-V or LoongArch system emulation is required\n",
              stderr);
        return -1;
    }
    riscv_target = strcmp(info->target_name, "riscv64") == 0;
    if (parse_options(argc, argv) != 0) {
        return -1;
    }
    output_socket = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (output_socket < 0) {
        fprintf(stderr, "buildstorm_observer: socket failed: %s\n",
                strerror(errno));
        return -1;
    }
    max_vcpus = info->system.max_vcpus;
    vcpu_states = g_new0(struct vcpu_state, max_vcpus);
    scoreboard = qemu_plugin_scoreboard_new(sizeof(uint64_t));
    kernel_insns = qemu_plugin_scoreboard_u64(scoreboard);
    scoreboard = qemu_plugin_scoreboard_new(sizeof(uint64_t));
    total_period = qemu_plugin_scoreboard_u64(scoreboard);
    if (histogram_path != NULL) {
        g_mutex_init(&histogram_mutex);
        tb_histogram = g_hash_table_new_full(g_direct_hash, g_direct_equal,
                                             NULL, g_free);
    }
    qemu_plugin_register_vcpu_init_cb(id, initialize_vcpu);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate_block);
    qemu_plugin_register_atexit_cb(id, plugin_exit, NULL);
    return 0;
}
