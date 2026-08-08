#include <assert.h>
#include <stdint.h>

#include "mygo_program.h"
#include <mrt/mrt.h>

static unsigned events[4];
static unsigned event_count;

static void init_first(void) {
    events[event_count++] = 1;
}

static void init_second(void) {
    events[event_count++] = 2;
}

static void fini_first(void) {
    events[event_count++] = 3;
}

static void fini_second(void) {
    events[event_count++] = 4;
}

static uint64_t image_base(void) {
    uintptr_t values[] = {
        (uintptr_t)init_first,
        (uintptr_t)init_second,
        (uintptr_t)fini_first,
        (uintptr_t)fini_second,
    };
    uintptr_t minimum = values[0];
    for (unsigned index = 1; index < 4; ++index) {
        if (values[index] < minimum) {
            minimum = values[index];
        }
    }
    return (uint64_t)(minimum & ~(uintptr_t)(MYGO_PAGE_SIZE - 1));
}

int main(void) {
    uint64_t base = image_base();
    uint64_t init[] = {
        (uint64_t)(uintptr_t)init_first - base,
        (uint64_t)(uintptr_t)init_second - base,
    };
    uint64_t fini[] = {
        (uint64_t)(uintptr_t)fini_first - base,
        (uint64_t)(uintptr_t)fini_second - base,
    };
    struct mygo_start_info info = {0};
    info.image_base = base;
    struct mrt_start_view view = {
        .info = &info,
        .init_array = init,
        .init_array_count = 2,
        .fini_array = fini,
        .fini_array_count = 2,
    };

    mrt_run_initializers(&view);
    mrt_run_finalizers(&view);

    assert(event_count == 4);
    assert(events[0] == 1);
    assert(events[1] == 2);
    assert(events[2] == 4);
    assert(events[3] == 3);
    return 0;
}
