#include <assert.h>
#include <stdint.h>

#include <mrt/mrt.h>

static mygo_component_context lifecycle_context;
static mygo_component_call_state call_state;
static mygo_component_interface_gate gate;
static unsigned load_step;
static unsigned unload_step;
static unsigned wake_count;
static uint64_t observed_caller;

static uint32_t init_component(const mygo_component_context *context) {
    assert(context == &lifecycle_context);
    return MYGO_STATUS_ok;
}

static uint32_t fini_component(const mygo_component_context *context) {
    assert(context == &lifecycle_context);
    return MYGO_STATUS_ok;
}

struct mygo_native_result mrt_call(
    uint64_t slot,
    uint64_t object_handle,
    uint64_t arg0,
    uint64_t arg1,
    uint64_t arg2,
    uint64_t arg3,
    uint64_t arg4) {
    assert(arg3 == 0);
    assert(arg4 == 0);
    struct mygo_native_result result = {.status = MYGO_STATUS_ok};
    if (slot == MYGO_SLOT_component_load) {
        assert(object_handle == 1);
        assert(arg0 != 0);
        mygo_component_lifecycle *lifecycle = (mygo_component_lifecycle *)(uintptr_t)arg1;
        *lifecycle = (mygo_component_lifecycle){
            .action = MYGO_COMPONENT_ACTION_INITIALIZE,
            .state = MYGO_COMPONENT_STATE_INITIALIZING,
            .entry = (uint64_t)(uintptr_t)init_component,
            .context = (uint64_t)(uintptr_t)&lifecycle_context,
        };
        result.value0 = 11;
        ++load_step;
    } else if (slot == MYGO_SLOT_component_activate) {
        assert(object_handle == 11);
        assert(arg0 == MYGO_STATUS_ok);
        mygo_component_lifecycle *lifecycle = (mygo_component_lifecycle *)(uintptr_t)arg1;
        *lifecycle = (mygo_component_lifecycle){0};
        result.value0 = 22;
        ++load_step;
    } else if (slot == MYGO_SLOT_component_query) {
        assert(object_handle == 22);
        mygo_component_query *query = (mygo_component_query *)(uintptr_t)arg0;
        query->state = MYGO_COMPONENT_STATE_ACTIVE;
    } else if (slot == MYGO_SLOT_component_interface) {
        assert(object_handle == 22);
        assert(arg0 != 0);
        result.value0 = 33;
        result.value1 = (uint64_t)(uintptr_t)&gate;
    } else if (slot == MYGO_SLOT_component_unload) {
        assert(object_handle == 22);
        observed_caller = arg2;
        if (arg2 != 0) {
            result.status = MYGO_STATUS_component_self_unload;
        } else {
            mygo_component_lifecycle *lifecycle =
                (mygo_component_lifecycle *)(uintptr_t)arg1;
            *lifecycle = (mygo_component_lifecycle){
                .action = MYGO_COMPONENT_ACTION_FINALIZE,
                .state = MYGO_COMPONENT_STATE_FINALIZING,
                .entry = (uint64_t)(uintptr_t)fini_component,
                .context = (uint64_t)(uintptr_t)&lifecycle_context,
            };
            result.value0 = 44;
            ++unload_step;
        }
    } else if (slot == MYGO_SLOT_component_finish) {
        assert(object_handle == 44);
        assert(arg0 == MYGO_STATUS_ok);
        mygo_component_lifecycle *lifecycle = (mygo_component_lifecycle *)(uintptr_t)arg1;
        *lifecycle = (mygo_component_lifecycle){0};
        call_state.state = MYGO_COMPONENT_STATE_UNLOADED;
        ++call_state.generation;
        ++unload_step;
    } else if (slot == MYGO_SLOT_component_wake) {
        assert(object_handle == 55);
        assert(arg0 == 7);
        ++wake_count;
    } else {
        assert(0 && "unexpected Native operation");
    }
    return result;
}

int main(void) {
    mygo_component_load_request request = {0};
    struct mrt_component_result loaded = mrt_component_load(1, &request);
    assert(loaded.status == MYGO_STATUS_ok);
    assert(loaded.handle == 22);
    assert(load_step == 2);

    mygo_component_query query = {0};
    assert(mrt_component_query(22, &query) == MYGO_STATUS_ok);
    assert(query.state == MYGO_COMPONENT_STATE_ACTIVE);

    gate = (mygo_component_interface_gate){
        .call_state = (uint64_t)(uintptr_t)&call_state,
        .target = 0x1234,
        .component = 55,
        .generation = 7,
    };
    call_state.state = MYGO_COMPONENT_STATE_ACTIVE;
    call_state.generation = 7;
    mygo_interface_request interface_request = {0};
    struct mrt_interface_result interface = mrt_component_interface(22, &interface_request);
    assert(interface.status == MYGO_STATUS_ok);
    assert(interface.handle == 33);
    assert(interface.gate == &gate);

    struct mrt_component_call call = mrt_component_enter(interface.gate);
    assert(call.status == MYGO_STATUS_ok);
    assert(call.target == 0x1234);
    assert(call_state.active_calls == 1);
    struct mrt_component_result self = mrt_component_unload(22, 0);
    assert(self.status == MYGO_STATUS_component_self_unload);
    assert(observed_caller == 55);
    call_state.state = MYGO_COMPONENT_STATE_DRAINING;
    mrt_component_leave(interface.gate, call.previous_component);
    assert(call_state.active_calls == 0);
    assert(wake_count == 1);

    struct mrt_component_result unloaded = mrt_component_unload(22, 0);
    assert(unloaded.status == MYGO_STATUS_ok);
    assert(unload_step == 2);
    assert(observed_caller == 0);
    struct mrt_component_call stale = mrt_component_enter(interface.gate);
    assert(stale.status == MYGO_STATUS_component_unloaded);
    return 0;
}
