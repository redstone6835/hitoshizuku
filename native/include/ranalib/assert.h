#ifndef RANALIB_ASSERT_H
#define RANALIB_ASSERT_H

#ifdef NDEBUG
#define assert(expression) ((void)0)
#else
_Noreturn void ranalib_assert_fail(
    const char *expression,
    const char *file,
    int line,
    const char *function);
#define assert(expression) \
    ((expression) ? (void)0 : ranalib_assert_fail(#expression, __FILE__, __LINE__, __func__))
#endif

#endif
