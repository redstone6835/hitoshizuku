#define _GNU_SOURCE

#include "elmctl_client.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

struct elmctl_sha256 {
    uint32_t state[8];
    uint64_t total_bytes;
    uint8_t block[64];
    size_t block_len;
};

static const uint32_t elmctl_sha256_round[64] = {
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u, 0x3956c25bu, 0x59f111f1u,
    0x923f82a4u, 0xab1c5ed5u, 0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
    0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u, 0xe49b69c1u, 0xefbe4786u,
    0x0fc19dc6u, 0x240ca1ccu, 0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u, 0xc6e00bf3u, 0xd5a79147u,
    0x06ca6351u, 0x14292967u, 0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
    0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u, 0xa2bfe8a1u, 0xa81a664bu,
    0xc24b8b70u, 0xc76c51a3u, 0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u, 0x391c0cb3u, 0x4ed8aa4au,
    0x5b9cca4fu, 0x682e6ff3u, 0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
    0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u,
};

static uint32_t elmctl_rotr32(uint32_t value, uint32_t shift)
{
    return (value >> shift) | (value << (32u - shift));
}

static void elmctl_sha256_transform(struct elmctl_sha256 *context, const uint8_t block[64])
{
    uint32_t words[64];
    uint32_t a;
    uint32_t b;
    uint32_t c;
    uint32_t d;
    uint32_t e;
    uint32_t f;
    uint32_t g;
    uint32_t h;

    for (size_t i = 0; i < 16; i++) {
        size_t offset = i * 4;
        words[i] = ((uint32_t)block[offset] << 24) |
                   ((uint32_t)block[offset + 1] << 16) |
                   ((uint32_t)block[offset + 2] << 8) |
                   (uint32_t)block[offset + 3];
    }
    for (size_t i = 16; i < 64; i++) {
        uint32_t s0 = elmctl_rotr32(words[i - 15], 7) ^
                      elmctl_rotr32(words[i - 15], 18) ^ (words[i - 15] >> 3);
        uint32_t s1 = elmctl_rotr32(words[i - 2], 17) ^
                      elmctl_rotr32(words[i - 2], 19) ^ (words[i - 2] >> 10);
        words[i] = words[i - 16] + s0 + words[i - 7] + s1;
    }

    a = context->state[0];
    b = context->state[1];
    c = context->state[2];
    d = context->state[3];
    e = context->state[4];
    f = context->state[5];
    g = context->state[6];
    h = context->state[7];
    for (size_t i = 0; i < 64; i++) {
        uint32_t sum1 = elmctl_rotr32(e, 6) ^ elmctl_rotr32(e, 11) ^ elmctl_rotr32(e, 25);
        uint32_t choose = (e & f) ^ (~e & g);
        uint32_t temp1 = h + sum1 + choose + elmctl_sha256_round[i] + words[i];
        uint32_t sum0 = elmctl_rotr32(a, 2) ^ elmctl_rotr32(a, 13) ^ elmctl_rotr32(a, 22);
        uint32_t majority = (a & b) ^ (a & c) ^ (b & c);
        uint32_t temp2 = sum0 + majority;
        h = g;
        g = f;
        f = e;
        e = d + temp1;
        d = c;
        c = b;
        b = a;
        a = temp1 + temp2;
    }
    context->state[0] += a;
    context->state[1] += b;
    context->state[2] += c;
    context->state[3] += d;
    context->state[4] += e;
    context->state[5] += f;
    context->state[6] += g;
    context->state[7] += h;
}

static void elmctl_sha256_init(struct elmctl_sha256 *context)
{
    static const uint32_t initial[8] = {
        0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
        0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u,
    };
    memcpy(context->state, initial, sizeof(initial));
    context->total_bytes = 0;
    context->block_len = 0;
}

static void elmctl_sha256_update(struct elmctl_sha256 *context, const uint8_t *bytes, size_t len)
{
    context->total_bytes += len;
    while (len != 0) {
        size_t available = sizeof(context->block) - context->block_len;
        size_t take = len < available ? len : available;
        memcpy(context->block + context->block_len, bytes, take);
        context->block_len += take;
        bytes += take;
        len -= take;
        if (context->block_len == sizeof(context->block)) {
            elmctl_sha256_transform(context, context->block);
            context->block_len = 0;
        }
    }
}

