#ifndef RANALIB_THREADS_H
#define RANALIB_THREADS_H

#include <stdint.h>

#include <ranalib/time.h>

#define __STDC_NO_THREADS__ 0

enum {
    thrd_success = 0,
    thrd_nomem = 1,
    thrd_timedout = 2,
    thrd_busy = 3,
    thrd_error = 4,
};

enum { mtx_plain = 0, mtx_recursive = 1, mtx_timed = 2 };

typedef int (*thrd_start_t)(void *argument);

typedef struct {
    uint64_t handle;
    uint64_t stack_memory;
    void *context;
} thrd_t;

typedef struct {
    unsigned char locked;
    uint32_t flags;
    uint64_t owner;
    uint32_t recursion;
    uint32_t waiter_count;
    uint64_t send_handle;
    uint64_t receive_handle;
} mtx_t;

typedef struct {
    uint64_t send_handle;
    uint64_t receive_handle;
    uint32_t waiter_count;
    uint32_t pending_wakes;
} cnd_t;

typedef struct {
    unsigned char state;
} once_flag;

#define ONCE_FLAG_INIT {0}

int thrd_create(thrd_t *thread, thrd_start_t function, void *argument);
int thrd_join(thrd_t thread, int *result);
int thrd_detach(thrd_t thread);
_Noreturn void thrd_exit(int result);
int thrd_sleep(const struct timespec *duration, struct timespec *remaining);
void thrd_yield(void);
int thrd_equal(thrd_t left, thrd_t right);
thrd_t thrd_current(void);

int mtx_init(mtx_t *mutex, int type);
void mtx_destroy(mtx_t *mutex);
int mtx_lock(mtx_t *mutex);
int mtx_trylock(mtx_t *mutex);
int mtx_timedlock(mtx_t *mutex, const struct timespec *deadline);
void mtx_unlock(mtx_t *mutex);

int cnd_init(cnd_t *condition);
void cnd_destroy(cnd_t *condition);
int cnd_signal(cnd_t *condition);
int cnd_broadcast(cnd_t *condition);
int cnd_wait(cnd_t *condition, mtx_t *mutex);
int cnd_timedwait(cnd_t *condition, mtx_t *mutex, const struct timespec *deadline);

void call_once(once_flag *flag, void (*function)(void));

#endif
