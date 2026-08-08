#ifndef MRT_MRT_H
#define MRT_MRT_H

#include "mygo_program.h"

enum mrt_start_error {
    MRT_START_OK = 0,
    MRT_START_BAD_HEADER,
    MRT_START_BAD_RANGE,
    MRT_START_BAD_RESERVED,
    MRT_START_BAD_CONTRACT,
    MRT_START_BAD_TLS,
    MRT_START_BAD_RANDOM,
    MRT_START_BAD_STRINGS,
    MRT_START_BAD_HANDLES,
};

struct mrt_start_view {
    const struct mygo_start_info *info;
    const struct mygo_initial_handle *initial_handles;
    uint32_t initial_handle_count;
    uint64_t self_process;
    uint64_t address_space;
    uint64_t stdin_stream;
    uint64_t stdout_stream;
    const uint64_t *init_array;
    uint32_t init_array_count;
    const uint64_t *fini_array;
    uint32_t fini_array_count;
};

struct mrt_program_result {
    int status;
};

struct mrt_handle_result {
    uint32_t status;
    uint64_t handle;
};

enum mrt_start_error mrt_validate_start_info(
    const struct mygo_start_info *info,
    uint64_t entry_size,
    uint64_t entry_image_base,
    uint64_t entry_thread_pointer,
    struct mrt_start_view *out);

void __mrt_native_call(
    const struct mygo_native_call *call,
    struct mygo_native_result *result);

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4);

uint32_t mrt_handle_close(uint64_t handle);
struct mrt_handle_result mrt_handle_duplicate(uint64_t handle);
struct mrt_handle_result mrt_handle_restrict(uint64_t handle, uint64_t rights);

uint64_t mrt_initial_handle(uint32_t requirement_id);

void mrt_run_initializers(const struct mrt_start_view *view);
void mrt_run_finalizers(const struct mrt_start_view *view);

int mrt_prepare_program(const struct mrt_start_view *view);
struct mrt_program_result mrt_invoke_program(const struct mrt_start_view *view);

_Noreturn void mrt_exit(uint32_t status);
_Noreturn void mrt_terminate(uint32_t status);
_Noreturn void mrt_abort(void);

#endif