static void elmctl_sha256_finish(struct elmctl_sha256 *context,
                                 uint8_t digest[ELM_IMAGE_SESSION_DIGEST_LEN])
{
    uint64_t bit_len = context->total_bytes * 8u;
    context->block[context->block_len++] = 0x80;
    if (context->block_len > 56) {
        memset(context->block + context->block_len, 0, sizeof(context->block) - context->block_len);
        elmctl_sha256_transform(context, context->block);
        context->block_len = 0;
    }
    memset(context->block + context->block_len, 0, 56 - context->block_len);
    for (size_t i = 0; i < 8; i++) {
        context->block[63 - i] = (uint8_t)(bit_len >> (i * 8));
    }
    elmctl_sha256_transform(context, context->block);
    for (size_t i = 0; i < 8; i++) {
        digest[i * 4] = (uint8_t)(context->state[i] >> 24);
        digest[i * 4 + 1] = (uint8_t)(context->state[i] >> 16);
        digest[i * 4 + 2] = (uint8_t)(context->state[i] >> 8);
        digest[i * 4 + 3] = (uint8_t)context->state[i];
    }
}

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
    uint8_t *input = NULL;
    struct elm_mgr_call_header header = {
        .kind = kind,
        .flags = 0,
        .payload_len = (uint32_t)payload_len,
        .reserved = 0,
    };
    ssize_t written = 0;
    struct elm_mgr_response_header reply;
    size_t input_len = sizeof(header) + payload_len;

    if (payload_len > ELM_MGR_MAX_PAYLOAD || input_len > ELM_MGR_MAX_INPUT) {
        errno = EMSGSIZE;
        return -1;
    }
    input = malloc(input_len);
    if (input == NULL) {
        errno = ENOMEM;
        return -1;
    }
    memcpy(input, &header, sizeof(header));
    if (payload_len != 0) {
        memcpy(input + sizeof(header), payload, payload_len);
    }
    if (elmctl_syscall(ELM_CTL_CMD_MGR_CALL, input, input_len, out, out_len, &written) != 0) {
        free(input);
        return -1;
    }
    free(input);
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

static int elmctl_mgr_status_errno(int32_t status)
{
    switch (status) {
    case ELM_MGR_STATUS_PERMISSION: return EPERM;
    case ELM_MGR_STATUS_NOT_FOUND: return ENOENT;
    case ELM_MGR_STATUS_NO_MEMORY: return ENOMEM;
    case ELM_MGR_STATUS_BUSY: return EBUSY;
    case ELM_MGR_STATUS_INVALID: return EINVAL;
    case ELM_MGR_STATUS_INTEGRITY: return EBADMSG;
    case ELM_MGR_STATUS_UNSUPPORTED: return EOPNOTSUPP;
    case ELM_MGR_STATUS_EXPIRED: return ETIMEDOUT;
    default: return EPROTO;
    }
}

static int elmctl_parse_image_session_response(const struct elmctl_mgr_response *response,
                                               struct elm_image_session_info_v1 *info)
{
    if (response->status != ELM_MGR_STATUS_OK) {
        errno = elmctl_mgr_status_errno(response->status);
        return -1;
    }
    if (response->payload_len != sizeof(*info)) {
        errno = EPROTO;
        return -1;
    }
    memcpy(info, response->payload, sizeof(*info));
    if (info->abi_version != ELM_IMAGE_SESSION_ABI_VERSION ||
        info->struct_size != sizeof(*info) || info->session_id == 0 ||
        info->hash_alg != ELM_IMAGE_SESSION_HASH_SHA256 ||
        info->digest_len != ELM_IMAGE_SESSION_DIGEST_LEN || info->flags != 0) {
        errno = EPROTO;
        return -1;
    }
    return 0;
}

int elmctl_abort_image_session(uint64_t session_id)
{
    struct elm_image_session_request_v1 request = {
        .abi_version = ELM_IMAGE_SESSION_ABI_VERSION,
        .flags = 0,
        .reserved = 0,
        .session_id = session_id,
    };
    uint8_t output[sizeof(struct elm_mgr_response_header) +
                   sizeof(struct elm_image_session_info_v1)];
    struct elmctl_mgr_response response;
    struct elm_image_session_info_v1 info;
    if (session_id == 0) {
        errno = EINVAL;
        return -1;
    }
    if (elmctl_mgr_call(ELM_MGR_CALL_ABORT_IMAGE_SESSION, &request, sizeof(request), output,
                        sizeof(output), &response) != 0) {
        return -1;
    }
    return elmctl_parse_image_session_response(&response, &info);
}

