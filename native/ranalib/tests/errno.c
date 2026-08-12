#include <assert.h>
#include <pthread.h>

#include <ranalib/errno.h>

static void *set_worker_errno(void *argument) {
    (void)argument;
    errno = 73;
    assert(errno == 73);
    return 0;
}

int main(void) {
    errno = 11;
    pthread_t thread;
    assert(pthread_create(&thread, 0, set_worker_errno, 0) == 0);
    assert(pthread_join(thread, 0) == 0);
    assert(errno == 11);
    return 0;
}
