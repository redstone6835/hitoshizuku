#include <stddef.h>
#include <stdint.h>

#include <component_repository.h>
#include <mrt/mrt.h>
#include <ranalib/stdlib.h>
#include <ranalib/string.h>

#define COMPONENT_REPOSITORY_MAX_IMAGE_SIZE (UINT64_C(128) * 1024 * 1024)

static uint32_t open_image_file(
    uint64_t directory,
    const char *path,
    size_t path_length,
    uint64_t *file) {
    mygo_directory_request request = {
        .path = {
            .ptr = (uint64_t)(uintptr_t)path,
            .length = (uint32_t)path_length,
        },
        .kind = MYGO_DIRECTORY_ENTRY_FILE,
        .requested_rights = MYGO_RIGHT_read | MYGO_RIGHT_inspect,
    };
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_directory_open,
        directory,
        (uint64_t)(uintptr_t)&request,
        0,
        0,
        0,
        0);
    if (result.status == MYGO_STATUS_ok) {
        *file = result.value0;
    }
    return result.status;
}

static uint32_t read_image(uint64_t file, unsigned char **bytes, uint64_t *length) {
    mygo_file_info info = {0};
    struct mygo_native_result queried = mrt_call(
        MYGO_SLOT_file_query,
        file,
        (uint64_t)(uintptr_t)&info,
        0,
        0,
        0,
        0);
    if (queried.status != MYGO_STATUS_ok) {
        return queried.status;
    }
    if (info.kind != MYGO_DIRECTORY_ENTRY_FILE || info.size == 0 ||
        info.size > COMPONENT_REPOSITORY_MAX_IMAGE_SIZE || info.size > SIZE_MAX) {
        return MYGO_STATUS_image_invalid;
    }
    unsigned char *buffer = malloc((size_t)info.size);
    if (buffer == 0) {
        return MYGO_STATUS_core_resource_exhausted;
    }
    uint64_t offset = 0;
    while (offset < info.size) {
        struct mygo_native_result read = mrt_call(
            MYGO_SLOT_file_read,
            file,
            (uint64_t)(uintptr_t)(buffer + (size_t)offset),
            info.size - offset,
            offset,
            0,
            0);
        if (read.status != MYGO_STATUS_ok || read.value0 == 0 ||
            read.value0 > info.size - offset) {
            free(buffer);
            return read.status == MYGO_STATUS_ok ? MYGO_STATUS_filesystem_end : read.status;
        }
        offset += read.value0;
    }
    *bytes = buffer;
    *length = info.size;
    return MYGO_STATUS_ok;
}

static uint32_t create_image(
    const component_repository_request *request,
    uint64_t *image) {
    char path[COMPONENT_REPOSITORY_PATH_CAPACITY];
    size_t path_length = component_repository_build_path(request, path, sizeof(path));
    if (path_length == 0) {
        return MYGO_STATUS_core_invalid_argument;
    }
    uint64_t directory = mrt_initial_handle(MYGO_REQUIREMENT_root_directory);
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (directory == 0 || process == 0) {
        return MYGO_STATUS_security_rights_denied;
    }

    uint64_t file = 0;
    uint32_t status = open_image_file(directory, path, path_length, &file);
    if (status != MYGO_STATUS_ok) {
        return status;
    }
    unsigned char *bytes = 0;
    uint64_t length = 0;
    status = read_image(file, &bytes, &length);
    (void)mrt_handle_close(file);
    if (status != MYGO_STATUS_ok) {
        return status;
    }
    struct mrt_handle_result created = mrt_image_create(process, bytes, length);
    free(bytes);
    if (created.status != MYGO_STATUS_ok) {
        return created.status;
    }
    mygo_image_info info = {0};
    struct mygo_native_result queried = mrt_call(
        MYGO_SLOT_image_query,
        created.handle,
        (uint64_t)(uintptr_t)&info,
        0,
        0,
        0,
        0);
    if (queried.status != MYGO_STATUS_ok ||
        info.artifact_kind != MYGO_IMAGE_ARTIFACT_SHARED_COMPONENT ||
        info.file_size != length ||
        !component_repository_identity_matches(
            request,
            info.component_identity,
            info.abi_identity,
            info.content_hash)) {
        (void)mrt_handle_close(created.handle);
        return queried.status == MYGO_STATUS_ok ? MYGO_STATUS_image_invalid : queried.status;
    }
    *image = created.handle;
    return MYGO_STATUS_ok;
}

static int send_response(
    uint64_t channel,
    const component_repository_request *request,
    uint32_t status,
    uint64_t image) {
    component_repository_response response = {
        .version = COMPONENT_REPOSITORY_PROTOCOL_VERSION,
        .status = status,
    };
    if (request != 0) {
        memcpy(response.content_hash, request->content_hash, sizeof(response.content_hash));
    }
    mygo_channel_handle_transfer transfer = {
        .source_handle = image,
        .requested_rights = MYGO_RIGHT_load,
    };
    mygo_channel_message message = {
        .data_ptr = (uint64_t)(uintptr_t)&response,
        .data_size = sizeof(response),
        .data_capacity = sizeof(response),
        .handles_ptr = image == 0 ? 0 : (uint64_t)(uintptr_t)&transfer,
        .handle_count = image == 0 ? 0 : 1,
        .handle_capacity = image == 0 ? 0 : 1,
    };
    return mrt_call(
               MYGO_SLOT_channel_send,
               channel,
               (uint64_t)(uintptr_t)&message,
               0,
               0,
               0,
               0)
               .status == MYGO_STATUS_ok
        ? 1
        : -1;
}

int component_repository_serve_once(void) {
    uint64_t channel = mrt_initial_handle(MYGO_REQUIREMENT_service_channel);
    if (channel == 0) {
        return -1;
    }
    component_repository_request request = {0};
    mygo_channel_message message = {
        .data_ptr = (uint64_t)(uintptr_t)&request,
        .data_capacity = sizeof(request),
    };
    struct mygo_native_result received = mrt_call(
        MYGO_SLOT_channel_receive,
        channel,
        (uint64_t)(uintptr_t)&message,
        UINT64_MAX,
        0,
        0,
        0);
    if (received.status == MYGO_STATUS_channel_peer_closed) {
        return 0;
    }
    if (received.status != MYGO_STATUS_ok) {
        return -1;
    }
    if (received.value0 != sizeof(request) || received.value1 != 0) {
        return send_response(channel, 0, MYGO_STATUS_core_invalid_argument, 0);
    }

    uint64_t image = 0;
    uint32_t status = create_image(&request, &image);
    int sent = send_response(channel, &request, status, image);
    if (image != 0) {
        (void)mrt_handle_close(image);
    }
    return sent;
}
