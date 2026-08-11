#ifndef MYGO_COMPONENT_REPOSITORY_H
#define MYGO_COMPONENT_REPOSITORY_H

#include <stddef.h>
#include <stdint.h>

#define COMPONENT_REPOSITORY_PROTOCOL_VERSION UINT32_C(1)
#define COMPONENT_REPOSITORY_PATH_CAPACITY 160u

typedef struct {
    uint32_t version;
    uint32_t flags;
    uint8_t component_id[16];
    uint8_t abi_id[16];
    uint8_t content_hash[32];
    uint64_t reserved[2];
} component_repository_request;

typedef struct {
    uint32_t version;
    uint32_t status;
    uint8_t content_hash[32];
    uint64_t reserved;
} component_repository_response;

size_t component_repository_build_path(
    const component_repository_request *request,
    char *output,
    size_t capacity);

int component_repository_identity_matches(
    const component_repository_request *request,
    const uint8_t component_id[16],
    const uint8_t abi_id[16],
    const uint8_t content_hash[32]);

int component_repository_serve_once(void);

#endif
