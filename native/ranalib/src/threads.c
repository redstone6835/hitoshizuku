#include <stddef.h>
#include <stdint.h>

#include <mrt/mrt.h>
#include <ranalib/stdlib.h>
#include <ranalib/threads.h>

#define RANALIB_THREAD_STACK_SIZE UINT64_C(65536)

struct thread_context {
    thrd_start_t function;
    void *argument;
    uint64_t handle;
    uint32_t state;
};

enum {
    THREAD_JOINABLE = 0,
    THREAD_JOINING = 1,
    THREAD_DETACHED = 2,
    THREAD_EXITED = 3,
};

static _Thread_local struct thread_context *current_context;

static uint64_t current_identity(void) {
    return current_context == 0 ? 0 : current_context->handle;
}

static _Noreturn void thread_entry(uint64_t raw_context) {
    struct thread_context *context = (struct thread_context *)(uintptr_t)raw_context;
    while (__atomic_load_n(&context->handle, __ATOMIC_ACQUIRE) == 0) {
        __asm__ volatile("" ::: "memory");
    }
    current_context = context;
    int result = context->function(context->argument);
    thrd_exit(result);
}

int thrd_create(thrd_t *thread, thrd_start_t function, void *argument) {
    if (thread == 0 || function == 0) {
        return thrd_error;
    }
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (process == 0) {
        return thrd_error;
    }
    struct thread_context *context = malloc(sizeof(*context));
    if (context == 0) {
        return thrd_nomem;
    }
    *context = (struct thread_context){function, argument, 0, THREAD_JOINABLE};

    mygo_memory_create_request memory_request = {
        .size = RANALIB_THREAD_STACK_SIZE,
        .alignment = MYGO_PAGE_SIZE,
        .kind = MYGO_MEMORY_KIND_ANONYMOUS,
    };
    struct mygo_native_result memory = mrt_call(
        MYGO_SLOT_memory_create,
        process,
        (uint64_t)(uintptr_t)&memory_request,
        0,
        0,
        0,
        0);
    if (memory.status != MYGO_STATUS_ok) {
        free(context);
        return memory.status == MYGO_STATUS_core_resource_exhausted ? thrd_nomem : thrd_error;
    }

    mygo_thread_create_request request = {
        .entry = (uint64_t)(uintptr_t)thread_entry,
        .stack_memory = memory.value0,
        .stack_size = RANALIB_THREAD_STACK_SIZE,
        .argument = (uint64_t)(uintptr_t)context,
    };
    struct mygo_native_result created = mrt_call(
        MYGO_SLOT_thread_create,
        process,
        (uint64_t)(uintptr_t)&request,
        mrt_current_component(),
        0,
        0,
        0);
    if (created.status != MYGO_STATUS_ok) {
        (void)mrt_handle_close(memory.value0);
        free(context);
        return created.status == MYGO_STATUS_core_resource_exhausted ? thrd_nomem : thrd_error;
    }
    if (mrt_handle_close(memory.value0) != MYGO_STATUS_ok) {
        mrt_abort();
    }
    __atomic_store_n(&context->handle, created.value0, __ATOMIC_RELEASE);
    *thread = (thrd_t){created.value0, 0, context};
    return thrd_success;
}

int thrd_join(thrd_t thread, int *result) {
    if (thread.handle == 0 || thread.context == 0) {
        return thrd_error;
    }
    struct thread_context *context = thread.context;
    uint32_t state = __atomic_load_n(&context->state, __ATOMIC_ACQUIRE);
    for (;;) {
        if (state != THREAD_JOINABLE && state != THREAD_EXITED) {
            return thrd_error;
        }
        if (__atomic_compare_exchange_n(
                &context->state,
                &state,
                THREAD_JOINING,
                0,
                __ATOMIC_ACQ_REL,
                __ATOMIC_ACQUIRE)) {
            break;
        }
    }
    mygo_thread_result native_result = {0};
    uint32_t status = mrt_call(
        MYGO_SLOT_thread_join,
        thread.handle,
        (uint64_t)(uintptr_t)&native_result,
        UINT64_MAX,
        0,
        0,
        0)
        .status;
    if (status != MYGO_STATUS_ok) {
        uint32_t joining = THREAD_JOINING;
        (void)__atomic_compare_exchange_n(
            &context->state,
            &joining,
            THREAD_JOINABLE,
            0,
            __ATOMIC_ACQ_REL,
            __ATOMIC_ACQUIRE);
        return status == MYGO_STATUS_thread_timeout ? thrd_timedout : thrd_error;
    }
    if (result != 0) {
        *result = (int)native_result.exit_code;
    }
    (void)mrt_handle_close(thread.handle);
    free(context);
    return thrd_success;
}

