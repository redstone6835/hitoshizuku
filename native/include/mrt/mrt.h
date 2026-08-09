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

struct mrt_count_result {
    uint32_t status;
    uint64_t count;
};

struct mrt_component_result {
    uint32_t status;
    uint64_t handle;
};

struct mrt_interface_result {
    uint32_t status;
    uint64_t handle;
    const mygo_component_interface_gate *gate;
};

struct mrt_component_call {
    uint32_t status;
    uint64_t target;
    uint64_t previous_component;
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

struct mrt_handle_result mrt_image_create(
    uint64_t process,
    const void *bytes,
    uint64_t length);
struct mrt_handle_result mrt_process_spawn(
    uint64_t process,
    const mygo_spawn_request *request);
uint32_t mrt_process_wait(
    uint64_t process,
    mygo_process_result *result,
    uint64_t deadline_ns);
struct mrt_handle_result mrt_event_create(uint64_t process, uint32_t capacity);
struct mrt_handle_result mrt_event_bind(
    uint64_t event_port,
    uint64_t source,
    uint32_t event_mask,
    uint64_t user_data);
struct mrt_count_result mrt_event_wait(
    uint64_t event_port,
    mygo_event_record *records,
    uint32_t capacity,
    uint64_t deadline_ns);
struct mrt_component_result mrt_component_load(
    uint64_t process,
    const mygo_component_load_request *request);
uint32_t mrt_component_query(uint64_t component, mygo_component_query *query);
struct mrt_interface_result mrt_component_interface(
    uint64_t component,
    const mygo_interface_request *request);
struct mrt_component_result mrt_component_unload(
    uint64_t component,
    uint64_t deadline_ns);
struct mrt_component_call mrt_component_enter(
    const mygo_component_interface_gate *gate);
void mrt_component_leave(
    const mygo_component_interface_gate *gate,
    uint64_t previous_component);

uint64_t mrt_initial_handle(uint32_t requirement_id);
uint64_t mrt_current_component(void);

void mrt_run_initializers(const struct mrt_start_view *view);
void mrt_run_finalizers(const struct mrt_start_view *view);

int mrt_prepare_program(const struct mrt_start_view *view);
struct mrt_program_result mrt_invoke_program(const struct mrt_start_view *view);

_Noreturn void mrt_exit(uint32_t status);
_Noreturn void mrt_terminate(uint32_t status);
_Noreturn void mrt_abort(void);

#endif
