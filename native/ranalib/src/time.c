#include <mrt/mrt.h>
#include <ranalib/time.h>

static int monotonic_ns(uint64_t *value) {
#if MYGO_HAS_clock_read
    uint64_t clock_handle = mrt_initial_handle(MYGO_REQUIREMENT_monotonic_clock);
    if (clock_handle == 0) {
        return 0;
    }
    struct mygo_native_result result =
        mrt_call(MYGO_SLOT_clock_read, clock_handle, 0, 0, 0, 0, 0);
    if (result.status != MYGO_STATUS_ok) {
        return 0;
    }
    *value = result.value0;
    return 1;
#else
    (void)value;
    return 0;
#endif
}

clock_t clock(void) {
    uint64_t value = 0;
    return monotonic_ns(&value) && value <= INT64_MAX ? (clock_t)value : (clock_t)-1;
}

int timespec_get(struct timespec *time, int base) {
    if (time == 0 || base != RANALIB_TIME_MONOTONIC) {
        return 0;
    }
    uint64_t value = 0;
    if (!monotonic_ns(&value) || value / UINT64_C(1000000000) > INT64_MAX) {
        return 0;
    }
    time->tv_sec = (time_t)(value / UINT64_C(1000000000));
    time->tv_nsec = (long)(value % UINT64_C(1000000000));
    return base;
}