int thrd_detach(thrd_t thread) {
    if (thread.handle == 0 || thread.context == 0) {
        return thrd_error;
    }
    struct thread_context *context = thread.context;
    uint32_t state = __atomic_load_n(&context->state, __ATOMIC_ACQUIRE);
    for (;;) {
        uint32_t desired;
        if (state == THREAD_JOINABLE) {
            desired = THREAD_DETACHED;
        } else if (state == THREAD_EXITED) {
            desired = THREAD_JOINING;
        } else {
            return thrd_error;
        }
        if (__atomic_compare_exchange_n(
                &context->state,
                &state,
                desired,
                0,
                __ATOMIC_ACQ_REL,
                __ATOMIC_ACQUIRE)) {
            if (mrt_handle_close(thread.handle) != MYGO_STATUS_ok) {
                return thrd_error;
            }
            if (desired == THREAD_JOINING) {
                free(context);
            }
            return thrd_success;
        }
    }
}

_Noreturn void thrd_exit(int result) {
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (process == 0) {
        mrt_abort();
    }
    struct thread_context *context = current_context;
    if (context != 0) {
        uint32_t previous = __atomic_exchange_n(
            &context->state, THREAD_EXITED, __ATOMIC_ACQ_REL);
        if (previous != THREAD_JOINABLE && previous != THREAD_JOINING &&
            previous != THREAD_DETACHED) {
            mrt_abort();
        }
        if (previous == THREAD_DETACHED) {
            free(context);
        }
    }
    (void)mrt_call(
        MYGO_SLOT_thread_exit,
        process,
        (uint32_t)result,
        0,
        0,
        0,
        0);
    mrt_abort();
}

int thrd_sleep(const struct timespec *duration, struct timespec *remaining) {
    if (duration == 0 || duration->tv_sec < 0 || duration->tv_nsec < 0 ||
        duration->tv_nsec >= 1000000000L) {
        return -1;
    }
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    uint64_t clock_handle = mrt_initial_handle(MYGO_REQUIREMENT_monotonic_clock);
    if (process == 0 || clock_handle == 0) {
        return -1;
    }
    struct mygo_native_result now =
        mrt_call(MYGO_SLOT_clock_read, clock_handle, 0, 0, 0, 0, 0);
    if (now.status != MYGO_STATUS_ok) {
        return -1;
    }
    uint64_t seconds = (uint64_t)duration->tv_sec;
    if (seconds > (UINT64_MAX - (uint64_t)duration->tv_nsec) / UINT64_C(1000000000)) {
        return -1;
    }
    uint64_t deadline = now.value0 + seconds * UINT64_C(1000000000) +
        (uint64_t)duration->tv_nsec;
    if (deadline < now.value0) {
        return -1;
    }
    struct mygo_native_result port =
        mrt_call(MYGO_SLOT_event_create, process, 1, 0, 0, 0, 0);
    if (port.status != MYGO_STATUS_ok) {
        return -1;
    }
    struct mygo_native_result timer = mrt_call(
        MYGO_SLOT_event_timer,
        port.value0,
        deadline,
        0,
        0,
        0,
        0);
    mygo_event_record record = {0};
    struct mygo_native_result waited = timer.status == MYGO_STATUS_ok
        ? mrt_call(
              MYGO_SLOT_event_wait,
              port.value0,
              (uint64_t)(uintptr_t)&record,
              1,
              deadline,
              0,
              0)
        : timer;
    (void)mrt_handle_close(port.value0);
    if (remaining != 0) {
        remaining->tv_sec = 0;
        remaining->tv_nsec = 0;
    }
    return waited.status == MYGO_STATUS_ok ? 0 : -1;
}

