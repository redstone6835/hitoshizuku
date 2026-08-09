#define _GNU_SOURCE

/*
 * Sample the userspace task-clock of every thread in a QEMU process.
 *
 * The output is a compact, little-endian stream.  It starts with
 * rv_tcg_file_header, followed by variable-sized records.  Every record starts
 * with rv_tcg_record_header; its type selects one of the payload structures
 * below.  Samples contain exactly PERF_SAMPLE_IP, TID, TIME, CPU and PERIOD.
 * Thread, lost, per-thread counter and quality records make the stream
 * self-describing enough for an offline quality gate.
 */

#include <dirent.h>
#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <inttypes.h>
#include <linux/perf_event.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define RV_TCG_VERSION 1U
#define RV_TCG_DATA_PAGES 64U
#define RV_TCG_TIMER_NS 50000000ULL
#define RV_TCG_RESCAN_TICKS 10U
#define RV_TCG_MAX_EPOLL_EVENTS 256
#define RV_TCG_COMM_LEN 32U

enum rv_tcg_record_type {
    RV_TCG_RECORD_SAMPLE = 1,
    RV_TCG_RECORD_LOST = 2,
    RV_TCG_RECORD_THREAD = 3,
    RV_TCG_RECORD_TID_STATS = 4,
    RV_TCG_RECORD_ATTACH_FAILURE = 5,
    RV_TCG_RECORD_GATE = 6,
    RV_TCG_RECORD_QUALITY = 7,
};

enum rv_tcg_gate_reason {
    RV_TCG_GATE_CONTROL = 1,
    RV_TCG_GATE_SHUTDOWN = 2,
};

enum rv_tcg_quality_status {
    RV_TCG_QUALITY_GOOD = 0,
    RV_TCG_QUALITY_DEGRADED = 1,
    RV_TCG_QUALITY_BAD = 2,
};

struct __attribute__((packed)) rv_tcg_file_header {
    char magic[8];
    uint16_t version;
    uint16_t header_size;
    uint32_t endian_marker;
    uint64_t start_monotonic_ns;
    uint64_t target_pid;
    uint64_t period_ns;
    uint64_t sample_type;
    uint32_t clock_id;
    uint32_t data_pages;
    uint64_t reserved[2];
};

struct __attribute__((packed)) rv_tcg_record_header {
    uint16_t type;
    uint16_t size;
    uint32_t flags;
};

struct __attribute__((packed)) rv_tcg_sample_record {
    uint64_t ip;
    uint64_t time_ns;
    uint64_t period_ns;
    uint32_t pid;
    uint32_t tid;
    uint32_t cpu;
    uint32_t reserved;
};

struct __attribute__((packed)) rv_tcg_lost_record {
    uint64_t time_ns;
    uint64_t event_id;
    uint64_t lost;
    uint32_t tid;
    uint32_t reserved;
};

struct __attribute__((packed)) rv_tcg_thread_record {
    uint64_t time_ns;
    uint32_t pid;
    uint32_t tid;
    uint32_t real_uid;
    uint32_t effective_uid;
    int32_t attach_errno;
    uint32_t reserved;
    char comm[RV_TCG_COMM_LEN];
};

struct __attribute__((packed)) rv_tcg_tid_stats_record {
    uint64_t time_ns;
    uint64_t task_clock_ns;
    uint64_t time_enabled_ns;
    uint64_t time_running_ns;
    uint64_t samples_seen;
    uint64_t samples_written;
    uint64_t samples_discarded;
    uint64_t lost;
    uint64_t throttle_records;
    uint64_t unthrottle_records;
    uint32_t tid;
    int32_t attach_errno;
    int32_t read_errno;
    uint32_t reserved;
};

struct __attribute__((packed)) rv_tcg_attach_failure_record {
    uint64_t time_ns;
    uint32_t tid;
    int32_t error;
    uint32_t effective_uid;
    uint32_t reserved;
};

struct __attribute__((packed)) rv_tcg_gate_record {
    uint64_t time_ns;
    uint32_t enabled;
    uint32_t reason;
};

struct __attribute__((packed)) rv_tcg_quality_record {
    uint64_t time_ns;
    uint64_t runtime_ns;
    uint64_t gate_active_ns;
    uint64_t task_clock_ns;
    uint64_t time_enabled_ns;
    uint64_t time_running_ns;
    uint64_t samples_seen;
    uint64_t samples_written;
    uint64_t samples_discarded;
    uint64_t lost;
    uint64_t throttle_records;
    uint64_t unthrottle_records;
    uint64_t running_ratio_ppm;
    uint64_t loss_ratio_ppm;
    uint32_t tids_discovered;
    uint32_t tids_attached;
    uint32_t attach_failures;
    uint32_t gate_transitions;
    uint32_t malformed_records;
    uint32_t status;
};

_Static_assert(sizeof(struct rv_tcg_file_header) == 72, "file header layout");
_Static_assert(sizeof(struct rv_tcg_record_header) == 8, "record header layout");
_Static_assert(sizeof(struct rv_tcg_sample_record) == 40, "sample layout");

struct event_state {
    struct event_state *next;
    int fd;
    pid_t tid;
    uid_t real_uid;
    uid_t effective_uid;
    int attach_errno;
    int read_errno;
    bool enabled;
    bool ended;
    void *mapping;
    size_t mapping_size;
    struct perf_event_mmap_page *metadata;
    uint8_t *data;
    uint64_t data_size;
    uint64_t samples_seen;
    uint64_t samples_written;
    uint64_t samples_discarded;
    uint64_t lost;
    uint64_t throttle_records;
    uint64_t unthrottle_records;
    uint64_t task_clock_ns;
    uint64_t time_enabled_ns;
    uint64_t time_running_ns;
    char comm[RV_TCG_COMM_LEN];
};

