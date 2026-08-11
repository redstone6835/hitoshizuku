#include <stdint.h>

uint64_t clock_slot;
struct component_interface_gate {
    uint64_t call_state;
    uint64_t target;
    uint64_t component;
    uint64_t generation;
};
struct component_interface_gate math_add_gate;
uint64_t component_tls_offset;
_Thread_local uint64_t component_tls_value = 7;

__attribute__((used)) uint32_t component_anchor(const void *context) {
    return context != 0;
}

uint32_t component_init(const void *context) {
    return context == 0;
}

uint32_t component_fini(const void *context) {
    return context == 0;
}

uint64_t plugin_run(uint64_t left, uint64_t right) {
    if (math_add_gate.target == 0) {
        return clock_slot;
    }
    return left + right;
}