void thrd_yield(void) {
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (process != 0) {
        (void)mrt_call(MYGO_SLOT_thread_yield, process, 0, 0, 0, 0, 0);
    }
}

int thrd_equal(thrd_t left, thrd_t right) { return left.handle == right.handle; }

thrd_t thrd_current(void) {
    return (thrd_t){current_identity(), 0, current_context};
}

int mtx_init(mtx_t *mutex, int type) {
    if (mutex == 0 || (type & ~(mtx_recursive | mtx_timed)) != 0) {
        return thrd_error;
    }
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (process == 0) {
        return thrd_error;
    }
    struct mygo_native_result channels =
        mrt_call(MYGO_SLOT_channel_create, process, 1, 0, 0, 0, 0);
    if (channels.status != MYGO_STATUS_ok || channels.value0 == 0 || channels.value1 == 0) {
        return channels.status == MYGO_STATUS_core_resource_exhausted ? thrd_nomem : thrd_error;
    }
    *mutex = (mtx_t){
        .flags = (uint32_t)type,
        .send_handle = channels.value0,
        .receive_handle = channels.value1,
    };
    return thrd_success;
}

void mtx_destroy(mtx_t *mutex) {
    if (mutex == 0) {
        return;
    }
    uint64_t send_handle = mutex->send_handle;
    uint64_t receive_handle = mutex->receive_handle;
    *mutex = (mtx_t){0};
    if (send_handle != 0) {
        (void)mrt_handle_close(send_handle);
    }
    if (receive_handle != 0) {
        (void)mrt_handle_close(receive_handle);
    }
}

int mtx_trylock(mtx_t *mutex) {
    if (mutex == 0 || mutex->send_handle == 0 || mutex->receive_handle == 0) {
        return thrd_error;
    }
    uint64_t identity = current_identity();
    if ((mutex->flags & mtx_recursive) != 0 && mutex->owner == identity && mutex->locked != 0) {
        if (mutex->recursion == UINT32_MAX) {
            return thrd_error;
        }
        ++mutex->recursion;
        return thrd_success;
    }
    if (__atomic_test_and_set(&mutex->locked, __ATOMIC_ACQUIRE)) {
        return thrd_busy;
    }
    mutex->owner = identity;
    mutex->recursion = 1;
    return thrd_success;
}

static int mutex_wait_token(mtx_t *mutex, uint64_t deadline_ns) {
    unsigned char token = 0;
    mygo_channel_message message = {
        .data_ptr = (uint64_t)(uintptr_t)&token,
        .data_capacity = 1,
    };
    struct mygo_native_result received = mrt_call(
        MYGO_SLOT_channel_receive,
        mutex->receive_handle,
        (uint64_t)(uintptr_t)&message,
        deadline_ns,
        0,
        0,
        0);
    if (received.status == MYGO_STATUS_channel_empty) {
        return thrd_timedout;
    }
    if (received.status != MYGO_STATUS_ok) {
        return thrd_error;
    }
    if (received.value0 != 1 || received.value1 != 0 || token != 1) {
        mrt_abort();
    }
    return thrd_success;
}

static int mutex_lock_until(mtx_t *mutex, uint64_t deadline_ns) {
    for (;;) {
        int status = mtx_trylock(mutex);
        if (status != thrd_busy) {
            return status;
        }
        __atomic_fetch_add(&mutex->waiter_count, 1, __ATOMIC_ACQ_REL);
        status = mtx_trylock(mutex);
        if (status == thrd_busy) {
            status = mutex_wait_token(mutex, deadline_ns);
        }
        __atomic_fetch_sub(&mutex->waiter_count, 1, __ATOMIC_ACQ_REL);
        if (status == thrd_success) {
            if (mutex->locked != 0 && mutex->owner == current_identity()) {
                return thrd_success;
            }
            continue;
        }
        return status;
    }
}