struct collector {
    pid_t target_pid;
    const char *output_path;
    const char *control_path;
    const char *ready_path;
    uint64_t period_ns;
    uint64_t start_ns;
    uint64_t gate_start_ns;
    uint64_t gate_active_ns;
    uint64_t timer_ticks;
    bool gate_enabled;
    bool gate_known;
    bool output_failed;
    FILE *output;
    int epoll_fd;
    int timer_fd;
    int inotify_fd;
    int inotify_watch;
    int control_fd;
    int timer_tag;
    int inotify_tag;
    struct event_state *events;
    uint32_t tids_discovered;
    uint32_t tids_attached;
    uint32_t attach_failures;
    uint32_t gate_transitions;
    uint32_t malformed_records;
};

struct read_count {
    uint64_t value;
    uint64_t time_enabled;
    uint64_t time_running;
};

static volatile sig_atomic_t stop_requested;

static void handle_signal(int signal_number)
{
    (void)signal_number;
    stop_requested = 1;
}

static uint64_t monotonic_ns(void)
{
    struct timespec timestamp;

    if (clock_gettime(CLOCK_MONOTONIC, &timestamp) != 0) {
        perror("clock_gettime");
        exit(EXIT_FAILURE);
    }
    return (uint64_t)timestamp.tv_sec * 1000000000ULL +
           (uint64_t)timestamp.tv_nsec;
}

static int perf_event_open(struct perf_event_attr *attr, pid_t pid, int cpu,
                           int group_fd, unsigned long flags)
{
    return (int)syscall(SYS_perf_event_open, attr, pid, cpu, group_fd, flags);
}

static bool output_bytes(struct collector *collector, const void *data,
                         size_t length)
{
    if (collector->output_failed)
        return false;
    if (fwrite(data, 1, length, collector->output) != length) {
        fprintf(stderr, "rv-tcg-time-collect: output write failed: %s\n",
                strerror(errno));
        collector->output_failed = true;
        stop_requested = 1;
        return false;
    }
    return true;
}

static bool output_record(struct collector *collector, uint16_t type,
                          uint32_t flags, const void *payload,
                          size_t payload_size)
{
    struct rv_tcg_record_header header;
    size_t total_size = sizeof(header) + payload_size;

    if (total_size > UINT16_MAX) {
        errno = EOVERFLOW;
        collector->output_failed = true;
        stop_requested = 1;
        return false;
    }
    header.type = htole16(type);
    header.size = htole16((uint16_t)total_size);
    header.flags = htole32(flags);
    return output_bytes(collector, &header, sizeof(header)) &&
           output_bytes(collector, payload, payload_size);
}

static void emit_gate(struct collector *collector, bool enabled,
                      uint32_t reason, uint64_t now)
{
    struct rv_tcg_gate_record record = {
        .time_ns = htole64(now),
        .enabled = htole32(enabled ? 1U : 0U),
        .reason = htole32(reason),
    };

    (void)output_record(collector, RV_TCG_RECORD_GATE, 0, &record,
                        sizeof(record));
}

static void emit_attach_failure(struct collector *collector,
                                const struct event_state *event, int error)
{
    struct rv_tcg_attach_failure_record record = {
        .time_ns = htole64(monotonic_ns()),
        .tid = htole32((uint32_t)event->tid),
        .error = (int32_t)htole32((uint32_t)error),
        .effective_uid = htole32((uint32_t)event->effective_uid),
        .reserved = 0,
    };

    (void)output_record(collector, RV_TCG_RECORD_ATTACH_FAILURE, 0, &record,
                        sizeof(record));
}

static void emit_thread(struct collector *collector,
                        const struct event_state *event)
{
    struct rv_tcg_thread_record record;

    memset(&record, 0, sizeof(record));
    record.time_ns = htole64(monotonic_ns());
    record.pid = htole32((uint32_t)collector->target_pid);
    record.tid = htole32((uint32_t)event->tid);
    record.real_uid = htole32((uint32_t)event->real_uid);
    record.effective_uid = htole32((uint32_t)event->effective_uid);
    record.attach_errno =
        (int32_t)htole32((uint32_t)event->attach_errno);
    memcpy(record.comm, event->comm, sizeof(record.comm));
    (void)output_record(collector, RV_TCG_RECORD_THREAD, 0, &record,
                        sizeof(record));
}

static void copy_from_ring(const struct event_state *event, uint64_t offset,
                           void *destination, size_t length)
{
    uint64_t position = offset & (event->data_size - 1U);
    size_t first = length;

    if (position + first > event->data_size)
        first = (size_t)(event->data_size - position);
    memcpy(destination, event->data + position, first);
    if (first < length)
        memcpy((uint8_t *)destination + first, event->data, length - first);
}

static bool read_u64(const uint8_t *record, size_t size, size_t *offset,
                     uint64_t *value)
{
    if (*offset > size || size - *offset < sizeof(*value))
        return false;
    memcpy(value, record + *offset, sizeof(*value));
    *offset += sizeof(*value);
    return true;
}

static bool read_u32(const uint8_t *record, size_t size, size_t *offset,
                     uint32_t *value)
{
    if (*offset > size || size - *offset < sizeof(*value))
        return false;
    memcpy(value, record + *offset, sizeof(*value));
    *offset += sizeof(*value);
    return true;
}

static void process_sample(struct collector *collector,
                           struct event_state *event, const uint8_t *record,
                           size_t size, bool write_sample)
{
    struct rv_tcg_sample_record output;
    size_t offset = sizeof(struct perf_event_header);
    uint64_t ip;
    uint64_t time_ns;
    uint64_t period_ns;
    uint32_t pid;
    uint32_t tid;
    uint32_t cpu;
    uint32_t reserved;

    if (!read_u64(record, size, &offset, &ip) ||
        !read_u32(record, size, &offset, &pid) ||
        !read_u32(record, size, &offset, &tid) ||
        !read_u64(record, size, &offset, &time_ns) ||
        !read_u32(record, size, &offset, &cpu) ||
        !read_u32(record, size, &offset, &reserved) ||
        !read_u64(record, size, &offset, &period_ns)) {
        collector->malformed_records++;
        return;
    }

    event->samples_seen++;
    if (!write_sample) {
        event->samples_discarded++;
        return;
    }

    output.ip = htole64(ip);
    output.time_ns = htole64(time_ns);
    output.period_ns = htole64(period_ns);
    output.pid = htole32(pid);
    output.tid = htole32(tid);
    output.cpu = htole32(cpu);
    output.reserved = 0;
    if (output_record(collector, RV_TCG_RECORD_SAMPLE, 0, &output,
                      sizeof(output)))
        event->samples_written++;
}

