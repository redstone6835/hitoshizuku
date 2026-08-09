#include <stdint.h>

uint64_t component_tls_offset;
_Thread_local uint64_t component_tls_calls;

static uint64_t *current_call_count(void) {
    uintptr_t thread_pointer = (uintptr_t)__builtin_thread_pointer();
    return (uint64_t *)(thread_pointer + component_tls_offset);
}

__attribute__((used)) uint32_t component_anchor(const void *context) {
    return context != 0;
}

uint32_t component_init(const void *context) {
    return context == 0;
}

uint32_t component_fini(const void *context) {
    return context == 0;
}

uint64_t plugin_add(uint64_t left, uint64_t right) {
    uint64_t *calls = current_call_count();
    *calls += 1;
    if (*calls != 1) {
        return 0;
    }
    return left + right;
}
