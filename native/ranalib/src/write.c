#include <limits.h>

#include <mrt/mrt.h>
#include <ranalib/errno.h>
#include <ranalib/unistd.h>

static int status_errno(uint32_t status) {
    if (status == MYGO_STATUS_IO_WOULD_BLOCK) {
        return EAGAIN;
    }
    if (status == MYGO_STATUS_IO_FAULT) {
        return EFAULT;
    }
    if (status == MYGO_STATUS_IO_CLOSED) {
        return EPIPE;
    }
    return EIO;
}

long write(int fd, const void *buffer, unsigned long size) {
    if (fd != 1) {
        errno = EBADF;
        return -1;
    }
    uint64_t stream = mrt_initial_handle(MYGO_REQUIREMENT_STDOUT);
    if (stream == 0) {
        errno = EBADF;
        return -1;
    }

    struct mygo_native_result result = mrt_call(
        MYGO_SLOT_STREAM_WRITE,
        stream,
        (uintptr_t)buffer,
        size,
        0,
        0,
        0);
    if (result.status != MYGO_STATUS_OK || result.value0 > LONG_MAX) {
        errno = result.status == MYGO_STATUS_OK ? EIO : status_errno(result.status);
        return -1;
    }
    return (long)result.value0;
}
