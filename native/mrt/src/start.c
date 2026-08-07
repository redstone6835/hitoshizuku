#include <mrt/mrt.h>

extern int main(void);

static struct mrt_start_view runtime_view;
static int runtime_ready;

_Noreturn void mrt_abort(void) {
    __builtin_trap();
    for (;;) {
    }
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    struct mygo_native_call call = {
        .slot = slot,
        .object_handle = object_handle,
        .args = {arg0, arg1, arg2, arg3, arg4},
        .reserved_arg = 0,
    };
    struct mygo_native_result result = {0};
    __mrt_native_call(&call, &result);
    if (result.status != MYGO_STATUS_OK) {
        result.reserved = 0;
        result.value0 = 0;
        result.value1 = 0;
    }
    return result;
}

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    if (!runtime_ready) {
        return 0;
    }
    if (requirement_id == MYGO_REQUIREMENT_SELF_PROCESS) {
        return runtime_view.self_process;
    }
    if (requirement_id == MYGO_REQUIREMENT_STDOUT) {
        return runtime_view.stdout_stream;
    }
    return 0;
}

_Noreturn void mrt_terminate(uint32_t status) {
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_SELF_PROCESS);
    if (process == 0) {
        mrt_abort();
    }
    (void)mrt_call(MYGO_SLOT_PROCESS_EXIT, process, status, 0, 0, 0, 0);
    mrt_abort();
}

_Noreturn void __mrt_start(
    const struct mygo_start_info *info,
    uint64_t entry_size,
    uint64_t entry_image_base,
    uint64_t entry_thread_pointer) {
    struct mrt_start_view view;
    enum mrt_start_error error = mrt_validate_start_info(
        info,
        entry_size,
        entry_image_base,
        entry_thread_pointer,
        &view);
    if (error != MRT_START_OK) {
        mrt_abort();
    }

    runtime_view = view;
    runtime_ready = 1;
    mrt_terminate((uint32_t)main());
}