static void process_lost(struct collector *collector,
                         struct event_state *event, const uint8_t *record,
                         size_t size)
{
    struct rv_tcg_lost_record output;
    size_t offset = sizeof(struct perf_event_header);
    uint64_t id;
    uint64_t lost;

    if (!read_u64(record, size, &offset, &id) ||
        !read_u64(record, size, &offset, &lost)) {
        collector->malformed_records++;
        return;
    }
    event->lost += lost;
    output.time_ns = htole64(monotonic_ns());
    output.event_id = htole64(id);
    output.lost = htole64(lost);
    output.tid = htole32((uint32_t)event->tid);
    output.reserved = 0;
    (void)output_record(collector, RV_TCG_RECORD_LOST, 0, &output,
                        sizeof(output));
}

static void process_lost_samples(struct collector *collector,
                                 struct event_state *event,
                                 const uint8_t *record, size_t size)
{
    struct rv_tcg_lost_record output;
    size_t offset = sizeof(struct perf_event_header);
    uint64_t lost;

    if (!read_u64(record, size, &offset, &lost)) {
        collector->malformed_records++;
        return;
    }
    event->lost += lost;
    output.time_ns = htole64(monotonic_ns());
    output.event_id = 0;
    output.lost = htole64(lost);
    output.tid = htole32((uint32_t)event->tid);
    output.reserved = 0;
    (void)output_record(collector, RV_TCG_RECORD_LOST, 1U, &output,
                        sizeof(output));
}

static void drain_ring(struct collector *collector, struct event_state *event,
                       bool write_samples)
{
    struct perf_event_mmap_page *metadata = event->metadata;
    uint64_t head;
    uint64_t tail;

    if (metadata == NULL)
        return;
    head = __atomic_load_n(&metadata->data_head, __ATOMIC_ACQUIRE);
    tail = metadata->data_tail;
    while (tail < head) {
        struct perf_event_header header;
        uint8_t stack_record[256];
        uint8_t *record = stack_record;

        copy_from_ring(event, tail, &header, sizeof(header));
        if (header.size < sizeof(header) || header.size > event->data_size ||
            header.size > head - tail) {
            collector->malformed_records++;
            tail = head;
            break;
        }
        if (header.size > sizeof(stack_record)) {
            record = malloc(header.size);
            if (record == NULL) {
                fprintf(stderr,
                        "rv-tcg-time-collect: cannot allocate ring record\n");
                stop_requested = 1;
                tail = head;
                break;
            }
        }
        copy_from_ring(event, tail, record, header.size);
        if (header.type == PERF_RECORD_SAMPLE)
            process_sample(collector, event, record, header.size,
                           write_samples);
        else if (header.type == PERF_RECORD_LOST)
            process_lost(collector, event, record, header.size);
        else if (header.type == 13U) /* PERF_RECORD_LOST_SAMPLES */
            process_lost_samples(collector, event, record, header.size);
        else if (header.type == PERF_RECORD_THROTTLE)
            event->throttle_records++;
        else if (header.type == PERF_RECORD_UNTHROTTLE)
            event->unthrottle_records++;
        if (record != stack_record)
            free(record);
        tail += header.size;
    }
    __atomic_store_n(&metadata->data_tail, tail, __ATOMIC_RELEASE);
}

static struct event_state *find_event(struct collector *collector, pid_t tid)
{
    struct event_state *event;

    for (event = collector->events; event != NULL; event = event->next) {
        if (event->tid == tid)
            return event;
    }
    return NULL;
}

static int read_thread_identity(pid_t pid, pid_t tid, uid_t *real_uid,
                                uid_t *effective_uid,
                                char comm[RV_TCG_COMM_LEN])
{
    char path[128];
    char line[256];
    FILE *status;
    bool have_uid = false;

    if (snprintf(path, sizeof(path), "/proc/%ld/task/%ld/status", (long)pid,
                 (long)tid) >= (int)sizeof(path))
        return ENAMETOOLONG;
    status = fopen(path, "re");
    if (status == NULL)
        return errno;
    comm[0] = '\0';
    while (fgets(line, sizeof(line), status) != NULL) {
        if (strncmp(line, "Name:", 5) == 0) {
            char *name = line + 5;
            size_t length;

            while (*name == ' ' || *name == '\t')
                name++;
            length = strcspn(name, "\r\n");
            if (length >= RV_TCG_COMM_LEN)
                length = RV_TCG_COMM_LEN - 1U;
            memcpy(comm, name, length);
            comm[length] = '\0';
        } else if (strncmp(line, "Uid:", 4) == 0) {
            unsigned int real;
            unsigned int effective;

            if (sscanf(line + 4, "%u %u", &real, &effective) == 2) {
                *real_uid = (uid_t)real;
                *effective_uid = (uid_t)effective;
                have_uid = true;
            }
        }
    }
    if (ferror(status)) {
        int error = errno != 0 ? errno : EIO;

        fclose(status);
        return error;
    }
    fclose(status);
    if (!have_uid)
        return EPROTO;
    if (comm[0] == '\0')
        snprintf(comm, RV_TCG_COMM_LEN, "tid-%ld", (long)tid);
    return 0;
}

static int add_epoll_fd(struct collector *collector, int fd, void *pointer,
                        uint32_t events)
{
    struct epoll_event event;

    memset(&event, 0, sizeof(event));
    event.events = events;
    event.data.ptr = pointer;
    return epoll_ctl(collector->epoll_fd, EPOLL_CTL_ADD, fd, &event);
}

