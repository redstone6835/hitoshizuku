#include <stdint.h>

#include <mrt/mrt.h>

typedef uint32_t (*mrt_component_lifecycle_fn)(const mygo_component_context *context);

static _Thread_local uint64_t current_component;

uint64_t mrt_current_component(void) {
    return current_component;
}

#if (MYGO_HAS_component_load && MYGO_HAS_component_activate && MYGO_HAS_component_finish) || \
    (MYGO_HAS_component_unload && MYGO_HAS_component_finish)
static uint32_t run_lifecycle(const mygo_component_lifecycle *lifecycle) {
    mrt_component_lifecycle_fn entry =
        (mrt_component_lifecycle_fn)(uintptr_t)lifecycle->entry;
    const mygo_component_context *context =
        (const mygo_component_context *)(uintptr_t)lifecycle->context;
    if (entry == 0 || context == 0) {
        return MYGO_STATUS_component_lifecycle_failed;
    }
    uint64_t previous_component = current_component;
    current_component = lifecycle->component;
    uint32_t result = entry(context);
    current_component = previous_component;
    return result;
}
#endif

struct mrt_component_result mrt_component_load(
    uint64_t process,
    const mygo_component_load_request *request) {
#if MYGO_HAS_component_load && MYGO_HAS_component_activate && MYGO_HAS_component_finish
    mygo_component_lifecycle lifecycle = {0};
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_component_load,
        process,
        (uint64_t)(uintptr_t)request,
        (uint64_t)(uintptr_t)&lifecycle,
        0,
        0,
        0);
    uint64_t transaction = result.value0;
    while (result.status == MYGO_STATUS_ok && lifecycle.action != MYGO_COMPONENT_ACTION_NONE) {
        uint32_t lifecycle_status = run_lifecycle(&lifecycle);
        if (lifecycle.action == MYGO_COMPONENT_ACTION_INITIALIZE) {
            result = mrt_call(
                MYGO_SLOT_component_activate,
                transaction,
                lifecycle_status,
                (uint64_t)(uintptr_t)&lifecycle,
                0,
                0,
                0);
        } else if (lifecycle.action == MYGO_COMPONENT_ACTION_FINALIZE) {
            result = mrt_call(
                MYGO_SLOT_component_finish,
                transaction,
                lifecycle_status,
                (uint64_t)(uintptr_t)&lifecycle,
                0,
                0,
                0);
        } else {
            result.status = MYGO_STATUS_component_invalid_transaction;
            result.value0 = 0;
            break;
        }
        if (result.value0 != 0) {
            transaction = result.value0;
        }
    }
    return (struct mrt_component_result){result.status, result.value0};
#else
    (void)process;
    (void)request;
    return (struct mrt_component_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}

uint32_t mrt_component_query(uint64_t component, mygo_component_query *query) {
#if MYGO_HAS_component_query
    return mrt_call(
               MYGO_SLOT_component_query,
               component,
               (uint64_t)(uintptr_t)query,
               0,
               0,
               0,
               0)
        .status;
#else
    (void)component;
    (void)query;
    return MYGO_STATUS_abi_unsupported_operation;
#endif
}

struct mrt_interface_result mrt_component_interface(
    uint64_t component,
    const mygo_interface_request *request) {
#if MYGO_HAS_component_interface
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_component_interface,
        component,
        (uint64_t)(uintptr_t)request,
        0,
        0,
        0,
        0);
    return (struct mrt_interface_result){
        result.status,
        result.value0,
        (const mygo_component_interface_gate *)(uintptr_t)result.value1,
    };
#else
    (void)component;
    (void)request;
    return (struct mrt_interface_result){MYGO_STATUS_abi_unsupported_operation, 0, 0};
#endif
}

struct mrt_component_result mrt_component_unload(
    uint64_t component,
    uint64_t deadline_ns) {
#if MYGO_HAS_component_unload && MYGO_HAS_component_finish
    mygo_component_lifecycle lifecycle = {0};
    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_component_unload,
        component,
        deadline_ns,
        (uint64_t)(uintptr_t)&lifecycle,
        current_component,
        0,
        0);
    uint64_t transaction = result.value0;
    while (result.status == MYGO_STATUS_ok && lifecycle.action != MYGO_COMPONENT_ACTION_NONE) {
        if (lifecycle.action != MYGO_COMPONENT_ACTION_FINALIZE) {
            result.status = MYGO_STATUS_component_invalid_transaction;
            result.value0 = 0;
            break;
        }
        uint32_t lifecycle_status = run_lifecycle(&lifecycle);
        result = mrt_call(
            MYGO_SLOT_component_finish,
            transaction,
            lifecycle_status,
            (uint64_t)(uintptr_t)&lifecycle,
            0,
            0,
            0);
    }
    return (struct mrt_component_result){result.status, result.value0};
#else
    (void)component;
    (void)deadline_ns;
    return (struct mrt_component_result){MYGO_STATUS_abi_unsupported_operation, 0};
#endif
}

struct mrt_component_call mrt_component_enter(
    const mygo_component_interface_gate *gate) {
    if (gate == 0 || gate->target == 0 || gate->call_state == 0) {
        return (struct mrt_component_call){MYGO_STATUS_component_unloaded, 0, 0};
    }
    mygo_component_call_state *state =
        (mygo_component_call_state *)(uintptr_t)gate->call_state;
    uint32_t component_state = __atomic_load_n(&state->state, __ATOMIC_ACQUIRE);
    uint64_t generation = __atomic_load_n(&state->generation, __ATOMIC_ACQUIRE);
    if (component_state != MYGO_COMPONENT_STATE_ACTIVE || generation != gate->generation) {
        return (struct mrt_component_call){MYGO_STATUS_component_unloaded, 0, 0};
    }
    __atomic_fetch_add(&state->active_calls, 1, __ATOMIC_ACQ_REL);
    component_state = __atomic_load_n(&state->state, __ATOMIC_ACQUIRE);
    generation = __atomic_load_n(&state->generation, __ATOMIC_ACQUIRE);
    if (component_state != MYGO_COMPONENT_STATE_ACTIVE || generation != gate->generation) {
        __atomic_fetch_sub(&state->active_calls, 1, __ATOMIC_RELEASE);
        return (struct mrt_component_call){MYGO_STATUS_component_unloaded, 0, 0};
    }
    uint64_t previous = current_component;
    current_component = gate->component;
    return (struct mrt_component_call){MYGO_STATUS_ok, gate->target, previous};
}

void mrt_component_leave(
    const mygo_component_interface_gate *gate,
    uint64_t previous_component) {
    if (gate == 0 || gate->call_state == 0) {
        return;
    }
    mygo_component_call_state *state =
        (mygo_component_call_state *)(uintptr_t)gate->call_state;
    current_component = previous_component;
    uint64_t previous = __atomic_fetch_sub(&state->active_calls, 1, __ATOMIC_ACQ_REL);
    if (previous == 1 &&
        __atomic_load_n(&state->state, __ATOMIC_ACQUIRE) == MYGO_COMPONENT_STATE_DRAINING &&
        gate->component != gate->call_state) {
#if MYGO_HAS_component_wake
        (void)mrt_call(
            MYGO_SLOT_component_wake,
            gate->component,
            gate->generation,
            0,
            0,
            0,
            0);
#endif
    }
}