int elmctl_upload_image_file(const char *path, uint64_t *session_id)
{
    FILE *fp = NULL;
    uint8_t *wire = NULL;
    uint8_t digest[ELM_IMAGE_SESSION_DIGEST_LEN];
    uint64_t total_len = 0;
    uint64_t offset = 0;
    uint64_t active_session = 0;
    struct elmctl_sha256 hash;
    struct elm_image_session_begin_request_v1 begin;
    struct elm_image_session_write_request_v1 write;
    struct elm_image_session_request_v1 seal;
    struct elm_image_session_info_v1 info;
    struct elmctl_mgr_response response;
    uint8_t output[sizeof(struct elm_mgr_response_header) +
                   sizeof(struct elm_image_session_info_v1)];
    int saved_errno;

    if (path == NULL || session_id == NULL) {
        errno = EINVAL;
        return -1;
    }
    *session_id = 0;
    fp = fopen(path, "rb");
    if (fp == NULL) {
        return -1;
    }
    wire = malloc(sizeof(write) + ELM_IMAGE_SESSION_MAX_CHUNK);
    if (wire == NULL) {
        saved_errno = ENOMEM;
        goto fail;
    }

    elmctl_sha256_init(&hash);
    for (;;) {
        size_t read_len = fread(wire + sizeof(write), 1, ELM_IMAGE_SESSION_MAX_CHUNK, fp);
        if (read_len != 0) {
            if (total_len > ELM_IMAGE_SESSION_MAX_LENGTH - read_len) {
                saved_errno = EFBIG;
                goto fail;
            }
            elmctl_sha256_update(&hash, wire + sizeof(write), read_len);
            total_len += read_len;
        }
        if (read_len < ELM_IMAGE_SESSION_MAX_CHUNK) {
            if (ferror(fp)) {
                saved_errno = errno != 0 ? errno : EIO;
                goto fail;
            }
            break;
        }
    }
    if (total_len == 0) {
        saved_errno = EINVAL;
        goto fail;
    }
    elmctl_sha256_finish(&hash, digest);
    if (fseek(fp, 0, SEEK_SET) != 0) {
        saved_errno = errno;
        goto fail;
    }

    memset(&begin, 0, sizeof(begin));
    begin.abi_version = ELM_IMAGE_SESSION_ABI_VERSION;
    begin.hash_alg = ELM_IMAGE_SESSION_HASH_SHA256;
    begin.total_len = total_len;
    begin.ttl_ms = ELM_IMAGE_SESSION_DEFAULT_TTL_MS;
    begin.digest_len = ELM_IMAGE_SESSION_DIGEST_LEN;
    memcpy(begin.expected_digest, digest, sizeof(digest));
    if (elmctl_mgr_call(ELM_MGR_CALL_BEGIN_IMAGE_SESSION, &begin, sizeof(begin), output,
                        sizeof(output), &response) != 0 ||
        elmctl_parse_image_session_response(&response, &info) != 0) {
        saved_errno = errno;
        goto fail;
    }
    if (info.state != ELM_IMAGE_SESSION_STATE_UPLOADING || info.total_len != total_len ||
        info.written_len != 0 || memcmp(info.expected_digest, digest, sizeof(digest)) != 0) {
        saved_errno = EPROTO;
        goto fail;
    }
    active_session = info.session_id;

    for (;;) {
        size_t read_len = fread(wire + sizeof(write), 1, ELM_IMAGE_SESSION_MAX_CHUNK, fp);
        if (read_len != 0) {
            if (offset > total_len || read_len > total_len - offset) {
                saved_errno = EBADMSG;
                goto fail;
            }
            memset(&write, 0, sizeof(write));
            write.abi_version = ELM_IMAGE_SESSION_ABI_VERSION;
            write.session_id = active_session;
            write.offset = offset;
            write.chunk_len = (uint32_t)read_len;
            memcpy(wire, &write, sizeof(write));
            if (elmctl_mgr_call(ELM_MGR_CALL_WRITE_IMAGE_SESSION, wire,
                                sizeof(write) + read_len, output, sizeof(output), &response) != 0 ||
                elmctl_parse_image_session_response(&response, &info) != 0) {
                saved_errno = errno;
                goto fail;
            }
            offset += read_len;
            if (info.session_id != active_session ||
                info.state != ELM_IMAGE_SESSION_STATE_UPLOADING ||
                info.total_len != total_len || info.written_len != offset) {
                saved_errno = EPROTO;
                goto fail;
            }
        }
        if (read_len < ELM_IMAGE_SESSION_MAX_CHUNK) {
            if (ferror(fp)) {
                saved_errno = errno != 0 ? errno : EIO;
                goto fail;
            }
            break;
        }
    }
    if (offset != total_len) {
        saved_errno = EBADMSG;
        goto fail;
    }

    memset(&seal, 0, sizeof(seal));
    seal.abi_version = ELM_IMAGE_SESSION_ABI_VERSION;
    seal.session_id = active_session;
    if (elmctl_mgr_call(ELM_MGR_CALL_SEAL_IMAGE_SESSION, &seal, sizeof(seal), output,
                        sizeof(output), &response) != 0 ||
        elmctl_parse_image_session_response(&response, &info) != 0) {
        saved_errno = errno;
        goto fail;
    }
    if (info.session_id != active_session || info.state != ELM_IMAGE_SESSION_STATE_SEALED ||
        info.total_len != total_len || info.written_len != total_len ||
        memcmp(info.expected_digest, digest, sizeof(digest)) != 0 ||
        memcmp(info.actual_digest, digest, sizeof(digest)) != 0) {
        saved_errno = EPROTO;
        goto fail;
    }

    fclose(fp);
    free(wire);
    *session_id = active_session;
    return 0;

fail:
    if (active_session != 0) {
        int original = saved_errno;
        (void)elmctl_abort_image_session(active_session);
        saved_errno = original;
    }
    if (fp != NULL) {
        fclose(fp);
    }
    free(wire);
    errno = saved_errno;
    return -1;
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
    case ELM_MGR_STATUS_NO_MEMORY: return "NO_MEMORY";
    case ELM_MGR_STATUS_BUSY: return "BUSY";
    case ELM_MGR_STATUS_INVALID: return "INVALID";
    case ELM_MGR_STATUS_INTEGRITY: return "INTEGRITY";
    case ELM_MGR_STATUS_UNSUPPORTED: return "UNSUPPORTED";
    case ELM_MGR_STATUS_EXPIRED: return "EXPIRED";
    case ELM_MGR_STATUS_TODO: return "TODO";
    case ELM_CALL_STATUS_PROVIDER_FAULT: return "PROVIDER_FAULT";
    default: return "UNKNOWN";
    }
}

