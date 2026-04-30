#![no_std]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Errno {
    ESUCCESS = 0,
    EPERM = 1,
    ENOENT = 2,
    ESRCH = 3,
    EINTR = 4,
    EIO = 5,
    EAGAIN = 11,
    ECHILD = 10,
    ENOEXEC = 8,
    ENOMEM = 12,
    EACCES = 13,
    EBADF = 9,
    EFAULT = 14,
    EBUSY = 16,
    EEXIST = 17,
    ENODEV = 19,
    ENOTDIR = 20,
    EISDIR = 21,
    EXDEV = 18,
    ENFILE = 23,
    EMFILE = 24,
    EMLINK = 31,
    ENOTTY = 25,
    EINVAL = 22,
    EFBIG = 27,
    ENOSPC = 28,
    EROFS = 30,
    EPIPE = 32,
    ERANGE = 34,
    ENAMETOOLONG = 36,
    ENOSYS = 38,
    ENOTEMPTY = 39,
    ELOOP = 40,
    EOPNOTSUPP = 95,
    EAFNOSUPPORT = 97,
    ECONNRESET = 104,
    ETIMEDOUT = 110,
    ECONNREFUSED = 111,
    Other(i32),
}

impl Errno {
    pub fn from_i32(code: i32) -> Self {
        match code {
            0 => Errno::ESUCCESS,
            1 => Errno::EPERM,
            2 => Errno::ENOENT,
            3 => Errno::ESRCH,
            4 => Errno::EINTR,
            5 => Errno::EIO,
            8 => Errno::ENOEXEC,
            9 => Errno::EBADF,
            10 => Errno::ECHILD,
            11 => Errno::EAGAIN,
            12 => Errno::ENOMEM,
            13 => Errno::EACCES,
            14 => Errno::EFAULT,
            16 => Errno::EBUSY,
            17 => Errno::EEXIST,
            18 => Errno::EXDEV,
            19 => Errno::ENODEV,
            20 => Errno::ENOTDIR,
            21 => Errno::EISDIR,
            23 => Errno::ENFILE,
            24 => Errno::EMFILE,
            25 => Errno::ENOTTY,
            31 => Errno::EMLINK,
            32 => Errno::EPIPE,
            22 => Errno::EINVAL,
            27 => Errno::EFBIG,
            28 => Errno::ENOSPC,
            30 => Errno::EROFS,
            34 => Errno::ERANGE,
            36 => Errno::ENAMETOOLONG,
            38 => Errno::ENOSYS,
            39 => Errno::ENOTEMPTY,
            40 => Errno::ELOOP,
            95 => Errno::EOPNOTSUPP,
            97 => Errno::EAFNOSUPPORT,
            104 => Errno::ECONNRESET,
            110 => Errno::ETIMEDOUT,
            111 => Errno::ECONNREFUSED,
            other => Errno::Other(other),
        }
    }

    pub fn as_i32(self) -> i32 {
        match self {
            Errno::ESUCCESS => 0,
            Errno::EPERM => 1,
            Errno::ENOENT => 2,
            Errno::ESRCH => 3,
            Errno::EINTR => 4,
            Errno::EIO => 5,
            Errno::ENOEXEC => 8,
            Errno::EAGAIN => 11,
            Errno::ECHILD => 10,
            Errno::ENOMEM => 12,
            Errno::EACCES => 13,
            Errno::EBADF => 9,
            Errno::EFAULT => 14,
            Errno::EBUSY => 16,
            Errno::EEXIST => 17,
            Errno::EXDEV => 18,
            Errno::ENODEV => 19,
            Errno::ENOTDIR => 20,
            Errno::EISDIR => 21,
            Errno::ENFILE => 23,
            Errno::EMFILE => 24,
            Errno::EMLINK => 31,
            Errno::ENOTTY => 25,
            Errno::EINVAL => 22,
            Errno::EFBIG => 27,
            Errno::ENOSPC => 28,
            Errno::EROFS => 30,
            Errno::EPIPE => 32,
            Errno::ERANGE => 34,
            Errno::ENAMETOOLONG => 36,
            Errno::ENOSYS => 38,
            Errno::ENOTEMPTY => 39,
            Errno::ELOOP => 40,
            Errno::EOPNOTSUPP => 95,
            Errno::EAFNOSUPPORT => 97,
            Errno::ECONNRESET => 104,
            Errno::ETIMEDOUT => 110,
            Errno::ECONNREFUSED => 111,
            Errno::Other(code) => code,
        }
    }

    pub fn as_usize(self) -> usize {
        (-self.as_i32()) as usize
    }
}

impl From<i32> for Errno {
    fn from(code: i32) -> Self {
        Self::from_i32(code)
    }
}

impl From<Errno> for i32 {
    fn from(errno: Errno) -> Self {
        errno.as_i32()
    }
}
