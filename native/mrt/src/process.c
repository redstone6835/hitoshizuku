#include <stdint.h>

#include <mrt/mrt.h>

#if MYGO_HAS_image_create || MYGO_HAS_process_spawn || MYGO_HAS_event_create || MYGO_HAS_event_bind
static struct mrt_handle_result handle_result(struct mygo_native_result result) {
    return (struct mrt_handle_result){result.status, result.value0};
}
#endif

struct mrt_handle_result mrt_image_create(
    uint64_t process,
    const void *bytes,
    uint64_t length) {
#if MYGO_HAS_image_create
    return handle_result(mrt_call(
        MYGO_SLOT_image_create,
        process,
        (uint64_t)(uintptr_t)bytes,
        length,
        0,
        0,
        0));
#else
    (void)process;
    (void)bytes;
    (void)length;
    return (struct mrt_handle_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}

struct mrt_handle_result mrt_process_spawn(
    uint64_t process,
    const mygo_spawn_request *request) {
#if MYGO_HAS_process_spawn
    return handle_result(mrt_call(
        MYGO_SLOT_process_spawn,
        process,
        (uint64_t)(uintptr_t)request,
        MYGO_SPAWN_REQUEST_SIZE,
        0,
        0,
        0));
#else
    (void)process;
    (void)request;
    return (struct mrt_handle_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}

uint32_t mrt_process_wait(
    uint64_t process,
    mygo_process_result *result,
    uint64_t deadline_ns) {
#if MYGO_HAS_process_wait
    return mrt_call(
               MYGO_SLOT_process_wait,
               process,
               (uint64_t)(uintptr_t)result,
               deadline_ns,
               0,
               0,
               0)
        .status;
#else
    (void)process;
    (void)result;
    (void)deadline_ns;
    return MYGO_STATUS_abi_unsupported_operation;
#endif
}

struct mrt_handle_result mrt_event_create(uint64_t process, uint32_t capacity) {
#if MYGO_HAS_event_create
    return handle_result(mrt_call(
        MYGO_SLOT_event_create,
        process,
        capacity,
        0,
        0,
        0,
        0));
#else
    (void)process;
    (void)capacity;
    return (struct mrt_handle_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}

struct mrt_handle_result mrt_event_bind(
    uint64_t event_port,
    uint64_t source,
    uint32_t event_mask,
    uint64_t user_data) {
#if MYGO_HAS_event_bind
    return handle_result(mrt_call(
        MYGO_SLOT_event_bind,
        event_port,
        source,
        event_mask,
        user_data,
        0,
        0));
#else
    (void)event_port;
    (void)source;
    (void)event_mask;
    (void)user_data;
    return (struct mrt_handle_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}

struct mrt_count_result mrt_event_wait(
    uint64_t event_port,
    mygo_event_record *records,
    uint32_t capacity,
    uint64_t deadline_ns) {
#if MYGO_HAS_event_wait
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_event_wait,
        event_port,
        (uint64_t)(uintptr_t)records,
        capacity,
        deadline_ns,
        0,
        0);
    return (struct mrt_count_result){result.status, result.value0};
#else
    (void)event_port;
    (void)records;
    (void)capacity;
    (void)deadline_ns;
    return (struct mrt_count_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}
