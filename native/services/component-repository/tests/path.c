#include <assert.h>
#include <string.h>

#include <component_repository.h>

int main(void) {
    component_repository_request request = {
        .version = COMPONENT_REPOSITORY_PROTOCOL_VERSION,
    };
    for (unsigned index = 0; index < sizeof(request.component_id); ++index) {
        request.component_id[index] = (uint8_t)index;
        request.abi_id[index] = (uint8_t)(0xf0u + index);
    }
    for (unsigned index = 0; index < sizeof(request.content_hash); ++index) {
        request.content_hash[index] = (uint8_t)(0x80u + index);
    }

    char path[COMPONENT_REPOSITORY_PATH_CAPACITY];
    size_t length = component_repository_build_path(&request, path, sizeof(path));
    static const char expected[] =
        "components/000102030405060708090a0b0c0d0e0f-"
        "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff-"
        "808182838485868788898a8b8c8d8e8f"
        "909192939495969798999a9b9c9d9e9f.soyo";
    assert(length == sizeof(expected) - 1);
    assert(memcmp(path, expected, length) == 0);

    request.flags = 1;
    assert(component_repository_build_path(&request, path, sizeof(path)) == 0);
    request.flags = 0;
    request.reserved[1] = 1;
    assert(component_repository_build_path(&request, path, sizeof(path)) == 0);
    request.reserved[1] = 0;
    assert(component_repository_build_path(&request, path, length - 1) == 0);

    assert(component_repository_identity_matches(
        &request,
        request.component_id,
        request.abi_id,
        request.content_hash));
    uint8_t changed_hash[32];
    memcpy(changed_hash, request.content_hash, sizeof(changed_hash));
    changed_hash[17] ^= 1;
    assert(!component_repository_identity_matches(
        &request,
        request.component_id,
        request.abi_id,
        changed_hash));
    return 0;
}
