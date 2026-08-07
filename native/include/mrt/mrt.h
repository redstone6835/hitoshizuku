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
    uint64_t self_process;
    uint64_t stdout_stream;
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

uint64_t mrt_initial_handle(uint32_t requirement_id);

_Noreturn void mrt_terminate(uint32_t status);
_Noreturn void mrt_abort(void);

#endif
