#include <stddef.h>
#include <stdint.h>

#include <component_repository.h>
#include <mrt/mrt.h>
#include <ranalib/stdio.h>
#include <ranalib/string.h>

#include "images.h"

typedef uint64_t (*add_function)(uint64_t left, uint64_t right);

static const uint8_t add_interface_id[16] = {
    0x6d, 0x79, 0x67, 0x6f, 0x2e, 0x61, 0x64, 0x64,
    0x2e, 0x69, 0x66, 0x61, 0x63, 0x65, 0x30, 0x31,
};

static const uint8_t add_signature_hash[32] = {
    0xcc, 0x8b, 0xc7, 0x7a, 0x01, 0xe5, 0x03, 0xa7,
    0xd4, 0x1b, 0x42, 0x4b, 0xf9, 0x39, 0x62, 0x26,
    0x16, 0xde, 0x5e, 0x75, 0x75, 0x44, 0x79, 0x13,
    0x4e, 0xff, 0xf2, 0x4f, 0x72, 0x7c, 0x8c, 0x25,
};

static struct mygo_native_result directory_call(
    uint64_t slot,
    uint64_t directory,
    const char *path,
    size_t path_length,
    uint32_t kind,
    uint64_t rights) {
    mygo_directory_request request = {
        .path = {
            .ptr = (uint64_t)(uintptr_t)path,
            .length = (uint32_t)path_length,
        },
        .kind = kind,
        .requested_rights = rights,
    };
    return mrt_call(
        slot,
        directory,
        (uint64_t)(uintptr_t)&request,
        0,
        0,
        0,
        0);
}

static uint32_t install_component(
    uint64_t root,
    const component_repository_request *request,
    const unsigned char *bytes,
    uint64_t length) {
    static const char components[] = "components";
    struct mygo_native_result directory = directory_call(
        MYGO_SLOT_directory_open,
        root,
        components,
        sizeof(components) - 1,
        MYGO_DIRECTORY_ENTRY_DIRECTORY,
        MYGO_RIGHT_open | MYGO_RIGHT_inspect);
    if (directory.status == MYGO_STATUS_filesystem_not_found) {
        directory = directory_call(
            MYGO_SLOT_directory_create,
            root,
            components,
            sizeof(components) - 1,
            MYGO_DIRECTORY_ENTRY_DIRECTORY,
            MYGO_RIGHT_open | MYGO_RIGHT_inspect);
    }
    if (directory.status != MYGO_STATUS_ok) {
        return directory.status;
    }
    (void)mrt_handle_close(directory.value0);

    char path[COMPONENT_REPOSITORY_PATH_CAPACITY];
    size_t path_length = component_repository_build_path(request, path, sizeof(path));
    if (path_length == 0) {
        return MYGO_STATUS_core_invalid_argument;
    }
    const uint64_t file_rights = MYGO_RIGHT_write | MYGO_RIGHT_resize | MYGO_RIGHT_inspect;
    struct mygo_native_result file = directory_call(
        MYGO_SLOT_directory_create,
        root,
        path,
        path_length,
        MYGO_DIRECTORY_ENTRY_FILE,
        file_rights);
    if (file.status == MYGO_STATUS_filesystem_already_exists) {
        file = directory_call(
            MYGO_SLOT_directory_open,
            root,
            path,
            path_length,
            MYGO_DIRECTORY_ENTRY_FILE,
            file_rights);
    }
    if (file.status != MYGO_STATUS_ok) {
        return file.status;
    }
    uint32_t status = mrt_call(MYGO_SLOT_file_resize, file.value0, 0, 0, 0, 0, 0).status;
    uint64_t offset = 0;
    while (status == MYGO_STATUS_ok && offset < length) {
        struct mygo_native_result written = mrt_call(
            MYGO_SLOT_file_write,
            file.value0,
            (uint64_t)(uintptr_t)(bytes + (size_t)offset),
            length - offset,
            offset,
            0,
            0);
        if (written.status != MYGO_STATUS_ok || written.value0 == 0 ||
            written.value0 > length - offset) {
            status = written.status == MYGO_STATUS_ok
                ? MYGO_STATUS_filesystem_error
                : written.status;
            break;
        }
        offset += written.value0;
    }
    if (status == MYGO_STATUS_ok) {
        status = mrt_call(MYGO_SLOT_file_resize, file.value0, length, 0, 0, 0, 0).status;
    }
    (void)mrt_handle_close(file.value0);
    return status;
}

