#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/stdio.h>

#include "child_image.h"

int main(void) {
    const uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    const uint64_t stdout_handle = mrt_initial_handle(MYGO_REQUIREMENT_stdout);
    const uint64_t image_size = (uint64_t)(mygo_child_image_end - mygo_child_image_start);
    struct mrt_handle_result image =
        mrt_image_create(process, mygo_child_image_start, image_size);
    struct mrt_handle_result port = mrt_event_create(process, 4);
    struct mygo_handle_transfer transfer = {
        .requirement_id = MYGO_REQUIREMENT_stdout,
        .reserved = 0,
        .source_handle = stdout_handle,
        .requested_rights = MYGO_RIGHT_write,
        .flags = 0,
    };
    struct mygo_spawn_request request = {
        .image = image.handle,
        .argv = {0},
        .env = {0},
        .transfers = {
            .ptr = (uint64_t)(uintptr_t)&transfer,
            .count = 1,
            .reserved = 0,
        },
        .resource_policy = 0,
    };
    struct mrt_handle_result child = mrt_process_spawn(process, &request);
    struct mrt_handle_result subscription = mrt_event_bind(
        port.handle,
        child.handle,
        MYGO_EVENT_KIND_PROCESS_EXITED,
        UINT64_C(0x504152454e54));
    struct mygo_event_record record = {0};
    struct mrt_count_result events = mrt_event_wait(port.handle, &record, 1, 0);
    struct mygo_process_result result = {0};
    uint32_t wait_status = mrt_process_wait(child.handle, &result, 0);

    const int valid = process != 0 && stdout_handle != 0 && image.status == MYGO_STATUS_ok &&
        image.handle != 0 && port.status == MYGO_STATUS_ok && port.handle != 0 &&
        child.status == MYGO_STATUS_ok && child.handle != 0 &&
        subscription.status == MYGO_STATUS_ok && subscription.handle != 0 &&
        events.status == MYGO_STATUS_ok && events.count == 1 &&
        record.event_kind == MYGO_EVENT_KIND_PROCESS_EXITED &&
        record.source_handle == child.handle && record.value0 == UINT64_C(0x4a11) &&
        record.value1 == UINT64_C(0x504152454e54) && wait_status == MYGO_STATUS_ok &&
        result.exit_code == UINT32_C(0x4a11);

    (void)mrt_handle_close(child.handle);
    (void)mrt_handle_close(port.handle);
    (void)mrt_handle_close(image.handle);
    if (!valid) {
        return 1;
    }
    (void)fwrite("C parent\n", 1, 9, stdout);
    return 0;
}
