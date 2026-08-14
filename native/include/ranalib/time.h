#ifndef RANALIB_TIME_H
#define RANALIB_TIME_H

#include <stddef.h>
#include <stdint.h>

typedef int64_t time_t;
typedef int64_t clock_t;

struct timespec {
    time_t tv_sec;
    long tv_nsec;
};

#define CLOCKS_PER_SEC 1000000000L
#define TIME_UTC 1
#define RANALIB_TIME_MONOTONIC 2

clock_t clock(void);
int timespec_get(struct timespec *time, int base);

#endif
