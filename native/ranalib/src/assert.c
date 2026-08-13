#include <mrt/mrt.h>
#include <ranalib/assert.h>
#include <ranalib/stdio.h>

_Noreturn void ranalib_assert_fail(
    const char *expression,
    const char *file,
    int line,
    const char *function) {
    (void)fprintf(
        stderr,
        "assertion failed: %s (%s:%d, %s)\n",
        expression,
        file,
        line,
        function);
    mrt_abort();
}
