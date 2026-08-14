#include <assert.h>
#include <setjmp.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/threads.h>

static unsigned call_step;
static cnd_t *active_condition;

static const uint64_t process_handle = UINT64_C(0x100000001);
static const uint64_t send_handle = UINT64_C(0x100000002);
static const uint64_t receive_handle = UINT64_C(0x100000003);
static const uint64_t memory_handle = UINT64_C(0x100000004);
static const uint64_t thread_handle = UINT64_C(0x100000005);
static const uint64_t mutex_send_handle = UINT64_C(0x100000006);
static const uint64_t mutex_receive_handle = UINT64_C(0x100000007);
static const uint64_t condition_mutex_send_handle = UINT64_C(0x100000008);
static const uint64_t condition_mutex_receive_handle = UINT64_C(0x100000009);
static void (*created_entry)(uint64_t);
static uint64_t created_argument;
static jmp_buf thread_exit_jump;
static mtx_t *blocked_mutex;

uint64_t mrt_initial_handle(uint32_t requirement_id) {
    return requirement_id == MYGO_REQUIREMENT_self_process ? process_handle : 0;
}

uint64_t mrt_current_component(void) {
    return 0;
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    struct mygo_native_result result = {MYGO_STATUS_ok, 0, 0, 0};
    unsigned step = call_step++;
    if (step == 0) {
        assert(slot == MYGO_SLOT_thread_yield);
        assert(object_handle == process_handle);
        assert(arg0 == 0 && arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
    } else if (step == 1) {
        assert(slot == MYGO_SLOT_channel_create);
        assert(object_handle == process_handle);
        assert(arg0 >= 1 && arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = send_handle;
        result.value1 = receive_handle;
    } else if (step == 2) {
        assert(slot == MYGO_SLOT_channel_create);
        assert(object_handle == process_handle);
        assert(arg0 == 1 && arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = condition_mutex_send_handle;
        result.value1 = condition_mutex_receive_handle;
    } else if (step == 3) {
        assert(slot == MYGO_SLOT_channel_receive);
        assert(object_handle == receive_handle);
        assert(arg1 == UINT64_MAX && arg2 == 0 && arg3 == 0 && arg4 == 0);
        mygo_channel_message *message = (mygo_channel_message *)(uintptr_t)arg0;
        assert(message->data_capacity >= 1 && message->handle_capacity == 0);
        assert(cnd_signal(active_condition) == thrd_success);
        *(unsigned char *)(uintptr_t)message->data_ptr = 1;
        result.value0 = 1;
    } else if (step == 4) {
        assert(slot == MYGO_SLOT_channel_send);
        assert(object_handle == send_handle);
        assert(arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        const mygo_channel_message *message =
            (const mygo_channel_message *)(uintptr_t)arg0;
        assert(message->data_size == 1 && message->handle_count == 0);
    } else if (step == 9) {
        assert(slot == MYGO_SLOT_memory_create);
        assert(object_handle == process_handle);
        const mygo_memory_create_request *request =
            (const mygo_memory_create_request *)(uintptr_t)arg0;
        assert(request->size >= MYGO_PAGE_SIZE);
        result.value0 = memory_handle;
    } else if (step == 10) {
        assert(slot == MYGO_SLOT_thread_create);
        assert(object_handle == process_handle);
        assert(arg1 == 0);
        const mygo_thread_create_request *request =
            (const mygo_thread_create_request *)(uintptr_t)arg0;
        assert(request->stack_memory == memory_handle);
        created_entry = (void (*)(uint64_t))(uintptr_t)request->entry;
        created_argument = request->argument;
        result.value0 = thread_handle;
    } else if (step == 13) {
        assert(slot == MYGO_SLOT_thread_exit);
        assert(object_handle == process_handle);
        assert(arg0 == 37 && arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        longjmp(thread_exit_jump, 1);
    } else if (step == 14) {
        assert(slot == MYGO_SLOT_channel_create);
        assert(object_handle == process_handle);
        assert(arg0 == 1 && arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        result.value0 = mutex_send_handle;
        result.value1 = mutex_receive_handle;
    } else if (step == 15) {
        assert(slot == MYGO_SLOT_channel_receive);
        assert(object_handle == mutex_receive_handle);
        assert(arg1 == UINT64_MAX && arg2 == 0 && arg3 == 0 && arg4 == 0);
        mtx_unlock(blocked_mutex);
        mygo_channel_message *message = (mygo_channel_message *)(uintptr_t)arg0;
        assert(message->data_capacity == 1 && message->handle_capacity == 0);
        *(unsigned char *)(uintptr_t)message->data_ptr = 1;
        result.value0 = 1;
    } else if (step == 16) {
        assert(slot == MYGO_SLOT_channel_send);
        assert(object_handle == mutex_send_handle);
        assert(arg1 == 0 && arg2 == 0 && arg3 == 0 && arg4 == 0);
        const mygo_channel_message *message =
            (const mygo_channel_message *)(uintptr_t)arg0;
        assert(message->data_size == 1 && message->handle_count == 0);
    } else {
        assert(!"unexpected Native call");
    }
    return result;
}

uint32_t mrt_handle_close(uint64_t handle) {
    assert(call_step == 5 || call_step == 6 || call_step == 7 || call_step == 8 ||
        call_step == 11 || call_step == 12 || call_step == 17 || call_step == 18);
    if (call_step <= 6) {
        assert(handle == send_handle || handle == receive_handle);
    } else if (call_step <= 8) {
        assert(handle == condition_mutex_send_handle ||
            handle == condition_mutex_receive_handle);
    } else if (call_step == 11) {
        assert(handle == memory_handle);
    } else if (call_step == 12) {
        assert(handle == thread_handle);
    } else {
        assert(handle == mutex_send_handle || handle == mutex_receive_handle);
    }
    ++call_step;
    return MYGO_STATUS_ok;
}

_Noreturn void mrt_abort(void) { assert(!"unexpected abort"); __builtin_unreachable(); }

static int detached_thread(void *argument) {
    assert(argument == (void *)(uintptr_t)0x1234);
    return 37;
}

int main(void) {
    cnd_t condition;
    mtx_t mutex;
    active_condition = &condition;
    thrd_yield();
    assert(call_step == 1);
    assert(cnd_init(&condition) == thrd_success);
    assert(call_step == 2);
    assert(mtx_init(&mutex, mtx_plain) == thrd_success);
    assert(call_step == 3);
    assert(mtx_lock(&mutex) == thrd_success);
    assert(cnd_wait(&condition, &mutex) == thrd_success);
    mtx_unlock(&mutex);
    cnd_destroy(&condition);
    mtx_destroy(&mutex);
    assert(call_step == 9);

    thrd_t thread;
    assert(thrd_create(&thread, detached_thread, (void *)(uintptr_t)0x1234) == thrd_success);
    assert(call_step == 12);
    assert(created_entry != 0 && created_argument != 0);
    assert(thrd_detach(thread) == thrd_success);
    assert(call_step == 13);
    if (setjmp(thread_exit_jump) == 0) {
        created_entry(created_argument);
        assert(!"detached thread returned from its exit path");
    }
    assert(call_step == 14);

    mtx_t blocking_mutex;
    blocked_mutex = &blocking_mutex;
    assert(mtx_init(&blocking_mutex, mtx_plain) == thrd_success);
    assert(call_step == 15);
    blocking_mutex.locked = 1;
    blocking_mutex.owner = UINT64_C(0xffff);
    blocking_mutex.recursion = 1;
    assert(mtx_lock(&blocking_mutex) == thrd_success);
    assert(call_step == 17);
    mtx_unlock(&blocking_mutex);
    mtx_destroy(&blocking_mutex);
    assert(call_step == 19);
    return 0;
}
