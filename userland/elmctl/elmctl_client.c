#define _GNU_SOURCE

#include "elmctl_client.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int elmctl_syscall(uint32_t command, const void *input, size_t input_len, void *output,
                   size_t output_len, ssize_t *written)
{
    long ret = syscall(SYS_ELM_CTL, command, input, input_len, output, output_len);
    if (ret < 0) {
        return -1;
    }
    if (written != NULL) {
        *written = (ssize_t)ret;
    }
    return 0;
}

int elmctl_core_query(struct elm_core_info *info)
{
    ssize_t written = 0;
    if (elmctl_syscall(ELM_CTL_CMD_CORE_QUERY, NULL, 0, info, sizeof(*info), &written) != 0) {
        return -1;
    }
    if ((size_t)written != sizeof(*info) || info->magic != ELM_CTL_MAGIC ||
        info->abi_version != ELM_CTL_ABI_VERSION) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

int elmctl_snapshot(uint8_t *out, size_t out_len, ssize_t *written)
{
    return elmctl_syscall(ELM_CTL_CMD_SNAPSHOT_READ, NULL, 0, out, out_len, written);
}

int elmctl_event_read(struct elm_event_record *record)
{
    ssize_t written = 0;
    if (elmctl_syscall(ELM_CTL_CMD_EVENT_READ, NULL, 0, record, sizeof(*record), &written) != 0) {
        return -1;
    }
    if ((size_t)written != sizeof(*record)) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

int elmctl_event_ack(uint64_t sequence)
{
    ssize_t written = 0;
    return elmctl_syscall(ELM_CTL_CMD_EVENT_ACK, &sequence, sizeof(sequence), NULL, 0, &written);
}

int elmctl_debug_dump(uint8_t *out, size_t out_len, ssize_t *written)
{
    return elmctl_syscall(ELM_CTL_CMD_DEBUG_DUMP, NULL, 0, out, out_len, written);
}

int elmctl_mgr_call(uint32_t kind, const void *payload, size_t payload_len, uint8_t *out,
                    size_t out_len, struct elmctl_mgr_response *response)
{
    uint8_t input[ELM_MGR_MAX_INPUT];
    struct elm_mgr_call_header header = {
        .kind = kind,
        .flags = 0,
        .payload_len = (uint32_t)payload_len,
        .reserved = 0,
    };
    ssize_t written = 0;
    struct elm_mgr_response_header reply;

    if (payload_len > ELM_MGR_MAX_PAYLOAD ||
        payload_len + sizeof(header) > sizeof(input)) {
        errno = EMSGSIZE;
        return -1;
    }
    memcpy(input, &header, sizeof(header));
    if (payload_len != 0) {
        memcpy(input + sizeof(header), payload, payload_len);
    }
    if (elmctl_syscall(ELM_CTL_CMD_MGR_CALL, input, sizeof(header) + payload_len, out, out_len,
                       &written) != 0) {
        return -1;
    }
    if ((size_t)written < sizeof(reply)) {
        errno = EPROTO;
        return -1;
    }
    memcpy(&reply, out, sizeof(reply));
    if (reply.reserved != 0 || (size_t)written != sizeof(reply) + reply.payload_len) {
        errno = EPROTO;
        return -1;
    }
    response->status = reply.status;
    response->payload = out + sizeof(reply);
    response->payload_len = reply.payload_len;
    response->written = written;
    return 0;
}

int elmctl_mgr_call_empty(uint32_t kind, uint8_t *out, size_t out_len,
                          struct elmctl_mgr_response *response)
{
    return elmctl_mgr_call(kind, NULL, 0, out, out_len, response);
}

int elmctl_read_file(const char *path, uint8_t *out, size_t cap, size_t *len)
{
    FILE *fp = fopen(path, "rb");
    size_t used = 0;
    if (fp == NULL) {
        return -1;
    }
    while (used < cap) {
        size_t n = fread(out + used, 1, cap - used, fp);
        used += n;
        if (n == 0) {
            break;
        }
    }
    if (ferror(fp)) {
        int saved = errno;
        fclose(fp);
        errno = saved;
        return -1;
    }
    if (!feof(fp)) {
        fclose(fp);
        errno = EMSGSIZE;
        return -1;
    }
    fclose(fp);
    *len = used;
    return 0;
}

int elmctl_parse_u64(const char *text, uint64_t *out)
{
    char *end = NULL;
    unsigned long long value;
    errno = 0;
    value = strtoull(text, &end, 0);
    if (errno != 0 || end == text || *end != '\0') {
        errno = EINVAL;
        return -1;
    }
    *out = (uint64_t)value;
    return 0;
}

int elmctl_parse_u32(const char *text, uint32_t *out)
{
    uint64_t value = 0;
    if (elmctl_parse_u64(text, &value) != 0 || value > UINT32_MAX) {
        errno = EINVAL;
        return -1;
    }
    *out = (uint32_t)value;
    return 0;
}

int elmctl_parse_hex(const char *text, uint8_t *out, size_t cap, size_t *len)
{
    size_t n = strlen(text);
    size_t used = 0;
    if (n >= 2 && text[0] == '0' && (text[1] == 'x' || text[1] == 'X')) {
        text += 2;
        n -= 2;
    }
    if ((n % 2) != 0 || n / 2 > cap) {
        errno = EINVAL;
        return -1;
    }
    for (size_t i = 0; i < n; i += 2) {
        char tmp[3] = { text[i], text[i + 1], 0 };
        char *end = NULL;
        unsigned long value = strtoul(tmp, &end, 16);
        if (*end != '\0') {
            errno = EINVAL;
            return -1;
        }
        out[used++] = (uint8_t)value;
    }
    *len = used;
    return 0;
}

void elmctl_print_hex(const uint8_t *bytes, size_t len)
{
    for (size_t i = 0; i < len; i++) {
        printf("%02x", bytes[i]);
    }
}

void elmctl_copy_string(uint8_t *dst, size_t cap, uint16_t *len_out, const char *src)
{
    size_t n = strlen(src);
    if (n > cap) {
        n = cap;
    }
    memcpy(dst, src, n);
    *len_out = (uint16_t)n;
}

const char *elmctl_status_name(int32_t status)
{
    switch (status) {
    case ELM_MGR_STATUS_OK: return "OK";
    case ELM_MGR_STATUS_PERMISSION: return "PERMISSION";
    case ELM_MGR_STATUS_NOT_FOUND: return "NOT_FOUND";
    case ELM_MGR_STATUS_BUSY: return "BUSY";
    case ELM_MGR_STATUS_INVALID: return "INVALID";
    case ELM_MGR_STATUS_UNSUPPORTED: return "UNSUPPORTED";
    case ELM_MGR_STATUS_TODO: return "TODO";
    case ELM_CALL_STATUS_PROVIDER_FAULT: return "PROVIDER_FAULT";
    default: return "UNKNOWN";
    }
}

const char *elmctl_state_name(uint32_t state)
{
    switch (state) {
    case 1: return "Discovered";
    case 2: return "Verified";
    case 3: return "Loaded";
    case 4: return "Linked";
    case 5: return "Ready";
    case 6: return "Active";
    case 7: return "Quiescing";
    case 8: return "Paused";
    case 9: return "Detached";
    case 10: return "Retired";
    case 11: return "Faulted";
    case 12: return "Quarantined";
    default: return "Unknown";
    }
}

const char *elmctl_kind_name(uint32_t kind)
{
    switch (kind) {
    case 1: return "Manager";
    case 2: return "Service";
    case 3: return "Driver";
    case 4: return "Extension";
    case 5: return "Filesystem";
    case 6: return "Network";
    case 7: return "Debug";
    case 255: return "Other";
    default: return "Unknown";
    }
}

const char *elmctl_source_name(uint32_t source)
{
    switch (source) {
    case ELM_EBI_SOURCE_KIND_EKI: return "Eki";
    case ELM_EBI_SOURCE_KIND_PROJECTION: return "Projection";
    case ELM_EBI_SOURCE_KIND_BUILTIN: return "Builtin";
    case ELM_EBI_SOURCE_KIND_MEMORY: return "Memory";
    case ELM_EBI_SOURCE_KIND_REMOTE: return "Remote";
    default: return "Unknown";
    }
}

const char *elmctl_direction_name(uint32_t direction)
{
    switch (direction) {
    case ELM_FLOW_SOURCE: return "Source";
    case ELM_FLOW_SINK: return "Sink";
    case ELM_FLOW_DUPLEX: return "Duplex";
    case ELM_FLOW_CONTROL: return "Control";
    default: return "Unknown";
    }
}

const char *elmctl_mode_name(uint32_t mode)
{
    switch (mode) {
    case ELM_FLOW_EXCLUSIVE: return "Exclusive";
    case ELM_FLOW_SHARED: return "Shared";
    case ELM_FLOW_ORDERED: return "Ordered";
    case ELM_FLOW_PIPELINE: return "Pipeline";
    case ELM_FLOW_BROADCAST: return "Broadcast";
    default: return "Unknown";
    }
}

void elmctl_print_fixed_string(const uint8_t *bytes, uint16_t len)
{
    fwrite(bytes, 1, len, stdout);
}
