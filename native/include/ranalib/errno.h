#ifndef RANALIB_ERRNO_H
#define RANALIB_ERRNO_H

#define EIO 5
#define EBADF 9
#define ENOMEM 12
#define EAGAIN 11
#define EFAULT 14
#define EINVAL 22
#define EPIPE 32
#define EOVERFLOW 75

extern _Thread_local int ranalib_errno __attribute__((visibility("hidden")));
#define errno ranalib_errno

#endif