static uint32_t request_component(
    uint64_t channel,
    const component_repository_request *request,
    uint64_t *image) {
    mygo_channel_message sent = {
        .data_ptr = (uint64_t)(uintptr_t)request,
        .data_size = sizeof(*request),
        .data_capacity = sizeof(*request),
    };
    uint32_t status = mrt_call(
        MYGO_SLOT_channel_send,
        channel,
        (uint64_t)(uintptr_t)&sent,
        0,
        0,
        0,
        0)
                          .status;
    if (status != MYGO_STATUS_ok) {
        return status;
    }

    component_repository_response response = {0};
    mygo_channel_handle_transfer received_handle = {0};
    mygo_channel_message received = {
        .data_ptr = (uint64_t)(uintptr_t)&response,
        .data_capacity = sizeof(response),
        .handles_ptr = (uint64_t)(uintptr_t)&received_handle,
        .handle_capacity = 1,
    };
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_channel_receive,
        channel,
        (uint64_t)(uintptr_t)&received,
        UINT64_MAX,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_ok) {
        return result.status;
    }
    if (result.value0 != sizeof(response) || result.value1 != 1 ||
        response.version != COMPONENT_REPOSITORY_PROTOCOL_VERSION ||
        response.status != MYGO_STATUS_ok ||
        memcmp(response.content_hash, request->content_hash, sizeof(response.content_hash)) != 0 ||
        received_handle.source_handle == 0 || received_handle.requested_rights != MYGO_RIGHT_load ||
        received_handle.flags != 0 || received_handle.reserved != 0) {
        if (received_handle.source_handle != 0) {
            (void)mrt_handle_close(received_handle.source_handle);
        }
        return response.status == MYGO_STATUS_ok ? MYGO_STATUS_image_invalid : response.status;
    }
    *image = received_handle.source_handle;
    return MYGO_STATUS_ok;
}

static uint32_t call_component(uint64_t process, uint64_t image, uint64_t *sum) {
    mygo_component_load_request load_request = {.root_image = image};
    struct mrt_component_result component = mrt_component_load(process, &load_request);
    if (component.status != MYGO_STATUS_ok) {
        return component.status;
    }
    mygo_interface_request request = {0};
    memcpy(request.interface_identity, add_interface_id, sizeof(add_interface_id));
    memcpy(request.signature_hash, add_signature_hash, sizeof(add_signature_hash));
    struct mrt_interface_result interface = mrt_component_interface(component.handle, &request);
    if (interface.status != MYGO_STATUS_ok) {
        (void)mrt_handle_close(component.handle);
        return interface.status;
    }
    struct mrt_component_call call = mrt_component_enter(interface.gate);
    if (call.status == MYGO_STATUS_ok) {
        add_function add = (add_function)(uintptr_t)call.target;
        *sum = add(19, 23);
        mrt_component_leave(interface.gate, call.previous_component);
    }
    uint32_t status = call.status;
    if (status == MYGO_STATUS_ok) {
        status = mrt_component_unload(component.handle, UINT64_MAX).status;
    }
    (void)mrt_handle_close(interface.handle);
    (void)mrt_handle_close(component.handle);
    return status;
}