int mtx_lock(mtx_t *mutex) {
    return mutex_lock_until(mutex, UINT64_MAX);
}

int mtx_timedlock(mtx_t *mutex, const struct timespec *deadline) {
    if (mutex == 0 || (mutex->flags & mtx_timed) == 0 || deadline == 0 ||
        deadline->tv_sec < 0 || deadline->tv_nsec < 0 || deadline->tv_nsec >= 1000000000L) {
        return thrd_error;
    }
    uint64_t seconds = (uint64_t)deadline->tv_sec;
    if (seconds > (UINT64_MAX - (uint64_t)deadline->tv_nsec) / UINT64_C(1000000000)) {
        return thrd_error;
    }
    return mutex_lock_until(
        mutex, seconds * UINT64_C(1000000000) + (uint64_t)deadline->tv_nsec);
}

void mtx_unlock(mtx_t *mutex) {
    if (mutex == 0 || mutex->locked == 0) {
        return;
    }
    if (mutex->recursion > 1) {
        --mutex->recursion;
        return;
    }
    mutex->owner = 0;
    mutex->recursion = 0;
    __atomic_clear(&mutex->locked, __ATOMIC_RELEASE);
    if (__atomic_load_n(&mutex->waiter_count, __ATOMIC_ACQUIRE) == 0) {
        return;
    }
    unsigned char token = 1;
    mygo_channel_message message = {
        .data_ptr = (uint64_t)(uintptr_t)&token,
        .data_size = 1,
        .data_capacity = 1,
    };
    uint32_t status = mrt_call(
        MYGO_SLOT_channel_send,
        mutex->send_handle,
        (uint64_t)(uintptr_t)&message,
        0,
        0,
        0,
        0)
        .status;
    if (status != MYGO_STATUS_ok && status != MYGO_STATUS_channel_full) {
        mrt_abort();
    }
}

#define RANALIB_CONDITION_CAPACITY UINT64_C(64)

static int condition_send_token(cnd_t *condition) {
    unsigned char token = 1;
    mygo_channel_message message = {
        .data_ptr = (uint64_t)(uintptr_t)&token,
        .data_size = 1,
        .data_capacity = 1,
    };
    return mrt_call(
               MYGO_SLOT_channel_send,
               condition->send_handle,
               (uint64_t)(uintptr_t)&message,
               0,
               0,
               0,
               0)
               .status == MYGO_STATUS_ok
        ? thrd_success
        : thrd_error;
}

static uint32_t condition_reserve_wakes(cnd_t *condition, uint32_t maximum) {
    for (;;) {
        uint32_t waiters = __atomic_load_n(&condition->waiter_count, __ATOMIC_ACQUIRE);
        uint32_t pending = __atomic_load_n(&condition->pending_wakes, __ATOMIC_ACQUIRE);
        if (pending >= waiters || maximum == 0) {
            return 0;
        }
        uint32_t count = waiters - pending;
        if (count > maximum) {
            count = maximum;
        }
        uint32_t desired = pending + count;
        if (__atomic_compare_exchange_n(
                &condition->pending_wakes,
                &pending,
                desired,
                0,
                __ATOMIC_ACQ_REL,
                __ATOMIC_ACQUIRE)) {
            return count;
        }
    }
}

static int condition_wake(cnd_t *condition, uint32_t maximum) {
    if (condition == 0 || condition->send_handle == 0) {
        return thrd_error;
    }
    uint32_t reserved = condition_reserve_wakes(condition, maximum);
    for (uint32_t index = 0; index < reserved; ++index) {
        if (condition_send_token(condition) != thrd_success) {
            __atomic_fetch_sub(
                &condition->pending_wakes, reserved - index, __ATOMIC_ACQ_REL);
            return thrd_error;
        }
    }
    return thrd_success;
}