const char *elmctl_ebi_load_status_name(int32_t status)
{
    switch (status) {
    case ELM_EBI_LOAD_STATUS_OK: return "OK";
    case ELM_EBI_LOAD_STATUS_INVALID_UNIT: return "INVALID_UNIT";
    case ELM_EBI_LOAD_STATUS_UNSUPPORTED_ABI: return "UNSUPPORTED_ABI";
    case ELM_EBI_LOAD_STATUS_INVALID_TARGET: return "INVALID_TARGET";
    case ELM_EBI_LOAD_STATUS_INVALID_SEGMENT: return "INVALID_SEGMENT";
    case ELM_EBI_LOAD_STATUS_ARCH_MISMATCH: return "ARCH_MISMATCH";
    case ELM_EBI_LOAD_STATUS_INVALID_MANIFEST: return "INVALID_MANIFEST";
    case ELM_EBI_LOAD_STATUS_INVALID_MENU: return "INVALID_MENU";
    case ELM_EBI_LOAD_STATUS_NATIVE_CODE_TODO: return "NATIVE_CODE_TODO";
    case ELM_EBI_LOAD_STATUS_RUNTIME_REJECTED: return "RUNTIME_REJECTED";
    case ELM_EBI_LOAD_STATUS_UNTRUSTED_IMAGE: return "UNTRUSTED_IMAGE";
    case ELM_EBI_LOAD_STATUS_ABI_FINGERPRINT_REJECTED: return "ABI_FINGERPRINT_REJECTED";
    case ELM_EBI_LOAD_STATUS_ROLLBACK_REJECTED: return "ROLLBACK_REJECTED";
    default: return "UNKNOWN";
    }
}

const char *elmctl_state_name(uint32_t state)
{
    switch (state) {
    case ELM_STATE_DISCOVERED: return "Discovered";
    case ELM_STATE_VERIFIED: return "Verified";
    case ELM_STATE_LOADED: return "Loaded";
    case ELM_STATE_LINKED: return "Linked";
    case ELM_STATE_READY: return "Ready";
    case ELM_STATE_ACTIVE: return "Active";
    case ELM_STATE_QUIESCING: return "Quiescing";
    case ELM_STATE_PAUSED: return "Paused";
    case ELM_STATE_DETACHED: return "Detached";
    case ELM_STATE_RETIRED: return "Retired";
    case ELM_STATE_FAULTED: return "Faulted";
    case ELM_STATE_QUARANTINED: return "Quarantined";
    default: return "Unknown";
    }
}

const char *elmctl_kind_name(uint32_t kind)
{
    switch (kind) {
    case ELM_KIND_MANAGER: return "Manager";
    case ELM_KIND_SERVICE: return "Service";
    case ELM_KIND_DRIVER: return "Driver";
    case ELM_KIND_EXTENSION: return "Extension";
    case ELM_KIND_FILESYSTEM: return "Filesystem";
    case ELM_KIND_NETWORK: return "Network";
    case ELM_KIND_DEBUG: return "Debug";
    case ELM_KIND_OTHER: return "Other";
    default: return "Unknown";
    }
}

const char *elmctl_source_name(uint32_t source)
{
    switch (source) {
    case ELM_EBI_SOURCE_KIND_PROJECTION: return "projection";
    case ELM_EBI_SOURCE_KIND_BUILTIN: return "<builtin>";
    case ELM_EBI_SOURCE_KIND_MEMORY: return "memory";
    default: return "unknown";
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
