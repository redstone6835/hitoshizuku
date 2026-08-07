#ifndef RANALIB_ERRNO_H
#define RANALIB_ERRNO_H

#define EIO 5
#define EBADF 9
#define EAGAIN 11
#define EFAULT 14
#define EPIPE 32

extern int ranalib_errno __attribute__((visibility("hidden")));
#define errno ranalib_errno

#endif
