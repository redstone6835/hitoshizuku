#include <component_repository.h>

static const char hex_digits[] = "0123456789abcdef";

static char *append_hex(char *output, const uint8_t *bytes, size_t count) {
    for (size_t index = 0; index < count; ++index) {
        *output++ = hex_digits[bytes[index] >> 4];
        *output++ = hex_digits[bytes[index] & 0x0f];
    }
    return output;
}

size_t component_repository_build_path(
    const component_repository_request *request,
    char *output,
    size_t capacity) {
    static const char prefix[] = "components/";
    static const char suffix[] = ".soyo";
    const size_t required = sizeof(prefix) - 1 + 32 + 1 + 32 + 1 + 64 + sizeof(suffix) - 1;
    if (request == 0 || output == 0 || capacity < required ||
        request->version != COMPONENT_REPOSITORY_PROTOCOL_VERSION || request->flags != 0 ||
        request->reserved[0] != 0 || request->reserved[1] != 0) {
        return 0;
    }

    char *cursor = output;
    for (size_t index = 0; index < sizeof(prefix) - 1; ++index) {
        *cursor++ = prefix[index];
    }
    cursor = append_hex(cursor, request->component_id, sizeof(request->component_id));
    *cursor++ = '-';
    cursor = append_hex(cursor, request->abi_id, sizeof(request->abi_id));
    *cursor++ = '-';
    cursor = append_hex(cursor, request->content_hash, sizeof(request->content_hash));
    for (size_t index = 0; index < sizeof(suffix) - 1; ++index) {
        *cursor++ = suffix[index];
    }
    return (size_t)(cursor - output);
}

int component_repository_identity_matches(
    const component_repository_request *request,
    const uint8_t component_id[16],
    const uint8_t abi_id[16],
    const uint8_t content_hash[32]) {
    if (request == 0 || component_id == 0 || abi_id == 0 || content_hash == 0) {
        return 0;
    }
    uint8_t difference = 0;
    for (size_t index = 0; index < sizeof(request->component_id); ++index) {
        difference |= request->component_id[index] ^ component_id[index];
        difference |= request->abi_id[index] ^ abi_id[index];
    }
    for (size_t index = 0; index < sizeof(request->content_hash); ++index) {
        difference |= request->content_hash[index] ^ content_hash[index];
    }
    return difference == 0;
}
