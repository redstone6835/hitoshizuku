#include <mrt/mrt.h>
#include <ranalib/stdlib.h>

_Noreturn void exit(int status) {
    mrt_exit((uint32_t)status);
}

_Noreturn void _Exit(int status) {
    mrt_terminate((uint32_t)status);
}