int main(void) {
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    uint64_t root = mrt_initial_handle(MYGO_REQUIREMENT_root_directory);
    uint64_t repository_size = (uint64_t)(uintptr_t)mygo_repository_image_end -
        (uint64_t)(uintptr_t)mygo_repository_image_start;
    uint64_t component_size = (uint64_t)(uintptr_t)mygo_component_image_end -
        (uint64_t)(uintptr_t)mygo_component_image_start;
    struct mrt_handle_result component_source =
        mrt_image_create(process, mygo_component_image_start, component_size);
    mygo_image_info info = {0};
    uint32_t status = component_source.status;
    if (status == MYGO_STATUS_ok) {
        status = mrt_call(
            MYGO_SLOT_image_query,
            component_source.handle,
            (uint64_t)(uintptr_t)&info,
            0,
            0,
            0,
            0)
                     .status;
    }
    component_repository_request request = {
        .version = COMPONENT_REPOSITORY_PROTOCOL_VERSION,
    };
    if (status == MYGO_STATUS_ok &&
        (info.artifact_kind != MYGO_IMAGE_ARTIFACT_SHARED_COMPONENT ||
            info.file_size != component_size)) {
        status = MYGO_STATUS_image_invalid;
    }
    if (status == MYGO_STATUS_ok) {
        memcpy(request.component_id, info.component_identity, sizeof(request.component_id));
        memcpy(request.abi_id, info.abi_identity, sizeof(request.abi_id));
        memcpy(request.content_hash, info.content_hash, sizeof(request.content_hash));
        status = install_component(root, &request, mygo_component_image_start, component_size);
    }

    struct mrt_handle_result repository = {.status = status};
    if (status == MYGO_STATUS_ok) {
        repository = mrt_image_create(process, mygo_repository_image_start, repository_size);
        status = repository.status;
    }
    struct mygo_native_result channels = {.status = status};
    if (status == MYGO_STATUS_ok) {
        channels = mrt_call(MYGO_SLOT_channel_create, process, 8, 0, 0, 0, 0);
        status = channels.status;
    }
    struct mygo_handle_transfer transfers[2] = {
        {
            .requirement_id = MYGO_REQUIREMENT_root_directory,
            .source_handle = root,
            .requested_rights = MYGO_RIGHT_open | MYGO_RIGHT_inspect,
        },
        {
            .requirement_id = MYGO_REQUIREMENT_service_channel,
            .source_handle = channels.value0,
            .requested_rights = MYGO_RIGHT_send | MYGO_RIGHT_receive | MYGO_RIGHT_observe,
        },
    };
    struct mygo_spawn_request spawn = {
        .image = repository.handle,
        .transfers = {
            .ptr = (uint64_t)(uintptr_t)transfers,
            .count = 2,
        },
    };
    struct mrt_handle_result child = {.status = status};
    if (status == MYGO_STATUS_ok) {
        child = mrt_process_spawn(process, &spawn);
        status = child.status;
    }
    if (channels.value0 != 0) {
        (void)mrt_handle_close(channels.value0);
    }

    uint64_t loaded_image = 0;
    if (status == MYGO_STATUS_ok) {
        status = request_component(channels.value1, &request, &loaded_image);
    }
    uint64_t sum = 0;
    if (status == MYGO_STATUS_ok) {
        status = call_component(process, loaded_image, &sum);
    }
    if (loaded_image != 0) {
        (void)mrt_handle_close(loaded_image);
    }
    if (channels.value1 != 0) {
        (void)mrt_handle_close(channels.value1);
    }

    struct mygo_process_result child_result = {0};
    if (child.handle != 0) {
        uint32_t waited = mrt_process_wait(child.handle, &child_result, UINT64_MAX);
        if (status == MYGO_STATUS_ok && waited != MYGO_STATUS_ok) {
            status = waited;
        } else if (status == MYGO_STATUS_ok && child_result.exit_code != 0) {
            status = MYGO_STATUS_process_invalid_state;
        }
        (void)mrt_handle_close(child.handle);
    }
    if (repository.handle != 0) {
        (void)mrt_handle_close(repository.handle);
    }
    if (component_source.handle != 0) {
        (void)mrt_handle_close(component_source.handle);
    }
    if (status != MYGO_STATUS_ok || sum != 42) {
        return 1;
    }
    printf("SOYO repository PASS: %llu\n", (unsigned long long)sum);
    return 0;
}