static int attach_event(struct collector *collector, struct event_state *event)
{
    struct perf_event_attr attr;
    long page_size = sysconf(_SC_PAGESIZE);
    size_t mapping_size;
    void *mapping;
    int fd;

    if (page_size <= 0)
        return errno != 0 ? errno : EINVAL;
    memset(&attr, 0, sizeof(attr));
    attr.type = PERF_TYPE_SOFTWARE;
    attr.size = sizeof(attr);
    attr.config = PERF_COUNT_SW_TASK_CLOCK;
    attr.sample_period = collector->period_ns;
    attr.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME |
                       PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD;
    attr.read_format = PERF_FORMAT_TOTAL_TIME_ENABLED |
                       PERF_FORMAT_TOTAL_TIME_RUNNING;
    attr.disabled = 1;
    attr.exclude_kernel = 1;
    attr.exclude_hv = 1;
    attr.watermark = 1;
    attr.wakeup_watermark =
        (uint32_t)((uint64_t)page_size * RV_TCG_DATA_PAGES / 4U);
    attr.use_clockid = 1;
    attr.clockid = CLOCK_MONOTONIC;

    fd = perf_event_open(&attr, event->tid, -1, -1, PERF_FLAG_FD_CLOEXEC);
    if (fd < 0)
        return errno;
    mapping_size = (size_t)page_size * (RV_TCG_DATA_PAGES + 1U);
    mapping = mmap(NULL, mapping_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                   0);
    if (mapping == MAP_FAILED) {
        int error = errno;

        close(fd);
        return error;
    }
    event->fd = fd;
    event->mapping = mapping;
    event->mapping_size = mapping_size;
    event->metadata = mapping;
    event->data = (uint8_t *)mapping + page_size;
    event->data_size = (uint64_t)page_size * RV_TCG_DATA_PAGES;
    if (event->metadata->data_offset != 0)
        event->data = (uint8_t *)mapping + event->metadata->data_offset;
    if (event->metadata->data_size != 0)
        event->data_size = event->metadata->data_size;
    if (event->data_size == 0 ||
        (event->data_size & (event->data_size - 1U)) != 0 ||
        event->data < (uint8_t *)mapping ||
        (uint64_t)(event->data - (uint8_t *)mapping) > mapping_size ||
        event->data_size >
            mapping_size - (size_t)(event->data - (uint8_t *)mapping)) {
        munmap(mapping, mapping_size);
        close(fd);
        event->fd = -1;
        event->mapping = NULL;
        event->metadata = NULL;
        return EPROTO;
    }
    if (add_epoll_fd(collector, fd, event, EPOLLIN | EPOLLHUP | EPOLLERR) !=
        0) {
        int error = errno;

        munmap(mapping, mapping_size);
        close(fd);
        event->fd = -1;
        event->mapping = NULL;
        event->metadata = NULL;
        return error;
    }
    if (collector->gate_enabled) {
        drain_ring(collector, event, false);
        if (ioctl(fd, PERF_EVENT_IOC_ENABLE, 0) != 0) {
            int error = errno;

            (void)epoll_ctl(collector->epoll_fd, EPOLL_CTL_DEL, fd, NULL);
            munmap(mapping, mapping_size);
            close(fd);
            event->fd = -1;
            event->mapping = NULL;
            event->metadata = NULL;
            return error;
        }
        event->enabled = true;
    }
    return 0;
}

static int discover_tid(struct collector *collector, pid_t tid)
{
    struct event_state *event;
    int error;

    if (find_event(collector, tid) != NULL)
        return 0;
    event = calloc(1, sizeof(*event));
    if (event == NULL)
        return ENOMEM;
    event->fd = -1;
    event->tid = tid;
    event->real_uid = (uid_t)-1;
    event->effective_uid = (uid_t)-1;
    event->next = collector->events;
    collector->events = event;
    collector->tids_discovered++;

    error = read_thread_identity(collector->target_pid, tid, &event->real_uid,
                                 &event->effective_uid, event->comm);
    if (error == 0 && event->effective_uid != geteuid() &&
        event->real_uid != getuid())
        error = EACCES;
    if (error == 0)
        error = attach_event(collector, event);
    event->attach_errno = error;
    if (error == 0) {
        collector->tids_attached++;
    } else {
        collector->attach_failures++;
        if (event->comm[0] == '\0')
            snprintf(event->comm, sizeof(event->comm), "tid-%ld",
                     (long)tid);
        emit_attach_failure(collector, event, error);
    }
    emit_thread(collector, event);
    return 0;
}

static int rescan_threads(struct collector *collector, bool initial)
{
    char path[64];
    struct dirent *entry;
    DIR *directory;
    uint32_t before = collector->tids_discovered;

    if (snprintf(path, sizeof(path), "/proc/%ld/task",
                 (long)collector->target_pid) >= (int)sizeof(path))
        return ENAMETOOLONG;
    directory = opendir(path);
    if (directory == NULL)
        return errno;
    for (;;) {
        char *end;
        long value;
        int error;

        errno = 0;
        entry = readdir(directory);
        if (entry == NULL) {
            error = errno;
            closedir(directory);
            if (error != 0)
                return error;
            break;
        }
        if (entry->d_name[0] == '.')
            continue;
        errno = 0;
        value = strtol(entry->d_name, &end, 10);
        if (errno != 0 || *end != '\0' || value <= 0 || value > INT32_MAX)
            continue;
        error = discover_tid(collector, (pid_t)value);
        if (error != 0) {
            closedir(directory);
            return error;
        }
    }
    if (initial && collector->tids_discovered == before)
        return ESRCH;
    return 0;
}

static void disable_all(struct collector *collector)
{
    struct event_state *event;

    for (event = collector->events; event != NULL; event = event->next) {
        if (event->fd >= 0 && event->enabled) {
            (void)ioctl(event->fd, PERF_EVENT_IOC_DISABLE, 0);
            event->enabled = false;
        }
    }
}

static void enable_all(struct collector *collector)
{
    struct event_state *event;

    for (event = collector->events; event != NULL; event = event->next) {
        if (event->fd < 0 || event->ended || event->enabled)
            continue;
        drain_ring(collector, event, false);
        if (ioctl(event->fd, PERF_EVENT_IOC_ENABLE, 0) == 0)
            event->enabled = true;
    }
}

