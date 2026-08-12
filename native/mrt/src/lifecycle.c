#include <mrt/mrt.h>

typedef void (*mrt_lifecycle_function)(void);

static void invoke(const struct mrt_start_view *view, uint64_t offset) {
    uintptr_t address = (uintptr_t)(view->info->image_base + offset);
    ((mrt_lifecycle_function)address)();
}

void mrt_run_initializers(const struct mrt_start_view *view) {
    for (uint32_t index = 0; index < view->init_array_count; ++index) {
        invoke(view, view->init_array[index]);
    }
}

void mrt_run_finalizers(const struct mrt_start_view *view) {
    for (uint32_t index = view->fini_array_count; index != 0; --index) {
        invoke(view, view->fini_array[index - 1]);
    }
}
