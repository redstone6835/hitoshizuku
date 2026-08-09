#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/stdio.h>

#include "plugin_image.h"

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

int main(void) {
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    uint64_t image_size = (uint64_t)(uintptr_t)mygo_plugin_image_end -
        (uint64_t)(uintptr_t)mygo_plugin_image_start;
    struct mrt_handle_result image =
        mrt_image_create(process, mygo_plugin_image_start, image_size);
    if (image.status != MYGO_STATUS_ok) {
        return 10;
    }

    mygo_component_load_request load_request = {
        .root_image = image.handle,
    };
    struct mrt_component_result component = mrt_component_load(process, &load_request);
    if (component.status != MYGO_STATUS_ok) {
        return 11;
    }

    mygo_interface_request request = {0};
    for (unsigned index = 0; index < sizeof(add_interface_id); ++index) {
        request.interface_identity[index] = add_interface_id[index];
    }
    for (unsigned index = 0; index < sizeof(add_signature_hash); ++index) {
        request.signature_hash[index] = add_signature_hash[index];
    }
    struct mrt_interface_result interface =
        mrt_component_interface(component.handle, &request);
    if (interface.status != MYGO_STATUS_ok) {
        return 12;
    }
    struct mrt_component_call call = mrt_component_enter(interface.gate);
    if (call.status != MYGO_STATUS_ok) {
        return 13;
    }
    add_function add = (add_function)(uintptr_t)call.target;
    uint64_t sum = add(19, 23);
    mrt_component_leave(interface.gate, call.previous_component);
    if (sum != 42) {
        return 14;
    }
    if (mrt_component_unload(component.handle, 0).status != MYGO_STATUS_ok) {
        return 15;
    }
    if (mrt_component_enter(interface.gate).status != MYGO_STATUS_component_unloaded) {
        return 16;
    }
    (void)mrt_handle_close(interface.handle);
    (void)mrt_handle_close(component.handle);
    (void)mrt_handle_close(image.handle);
    printf("SOYO component PASS: %llu\n", (unsigned long long)sum);
    return 0;
}
