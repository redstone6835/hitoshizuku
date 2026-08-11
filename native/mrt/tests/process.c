#include <assert.h>
#include <stdint.h>

#include "mygo_program.h"
#include <mrt/mrt.h>

enum call_step {
    CALL_IMAGE_CREATE,
    CALL_EVENT_CREATE,
    CALL_PROCESS_SPAWN,
    CALL_EVENT_BIND,
    CALL_EVENT_WAIT,
    CALL_PROCESS_WAIT,
};

static enum call_step next_call;
static uint64_t expected_process = UINT64_C(0x0000000100000001);
static uint64_t expected_image = UINT64_C(0x0000000100000002);
static uint64_t expected_port = UINT64_C(0x0000000100000003);
static uint64_t expected_child = UINT64_C(0x0000000100000004);
static unsigned char image_bytes[64];
static mygo_spawn_request request;
static mygo_event_record records[2];
static mygo_process_result process_result;

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    struct mygo_native_result result = {.status = MYGO_STATUS_ok};
    switch (next_call++) {
    case CALL_IMAGE_CREATE:
        assert(slot == MYGO_SLOT_image_create);
        assert(object_handle == expected_process);
        assert(arg0 == (uintptr_t)image_bytes);
        assert(arg1 == sizeof(image_bytes));
        assert(arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = expected_image;
        break;
    case CALL_EVENT_CREATE:
        assert(slot == MYGO_SLOT_event_create);
        assert(object_handle == expected_process);
        assert(arg0 == 4 && arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = expected_port;
        break;
    case CALL_PROCESS_SPAWN:
        assert(slot == MYGO_SLOT_process_spawn);
        assert(object_handle == expected_process);
        assert(arg0 == (uintptr_t)&request);
        assert(arg1 == MYGO_SPAWN_REQUEST_SIZE);
        assert(arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = expected_child;
        break;
    case CALL_EVENT_BIND:
        assert(slot == MYGO_SLOT_event_bind);
        assert(object_handle == expected_port);
        assert(arg0 == expected_child);
        assert(arg1 == MYGO_EVENT_KIND_PROCESS_EXITED);
        assert(arg2 == UINT64_C(0x1234));
        assert(arg3 == 0 && arg4 == 0);
        result.value0 = 7;
        break;
    case CALL_EVENT_WAIT:
        assert(slot == MYGO_SLOT_event_wait);
        assert(object_handle == expected_port);
        assert(arg0 == (uintptr_t)records);
        assert(arg1 == 2);
        assert(arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = 1;
        break;
    case CALL_PROCESS_WAIT:
        assert(slot == MYGO_SLOT_process_wait);
        assert(object_handle == expected_child);
        assert(arg0 == (uintptr_t)&process_result);
        assert(arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        break;
    }
    return result;
}

int main(void) {
    struct mrt_handle_result image = mrt_image_create(
        expected_process, image_bytes, sizeof(image_bytes));
    assert(image.status == MYGO_STATUS_ok && image.handle == expected_image);

    struct mrt_handle_result port = mrt_event_create(expected_process, 4);
    assert(port.status == MYGO_STATUS_ok && port.handle == expected_port);

    request.image = expected_image;
    struct mrt_handle_result child = mrt_process_spawn(expected_process, &request);
    assert(child.status == MYGO_STATUS_ok && child.handle == expected_child);

    struct mrt_handle_result subscription = mrt_event_bind(
        expected_port, expected_child, MYGO_EVENT_KIND_PROCESS_EXITED, UINT64_C(0x1234));
    assert(subscription.status == MYGO_STATUS_ok && subscription.handle == 7);

    struct mrt_count_result events = mrt_event_wait(expected_port, records, 2, 0);
    assert(events.status == MYGO_STATUS_ok && events.count == 1);

    assert(mrt_process_wait(expected_child, &process_result, 0) == MYGO_STATUS_ok);
    assert(next_call == CALL_PROCESS_WAIT + 1);
    return 0;
}