int cnd_init(cnd_t *condition) {
    if (condition == 0) {
        return thrd_error;
    }
    uint64_t process = mrt_initial_handle(MYGO_REQUIREMENT_self_process);
    if (process == 0) {
        return thrd_error;
    }
    struct mygo_native_result channels = mrt_call(
        MYGO_SLOT_channel_create,
        process,
        RANALIB_CONDITION_CAPACITY,
        0,
        0,
        0,
        0);
    if (channels.status != MYGO_STATUS_ok || channels.value0 == 0 || channels.value1 == 0) {
        return channels.status == MYGO_STATUS_core_resource_exhausted ? thrd_nomem : thrd_error;
    }
    *condition = (cnd_t){channels.value0, channels.value1, 0, 0};
    return thrd_success;
}

void cnd_destroy(cnd_t *condition) {
    if (condition == 0) {
        return;
    }
    uint64_t send_handle = condition->send_handle;
    uint64_t receive_handle = condition->receive_handle;
    *condition = (cnd_t){0};
    if (send_handle != 0) {
        (void)mrt_handle_close(send_handle);
    }
    if (receive_handle != 0) {
        (void)mrt_handle_close(receive_handle);
    }
}

int cnd_signal(cnd_t *condition) { return condition_wake(condition, 1); }

int cnd_broadcast(cnd_t *condition) { return condition_wake(condition, UINT32_MAX); }

static int condition_wait(cnd_t *condition, mtx_t *mutex, uint64_t deadline_ns) {
    if (condition == 0 || mutex == 0 || condition->receive_handle == 0) {
        return thrd_error;
    }
    __atomic_fetch_add(&condition->waiter_count, 1, __ATOMIC_ACQ_REL);
    mtx_unlock(mutex);

    unsigned char token = 0;
    mygo_channel_message message = {
        .data_ptr = (uint64_t)(uintptr_t)&token,
        .data_capacity = 1,
    };
    struct mygo_native_result received = mrt_call(
        MYGO_SLOT_channel_receive,
        condition->receive_handle,
        (uint64_t)(uintptr_t)&message,
        deadline_ns,
        0,
        0,
        0);
    if (received.status == MYGO_STATUS_ok) {
        if (received.value0 != 1 || received.value1 != 0 || token != 1) {
            mrt_abort();
        }
        __atomic_fetch_sub(&condition->pending_wakes, 1, __ATOMIC_ACQ_REL);
    }
    __atomic_fetch_sub(&condition->waiter_count, 1, __ATOMIC_ACQ_REL);
    int lock_status = mtx_lock(mutex);
    if (lock_status != thrd_success) {
        return lock_status;
    }
    if (received.status == MYGO_STATUS_ok) {
        return thrd_success;
    }
    return received.status == MYGO_STATUS_channel_empty ? thrd_timedout : thrd_error;
}

int cnd_wait(cnd_t *condition, mtx_t *mutex) {
    return condition_wait(condition, mutex, UINT64_MAX);
}

int cnd_timedwait(cnd_t *condition, mtx_t *mutex, const struct timespec *deadline) {
    if (deadline == 0 || deadline->tv_sec < 0 || deadline->tv_nsec < 0 ||
        deadline->tv_nsec >= 1000000000L) {
        return thrd_error;
    }
    uint64_t seconds = (uint64_t)deadline->tv_sec;
    if (seconds > (UINT64_MAX - (uint64_t)deadline->tv_nsec) / UINT64_C(1000000000)) {
        return thrd_error;
    }
    return condition_wait(
        condition,
        mutex,
        seconds * UINT64_C(1000000000) + (uint64_t)deadline->tv_nsec);
}

void call_once(once_flag *flag, void (*function)(void)) {
    unsigned char expected = 0;
    if (__atomic_compare_exchange_n(
            &flag->state, &expected, 1, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        function();
        __atomic_store_n(&flag->state, 2, __ATOMIC_RELEASE);
        return;
    }
    while (__atomic_load_n(&flag->state, __ATOMIC_ACQUIRE) != 2) {
        thrd_yield();
    }
}