static void drain_all(struct collector *collector, bool write_samples)
{
    struct event_state *event;

    for (event = collector->events; event != NULL; event = event->next)
        drain_ring(collector, event, write_samples);
}

static void set_gate(struct collector *collector, bool enabled,
                     uint32_t reason)
{
    uint64_t now;

    if (collector->gate_known && collector->gate_enabled == enabled)
        return;
    now = monotonic_ns();
    if (enabled) {
        drain_all(collector, false);
        collector->gate_enabled = true;
        collector->gate_known = true;
        collector->gate_start_ns = now;
        enable_all(collector);
    } else {
        disable_all(collector);
        drain_all(collector, collector->gate_known && collector->gate_enabled);
        if (collector->gate_known && collector->gate_enabled)
            collector->gate_active_ns += now - collector->gate_start_ns;
        collector->gate_enabled = false;
        collector->gate_known = true;
    }
    collector->gate_transitions++;
    emit_gate(collector, enabled, reason, now);
}

static int open_control(struct collector *collector)
{
    int fd = open(collector->control_path, O_RDONLY | O_CLOEXEC);

    if (fd < 0)
        return errno;
    if (collector->control_fd >= 0)
        close(collector->control_fd);
    collector->control_fd = fd;
    return 0;
}

static int read_control_value(struct collector *collector, bool *enabled)
{
    char buffer[64];
    struct stat current_status;
    struct stat path_status;
    ssize_t length;
    size_t position = 0;

    if (collector->control_fd < 0) {
        int error = open_control(collector);

        if (error != 0)
            return error;
    }
    if (fstat(collector->control_fd, &current_status) != 0)
        return errno;
    if (stat(collector->control_path, &path_status) != 0)
        return errno;
    if (current_status.st_dev != path_status.st_dev ||
        current_status.st_ino != path_status.st_ino) {
        int error = open_control(collector);

        if (error != 0)
            return error;
    }
    length = pread(collector->control_fd, buffer, sizeof(buffer) - 1U, 0);
    if (length < 0) {
        int error = errno;

        close(collector->control_fd);
        collector->control_fd = -1;
        return error;
    }
    buffer[length] = '\0';
    while (position < (size_t)length &&
           (buffer[position] == ' ' || buffer[position] == '\t' ||
            buffer[position] == '\r' || buffer[position] == '\n'))
        position++;
    if (position >= (size_t)length ||
        (buffer[position] != '0' && buffer[position] != '1'))
        return EINVAL;
    *enabled = buffer[position++] == '1';
    while (position < (size_t)length) {
        if (buffer[position] != ' ' && buffer[position] != '\t' &&
            buffer[position] != '\r' && buffer[position] != '\n')
            return EINVAL;
        position++;
    }
    return 0;
}

static void refresh_gate(struct collector *collector)
{
    bool enabled = false;
    int error = read_control_value(collector, &enabled);

    if (error == 0) {
        set_gate(collector, enabled, RV_TCG_GATE_CONTROL);
    } else if (error != ENOENT) {
        static int last_error;

        if (error != last_error) {
            fprintf(stderr,
                    "rv-tcg-time-collect: cannot read control %s: %s\n",
                    collector->control_path, strerror(error));
            last_error = error;
        }
    }
}

static int setup_inotify(struct collector *collector)
{
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);

    if (fd < 0)
        return errno;
    collector->inotify_fd = fd;
    collector->inotify_watch =
        inotify_add_watch(fd, collector->control_path,
                          IN_MODIFY | IN_CLOSE_WRITE | IN_ATTRIB |
                              IN_MOVE_SELF | IN_DELETE_SELF);
    if (collector->inotify_watch < 0) {
        int error = errno;

        close(fd);
        collector->inotify_fd = -1;
        return error;
    }
    if (add_epoll_fd(collector, fd, &collector->inotify_tag, EPOLLIN) != 0) {
        int error = errno;

        close(fd);
        collector->inotify_fd = -1;
        collector->inotify_watch = -1;
        return error;
    }
    return 0;
}

static void drain_inotify(struct collector *collector)
{
    uint8_t buffer[4096];
    ssize_t length;

    do {
        length = read(collector->inotify_fd, buffer, sizeof(buffer));
    } while (length > 0);
    refresh_gate(collector);
}

static int setup_timer(struct collector *collector)
{
    struct itimerspec interval;
    int fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);

    if (fd < 0)
        return errno;
    memset(&interval, 0, sizeof(interval));
    interval.it_value.tv_nsec = (long)RV_TCG_TIMER_NS;
    interval.it_interval.tv_nsec = (long)RV_TCG_TIMER_NS;
    if (timerfd_settime(fd, 0, &interval, NULL) != 0) {
        int error = errno;

        close(fd);
        return error;
    }
    collector->timer_fd = fd;
    if (add_epoll_fd(collector, fd, &collector->timer_tag, EPOLLIN) != 0) {
        int error = errno;

        close(fd);
        collector->timer_fd = -1;
        return error;
    }
    return 0;
}

static bool target_exists(const struct collector *collector)
{
    char path[64];

    if (snprintf(path, sizeof(path), "/proc/%ld/task",
                 (long)collector->target_pid) >= (int)sizeof(path))
        return false;
    return access(path, F_OK) == 0;
}

static void handle_timer(struct collector *collector)
{
    uint64_t expirations = 0;
    ssize_t length = read(collector->timer_fd, &expirations,
                          sizeof(expirations));

    if (length != (ssize_t)sizeof(expirations))
        expirations = 1;
    collector->timer_ticks += expirations;
    refresh_gate(collector);
    if (collector->timer_ticks >= RV_TCG_RESCAN_TICKS) {
        int error;

        collector->timer_ticks %= RV_TCG_RESCAN_TICKS;
        error = rescan_threads(collector, false);
        if (error != 0 && error != ENOENT && error != ESRCH)
            fprintf(stderr, "rv-tcg-time-collect: thread rescan: %s\n",
                    strerror(error));
    }
    if (!target_exists(collector))
        stop_requested = 1;
}

