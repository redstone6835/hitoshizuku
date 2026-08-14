#define _POSIX_C_SOURCE 200809L

#include <assert.h>
#include <setjmp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>

#include <mrt/mrt.h>

static sigjmp_buf jump_buffer;
static struct mygo_native_call observed_call;
static int call_observed;

extern _Noreturn void __mrt_start(
    const struct mygo_start_info *info,
    uint64_t entry_size,
    uint64_t entry_image_base,
    uint64_t bootstrap_process,
    uint64_t entry_thread_pointer);

enum mrt_start_error mrt_validate_start_info(
    const struct mygo_start_info *info,
    uint64_t entry_size,
    uint64_t entry_image_base,
    uint64_t entry_thread_pointer,
    struct mrt_start_view *out) {
    (void)info;
    (void)entry_size;
    (void)entry_image_base;
    (void)entry_thread_pointer;
    (void)out;
    return MRT_START_BAD_HEADER;
}

void mrt_run_initializers(const struct mrt_start_view *view) {
    (void)view;
}

void mrt_run_finalizers(const struct mrt_start_view *view) {
    (void)view;
}

int program_main(void) {
    return 0;
}

void __mrt_native_call(
    const struct mygo_native_call *call,
    struct mygo_native_result *result) {
    observed_call = *call;
    call_observed = 1;
    result->status = MYGO_STATUS_ok;
    result->value0 = 0;
    result->value1 = 0;
    siglongjmp(jump_buffer, 1);
}

static void unexpected_trap(int signal_number) {
    (void)signal_number;
    siglongjmp(jump_buffer, 2);
}

int main(void) {
    struct sigaction action = {0};
    action.sa_handler = unexpected_trap;
    sigemptyset(&action.sa_mask);
    assert(sigaction(SIGILL, &action, 0) == 0);

    const uint64_t bootstrap_process = UINT64_C(0x0000000700000001);
    struct mygo_start_info info = {0};
    int jump_result = sigsetjmp(jump_buffer, 1);
    if (jump_result == 0) {
        __mrt_start(&info, 0, 0, bootstrap_process, 0);
        assert(!"__mrt_start returned");
    }
    assert(jump_result == 1);
    assert(call_observed);
    assert(observed_call.slot == MYGO_SLOT_process_exit);
    assert(observed_call.object_handle == bootstrap_process);
    assert(observed_call.args[0] == 160 + MRT_START_BAD_HEADER);
    return 0;
}
