#include <mrt/mrt.h>

static struct mrt_start_view runtime_view;
static int runtime_ready;
static uint64_t bootstrap_process_handle;

enum mrt_runtime_phase {
    MRT_PHASE_STARTING,
    MRT_PHASE_RUNNING,
    MRT_PHASE_FINALIZING,
};

static enum mrt_runtime_phase runtime_phase;

_Noreturn void mrt_abort(void) {
    if (bootstrap_process_handle != 0) {
        (void)mrt_call(
            MYGO_SLOT_process_exit,
            bootstrap_process_handle,
            134,
            0,
            0,
            0,
            0);
    }
    /* process.exit 按契约不会返回。若内核边界异常返回，只能停在当前用户任务，
     * 不能执行非法指令把 LA64 trap 送入整机 halt 路径。 */
    for (;;) {
        __asm__ volatile ("" ::: "memory");
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
    if (result.status != MYGO_STATUS_ok) {
        result.reserved = 0;
        result.value0 = 0;
        result.value1 = 0;
    }
    return result;
}

#ifdef MYGO_SLOT_handle_close
uint32_t mrt_handle_close(uint64_t handle) {
    return mrt_call(MYGO_SLOT_handle_close, handle, 0, 0, 0, 0, 0).status;
}
#endif

#ifdef MYGO_SLOT_handle_duplicate
struct mrt_handle_result mrt_handle_duplicate(uint64_t handle) {
    struct mygo_native_result result =
        mrt_call(MYGO_SLOT_handle_duplicate, handle, 0, 0, 0, 0, 0);
    return (struct mrt_handle_result){result.status, result.value0};
}
#endif

#ifdef MYGO_SLOT_handle_restrict
struct mrt_handle_result mrt_handle_restrict(uint64_t handle, uint64_t rights) {
    struct mygo_native_result result =
        mrt_call(MYGO_SLOT_handle_restrict, handle, rights, 0, 0, 0, 0);
    return (struct mrt_handle_result){result.status, result.value0};
}
#endif

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    if (!runtime_ready) {
        return requirement_id == MYGO_REQUIREMENT_self_process ? bootstrap_process_handle : 0;
    }
    uint32_t lower = 0;
    uint32_t upper = runtime_view.initial_handle_count;
    while (lower < upper) {
        uint32_t middle = lower + (upper - lower) / 2;
        const struct mygo_initial_handle *handle = &runtime_view.initial_handles[middle];
        if (handle->requirement_id < requirement_id) {
            lower = middle + 1;
        } else {
            upper = middle;
        }
    }
    if (lower < runtime_view.initial_handle_count &&
        runtime_view.initial_handles[lower].requirement_id == requirement_id) {
        return runtime_view.initial_handles[lower].handle;
    }
    return 0;
}

_Noreturn void mrt_terminate(uint32_t status) {
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (process == 0) {
        mrt_abort();
    }
    (void)mrt_call(MYGO_SLOT_process_exit, process, status, 0, 0, 0, 0);
    mrt_abort();
}

_Noreturn void mrt_exit(uint32_t status) {
    if (runtime_phase == MRT_PHASE_RUNNING) {
        runtime_phase = MRT_PHASE_FINALIZING;
        mrt_run_finalizers(&runtime_view);
    }
    mrt_terminate(status);
}

_Noreturn void __mrt_start(
    const struct mygo_start_info *info,
    uint64_t entry_size,
    uint64_t entry_image_base,
    uint64_t bootstrap_process,
    uint64_t entry_thread_pointer) {
    bootstrap_process_handle = bootstrap_process;
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
    if (view.self_process != bootstrap_process_handle) {
        mrt_abort();
    }

    runtime_view.info = view.info;
    runtime_view.initial_handles = view.initial_handles;
    runtime_view.initial_handle_count = view.initial_handle_count;
    runtime_view.self_process = view.self_process;
    runtime_view.address_space = view.address_space;
    runtime_view.stdin_stream = view.stdin_stream;
    runtime_view.stdout_stream = view.stdout_stream;
    runtime_view.init_array = view.init_array;
    runtime_view.init_array_count = view.init_array_count;
    runtime_view.fini_array = view.fini_array;
    runtime_view.fini_array_count = view.fini_array_count;
    runtime_ready = 1;
    if (mrt_prepare_program(&runtime_view) != 0) {
        mrt_abort();
    }
    runtime_phase = MRT_PHASE_RUNNING;
    mrt_run_initializers(&runtime_view);
    struct mrt_program_result result = mrt_invoke_program(&runtime_view);
    mrt_exit((uint32_t)result.status);
}
