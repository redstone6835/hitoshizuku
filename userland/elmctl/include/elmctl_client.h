#ifndef ELMCTL_CLIENT_H
#define ELMCTL_CLIENT_H

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#include "elmctl_abi.h"

struct elmctl_mgr_response {
    int32_t status;
    const uint8_t *payload;
    uint32_t payload_len;
    ssize_t written;
};

int elmctl_syscall(uint32_t command, const void *input, size_t input_len, void *output,
                   size_t output_len, ssize_t *written);
int elmctl_core_query(struct elm_core_info *info);
int elmctl_snapshot(uint8_t *out, size_t out_len, ssize_t *written);
int elmctl_event_read(struct elm_event_record *record);
int elmctl_event_ack(uint64_t sequence);
int elmctl_debug_dump(uint8_t *out, size_t out_len, ssize_t *written);
int elmctl_mgr_call(uint32_t kind, const void *payload, size_t payload_len, uint8_t *out,
                    size_t out_len, struct elmctl_mgr_response *response);
int elmctl_mgr_call_empty(uint32_t kind, uint8_t *out, size_t out_len,
                          struct elmctl_mgr_response *response);
int elmctl_upload_image_file(const char *path, uint64_t *session_id);
int elmctl_abort_image_session(uint64_t session_id);
int elmctl_read_file(const char *path, uint8_t *out, size_t cap, size_t *len);
int elmctl_parse_u64(const char *text, uint64_t *out);
int elmctl_parse_u32(const char *text, uint32_t *out);
int elmctl_parse_hex(const char *text, uint8_t *out, size_t cap, size_t *len);
void elmctl_print_hex(const uint8_t *bytes, size_t len);
void elmctl_copy_string(uint8_t *dst, size_t cap, uint16_t *len_out, const char *src);
const char *elmctl_status_name(int32_t status);
const char *elmctl_ebi_load_status_name(int32_t status);
const char *elmctl_state_name(uint32_t state);
const char *elmctl_kind_name(uint32_t kind);
const char *elmctl_source_name(uint32_t source);
const char *elmctl_direction_name(uint32_t direction);
const char *elmctl_mode_name(uint32_t mode);
void elmctl_print_fixed_string(const uint8_t *bytes, uint16_t len);

#endif