static int write_ready_file(const struct collector *collector)
{
    char *temporary;
    size_t length;
    int fd;
    int result = 0;

    if (collector->ready_path == NULL)
        return 0;
    length = strlen(collector->ready_path) + 64U;
    temporary = malloc(length);
    if (temporary == NULL)
        return ENOMEM;
    snprintf(temporary, length, "%s.tmp.%ld", collector->ready_path,
             (long)getpid());
    fd = open(temporary, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (fd < 0) {
        result = errno;
        goto done;
    }
    if (dprintf(fd,
                "collector_pid=%ld\ntarget_pid=%ld\ntids_discovered=%u\n"
                "tids_attached=%u\nattach_failures=%u\n",
                (long)getpid(), (long)collector->target_pid,
                collector->tids_discovered, collector->tids_attached,
                collector->attach_failures) < 0 ||
        fsync(fd) != 0) {
        result = errno != 0 ? errno : EIO;
        close(fd);
        unlink(temporary);
        goto done;
    }
    if (close(fd) != 0) {
        result = errno;
        unlink(temporary);
        goto done;
    }
    if (rename(temporary, collector->ready_path) != 0) {
        result = errno;
        unlink(temporary);
    }
done:
    free(temporary);
    return result;
}

static uint64_t ratio_ppm(uint64_t numerator, uint64_t denominator)
{
    if (denominator == 0)
        return 0;
    if (numerator > UINT64_MAX / 1000000ULL) {
        long double ratio = (long double)numerator / (long double)denominator;

        return (uint64_t)(ratio * 1000000.0L);
    }
    return numerator * 1000000ULL / denominator;
}

static void read_event_count(struct event_state *event)
{
    struct read_count count;
    ssize_t length;

    if (event->fd < 0)
        return;
    length = read(event->fd, &count, sizeof(count));
    if (length != (ssize_t)sizeof(count)) {
        event->read_errno = length < 0 ? errno : EIO;
        return;
    }
    event->task_clock_ns = count.value;
    event->time_enabled_ns = count.time_enabled;
    event->time_running_ns = count.time_running;
}

static void emit_tid_stats(struct collector *collector,
                           const struct event_state *event, uint64_t now)
{
    struct rv_tcg_tid_stats_record record = {
        .time_ns = htole64(now),
        .task_clock_ns = htole64(event->task_clock_ns),
        .time_enabled_ns = htole64(event->time_enabled_ns),
        .time_running_ns = htole64(event->time_running_ns),
        .samples_seen = htole64(event->samples_seen),
        .samples_written = htole64(event->samples_written),
        .samples_discarded = htole64(event->samples_discarded),
        .lost = htole64(event->lost),
        .throttle_records = htole64(event->throttle_records),
        .unthrottle_records = htole64(event->unthrottle_records),
        .tid = htole32((uint32_t)event->tid),
        .attach_errno =
            (int32_t)htole32((uint32_t)event->attach_errno),
        .read_errno = (int32_t)htole32((uint32_t)event->read_errno),
        .reserved = 0,
    };

    (void)output_record(collector, RV_TCG_RECORD_TID_STATS, 0, &record,
                        sizeof(record));
}

static uint32_t choose_quality_status(const struct collector *collector,
                                      uint64_t samples_seen, uint64_t lost,
                                      uint64_t running_ratio,
                                      uint64_t throttle_records)
{
    uint64_t loss_ratio = ratio_ppm(lost, samples_seen + lost);

    if (collector->tids_attached == 0 || samples_seen == 0 ||
        loss_ratio > 10000ULL ||
        (running_ratio != 0 && running_ratio < 900000ULL) ||
        collector->malformed_records != 0)
        return RV_TCG_QUALITY_BAD;
    if (collector->attach_failures != 0 || lost != 0 ||
        throttle_records != 0 ||
        (running_ratio != 0 && running_ratio < 990000ULL))
        return RV_TCG_QUALITY_DEGRADED;
    return RV_TCG_QUALITY_GOOD;
}

static void finalize(struct collector *collector)
{
    struct event_state *event;
    uint64_t now;
    uint64_t task_clock = 0;
    uint64_t time_enabled = 0;
    uint64_t time_running = 0;
    uint64_t samples_seen = 0;
    uint64_t samples_written = 0;
    uint64_t samples_discarded = 0;
    uint64_t lost = 0;
    uint64_t throttle_records = 0;
    uint64_t unthrottle_records = 0;
    uint64_t running_ratio;
    uint64_t loss_ratio;
    uint32_t status;
    struct rv_tcg_quality_record quality;

    if (collector->gate_known && collector->gate_enabled)
        set_gate(collector, false, RV_TCG_GATE_SHUTDOWN);
    else {
        disable_all(collector);
        drain_all(collector, false);
    }
    now = monotonic_ns();
    for (event = collector->events; event != NULL; event = event->next) {
        read_event_count(event);
        emit_tid_stats(collector, event, now);
        task_clock += event->task_clock_ns;
        time_enabled += event->time_enabled_ns;
        time_running += event->time_running_ns;
        samples_seen += event->samples_seen;
        samples_written += event->samples_written;
        samples_discarded += event->samples_discarded;
        lost += event->lost;
        throttle_records += event->throttle_records;
        unthrottle_records += event->unthrottle_records;
        fprintf(stderr,
                "RVTCG_TID tid=%ld comm=\"%s\" attached=%d "
                "attach_errno=%d read_errno=%d task_clock_ns=%" PRIu64
                " time_enabled_ns=%" PRIu64 " time_running_ns=%" PRIu64
                " samples=%" PRIu64 " written=%" PRIu64
                " discarded=%" PRIu64 " lost=%" PRIu64
                " throttle=%" PRIu64 " unthrottle=%" PRIu64 "\n",
                (long)event->tid, event->comm, event->fd >= 0,
                event->attach_errno, event->read_errno,
                event->task_clock_ns, event->time_enabled_ns,
                event->time_running_ns, event->samples_seen,
                event->samples_written, event->samples_discarded, event->lost,
                event->throttle_records, event->unthrottle_records);
    }
    running_ratio = ratio_ppm(time_running, time_enabled);
    loss_ratio = ratio_ppm(lost, samples_seen + lost);
    status = choose_quality_status(collector, samples_seen, lost, running_ratio,
                                   throttle_records);
    memset(&quality, 0, sizeof(quality));
    quality.time_ns = htole64(now);
    quality.runtime_ns = htole64(now - collector->start_ns);
    quality.gate_active_ns = htole64(collector->gate_active_ns);
    quality.task_clock_ns = htole64(task_clock);
    quality.time_enabled_ns = htole64(time_enabled);
    quality.time_running_ns = htole64(time_running);
    quality.samples_seen = htole64(samples_seen);
    quality.samples_written = htole64(samples_written);
    quality.samples_discarded = htole64(samples_discarded);
    quality.lost = htole64(lost);
    quality.throttle_records = htole64(throttle_records);
    quality.unthrottle_records = htole64(unthrottle_records);
    quality.running_ratio_ppm = htole64(running_ratio);
    quality.loss_ratio_ppm = htole64(loss_ratio);
    quality.tids_discovered = htole32(collector->tids_discovered);
    quality.tids_attached = htole32(collector->tids_attached);
    quality.attach_failures = htole32(collector->attach_failures);
    quality.gate_transitions = htole32(collector->gate_transitions);
    quality.malformed_records = htole32(collector->malformed_records);
    quality.status = htole32(status);
    (void)output_record(collector, RV_TCG_RECORD_QUALITY, 0, &quality,
                        sizeof(quality));
    fprintf(stderr,
            "RVTCG_QUALITY status=%s target_pid=%ld tids_discovered=%u "
            "tids_attached=%u attach_failures=%u samples=%" PRIu64
            " written=%" PRIu64 " discarded=%" PRIu64 " lost=%" PRIu64
            " throttle=%" PRIu64 " unthrottle=%" PRIu64
            " loss_ppm=%" PRIu64 " running_ppm=%" PRIu64
            " gate_active_ns=%" PRIu64 " malformed=%u\n",
            status == RV_TCG_QUALITY_GOOD
                ? "good"
                : (status == RV_TCG_QUALITY_DEGRADED ? "degraded" : "bad"),
            (long)collector->target_pid, collector->tids_discovered,
            collector->tids_attached, collector->attach_failures, samples_seen,
            samples_written, samples_discarded, lost, throttle_records,
            unthrottle_records, loss_ratio, running_ratio,
            collector->gate_active_ns, collector->malformed_records);
    if (fflush(collector->output) != 0)
        collector->output_failed = true;
    if (collector->output != stdout) {
        struct stat status_buffer;
        int output_fd = fileno(collector->output);

        if (fstat(output_fd, &status_buffer) != 0 ||
            (S_ISREG(status_buffer.st_mode) && fsync(output_fd) != 0))
            collector->output_failed = true;
    }
}

static void cleanup(struct collector *collector)
{
    struct event_state *event = collector->events;

    while (event != NULL) {
        struct event_state *next = event->next;

        if (event->mapping != NULL)
            munmap(event->mapping, event->mapping_size);
        if (event->fd >= 0)
            close(event->fd);
        free(event);
        event = next;
    }
    if (collector->inotify_fd >= 0)
        close(collector->inotify_fd);
    if (collector->timer_fd >= 0)
        close(collector->timer_fd);
    if (collector->control_fd >= 0)
        close(collector->control_fd);
    if (collector->epoll_fd >= 0)
        close(collector->epoll_fd);
    if (collector->output != NULL && collector->output != stdout)
        fclose(collector->output);
}

static void write_file_header(struct collector *collector)
{
    struct rv_tcg_file_header header;

    memset(&header, 0, sizeof(header));
    memcpy(header.magic, "RVTCGT1", 7);
    header.version = htole16(RV_TCG_VERSION);
    header.header_size = htole16(sizeof(header));
    header.endian_marker = htole32(0x01020304U);
    header.start_monotonic_ns = htole64(collector->start_ns);
    header.target_pid = htole64((uint64_t)collector->target_pid);
    header.period_ns = htole64(collector->period_ns);
    header.sample_type =
        htole64(PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_TIME |
                PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD);
    header.clock_id = htole32(CLOCK_MONOTONIC);
    header.data_pages = htole32(RV_TCG_DATA_PAGES);
    (void)output_bytes(collector, &header, sizeof(header));
}

static int parse_pid(const char *text, pid_t *pid)
{
    char *end;
    long value;

    errno = 0;
    value = strtol(text, &end, 10);
    if (errno != 0 || *text == '\0' || *end != '\0' || value <= 0 ||
        value > INT32_MAX)
        return EINVAL;
    *pid = (pid_t)value;
    return 0;
}

static int parse_period(const char *text, uint64_t *period)
{
    char *end;
    unsigned long long value;

    errno = 0;
    value = strtoull(text, &end, 10);
    if (errno != 0 || *text == '\0' || *end != '\0' || value == 0)
        return EINVAL;
    *period = (uint64_t)value;
    return 0;
}

static void usage(FILE *stream, const char *program)
{
    fprintf(stream,
            "usage: %s --pid PID --output FILE --control FILE --period-ns N "
            "[--ready FILE]\n"
            "\n"
            "Sample PERF_COUNT_SW_TASK_CLOCK on every same-UID thread of PID.\n"
            "The control file must contain 0 or 1; only samples collected while "
            "it is 1 are written.\n",
            program);
}

int main(int argc, char **argv)
{
    static const struct option options[] = {
        {"pid", required_argument, NULL, 'p'},
        {"output", required_argument, NULL, 'o'},
        {"control", required_argument, NULL, 'c'},
        {"period-ns", required_argument, NULL, 'n'},
        {"ready", required_argument, NULL, 'r'},
        {"help", no_argument, NULL, 'h'},
        {NULL, 0, NULL, 0},
    };
    struct collector collector;
    struct sigaction action;
    struct epoll_event epoll_events[RV_TCG_MAX_EPOLL_EVENTS];
    bool have_pid = false;
    bool have_period = false;
    bool initial_gate = false;
    int option;
    int error;
    int exit_status = EXIT_SUCCESS;

    memset(&collector, 0, sizeof(collector));
    collector.epoll_fd = -1;
    collector.timer_fd = -1;
    collector.inotify_fd = -1;
    collector.inotify_watch = -1;
    collector.control_fd = -1;
    while ((option = getopt_long(argc, argv, "p:o:c:n:r:h", options, NULL)) !=
           -1) {
        switch (option) {
        case 'p':
            if (parse_pid(optarg, &collector.target_pid) != 0) {
                fprintf(stderr, "invalid PID: %s\n", optarg);
                return EXIT_FAILURE;
            }
            have_pid = true;
            break;
        case 'o':
            collector.output_path = optarg;
            break;
        case 'c':
            collector.control_path = optarg;
            break;
        case 'n':
            if (parse_period(optarg, &collector.period_ns) != 0) {
                fprintf(stderr, "invalid period: %s\n", optarg);
                return EXIT_FAILURE;
            }
            have_period = true;
            break;
        case 'r':
            collector.ready_path = optarg;
            break;
        case 'h':
            usage(stdout, argv[0]);
            return EXIT_SUCCESS;
        default:
            usage(stderr, argv[0]);
            return EXIT_FAILURE;
        }
    }
    if (!have_pid || collector.output_path == NULL ||
        collector.control_path == NULL || !have_period || optind != argc) {
        usage(stderr, argv[0]);
        return EXIT_FAILURE;
    }

    memset(&action, 0, sizeof(action));
    action.sa_handler = handle_signal;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGINT, &action, NULL) != 0 ||
        sigaction(SIGTERM, &action, NULL) != 0) {
        perror("sigaction");
        return EXIT_FAILURE;
    }
    signal(SIGPIPE, SIG_IGN);

    collector.start_ns = monotonic_ns();
    if (strcmp(collector.output_path, "-") == 0) {
        collector.output = stdout;
    } else {
        collector.output = fopen(collector.output_path, "wb");
        if (collector.output == NULL) {
            fprintf(stderr, "cannot open output %s: %s\n",
                    collector.output_path, strerror(errno));
            return EXIT_FAILURE;
        }
    }
    (void)setvbuf(collector.output, NULL, _IOFBF, 1024U * 1024U);
    write_file_header(&collector);

    error = open_control(&collector);
    if (error != 0) {
        fprintf(stderr, "cannot open control %s: %s\n", collector.control_path,
                strerror(error));
        cleanup(&collector);
        return EXIT_FAILURE;
    }
    collector.epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    if (collector.epoll_fd < 0) {
        perror("epoll_create1");
        cleanup(&collector);
        return EXIT_FAILURE;
    }
    error = setup_timer(&collector);
    if (error != 0) {
        fprintf(stderr, "cannot create timer: %s\n", strerror(error));
        cleanup(&collector);
        return EXIT_FAILURE;
    }
    error = setup_inotify(&collector);
    if (error != 0)
        fprintf(stderr,
                "rv-tcg-time-collect: inotify unavailable (%s), using timer\n",
                strerror(error));
    error = rescan_threads(&collector, true);
    if (error != 0) {
        fprintf(stderr, "cannot enumerate target threads: %s\n",
                strerror(error));
        finalize(&collector);
        cleanup(&collector);
        return EXIT_FAILURE;
    }
    error = read_control_value(&collector, &initial_gate);
    if (error != 0) {
        fprintf(stderr, "invalid control file %s: %s\n",
                collector.control_path, strerror(error));
        finalize(&collector);
        cleanup(&collector);
        return EXIT_FAILURE;
    }
    set_gate(&collector, initial_gate, RV_TCG_GATE_CONTROL);
    if (fflush(collector.output) != 0) {
        fprintf(stderr, "cannot flush output before ready: %s\n",
                strerror(errno));
        cleanup(&collector);
        return EXIT_FAILURE;
    }
    error = write_ready_file(&collector);
    if (error != 0) {
        fprintf(stderr, "cannot publish ready file %s: %s\n",
                collector.ready_path, strerror(error));
        finalize(&collector);
        cleanup(&collector);
        return EXIT_FAILURE;
    }

    while (!stop_requested) {
        int count = epoll_wait(collector.epoll_fd, epoll_events,
                               RV_TCG_MAX_EPOLL_EVENTS, -1);
        int index;

        if (count < 0) {
            if (errno == EINTR)
                continue;
            fprintf(stderr, "epoll_wait: %s\n", strerror(errno));
            exit_status = EXIT_FAILURE;
            break;
        }
        /* Apply control transitions before consuming samples in this batch. */
        for (index = 0; index < count; index++) {
            void *pointer = epoll_events[index].data.ptr;

            if (pointer == &collector.timer_tag)
                handle_timer(&collector);
            else if (pointer == &collector.inotify_tag)
                drain_inotify(&collector);
        }
        for (index = 0; index < count; index++) {
            void *pointer = epoll_events[index].data.ptr;
            struct event_state *event;

            if (pointer == &collector.timer_tag ||
                pointer == &collector.inotify_tag)
                continue;
            event = pointer;
            drain_ring(&collector, event, collector.gate_enabled);
            if ((epoll_events[index].events & (EPOLLHUP | EPOLLERR)) != 0 &&
                !event->ended) {
                (void)epoll_ctl(collector.epoll_fd, EPOLL_CTL_DEL, event->fd,
                                NULL);
                event->ended = true;
                event->enabled = false;
            }
        }
    }

    finalize(&collector);
    if (collector.output_failed)
        exit_status = EXIT_FAILURE;
    cleanup(&collector);
    return exit_status;
}
