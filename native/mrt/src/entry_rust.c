#include <mrt/mrt.h>

int mrt_prepare_program(const struct mrt_start_view *view) {
    return view == 0 || view->info == 0 ? -1 : 0;
}

extern int main(void);

struct mrt_program_result mrt_invoke_program(const struct mrt_start_view *view) {
    (void)view;
    struct mrt_program_result result = {main()};
    return result;
}
