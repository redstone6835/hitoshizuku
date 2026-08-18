//! 文件系统相关 syscall。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::{ControlFlow, Deref};

use log::printk;

use errno::Errno;
use general::mm::{VmSpace, copy_cstr_from_user, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::{current_fdtable, current_vfs_context, namespace_path, pidfd};
use hal::abi::{decode_dev_t, encode_dev_t};
use mm::UserAccessError;
use sched::{Capability, SigProcMaskHow, SigSet};
use vfs::cred::{Gid, Uid};
use vfs::error::VfsError;
use vfs::fdtable::{Fd, FdFlags};
use vfs::file::{AccessMode, FallocateMode, IoctlCmd, OpenOptions, PollEvents, SeekFrom};
use vfs::mount::MountFlags;
use vfs::operation;
use vfs::path::{Dirfd, LookupFlags};
use vfs::socket as vfs_socket;
use vfs::stat::{DevId, FileMode, FileStat, FileType, FsStat, Timespec};

/// 单次最多从用户态拷到内核临时缓冲的字节数。
const COPY_CHUNK: usize = 8192;
const MAX_SOCKET_IO: usize = 256 * 1024;
/// 单个 sendmsg/recvmsg 等系统调用允许的 iovec 数量上限（Linux 的 `IOV_MAX`）。
const IOV_MAX: usize = 1024;
const MAX_SOCKET_CONTROL: usize = 4096;
const MAX_SOCKET_ADDR: usize = 128;
const PATH_MAX: usize = 4096;
const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_EACCESS: usize = 0x200;
const AT_NO_AUTOMOUNT: usize = 0x800;
const AT_EMPTY_PATH: usize = 0x1000;
const AT_STATX_FORCE_SYNC: usize = 0x2000;
const AT_STATX_DONT_SYNC: usize = 0x4000;

struct SocketAddressBuffer {
    bytes: [u8; MAX_SOCKET_ADDR],
    len: usize,
}

impl Deref for SocketAddressBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes[..self.len]
    }
}

const F_OK: usize = 0;
const X_OK: usize = 1;
const W_OK: usize = 2;
const R_OK: usize = 4;

const O_ACCMODE: usize = 0o00000003;
const O_WRONLY: usize = 0o00000001;
const O_RDWR: usize = 0o00000002;
const O_CREAT: usize = 0o00000100;
const O_EXCL: usize = 0o00000200;
const O_TRUNC: usize = 0o00001000;
const O_APPEND: usize = 0o00002000;
const O_NONBLOCK: usize = 0o00004000;
const O_DIRECTORY: usize = 0o00200000;
const O_NOFOLLOW: usize = 0o00400000;
const O_NOCTTY: usize = 0o00000400;
const O_DSYNC: usize = 0o00010000;
const O_ASYNC: usize = 0o00020000;
const O_DIRECT: usize = 0o00040000;
const O_NOATIME: usize = 0o01000000;
const O_CLOEXEC: usize = 0o02000000;
const O_PATH: usize = 0o10000000;
const O_SYNC: usize = 0o4010000;

const FALLOC_FL_KEEP_SIZE: usize = 0x01;
const FALLOC_FL_PUNCH_HOLE: usize = 0x02;
const FALLOC_FL_NO_HIDE_STALE: usize = 0x04;
const FALLOC_FL_COLLAPSE_RANGE: usize = 0x08;
const FALLOC_FL_ZERO_RANGE: usize = 0x10;
const FALLOC_FL_INSERT_RANGE: usize = 0x20;
const FALLOC_FL_UNSHARE_RANGE: usize = 0x40;
const FALLOC_FL_SUPPORTED: usize = FALLOC_FL_KEEP_SIZE
    | FALLOC_FL_PUNCH_HOLE
    | FALLOC_FL_NO_HIDE_STALE
    | FALLOC_FL_COLLAPSE_RANGE
    | FALLOC_FL_ZERO_RANGE
    | FALLOC_FL_INSERT_RANGE
    | FALLOC_FL_UNSHARE_RANGE;

const MS_RDONLY: usize = 1 << 0;
const MS_NOSUID: usize = 1 << 1;
const MS_NODEV: usize = 1 << 2;
const MS_NOEXEC: usize = 1 << 3;
const MS_SYNCHRONOUS: usize = 1 << 4;
const MS_REMOUNT: usize = 1 << 5;
const MS_BIND: usize = 1 << 12;
const MS_MOVE: usize = 1 << 13;
const MS_REC: usize = 1 << 14;
const MS_UNBINDABLE: usize = 1 << 17;
const MS_PRIVATE: usize = 1 << 18;
const MS_SLAVE: usize = 1 << 19;
const MS_SHARED: usize = 1 << 20;
const MS_SILENT: usize = 1 << 15;
const MS_NOATIME: usize = 1 << 10;
const MS_NODIRATIME: usize = 1 << 11;
const MS_RELATIME: usize = 1 << 21;
const MS_STRICTATIME: usize = 1 << 24;
const MS_LAZYTIME: usize = 1 << 25;
const MS_NOREMOTELOCK: usize = 1 << 27;
const MS_NOSEC: usize = 1 << 28;
const MS_BORN: usize = 1 << 29;
const MS_ACTIVE: usize = 1 << 30;

// mount_setattr(2) / fsmount(2) 的 MOUNT_ATTR_* 位（Linux uapi/linux/mount.h）。
const MOUNT_ATTR_RDONLY: usize = 0x0000_0001;
const MOUNT_ATTR_NOSUID: usize = 0x0000_0002;
const MOUNT_ATTR_NODEV: usize = 0x0000_0004;
const MOUNT_ATTR_NOEXEC: usize = 0x0000_0008;
const MOUNT_ATTR_NOATIME: usize = 0x0000_0010;
const MOUNT_ATTR_STRICTATIME: usize = 0x0000_0020;
const MOUNT_ATTR_NODIRATIME: usize = 0x0000_0080;
const MOUNT_ATTR_IDMAP: usize = 0x0010_0000;
const MOUNT_ATTR_NOSYMFOLLOW: usize = 0x0020_0000;
// 本内核可映射到 VFS MountFlags 的挂载属性位；其余返回 EOPNOTSUPP。
const MOUNT_ATTR_SUPPORTED: usize = MOUNT_ATTR_RDONLY
    | MOUNT_ATTR_NOSUID
    | MOUNT_ATTR_NODEV
    | MOUNT_ATTR_NOEXEC
    | MOUNT_ATTR_NOATIME
    | MOUNT_ATTR_NODIRATIME;

const AT_RECURSIVE: usize = 0x8000;

const OPEN_HOW_SIZE: usize = 24;
const OPEN_HOW_MAX_SIZE: usize = 4096;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_IN_ROOT: u64 = 0x10;
const RESOLVE_CACHED: u64 = 0x20;

const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_GETLK: usize = 5;
const F_SETLK: usize = 6;
const F_SETLKW: usize = 7;
const F_SETOWN: usize = 8;
const F_GETOWN: usize = 9;
const F_SETSIG: usize = 10;
const F_GETSIG: usize = 11;
const F_GETLK64: usize = 12;
const F_SETLK64: usize = 13;
const F_SETLKW64: usize = 14;
const F_SETOWN_EX: usize = 15;
const F_GETOWN_EX: usize = 16;
const F_OFD_GETLK: usize = 36;
const F_OFD_SETLK: usize = 37;
const F_OFD_SETLKW: usize = 38;
const F_DUPFD_CLOEXEC: usize = 1030;
const F_SETLEASE: usize = 1024;
const F_GETLEASE: usize = 1025;
const F_ADD_SEALS: usize = 1033;
const F_GET_SEALS: usize = 1034;
const FD_CLOEXEC: usize = 1;
const FIONBIO: usize = 0x5421;
const FIOASYNC: usize = 0x5452;
const FIOSETOWN: usize = 0x8901;
const SIOCSPGRP: usize = 0x8902;
const FIOGETOWN: usize = 0x8903;
const SIOCGPGRP: usize = 0x8904;
const FIONREAD: usize = 0x541b;
const FIOQSIZE: usize = 0x5460;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
const F_OWNER_TID: i32 = 0;
const F_OWNER_PID: i32 = 1;
const F_OWNER_PGRP: i32 = 2;

const MFD_CLOEXEC: usize = 0x0001;
const MFD_ALLOW_SEALING: usize = 0x0002;
const MFD_HUGETLB: usize = 0x0004;
const MFD_NOEXEC_SEAL: usize = 0x0008;
const MFD_EXEC: usize = 0x0010;
const MFD_UNSUPPORTED: usize = MFD_HUGETLB;

const TFD_TIMER_ABSTIME: usize = 1;
const TFD_TIMER_CANCEL_ON_SET: usize = 2;
const TFD_TIMER_SUPPORTED_FLAGS: usize = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;
const TFD_CREATE_SUPPORTED_FLAGS: usize = O_CLOEXEC | O_NONBLOCK;

const SFD_SUPPORTED_FLAGS: usize = O_CLOEXEC | O_NONBLOCK;

const RWF_HIPRI: usize = 0x00000001;
const RWF_DSYNC: usize = 0x00000002;
const RWF_SYNC: usize = 0x00000004;
const RWF_NOWAIT: usize = 0x00000008;
const RWF_APPEND: usize = 0x00000010;
const RWF_NOAPPEND: usize = 0x00000020;
const RWF_SUPPORTED: usize =
    RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND | RWF_NOAPPEND;

const SPLICE_F_MOVE: usize = 0x01;
const SPLICE_F_NONBLOCK: usize = 0x02;
const SPLICE_F_MORE: usize = 0x04;
const SPLICE_F_GIFT: usize = 0x08;
const SPLICE_F_SUPPORTED: usize = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;

const STATX_TYPE: u32 = 0x0001;
const STATX_MODE: u32 = 0x0002;
const STATX_NLINK: u32 = 0x0004;
const STATX_UID: u32 = 0x0008;
const STATX_GID: u32 = 0x0010;
const STATX_ATIME: u32 = 0x0020;
const STATX_MTIME: u32 = 0x0040;
const STATX_CTIME: u32 = 0x0080;
const STATX_INO: u32 = 0x0100;
const STATX_SIZE: u32 = 0x0200;
const STATX_BLOCKS: u32 = 0x0400;
const STATX_BASIC_STATS: u32 = STATX_TYPE
    | STATX_MODE
    | STATX_NLINK
    | STATX_UID
    | STATX_GID
    | STATX_ATIME
    | STATX_MTIME
    | STATX_CTIME
    | STATX_INO
    | STATX_SIZE
    | STATX_BLOCKS;
const STATX_BTIME: u32 = 0x0800;
const STATX_MNT_ID: u32 = 0x1000;
const STATX_DIOALIGN: u32 = 0x2000;
// 高于所有合法 STATX_* 位的保留位（Linux 返回 EINVAL）。
const STATX__RESERVED: u32 = 0xffff_0000;

const MSGHDR_SIZE_64: usize = 56;
const MMSGHDR_SIZE_64: usize = 64;
// Linux 只在 x86_64 上把 epoll_event 压缩为 12 字节；LoongArch64、
// RISC-V64 等 64 位架构按 8 字节对齐 data，结构体大小为 16 字节。
const EPOLL_EVENT_DATA_OFFSET_64: usize = if cfg!(target_arch = "x86_64") { 4 } else { 8 };
const EPOLL_EVENT_SIZE_64: usize = EPOLL_EVENT_DATA_OFFSET_64 + 8;
const PSELECT6_SIGSET_ARG_SIZE_64: usize = 16;

pub(super) fn sys_write(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let file = file_for_fd(fd)?;
    if len != 0 {
        ensure_network_execution_scope_for_file(ctx, &file);
    }
    write_from_user(&file, buf, len)
}

pub(super) fn sys_read(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let file = file_for_fd(fd)?;
    if len != 0 {
        ensure_network_execution_scope_for_file(ctx, &file);
    }
    read_to_user(&file, buf, len, None, false)
}

pub(super) fn sys_pread64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let offset = nonnegative_i64_arg(ctx.args[3])?;
    let file = file_for_fd(fd)?;
    read_to_user(&file, buf, len, Some(offset), false)
}

pub(super) fn sys_pwrite64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let offset = nonnegative_i64_arg(ctx.args[3])?;
    let file = file_for_fd(fd)?;
    write_from_user_at(&file, buf, len, Some(offset), false)
}

pub(super) fn sys_writev(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    write_iovecs(ctx, &file, iov, iovcnt, None, false)
}

pub(super) fn sys_readv(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    read_iovecs(ctx, &file, iov, iovcnt, None, false)
}

pub(super) fn sys_close(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    operation::close_for_owner(&fdt, fd, record_lock_owner_pid(ctx)).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_lseek(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let offset = ctx.args[1] as isize as i64;
    let whence = ctx.args[2];
    let file = file_for_fd(fd)?;
    if !file.is_seekable() {
        return Err(Errno::ESPIPE);
    }
    let from = match whence {
        0 => {
            if offset < 0 {
                return Err(Errno::EINVAL);
            }
            SeekFrom::Start(offset as u64)
        }
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        3 => {
            if offset < 0 {
                return Err(Errno::EINVAL);
            }
            SeekFrom::Data(offset as u64)
        }
        4 => {
            if offset < 0 {
                return Err(Errno::EINVAL);
            }
            SeekFrom::Hole(offset as u64)
        }
        _ => return Err(Errno::EINVAL),
    };
    file.seek(from)
        .map(|v| v as usize)
        .map_err(|e| e.to_errno())
}

pub(super) fn sys_openat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let flags = decode_open_options(ctx.args[2])?;
    let mode = FileMode::new((ctx.args[3] & 0o7777) as u16);
    let fd =
        operation::openat(&vfs_ctx, &fdt, &dirfd, &path, flags, mode).map_err(|e| e.to_errno())?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_faccessat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    faccessat_common(ctx, false)
}

pub(super) fn sys_faccessat2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    faccessat_common(ctx, true)
}

pub(super) fn sys_fstat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let st = operation::fstat(&fdt, fd).map_err(|e| e.to_errno())?;
    write_linux_stat(ctx.args[1], &st)?;
    Ok(0)
}

pub(super) fn sys_newfstatat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let raw_dirfd = ctx.args[0];
    let path = copy_path_from_user(ctx.args[1])?;
    let stat_user = ctx.args[2];
    let flags = ctx.args[3];

    let st = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        if raw_dirfd as i32 == AT_FDCWD {
            let r = vfs::path::lookup(&vfs_ctx, &Dirfd::Cwd, ".", LookupFlags::default())
                .map_err(|e| e.to_errno())?;
            let inode = r.dentry.inode().ok_or(Errno::ENOENT)?;
            inode.stat().map_err(|e| e.to_errno())?
        } else {
            let fd = fd_arg(raw_dirfd)?;
            operation::fstat(&fdt, fd).map_err(|e| e.to_errno())?
        }
    } else {
        let dirfd = dirfd_arg_for_path(raw_dirfd, &path, &fdt)?;
        operation::fstatat(&vfs_ctx, &dirfd, &path, (flags & AT_SYMLINK_NOFOLLOW) != 0)
            .map_err(|e| e.to_errno())?
    };
    write_linux_stat(stat_user, &st)?;
    Ok(0)
}

pub(super) fn sys_statx(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let raw_dirfd = ctx.args[0];
    let path = copy_path_from_user(ctx.args[1])?;
    let flags = ctx.args[2];
    let requested_mask = ctx.args[3] as u32;
    let statx_user = ctx.args[4];

    const ALLOWED_FLAGS: usize = AT_SYMLINK_NOFOLLOW
        | AT_NO_AUTOMOUNT
        | AT_EMPTY_PATH
        | AT_STATX_FORCE_SYNC
        | AT_STATX_DONT_SYNC;
    if (flags & !ALLOWED_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    if (requested_mask & STATX__RESERVED) != 0 {
        return Err(Errno::EINVAL);
    }

    // 解析得到 FileStat + 所属 superblock（用于 mnt_id 与 DIO 对齐）。
    let (st, sb) = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        if raw_dirfd as i32 == AT_FDCWD {
            let r = vfs::path::lookup(&vfs_ctx, &Dirfd::Cwd, ".", LookupFlags::default())
                .map_err(|e| e.to_errno())?;
            let inode = r.dentry.inode().ok_or(Errno::ENOENT)?;
            (inode.stat().map_err(|e| e.to_errno())?, inode.superblock())
        } else {
            let fd = fd_arg(raw_dirfd)?;
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            (
                file.stat().map_err(|e| e.to_errno())?,
                file.inode().superblock(),
            )
        }
    } else {
        let dirfd = dirfd_arg_for_path(raw_dirfd, &path, &fdt)?;
        let r = vfs::path::lookup(
            &vfs_ctx,
            &dirfd,
            &path,
            if (flags & AT_SYMLINK_NOFOLLOW) != 0 {
                LookupFlags::NO_FOLLOW
            } else {
                LookupFlags::default()
            },
        )
        .map_err(|e| e.to_errno())?;
        let inode = r.dentry.inode().ok_or(Errno::ENOENT)?;
        (inode.stat().map_err(|e| e.to_errno())?, inode.superblock())
    };

    // mnt_id 以 superblock 实例 ID 近似（本 VFS 无 per-mount id 注册表）；
    // DIO 对齐取自文件系统块大小，仅当后端声明支持直接 I/O 时声明 STATX_DIOALIGN。
    let mnt_id = sb.as_ref().map(|s| s.fs_id.raw()).unwrap_or(0);
    let dio_align = sb
        .as_ref()
        .filter(|s| s.ops.supports_direct_io())
        .map(|s| (s.block_size.max(512), s.block_size.max(512)))
        .unwrap_or((0, 0));

    write_linux_statx(
        statx_user,
        &st,
        mnt_id,
        dio_align.0,
        dio_align.1,
        requested_mask,
    )?;
    Ok(0)
}

pub(super) fn sys_readlinkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let dirfd = dirfd_arg_for_path(ctx.args[0], &path, &fdt)?;
    let buf = ctx.args[2];
    let size = ctx.args[3];

    if let Some(target) = synthetic_readlink_target(ctx, &path)? {
        return copy_readlink_target(buf, size, &target);
    }

    let target = operation::readlinkat(&vfs_ctx, &dirfd, &path).map_err(|e| e.to_errno())?;
    copy_readlink_target(buf, size, &target)
}

pub(super) fn sys_getcwd(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::ENOENT)?;
    let user = ctx.args[0];
    let size = ctx.args[1];
    if size == 0 {
        return Err(Errno::EINVAL);
    }
    if !vfs_ctx.cwd().is_positive() {
        return Err(Errno::ENOENT);
    }
    let mut path =
        namespace_path(&vfs_ctx, &vfs_ctx.cwd(), &vfs_ctx.cwd_mount()).ok_or(Errno::ENOENT)?;
    if path.is_empty() {
        path.push('/');
    }
    let needed = path.len().checked_add(1).ok_or(Errno::ERANGE)?;
    if size < needed {
        return Err(Errno::ERANGE);
    }
    copy_to_user(user, path.as_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(user + path.len(), &[0]).map_err(|e| e.as_errno())?;
    // Linux getcwd(2) syscall 返回包含结尾 NUL 的字节数，而 libc 的 getcwd()
    // 再把它转换为用户缓冲区指针。返回地址会让 glibc 误判结果并进入基于
    // 文件描述符的兼容回退路径。
    Ok(needed)
}

pub(super) fn sys_dup(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let new_fd = fdt.dup_fd(fd).map_err(|e| e.to_errno())?;
    Ok(new_fd.as_raw() as usize)
}

pub(super) fn sys_dup3(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let old_fd = fd_arg(ctx.args[0])?;
    let new_fd = fd_arg(ctx.args[1])?;
    let flags = ctx.args[2];
    if old_fd == new_fd || (flags & !O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd_flags = if (flags & O_CLOEXEC) != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let out = fdt
        .dup2_fd_for_owner(old_fd, new_fd, fd_flags, record_lock_owner_pid(ctx))
        .map_err(|e| e.to_errno())?;
    Ok(out.as_raw() as usize)
}

pub(super) fn sys_fcntl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let cmd = ctx.args[1];
    let arg = ctx.args[2];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    match cmd {
        F_DUPFD => {
            let out = fdt
                .dup_fd_from(fd, arg as u32, FdFlags::default())
                .map_err(|e| e.to_errno())?;
            Ok(out.as_raw() as usize)
        }
        F_DUPFD_CLOEXEC => {
            let out = fdt
                .dup_fd_from(fd, arg as u32, FdFlags::CLOEXEC)
                .map_err(|e| e.to_errno())?;
            Ok(out.as_raw() as usize)
        }
        F_GETFD => fdt
            .fd_flags(fd)
            .map(|f| f.raw() as usize)
            .map_err(|e| e.to_errno()),
        F_SETFD => {
            let flags = if (arg & FD_CLOEXEC) != 0 {
                FdFlags::CLOEXEC
            } else {
                FdFlags::default()
            };
            fdt.set_fd_flags(fd, flags).map_err(|e| e.to_errno())?;
            Ok(0)
        }
        F_GETFL => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            Ok(open_options_to_linux_flags(&file.flags()))
        }
        F_SETFL => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let current = file.flags();
            file.set_status_flags(
                (arg & O_APPEND) != 0,
                (arg & O_NONBLOCK) != 0,
                current.sync, // O_SYNC 不可经 F_SETFL 修改，保留 open 时值
                (arg & O_DIRECT) != 0,
                (arg & O_ASYNC) != 0,
            );
            Ok(0)
        }
        F_SETOWN => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let owner = arg as isize as i32;
            if owner < 0 {
                file.set_owner(F_OWNER_PGRP, owner.wrapping_neg());
            } else {
                file.set_owner(F_OWNER_PID, owner);
            }
            Ok(0)
        }
        F_GETOWN => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let (owner_type, owner_pid) = file.owner();
            let owner = if owner_type == F_OWNER_PGRP {
                owner_pid.wrapping_neg()
            } else {
                owner_pid
            };
            Ok(owner as isize as usize)
        }
        F_SETSIG => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            file.set_owner_sig(arg as i32);
            Ok(0)
        }
        F_GETSIG => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            Ok(file.owner_sig() as isize as usize)
        }
        F_SETOWN_EX => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let (owner_type, owner_pid) = read_f_owner_ex(arg)?;
            validate_f_owner_type(owner_type)?;
            file.set_owner(owner_type, owner_pid);
            Ok(0)
        }
        F_GETOWN_EX => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let (owner_type, owner_pid) = file.owner();
            validate_f_owner_type(owner_type)?;
            write_f_owner_ex(arg, owner_type, owner_pid)?;
            Ok(0)
        }
        F_GETLK | F_GETLK64 => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            fcntl_getlk(ctx, &file, arg, false)
        }
        F_OFD_GETLK => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            fcntl_getlk(ctx, &file, arg, true)
        }
        F_SETLK | F_SETLK64 => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            fcntl_setlk(ctx, &file, arg, false, false)
        }
        F_OFD_SETLK => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            fcntl_setlk(ctx, &file, arg, false, true)
        }
        F_SETLKW | F_SETLKW64 => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            fcntl_setlk(ctx, &file, arg, true, false)
        }
        F_OFD_SETLKW => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            fcntl_setlk(ctx, &file, arg, true, true)
        }
        F_SETLEASE => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let lock_type = arg as i32;
            if !file.is_seekable() {
                return Err(Errno::EINVAL);
            }
            let owner_pid = record_lock_owner_pid(ctx);
            vfs::lease::setlease(&file, owner_pid, linux_lease_type(lock_type)?)?;
            Ok(0)
        }
        F_GETLEASE => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            Ok(
                linux_lease_type_raw(vfs::lease::getlease(&file, record_lock_owner_pid(ctx)))
                    as usize,
            )
        }
        F_ADD_SEALS => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let memfd = file
                .downcast_ops::<vfs::memfd::MemfdFileOps>()
                .ok_or(Errno::EINVAL)?;
            memfd.add_seals(arg as u32)?;
            Ok(0)
        }
        F_GET_SEALS => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let memfd = file
                .downcast_ops::<vfs::memfd::MemfdFileOps>()
                .ok_or(Errno::EINVAL)?;
            Ok(memfd.seals() as usize)
        }
        vfs::pipe::F_SETPIPE_SZ | vfs::pipe::F_GETPIPE_SZ => {
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
            file.fcntl(cmd, arg, vfs_ctx.cred().as_ref())
        }
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_ioctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let file = file_for_fd(fd_arg(ctx.args[0])?)?;
    let cmd = IoctlCmd::new(ctx.args[1] & u32::MAX as usize);
    if cmd.raw() == FIONBIO {
        let enabled = read_user_i32(ctx.args[2])? != 0;
        let flags = file.flags();
        file.set_status_flags(
            flags.append,
            enabled,
            flags.sync,
            flags.direct,
            flags.async_,
        );
        return Ok(0);
    }
    if cmd.raw() == FIOASYNC {
        let on = read_user_i32(ctx.args[2])? != 0;
        file.set_fasync(on);
        return Ok(0);
    }
    if cmd.raw() == FIOSETOWN {
        let owner = read_user_i32(ctx.args[2])?;
        if owner < 0 {
            file.set_owner(F_OWNER_PGRP, owner.wrapping_neg());
        } else {
            file.set_owner(F_OWNER_PID, owner);
        }
        return Ok(0);
    }
    if cmd.raw() == SIOCSPGRP {
        let pgid = read_user_i32(ctx.args[2])?;
        if pgid < 0 {
            return Err(Errno::EINVAL);
        }
        file.set_owner(F_OWNER_PGRP, pgid);
        return Ok(0);
    }
    if cmd.raw() == FIOGETOWN {
        let (t, pid) = file.owner();
        let owner = if t == F_OWNER_PGRP {
            pid.wrapping_neg()
        } else {
            pid
        };
        return Ok(owner as isize as usize);
    }
    if cmd.raw() == SIOCGPGRP {
        let (_, pid) = file.owner();
        return Ok(pid as usize);
    }
    if cmd.raw() == FIONREAD {
        if let Some(pipe) = vfs::pipe::pipe_of(&file) {
            let bytes = pipe.available_len() as u32;
            copy_to_user(ctx.args[2], &bytes.to_ne_bytes()).map_err(|e| e.as_errno())?;
            return Ok(0);
        }
    }
    if cmd.raw() == FIOQSIZE {
        if file.inode().kind() == FileType::Regular {
            let size = file.inode().size() as i64;
            copy_to_user(ctx.args[2], &size.to_ne_bytes()).map_err(|e| e.as_errno())?;
            return Ok(0);
        }
    }
    if cmd.raw() == general::dev::tty::TIOCGPTPEER {
        return sys_tiocgptpeer(ctx, &file);
    }
    file.ioctl(cmd, ctx.args[2])
}

/// TIOCGPTPEER:返回指向 pty master 的 slave 的新 fd(Linux 语义)。
fn sys_tiocgptpeer(ctx: &mut SyscallContext<'_>, file: &vfs::file::File) -> Result<usize, Errno> {
    let Some(master) = file.downcast_ops::<general::dev::tty::PtyMasterFileOps>() else {
        return Err(Errno::ENOTTY);
    };
    let flags = ctx.args[2];
    let pair = master.pair().clone();
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let opts = vfs::file::OpenOptions {
        access: vfs::file::AccessMode::ReadWrite,
        nonblock: (flags & O_NONBLOCK) != 0,
        cloexec: (flags & O_CLOEXEC) != 0,
        ..Default::default()
    };
    let slave = general::dev::tty::open_slave_file(&pair, opts, vfs_ctx.cred().clone())
        .map_err(|e| e.to_errno())?;
    let fd_flags = if opts.cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let fd = fdt.alloc_fd(slave, fd_flags).map_err(|e| e.to_errno())?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_pipe2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fds_user = ctx.args[0];
    let flags = ctx.args[1];
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;

    if (flags & !(O_NONBLOCK | O_CLOEXEC | O_DIRECT)) != 0 {
        return Err(Errno::EINVAL);
    }

    let nonblock = (flags & O_NONBLOCK) != 0;
    let cloexec = (flags & O_CLOEXEC) != 0;

    let (read_end, write_end) =
        vfs::pipe::new_pipe(vfs_ctx.cred(), nonblock).map_err(|e| e.to_errno())?;

    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };

    let read_fd = fdt.alloc_fd(read_end, fd_flags).map_err(|e| e.to_errno())?;
    let write_fd = match fdt.alloc_fd(write_end, fd_flags) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = fdt.close_fd(read_fd);
            return Err(e.to_errno());
        }
    };

    let fds: [i32; 2] = [read_fd.as_raw() as i32, write_fd.as_raw() as i32];
    let fds_bytes: &[u8] = unsafe {
        core::slice::from_raw_parts(fds.as_ptr() as *const u8, core::mem::size_of::<[i32; 2]>())
    };
    if let Err(err) = copy_to_user(fds_user, fds_bytes) {
        let _ = fdt.close_fd(write_fd);
        let _ = fdt.close_fd(read_fd);
        return Err(err.as_errno());
    }

    Ok(0)
}

pub(super) fn sys_mkdirat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let mode = FileMode::new((ctx.args[2] & 0o7777) as u16);
    operation::mkdirat(&vfs_ctx, &dirfd, &path, mode).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_unlinkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const AT_REMOVEDIR: usize = 0x200;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let flags = ctx.args[2];
    if (flags & !AT_REMOVEDIR) != 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & AT_REMOVEDIR) != 0 {
        operation::rmdir(&vfs_ctx, &dirfd, &path).map_err(|e| e.to_errno())?;
    } else {
        operation::unlink(&vfs_ctx, &dirfd, &path).map_err(|e| e.to_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_renameat2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    renameat_common(
        ctx.args[0],
        ctx.args[1],
        ctx.args[2],
        ctx.args[3],
        ctx.args[4],
    )
}

const RENAME_NOREPLACE: usize = 1;
const RENAME_EXCHANGE: usize = 2;
const RENAME_WHITEOUT: usize = 4;

static RENAME_TMP_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 生成 EXCHANGE 用的临时名（落在 new_path 所在目录，避免跨目录碰撞）。
fn exchange_tmp_path(new_path: &str) -> String {
    let seq = RENAME_TMP_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let tmp_name = alloc::format!(".mygo_xchg_{}", seq);
    match new_path.rfind('/') {
        Some(idx) => alloc::format!("{}{}", &new_path[..idx + 1], tmp_name),
        None => tmp_name,
    }
}

fn renameat_common(
    old_dirfd_raw: usize,
    old_path_user: usize,
    new_dirfd_raw: usize,
    new_path_user: usize,
    flags: usize,
) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let old_dirfd = dirfd_arg(old_dirfd_raw, &fdt)?;
    let old_path = copy_path_from_user(old_path_user)?;
    let new_dirfd = dirfd_arg(new_dirfd_raw, &fdt)?;
    let new_path = copy_path_from_user(new_path_user)?;
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & RENAME_WHITEOUT != 0 {
        // RENAME_WHITEOUT 需要 overlayfs 的 whiteout 设备语义，本内核无此机制。
        return Err(Errno::EOPNOTSUPP);
    }
    if flags & RENAME_NOREPLACE != 0 && flags & RENAME_EXCHANGE != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & RENAME_NOREPLACE != 0 {
        // Linux RENAME_NOREPLACE：目标存在即 EEXIST。这里在调用 renameat 前做
        // 存在性检查（非原子，与 Linux 的原子 no-replace 存在 TOCTOU 差异，已注明）。
        match vfs::path::lookup(
            &vfs_ctx,
            &new_dirfd,
            &new_path,
            LookupFlags::NO_FOLLOW.with(LookupFlags::NO_MOUNT_LAST),
        ) {
            Ok(_) => return Err(Errno::EEXIST),
            Err(VfsError::NotFound) => {}
            Err(e) => return Err(e.to_errno()),
        }
    }
    if flags & RENAME_EXCHANGE != 0 {
        return rename_exchange(&vfs_ctx, &old_dirfd, &old_path, &new_dirfd, &new_path);
    }
    operation::renameat(&vfs_ctx, &old_dirfd, &old_path, &new_dirfd, &new_path)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

/// `RENAME_EXCHANGE`：原子交换两个路径。当前 VFS 的 `renameat` 只支持单向重命名，
/// 这里用“临时名三段搬移”实现，非原子；任一步失败尽力回滚。两路径须存在且同
/// 文件系统（Linux 语义）。
fn rename_exchange(
    vfs_ctx: &Arc<vfs::VfsContext>,
    old_dirfd: &Dirfd,
    old_path: &str,
    new_dirfd: &Dirfd,
    new_path: &str,
) -> Result<usize, Errno> {
    // 两端都必须存在，且位于同一文件系统。
    let old_inode = vfs::path::lookup(
        vfs_ctx,
        old_dirfd,
        old_path,
        LookupFlags::NO_FOLLOW.with(LookupFlags::NO_MOUNT_LAST),
    )
    .and_then(|r| r.dentry.inode().ok_or(VfsError::NotFound))
    .map_err(|e| e.to_errno())?;
    let new_inode = vfs::path::lookup(
        vfs_ctx,
        new_dirfd,
        new_path,
        LookupFlags::NO_FOLLOW.with(LookupFlags::NO_MOUNT_LAST),
    )
    .and_then(|r| r.dentry.inode().ok_or(VfsError::NotFound))
    .map_err(|e| e.to_errno())?;
    if old_inode.fs_id() != new_inode.fs_id() {
        return Err(Errno::EXDEV);
    }

    let tmp = exchange_tmp_path(new_path);
    // 1. old → tmp（tmp 落在 new 所在目录，保证同一文件系统）。
    operation::renameat(vfs_ctx, old_dirfd, old_path, new_dirfd, &tmp).map_err(|e| e.to_errno())?;
    // 2. new → old。
    if let Err(e) = operation::renameat(vfs_ctx, new_dirfd, new_path, old_dirfd, old_path) {
        // 回滚 1。
        let _ = operation::renameat(vfs_ctx, new_dirfd, &tmp, old_dirfd, old_path);
        return Err(e.to_errno());
    }
    // 3. tmp → new。
    if let Err(e) = operation::renameat(vfs_ctx, new_dirfd, &tmp, new_dirfd, new_path) {
        // 回滚 2 与 1。
        let _ = operation::renameat(vfs_ctx, old_dirfd, old_path, new_dirfd, new_path);
        let _ = operation::renameat(vfs_ctx, new_dirfd, &tmp, old_dirfd, old_path);
        return Err(e.to_errno());
    }
    Ok(0)
}

pub(super) fn sys_linkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let old_dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let old_path = copy_path_from_user(ctx.args[1])?;
    let new_dirfd = dirfd_arg(ctx.args[2], &fdt)?;
    let new_path = copy_path_from_user(ctx.args[3])?;
    let flags = ctx.args[4];
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW) != 0;
    if (flags & !AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    operation::linkat(
        &vfs_ctx, &old_dirfd, &old_path, &new_dirfd, &new_path, no_follow,
    )
    .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_symlinkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let target = copy_path_from_user(ctx.args[0])?;
    let dirfd = dirfd_arg(ctx.args[1], &fdt)?;
    let link_path = copy_path_from_user(ctx.args[2])?;
    operation::symlinkat(&vfs_ctx, &target, &dirfd, &link_path).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_mknodat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let mode = ctx.args[2];
    let dev = ctx.args[3] as u64;
    let kind = match mode & 0o170000 {
        0o010000 => FileType::Fifo,
        0o020000 => FileType::CharDevice,
        0o040000 => FileType::Directory,
        0o060000 => FileType::BlockDevice,
        0o100000 => FileType::Regular,
        0o120000 => FileType::Symlink,
        0o140000 => FileType::Socket,
        _ => return Err(Errno::EINVAL),
    };
    let file_mode = FileMode::new((mode & 0o7777) as u16);
    let dev_id = decode_dev_t(dev);
    operation::mknodat(&vfs_ctx, &dirfd, &path, kind, file_mode, dev_id)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fchmod(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let mode = FileMode::new((ctx.args[1] & 0o7777) as u16);
    operation::fchmod(&vfs_ctx, &fdt, fd, mode).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fchmodat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let mode = FileMode::new((ctx.args[2] & 0o7777) as u16);
    operation::fchmodat(&vfs_ctx, &dirfd, &path, mode, false).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fchownat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[4];
    if (flags & !AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let (uid, gid) = decode_optional_owner(ctx.args[2] as u32, ctx.args[3] as u32);
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW) != 0;
    operation::fchownat(&vfs_ctx, &dirfd, &path, uid, gid, no_follow).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fchown(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let (uid, gid) = decode_optional_owner(ctx.args[1] as u32, ctx.args[2] as u32);
    operation::fchown(&vfs_ctx, &fdt, fd, uid, gid).map_err(|e| e.to_errno())?;
    Ok(0)
}

fn decode_optional_owner(uid_raw: u32, gid_raw: u32) -> (Option<Uid>, Option<Gid>) {
    let uid = if uid_raw == u32::MAX {
        None
    } else {
        Some(Uid(uid_raw))
    };
    let gid = if gid_raw == u32::MAX {
        None
    } else {
        Some(Gid(gid_raw))
    };
    (uid, gid)
}

pub(super) fn sys_utimensat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path_user = ctx.args[1];
    let times_user = ctx.args[2];
    let flags = ctx.args[3];
    if (flags & !AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    let (atime, mtime) = decode_utimens_times(times_user)?;
    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }

    if path_user == 0 {
        if flags != 0 {
            return Err(Errno::EINVAL);
        }
        let fd = fd_arg(ctx.args[0])?;
        operation::futimens(&vfs_ctx, &fdt, fd, atime, mtime).map_err(|e| e.to_errno())?;
        return Ok(0);
    }

    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(path_user)?;
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW) != 0;
    operation::utimensat(&vfs_ctx, &dirfd, &path, atime, mtime, no_follow)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

fn decode_utimens_times(times_user: usize) -> Result<(Option<Timespec>, Option<Timespec>), Errno> {
    if times_user == 0 {
        let now = realtime_timespec();
        return Ok((Some(now), Some(now)));
    }

    const UTIME_NOW_NSEC: i64 = 0x3fff_ffff;
    const UTIME_OMIT_NSEC: i64 = 0x3fff_fffe;
    const NSEC_PER_SEC: i64 = 1_000_000_000;

    let mut raw = [0u8; 32];
    copy_from_user(times_user, &mut raw).map_err(|e| e.as_errno())?;
    let now = realtime_timespec();
    let read_ts = |off: usize| -> Result<Option<Timespec>, Errno> {
        let sec = i64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
        let nsec = i64::from_le_bytes(raw[off + 8..off + 16].try_into().unwrap());
        if nsec == UTIME_NOW_NSEC {
            return Ok(Some(now));
        }
        if nsec == UTIME_OMIT_NSEC {
            return Ok(None);
        }
        if !(0..NSEC_PER_SEC).contains(&nsec) {
            return Err(Errno::EINVAL);
        }
        Ok(Some(Timespec {
            secs: sec,
            nsecs: nsec as u32,
        }))
    };
    Ok((read_ts(0)?, read_ts(16)?))
}

fn realtime_timespec() -> Timespec {
    let ns = crate::vdso::realtime_ns();
    Timespec {
        secs: (ns / 1_000_000_000) as i64,
        nsecs: (ns % 1_000_000_000) as u32,
    }
}

pub(super) fn sys_truncate(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_path_from_user(ctx.args[0])?;
    let size = nonnegative_i64_arg(ctx.args[1])?;
    let dirfd = Dirfd::Cwd;
    operation::truncate(&vfs_ctx, &dirfd, &path, size).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_ftruncate(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let size = nonnegative_i64_arg(ctx.args[1])?;
    let file = file_for_fd(fd)?;
    if file.inode().kind() == FileType::Directory {
        return Err(Errno::EISDIR);
    }
    if !file.flags().writable() {
        return Err(Errno::EINVAL);
    }
    file.truncate(size).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fsync(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let file = file_for_fd(fd)?;
    file.sync().map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fdatasync(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let file = file_for_fd(fd)?;
    file.datasync().map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_getdents64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let dirent = ctx.args[1];
    let count = ctx.args[2];
    let file = file_for_fd(fd)?;
    if !file.flags().readable() {
        return Err(Errno::EBADF);
    }
    if file.inode().kind() != FileType::Directory {
        return Err(Errno::ENOTDIR);
    }
    if file.inode().nlink() == 0 {
        return Err(Errno::ENOENT);
    }
    if count < 24 {
        return Err(Errno::EINVAL);
    }

    let mut buf_pos = 0usize;
    let mut copy_error = None;
    file.readdir(&mut |entry| {
        if copy_error.is_some() {
            return ControlFlow::Break(());
        }
        if buf_pos >= count {
            return ControlFlow::Break(());
        }
        let name_bytes = entry.name.as_str().as_bytes();
        let name_len = name_bytes.len().min(255);
        let reclen = match align_up(19 + name_len + 1, 8) {
            Some(r) => r,
            None => return ControlFlow::Break(()),
        };
        if buf_pos + reclen > count {
            return ControlFlow::Break(());
        }
        let mut raw = vec![0u8; reclen];
        put_u64(&mut raw, 0, entry.ino);
        put_u64(&mut raw, 8, (buf_pos + reclen) as u64);
        put_u16(&mut raw, 16, reclen as u16);
        raw[18] = file_type_to_d_type(entry.kind);
        raw[19..19 + name_len].copy_from_slice(&name_bytes[..name_len]);
        raw[19 + name_len] = 0;
        if let Err(err) = copy_to_user(dirent + buf_pos, &raw) {
            copy_error = Some(err.as_errno());
            return ControlFlow::Break(());
        }
        buf_pos += reclen;
        ControlFlow::Continue(())
    })
    .map_err(|e| e.to_errno())?;

    if let Some(err) = copy_error
        && buf_pos == 0
    {
        return Err(err);
    }

    Ok(buf_pos)
}

pub(super) fn sys_statfs(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_path_from_user(ctx.args[0])?;
    let buf = ctx.args[1];
    let dirfd = Dirfd::Cwd;
    let result = vfs::path::lookup(&vfs_ctx, &dirfd, &path, vfs::path::LookupFlags(0))
        .map_err(|e| e.to_errno())?;
    let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
    let sb = inode.superblock().ok_or(Errno::ENOENT)?;
    let st = sb.statfs().map_err(|e| e.to_errno())?;
    write_linux_statfs(buf, &st)?;
    Ok(0)
}

pub(super) fn sys_fstatfs(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let file = file_for_fd(fd)?;
    let sb = file.inode().superblock().ok_or(Errno::ENOENT)?;
    let st = sb.statfs().map_err(|e| e.to_errno())?;
    write_linux_statfs(buf, &st)?;
    Ok(0)
}

pub(super) fn sys_chdir(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    use vfs::cred::{Gid, Uid};
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_path_from_user(ctx.args[0])?;
    let dirfd = Dirfd::Cwd;
    let result = vfs::path::lookup(&vfs_ctx, &dirfd, &path, vfs::path::LookupFlags::DIRECTORY)
        .map_err(|e| e.to_errno())?;
    let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
    let st = inode.stat().map_err(|e| e.to_errno())?;
    if !vfs_ctx.cred().can_exec(
        Uid(st.uid),
        Gid(st.gid),
        FileMode::new(st.mode as u16),
        true,
    ) {
        return Err(Errno::EACCES);
    }
    vfs_ctx
        .set_cwd(result.dentry, result.mount)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fchdir(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    use vfs::cred::{Gid, Uid};
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let file = file_for_fd(fd)?;
    let dirfd = Dirfd::Fd(file);
    let result = vfs::path::lookup(&vfs_ctx, &dirfd, ".", vfs::path::LookupFlags::DIRECTORY)
        .map_err(|e| e.to_errno())?;
    let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
    let st = inode.stat().map_err(|e| e.to_errno())?;
    if !vfs_ctx.cred().can_exec(
        Uid(st.uid),
        Gid(st.gid),
        FileMode::new(st.mode as u16),
        true,
    ) {
        return Err(Errno::EACCES);
    }
    vfs_ctx
        .set_cwd(result.dentry, result.mount)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_chroot(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let path = copy_path_from_user(ctx.args[0])?;
    operation::chroot(&vfs_ctx, &Dirfd::Cwd, &path).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_mount(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let source = copy_optional_path_from_user(ctx.args[0])?;
    let target = copy_path_from_user(ctx.args[1])?;
    let fs_type = copy_optional_cstr_from_user(ctx.args[2], 64)?;
    let mount_flags_raw = ctx.args[3];
    // 只把当前内核真正实现的访问约束转成 VFS MountFlags，其余 Linux 内核内部位
    // 作为兼容输入接受但不持久化，避免 LTP 的通用 mount helper 在参数校验阶段失败。
    const KNOWN_MOUNT_FLAGS: usize = MS_RDONLY
        | MS_NOSUID
        | MS_NODEV
        | MS_NOEXEC
        | MS_SYNCHRONOUS
        | MS_REMOUNT
        | MS_NOATIME
        | MS_NODIRATIME
        | MS_BIND
        | MS_MOVE
        | MS_REC
        | MS_UNBINDABLE
        | MS_PRIVATE
        | MS_SLAVE
        | MS_SHARED
        | MS_SILENT
        | MS_RELATIME
        | MS_STRICTATIME
        | MS_LAZYTIME
        | MS_NOREMOTELOCK
        | MS_NOSEC
        | MS_BORN
        | MS_ACTIVE;
    if (mount_flags_raw & !KNOWN_MOUNT_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let data = copy_optional_cstr_from_user(ctx.args[4], 4096)?;

    let mut flags = MountFlags::RDONLY.without(MountFlags::RDONLY);
    if (mount_flags_raw & MS_RDONLY) != 0 {
        flags = flags.with(MountFlags::RDONLY);
    }
    if (mount_flags_raw & MS_NOSUID) != 0 {
        flags = flags.with(MountFlags::NOSUID);
    }
    if (mount_flags_raw & MS_NODEV) != 0 {
        flags = flags.with(MountFlags::NODEV);
    }
    if (mount_flags_raw & MS_NOEXEC) != 0 {
        flags = flags.with(MountFlags::NOEXEC);
    }
    if (mount_flags_raw & MS_SYNCHRONOUS) != 0 {
        flags = flags.with(MountFlags::SYNCHRONOUS);
    }
    if (mount_flags_raw & MS_NOATIME) != 0 {
        flags = flags.with(MountFlags::NOATIME);
    }
    if (mount_flags_raw & MS_NODIRATIME) != 0 {
        flags = flags.with(MountFlags::NODIRATIME);
    }
    if (mount_flags_raw & MS_BIND) != 0 {
        flags = flags.with(MountFlags::BIND);
    }
    if (mount_flags_raw & MS_REC) != 0 {
        flags = flags.with(MountFlags::REC);
    }

    let dev = if source.is_empty() {
        None
    } else {
        Some(source.as_str())
    };
    let dirfd = Dirfd::Cwd;
    if (mount_flags_raw & MS_REMOUNT) != 0 {
        let mountpoint = vfs::path::lookup(
            &vfs_ctx,
            &dirfd,
            &target,
            LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
        )
        .map_err(|e| e.to_errno())?;
        vfs_ctx
            .mount_ns
            .remount_at(&mountpoint.dentry, flags)
            .map_err(|e| e.to_errno())?;
        return Ok(0);
    }
    // ── bind / move / 传播类型设置（不创建新文件系统）──
    if (mount_flags_raw & MS_BIND) != 0 {
        // mount --bind：源路径的子树绑定到目标（共享文件系统实例）。
        if source.is_empty() {
            return Err(Errno::EINVAL);
        }
        let src = vfs::path::lookup(&vfs_ctx, &dirfd, &source, LookupFlags::default())
            .map_err(|e| e.to_errno())?;
        let dst = vfs::path::lookup(
            &vfs_ctx,
            &dirfd,
            &target,
            LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
        )
        .map_err(|e| e.to_errno())?;
        let src_mount = match vfs_ctx.mount_ns.lookup_mount(&src.dentry) {
            Some(m) => m,
            None => Arc::clone(&src.mount),
        };
        vfs_ctx
            .mount_ns
            .bind_at(
                Arc::clone(&dst.dentry),
                Arc::clone(&dst.mount),
                Arc::clone(&src_mount.superblock),
                Arc::clone(&src.dentry),
                flags,
                Some(&src_mount),
                true,
            )
            .map_err(|e| e.to_errno())?;
        return Ok(0);
    }
    if (mount_flags_raw & MS_MOVE) != 0 {
        // mount --move：把 source 上的挂载迁移到 target。
        if source.is_empty() {
            return Err(Errno::EINVAL);
        }
        let src = vfs::path::lookup(&vfs_ctx, &dirfd, &source, LookupFlags::NO_MOUNT_LAST)
            .map_err(|e| e.to_errno())?;
        let m = vfs_ctx
            .mount_ns
            .lookup_mount(&src.dentry)
            .ok_or(Errno::ENOENT)?;
        let dst = vfs::path::lookup(
            &vfs_ctx,
            &dirfd,
            &target,
            LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
        )
        .map_err(|e| e.to_errno())?;
        vfs_ctx
            .mount_ns
            .move_mount_at(&m, Arc::clone(&dst.dentry), Arc::clone(&dst.mount))
            .map_err(|e| e.to_errno())?;
        return Ok(0);
    }
    let rec = (mount_flags_raw & MS_REC) != 0;
    for (bit, kind) in [
        (MS_SHARED, vfs::mount::PROP_SHARED),
        (MS_PRIVATE, vfs::mount::PROP_PRIVATE),
        (MS_SLAVE, vfs::mount::PROP_SLAVE),
        (MS_UNBINDABLE, vfs::mount::PROP_UNBINDABLE),
    ] {
        if (mount_flags_raw & bit) != 0 {
            let dst = vfs::path::lookup(
                &vfs_ctx,
                &dirfd,
                &target,
                LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
            )
            .map_err(|e| e.to_errno())?;
            vfs_ctx
                .mount_ns
                .set_propagation_at(&dst.dentry, kind, rec)
                .map_err(|e| e.to_errno())?;
            return Ok(0);
        }
    }
    if fs_type.is_empty() || fs_type == "auto" {
        return mount_autodetect(&vfs_ctx, &dirfd, &target, flags, dev, &data);
    }
    operation::mount(&vfs_ctx, &dirfd, &target, &fs_type, flags, dev, &data)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

fn copy_path_from_user(user: usize) -> Result<String, Errno> {
    copy_cstr_from_user(user, PATH_MAX).map_err(path_copy_errno)
}

fn copy_optional_path_from_user(user: usize) -> Result<String, Errno> {
    if user == 0 {
        Ok(String::new())
    } else {
        copy_path_from_user(user)
    }
}

fn path_copy_errno(err: UserAccessError) -> Errno {
    match err {
        UserAccessError::TooLong => Errno::ENAMETOOLONG,
        _ => err.as_errno(),
    }
}

fn copy_optional_cstr_from_user(user: usize, max: usize) -> Result<String, Errno> {
    if user == 0 {
        Ok(String::new())
    } else {
        copy_cstr_from_user(user, max).map_err(|e| e.as_errno())
    }
}

fn mount_autodetect(
    vfs_ctx: &Arc<vfs::VfsContext>,
    dirfd: &Dirfd,
    target: &str,
    flags: MountFlags,
    dev: Option<&str>,
    data: &str,
) -> Result<usize, Errno> {
    if dev.is_none() {
        return Err(Errno::EINVAL);
    }

    let mut last = VfsError::NoDevice;
    for fs_type in ["extfs", "fatfs"] {
        match operation::mount(vfs_ctx, dirfd, target, fs_type, flags, dev, data) {
            Ok(_) => return Ok(0),
            Err(
                err @ (VfsError::InvalidArgument
                | VfsError::NotSupported
                | VfsError::NoDevice
                | VfsError::NotFound),
            ) => {
                last = err;
            }
            Err(err) => return Err(err.to_errno()),
        }
    }
    Err(last.to_errno())
}

pub(super) fn sys_pivot_root(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let new_root = copy_path_from_user(ctx.args[0])?;
    let put_old = copy_path_from_user(ctx.args[1])?;
    operation::pivot_root(&vfs_ctx, &new_root, &put_old).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_umount2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_path_from_user(ctx.args[0])?;
    let flags = ctx.args[1];
    // Linux umount2(2) 标志：MNT_FORCE / MNT_DETACH / MNT_EXPIRE / UMOUNT_NOFOLLOW。
    const MNT_FORCE: usize = 0x1;
    const MNT_DETACH: usize = 0x2;
    const MNT_EXPIRE: usize = 0x4;
    const UMOUNT_NOFOLLOW: usize = 0x8;
    const KNOWN_UMOUNT_FLAGS: usize = MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW;
    // 未知位直接拒绝，不静默忽略。
    if (flags & !KNOWN_UMOUNT_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let force = (flags & MNT_FORCE) != 0;
    // 已知但当前内核未实现的位显式返回不支持，而不是当作 0 处理。
    if (flags & (MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW)) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let dirfd = Dirfd::Cwd;
    operation::umount(&vfs_ctx, &dirfd, &path, force).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_sync(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    if let Some(vfs_ctx) = current_vfs_context() {
        // iozone/lmbench 会依赖 sync(2) 作为阶段边界。这里至少同步当前
        // mount namespace 中可见的 superblock,确保 ext4 位图和块组描述符刷盘。
        vfs_ctx.mount_ns.sync_all().map_err(|e| e.to_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_syncfs(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let file = file_for_fd(fd)?;
    // syncfs(2) 同步 fd 所属的整个文件系统，而非单个文件。
    let sb = file.inode().superblock().ok_or(Errno::ENOENT)?;
    sb.sync().map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_sendfile(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let out_fd = fd_arg(ctx.args[0])?;
    let in_fd = fd_arg(ctx.args[1])?;
    let offset_user = ctx.args[2];
    let count = ctx.args[3];

    let in_file = file_for_fd(in_fd)?;
    let out_file = file_for_fd(out_fd)?;

    let use_file_offset = offset_user == 0;
    let mut offset = if !use_file_offset {
        let mut raw = [0u8; 8];
        copy_from_user(offset_user, &mut raw).map_err(|e| e.as_errno())?;
        u64::from_le_bytes(raw)
    } else {
        0
    };

    let mut total = 0usize;
    let mut buf = [0u8; COPY_CHUNK];
    while total < count {
        let chunk = (count - total).min(buf.len());
        let n = if use_file_offset {
            ensure_network_execution_scope_for_file(ctx, &in_file);
            in_file.read(&mut buf[..chunk]).map_err(|e| e.to_errno())?
        } else {
            in_file
                .read_at(&mut buf[..chunk], offset)
                .map_err(|e| e.to_errno())?
        };
        if n == 0 {
            break;
        }
        let mut written = 0usize;
        ensure_network_execution_scope_for_file(ctx, &out_file);
        while written < n {
            let w = out_file.write(&buf[written..n]).map_err(|e| e.to_errno())?;
            if w == 0 {
                return Err(Errno::EIO);
            }
            written += w;
        }
        if !use_file_offset {
            offset += n as u64;
        }
        total += n;
    }

    if !use_file_offset {
        copy_to_user(offset_user, &offset.to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    Ok(total)
}

pub(super) fn sys_copy_file_range(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd_in = fd_arg(ctx.args[0])?;
    let off_in_user = ctx.args[1];
    let fd_out = fd_arg(ctx.args[2])?;
    let off_out_user = ctx.args[3];
    let len = ctx.args[4];
    let flags = ctx.args[5];
    // Linux 规定 copy_file_range 的 flags 必须为 0（历史 COPY_FILE_RANGE_REFLINK
    // 已移除）；非零值返回 EINVAL，不能静默当作 0 处理。
    if flags != 0 {
        return Err(Errno::EINVAL);
    }

    let in_file = file_for_fd(fd_in)?;
    let out_file = file_for_fd(fd_out)?;

    let use_in_file_offset = off_in_user == 0;
    let use_out_file_offset = off_out_user == 0;

    let mut in_off = if !use_in_file_offset {
        let mut raw = [0u8; 8];
        copy_from_user(off_in_user, &mut raw).map_err(|e| e.as_errno())?;
        u64::from_le_bytes(raw)
    } else {
        0
    };
    let mut out_off = if !use_out_file_offset {
        let mut raw = [0u8; 8];
        copy_from_user(off_out_user, &mut raw).map_err(|e| e.as_errno())?;
        u64::from_le_bytes(raw)
    } else {
        0
    };

    let mut total = 0usize;
    let mut buf = [0u8; COPY_CHUNK];
    while total < len {
        let chunk = (len - total).min(buf.len());
        let n = if use_in_file_offset {
            ensure_network_execution_scope_for_file(ctx, &in_file);
            in_file.read(&mut buf[..chunk]).map_err(|e| e.to_errno())?
        } else {
            in_file
                .read_at(&mut buf[..chunk], in_off)
                .map_err(|e| e.to_errno())?
        };
        if n == 0 {
            break;
        }
        let mut written = 0usize;
        if use_out_file_offset {
            ensure_network_execution_scope_for_file(ctx, &out_file);
        }
        while written < n {
            let w = if use_out_file_offset {
                out_file.write(&buf[written..n]).map_err(|e| e.to_errno())?
            } else {
                out_file
                    .write_at(&buf[written..n], out_off)
                    .map_err(|e| e.to_errno())?
            };
            if w == 0 {
                return Err(Errno::EIO);
            }
            written += w;
            if !use_out_file_offset {
                out_off += w as u64;
            }
        }
        if !use_in_file_offset {
            in_off += n as u64;
        }
        total += n;
    }

    if !use_in_file_offset {
        copy_to_user(off_in_user, &in_off.to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    if !use_out_file_offset {
        copy_to_user(off_out_user, &out_off.to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    Ok(total)
}

pub(super) fn sys_fallocate(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let raw_mode = ctx.args[1];
    let offset = nonnegative_i64_arg(ctx.args[2])?;
    let len = nonnegative_i64_arg(ctx.args[3])?;
    // Linux 模式校验（fs/open.c do_fallocate）：未知位 EOPNOTSUPP；
    // PUNCH_HOLE 必须搭配 KEEP_SIZE；COLLAPSE/INSERT/UNSHARE 必须独占使用。
    if raw_mode & !FALLOC_FL_SUPPORTED != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if raw_mode & FALLOC_FL_PUNCH_HOLE != 0 && raw_mode & FALLOC_FL_KEEP_SIZE == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if raw_mode & FALLOC_FL_COLLAPSE_RANGE != 0 && raw_mode & !FALLOC_FL_COLLAPSE_RANGE != 0 {
        return Err(Errno::EINVAL);
    }
    if raw_mode & FALLOC_FL_INSERT_RANGE != 0 && raw_mode & !FALLOC_FL_INSERT_RANGE != 0 {
        return Err(Errno::EINVAL);
    }
    if raw_mode & FALLOC_FL_UNSHARE_RANGE != 0 && raw_mode & !FALLOC_FL_UNSHARE_RANGE != 0 {
        return Err(Errno::EINVAL);
    }
    let mode = FallocateMode::from_bits(raw_mode as u32);
    let file = file_for_fd(fd)?;
    if !file.flags().writable() {
        return Err(Errno::EBADF);
    }
    // 具体模式是否可用由后端（extfs/memfd 等）决定；不支持的模式返回
    // VfsError::NotSupported → EOPNOTSUPP（Linux 语义）。
    file.fallocate(mode, offset, len)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_readahead(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let offset = ctx.args[1] as i64;
    let count = ctx.args[2];
    if offset < 0 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    if !file.is_seekable() {
        return Err(Errno::ESPIPE);
    }
    let _end = (offset as u64)
        .checked_add(count as u64)
        .ok_or(Errno::EINVAL)?;
    // 当前 VFS 还没有显式页缓存预读队列；这里完整执行 Linux 可见的参数
    // 校验后作为性能 hint 成功返回。
    Ok(0)
}

pub(super) fn sys_fadvise64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const POSIX_FADV_NORMAL: usize = 0;
    const POSIX_FADV_RANDOM: usize = 1;
    const POSIX_FADV_SEQUENTIAL: usize = 2;
    const POSIX_FADV_WILLNEED: usize = 3;
    const POSIX_FADV_DONTNEED: usize = 4;
    const POSIX_FADV_NOREUSE: usize = 5;

    let fd = fd_arg(ctx.args[0])?;
    let offset = ctx.args[1] as i64;
    let len = ctx.args[2] as i64;
    let advice = ctx.args[3];
    if offset < 0 || len < 0 {
        return Err(Errno::EINVAL);
    }
    if !matches!(
        advice,
        POSIX_FADV_NORMAL
            | POSIX_FADV_RANDOM
            | POSIX_FADV_SEQUENTIAL
            | POSIX_FADV_WILLNEED
            | POSIX_FADV_DONTNEED
            | POSIX_FADV_NOREUSE
    ) {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    if !file.is_seekable() {
        return Err(Errno::ESPIPE);
    }
    let _end = if len == 0 {
        None
    } else {
        Some(
            (offset as u64)
                .checked_add(len as u64)
                .ok_or(Errno::EINVAL)?,
        )
    };
    // advisory hint：当前没有 per-file readahead/writeback 策略状态，校验通过即成功。
    Ok(0)
}

pub(super) fn sys_flock(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const LOCK_SH: usize = 1;
    const LOCK_EX: usize = 2;
    const LOCK_NB: usize = 4;
    const LOCK_UN: usize = 8;

    let fd = fd_arg(ctx.args[0])?;
    let op = ctx.args[1];
    if (op & !(LOCK_SH | LOCK_EX | LOCK_NB | LOCK_UN)) != 0 {
        return Err(Errno::EINVAL);
    }
    let lock_op = op & (LOCK_SH | LOCK_EX | LOCK_UN);
    if !matches!(lock_op, LOCK_SH | LOCK_EX | LOCK_UN) {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    if lock_op == LOCK_UN {
        vfs::flock::unlock(&file);
        return Ok(0);
    }
    vfs::flock::flock(&file, lock_op == LOCK_EX, (op & LOCK_NB) != 0)?;
    Ok(0)
}

pub(super) fn sys_close_range(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let first = ctx.args[0] as u32;
    let last = ctx.args[1] as u32;
    let flags = ctx.args[2];

    const CLOSE_RANGE_UNSHARE: usize = 1 << 1;
    const CLOSE_RANGE_CLOEXEC: usize = 1 << 2;
    if (flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC)) != 0 {
        return Err(Errno::EINVAL);
    }
    if first > last {
        return Err(Errno::EINVAL);
    }

    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fdt = if (flags & CLOSE_RANGE_UNSHARE) != 0 {
        // Linux 语义要求先解除 CLONE_FILES 共享，再在新 fdtable 上执行 close/cloexec。
        // record lock 属于进程而不是 fdtable；只有新表中实际关闭的 fd
        // 才会触发对应 inode 的进程锁释放。
        let new_fdt = Arc::new(fdt.fork());
        let _ = ctx.task().ext_remove(sched::TASKEXT_VFS_FDTABLE);
        ctx.task()
            .ext_install(sched::TASKEXT_VFS_FDTABLE, new_fdt.clone());
        new_fdt
    } else {
        fdt
    };
    let cloexec = (flags & CLOSE_RANGE_CLOEXEC) != 0;
    fdt.close_range_for_owner(first, last, cloexec, record_lock_owner_pid(ctx));
    Ok(0)
}

pub(super) fn sys_eventfd2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const EFD_SEMAPHORE: usize = 1;
    const EFD_SUPPORTED: usize = EFD_SEMAPHORE | O_CLOEXEC | O_NONBLOCK;

    let initval = ctx.args[0] as u32 as u64;
    let flags = ctx.args[1];
    if (flags & !EFD_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::eventfd::create(
        &fdt,
        vfs_ctx.cred(),
        initval,
        (flags & EFD_SEMAPHORE) != 0,
        (flags & O_NONBLOCK) != 0,
        (flags & O_CLOEXEC) != 0,
    )?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_timerfd_create(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let clock_id = ctx.args[0];
    let flags = ctx.args[1];
    clock_now_ns(clock_id)?;
    if (flags & !TFD_CREATE_SUPPORTED_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::timerfd::create(
        &fdt,
        vfs_ctx.cred(),
        clock_id,
        (flags & O_NONBLOCK) != 0,
        (flags & O_CLOEXEC) != 0,
    )?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_timerfd_settime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timerfd_settime_common(ctx)
}

pub(super) fn sys_timerfd_gettime(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timerfd_gettime_common(ctx)
}

pub(super) fn sys_signalfd4(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd_raw = ctx.args[0];
    let mask = read_sigset_arg(ctx.args[1], ctx.args[2])?;
    let flags = ctx.args[3];
    if (flags & !SFD_SUPPORTED_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    if fd_raw == usize::MAX {
        let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
        let fd = vfs::signalfd::create(
            &fdt,
            vfs_ctx.cred(),
            mask,
            (flags & O_NONBLOCK) != 0,
            (flags & O_CLOEXEC) != 0,
        )?;
        return Ok(fd.as_raw() as usize);
    }
    let fd = fd_arg(fd_raw)?;
    let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
    let signalfd = file
        .downcast_ops::<vfs::signalfd::SignalfdFileOps>()
        .ok_or(Errno::EINVAL)?;
    signalfd.set_mask(mask);
    let current = file.flags();
    file.set_status_flags(
        current.append,
        (flags & O_NONBLOCK) != 0,
        current.sync,
        current.direct,
        current.async_,
    );
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_epoll_create1(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[0];
    if (flags & !O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::epoll::create(&fdt, vfs_ctx.cred(), (flags & O_CLOEXEC) != 0)?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_epoll_ctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let epfd = fd_arg(ctx.args[0])?;
    let op = ctx.args[1] as i32;
    let fd = fd_arg(ctx.args[2])?;
    let event = if op == vfs::epoll::EPOLL_CTL_DEL {
        None
    } else {
        Some(read_epoll_event(ctx.args[3])?)
    };
    vfs::epoll::ctl(&fdt, epfd, op, fd, event)?;
    Ok(0)
}

pub(super) fn sys_epoll_pwait(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timeout_base_ns = sched::now_ns_direct();
    let epfd = fd_arg(ctx.args[0])?;
    let events_user = ctx.args[1];
    let maxevents = ctx.args[2] as i32;
    let timeout_ms = ctx.args[3] as i32 as i64;
    if maxevents <= 0 {
        return Err(Errno::EINVAL);
    }
    let deadline = if timeout_ms < 0 {
        None
    } else {
        Some(timeout_base_ns.saturating_add((timeout_ms as u64).saturating_mul(1_000_000)))
    };
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let sigmask = read_direct_sigmask(ctx.args[4], ctx.args[5])?;
    let _mask_guard = TemporarySigmask::install(sigmask);
    let ready = vfs::epoll::wait_until(&fdt, epfd, maxevents as usize, deadline)?;
    write_epoll_events(events_user, &ready)?;
    Ok(ready.len())
}

pub(super) fn sys_socket(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    ctx.ensure_network_execution_scope();
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = vfs_socket::socket(&vfs_ctx, &fdt, ctx.args[0], ctx.args[1], ctx.args[2])?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_socketpair(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    ctx.ensure_network_execution_scope();
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let (a, b) = vfs_socket::socketpair(&vfs_ctx, &fdt, ctx.args[0], ctx.args[1], ctx.args[2])?;
    let out = [a.as_raw() as i32, b.as_raw() as i32];
    let bytes = unsafe { core::slice::from_raw_parts(out.as_ptr() as *const u8, 8) };
    copy_to_user(ctx.args[3], bytes).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_bind(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let addr = copy_sockaddr_from_user(ctx.args[1], ctx.args[2])?;
    ctx.ensure_network_execution_scope();
    vfs_socket::bind(&vfs_ctx, &fdt, fd, &addr)?;
    Ok(0)
}

pub(super) fn sys_listen(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let backlog = (ctx.args[1] as i32).max(0) as usize;
    ctx.ensure_network_execution_scope();
    vfs_socket::listen(&fdt, fd, backlog)?;
    Ok(0)
}

pub(super) fn sys_accept(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    accept_common(ctx, 0)
}

pub(super) fn sys_accept4(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    accept_common(ctx, ctx.args[3])
}

pub(super) fn sys_connect(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let addr = copy_sockaddr_from_user(ctx.args[1], ctx.args[2])?;
    ctx.ensure_network_execution_scope();
    vfs_socket::connect(&vfs_ctx, &fdt, fd, &addr)?;
    Ok(0)
}

pub(super) fn sys_getsockname(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    getsockname_common(ctx, false)
}

pub(super) fn sys_getpeername(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    getsockname_common(ctx, true)
}

pub(super) fn sys_sendto(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    #[cfg(feature = "performance-profile")]
    let lookup_profile = profiling::scope(profiling::Event::SysUdpLookup);
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let len = ctx.args[2];
    if len > MAX_SOCKET_IO {
        return Err(Errno::EMSGSIZE);
    }
    let addr = if ctx.args[4] == 0 {
        None
    } else {
        Some(copy_sockaddr_from_user(ctx.args[4], ctx.args[5])?)
    };
    ctx.ensure_network_execution_scope();
    if let Some(vm) = current_vm_space()
        && let Some(file) = fdt.get_file(fd)
        && let Some(socket) = file.downcast_ops::<vfs::net_socket::NetSocketFileOps>()
        && usize::from(socket.sock_type()) == vfs_socket::SOCK_DGRAM
    {
        #[cfg(feature = "performance-profile")]
        drop(lookup_profile);
        vfs_socket::validate_send_flags(ctx.args[3])?;
        if (ctx.args[3] & (vfs_socket::MSG_OOB | vfs_socket::MSG_FASTOPEN)) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        // 最大 UDP 数据报即使从页尾开始也只覆盖 17 个 4 KiB 页；多留一个槽
        // 兼容页粒度变化，并在取得 socket 自旋锁前完成全部 fault-in。
        #[cfg(feature = "performance-profile")]
        let pin_profile = profiling::scope(profiling::Event::SysUdpPin);
        #[cfg(feature = "performance-profile")]
        let pin_start = profiling::read_counter();
        let windows = vm.pin_user_read_windows::<18>(ctx.args[1], len)?;
        #[cfg(feature = "performance-profile")]
        {
            drop(pin_profile);
            profiling::observe(
                profiling::Metric::UdpUserPinCycles,
                profiling::read_counter().wrapping_sub(pin_start),
            );
            profiling::observe(
                profiling::Metric::UdpUserPinnedWindows,
                windows.window_count() as u64,
            );
        }
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::SysSendSocket);
        #[cfg(feature = "performance-profile")]
        let mut user_copy_cycles = 0u64;
        let opts = vfs::net_socket::InetSendOptions {
            nonblocking: file.flags().nonblock || (ctx.args[3] & vfs_socket::MSG_DONTWAIT) != 0,
            more: (ctx.args[3] & vfs_socket::MSG_MORE) != 0,
            dont_route: (ctx.args[3] & vfs_socket::MSG_DONTROUTE) != 0,
            confirm: (ctx.args[3] & vfs_socket::MSG_CONFIRM) != 0,
            deadline_ns: None,
        };
        let result = socket.sendto_from(len, addr.as_deref(), opts, |offset, output| {
            #[cfg(feature = "performance-profile")]
            let copy_start = profiling::read_counter();
            let result = windows.copy_into(offset, output);
            #[cfg(feature = "performance-profile")]
            {
                user_copy_cycles = user_copy_cycles
                    .saturating_add(profiling::read_counter().wrapping_sub(copy_start));
            }
            result
        });
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(profiling::Metric::UdpUserCopyCycles, user_copy_cycles);
            if let Ok(written) = result {
                profile.set_bytes(written);
            }
        }
        return result;
    }
    if addr.is_none()
        && len != 0
        && (ctx.args[3] & vfs_socket::MSG_FASTOPEN) == 0
        && let Some(vm) = current_vm_space()
        && let Some(file) = fdt.get_file(fd)
        && let Some(socket) = inet_stream_file(&file)
    {
        return send_inet_stream_file_from_user(&vm, &file, socket, ctx.args[1], len, ctx.args[3]);
    }
    if len <= COPY_CHUNK {
        let mut data = [0u8; COPY_CHUNK];
        {
            #[cfg(feature = "performance-profile")]
            let _profile = profiling::scope(profiling::Event::SysSendCopy).bytes(len);
            copy_from_user(ctx.args[1], &mut data[..len]).map_err(|e| e.as_errno())?;
        }
        send_socket_payload(
            &vfs_ctx,
            &fdt,
            fd,
            &data[..len],
            addr.as_deref(),
            ctx.args[3],
        )
    } else {
        let mut data = zeroed_vec(len)?;
        {
            #[cfg(feature = "performance-profile")]
            let _profile = profiling::scope(profiling::Event::SysSendCopy).bytes(len);
            copy_from_user(ctx.args[1], &mut data).map_err(|e| e.as_errno())?;
        }
        send_socket_payload(&vfs_ctx, &fdt, fd, &data, addr.as_deref(), ctx.args[3])
    }
}

pub(super) fn sys_recvfrom(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    #[cfg(feature = "performance-profile")]
    let lookup_profile = profiling::scope(profiling::Event::SysUdpLookup);
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let len = ctx.args[2].min(MAX_SOCKET_IO);
    let want_addr = ctx.args[4] != 0 && ctx.args[5] != 0;
    let flags = ctx.args[3];
    ctx.ensure_network_execution_scope();
    if len != 0
        && (flags
            & (vfs_socket::MSG_PEEK
                | vfs_socket::MSG_WAITALL
                | vfs_socket::MSG_OOB
                | vfs_socket::MSG_ERRQUEUE))
            == 0
        && let Some(vm) = current_vm_space()
        && let Some(file) = fdt.get_file(fd)
        && let Some(socket) = file.downcast_ops::<vfs::net_socket::NetSocketFileOps>()
        && usize::from(socket.sock_type()) == vfs_socket::SOCK_DGRAM
        && let Some(received) = {
            #[cfg(feature = "performance-profile")]
            drop(lookup_profile);
            recv_local_datagram_to_user(&vm, &file, socket, ctx.args[1], len, flags)?
        }
    {
        if want_addr {
            if let Some(remote) = received.remote {
                let mut address = [0u8; MAX_SOCKET_ADDR];
                let address_len =
                    vfs::addr::encode_inet_sockaddr(&remote, socket.family(), &mut address)?;
                copy_sockaddr_to_user(ctx.args[4], ctx.args[5], Some(&address[..address_len]))?;
            } else {
                copy_sockaddr_to_user(ctx.args[4], ctx.args[5], None)?;
            }
        }
        return Ok(received.len);
    }
    if len != 0
        && !want_addr
        && (flags
            & (vfs_socket::MSG_PEEK
                | vfs_socket::MSG_WAITALL
                | vfs_socket::MSG_OOB
                | vfs_socket::MSG_ERRQUEUE))
            == 0
        && let Some(vm) = current_vm_space()
        && let Some(file) = fdt.get_file(fd)
        && let Some(socket) = inet_stream_file(&file)
    {
        return recv_inet_stream_file_to_user(&vm, &file, socket, ctx.args[1], len, flags);
    }
    if len <= COPY_CHUNK {
        let mut data = [0u8; COPY_CHUNK];
        recv_socket_payload(&fdt, fd, ctx.args[1], &mut data[..len], want_addr, ctx)
    } else {
        let mut data = zeroed_vec(len)?;
        recv_socket_payload(&fdt, fd, ctx.args[1], &mut data, want_addr, ctx)
    }
}

fn recv_local_datagram_to_user(
    vm: &VmSpace,
    file: &vfs::file::File,
    socket: &vfs::net_socket::NetSocketFileOps,
    user: usize,
    len: usize,
    flags: usize,
) -> Result<Option<vfs::net_socket::InetRecvResult>, Errno> {
    vfs_socket::validate_recv_flags(flags)?;
    let opts = vfs::net_socket::InetRecvOptions {
        nonblocking: file.flags().nonblock || (flags & vfs_socket::MSG_DONTWAIT) != 0,
        peek: false,
        wait_all: false,
        trunc: (flags & vfs_socket::MSG_TRUNC) != 0,
        defer_window_update: false,
        deadline_ns: None,
    };
    #[cfg(feature = "performance-profile")]
    let mut profile = profiling::scope(profiling::Event::SysRecvSocket);
    #[cfg(feature = "performance-profile")]
    let wait_profile = profiling::scope(profiling::Event::SysUdpWait);
    let Some(datagram_len) = socket.wait_datagram_readable(opts)? else {
        return Ok(None);
    };
    #[cfg(feature = "performance-profile")]
    drop(wait_profile);
    // 最大 UDP 数据报即使从页尾开始也只覆盖 17 个 4 KiB 页。等待完成后
    // 再固定目标，避免阻塞接收期间长期保留用户页。
    #[cfg(feature = "performance-profile")]
    let pin_profile = profiling::scope(profiling::Event::SysUdpPin);
    #[cfg(feature = "performance-profile")]
    let pin_start = profiling::read_counter();
    let windows = vm.pin_user_write_windows::<18>(user, len.min(datagram_len))?;
    #[cfg(feature = "performance-profile")]
    {
        drop(pin_profile);
        profiling::observe(
            profiling::Metric::UdpUserWritePinCycles,
            profiling::read_counter().wrapping_sub(pin_start),
        );
        profiling::observe(
            profiling::Metric::UdpUserWritePinnedWindows,
            windows.window_count() as u64,
        );
    }
    #[cfg(feature = "performance-profile")]
    let consume_profile = profiling::scope(profiling::Event::SysUdpConsume);
    let received = socket.recv_local_datagram_from(len, windows.len(), opts, |offset, input| {
        windows.copy_from(offset, input)
    })?;
    #[cfg(feature = "performance-profile")]
    drop(consume_profile);
    #[cfg(feature = "performance-profile")]
    if let Some(received) = received {
        profile.set_bytes(received.len.min(len));
    }
    Ok(received)
}

fn send_inet_stream_file_from_user(
    vm: &VmSpace,
    file: &vfs::file::File,
    socket: &vfs::net_socket::NetSocketFileOps,
    user: usize,
    len: usize,
    flags: usize,
) -> Result<usize, Errno> {
    vfs_socket::validate_send_flags(flags)?;
    if (flags & vfs_socket::MSG_OOB) != 0 {
        // Linux 语义：MSG_OOB 只发送缓冲的最后一个字节作为紧急数据，返回 1。
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        let mut byte = [0u8; 1];
        let urgent_ptr = user.checked_add(len - 1).ok_or(Errno::EFAULT)?;
        copy_from_user(urgent_ptr, &mut byte).map_err(|e| e.as_errno())?;
        let nonblocking = file.flags().nonblock || (flags & vfs_socket::MSG_DONTWAIT) != 0;
        return socket.send_oob(byte[0], nonblocking);
    }
    // 内部分页会临时设置 MSG_MORE；若后续用户页缺页失败或发送只完成一部分，
    // 仍须发布已经接受的数据。应用显式传入 MSG_MORE 时则保留其 cork 语义。
    let _batch = ((flags & vfs_socket::MSG_MORE) == 0).then(|| InetStreamWriteBatch { socket });
    let nonblocking = file.flags().nonblock || (flags & vfs_socket::MSG_DONTWAIT) != 0;
    let deadline_ns = socket.stream_send_deadline();
    let mut sent = 0usize;
    while sent < len {
        let user_ptr = user.checked_add(sent).ok_or(Errno::EFAULT)?;
        let remaining = len - sent;
        let window_target = remaining.min(MAX_SOCKET_IO);
        #[cfg(feature = "performance-profile")]
        let pin_start = profiling::read_counter();
        // MAX_SOCKET_IO 为 256 KiB；任意未对齐范围最多覆盖 65 个 4 KiB 页。
        // 一次固定后直接写入 socket ring，避免每页重复获取 TX 锁和更新 readiness。
        let windows = match vm.pin_user_read_windows::<65>(user_ptr, window_target) {
            Ok(windows) => windows,
            Err(error) => return if sent == 0 { Err(error) } else { Ok(sent) },
        };
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(
                profiling::Metric::TcpUserSendPinCycles,
                profiling::read_counter().wrapping_sub(pin_start),
            );
            profiling::observe(
                profiling::Metric::TcpUserSendPinnedWindows,
                windows.window_count() as u64,
            );
        }
        let window_len = windows.len();
        let more = sent + window_len < len || (flags & vfs_socket::MSG_MORE) != 0;
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::SysSendSocket);
        let result = socket.send_stream_from(
            window_len,
            nonblocking,
            deadline_ns,
            more,
            |offset, output| {
                windows
                    .copy_into(offset, output)
                    .expect("固定的 TCP 发送窗口必须覆盖声明范围");
            },
        );
        #[cfg(feature = "performance-profile")]
        if let Ok(written) = result {
            profile.set_bytes(written);
        }
        let written = match result {
            Ok(written) => written,
            Err(error) if sent != 0 && matches!(error, Errno::EAGAIN | Errno::EINTR) => {
                return Ok(sent);
            }
            Err(Errno::EPIPE) => {
                if (flags & vfs_socket::MSG_NOSIGNAL) == 0 {
                    deliver_sigpipe();
                }
                return Err(Errno::EPIPE);
            }
            Err(error) => return Err(error),
        };
        sent += written;
        if written == 0 || written < window_len {
            break;
        }
    }
    Ok(sent)
}

fn send_inet_stream_iovecs_from_user(
    vm: &VmSpace,
    file: &vfs::file::File,
    socket: &vfs::net_socket::NetSocketFileOps,
    iov: usize,
    iovcnt: usize,
    total: usize,
    flags: usize,
) -> Result<usize, Errno> {
    let mut sent = 0usize;
    for index in 0..iovcnt {
        let (base, len) = read_iovec(iov, index)?;
        let len = len.min(total.saturating_sub(sent));
        if len == 0 {
            continue;
        }
        let chunk_flags = if sent + len < total {
            flags | vfs_socket::MSG_MORE
        } else {
            flags
        };
        match send_inet_stream_file_from_user(vm, file, socket, base, len, chunk_flags) {
            Ok(written) => {
                sent += written;
                if written < len {
                    break;
                }
            }
            Err(_error) if sent != 0 => return Ok(sent),
            Err(error) => return Err(error),
        }
        if sent == total {
            break;
        }
    }
    Ok(sent)
}

fn recv_inet_stream_file_to_user(
    vm: &VmSpace,
    file: &vfs::file::File,
    socket: &vfs::net_socket::NetSocketFileOps,
    user: usize,
    len: usize,
    flags: usize,
) -> Result<usize, Errno> {
    vfs_socket::validate_recv_flags(flags)?;
    let _batch = InetStreamFileReceiveBatch { socket };
    let nonblocking = file.flags().nonblock || (flags & vfs_socket::MSG_DONTWAIT) != 0;
    let deadline_ns = socket.stream_recv_deadline();
    let mut received = 0usize;
    while received < len {
        let user_ptr = user.checked_add(received).ok_or(Errno::EFAULT)?;
        let remaining = len - received;
        let window_target = remaining.min(MAX_SOCKET_IO);
        #[cfg(feature = "performance-profile")]
        let pin_start = profiling::read_counter();
        let windows = match vm.pin_user_write_windows::<65>(user_ptr, window_target) {
            Ok(windows) => windows,
            Err(error) => {
                return if received == 0 {
                    Err(error)
                } else {
                    Ok(received)
                };
            }
        };
        #[cfg(feature = "performance-profile")]
        {
            profiling::observe(
                profiling::Metric::TcpUserReceivePinCycles,
                profiling::read_counter().wrapping_sub(pin_start),
            );
            profiling::observe(
                profiling::Metric::TcpUserReceivePinnedWindows,
                windows.window_count() as u64,
            );
        }
        let window_len = windows.len();
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::SysRecvSocket);
        let result = socket.recv_stream_to(
            window_len,
            nonblocking || received != 0,
            deadline_ns,
            true,
            |offset, input| {
                windows
                    .copy_from(offset, input)
                    .expect("固定的 TCP 接收窗口必须覆盖声明范围");
            },
        );
        #[cfg(feature = "performance-profile")]
        if let Ok(copied) = result {
            profile.set_bytes(copied);
        }
        let copied = match result {
            Ok(copied) => copied,
            Err(error) if received != 0 && matches!(error, Errno::EAGAIN | Errno::EINTR) => {
                break;
            }
            Err(error) => return Err(error),
        };
        received += copied;
        if copied == 0 || copied < window_len {
            break;
        }
    }
    Ok(received)
}

struct InetStreamFileReceiveBatch<'a> {
    socket: &'a vfs::net_socket::NetSocketFileOps,
}

impl Drop for InetStreamFileReceiveBatch<'_> {
    fn drop(&mut self) {
        self.socket.finish_stream_receive();
    }
}

fn recv_inet_stream_iovecs_to_user(
    vm: &VmSpace,
    file: &vfs::file::File,
    socket: &vfs::net_socket::NetSocketFileOps,
    iov: usize,
    iovcnt: usize,
    total: usize,
    flags: usize,
) -> Result<usize, Errno> {
    let _batch = InetStreamFileReceiveBatch { socket };
    let mut received = 0usize;
    for index in 0..iovcnt {
        let (base, len) = read_iovec(iov, index)?;
        let len = len.min(total.saturating_sub(received));
        if len == 0 {
            continue;
        }
        let chunk_flags = if received == 0 {
            flags
        } else {
            flags | vfs_socket::MSG_DONTWAIT
        };
        match recv_inet_stream_file_to_user(vm, file, socket, base, len, chunk_flags) {
            Ok(copied) => {
                received += copied;
                if copied < len {
                    break;
                }
            }
            Err(error) if received != 0 && matches!(error, Errno::EAGAIN | Errno::EINTR) => {
                return Ok(received);
            }
            Err(error) => return Err(error),
        }
        if received == total {
            break;
        }
    }
    Ok(received)
}

fn send_socket_payload(
    vfs_ctx: &vfs::VfsContext,
    fdt: &vfs::fdtable::FdTable,
    fd: Fd,
    data: &[u8],
    addr: Option<&[u8]>,
    flags: usize,
) -> Result<usize, Errno> {
    #[cfg(feature = "performance-profile")]
    let _profile = profiling::scope(profiling::Event::SysSendSocket).bytes(data.len());
    vfs_socket::send(vfs_ctx, fdt, fd, data, &[], addr, flags).map_err(|err| {
        if err == Errno::EPIPE && (flags & vfs_socket::MSG_NOSIGNAL) == 0 {
            deliver_sigpipe();
        }
        err
    })
}

fn recv_socket_payload(
    fdt: &vfs::fdtable::FdTable,
    fd: Fd,
    user_buf: usize,
    data: &mut [u8],
    want_addr: bool,
    ctx: &SyscallContext<'_>,
) -> Result<usize, Errno> {
    let out = {
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::SysRecvSocket);
        let out = vfs_socket::recv(fdt, fd, data, 0, want_addr, ctx.args[3], None)?;
        #[cfg(feature = "performance-profile")]
        profile.set_bytes(out.len);
        out
    };
    if out.len != 0 {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::SysRecvCopy).bytes(out.len);
        copy_to_user(user_buf, &data[..out.len]).map_err(|e| e.as_errno())?;
    }
    if want_addr {
        copy_sockaddr_to_user(ctx.args[4], ctx.args[5], out.address.as_deref())?;
    }
    Ok(out.len)
}

pub(super) fn sys_sendmsg(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let hdr = read_msghdr(ctx.args[1])?;
    if hdr.iovlen > IOV_MAX {
        return Err(Errno::EMSGSIZE);
    }
    ctx.ensure_network_execution_scope();
    if hdr.iovlen <= 1024
        && hdr.controllen == 0
        && (hdr.name == 0 || hdr.namelen == 0)
        && (ctx.args[2] & (vfs_socket::MSG_OOB | vfs_socket::MSG_FASTOPEN)) == 0
        && vfs_socket::inet_socket_type(&fdt, fd)? == Some(vfs_socket::SOCK_STREAM)
        && let Some(vm) = current_vm_space()
        && let Some(file) = fdt.get_file(fd)
        && let Some(socket) = inet_stream_file(&file)
    {
        let total = iov_total_len_capped(hdr.iov, hdr.iovlen, MAX_SOCKET_IO)?;
        if total != 0 {
            // 中间 iovec 会携带内部 MSG_MORE；任一后续 iovec 读取失败时也要发布
            // 已接受的数据。调用者显式 MSG_MORE 时不改变其可见状态。
            let _batch = ((ctx.args[2] & vfs_socket::MSG_MORE) == 0)
                .then(|| InetStreamWriteBatch { socket });
            return send_inet_stream_iovecs_from_user(
                &vm,
                &file,
                socket,
                hdr.iov,
                hdr.iovlen,
                total,
                ctx.args[2],
            );
        }
    }
    let data = copy_send_iovecs(hdr.iov, hdr.iovlen)?;
    let control = copy_user_region(hdr.control, hdr.controllen)?;
    let addr = if hdr.name == 0 || hdr.namelen == 0 {
        None
    } else {
        Some(copy_sockaddr_from_user(hdr.name, hdr.namelen as usize)?)
    };
    let sent = vfs_socket::send(
        &vfs_ctx,
        &fdt,
        fd,
        &data,
        &control,
        addr.as_deref(),
        ctx.args[2],
    )
    .map_err(|err| {
        if err == Errno::EPIPE && (ctx.args[2] & vfs_socket::MSG_NOSIGNAL) == 0 {
            deliver_sigpipe();
        }
        err
    })?;
    Ok(sent)
}

pub(super) fn sys_sendmmsg(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let msgvec_user = ctx.args[1];
    let vlen = ctx.args[2].min(1024);
    let flags = ctx.args[3];
    if vlen != 0 {
        ctx.ensure_network_execution_scope();
    }
    let mut sent_count = 0usize;
    for index in 0..vlen {
        let user = msgvec_ptr(msgvec_user, index)?;
        let hdr = read_mmsghdr(user)?;
        if hdr.msg_hdr.iovlen > IOV_MAX {
            return Err(Errno::EMSGSIZE);
        }
        let data = copy_send_iovecs(hdr.msg_hdr.iov, hdr.msg_hdr.iovlen)?;
        let control = copy_user_region(hdr.msg_hdr.control, hdr.msg_hdr.controllen)?;
        let addr = if hdr.msg_hdr.name == 0 || hdr.msg_hdr.namelen == 0 {
            None
        } else {
            Some(copy_sockaddr_from_user(
                hdr.msg_hdr.name,
                hdr.msg_hdr.namelen as usize,
            )?)
        };
        match vfs_socket::send(&vfs_ctx, &fdt, fd, &data, &control, addr.as_deref(), flags) {
            Ok(len) => {
                write_mmsghdr_len(user, len)?;
                sent_count += 1;
            }
            Err(_err) if sent_count != 0 => return Ok(sent_count),
            Err(Errno::EPIPE) if (flags & vfs_socket::MSG_NOSIGNAL) == 0 => {
                deliver_sigpipe();
                return Err(Errno::EPIPE);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(sent_count)
}

pub(super) fn sys_recvmsg(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let mut hdr = read_msghdr(ctx.args[1])?;
    if hdr.iovlen > IOV_MAX {
        return Err(Errno::EMSGSIZE);
    }
    ctx.ensure_network_execution_scope();
    let total = iov_total_len_capped(hdr.iov, hdr.iovlen, MAX_SOCKET_IO)?;
    if hdr.iovlen <= 1024
        && hdr.controllen == 0
        && (hdr.name == 0 || hdr.namelen == 0)
        && total != 0
        && (ctx.args[2]
            & (vfs_socket::MSG_PEEK
                | vfs_socket::MSG_WAITALL
                | vfs_socket::MSG_OOB
                | vfs_socket::MSG_ERRQUEUE))
            == 0
        && vfs_socket::inet_socket_type(&fdt, fd)? == Some(vfs_socket::SOCK_STREAM)
        && let Some(vm) = current_vm_space()
        && let Some(file) = fdt.get_file(fd)
        && let Some(socket) = inet_stream_file(&file)
    {
        let len = recv_inet_stream_iovecs_to_user(
            &vm,
            &file,
            socket,
            hdr.iov,
            hdr.iovlen,
            total,
            ctx.args[2],
        )?;
        hdr.controllen = 0;
        hdr.namelen = 0;
        hdr.flags = 0;
        write_msghdr(ctx.args[1], &hdr)?;
        return Ok(len);
    }
    let mut data = zeroed_vec(total)?;
    let want_addr = hdr.name != 0 && hdr.namelen != 0;
    let out = vfs_socket::recv(
        &fdt,
        fd,
        &mut data,
        hdr.controllen,
        want_addr,
        ctx.args[2],
        None,
    )?;
    scatter_recv_iovecs(hdr.iov, hdr.iovlen, &data[..out.len])?;
    if hdr.control != 0 && !out.control.is_empty() {
        let copy_len = out.control.len().min(hdr.controllen);
        copy_to_user(hdr.control, &out.control[..copy_len]).map_err(|e| e.as_errno())?;
        hdr.controllen = copy_len;
    } else {
        hdr.controllen = 0;
    }
    if want_addr {
        copy_sockaddr_bytes(hdr.name, hdr.namelen as usize, out.address.as_deref())?;
        hdr.namelen = out.address.as_ref().map_or(0, |a| a.len() as u32);
    } else {
        hdr.namelen = 0;
    }
    hdr.flags = out.msg_flags as i32;
    write_msghdr(ctx.args[1], &hdr)?;
    Ok(out.len)
}

pub(super) fn sys_recvmmsg(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let msgvec_user = ctx.args[1];
    let vlen = ctx.args[2].min(1024);
    let flags = ctx.args[3];
    let deadline = read_socket_timeout_deadline(ctx.args[4])?;
    if vlen != 0 {
        ctx.ensure_network_execution_scope();
    }
    let mut recv_count = 0usize;
    for index in 0..vlen {
        let user = msgvec_ptr(msgvec_user, index)?;
        let mut hdr = read_mmsghdr(user)?;
        if hdr.msg_hdr.iovlen > IOV_MAX {
            return Err(Errno::EMSGSIZE);
        }
        let total = iov_total_len_capped(hdr.msg_hdr.iov, hdr.msg_hdr.iovlen, MAX_SOCKET_IO)?;
        let mut data = zeroed_vec(total)?;
        let want_addr = hdr.msg_hdr.name != 0 && hdr.msg_hdr.namelen != 0;
        match vfs_socket::recv(
            &fdt,
            fd,
            &mut data,
            hdr.msg_hdr.controllen,
            want_addr,
            flags,
            deadline,
        ) {
            Ok(out) => {
                scatter_recv_iovecs(hdr.msg_hdr.iov, hdr.msg_hdr.iovlen, &data[..out.len])?;
                if hdr.msg_hdr.control != 0 && !out.control.is_empty() {
                    let copy_len = out.control.len().min(hdr.msg_hdr.controllen);
                    copy_to_user(hdr.msg_hdr.control, &out.control[..copy_len])
                        .map_err(|e| e.as_errno())?;
                    hdr.msg_hdr.controllen = copy_len;
                } else {
                    hdr.msg_hdr.controllen = 0;
                }
                if want_addr {
                    copy_sockaddr_bytes(
                        hdr.msg_hdr.name,
                        hdr.msg_hdr.namelen as usize,
                        out.address.as_deref(),
                    )?;
                    hdr.msg_hdr.namelen = out.address.as_ref().map_or(0, |a| a.len() as u32);
                } else {
                    hdr.msg_hdr.namelen = 0;
                }
                hdr.msg_hdr.flags = out.msg_flags as i32;
                hdr.msg_len = out.len as u32;
                write_mmsghdr(user, &hdr)?;
                recv_count += 1;
                if out.len == 0 {
                    break;
                }
            }
            Err(err) if recv_count != 0 && matches!(err, Errno::EAGAIN | Errno::EINTR) => {
                return Ok(recv_count);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(recv_count)
}

/// SOL_SOCKET（Linux 标准值 1）。
const SOL_SOCKET_LEVEL: i32 = 1;

pub(super) fn sys_setsockopt(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let level = ctx.args[1] as i32;
    let optname = ctx.args[2] as i32;
    // SO_ATTACH_FILTER 的 optval 是 sock_fprog（含用户指针），需要在内核层解引用。
    if level == SOL_SOCKET_LEVEL && optname == vfs_socket::SO_ATTACH_FILTER {
        let fprog = copy_user_region(ctx.args[3], ctx.args[4])?;
        let instructions = read_sock_fprog(&fprog)?;
        ctx.ensure_network_execution_scope();
        vfs_socket::socket_attach_filter(&fdt, fd, instructions)?;
        return Ok(0);
    }
    if level == SOL_SOCKET_LEVEL && optname == vfs_socket::SO_DETACH_FILTER {
        ctx.ensure_network_execution_scope();
        vfs_socket::socket_detach_filter(&fdt, fd)?;
        return Ok(0);
    }
    if level == SOL_SOCKET_LEVEL && optname == vfs_socket::SO_LOCK_FILTER {
        ctx.ensure_network_execution_scope();
        vfs_socket::socket_lock_filter(&fdt, fd)?;
        return Ok(0);
    }
    let value = copy_user_region(ctx.args[3], ctx.args[4])?;
    ctx.ensure_network_execution_scope();
    vfs_socket::setsockopt(&fdt, fd, level, optname, &value)?;
    Ok(0)
}

pub(super) fn sys_getsockopt(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let level = ctx.args[1] as i32;
    let optname = ctx.args[2] as i32;
    if level == SOL_SOCKET_LEVEL && optname == vfs_socket::SO_ATTACH_FILTER {
        // SO_GET_FILTER：把已安装程序写入用户 sock_fprog。
        ctx.ensure_network_execution_scope();
        let instructions = vfs_socket::socket_get_filter(&fdt, fd)?;
        write_sock_fprog(ctx.args[3], ctx.args[4], &instructions)?;
        return Ok(0);
    }
    ctx.ensure_network_execution_scope();
    let value = vfs_socket::getsockopt(&fdt, fd, level, optname)?;
    copy_optval_to_user(ctx.args[3], ctx.args[4], &value)?;
    Ok(0)
}

/// 从用户 sock_fprog（64 位：len u16 + pad 6 + filter 指针 8）读取指令数组。
fn read_sock_fprog(fprog: &[u8]) -> Result<alloc::vec::Vec<net::bpf::CbpfInsn>, Errno> {
    if fprog.len() < 16 {
        return Err(Errno::EINVAL);
    }
    let len = u16::from_ne_bytes(fprog[..2].try_into().unwrap());
    let filter_ptr = usize::from_ne_bytes(fprog[8..16].try_into().unwrap());
    if len == 0 || usize::from(len) > 4096 {
        return Err(Errno::EINVAL);
    }
    let mut raw = alloc::vec![0u8; usize::from(len) * 8];
    copy_from_user(filter_ptr, &mut raw).map_err(|e| e.as_errno())?;
    net::bpf::parse_sock_filters(&raw).map_err(|_| Errno::EINVAL)
}

/// 把过滤器指令写回用户 sock_fprog（SO_GET_FILTER 语义）。
fn write_sock_fprog(
    optval: usize,
    optlen: usize,
    instructions: &[net::bpf::CbpfInsn],
) -> Result<(), Errno> {
    if optval == 0 || optlen < 16 {
        return Err(Errno::EINVAL);
    }
    let mut fprog = [0u8; 16];
    copy_from_user(optval, &mut fprog).map_err(|e| e.as_errno())?;
    let capacity = u16::from_ne_bytes(fprog[..2].try_into().unwrap()) as usize;
    let filter_ptr = usize::from_ne_bytes(fprog[8..16].try_into().unwrap());
    let count = instructions.len().min(capacity);
    let bytes = net::bpf::serialize_sock_filters(&instructions[..count]);
    if !bytes.is_empty() {
        copy_to_user(filter_ptr, &bytes).map_err(|e| e.as_errno())?;
    }
    // 回写实际指令数。
    let mut updated = fprog;
    updated[..2].copy_from_slice(&(count as u16).to_ne_bytes());
    copy_to_user(optval, &updated).map_err(|e| e.as_errno())?;
    Ok(())
}

pub(super) fn sys_shutdown(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    ctx.ensure_network_execution_scope();
    vfs_socket::shutdown(&fdt, fd, ctx.args[1])?;
    Ok(0)
}

pub(super) fn sys_ppoll(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fds_user = ctx.args[0];
    let nfds = ctx.args[1];
    let timeout_user = ctx.args[2];
    let sigmask = read_direct_sigmask(ctx.args[3], ctx.args[4])?;
    #[cfg(feature = "trace-signal-wait")]
    log::info!(
        "[syscall][ppoll] enter pid={:?} nfds={} timeout_ptr={:#x} sigmask_ptr={:#x}",
        ctx.task().pid_root(),
        nfds,
        timeout_user,
        ctx.args[3],
    );

    const POLLFD_SIZE: usize = 8;
    const MAX_POLLFDS: usize = 1024;
    if nfds > MAX_POLLFDS {
        return Err(Errno::EINVAL);
    }
    let total_bytes = nfds.checked_mul(POLLFD_SIZE).ok_or(Errno::EINVAL)?;
    let mut pollfds_stack = [0u8; POLLFD_SIZE * 64];
    let mut pollfds_heap;
    let pollfds = if total_bytes <= pollfds_stack.len() {
        &mut pollfds_stack[..total_bytes]
    } else {
        pollfds_heap = vec![0u8; total_bytes];
        pollfds_heap.as_mut_slice()
    };
    copy_from_user(fds_user, pollfds).map_err(|e| e.as_errno())?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let mut lookup_index = Vec::with_capacity(nfds.min(64));
    let mut lookup_fds = Vec::with_capacity(nfds.min(64));
    for i in 0..nfds {
        let off = i * POLLFD_SIZE;
        let fd_raw = i32::from_le_bytes(pollfds[off..off + 4].try_into().unwrap());
        if fd_raw >= 0 {
            lookup_index.push(i);
            lookup_fds.push(Fd::from_raw(fd_raw as u32));
        }
    }

    let timeout_ms = read_timespec_ms_ceil(timeout_user)?;
    let _mask_guard = TemporarySigmask::install(sigmask);

    let deadline = timeout_deadline(timeout_ms);
    loop {
        let mut count = 0usize;
        let mut waiters: Vec<(Arc<vfs::file::File>, PollEvents)> =
            Vec::with_capacity(lookup_fds.len());
        let files = fdt.get_files_dense(&lookup_fds);
        let mut lookup_cursor = 0usize;

        for i in 0..nfds {
            let off = i * POLLFD_SIZE;
            let fd_raw = i32::from_le_bytes(pollfds[off..off + 4].try_into().unwrap());
            let events = u16::from_le_bytes(pollfds[off + 4..off + 6].try_into().unwrap());
            if fd_raw < 0 {
                pollfds[off + 6..off + 8].copy_from_slice(&0u16.to_le_bytes());
                continue;
            }

            debug_assert_eq!(lookup_index[lookup_cursor], i);
            if let Some(file) = files[lookup_cursor].as_ref().map(Arc::clone) {
                let interest = PollEvents(events);
                let ready = file.poll(interest);
                if ready.0 != 0 {
                    pollfds[off + 6..off + 8].copy_from_slice(&ready.0.to_le_bytes());
                    count += 1;
                } else {
                    pollfds[off + 6..off + 8].copy_from_slice(&0u16.to_le_bytes());
                    if !interest.is_empty() {
                        waiters.push((file, interest));
                    }
                }
            } else {
                pollfds[off + 6..off + 8].copy_from_slice(&PollEvents::POLLNVAL.0.to_le_bytes());
                count += 1;
            }
            lookup_cursor += 1;
        }

        if count != 0 {
            copy_to_user(fds_user, pollfds).map_err(|e| e.as_errno())?;
            return Ok(count);
        }

        if timeout_expired(deadline) || timeout_ms == 0 {
            // 确保 revents 已写回到用户空间
            copy_to_user(fds_user, pollfds).map_err(|e| e.as_errno())?;
            return Ok(0);
        }

        wait_on_poll_sources(&waiters, deadline)?;
    }
}

pub(super) fn sys_pselect6(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let nfds = ctx.args[0];
    let readfds_user = ctx.args[1];
    let writefds_user = ctx.args[2];
    let exceptfds_user = ctx.args[3];
    let timeout_user = ctx.args[4];
    let sigmask = read_pselect_sigmask(ctx.args[5])?;
    #[cfg(feature = "trace-signal-wait")]
    log::info!(
        "[syscall][pselect] enter pid={:?} nfds={} timeout_ptr={:#x} sigmask_arg={:#x}",
        ctx.task().pid_root(),
        nfds,
        timeout_user,
        ctx.args[5],
    );

    const MAX_SELECT_FDS: usize = 1024;
    if nfds > MAX_SELECT_FDS {
        return Err(Errno::EINVAL);
    }

    let set_len = nfds.div_ceil(8);
    let read_in = copy_fdset_from_user(readfds_user, set_len)?;
    let write_in = copy_fdset_from_user(writefds_user, set_len)?;
    let except_in = copy_fdset_from_user(exceptfds_user, set_len)?;
    let mut read_out_storage = [0u8; MAX_SELECT_FDS / 8];
    let mut write_out_storage = [0u8; MAX_SELECT_FDS / 8];
    let mut except_out_storage = [0u8; MAX_SELECT_FDS / 8];
    let read_out = &mut read_out_storage[..set_len];
    let write_out = &mut write_out_storage[..set_len];
    let except_out = &mut except_out_storage[..set_len];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let mut lookup_index = Vec::with_capacity(nfds.min(64));
    let mut lookup_fds = Vec::with_capacity(nfds.min(64));
    for fd_num in 0..nfds {
        if fdset_test(&read_in, fd_num)
            || fdset_test(&write_in, fd_num)
            || fdset_test(&except_in, fd_num)
        {
            lookup_index.push(fd_num);
            lookup_fds.push(Fd::from_raw(fd_num as u32));
        }
    }
    let timeout_ms = read_timespec_ms_ceil(timeout_user)?;
    let _mask_guard = TemporarySigmask::install(sigmask);
    let deadline = timeout_deadline(timeout_ms);
    loop {
        clear_fdset(read_out);
        clear_fdset(write_out);
        clear_fdset(except_out);
        let mut count = 0usize;
        let mut waiters: Vec<(Arc<vfs::file::File>, PollEvents)> =
            Vec::with_capacity(lookup_fds.len());
        let files = fdt.get_files_dense(&lookup_fds);
        let mut lookup_cursor = 0usize;

        for fd_num in 0..nfds {
            let want_read = fdset_test(&read_in, fd_num);
            let want_write = fdset_test(&write_in, fd_num);
            let want_except = fdset_test(&except_in, fd_num);
            if !want_read && !want_write && !want_except {
                continue;
            }
            debug_assert_eq!(lookup_index[lookup_cursor], fd_num);
            let file = files[lookup_cursor]
                .as_ref()
                .map(Arc::clone)
                .ok_or(Errno::EBADF)?;
            lookup_cursor += 1;
            let mut interest = PollEvents::default();
            if want_read {
                interest = interest.with(PollEvents::POLLIN);
            }
            if want_write {
                interest = interest.with(PollEvents::POLLOUT);
            }
            if want_except {
                interest = interest.with(PollEvents::POLLPRI);
            }
            let ready = file.poll(interest);
            let mut fd_ready = false;
            if want_read
                && ready.has(
                    PollEvents::POLLIN
                        .with(PollEvents::POLLHUP)
                        .with(PollEvents::POLLERR),
                )
            {
                fdset_set(read_out, fd_num);
                fd_ready = true;
            }
            if want_write && ready.has(PollEvents::POLLOUT.with(PollEvents::POLLERR)) {
                fdset_set(write_out, fd_num);
                fd_ready = true;
            }
            if want_except && ready.has(PollEvents::POLLPRI.with(PollEvents::POLLERR)) {
                fdset_set(except_out, fd_num);
                fd_ready = true;
            }
            if fd_ready {
                count += 1;
            } else if !interest.is_empty() {
                waiters.push((file, interest));
            }
        }

        if count != 0 {
            copy_fdset_to_user(readfds_user, read_out)?;
            copy_fdset_to_user(writefds_user, write_out)?;
            copy_fdset_to_user(exceptfds_user, except_out)?;
            return Ok(count);
        }

        if timeout_expired(deadline) || timeout_ms == 0 {
            copy_fdset_to_user(readfds_user, read_out)?;
            copy_fdset_to_user(writefds_user, write_out)?;
            copy_fdset_to_user(exceptfds_user, except_out)?;
            return Ok(0);
        }

        wait_on_poll_sources(&waiters, deadline)?;
    }
}

// ── xattr ────────────────────────────────────────────────────────────────────

const XATTR_CREATE: u32 = 1;
const XATTR_REPLACE: u32 = 2;

/// 拷贝并校验 xattr 名称（长度超限 → ERANGE）。
fn copy_xattr_name(user: usize) -> Result<Vec<u8>, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let name =
        copy_cstr_from_user(user, vfs::xattr::XATTR_NAME_MAX + 1).map_err(|e| e.as_errno())?;
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Err(Errno::EINVAL);
    }
    if bytes.len() > vfs::xattr::XATTR_NAME_MAX {
        return Err(Errno::ERANGE);
    }
    Ok(bytes.to_vec())
}

/// 拷贝 xattr 值（长度超限 → E2BIG）。
fn copy_xattr_value(user: usize, size: usize) -> Result<Vec<u8>, Errno> {
    if user == 0 {
        return Ok(Vec::new());
    }
    if size > vfs::xattr::XATTR_SIZE_MAX {
        return Err(Errno::E2BIG);
    }
    let mut buf = vec![0u8; size];
    copy_from_user(user, &mut buf).map_err(|e| e.as_errno())?;
    Ok(buf)
}

/// getxattr/listxattr 的"值/列表拷回"公共逻辑：size==0 返回所需长度；
/// 缓冲不足返回 ERANGE。
fn copy_xattr_out(user: usize, size: usize, data: &[u8]) -> Result<usize, Errno> {
    if size == 0 {
        return Ok(data.len());
    }
    if size < data.len() {
        return Err(Errno::ERANGE);
    }
    copy_to_user(user, data).map_err(|e| e.as_errno())?;
    Ok(data.len())
}

fn vfs_ctx_or_err() -> Result<alloc::sync::Arc<vfs::VfsContext>, Errno> {
    current_vfs_context().ok_or(Errno::EBADF)
}

fn xattr_flags_validate(flags: usize) -> Result<u32, Errno> {
    if flags & !(XATTR_CREATE as usize | XATTR_REPLACE as usize) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(flags as u32)
}

// ── 路径变体（setxattr/lsetxattr/getxattr/lgetxattr/listxattr/llistxattr/
//    removexattr/lremovexattr）───────────────────────────────────────────────

pub(super) fn sys_setxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let value = copy_xattr_value(ctx.args[2], ctx.args[3])?;
    let flags = xattr_flags_validate(ctx.args[4])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    operation::setxattr(&vfs_ctx, &Dirfd::Cwd, &path, &name, &value, flags, false)
        .map_err(|e| e.to_errno())?;
    let _ = fdt;
    Ok(0)
}

pub(super) fn sys_lsetxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let value = copy_xattr_value(ctx.args[2], ctx.args[3])?;
    let flags = xattr_flags_validate(ctx.args[4])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    operation::setxattr(&vfs_ctx, &Dirfd::Cwd, &path, &name, &value, flags, true)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fsetxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let value = copy_xattr_value(ctx.args[2], ctx.args[3])?;
    let flags = xattr_flags_validate(ctx.args[4])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    operation::fsetxattr(&vfs_ctx, &fdt, fd, &name, &value, flags).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_getxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let value = ctx.args[2];
    let size = ctx.args[3];
    let vfs_ctx = vfs_ctx_or_err()?;
    let data = operation::getxattr(&vfs_ctx, &Dirfd::Cwd, &path, &name, false)
        .map_err(|e| e.to_errno())?
        .ok_or(Errno::ENODATA)?;
    copy_xattr_out(value, size, &data)
}

pub(super) fn sys_lgetxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let value = ctx.args[2];
    let size = ctx.args[3];
    let vfs_ctx = vfs_ctx_or_err()?;
    let data = operation::getxattr(&vfs_ctx, &Dirfd::Cwd, &path, &name, true)
        .map_err(|e| e.to_errno())?
        .ok_or(Errno::ENODATA)?;
    copy_xattr_out(value, size, &data)
}

pub(super) fn sys_fgetxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let value = ctx.args[2];
    let size = ctx.args[3];
    let vfs_ctx = vfs_ctx_or_err()?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let data = operation::fgetxattr(&vfs_ctx, &fdt, fd, &name)
        .map_err(|e| e.to_errno())?
        .ok_or(Errno::ENODATA)?;
    copy_xattr_out(value, size, &data)
}

pub(super) fn sys_listxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let list = ctx.args[1];
    let size = ctx.args[2];
    let vfs_ctx = vfs_ctx_or_err()?;
    let names =
        operation::listxattr(&vfs_ctx, &Dirfd::Cwd, &path, false).map_err(|e| e.to_errno())?;
    let data = vfs::xattr::encode_list(&names);
    copy_xattr_out(list, size, &data)
}

pub(super) fn sys_llistxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let list = ctx.args[1];
    let size = ctx.args[2];
    let vfs_ctx = vfs_ctx_or_err()?;
    let names =
        operation::listxattr(&vfs_ctx, &Dirfd::Cwd, &path, true).map_err(|e| e.to_errno())?;
    let data = vfs::xattr::encode_list(&names);
    copy_xattr_out(list, size, &data)
}

pub(super) fn sys_flistxattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let list = ctx.args[1];
    let size = ctx.args[2];
    let vfs_ctx = vfs_ctx_or_err()?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let names = operation::flistxattr(&vfs_ctx, &fdt, fd).map_err(|e| e.to_errno())?;
    let data = vfs::xattr::encode_list(&names);
    copy_xattr_out(list, size, &data)
}

pub(super) fn sys_removexattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    operation::removexattr(&vfs_ctx, &Dirfd::Cwd, &path, &name, false).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_lremovexattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let path = copy_path_from_user(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    operation::removexattr(&vfs_ctx, &Dirfd::Cwd, &path, &name, true).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fremovexattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let name = copy_xattr_name(ctx.args[1])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    operation::fremovexattr(&vfs_ctx, &fdt, fd, &name).map_err(|e| e.to_errno())?;
    Ok(0)
}

// ── at 变体（setxattrat/getxattrat/listxattrat/removexattrat）──────────────

fn xattrat_flags_validate(flags: usize) -> Result<u32, Errno> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(flags as u32)
}

pub(super) fn sys_setxattrat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let name = copy_xattr_name(ctx.args[2])?;
    let value = copy_xattr_value(ctx.args[3], ctx.args[4])?;
    let flags = xattrat_flags_validate(ctx.args[5])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    if path.is_empty() {
        // AT_EMPTY_PATH：作用于 dirfd 指向的文件本身。
        let fd = dirfd_as_fd(&dirfd, &fdt).ok_or(Errno::EINVAL)?;
        operation::fsetxattr(&vfs_ctx, &fdt, fd, &name, &value, flags).map_err(|e| e.to_errno())?;
        return Ok(0);
    }
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW as u32) != 0;
    operation::setxattr(&vfs_ctx, &dirfd, &path, &name, &value, flags, no_follow)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_getxattrat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let name = copy_xattr_name(ctx.args[2])?;
    let value = ctx.args[3];
    let size = ctx.args[4];
    let flags = xattrat_flags_validate(ctx.args[5])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    let data = if path.is_empty() {
        let fd = dirfd_as_fd(&dirfd, &fdt).ok_or(Errno::EINVAL)?;
        operation::fgetxattr(&vfs_ctx, &fdt, fd, &name)
            .map_err(|e| e.to_errno())?
            .ok_or(Errno::ENODATA)?
    } else {
        let no_follow = (flags & AT_SYMLINK_NOFOLLOW as u32) != 0;
        operation::getxattr(&vfs_ctx, &dirfd, &path, &name, no_follow)
            .map_err(|e| e.to_errno())?
            .ok_or(Errno::ENODATA)?
    };
    copy_xattr_out(value, size, &data)
}

pub(super) fn sys_listxattrat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let list = ctx.args[2];
    let size = ctx.args[3];
    let flags = xattrat_flags_validate(ctx.args[4])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    let names = if path.is_empty() {
        let fd = dirfd_as_fd(&dirfd, &fdt).ok_or(Errno::EINVAL)?;
        operation::flistxattr(&vfs_ctx, &fdt, fd).map_err(|e| e.to_errno())?
    } else {
        let no_follow = (flags & AT_SYMLINK_NOFOLLOW as u32) != 0;
        operation::listxattr(&vfs_ctx, &dirfd, &path, no_follow).map_err(|e| e.to_errno())?
    };
    let data = vfs::xattr::encode_list(&names);
    copy_xattr_out(list, size, &data)
}

pub(super) fn sys_removexattrat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let name = copy_xattr_name(ctx.args[2])?;
    let flags = xattrat_flags_validate(ctx.args[3])?;
    let vfs_ctx = vfs_ctx_or_err()?;
    if path.is_empty() {
        let fd = dirfd_as_fd(&dirfd, &fdt).ok_or(Errno::EINVAL)?;
        operation::fremovexattr(&vfs_ctx, &fdt, fd, &name).map_err(|e| e.to_errno())?;
        return Ok(0);
    }
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW as u32) != 0;
    operation::removexattr(&vfs_ctx, &dirfd, &path, &name, no_follow).map_err(|e| e.to_errno())?;
    Ok(0)
}

/// 从 Dirfd 取底层 fd（仅 Fd 变体；Cwd 返回 None）。
///
/// FdTable 无反向索引，用快照线性反查（AT_EMPTY_PATH 属低频路径）。
fn dirfd_as_fd(dirfd: &Dirfd, fdt: &vfs::fdtable::FdTable) -> Option<Fd> {
    match dirfd {
        Dirfd::Cwd => None,
        Dirfd::Fd(file) => fdt
            .snapshot_fds()
            .into_iter()
            .find(|(_, f)| alloc::sync::Arc::ptr_eq(f, file))
            .map(|(raw, _)| Fd::from_raw(raw)),
    }
}

pub(super) fn sys_lookup_dcookie(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // lookup_dcookie(2) 把 perf/oprofile 的 64-bit cookie 反查为文件路径。本内核
    // 没有 perf 子系统产生 cookie，维护一个 cookie→路径注册表只会留下无生产者
    // 的死状态；因此保持 ENOSYS（Linux 在无 CONFIG_PROFILING 时同样不可用）。
    Err(Errno::ENOSYS)
}

pub(super) fn sys_inotify_init1(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // IN_CLOEXEC = O_CLOEXEC、IN_NONBLOCK = O_NONBLOCK（Linux 语义）。
    const IN_NONBLOCK: usize = O_NONBLOCK;
    const IN_CLOEXEC: usize = O_CLOEXEC;
    let flags = ctx.args[0];
    if flags & !(IN_NONBLOCK | IN_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::inotify::create(
        &fdt,
        vfs_ctx.cred(),
        (flags & IN_NONBLOCK) != 0,
        (flags & IN_CLOEXEC) != 0,
    )?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_inotify_add_watch(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    use vfs::fsnotify::*;
    let fd = fd_arg(ctx.args[0])?;
    let path = copy_path_from_user(ctx.args[1])?;
    let mask = ctx.args[2] as u32;
    if mask & !IN_ADD_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    // 至少包含一个事件位（Linux 语义）。
    const IN_EVENT_BITS: u32 = IN_ACCESS
        | IN_MODIFY
        | IN_ATTRIB
        | IN_CLOSE_WRITE
        | IN_CLOSE_NOWRITE
        | IN_OPEN
        | IN_MOVED_FROM
        | IN_MOVED_TO
        | IN_CREATE
        | IN_DELETE
        | IN_DELETE_SELF
        | IN_MOVE_SELF
        | IN_UNMOUNT;
    if mask & IN_EVENT_BITS == 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let file = file_for_fd(fd)?;
    let instance = vfs::inotify::instance_from_file(&file).ok_or(Errno::EINVAL)?;
    let no_follow = (mask & IN_DONT_FOLLOW) != 0;
    let onlydir = (mask & IN_ONLYDIR) != 0;
    let inode = operation::lookup_watch_inode(&vfs_ctx, &Dirfd::Cwd, &path, no_follow, onlydir)
        .map_err(|e| e.to_errno())?;
    let watch_mask = mask & IN_EVENT_BITS;
    let watch_flags =
        mask & (IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_ADD | IN_ONESHOT);
    let wd = instance.add_watch(&inode, watch_mask, watch_flags)?;
    Ok(wd as usize)
}

pub(super) fn sys_inotify_rm_watch(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let wd = ctx.args[1] as i32;
    let file = file_for_fd(fd)?;
    let instance = vfs::inotify::instance_from_file(&file).ok_or(Errno::EINVAL)?;
    instance.rm_watch(wd)?;
    Ok(0)
}

pub(super) fn sys_ioprio_set(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let which = ctx.args[0];
    let who = ctx.args[1] as i32;
    let ioprio = validate_ioprio(ctx.args[2])?;
    if ioprio_class(ioprio) == IOPRIO_CLASS_RT
        && !ctx.task().credentials().has_cap(Capability::SysAdmin)
    {
        return Err(Errno::EPERM);
    }
    let targets = ioprio_targets(which, who, ctx.task())?;
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    for task in targets {
        if !task_may_access(ctx.task(), &task) {
            return Err(Errno::EPERM);
        }
        task.set_ioprio(ioprio);
    }
    Ok(0)
}

pub(super) fn sys_ioprio_get(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let which = ctx.args[0];
    let who = ctx.args[1] as i32;
    let targets = ioprio_targets(which, who, ctx.task())?;
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    let mut best = u16::MAX;
    for task in targets {
        if !task_may_access(ctx.task(), &task) {
            return Err(Errno::EPERM);
        }
        best = best.min(task.ioprio());
    }
    Ok(best as usize)
}

pub(super) fn sys_renameat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    renameat_common(ctx.args[0], ctx.args[1], ctx.args[2], ctx.args[3], 0)
}

pub(super) fn sys_nfsservctl(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_vhangup(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    // Linux 要求 CAP_SYS_TTY_CONFIG；本内核能力集以 SysAdmin 近似该管理能力。
    if !ctx.task().credentials().has_cap(Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    // 对当前会话的控制终端执行挂起（SIGHUP 到前台进程组 + 撤销后续访问）。
    // 无控制终端时按 Linux 语义视为 no-op 成功。
    if let Some(cookie) = sched::operation::current_session_ctty()
        && let Some(core) = general::dev::tty::resolve_ctty_cookie(cookie)
    {
        core.hangup();
    }
    Ok(0)
}

pub(super) fn sys_quotactl(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_preadv(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let offset = nonnegative_split_offset_arg(ctx.args[3], ctx.args[4])?;
    let file = file_for_fd(fd)?;
    read_iovecs(ctx, &file, iov, iovcnt, Some(offset), false)
}

pub(super) fn sys_pwritev(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let offset = nonnegative_split_offset_arg(ctx.args[3], ctx.args[4])?;
    let file = file_for_fd(fd)?;
    write_iovecs(ctx, &file, iov, iovcnt, Some(offset), false)
}

pub(super) fn sys_vmsplice(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    let flags = ctx.args[3];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    if (flags & !SPLICE_F_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    write_iovecs(ctx, &file, iov, iovcnt, None, false)
}

pub(super) fn sys_splice(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd_in = fd_arg(ctx.args[0])?;
    let off_in_user = ctx.args[1];
    let fd_out = fd_arg(ctx.args[2])?;
    let off_out_user = ctx.args[3];
    let len = ctx.args[4];
    let flags = ctx.args[5];
    if (flags & !SPLICE_F_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    let in_file = file_for_fd(fd_in)?;
    let out_file = file_for_fd(fd_out)?;
    // splice 至少一端必须是管道,否则 EINVAL。
    let in_pipe = vfs::pipe::pipe_of(&in_file).is_some();
    let out_pipe = vfs::pipe::pipe_of(&out_file).is_some();
    if !in_pipe && !out_pipe {
        return Err(Errno::EINVAL);
    }
    // 非 seekable 描述符不接受非 NULL offset,否则 ESPIPE。
    if off_in_user != 0 && !in_file.is_seekable() {
        return Err(Errno::ESPIPE);
    }
    if off_out_user != 0 && !out_file.is_seekable() {
        return Err(Errno::ESPIPE);
    }
    let mut in_off = read_optional_offset(off_in_user)?;
    let mut out_off = read_optional_offset(off_out_user)?;
    let copied = copy_between_files(
        ctx,
        &in_file,
        &out_file,
        len,
        &mut in_off,
        &mut out_off,
        (flags & SPLICE_F_NONBLOCK) != 0,
    )?;
    write_optional_offset(off_in_user, in_off)?;
    write_optional_offset(off_out_user, out_off)?;
    Ok(copied)
}

pub(super) fn sys_tee(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd_in = fd_arg(_ctx.args[0])?;
    let fd_out = fd_arg(_ctx.args[1])?;
    let len = _ctx.args[2];
    let flags = _ctx.args[3];
    if (flags & !SPLICE_F_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    let in_file = file_for_fd(fd_in)?;
    let out_file = file_for_fd(fd_out)?;
    // Linux tee(2)：两端都必须是 pipe，否则 EINVAL；且不消费源数据（与 splice
    // 不同）。本内核按已缓冲字节非消费地复制，实现真实 tee 语义。
    let in_pipe = vfs::pipe::pipe_of(&in_file).ok_or(Errno::EINVAL)?;
    let out_pipe = vfs::pipe::pipe_of(&out_file).ok_or(Errno::EINVAL)?;
    let nonblock = (flags & SPLICE_F_NONBLOCK) != 0;
    let mut total = 0usize;
    while total < len {
        match vfs::pipe::Pipe::tee_to(&in_pipe, &out_pipe, len - total) {
            Ok(0) => {
                if total > 0 {
                    return Ok(total);
                }
                if in_pipe.writer_count() == 0 {
                    return Ok(0);
                }
                if nonblock || in_file.flags().nonblock || out_file.flags().nonblock {
                    return Err(Errno::EAGAIN);
                }
                // 源有数据但目标满 → 等目标可写；否则等源可读。
                if in_pipe.available_len() > 0 {
                    wait_for_file_readiness(&out_file, PollEvents::POLLOUT)?;
                } else {
                    wait_for_file_readiness(&in_file, PollEvents::POLLIN)?;
                }
            }
            Ok(n) => total = total.checked_add(n).ok_or(Errno::EINVAL)?,
            Err(Errno::EPIPE) if total > 0 => return Ok(total),
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

pub(super) fn sys_sync_file_range2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const SYNC_FILE_RANGE_WAIT_BEFORE: usize = 1;
    const SYNC_FILE_RANGE_WRITE: usize = 2;
    const SYNC_FILE_RANGE_WAIT_AFTER: usize = 4;
    const SYNC_FILE_RANGE_SUPPORTED: usize =
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER;

    let fd = fd_arg(ctx.args[0])?;
    let flags = ctx.args[1];
    let _offset = nonnegative_i64_arg(ctx.args[2])?;
    let _nbytes = nonnegative_i64_arg(ctx.args[3])?;
    if (flags & !SYNC_FILE_RANGE_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    if flags != 0 {
        file.sync().map_err(|e| e.to_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_acct(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    if !ctx.task().credentials().has_cap(Capability::SysPacct) {
        return Err(Errno::EPERM);
    }
    if ctx.args[0] == 0 {
        crate::acct::disable();
        return Ok(0);
    }

    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = vfs::fdtable::FdTable::new_default();
    let path = copy_path_from_user(ctx.args[0])?;
    let options = OpenOptions {
        access: AccessMode::WriteOnly,
        append: true,
        ..OpenOptions::default()
    };
    let fd = operation::openat(
        &vfs_ctx,
        &fdt,
        &Dirfd::Cwd,
        &path,
        options,
        FileMode::new(0),
    )
    .map_err(|error| error.to_errno())?;
    let Some(file) = fdt.get_file(fd) else {
        let _ = operation::close(&fdt, fd);
        return Err(Errno::EBADF);
    };
    let regular = file.inode().kind() == FileType::Regular;
    operation::close(&fdt, fd).map_err(|error| error.to_errno())?;
    if !regular {
        return Err(Errno::EACCES);
    }
    crate::acct::install(file);
    Ok(0)
}

pub(super) fn sys_fanotify_init(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[0] as u32;
    let event_f_flags = ctx.args[1] as u32;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::fanotify::create_group(&fdt, vfs_ctx.cred(), flags, event_f_flags)?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_fanotify_mark(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let flags = ctx.args[1] as u32;
    let mask = ctx.args[2] as u32;
    let dirfd = ctx.args[3];
    let path_user = ctx.args[4];
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let file = file_for_fd(fd)?;
    let group = vfs::fanotify::group_from_file(&file).ok_or(Errno::EINVAL)?;
    let has_sysadmin = vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin);

    // pathname == NULL → 标记 dirfd 指向的对象本身（inode 作用域）。
    let (inode, mount, sb_id) = if path_user == 0 {
        let df = file_for_fd(fd_arg(dirfd)?)?;
        let inode = Arc::clone(df.inode());
        let sb_id = inode
            .superblock()
            .map(|sb| sb.fs_id.raw() as u64)
            .unwrap_or(0);
        (Some(inode), Some(Arc::clone(df.mount())), sb_id)
    } else {
        let path = copy_path_from_user(path_user)?;
        let dirfd = dirfd_arg(dirfd, &fdt)?;
        let no_follow = (flags & vfs::fanotify::FAN_MARK_DONT_FOLLOW) != 0;
        let onlydir = (flags & vfs::fanotify::FAN_MARK_ONLYDIR) != 0;
        let (inode, mount, sb_id) =
            operation::lookup_for_fanotify(&vfs_ctx, &dirfd, &path, no_follow, onlydir)
                .map_err(|e| e.to_errno())?;
        (Some(inode), Some(mount), sb_id)
    };
    let (i_ref, m_ref) = match (inode, mount) {
        (Some(i), Some(m)) => (Some(i), Some(m)),
        _ => (None, None),
    };
    vfs::fanotify::mark(
        &group,
        flags,
        mask,
        i_ref.as_ref(),
        None,
        m_ref.as_ref(),
        sb_id,
        has_sysadmin,
    )?;
    Ok(0)
}

/// 文件句柄编码：`fs_id`(u64) + `ino`(u64)，共 16 字节。
const FILE_HANDLE_SIZE: usize = 16;
const FILE_HANDLE_TYPE: i32 = 1;

/// `name_to_handle_at(2)`：把路径解析为 inode，导出 (fs_id, ino) 句柄与挂载 id。
pub(super) fn sys_name_to_handle_at(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let handle_user = ctx.args[2];
    let mount_id_user = ctx.args[3];
    let flags = ctx.args[4];
    if (flags & !AT_SYMLINK_NOFOLLOW) != 0 {
        return Err(Errno::EINVAL);
    }
    let result = vfs::path::lookup(
        &vfs_ctx,
        &dirfd,
        &path,
        if (flags & AT_SYMLINK_NOFOLLOW) != 0 {
            LookupFlags::NO_FOLLOW
        } else {
            LookupFlags::default()
        },
    )
    .map_err(|e| e.to_errno())?;
    let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
    let sb_id = inode.superblock().map(|s| s.fs_id.raw()).unwrap_or(0);

    // struct file_handle：handle_bytes(u32) + handle_type(i32) + f_handle[]。
    if handle_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut hdr = [0u8; 8];
    copy_from_user(handle_user, &mut hdr).map_err(|e| e.as_errno())?;
    let capacity = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
    if capacity < FILE_HANDLE_SIZE {
        put_u32(&mut hdr, 0, FILE_HANDLE_SIZE as u32);
        copy_to_user(handle_user, &hdr).map_err(|e| e.as_errno())?;
        return Err(Errno::EOVERFLOW);
    }
    let mut handle = [0u8; FILE_HANDLE_SIZE];
    put_u64(&mut handle, 0, sb_id);
    put_u64(&mut handle, 8, inode.ino());
    copy_to_user(handle_user + 8, &handle).map_err(|e| e.as_errno())?;
    put_u32(&mut hdr, 0, FILE_HANDLE_SIZE as u32);
    put_i32(&mut hdr, 4, FILE_HANDLE_TYPE);
    copy_to_user(handle_user, &hdr).map_err(|e| e.as_errno())?;
    // mount_id 以 superblock 实例 ID 低 32 位近似（本 VFS 无 per-mount id 注册表）。
    if mount_id_user != 0 {
        copy_to_user(mount_id_user, &(sb_id as u32).to_le_bytes()).map_err(|e| e.as_errno())?;
    }
    Ok(0)
}

/// `open_by_handle_at(2)`：按句柄重开文件。mount_fd 是 open_tree/fsmount 产生的
/// 挂载 fd，用于定位句柄所属文件系统实例。
pub(super) fn sys_open_by_handle_at(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let mount_fd = fd_arg(ctx.args[0])?;
    let handle_user = ctx.args[1];
    let flags = ctx.args[2];

    if handle_user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut hdr = [0u8; 8];
    copy_from_user(handle_user, &mut hdr).map_err(|e| e.as_errno())?;
    let handle_bytes = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
    let handle_type = i32::from_le_bytes(hdr[4..8].try_into().unwrap());
    if handle_bytes < FILE_HANDLE_SIZE || handle_type != FILE_HANDLE_TYPE {
        return Err(Errno::EINVAL);
    }
    let mut handle = [0u8; FILE_HANDLE_SIZE];
    copy_from_user(handle_user + 8, &mut handle).map_err(|e| e.as_errno())?;
    let sb_id = u64::from_le_bytes(handle[0..8].try_into().unwrap());
    let ino = u64::from_le_bytes(handle[8..16].try_into().unwrap());

    // 从挂载 fd 取出 superblock 并校验句柄所属文件系统实例。
    let mount_file = fdt.get_file(mount_fd).ok_or(Errno::EBADF)?;
    let fsc = vfs::fs_context::FsContextFileOps::from_file(&mount_file).ok_or(Errno::EINVAL)?;
    let sb = fsc.take_superblock().ok_or(Errno::EINVAL)?;
    if sb.fs_id.raw() != sb_id {
        return Err(Errno::ESTALE);
    }
    let inode = sb.find_inode(ino).ok_or(Errno::ESTALE)?;

    let opts = decode_open_options(flags)?;
    let cred = vfs_ctx.cred().clone();
    let ops = inode.open_ops(&opts, &cred).map_err(|e| e.to_errno())?;
    let mount = fsc
        .clone_root()
        .and_then(|root| vfs_ctx.mount_ns.find_mount_for_root(&root))
        .or_else(|| vfs_ctx.mount_ns.find_mount_for_root(&sb.root_dentry))
        .ok_or(Errno::ESTALE)?;
    // 打开句柄不经过路径，用独立 dentry 承载 inode（仅用于 fd 定位语义）。
    let dentry = vfs::dentry::Dentry::new_positive("", None, Arc::clone(&inode));
    let file = vfs::file::File::new(
        Arc::clone(&inode),
        opts,
        cred,
        ops,
        dentry,
        Arc::clone(&mount),
    );
    mount.inc_open();
    let fd_flags = if opts.cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let fd = fdt
        .alloc_fd(Arc::new(file), fd_flags)
        .map_err(|e| e.to_errno())?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_memfd_create(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let name_user = ctx.args[0];
    let flags = ctx.args[1];
    if (flags & MFD_UNSUPPORTED) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if (flags & (MFD_EXEC | MFD_NOEXEC_SEAL)) == (MFD_EXEC | MFD_NOEXEC_SEAL) {
        return Err(Errno::EINVAL);
    }
    if (flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_UNSUPPORTED | MFD_NOEXEC_SEAL | MFD_EXEC))
        != 0
    {
        return Err(Errno::EINVAL);
    }
    // memfd 名称只用于调试可见性；当前 anonfs 不暴露 /proc/<pid>/fd 名称，但仍
    // 完整校验用户指针和长度，避免无效 ABI 输入被静默接受。
    let _name = copy_cstr_from_user(name_user, 249).map_err(|e| e.as_errno())?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::memfd::create_ext(
        &fdt,
        vfs_ctx.cred(),
        (flags & MFD_ALLOW_SEALING) != 0,
        (flags & MFD_CLOEXEC) != 0,
        (flags & MFD_NOEXEC_SEAL) != 0,
    )?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_preadv2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[5];
    if (flags & !RWF_SUPPORTED) != 0 || (flags & RWF_APPEND) != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let offset = split_offset_arg(ctx.args[3], ctx.args[4])?;
    let file = file_for_fd(fd)?;
    // RWF_NOWAIT：遇到会阻塞的 I/O 时返回 EAGAIN 而非睡眠；RWF_HIPRI 在本内核
    // 无轮询队列，作为性能 hint 接受但无额外效果（Linux 上同为 best-effort）。
    read_iovecs(ctx, &file, iov, iovcnt, offset, (flags & RWF_NOWAIT) != 0)
}

pub(super) fn sys_pwritev2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[5];
    if (flags & !RWF_SUPPORTED) != 0 || ((flags & RWF_APPEND) != 0 && (flags & RWF_NOAPPEND) != 0) {
        return Err(Errno::EOPNOTSUPP);
    }
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let offset = if (flags & RWF_APPEND) != 0 {
        Some(u64::MAX)
    } else {
        split_offset_arg(ctx.args[3], ctx.args[4])?
    };
    let file = file_for_fd(fd)?;
    let written = write_iovecs(ctx, &file, iov, iovcnt, offset, (flags & RWF_NOWAIT) != 0)?;
    if (flags & (RWF_DSYNC | RWF_SYNC)) != 0 {
        file.sync().map_err(|e| e.to_errno())?;
    }
    Ok(written)
}

pub(super) fn sys_timerfd_gettime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timerfd_gettime_common(ctx)
}

pub(super) fn sys_timerfd_settime64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    timerfd_settime_common(ctx)
}

pub(super) fn sys_utimensat_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_utimensat(ctx)
}

pub(super) fn sys_pselect6_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_pselect6(ctx)
}

pub(super) fn sys_ppoll_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_ppoll(ctx)
}

pub(super) fn sys_recvmmsg_time64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    sys_recvmmsg(ctx)
}

// ── io_uring 最小同步实现 ────────────────────────────────────────────────────
//
// 提供“固定 SQ/CQ 队列 + 同步执行 SQE”的最小闭环：io_uring_setup 分配并映射
// 一个共享匿名内存对象作为三块 ring（SQ ring + SQE 数组 + CQ ring），io_uring_enter
// 同步执行 NOP/READ/WRITE/READV/WRITEV/FSYNC，io_uring_register 仅识别清理命令。
// ring 通过 `SharedAnonObject` 在 setup 时映射进用户地址空间（基址写入
// `io_uring_params.sq_off.user_addr`），内核侧经 read/write_shared_anon 访问同一
// 物理页，保证 head/tail 索引相干。这是自洽的最小闭环，但**不兼容标准 liburing**
// （其自行 mmap(fd, IORING_OFF_*) 会得到写回式文件页而非本共享对象）。

const IORING_SQE_SIZE: usize = 64;
const IORING_CQE_SIZE: usize = 16;
const IORING_MAX_ENTRIES: u32 = 4096;

const IORING_OP_NOP: u8 = 0;
const IORING_OP_READV: u8 = 1;
const IORING_OP_WRITEV: u8 = 2;
const IORING_OP_FSYNC: u8 = 3;
const IORING_OP_READ: u8 = 22;
const IORING_OP_WRITE: u8 = 23;

const IORING_UNREGISTER_BUFFERS: u32 = 1;
const IORING_UNREGISTER_FILES: u32 = 3;

struct IoUringState {
    object: Arc<mm::SharedAnonObject>,
    sq_entries: u32,
    cq_entries: u32,
    sq_ring_size: usize,
    sqes_size: usize,
    cq_ring_size: usize,
    sq_ring_off: u64,
    sqes_off: u64,
    cq_ring_off: u64,
}

impl IoUringState {
    fn new(entries: u32) -> Self {
        let sq_ring_size = 24 + entries as usize * 4;
        let sqes_size = entries as usize * IORING_SQE_SIZE;
        let cq_ring_size = 20 + entries as usize * IORING_CQE_SIZE;
        let sq_ring_off = 0u64;
        let sqes_off = sq_ring_size as u64;
        let cq_ring_off = sqes_off + sqes_size as u64;
        Self {
            object: Arc::new(mm::SharedAnonObject::new()),
            sq_entries: entries,
            cq_entries: entries,
            sq_ring_size,
            sqes_size,
            cq_ring_size,
            sq_ring_off,
            sqes_off,
            cq_ring_off,
        }
    }

    fn total_size(&self) -> usize {
        self.sq_ring_size + self.sqes_size + self.cq_ring_size
    }

    fn read_u32(&self, off: u64) -> Result<u32, Errno> {
        let mut b = [0u8; 4];
        general::mm::read_shared_anon(&self.object, off, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn write_u32(&self, off: u64, v: u32) -> Result<(), Errno> {
        general::mm::write_shared_anon(&self.object, off, &v.to_le_bytes())
    }

    fn read_bytes(&self, off: u64, buf: &mut [u8]) -> Result<(), Errno> {
        general::mm::read_shared_anon(&self.object, off, buf)
    }

    fn write_bytes(&self, off: u64, buf: &[u8]) -> Result<(), Errno> {
        general::mm::write_shared_anon(&self.object, off, buf)
    }

    fn init_ring(&self) -> Result<(), Errno> {
        let mask = self.sq_entries - 1;
        self.write_u32(self.sq_ring_off, 0)?;
        self.write_u32(self.sq_ring_off + 4, 0)?;
        self.write_u32(self.sq_ring_off + 8, mask)?;
        self.write_u32(self.sq_ring_off + 12, self.sq_entries)?;
        self.write_u32(self.sq_ring_off + 16, 0)?;
        self.write_u32(self.sq_ring_off + 20, 0)?;
        self.write_u32(self.cq_ring_off, 0)?;
        self.write_u32(self.cq_ring_off + 4, 0)?;
        self.write_u32(self.cq_ring_off + 8, mask)?;
        self.write_u32(self.cq_ring_off + 12, self.cq_entries)?;
        self.write_u32(self.cq_ring_off + 16, 0)?;
        Ok(())
    }
}

struct IoUringFileOps {
    state: Arc<IoUringState>,
}

impl vfs::file::FileOps for IoUringFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> vfs::error::VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> vfs::error::VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(vfs::file::DirEntry) -> ControlFlow<()>,
    ) -> vfs::error::VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> vfs::error::VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents::default()
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 同步执行单个 SQE，返回 CQE 的 res 值（错误编码为负 errno）。
fn execute_uring_sqe(ctx: &mut SyscallContext<'_>, sqe: &[u8]) -> i32 {
    let opcode = sqe[0];
    let fd_raw = i32::from_le_bytes(sqe[4..8].try_into().unwrap());
    let off = u64::from_le_bytes(sqe[8..16].try_into().unwrap());
    let addr = u64::from_le_bytes(sqe[16..24].try_into().unwrap());
    let len = u32::from_le_bytes(sqe[24..28].try_into().unwrap());
    if opcode == IORING_OP_NOP {
        return 0;
    }
    let fd = match fd_arg(fd_raw as usize) {
        Ok(fd) => fd,
        Err(e) => return -i32::from(e),
    };
    let res: Result<usize, Errno> = match opcode {
        IORING_OP_READ => {
            let file = match file_for_fd(fd) {
                Ok(f) => f,
                Err(e) => return -i32::from(e),
            };
            read_to_user(&file, addr as usize, len as usize, Some(off), false)
        }
        IORING_OP_WRITE => {
            let file = match file_for_fd(fd) {
                Ok(f) => f,
                Err(e) => return -i32::from(e),
            };
            write_from_user_at(&file, addr as usize, len as usize, Some(off), false)
        }
        IORING_OP_READV => {
            let file = match file_for_fd(fd) {
                Ok(f) => f,
                Err(e) => return -i32::from(e),
            };
            read_iovecs(ctx, &file, addr as usize, len as usize, Some(off), false)
        }
        IORING_OP_WRITEV => {
            let file = match file_for_fd(fd) {
                Ok(f) => f,
                Err(e) => return -i32::from(e),
            };
            write_iovecs(ctx, &file, addr as usize, len as usize, Some(off), false)
        }
        IORING_OP_FSYNC => {
            let file = match file_for_fd(fd) {
                Ok(f) => f,
                Err(e) => return -i32::from(e),
            };
            match file.sync() {
                Ok(()) => return 0,
                Err(e) => return -i32::from(e.to_errno()),
            }
        }
        _ => return -i32::from(Errno::EINVAL),
    };
    match res {
        Ok(n) => n as i32,
        Err(e) => -i32::from(e),
    }
}

pub(super) fn sys_io_uring_setup(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let entries = ctx.args[0] as u32;
    let params_user = ctx.args[1];
    if entries == 0 || entries > IORING_MAX_ENTRIES || params_user == 0 {
        return Err(Errno::EINVAL);
    }
    let entries = entries.next_power_of_two();
    let state = Arc::new(IoUringState::new(entries));
    state.init_ring()?;

    // 将 ring 映射进当前进程地址空间（共享匿名对象，内核/用户相干）。
    let vm = current_vm_space().ok_or(Errno::ENOMEM)?;
    let range = vm.alloc_mmap_range(state.total_size())?;
    let vflags = mm::VmFlags::from_bits(mm::VmFlags::USER | mm::VmFlags::READ | mm::VmFlags::WRITE);
    vm.map_shared_anon(range.clone(), Arc::clone(&state.object), 0, vflags)?;
    let user_base = range.start;

    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let file_flags = OpenOptions {
        access: AccessMode::ReadWrite,
        ..OpenOptions::default()
    };
    let fd = vfs::anon::create_fd(
        &fdt,
        vfs_ctx.cred().clone(),
        file_flags,
        FdFlags::default(),
        alloc::boxed::Box::new(IoUringFileOps {
            state: Arc::clone(&state),
        }),
    )
    .map_err(|e| e.to_errno())?;

    // struct io_uring_params（128 字节）：sq_off 在偏移 40，cq_off 在偏移 80。
    let mut params = [0u8; 128];
    put_u32(&mut params, 0, entries);
    put_u32(&mut params, 4, entries);
    // sq_off
    put_u32(&mut params, 40, 0);
    put_u32(&mut params, 44, 4);
    put_u32(&mut params, 48, 8);
    put_u32(&mut params, 52, 12);
    put_u32(&mut params, 56, 16);
    put_u32(&mut params, 60, 20);
    put_u32(&mut params, 64, 24);
    put_u64(&mut params, 72, user_base as u64);
    // cq_off
    put_u32(&mut params, 80, 0);
    put_u32(&mut params, 84, 4);
    put_u32(&mut params, 88, 8);
    put_u32(&mut params, 92, 12);
    put_u32(&mut params, 96, 16);
    put_u32(&mut params, 100, 20);
    put_u32(&mut params, 104, 24);
    put_u64(
        &mut params,
        112,
        (user_base + state.cq_ring_off as usize) as u64,
    );
    copy_to_user(params_user, &params).map_err(|e| e.as_errno())?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_io_uring_enter(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let to_submit = ctx.args[1] as u32;
    let file = file_for_fd(fd)?;
    let ring = file.downcast_ops::<IoUringFileOps>().ok_or(Errno::EBADF)?;
    let st = &ring.state;

    let mask = st.read_u32(st.sq_ring_off + 8)?;
    let mut sq_head = st.read_u32(st.sq_ring_off)?;
    let sq_tail = st.read_u32(st.sq_ring_off + 4)?;
    let mut cq_tail = st.read_u32(st.cq_ring_off + 4)?;
    let mut completed = 0u32;

    while completed < to_submit && sq_head != sq_tail {
        let array_off = st.sq_ring_off + 24 + (sq_head & mask) as u64 * 4;
        let sqe_index = st.read_u32(array_off)?;
        let sqe_off = st.sqes_off + sqe_index as u64 * IORING_SQE_SIZE as u64;
        let mut sqe = [0u8; IORING_SQE_SIZE];
        st.read_bytes(sqe_off, &mut sqe)?;
        let res = execute_uring_sqe(ctx, &sqe);
        let user_data = u64::from_le_bytes(sqe[32..40].try_into().unwrap());
        let mut cqe = [0u8; IORING_CQE_SIZE];
        put_u64(&mut cqe, 0, user_data);
        put_i32(&mut cqe, 8, res);
        let cqe_off = st.cq_ring_off + 20 + (cq_tail & mask) as u64 * IORING_CQE_SIZE as u64;
        st.write_bytes(cqe_off, &cqe)?;
        cq_tail = cq_tail.wrapping_add(1);
        sq_head = sq_head.wrapping_add(1);
        completed += 1;
    }
    st.write_u32(st.sq_ring_off, sq_head)?;
    st.write_u32(st.cq_ring_off + 4, cq_tail)?;
    Ok(completed as usize)
}

pub(super) fn sys_io_uring_register(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let opcode = ctx.args[1] as u32;
    let file = file_for_fd(fd)?;
    file.downcast_ops::<IoUringFileOps>().ok_or(Errno::EBADF)?;
    // 最小语义：固定文件/缓冲注册表未实现；仅接受清理命令为 no-op，便于
    // 用户态 teardown 路径不因 EOPNOTSUPP 中断。
    match opcode {
        IORING_UNREGISTER_BUFFERS | IORING_UNREGISTER_FILES => Ok(0),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

/// `fsopen(2)`：按文件系统类型创建 fs_context 并返回其 fd。
pub(super) fn sys_fsopen(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fs_name = copy_cstr_from_user(ctx.args[0], 64).map_err(|e| e.as_errno())?;
    let flags = ctx.args[1] as u32;
    if flags & !vfs::fs_context::FSOPEN_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    if vfs::FS_REGISTRY.find(&fs_name).is_none() {
        return Err(Errno::ENODEV);
    }
    let fsc = vfs::fs_context::FsContext::new(fs_name);
    let fd = vfs::fs_context::create_fs_context_fd(
        &fdt,
        vfs_ctx.cred(),
        fsc,
        (flags & vfs::fs_context::FSOPEN_CLOEXEC) != 0,
    )?;
    Ok(fd.as_raw() as usize)
}

/// `fsconfig(2)`：配置 fs_context（SET_FLAG / SET_STRING / CMD_CREATE）。
pub(super) fn sys_fsconfig(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let fd = fd_arg(ctx.args[0])?;
    let cmd = ctx.args[1] as u32;
    let file = file_for_fd(fd)?;
    let fsc = vfs::fs_context::FsContextFileOps::from_file(&file).ok_or(Errno::EBADF)?;
    if fsc.is_consumed() {
        return Err(Errno::EBADF);
    }
    match cmd {
        vfs::fs_context::FSCONFIG_SET_FLAG => {
            let key = copy_cstr_from_user(ctx.args[2], 64).map_err(|e| e.as_errno())?;
            fsc.set_flag(&key)?;
        }
        vfs::fs_context::FSCONFIG_SET_STRING => {
            let key = copy_cstr_from_user(ctx.args[2], 64).map_err(|e| e.as_errno())?;
            let value = copy_optional_cstr_from_user(ctx.args[3], 4096)?;
            fsc.set_string(&key, &value)?;
        }
        vfs::fs_context::FSCONFIG_CMD_CREATE => {
            fsc.create_superblock()?;
        }
        vfs::fs_context::FSCONFIG_CMD_RECONFIGURE => {
            // fspick 得到的 fs_context 携带既有 superblock；RECONFIGURE 把累计的
            // 挂载标志重新应用到该 superblock（Linux remount 语义）。
            let sb = fsc.take_superblock().ok_or(Errno::EINVAL)?;
            let flags = fsc.flags();
            let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
            let mount = fsc
                .clone_root()
                .and_then(|root| vfs_ctx.mount_ns.find_mount_for_root(&root));
            match mount {
                Some(m) => {
                    m.superblock.remount(flags).map_err(|e| e.to_errno())?;
                    m.set_flags(flags);
                }
                None => {
                    sb.remount(flags).map_err(|e| e.to_errno())?;
                }
            }
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

/// 把 Linux `MOUNT_ATTR_*` 属性位映射到 fs_context 的 MountFlags（`fsmount` 与
/// `mount_setattr` 共用）。不支持的位返回 EOPNOTSUPP。
fn apply_mount_attr_flags(
    fsc: &vfs::fs_context::FsContext,
    attr_flags: usize,
) -> Result<(), Errno> {
    if attr_flags & !MOUNT_ATTR_SUPPORTED != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    for (bit, key) in [
        (MOUNT_ATTR_RDONLY, "ro"),
        (MOUNT_ATTR_NOSUID, "nosuid"),
        (MOUNT_ATTR_NODEV, "nodev"),
        (MOUNT_ATTR_NOEXEC, "noexec"),
        (MOUNT_ATTR_NOATIME, "noatime"),
        (MOUNT_ATTR_NODIRATIME, "nodiratime"),
    ] {
        if attr_flags & bit != 0 {
            fsc.set_flag(key)?;
        }
    }
    Ok(())
}

/// `fsmount(2)`：校验 fs_context 已 CREATE，应用 MOUNT_ATTR_* 属性，标记挂载
/// 就绪并返回挂载 fd。
pub(super) fn sys_fsmount(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let fd = fd_arg(ctx.args[0])?;
    let flags = ctx.args[1] as u32;
    let attr_flags = ctx.args[2];
    if flags & !vfs::fs_context::FSMOUNT_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    let fsc = vfs::fs_context::FsContextFileOps::from_file(&file).ok_or(Errno::EBADF)?;
    if fsc.is_consumed() {
        return Err(Errno::EBADF);
    }
    apply_mount_attr_flags(&fsc, attr_flags)?;
    // fsmount 必须返回一个全新的 fd（不再复用 fsopen 传入的 fd）。这里派生
    // 出挂载就绪的新上下文，分配新 fd 成功后消费原 fsopen 上下文。
    let mount_ctx = fsc.derive_mount_context()?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let new_fd = vfs::fs_context::create_fs_context_fd(
        &fdt,
        vfs_ctx.cred(),
        mount_ctx,
        (flags & vfs::fs_context::FSMOUNT_CLOEXEC) != 0,
    )?;
    fsc.mark_consumed();
    Ok(new_fd.as_raw() as usize)
}

/// `move_mount(2)`：MOVE_MOUNT_F_EMPTY_PATH 时把 fs_context 挂载落到目标
/// 路径；否则把 from 路径上的挂载迁移到 to。
pub(super) fn sys_move_mount(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let from_fd = ctx.args[0];
    let from_path_user = ctx.args[1];
    let to_fd = ctx.args[2];
    let to_path_user = ctx.args[3];
    let flags = ctx.args[4] as u32;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;

    if flags & vfs::fs_context::MOVE_MOUNT_F_EMPTY_PATH != 0 {
        // fromfd 是 fsmount 后的 fs_context fd（或 open_tree 克隆 fd）。
        let file = file_for_fd(fd_arg(from_fd)?)?;
        let fsc = vfs::fs_context::FsContextFileOps::from_file(&file).ok_or(Errno::EBADF)?;
        if !fsc.is_mount_ready() && fsc.clone_root().is_none() {
            return Err(Errno::EINVAL);
        }
        let to_path = copy_path_from_user(to_path_user)?;
        let to_dirfd = dirfd_arg(to_fd, &fdt)?;
        let target = vfs::path::lookup(
            &vfs_ctx,
            &to_dirfd,
            &to_path,
            LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
        )
        .map_err(|e| e.to_errno())?;
        vfs::fs_context::land_mount(
            &vfs_ctx.mount_ns,
            &fsc,
            Arc::clone(&target.dentry),
            &target.mount,
        )
        .map_err(|e| e.to_errno())?;
        return Ok(0);
    }

    // 普通路径迁移（mount --move 语义）。
    let from_path = copy_path_from_user(from_path_user)?;
    let to_path = copy_path_from_user(to_path_user)?;
    let from_dirfd = dirfd_arg(from_fd, &fdt)?;
    let to_dirfd = dirfd_arg(to_fd, &fdt)?;
    let src = vfs::path::lookup(
        &vfs_ctx,
        &from_dirfd,
        &from_path,
        LookupFlags::NO_MOUNT_LAST,
    )
    .map_err(|e| e.to_errno())?;
    let m = vfs_ctx
        .mount_ns
        .lookup_mount(&src.dentry)
        .ok_or(Errno::ENOENT)?;
    let dst = vfs::path::lookup(
        &vfs_ctx,
        &to_dirfd,
        &to_path,
        LookupFlags::DIRECTORY.with(LookupFlags::NO_MOUNT_LAST),
    )
    .map_err(|e| e.to_errno())?;
    vfs_ctx
        .mount_ns
        .move_mount_at(&m, Arc::clone(&dst.dentry), Arc::clone(&dst.mount))
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

/// `open_tree(2)`：OPEN_TREE_CLONE 时创建目标挂载的克隆上下文 fd
/// （move_mount 可将其挂到新位置）。
pub(super) fn sys_open_tree(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    open_tree_common(ctx.args[0], ctx.args[1], ctx.args[2] as u32)
}

fn open_tree_common(dirfd_raw: usize, path_user: usize, flags: u32) -> Result<usize, Errno> {
    // asm-generic（LoongArch/RISC-V）O_CLOEXEC=0x80000；x86 为 0o200000。
    const OPEN_TREE_CLOEXEC_ANY: u32 = 0o200000 | 0x80000;
    if flags & !(vfs::fs_context::OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC_ANY) != 0 {
        return Err(Errno::EINVAL);
    }
    let cloexec = (flags & (0o200000 | 0x80000)) != 0;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let path = copy_path_from_user(path_user)?;
    let dirfd = dirfd_arg(dirfd_raw, &fdt)?;
    let result = vfs::path::lookup(&vfs_ctx, &dirfd, &path, LookupFlags::DIRECTORY)
        .map_err(|e| e.to_errno())?;
    // 目标路径所在挂载（若路径本身是挂载点则取覆盖其上的挂载）。
    let m = match vfs_ctx.mount_ns.lookup_mount(&result.dentry) {
        Some(m) => m,
        None => Arc::clone(&result.mount),
    };
    let fsc = vfs::fs_context::FsContext::from_mount(&m);
    let fd = vfs::fs_context::create_fs_context_fd(&fdt, vfs_ctx.cred(), fsc, cloexec)?;
    Ok(fd.as_raw() as usize)
}

/// `fspick(2)`：从已挂载路径创建 fs_context（供重新配置）。
pub(super) fn sys_fspick(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let dirfd = ctx.args[0];
    let path_user = ctx.args[1];
    let flags = ctx.args[2] as u32;
    const FSPICK_CLOEXEC: u32 = 1;
    const FSPICK_EMPTY_PATH: u32 = 8;
    if flags & !(FSPICK_CLOEXEC | FSPICK_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let path = copy_path_from_user(path_user)?;
    let dirfd = dirfd_arg(dirfd, &fdt)?;
    let result = vfs::path::lookup(&vfs_ctx, &dirfd, &path, LookupFlags::DIRECTORY)
        .map_err(|e| e.to_errno())?;
    let m = match vfs_ctx.mount_ns.lookup_mount(&result.dentry) {
        Some(m) => m,
        None => Arc::clone(&result.mount),
    };
    let fsc = vfs::fs_context::FsContext::from_mount(&m);
    let fd = vfs::fs_context::create_fs_context_fd(
        &fdt,
        vfs_ctx.cred(),
        fsc,
        (flags & FSPICK_CLOEXEC) != 0,
    )?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_openat2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let how = read_open_how(ctx.args[2], ctx.args[3])?;
    const RESOLVE_SUPPORTED: u64 = RESOLVE_NO_MAGICLINKS
        | RESOLVE_NO_SYMLINKS
        | RESOLVE_BENEATH
        | RESOLVE_IN_ROOT
        | RESOLVE_CACHED;
    if (how.resolve & !RESOLVE_SUPPORTED) != 0 {
        return Err(Errno::EINVAL);
    }
    // RESOLVE_BENEATH / RESOLVE_IN_ROOT 要求以 dirfd 为解析边界。
    let beneath = (how.resolve & RESOLVE_BENEATH) != 0;
    let in_root = (how.resolve & RESOLVE_IN_ROOT) != 0;
    let raw_flags = usize::try_from(how.flags).map_err(|_| Errno::EINVAL)?;
    validate_openat2_flags(raw_flags)?;
    let flags = decode_open_options(raw_flags)?;
    if how.mode != 0 && (raw_flags & O_CREAT) == 0 {
        return Err(Errno::EINVAL);
    }
    if (how.mode & !0o7777) != 0 {
        return Err(Errno::EINVAL);
    }
    let mode = FileMode::new((how.mode & 0o7777) as u16);
    let mut lookup_flags = LookupFlags::default();
    if (how.resolve & RESOLVE_NO_SYMLINKS) != 0 {
        lookup_flags = lookup_flags
            .with(LookupFlags::NO_SYMLINKS)
            .with(LookupFlags::NO_FOLLOW);
    }
    // BENEATH/IN_ROOT 的越界处理：BENEATH 拒绝绝对路径；IN_ROOT 把绝对路径当
    // 作相对 dirfd 解析。路径解析器对 ".." 的钳制以进程根为界，这里用 Dentry
    // 祖先校验兜底（越界 → EXDEV，Linux 语义）。
    let resolved_path = if vfs::path::PathComponents::is_absolute(&path) {
        if beneath {
            return Err(Errno::EXDEV);
        }
        if in_root {
            let stripped = path.trim_start_matches('/');
            if stripped.is_empty() {
                // "/" 在 IN_ROOT 下等价于 dirfd 本身。
                "."
            } else {
                stripped
            }
        } else {
            path.as_str()
        }
    } else {
        path.as_str()
    };
    let fd = operation::openat_with_lookup_flags(
        &vfs_ctx,
        &fdt,
        &dirfd,
        resolved_path,
        flags,
        mode,
        lookup_flags,
    )
    .map_err(|e| e.to_errno())?;
    // BENEATH/IN_ROOT 的解析后祖先校验（相对路径经 ".."/符号链接可能逃出 dirfd）。
    if beneath || in_root {
        let dirfd_root = match &dirfd {
            Dirfd::Cwd => vfs_ctx.cwd(),
            Dirfd::Fd(f) => f.dentry().clone(),
        };
        let resolved = fdt.get_file(fd).ok_or(Errno::EBADF)?.dentry().clone();
        if !resolved.is_descendant_of(&dirfd_root) {
            let _ = operation::close(&fdt, fd);
            return Err(Errno::EXDEV);
        }
    }
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_pidfd_getfd(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let pidfd = fd_arg(ctx.args[0])?;
    let targetfd = fd_arg(ctx.args[1])?;
    let flags = ctx.args[2];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let pid_file = fdt.get_file(pidfd).ok_or(Errno::EBADF)?;
    let target_group = pidfd::group_from_file(&pid_file).ok_or(Errno::EINVAL)?;
    let target = target_group
        .with_running_leader(|leader| (task_may_access(ctx.task(), leader), task_fdtable(leader)))
        .ok_or_else(|| {
            if target_group.is_terminated() {
                Errno::ESRCH
            } else {
                Errno::EAGAIN
            }
        })?;
    if !target.0 {
        return Err(Errno::EPERM);
    }
    let target_fdt = target.1.ok_or(Errno::EBADF)?;
    let file = target_fdt.get_file(targetfd).ok_or(Errno::EBADF)?;
    let new_fd = fdt
        .alloc_fd(file, FdFlags::CLOEXEC)
        .map_err(|err| err.to_errno())?;
    Ok(new_fd.as_raw() as usize)
}

pub(super) fn sys_epoll_pwait2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let timeout_base_ns = sched::now_ns_direct();
    let epfd = fd_arg(ctx.args[0])?;
    let events_user = ctx.args[1];
    let maxevents = ctx.args[2] as i32;
    if maxevents <= 0 {
        return Err(Errno::EINVAL);
    }
    let timeout_ns = read_timespec_timeout_ns(ctx.args[3])?;
    let deadline = timeout_ns.map(|timeout_ns| timeout_base_ns.saturating_add(timeout_ns));
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let sigmask = read_direct_sigmask(ctx.args[4], ctx.args[5])?;
    let _mask_guard = TemporarySigmask::install(sigmask);
    let ready = vfs::epoll::wait_until(&fdt, epfd, maxevents as usize, deadline)?;
    write_epoll_events(events_user, &ready)?;
    Ok(ready.len())
}

/// `mount_setattr(2)`：批量修改挂载属性（只读/访问约束位 + 传播类型）。
pub(super) fn sys_mount_setattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    if !vfs_ctx.cred().has_cap(vfs::cred::Capability::SysAdmin) {
        return Err(Errno::EPERM);
    }
    let dfd = ctx.args[0];
    let path = copy_path_from_user(ctx.args[1])?;
    let flags = ctx.args[2];
    let attr_user = ctx.args[3];
    let size = ctx.args[4];
    const MOUNT_ATTR_SIZE_VER0: usize = 32;
    if size < MOUNT_ATTR_SIZE_VER0 {
        return Err(Errno::EINVAL);
    }
    if (flags & !(AT_EMPTY_PATH | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW)) != 0 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 32];
    copy_from_user(attr_user, &mut raw).map_err(|e| e.as_errno())?;
    let attr_set = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    let attr_clr = u64::from_le_bytes(raw[8..16].try_into().unwrap());
    let propagation = u64::from_le_bytes(raw[16..24].try_into().unwrap());
    let userns_fd = u64::from_le_bytes(raw[24..32].try_into().unwrap());
    if userns_fd != 0 {
        // 无 userns idmap 挂载支持。
        return Err(Errno::EOPNOTSUPP);
    }
    if (attr_set & !MOUNT_ATTR_SUPPORTED as u64) != 0
        || (attr_clr & !MOUNT_ATTR_SUPPORTED as u64) != 0
    {
        return Err(Errno::EOPNOTSUPP);
    }

    // 定位目标挂载：AT_EMPTY_PATH + 空路径表示 dfd 即 open_tree/fsmount 挂载 fd。
    let mount = if (flags & AT_EMPTY_PATH) != 0 && path.is_empty() {
        let file = fdt.get_file(fd_arg(dfd)?).ok_or(Errno::EBADF)?;
        let fsc = vfs::fs_context::FsContextFileOps::from_file(&file).ok_or(Errno::EINVAL)?;
        let root = fsc.clone_root().ok_or(Errno::EINVAL)?;
        vfs_ctx
            .mount_ns
            .find_mount_for_root(&root)
            .ok_or(Errno::EINVAL)?
    } else {
        let dirfd = dirfd_arg(dfd, &fdt)?;
        let r = vfs::path::lookup(&vfs_ctx, &dirfd, &path, LookupFlags::NO_MOUNT_LAST)
            .map_err(|e| e.to_errno())?;
        vfs_ctx
            .mount_ns
            .lookup_mount(&r.dentry)
            .ok_or(Errno::EINVAL)?
    };

    let rec = (flags & AT_RECURSIVE) != 0;
    apply_mount_setattr_one(&mount, attr_set, attr_clr, propagation)?;
    if rec {
        let children: Vec<Arc<vfs::mount::Mount>> = mount.children.lock().clone();
        for child in children {
            apply_mount_setattr_recursive(&child, attr_set, attr_clr, propagation)?;
        }
    }
    Ok(0)
}

fn apply_mount_setattr_one(
    mount: &Arc<vfs::mount::Mount>,
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
) -> Result<(), Errno> {
    let mut flags = mount.flags_snapshot();
    for (bit, flag) in [
        (MOUNT_ATTR_RDONLY as u64, MountFlags::RDONLY),
        (MOUNT_ATTR_NOSUID as u64, MountFlags::NOSUID),
        (MOUNT_ATTR_NODEV as u64, MountFlags::NODEV),
        (MOUNT_ATTR_NOEXEC as u64, MountFlags::NOEXEC),
        (MOUNT_ATTR_NOATIME as u64, MountFlags::NOATIME),
        (MOUNT_ATTR_NODIRATIME as u64, MountFlags::NODIRATIME),
    ] {
        if attr_set & bit != 0 {
            flags = flags.with(flag);
        }
        if attr_clr & bit != 0 {
            flags = flags.without(flag);
        }
    }
    mount.superblock.remount(flags).map_err(|e| e.to_errno())?;
    mount.set_flags(flags);

    if propagation != 0 {
        let kind = match propagation as usize {
            MS_SHARED => vfs::mount::PROP_SHARED,
            MS_PRIVATE => vfs::mount::PROP_PRIVATE,
            MS_SLAVE => vfs::mount::PROP_SLAVE,
            MS_UNBINDABLE => vfs::mount::PROP_UNBINDABLE,
            _ => return Err(Errno::EINVAL),
        };
        vfs::mount::set_mount_propagation(mount, kind);
    }
    Ok(())
}

fn apply_mount_setattr_recursive(
    mount: &Arc<vfs::mount::Mount>,
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
) -> Result<(), Errno> {
    apply_mount_setattr_one(mount, attr_set, attr_clr, propagation)?;
    let children: Vec<Arc<vfs::mount::Mount>> = mount.children.lock().clone();
    for child in children {
        apply_mount_setattr_recursive(&child, attr_set, attr_clr, propagation)?;
    }
    Ok(())
}

pub(super) fn sys_quotactl_fd(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_fchmodat2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_path_from_user(ctx.args[1])?;
    let mode = FileMode::new((ctx.args[2] & 0o7777) as u16);
    let flags = ctx.args[3];
    if (flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)) != 0 {
        return Err(Errno::EINVAL);
    }
    if path.is_empty() {
        if (flags & AT_EMPTY_PATH) == 0 {
            return Err(Errno::ENOENT);
        }
        match dirfd {
            Dirfd::Fd(_) => {
                let fd = fd_arg(ctx.args[0])?;
                operation::fchmod(&vfs_ctx, &fdt, fd, mode).map_err(|e| e.to_errno())?;
            }
            Dirfd::Cwd => {
                operation::fchmodat(&vfs_ctx, &Dirfd::Cwd, ".", mode, false)
                    .map_err(|e| e.to_errno())?;
            }
        }
        return Ok(0);
    }
    operation::fchmodat(
        &vfs_ctx,
        &dirfd,
        &path,
        mode,
        (flags & AT_SYMLINK_NOFOLLOW) != 0,
    )
    .map_err(|e| e.to_errno())?;
    Ok(0)
}

/// statmount(2) 的 STATMOUNT_* mask 位（Linux uapi/linux/mount.h）。
const STATMOUNT_SB_BASIC: u64 = 0x0000_0001;
const STATMOUNT_MNT_BASIC: u64 = 0x0000_0002;
const STATMOUNT_MNT_ROOT: u64 = 0x0000_0008;
const STATMOUNT_MNT_POINT: u64 = 0x0000_0010;
const STATMOUNT_FS_TYPE: u64 = 0x0000_0020;
const STATMOUNT_MNT_NS_ID: u64 = 0x0000_0040;
/// `struct statmount` 固定头大小（字符串区在其后）。
const STATMOUNT_HEADER_SIZE: usize = 512;

struct MountSnapEntry {
    mount: Arc<vfs::mount::Mount>,
    id: u64,
    parent_id: u64,
}

/// 深度优先（前序）枚举当前命名空间的挂载树，并分配稳定挂载 id（1 起）。
fn mount_snapshot(vfs_ctx: &Arc<vfs::VfsContext>) -> Vec<MountSnapEntry> {
    let root = vfs_ctx.mount_ns.root.lock().clone();
    let mut order: Vec<Arc<vfs::mount::Mount>> = Vec::new();
    let mut stack = vec![Arc::clone(&root)];
    while let Some(m) = stack.pop() {
        order.push(Arc::clone(&m));
        let children = m.children.lock().clone();
        for c in children.into_iter().rev() {
            stack.push(c);
        }
    }
    let mut id_by_ptr: alloc::collections::BTreeMap<usize, u64> =
        alloc::collections::BTreeMap::new();
    for (idx, m) in order.iter().enumerate() {
        id_by_ptr.insert(Arc::as_ptr(m) as usize, idx as u64 + 1);
    }
    order
        .into_iter()
        .map(|m| {
            let parent_id = m
                .location
                .lock()
                .parent
                .as_ref()
                .and_then(|w| w.upgrade())
                .and_then(|p| id_by_ptr.get(&(Arc::as_ptr(&p) as usize)).copied())
                .unwrap_or(1);
            let id = id_by_ptr[&(Arc::as_ptr(&m) as usize)];
            MountSnapEntry {
                mount: m,
                id,
                parent_id,
            }
        })
        .collect()
}

fn mount_flags_to_mount_attr(flags: MountFlags) -> u64 {
    let mut attr = 0u64;
    if flags.has(MountFlags::RDONLY) {
        attr |= MOUNT_ATTR_RDONLY as u64;
    }
    if flags.has(MountFlags::NOSUID) {
        attr |= MOUNT_ATTR_NOSUID as u64;
    }
    if flags.has(MountFlags::NODEV) {
        attr |= MOUNT_ATTR_NODEV as u64;
    }
    if flags.has(MountFlags::NOEXEC) {
        attr |= MOUNT_ATTR_NOEXEC as u64;
    }
    if flags.has(MountFlags::NOATIME) {
        attr |= MOUNT_ATTR_NOATIME as u64;
    }
    if flags.has(MountFlags::NODIRATIME) {
        attr |= MOUNT_ATTR_NODIRATIME as u64;
    }
    attr
}

fn mount_propagation_to_ms(prop: u32) -> u64 {
    match prop {
        vfs::mount::PROP_SHARED => MS_SHARED as u64,
        vfs::mount::PROP_SLAVE => MS_SLAVE as u64,
        vfs::mount::PROP_UNBINDABLE => MS_UNBINDABLE as u64,
        _ => MS_PRIVATE as u64,
    }
}

/// `statmount(2)`：按挂载 id 查询挂载元数据（最小可用语义）。
pub(super) fn sys_statmount(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let mnt_id = ctx.args[0] as u64;
    let requested_mask = ctx.args[1] as u64;
    let buf = ctx.args[2];
    let bufsize = ctx.args[3];
    if buf == 0 || bufsize < STATMOUNT_HEADER_SIZE {
        return Err(Errno::EINVAL);
    }
    let snapshot = mount_snapshot(&vfs_ctx);
    let entry = snapshot
        .iter()
        .find(|e| e.id == mnt_id)
        .ok_or(Errno::ENOENT)?;
    let m = &entry.mount;

    // 字符串区：fs_type 名、挂载根 "/"、挂载点路径。
    let root_mount = vfs_ctx.mount_ns.root.lock().clone();
    let visible_root = root_mount.mount_root.clone();
    let mnt_point = m
        .mountpoint()
        .full_path(&visible_root)
        .unwrap_or_else(|| String::from("?"));
    let fs_type = m.superblock.fs_type;
    let mut strs = String::new();
    let off_fs_type = strs.len();
    strs.push_str(fs_type);
    strs.push('\0');
    let off_point = strs.len();
    strs.push_str(&mnt_point);
    strs.push('\0');
    let total = STATMOUNT_HEADER_SIZE
        .checked_add(strs.len())
        .ok_or(Errno::EOVERFLOW)?;
    if bufsize < total {
        return Err(Errno::EOVERFLOW);
    }

    let st = m.superblock.statfs().map_err(|e| e.to_errno())?;
    let dev = m.superblock.dev_id.unwrap_or_default();

    let mut out = vec![0u8; total];
    let mask = requested_mask
        & (STATMOUNT_SB_BASIC
            | STATMOUNT_MNT_BASIC
            | STATMOUNT_MNT_ROOT
            | STATMOUNT_MNT_POINT
            | STATMOUNT_FS_TYPE
            | STATMOUNT_MNT_NS_ID);
    put_u32(&mut out, 0, STATMOUNT_HEADER_SIZE as u32);
    put_u64(&mut out, 8, mask);
    put_u32(&mut out, 16, dev.major);
    put_u32(&mut out, 20, dev.minor);
    put_u64(&mut out, 24, st.fs_type);
    put_u64(&mut out, 40, entry.id);
    put_u64(&mut out, 48, entry.parent_id);
    put_u64(&mut out, 64, mount_flags_to_mount_attr(m.flags_snapshot()));
    put_u64(
        &mut out,
        72,
        mount_propagation_to_ms(m.propagation.load(core::sync::atomic::Ordering::Acquire)),
    );
    put_u64(
        &mut out,
        80,
        m.peer_group.load(core::sync::atomic::Ordering::Acquire),
    );
    put_u64(&mut out, 112, vfs_ctx.mount_ns.id);
    put_u32(&mut out, 104, off_fs_type as u32);
    put_u32(&mut out, 108, off_point as u32);
    put_u32(&mut out, 120, off_fs_type as u32);
    out[STATMOUNT_HEADER_SIZE..].copy_from_slice(strs.as_bytes());
    copy_to_user(buf, &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

/// `listmount(2)`：枚举命名空间内挂载 id（最小可用语义，忽略筛选参数）。
pub(super) fn sys_listmount(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let last_mnt_id = ctx.args[2] as u64;
    let list = ctx.args[3];
    let nr_entries = ctx.args[4];
    if list == 0 && nr_entries != 0 {
        return Err(Errno::EFAULT);
    }
    let snapshot = mount_snapshot(&vfs_ctx);
    let ids: Vec<u64> = snapshot
        .iter()
        .map(|e| e.id)
        .filter(|id| *id > last_mnt_id)
        .take(nr_entries)
        .collect();
    let mut raw = vec![0u8; ids.len() * 8];
    for (i, id) in ids.iter().enumerate() {
        put_u64(&mut raw, i * 8, *id);
    }
    if !raw.is_empty() {
        copy_to_user(list, &raw).map_err(|e| e.as_errno())?;
    }
    Ok(ids.len())
}

/// `open_tree_attr(2)`：open_tree 的扩展入口。当前内核不处理追加的 mount_attr
/// 参数，按 open_tree 语义落位（挂载属性通过后续 fsmount/mount_setattr 应用）。
pub(super) fn sys_open_tree_attr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    open_tree_common(ctx.args[0], ctx.args[1], ctx.args[2] as u32)
}

// `file_getattr`/`file_setattr`（Linux 6.15+）按 fd 读取/设置扩展文件属性。
// vendor linux-raw-sys 仅携带 XFS ioctl 的 `struct file_attr`（fa_xflags 等），
// 未含新 syscall 的 ABI；此处按 Linux 6.15+ 文档的定长头部做 best-effort 映射：
//   fa_valid(u64) + mode/uid/gid/xflags(u32×4) + size/atime/mtime/ctime(s64+s32)。
const FILE_ATTR_SIZE: usize = 80;
const FILE_ATTR_VALID_MODE: u64 = 1 << 0;
const FILE_ATTR_VALID_UID: u64 = 1 << 1;
const FILE_ATTR_VALID_GID: u64 = 1 << 2;
const FILE_ATTR_VALID_XFLAGS: u64 = 1 << 3;
const FILE_ATTR_VALID_SIZE: u64 = 1 << 4;
const FILE_ATTR_VALID_ATIME: u64 = 1 << 5;
const FILE_ATTR_VALID_MTIME: u64 = 1 << 6;
const FILE_ATTR_VALID_CTIME: u64 = 1 << 7;

pub(super) fn sys_file_getattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let fa_user = ctx.args[3];
    let size = ctx.args[4];
    if fa_user == 0 || size < FILE_ATTR_SIZE {
        return Err(Errno::EINVAL);
    }
    let st = operation::fstat(&fdt, fd).map_err(|e| e.to_errno())?;
    let mut out = [0u8; FILE_ATTR_SIZE];
    let valid = FILE_ATTR_VALID_MODE
        | FILE_ATTR_VALID_UID
        | FILE_ATTR_VALID_GID
        | FILE_ATTR_VALID_SIZE
        | FILE_ATTR_VALID_ATIME
        | FILE_ATTR_VALID_MTIME
        | FILE_ATTR_VALID_CTIME;
    put_u64(&mut out, 0, valid);
    put_u32(&mut out, 8, st.mode & 0o7777);
    put_u32(&mut out, 12, st.uid);
    put_u32(&mut out, 16, st.gid);
    put_u32(&mut out, 20, 0);
    put_i64(&mut out, 24, st.size);
    put_i64(&mut out, 32, st.atime.secs);
    put_u32(&mut out, 40, st.atime.nsecs);
    put_i64(&mut out, 48, st.mtime.secs);
    put_u32(&mut out, 56, st.mtime.nsecs);
    put_i64(&mut out, 64, st.ctime.secs);
    put_u32(&mut out, 72, st.ctime.nsecs);
    copy_to_user(fa_user, &out).map_err(|e| e.as_errno())?;
    Ok(0)
}

pub(super) fn sys_file_setattr(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let fa_user = ctx.args[3];
    let size = ctx.args[4];
    if fa_user == 0 || size < FILE_ATTR_SIZE {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; FILE_ATTR_SIZE];
    copy_from_user(fa_user, &mut raw).map_err(|e| e.as_errno())?;
    let valid = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    if valid
        & !(FILE_ATTR_VALID_MODE
            | FILE_ATTR_VALID_UID
            | FILE_ATTR_VALID_GID
            | FILE_ATTR_VALID_SIZE
            | FILE_ATTR_VALID_ATIME
            | FILE_ATTR_VALID_MTIME)
        != 0
    {
        return Err(Errno::EOPNOTSUPP);
    }
    if valid & (FILE_ATTR_VALID_MODE | FILE_ATTR_VALID_UID | FILE_ATTR_VALID_GID) != 0 {
        let mode = FileMode::new(u32::from_le_bytes(raw[8..12].try_into().unwrap()) as u16);
        let uid = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        let gid = u32::from_le_bytes(raw[16..20].try_into().unwrap());
        let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
        if valid & FILE_ATTR_VALID_MODE != 0 {
            operation::fchmod(&vfs_ctx, &fdt, fd, mode).map_err(|e| e.to_errno())?;
        }
        if valid & (FILE_ATTR_VALID_UID | FILE_ATTR_VALID_GID) != 0 {
            let uid = (valid & FILE_ATTR_VALID_UID != 0).then_some(Uid(uid));
            let gid = (valid & FILE_ATTR_VALID_GID != 0).then_some(Gid(gid));
            if uid.is_some() || gid.is_some() {
                operation::fchown(&vfs_ctx, &fdt, fd, uid, gid).map_err(|e| e.to_errno())?;
            }
        }
        drop(file);
    }
    if valid & FILE_ATTR_VALID_SIZE != 0 {
        let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
        let size = i64::from_le_bytes(raw[24..32].try_into().unwrap());
        if size < 0 {
            return Err(Errno::EINVAL);
        }
        file.truncate(size as u64).map_err(|e| e.to_errno())?;
    }
    if valid & (FILE_ATTR_VALID_ATIME | FILE_ATTR_VALID_MTIME) != 0 {
        let atime = (valid & FILE_ATTR_VALID_ATIME != 0).then(|| Timespec {
            secs: i64::from_le_bytes(raw[32..40].try_into().unwrap()),
            nsecs: u32::from_le_bytes(raw[40..44].try_into().unwrap()),
        });
        let mtime = (valid & FILE_ATTR_VALID_MTIME != 0).then(|| Timespec {
            secs: i64::from_le_bytes(raw[48..56].try_into().unwrap()),
            nsecs: u32::from_le_bytes(raw[56..60].try_into().unwrap()),
        });
        operation::futimens(&vfs_ctx, &fdt, fd, atime, mtime).map_err(|e| e.to_errno())?;
    }
    Ok(0)
}

fn timeout_deadline(timeout_ms: i64) -> Option<u64> {
    if timeout_ms >= 0 {
        Some(sched::now_ns_direct().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)))
    } else {
        None
    }
}

fn timeout_expired(deadline: Option<u64>) -> bool {
    deadline.is_some_and(|dl| sched::now_ns_direct() >= dl)
}

fn poll_recheck_deadline(
    now: u64,
    deadline: Option<u64>,
    all_sources_registered: bool,
) -> Option<u64> {
    const POLL_RECHECK_NS: u64 = 10_000_000;
    if all_sources_registered {
        deadline
    } else {
        let quantum = now.saturating_add(POLL_RECHECK_NS);
        Some(deadline.map_or(quantum, |dl| dl.min(quantum)))
    }
}

#[cfg(feature = "kernel-tests")]
mod poll_deadline_tests {
    use ktest::ktest;

    use super::poll_recheck_deadline;

    #[ktest]
    fn registered_poll_sources_keep_the_original_deadline() {
        assert_eq!(poll_recheck_deadline(100, None, true), None);
        assert_eq!(
            poll_recheck_deadline(100, Some(30_000_000), true),
            Some(30_000_000)
        );
    }

    #[ktest]
    fn unregistered_poll_sources_use_a_bounded_recheck() {
        assert_eq!(poll_recheck_deadline(100, None, false), Some(10_000_100));
        assert_eq!(
            poll_recheck_deadline(100, Some(5_000_000), false),
            Some(5_000_000)
        );
    }
}

fn restore_current_task_after_wait(task: &Arc<sched::Task>) {
    if !task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running) {
        let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
    }
}

fn wait_on_poll_sources(
    sources: &[(Arc<vfs::file::File>, PollEvents)],
    deadline: Option<u64>,
) -> Result<(), Errno> {
    let task = sched::current_task_direct();
    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    if timeout_expired(deadline) {
        return Ok(());
    }

    let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
    let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);

    let mut registered_waiter = false;
    let mut all_sources_registered = !sources.is_empty();
    for (file, interest) in sources {
        let registered = file.poll_add_waiter(&task, *interest);
        registered_waiter |= registered;
        all_sources_registered &= registered;
    }
    // 完整注册 waiter 后，注册后的 readiness 复查已经闭合丢失唤醒窗口，直接
    // 等待事件或原始超时。只有混入无 waiter 的设备时才按短周期兼容轮询。
    let recheck_deadline =
        poll_recheck_deadline(sched::now_ns_direct(), deadline, all_sources_registered);
    let deadline_armed =
        recheck_deadline.is_some_and(|deadline| sched::register_sleep_deadline(&task, deadline));

    if sources
        .iter()
        .any(|(file, interest)| !file.poll(*interest).is_empty())
    {
        for (file, _) in sources {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
        return Ok(());
    }
    if timeout_expired(deadline) {
        for (file, _) in sources {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
        return Ok(());
    }

    if !registered_waiter && !deadline_armed {
        restore_current_task_after_wait(&task);
        drop(task);
        return sched::operation::sched_yield();
    }

    drop(task);
    sched::schedule_once(sched::now_ns_direct());
    let task = sched::current_task_direct();
    for (file, _) in sources {
        file.poll_remove_waiter(&task);
    }
    if deadline_armed {
        sched::cancel_sleep_deadline(&task);
    }
    restore_current_task_after_wait(&task);
    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    Ok(())
}

struct TemporarySigmask {
    task: Option<Arc<sched::Task>>,
    old: SigSet,
}

impl TemporarySigmask {
    fn install(mask: Option<SigSet>) -> Self {
        let Some(mask) = mask else {
            return Self {
                task: None,
                old: SigSet::EMPTY,
            };
        };
        let task = sched::current_task_direct();
        let old = task.signal.block(mask, SigProcMaskHow::SetMask);
        Self {
            task: Some(task),
            old,
        }
    }
}

impl Drop for TemporarySigmask {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.signal.block(self.old, SigProcMaskHow::SetMask);
        }
    }
}

fn read_direct_sigmask(sigmask_user: usize, sigset_size: usize) -> Result<Option<SigSet>, Errno> {
    if sigmask_user == 0 {
        return Ok(None);
    }
    if sigset_size != 8 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    copy_from_user(sigmask_user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(Some(SigSet::from_raw(u64::from_le_bytes(raw))))
}

fn read_sigset_arg(user: usize, sigset_size: usize) -> Result<SigSet, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    if sigset_size != 8 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(SigSet::from_raw(u64::from_le_bytes(raw)).sanitized())
}

fn read_pselect_sigmask(user: usize) -> Result<Option<SigSet>, Errno> {
    if user == 0 {
        return Ok(None);
    }
    let mut raw = [0u8; PSELECT6_SIGSET_ARG_SIZE_64];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sigmask_user = usize::from_le_bytes(raw[0..8].try_into().unwrap());
    let sigset_size = usize::from_le_bytes(raw[8..16].try_into().unwrap());
    read_direct_sigmask(sigmask_user, sigset_size)
}

fn copy_fdset_from_user(user: usize, len: usize) -> Result<Vec<u8>, Errno> {
    if user == 0 {
        return Ok(vec![0u8; len]);
    }
    let mut out = vec![0u8; len];
    copy_from_user(user, &mut out).map_err(|e| e.as_errno())?;
    Ok(out)
}

fn copy_fdset_to_user(user: usize, set: &[u8]) -> Result<(), Errno> {
    if user == 0 {
        return Ok(());
    }
    copy_to_user(user, set).map_err(|e| e.as_errno())
}

fn clear_fdset(set: &mut [u8]) {
    set.fill(0);
}

fn fdset_test(set: &[u8], fd: usize) -> bool {
    if set.is_empty() {
        return false;
    }
    let byte = fd / 8;
    let bit = fd % 8;
    set.get(byte).is_some_and(|value| (value & (1 << bit)) != 0)
}

fn fdset_set(set: &mut [u8], fd: usize) {
    let byte = fd / 8;
    let bit = fd % 8;
    if let Some(slot) = set.get_mut(byte) {
        *slot |= 1 << bit;
    }
}

fn fd_arg(raw: usize) -> Result<Fd, Errno> {
    let fd = raw as isize;
    if fd < 0 {
        return Err(Errno::EBADF);
    }
    Ok(Fd::from_raw(fd as u32))
}

const IOPRIO_WHO_PROCESS: usize = 1;
const IOPRIO_WHO_PGRP: usize = 2;
const IOPRIO_WHO_USER: usize = 3;
const IOPRIO_CLASS_SHIFT: u16 = 13;
const IOPRIO_CLASS_RT: u16 = 1;

fn validate_ioprio(raw: usize) -> Result<u16, Errno> {
    if raw > u16::MAX as usize {
        return Err(Errno::EINVAL);
    }
    let value = raw as u16;
    let class = ioprio_class(value);
    if class > 3 {
        return Err(Errno::EINVAL);
    }
    Ok(value)
}

fn ioprio_class(value: u16) -> u16 {
    value >> IOPRIO_CLASS_SHIFT
}

fn ioprio_targets(
    which: usize,
    who: i32,
    current: &Arc<sched::Task>,
) -> Result<Vec<Arc<sched::Task>>, Errno> {
    match which {
        IOPRIO_WHO_PROCESS => {
            let task = if who == 0 {
                Arc::clone(current)
            } else {
                lookup_task_by_pid(who)?
            };
            Ok(vec![task])
        }
        IOPRIO_WHO_PGRP => {
            let pgid = if who == 0 {
                current.process_group().pgid()
            } else {
                who
            };
            Ok(sched::root_pid_ns()
                .registry()
                .snapshot()
                .into_iter()
                .filter_map(|(_, weak)| weak.upgrade())
                .filter(|task| task.process_group().pgid() == pgid)
                .collect())
        }
        IOPRIO_WHO_USER => {
            let uid = if who == 0 {
                current.credentials().uid.0
            } else if who < 0 {
                return Err(Errno::EINVAL);
            } else {
                who as u32
            };
            Ok(sched::root_pid_ns()
                .registry()
                .snapshot()
                .into_iter()
                .filter_map(|(_, weak)| weak.upgrade())
                .filter(|task| task.credentials().uid.0 == uid)
                .collect())
        }
        _ => Err(Errno::EINVAL),
    }
}

fn lookup_task_by_pid(pid: i32) -> Result<Arc<sched::Task>, Errno> {
    if pid <= 0 {
        return Err(Errno::EINVAL);
    }
    sched::root_pid_ns()
        .registry()
        .lookup(pid)
        .and_then(|weak| weak.upgrade())
        .ok_or(Errno::ESRCH)
}

fn task_may_access(current: &Arc<sched::Task>, target: &Arc<sched::Task>) -> bool {
    if Arc::ptr_eq(current, target) {
        return true;
    }
    let current_creds = current.credentials();
    if current_creds.has_cap(Capability::SysAdmin) {
        return true;
    }
    let target_creds = target.credentials();
    current_creds.euid == target_creds.uid
        || current_creds.euid == target_creds.euid
        || current_creds.uid == target_creds.uid
        || current_creds.uid == target_creds.euid
}

fn task_fdtable(task: &Arc<sched::Task>) -> Option<Arc<vfs::fdtable::FdTable>> {
    task.ext_lookup(sched::TASKEXT_VFS_FDTABLE)?
        .downcast::<vfs::fdtable::FdTable>()
        .ok()
}

fn nonnegative_i64_arg(raw: usize) -> Result<u64, Errno> {
    let value = raw as isize as i64;
    if value < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(value as u64)
}

fn split_offset_arg(pos_l: usize, pos_h: usize) -> Result<Option<u64>, Errno> {
    // Linux raw preadv/pwritev ABI 把 64-bit offset 拆成 pos_l/pos_h 两个寄存器。
    // libc/rustix 在 64-bit 架构上也按这个 raw ABI 传参；只取低 32 位可兼容
    // pos_l 传完整 64-bit 值或只传低 32-bit 值的两种封装。
    let raw = ((pos_h as u64 & 0xffff_ffff) << 32) | (pos_l as u64 & 0xffff_ffff);
    let signed = raw as i64;
    if signed == -1 {
        return Ok(None);
    }
    if signed < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(Some(signed as u64))
}

fn nonnegative_split_offset_arg(pos_l: usize, pos_h: usize) -> Result<u64, Errno> {
    split_offset_arg(pos_l, pos_h)?.ok_or(Errno::EINVAL)
}

fn dirfd_arg(raw: usize, fdt: &vfs::fdtable::FdTable) -> Result<Dirfd, Errno> {
    let fd = raw as isize as i32;
    if fd == AT_FDCWD {
        return Ok(Dirfd::Cwd);
    }
    if fd < 0 {
        return Err(Errno::EBADF);
    }
    let file = fdt.get_file(Fd::from_raw(fd as u32)).ok_or(Errno::EBADF)?;
    Ok(Dirfd::Fd(file))
}

/// 绝对路径时忽略 dirfd（Linux 语义：不校验 fd 是否有效）。
fn dirfd_arg_for_path(raw: usize, path: &str, fdt: &vfs::fdtable::FdTable) -> Result<Dirfd, Errno> {
    if path.starts_with('/') {
        Ok(Dirfd::Cwd)
    } else {
        dirfd_arg(raw, fdt)
    }
}

fn file_for_fd(fd: Fd) -> Result<Arc<vfs::file::File>, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    fdt.get_file(fd).ok_or(Errno::EBADF)
}

fn ensure_network_execution_scope_for_file(ctx: &mut SyscallContext<'_>, file: &vfs::file::File) {
    if file
        .downcast_ops::<vfs::net_socket::NetSocketFileOps>()
        .is_some()
    {
        ctx.ensure_network_execution_scope();
    }
}

fn synthetic_readlink_target(
    ctx: &SyscallContext<'_>,
    path: &str,
) -> Result<Option<String>, Errno> {
    match path {
        "/proc/self/exe" | "/proc/thread-self/exe" => crate::sched::task_exec_path(ctx.task())
            .map(Some)
            .ok_or(Errno::ENOENT),
        "/proc/self/root" | "/proc/thread-self/root" => Ok(Some(String::from("/"))),
        "/proc/self/cwd" | "/proc/thread-self/cwd" => {
            let vfs_ctx = current_vfs_context().ok_or(Errno::ENOENT)?;
            let mut cwd = namespace_path(&vfs_ctx, &vfs_ctx.cwd(), &vfs_ctx.cwd_mount())
                .unwrap_or_else(|| String::from("/"));
            if cwd.is_empty() {
                cwd.push('/');
            }
            Ok(Some(cwd))
        }
        _ => Ok(None),
    }
}

fn copy_readlink_target(buf: usize, size: usize, target: &str) -> Result<usize, Errno> {
    let bytes = target.as_bytes();
    if size == 0 {
        return Err(Errno::EINVAL);
    }
    let n = bytes.len().min(size);
    copy_to_user(buf, &bytes[..n]).map_err(|e| e.as_errno())?;
    Ok(n)
}

fn faccessat_common(ctx: &mut SyscallContext<'_>, has_flags: bool) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let raw_dirfd = ctx.args[0];
    let path = copy_path_from_user(ctx.args[1])?;
    let mode = ctx.args[2];
    let flags = if has_flags { ctx.args[3] } else { 0 };
    if (mode & !(R_OK | W_OK | X_OK)) != 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)) != 0 {
        return Err(Errno::EINVAL);
    }

    let (st, readonly) = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        if raw_dirfd as i32 == AT_FDCWD {
            let r = vfs::path::lookup(&vfs_ctx, &Dirfd::Cwd, ".", LookupFlags::default())
                .map_err(|e| e.to_errno())?;
            let inode = r.dentry.inode().ok_or(Errno::ENOENT)?;
            (inode.stat().map_err(|e| e.to_errno())?, r.mount.is_rdonly())
        } else {
            let fd = fd_arg(raw_dirfd)?;
            let file = fdt.get_file(fd).ok_or(Errno::EBADF)?;
            (
                file.stat().map_err(|e| e.to_errno())?,
                file.mount().is_rdonly(),
            )
        }
    } else {
        let dirfd = dirfd_arg_for_path(raw_dirfd, &path, &fdt)?;
        let lookup_flags = if (flags & AT_SYMLINK_NOFOLLOW) != 0 {
            LookupFlags::NO_FOLLOW
        } else {
            LookupFlags::default()
        };
        let result =
            vfs::path::lookup(&vfs_ctx, &dirfd, &path, lookup_flags).map_err(|e| e.to_errno())?;
        let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
        (
            inode.stat().map_err(|e| e.to_errno())?,
            result.mount.is_rdonly(),
        )
    };
    if (mode & W_OK) != 0 && readonly {
        return Err(Errno::EROFS);
    }
    if mode == F_OK || access_mode_allowed(ctx, &st, mode, flags) {
        Ok(0)
    } else {
        Err(Errno::EACCES)
    }
}

fn access_mode_allowed(ctx: &SyscallContext<'_>, st: &FileStat, mode: usize, flags: usize) -> bool {
    let creds = ctx.task().credentials();
    let uid = if (flags & AT_EACCESS) != 0 {
        creds.euid.0
    } else {
        creds.uid.0
    };
    let gid = if (flags & AT_EACCESS) != 0 {
        creds.egid.0
    } else {
        creds.gid.0
    };
    let bits = st.mode & 0o777;
    if uid == 0 {
        return (mode & X_OK) == 0 || (bits & 0o111) != 0;
    }
    let shift = if uid == st.uid {
        6
    } else if gid == st.gid {
        3
    } else {
        0
    };
    let perms = (bits >> shift) & 0o7;
    ((mode & R_OK) == 0 || (perms & 0o4) != 0)
        && ((mode & W_OK) == 0 || (perms & 0o2) != 0)
        && ((mode & X_OK) == 0 || (perms & 0o1) != 0)
}

fn open_options_to_linux_flags(opts: &OpenOptions) -> usize {
    let mut raw = 0usize;
    raw |= match opts.access {
        AccessMode::ReadOnly => 0,
        AccessMode::WriteOnly => O_WRONLY,
        AccessMode::ReadWrite => O_RDWR,
    };
    if opts.append {
        raw |= O_APPEND;
    }
    if opts.nonblock {
        raw |= O_NONBLOCK;
    }
    if opts.sync {
        raw |= O_SYNC;
    }
    if opts.direct {
        raw |= O_DIRECT;
    }
    if opts.async_ {
        raw |= O_ASYNC;
    }
    raw
}

fn decode_open_options(raw: usize) -> Result<OpenOptions, Errno> {
    let access = match raw & O_ACCMODE {
        0 => AccessMode::ReadOnly,
        O_WRONLY => AccessMode::WriteOnly,
        O_RDWR => AccessMode::ReadWrite,
        _ => return Err(Errno::EINVAL),
    };
    Ok(OpenOptions {
        access,
        create: (raw & O_CREAT) != 0,
        exclusive: (raw & O_EXCL) != 0,
        truncate: (raw & O_TRUNC) != 0,
        append: (raw & O_APPEND) != 0,
        nofollow: (raw & O_NOFOLLOW) != 0,
        directory: (raw & O_DIRECTORY) != 0,
        noatime: (raw & O_NOATIME) != 0,
        path_only: (raw & O_PATH) != 0,
        nonblock: (raw & O_NONBLOCK) != 0,
        sync: (raw & (O_SYNC | O_DSYNC)) != 0,
        direct: (raw & O_DIRECT) != 0,
        async_: (raw & O_ASYNC) != 0,
        cloexec: (raw & O_CLOEXEC) != 0,
    })
}

#[derive(Clone, Copy)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn read_open_how(user: usize, size: usize) -> Result<OpenHow, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    if size < OPEN_HOW_SIZE {
        return Err(Errno::EINVAL);
    }
    if size > OPEN_HOW_MAX_SIZE {
        return Err(Errno::E2BIG);
    }
    let mut raw = [0u8; OPEN_HOW_SIZE];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    if size > OPEN_HOW_SIZE {
        let extra_len = size - OPEN_HOW_SIZE;
        let mut extra = vec![0u8; extra_len];
        let extra_user = user.checked_add(OPEN_HOW_SIZE).ok_or(Errno::EFAULT)?;
        copy_from_user(extra_user, &mut extra).map_err(|e| e.as_errno())?;
        if extra.iter().any(|b| *b != 0) {
            return Err(Errno::E2BIG);
        }
    }
    Ok(OpenHow {
        flags: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
        mode: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
        resolve: u64::from_le_bytes(raw[16..24].try_into().unwrap()),
    })
}

fn validate_openat2_flags(raw: usize) -> Result<(), Errno> {
    const SUPPORTED_OPEN_FLAGS: usize = O_ACCMODE
        | O_CREAT
        | O_EXCL
        | O_NOCTTY
        | O_TRUNC
        | O_APPEND
        | O_NONBLOCK
        | O_DSYNC
        | O_ASYNC
        | O_DIRECT
        | O_DIRECTORY
        | O_NOFOLLOW
        | O_NOATIME
        | O_CLOEXEC
        | O_PATH
        | O_SYNC;
    if (raw & !SUPPORTED_OPEN_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn write_from_user(file: &vfs::file::File, user: usize, len: usize) -> Result<usize, Errno> {
    write_from_user_at(file, user, len, None, false)
}

struct InetStreamWriteBatch<'a> {
    socket: &'a vfs::net_socket::NetSocketFileOps,
}

impl Drop for InetStreamWriteBatch<'_> {
    fn drop(&mut self) {
        self.socket.finish_stream_send();
    }
}

fn inet_stream_file(file: &vfs::file::File) -> Option<&vfs::net_socket::NetSocketFileOps> {
    let socket = file.downcast_ops::<vfs::net_socket::NetSocketFileOps>()?;
    (usize::from(socket.sock_type()) == vfs_socket::SOCK_STREAM).then_some(socket)
}

fn write_from_user_at(
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    if len == 0 {
        return Ok(0);
    }
    check_direct_alignment(file, user, offset, len)?;
    let Some(vm) = current_vm_space() else {
        return write_from_user_at_fallback(file, user, len, offset, nowait);
    };
    if offset.is_none()
        && let Some(socket) = inet_stream_file(file)
    {
        let _batch = InetStreamWriteBatch { socket };
        // RWF_NOWAIT 映射为 MSG_DONTWAIT，让流式 socket 快速路径同样非阻塞。
        let flags = if nowait { vfs_socket::MSG_DONTWAIT } else { 0 };
        return send_inet_stream_file_from_user(&vm, file, socket, user, len, flags);
    }

    let mut remaining = len;
    let mut user_ptr = user;
    let mut pos = offset.unwrap_or(0);
    let mut written = 0usize;
    while remaining > 0 {
        let chunk = remaining.min(COPY_CHUNK);
        let result = unsafe {
            vm.with_user_read_slice(user_ptr, chunk, |buf| {
                file_write_user_chunk(file, offset, pos, buf).map(|n| (n, buf.len()))
            })
        };
        let (n, window_len) = match result {
            Ok(Ok(pair)) => pair,
            Ok(Err(VfsError::WouldBlock)) if written > 0 => return Ok(written),
            Ok(Err(VfsError::WouldBlock)) if nowait || file.flags().nonblock => {
                return Err(Errno::EAGAIN);
            }
            Ok(Err(VfsError::WouldBlock)) => {
                wait_for_file_readiness(file, PollEvents::POLLOUT)?;
                continue;
            }
            Ok(Err(VfsError::BrokenPipe)) if written == 0 => {
                deliver_sigpipe();
                return Err(Errno::EPIPE);
            }
            Ok(Err(e)) => return Err(e.to_errno()),
            Err(e) => {
                return if written > 0 { Ok(written) } else { Err(e) };
            }
        };
        if n == 0 {
            // 非空 write 返回 0 表示底层没有取得任何进展。Linux write(2)
            // 对常规文件通常应返回错误；这里优先返回已写入字节数，避免 libc/测试
            // 在同一缓冲区上无限重试导致 iozone/lmbench 卡死。
            return if written > 0 {
                Ok(written)
            } else {
                Err(Errno::EIO)
            };
        }
        written += n;
        if n < window_len {
            break;
        }
        user_ptr = user_ptr.checked_add(n).ok_or(Errno::EFAULT)?;
        pos = pos.saturating_add(n as u64);
        remaining -= n;
    }
    Ok(written)
}

fn write_from_user_at_fallback(
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    check_direct_alignment(file, user, offset, len)?;
    let mut remaining = len;
    let mut user_ptr = user;
    let mut pos = offset.unwrap_or(0);
    let mut written = 0usize;
    let mut tmp = [0u8; COPY_CHUNK];
    while remaining > 0 {
        let chunk = remaining.min(tmp.len());
        if let Err(e) = copy_from_user(user_ptr, &mut tmp[..chunk]) {
            return if written > 0 {
                Ok(written)
            } else {
                Err(e.as_errno())
            };
        }
        let n = match file_write_user_chunk(file, offset, pos, &tmp[..chunk]) {
            Ok(n) => n,
            Err(VfsError::WouldBlock) if written > 0 => return Ok(written),
            Err(VfsError::WouldBlock) if nowait || file.flags().nonblock => {
                return Err(Errno::EAGAIN);
            }
            Err(VfsError::WouldBlock) => {
                wait_for_file_readiness(file, PollEvents::POLLOUT)?;
                continue;
            }
            Err(VfsError::BrokenPipe) if written == 0 => {
                deliver_sigpipe();
                return Err(Errno::EPIPE);
            }
            Err(e) => return Err(e.to_errno()),
        };
        if n == 0 {
            // 非空 write 返回 0 表示底层没有取得任何进展。Linux write(2)
            // 对常规文件通常应返回错误；这里优先返回已写入字节数，避免 libc/测试
            // 在同一缓冲区上无限重试导致 iozone/lmbench 卡死。
            return if written > 0 {
                Ok(written)
            } else {
                Err(Errno::EIO)
            };
        }
        written += n;
        if n < chunk {
            break;
        }
        user_ptr = user_ptr.checked_add(n).ok_or(Errno::EFAULT)?;
        pos = pos.saturating_add(n as u64);
        remaining -= n;
    }
    Ok(written)
}

/// O_DIRECT 对齐校验（Linux 语义）：用户缓冲区、偏移、长度均须按 512 字节
/// 对齐，否则 read/write 返回 EINVAL。
fn check_direct_alignment(
    file: &vfs::file::File,
    user: usize,
    offset: Option<u64>,
    len: usize,
) -> Result<(), Errno> {
    if !file.flags().direct {
        return Ok(());
    }
    // 只有支持直接 I/O 的文件系统执行对齐校验：Linux 上 tmpfs 等对
    // fcntl(F_SETFL, O_DIRECT) 设置后的 I/O 保持普通路径，不受对齐约束。
    let supports = file
        .inode()
        .superblock()
        .map(|sb| sb.ops.supports_direct_io())
        .unwrap_or(false);
    if !supports {
        return Ok(());
    }
    let off = offset.unwrap_or_else(|| file.pos());
    if (user & 511) != 0 || (off & 511) != 0 || (len & 511) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn read_to_user(
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    if len == 0 {
        return Ok(0);
    }
    let Some(vm) = current_vm_space() else {
        return read_to_user_fallback(file, user, len, offset, nowait);
    };
    if offset.is_none()
        && let Some(socket) = inet_stream_file(file)
    {
        let _batch = InetStreamFileReceiveBatch { socket };
        // RWF_NOWAIT 映射为 MSG_DONTWAIT，让流式 socket 快速路径同样非阻塞。
        let flags = if nowait { vfs_socket::MSG_DONTWAIT } else { 0 };
        return recv_inet_stream_file_to_user(&vm, file, socket, user, len, flags);
    }
    read_to_user_windows(&vm, file, user, len, offset, nowait)
}

fn read_to_user_windows(
    vm: &VmSpace,
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    check_direct_alignment(file, user, offset, len)?;
    let mut remaining = len;
    let mut user_ptr = user;
    let mut pos = offset.unwrap_or(0);
    let mut read = 0usize;
    while remaining > 0 {
        let chunk = remaining.min(COPY_CHUNK);
        let result = unsafe {
            vm.with_user_write_slice(user_ptr, chunk, |buf| {
                file_read_user_chunk(file, offset, pos, buf).map(|n| (n, buf.len()))
            })
        };
        let (n, window_len) = match result {
            Ok(Ok(pair)) => pair,
            Ok(Err(VfsError::WouldBlock)) if read > 0 => return Ok(read),
            Ok(Err(VfsError::WouldBlock)) if nowait || file.flags().nonblock => {
                return Err(Errno::EAGAIN);
            }
            Ok(Err(VfsError::WouldBlock)) => {
                wait_for_file_readiness(file, PollEvents::POLLIN)?;
                continue;
            }
            Ok(Err(e)) => return Err(e.to_errno()),
            Err(e) => {
                return if read > 0 { Ok(read) } else { Err(e) };
            }
        };
        if n == 0 {
            break;
        }
        read += n;
        user_ptr = user_ptr.checked_add(n).ok_or(Errno::EFAULT)?;
        pos = pos.saturating_add(n as u64);
        remaining -= n;
        if n < window_len {
            break;
        }
    }
    Ok(read)
}

fn read_to_user_fallback(
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    check_direct_alignment(file, user, offset, len)?;
    let mut remaining = len;
    let mut user_ptr = user;
    let mut pos = offset.unwrap_or(0);
    let mut read = 0usize;
    let mut tmp = [0u8; COPY_CHUNK];
    while remaining > 0 {
        let chunk = remaining.min(tmp.len());
        let n = match if offset.is_some() {
            file.read_at(&mut tmp[..chunk], pos)
        } else {
            file.read(&mut tmp[..chunk])
        } {
            Ok(n) => n,
            Err(VfsError::WouldBlock) if read > 0 => return Ok(read),
            Err(VfsError::WouldBlock) if nowait || file.flags().nonblock => {
                return Err(Errno::EAGAIN);
            }
            Err(VfsError::WouldBlock) => {
                wait_for_file_readiness(file, PollEvents::POLLIN)?;
                continue;
            }
            Err(e) => return Err(e.to_errno()),
        };
        if n == 0 {
            break;
        }
        if let Err(e) = copy_to_user(user_ptr, &tmp[..n]) {
            return if read > 0 {
                Ok(read)
            } else {
                Err(e.as_errno())
            };
        }
        read += n;
        user_ptr = user_ptr.checked_add(n).ok_or(Errno::EFAULT)?;
        pos = pos.saturating_add(n as u64);
        remaining -= n;
        if n < chunk {
            break;
        }
    }
    Ok(read)
}

fn file_write_user_chunk(
    file: &vfs::file::File,
    offset: Option<u64>,
    pos: u64,
    buf: &[u8],
) -> vfs::error::VfsResult<usize> {
    if offset.is_some() {
        file.write_at(buf, pos)
    } else {
        file.write(buf)
    }
}

fn file_read_user_chunk(
    file: &vfs::file::File,
    offset: Option<u64>,
    pos: u64,
    buf: &mut [u8],
) -> vfs::error::VfsResult<usize> {
    if offset.is_some() {
        file.read_at(buf, pos)
    } else {
        file.read(buf)
    }
}

fn current_vm_space() -> Option<Arc<VmSpace>> {
    if !sched::is_ready_direct() {
        return None;
    }
    sched::current_task_direct()
        .ext_lookup(sched::TASKEXT_VM_SPACE)?
        .downcast::<VmSpace>()
        .ok()
}

fn read_optional_offset(user: usize) -> Result<Option<u64>, Errno> {
    if user == 0 {
        return Ok(None);
    }
    let off = read_user_i64(user)?;
    if off < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(Some(off as u64))
}

fn write_optional_offset(user: usize, off: Option<u64>) -> Result<(), Errno> {
    let Some(off) = off else {
        return Ok(());
    };
    let off = i64::try_from(off).map_err(|_| Errno::EINVAL)?;
    copy_to_user(user, &off.to_le_bytes()).map_err(|e| e.as_errno())
}

fn copy_between_files(
    ctx: &mut SyscallContext<'_>,
    input: &vfs::file::File,
    output: &vfs::file::File,
    len: usize,
    in_off: &mut Option<u64>,
    out_off: &mut Option<u64>,
    nonblock: bool,
) -> Result<usize, Errno> {
    let mut tmp = [0u8; COPY_CHUNK];
    let mut remaining = len;
    let mut total = 0usize;
    while remaining > 0 {
        let chunk = remaining.min(tmp.len());
        let nread = loop {
            let result = match *in_off {
                Some(pos) => input.read_at(&mut tmp[..chunk], pos),
                None => {
                    ensure_network_execution_scope_for_file(ctx, input);
                    input.read(&mut tmp[..chunk])
                }
            };
            match result {
                Ok(n) => break n,
                Err(VfsError::WouldBlock) if total > 0 => return Ok(total),
                Err(VfsError::WouldBlock) if nonblock || input.flags().nonblock => {
                    return Err(Errno::EAGAIN);
                }
                Err(VfsError::WouldBlock) => wait_for_file_readiness(input, PollEvents::POLLIN)?,
                Err(e) => return Err(e.to_errno()),
            }
        };
        if nread == 0 {
            break;
        }
        let mut written_this_chunk = 0usize;
        while written_this_chunk < nread {
            let slice = &tmp[written_this_chunk..nread];
            let write_pos = out_off.map(|pos| pos.saturating_add(written_this_chunk as u64));
            let nwritten = match write_pos {
                Some(pos) => output.write_at(slice, pos),
                None => {
                    ensure_network_execution_scope_for_file(ctx, output);
                    output.write(slice)
                }
            };
            match nwritten {
                Ok(0) => return Ok(total),
                Ok(n) => {
                    written_this_chunk += n;
                    total = total.checked_add(n).ok_or(Errno::EINVAL)?;
                }
                Err(VfsError::WouldBlock) if total > 0 => return Ok(total),
                Err(VfsError::WouldBlock) if nonblock || output.flags().nonblock => {
                    return Err(Errno::EAGAIN);
                }
                Err(VfsError::WouldBlock) => {
                    wait_for_file_readiness(output, PollEvents::POLLOUT)?;
                }
                Err(VfsError::BrokenPipe) if total == 0 => {
                    deliver_sigpipe();
                    return Err(Errno::EPIPE);
                }
                Err(e) => return Err(e.to_errno()),
            }
        }
        if let Some(pos) = in_off.as_mut() {
            *pos = pos.saturating_add(nread as u64);
        }
        if let Some(pos) = out_off.as_mut() {
            *pos = pos.saturating_add(written_this_chunk as u64);
        }
        remaining -= written_this_chunk;
        if nread < chunk || written_this_chunk < nread {
            break;
        }
    }
    Ok(total)
}

fn write_iovecs(
    ctx: &mut SyscallContext<'_>,
    file: &vfs::file::File,
    iov: usize,
    iovcnt: usize,
    mut offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    // 进入循环前先累加各段 len 校验溢出，避免部分 I/O 之后才返回 EINVAL。
    let mut total_len = 0usize;
    for i in 0..iovcnt {
        let (_, len) = read_iovec(iov, i)?;
        total_len = total_len.checked_add(len).ok_or(Errno::EINVAL)?;
    }

    let mut total = 0usize;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov, i)?;
        let current_offset = offset;
        if len != 0 && current_offset.is_none() {
            ensure_network_execution_scope_for_file(ctx, file);
        }
        match write_from_user_at(file, base, len, current_offset, nowait) {
            Ok(n) => {
                total = total.checked_add(n).ok_or(Errno::EINVAL)?;
                if let Some(pos) = offset.as_mut() {
                    *pos = pos.saturating_add(n as u64);
                }
                if n < len {
                    break;
                }
            }
            Err(_) if total > 0 => return Ok(total),
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn read_iovecs(
    ctx: &mut SyscallContext<'_>,
    file: &vfs::file::File,
    iov: usize,
    iovcnt: usize,
    mut offset: Option<u64>,
    nowait: bool,
) -> Result<usize, Errno> {
    // 进入循环前先累加各段 len 校验溢出，避免部分 I/O 之后才返回 EINVAL。
    let mut total_len = 0usize;
    for i in 0..iovcnt {
        let (_, len) = read_iovec(iov, i)?;
        total_len = total_len.checked_add(len).ok_or(Errno::EINVAL)?;
    }

    let mut total = 0usize;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov, i)?;
        let current_offset = offset;
        if len != 0 && current_offset.is_none() {
            ensure_network_execution_scope_for_file(ctx, file);
        }
        match read_to_user(file, base, len, current_offset, nowait) {
            Ok(n) => {
                total = total.checked_add(n).ok_or(Errno::EINVAL)?;
                if let Some(pos) = offset.as_mut() {
                    *pos = pos.saturating_add(n as u64);
                }
                if n < len {
                    break;
                }
            }
            Err(_) if total > 0 => return Ok(total),
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn wait_for_file_readiness(file: &vfs::file::File, interest: PollEvents) -> Result<(), Errno> {
    const IO_RECHECK_NS: u64 = 10_000_000;
    let task = sched::current_task_direct();
    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    let deadline = file.io_timeout_deadline(interest);
    if timeout_expired(deadline) {
        return Err(Errno::EAGAIN);
    }
    let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
    let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);

    let registered = file.poll_add_waiter(&task, interest);
    // 不是所有文件都有专用唤醒源，例如 UART/TTY 当前是轮询设备。没有
    // waiter 时也必须定期重检 readiness，否则阻塞 read/write 可能永久睡眠；
    // 直接忙等又会饿死 QEMU/设备后端的输入投递。
    let recheck_deadline = {
        let now = sched::now_ns_direct();
        let quantum = now.saturating_add(IO_RECHECK_NS);
        deadline.map_or(quantum, |dl| dl.min(quantum))
    };
    let deadline_armed = sched::register_sleep_deadline(&task, recheck_deadline);
    if !file.poll(interest).is_empty() {
        if registered {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
        return Ok(());
    }
    if timeout_expired(deadline) {
        if registered {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
        return Err(Errno::EAGAIN);
    }
    if has_unblocked_signal(&task) {
        if registered {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
        return Err(Errno::EINTR);
    }

    let task = if registered || deadline_armed {
        drop(task);
        sched::schedule_once(sched::now_ns_direct());
        let task = sched::current_task_direct();
        if registered {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
        task
    } else {
        restore_current_task_after_wait(&task);
        drop(task);
        sched::operation::sched_yield()?;
        sched::current_task_direct()
    };

    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    if timeout_expired(deadline) {
        return Err(Errno::EAGAIN);
    }
    Ok(())
}

fn has_unblocked_signal(task: &Arc<sched::Task>) -> bool {
    sched::operation::has_interrupting_signal(task)
}

fn deliver_sigpipe() {
    let task = sched::current_task_direct();
    let creds = task.credentials();
    let info = sched::SigInfo {
        sig: sched::SignalNumber::SIGPIPE,
        code: 0,
        sender_pid: task.pid_root().unwrap_or(0),
        sender_uid: creds.uid,
        raw: None,
    };
    task.signal.deliver(info);
    sched::signal_wakeup(&task, &info);
}

fn read_iovec(iov: usize, index: usize) -> Result<(usize, usize), Errno> {
    let mut raw = [0u8; 16];
    let ptr = iov
        .checked_add(index.checked_mul(16).ok_or(Errno::EFAULT)?)
        .ok_or(Errno::EFAULT)?;
    copy_from_user(ptr, &mut raw).map_err(|e| e.as_errno())?;
    let base = usize::from_le_bytes(raw[0..8].try_into().unwrap());
    let len = usize::from_le_bytes(raw[8..16].try_into().unwrap());
    Ok((base, len))
}

fn read_user_i32(user: usize) -> Result<i32, Errno> {
    let mut raw = [0u8; 4];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(i32::from_ne_bytes(raw))
}

fn read_user_i64(user: usize) -> Result<i64, Errno> {
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(i64::from_le_bytes(raw))
}

struct UserMsghdr {
    name: usize,
    namelen: u32,
    iov: usize,
    iovlen: usize,
    control: usize,
    controllen: usize,
    flags: i32,
}

struct UserMmsghdr {
    msg_hdr: UserMsghdr,
    msg_len: u32,
}

fn read_msghdr(user: usize) -> Result<UserMsghdr, Errno> {
    let mut raw = [0u8; MSGHDR_SIZE_64];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(UserMsghdr {
        name: usize::from_le_bytes(raw[0..8].try_into().unwrap()),
        namelen: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
        iov: usize::from_le_bytes(raw[16..24].try_into().unwrap()),
        iovlen: usize::from_le_bytes(raw[24..32].try_into().unwrap()),
        control: usize::from_le_bytes(raw[32..40].try_into().unwrap()),
        controllen: usize::from_le_bytes(raw[40..48].try_into().unwrap()),
        flags: i32::from_le_bytes(raw[48..52].try_into().unwrap()),
    })
}

fn write_msghdr(user: usize, hdr: &UserMsghdr) -> Result<(), Errno> {
    let mut raw = [0u8; MSGHDR_SIZE_64];
    raw[0..8].copy_from_slice(&hdr.name.to_le_bytes());
    raw[8..12].copy_from_slice(&hdr.namelen.to_le_bytes());
    raw[16..24].copy_from_slice(&hdr.iov.to_le_bytes());
    raw[24..32].copy_from_slice(&hdr.iovlen.to_le_bytes());
    raw[32..40].copy_from_slice(&hdr.control.to_le_bytes());
    raw[40..48].copy_from_slice(&hdr.controllen.to_le_bytes());
    raw[48..52].copy_from_slice(&hdr.flags.to_le_bytes());
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn read_mmsghdr(user: usize) -> Result<UserMmsghdr, Errno> {
    let msg_hdr = read_msghdr(user)?;
    let mut len_raw = [0u8; 4];
    copy_from_user(
        user.checked_add(MSGHDR_SIZE_64).ok_or(Errno::EFAULT)?,
        &mut len_raw,
    )
    .map_err(|e| e.as_errno())?;
    Ok(UserMmsghdr {
        msg_hdr,
        msg_len: u32::from_le_bytes(len_raw),
    })
}

fn write_mmsghdr(user: usize, hdr: &UserMmsghdr) -> Result<(), Errno> {
    write_msghdr(user, &hdr.msg_hdr)?;
    write_mmsghdr_len(user, hdr.msg_len as usize)
}

fn write_mmsghdr_len(user: usize, len: usize) -> Result<(), Errno> {
    copy_to_user(
        user.checked_add(MSGHDR_SIZE_64).ok_or(Errno::EFAULT)?,
        &(len as u32).to_le_bytes(),
    )
    .map_err(|e| e.as_errno())
}

fn msgvec_ptr(base: usize, index: usize) -> Result<usize, Errno> {
    base.checked_add(index.checked_mul(MMSGHDR_SIZE_64).ok_or(Errno::EFAULT)?)
        .ok_or(Errno::EFAULT)
}

fn read_epoll_event(user: usize) -> Result<vfs::epoll::EpollEvent, Errno> {
    let mut raw = [0u8; EPOLL_EVENT_SIZE_64];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(vfs::epoll::EpollEvent {
        events: u32::from_le_bytes(raw[0..4].try_into().unwrap()),
        data: u64::from_le_bytes(
            raw[EPOLL_EVENT_DATA_OFFSET_64..EPOLL_EVENT_SIZE_64]
                .try_into()
                .unwrap(),
        ),
    })
}

fn write_epoll_events(user: usize, events: &[vfs::epoll::EpollEvent]) -> Result<(), Errno> {
    for (index, event) in events.iter().enumerate() {
        let mut raw = [0u8; EPOLL_EVENT_SIZE_64];
        raw[0..4].copy_from_slice(&event.events.to_le_bytes());
        raw[EPOLL_EVENT_DATA_OFFSET_64..EPOLL_EVENT_SIZE_64]
            .copy_from_slice(&event.data.to_le_bytes());
        let ptr = user
            .checked_add(
                index
                    .checked_mul(EPOLL_EVENT_SIZE_64)
                    .ok_or(Errno::EFAULT)?,
            )
            .ok_or(Errno::EFAULT)?;
        copy_to_user(ptr, &raw).map_err(|e| e.as_errno())?;
    }
    Ok(())
}

fn copy_user_region(user: usize, len: usize) -> Result<Vec<u8>, Errno> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if len > MAX_SOCKET_CONTROL {
        return Err(Errno::EMSGSIZE);
    }
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut out = zeroed_vec(len)?;
    copy_from_user(user, &mut out).map_err(|e| e.as_errno())?;
    Ok(out)
}

fn copy_sockaddr_from_user(user: usize, len: usize) -> Result<SocketAddressBuffer, Errno> {
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    if len > MAX_SOCKET_ADDR {
        return Err(Errno::EINVAL);
    }
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut out = SocketAddressBuffer {
        bytes: [0; MAX_SOCKET_ADDR],
        len,
    };
    copy_from_user(user, &mut out.bytes[..len]).map_err(|e| e.as_errno())?;
    Ok(out)
}

fn read_socklen_user(user: usize) -> Result<usize, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 4];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let len = i32::from_le_bytes(raw);
    if len < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(len as usize)
}

fn write_socklen_user(user: usize, len: usize) -> Result<(), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    copy_to_user(user, &(len as u32).to_le_bytes()).map_err(|e| e.as_errno())
}

fn copy_sockaddr_bytes(user: usize, user_len: usize, raw: Option<&[u8]>) -> Result<(), Errno> {
    let data = raw.unwrap_or(&[]);
    if user_len == 0 || data.is_empty() {
        return Ok(());
    }
    let copy_len = data.len().min(user_len);
    copy_to_user(user, &data[..copy_len]).map_err(|e| e.as_errno())
}

fn copy_sockaddr_to_user(user: usize, len_user: usize, raw: Option<&[u8]>) -> Result<(), Errno> {
    let max_len = read_socklen_user(len_user)?;
    copy_sockaddr_bytes(user, max_len, raw)?;
    write_socklen_user(len_user, raw.map_or(0, <[u8]>::len))
}

fn iov_total_len_capped(iov: usize, iovcnt: usize, cap: usize) -> Result<usize, Errno> {
    let mut total = 0usize;
    for i in 0..iovcnt {
        let (_, len) = read_iovec(iov, i)?;
        let remaining = cap.saturating_sub(total);
        total = total.saturating_add(len.min(remaining));
        if total == cap {
            break;
        }
    }
    Ok(total)
}

fn copy_send_iovecs(iov: usize, iovcnt: usize) -> Result<Vec<u8>, Errno> {
    let total = iov_total_len_capped(iov, iovcnt, MAX_SOCKET_IO)?;
    let mut out = Vec::new();
    out.try_reserve(total).map_err(|_| Errno::ENOMEM)?;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov, i)?;
        let remaining = total - out.len();
        let len = len.min(remaining);
        if len == 0 {
            continue;
        }
        let start = out.len();
        out.resize(start + len, 0);
        copy_from_user(base, &mut out[start..start + len]).map_err(|e| e.as_errno())?;
        if out.len() == total {
            break;
        }
    }
    Ok(out)
}

fn scatter_recv_iovecs(iov: usize, iovcnt: usize, data: &[u8]) -> Result<(), Errno> {
    let mut offset = 0usize;
    for i in 0..iovcnt {
        if offset >= data.len() {
            break;
        }
        let (base, len) = read_iovec(iov, i)?;
        if len == 0 {
            continue;
        }
        let take = (data.len() - offset).min(len);
        copy_to_user(base, &data[offset..offset + take]).map_err(|e| e.as_errno())?;
        offset += take;
    }
    Ok(())
}

fn zeroed_vec(len: usize) -> Result<Vec<u8>, Errno> {
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
    out.resize(len, 0);
    Ok(out)
}

fn copy_optval_to_user(optval_user: usize, optlen_user: usize, value: &[u8]) -> Result<(), Errno> {
    let max_len = read_socklen_user(optlen_user)?;
    let copy_len = value.len().min(max_len);
    if copy_len != 0 {
        copy_to_user(optval_user, &value[..copy_len]).map_err(|e| e.as_errno())?;
    }
    write_socklen_user(optlen_user, value.len())
}

fn accept_common(ctx: &mut SyscallContext<'_>, flags: usize) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    ctx.ensure_network_execution_scope();
    let (new_fd, addr) = vfs_socket::accept(&vfs_ctx, &fdt, fd, flags)?;
    if ctx.args[1] != 0 && ctx.args[2] != 0 {
        copy_sockaddr_to_user(ctx.args[1], ctx.args[2], addr.as_deref())?;
    }
    Ok(new_fd.as_raw() as usize)
}

fn getsockname_common(ctx: &mut SyscallContext<'_>, peer: bool) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    ctx.ensure_network_execution_scope();
    let raw = if peer {
        vfs_socket::getpeername(&fdt, fd)?
    } else {
        vfs_socket::getsockname(&fdt, fd)?
    };
    copy_sockaddr_to_user(ctx.args[1], ctx.args[2], Some(&raw))?;
    Ok(0)
}

fn write_linux_stat(user: usize, st: &FileStat) -> Result<(), Errno> {
    let mut out = [0u8; 128];
    put_u64(&mut out, 0, encode_dev_t(st.dev));
    put_u64(&mut out, 8, st.ino);
    put_u32(&mut out, 16, st.mode);
    put_u32(&mut out, 20, st.nlink);
    put_u32(&mut out, 24, st.uid);
    put_u32(&mut out, 28, st.gid);
    put_u64(&mut out, 32, encode_dev_t(st.rdev));
    put_i64(&mut out, 48, st.size);
    put_u32(&mut out, 56, st.blksize);
    put_u64(&mut out, 64, st.blocks);
    put_i64(&mut out, 72, st.atime.secs);
    put_u64(&mut out, 80, st.atime.nsecs as u64);
    put_i64(&mut out, 88, st.mtime.secs);
    put_u64(&mut out, 96, st.mtime.nsecs as u64);
    put_i64(&mut out, 104, st.ctime.secs);
    put_u64(&mut out, 112, st.ctime.nsecs as u64);
    copy_to_user(user, &out).map_err(|e| e.as_errno())
}

fn write_linux_statx(
    user: usize,
    st: &FileStat,
    mnt_id: u64,
    dio_mem_align: u32,
    dio_offset_align: u32,
    requested_mask: u32,
) -> Result<(), Errno> {
    let mut out = [0u8; 256];
    let rdev = statx_dev_components(st.rdev);
    let dev = statx_dev_components(st.dev);
    // 可回报字段 = 基本统计 + btime（用 ctime 近似，VFS 无 crtime）+ mnt_id +
    // DIO 对齐（仅后端支持直接 I/O 时）。mask=0 按 Linux 语义等价 STATX_BASIC_STATS。
    let mut report_mask = STATX_BASIC_STATS | STATX_BTIME | STATX_MNT_ID;
    if dio_mem_align != 0 {
        report_mask |= STATX_DIOALIGN;
    }
    let effective_mask = if requested_mask == 0 {
        STATX_BASIC_STATS
    } else {
        requested_mask
    } & report_mask;
    put_u32(&mut out, 0, effective_mask);
    put_u32(&mut out, 4, st.blksize);
    // stx_attributes / stx_attributes_mask：本 VFS 不追踪 inode 属性标志
    // （IMMUTABLE/APPEND/COMPRESSED 等），按 Linux 语义置 0。
    put_u64(&mut out, 8, 0);
    put_u32(&mut out, 16, st.nlink);
    put_u32(&mut out, 20, st.uid);
    put_u32(&mut out, 24, st.gid);
    put_u16(&mut out, 28, st.mode as u16);
    put_u64(&mut out, 32, st.ino);
    put_u64(&mut out, 40, st.size.max(0) as u64);
    put_u64(&mut out, 48, st.blocks);
    put_u64(&mut out, 56, 0);
    put_statx_timestamp(&mut out, 64, st.atime);
    put_statx_timestamp(&mut out, 80, st.ctime);
    put_statx_timestamp(&mut out, 96, st.ctime);
    put_statx_timestamp(&mut out, 112, st.mtime);
    put_u32(&mut out, 128, rdev.major);
    put_u32(&mut out, 132, rdev.minor);
    put_u32(&mut out, 136, dev.major);
    put_u32(&mut out, 140, dev.minor);
    put_u64(&mut out, 144, mnt_id);
    put_u32(&mut out, 152, dio_mem_align);
    put_u32(&mut out, 156, dio_offset_align);
    copy_to_user(user, &out).map_err(|e| e.as_errno())
}

fn statx_dev_components(dev: DevId) -> DevId {
    decode_dev_t(encode_dev_t(dev))
}

fn put_statx_timestamp(out: &mut [u8], off: usize, ts: Timespec) {
    put_i64(out, off, ts.secs);
    put_u32(out, off + 8, ts.nsecs);
}

fn put_u16(out: &mut [u8], off: usize, v: u16) {
    out[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut [u8], off: usize, v: u32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_i32(out: &mut [u8], off: usize, v: i32) {
    out[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut [u8], off: usize, v: u64) {
    out[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_i64(out: &mut [u8], off: usize, v: i64) {
    out[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn align_up(x: usize, align: usize) -> Option<usize> {
    if align == 0 || (align & (align - 1)) != 0 {
        return None;
    }
    (x.checked_add(align - 1)? & !(align - 1)).into()
}

fn file_type_to_d_type(kind: FileType) -> u8 {
    match kind {
        FileType::Fifo => 1,
        FileType::CharDevice => 2,
        FileType::Directory => 4,
        FileType::BlockDevice => 6,
        FileType::Regular => 8,
        FileType::Symlink => 10,
        FileType::Socket => 12,
    }
}

fn write_linux_statfs(user: usize, st: &FsStat) -> Result<(), Errno> {
    let mut out = [0u8; 120];
    let total_inodes = st.total_inodes;
    let free_inodes = st.free_inodes.min(total_inodes);
    put_i64(&mut out, 0, st.fs_type as i64);
    put_i64(&mut out, 8, st.block_size as i64);
    put_u64(&mut out, 16, st.total_blocks);
    put_u64(&mut out, 24, st.free_blocks);
    put_u64(&mut out, 32, st.avail_blocks);
    put_u64(&mut out, 40, total_inodes);
    put_u64(&mut out, 48, free_inodes);
    put_u64(&mut out, 56, st.fs_id);
    put_i64(&mut out, 64, st.name_max as i64);
    put_i64(&mut out, 72, st.block_size as i64);
    put_i64(&mut out, 80, 0);
    copy_to_user(user, &out).map_err(|e| e.as_errno())
}

#[derive(Clone, Copy, Debug)]
struct LinuxFlock {
    lock_type: i16,
    whence: i16,
    start: i64,
    len: i64,
    pid: i32,
}

impl LinuxFlock {
    const SIZE: usize = 32;

    fn read(user: usize) -> Result<Self, Errno> {
        if user == 0 {
            return Err(Errno::EFAULT);
        }
        let mut raw = [0u8; Self::SIZE];
        copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
        Ok(Self {
            lock_type: i16::from_le_bytes(raw[0..2].try_into().unwrap()),
            whence: i16::from_le_bytes(raw[2..4].try_into().unwrap()),
            start: i64::from_le_bytes(raw[8..16].try_into().unwrap()),
            len: i64::from_le_bytes(raw[16..24].try_into().unwrap()),
            pid: i32::from_le_bytes(raw[24..28].try_into().unwrap()),
        })
    }

    fn write(self, user: usize) -> Result<(), Errno> {
        if user == 0 {
            return Err(Errno::EFAULT);
        }
        let mut raw = [0u8; Self::SIZE];
        raw[0..2].copy_from_slice(&self.lock_type.to_le_bytes());
        raw[2..4].copy_from_slice(&self.whence.to_le_bytes());
        put_i64(&mut raw, 8, self.start);
        put_i64(&mut raw, 16, self.len);
        raw[24..28].copy_from_slice(&self.pid.to_le_bytes());
        copy_to_user(user, &raw).map_err(|e| e.as_errno())
    }
}

fn record_lock_owner_pid(ctx: &SyscallContext<'_>) -> i32 {
    ctx.task()
        .thread_group()
        .leader()
        .and_then(|leader| leader.pid_root())
        .or_else(|| ctx.task().pid_root())
        .unwrap_or(0)
}

fn linux_flock_type(raw: i16) -> Result<vfs::record_lock::RecordLockType, Errno> {
    match raw {
        F_RDLCK => Ok(vfs::record_lock::RecordLockType::Read),
        F_WRLCK => Ok(vfs::record_lock::RecordLockType::Write),
        F_UNLCK => Ok(vfs::record_lock::RecordLockType::Unlock),
        _ => Err(Errno::EINVAL),
    }
}

fn linux_flock_type_raw(kind: vfs::record_lock::RecordLockType) -> i16 {
    match kind {
        vfs::record_lock::RecordLockType::Read => F_RDLCK,
        vfs::record_lock::RecordLockType::Write => F_WRLCK,
        vfs::record_lock::RecordLockType::Unlock => F_UNLCK,
    }
}

fn validate_f_owner_type(owner_type: i32) -> Result<(), Errno> {
    match owner_type {
        F_OWNER_TID | F_OWNER_PID | F_OWNER_PGRP => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

fn read_f_owner_ex(user: usize) -> Result<(i32, i32), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok((
        i32::from_le_bytes(raw[0..4].try_into().unwrap()),
        i32::from_le_bytes(raw[4..8].try_into().unwrap()),
    ))
}

fn write_f_owner_ex(user: usize, owner_type: i32, owner_pid: i32) -> Result<(), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 8];
    put_i32(&mut raw, 0, owner_type);
    put_i32(&mut raw, 4, owner_pid);
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn linux_lease_type(raw: i32) -> Result<vfs::lease::LeaseType, Errno> {
    match raw {
        raw if raw == F_RDLCK as i32 => Ok(vfs::lease::LeaseType::Read),
        raw if raw == F_WRLCK as i32 => Ok(vfs::lease::LeaseType::Write),
        raw if raw == F_UNLCK as i32 => Ok(vfs::lease::LeaseType::Unlock),
        _ => Err(Errno::EINVAL),
    }
}

fn linux_lease_type_raw(kind: vfs::lease::LeaseType) -> i32 {
    match kind {
        vfs::lease::LeaseType::Read => F_RDLCK as i32,
        vfs::lease::LeaseType::Write => F_WRLCK as i32,
        vfs::lease::LeaseType::Unlock => F_UNLCK as i32,
    }
}

fn validate_record_lock_access(
    file: &vfs::file::File,
    req: &vfs::record_lock::RecordLockRequest,
) -> Result<(), Errno> {
    match req.lock_type {
        vfs::record_lock::RecordLockType::Read if !file.flags().readable() => Err(Errno::EBADF),
        vfs::record_lock::RecordLockType::Write if !file.flags().writable() => Err(Errno::EBADF),
        _ => Ok(()),
    }
}

fn fcntl_getlk(
    ctx: &SyscallContext<'_>,
    file: &vfs::file::File,
    flock_user: usize,
    ofd: bool,
) -> Result<usize, Errno> {
    let raw = LinuxFlock::read(flock_user)?;
    let lock_type = linux_flock_type(raw.lock_type)?;
    if !file.is_seekable() {
        return Err(Errno::EINVAL);
    }
    let mut raw = raw;
    let req =
        vfs::record_lock::request_from_parts(file, lock_type, raw.whence, raw.start, raw.len)?;
    if req.lock_type == vfs::record_lock::RecordLockType::Unlock {
        raw.lock_type = F_UNLCK;
        raw.write(flock_user)?;
        return Ok(0);
    }
    let conflict = if ofd {
        // OFD owner 用打开文件描述的地址标识；`&File` 地址与 `Arc::as_ptr` 一致。
        vfs::record_lock::getlk_ofd(file, file as *const _ as usize, req)
    } else {
        vfs::record_lock::getlk(file, record_lock_owner_pid(ctx), req)
    };
    if let Some(conflict) = conflict {
        let conflict = vfs::record_lock::clipped_conflict(conflict, &req);
        raw.lock_type = linux_flock_type_raw(conflict.lock_type);
        raw.whence = 0;
        raw.start = conflict.start as i64;
        raw.len = vfs::record_lock::len_from_range(conflict.start, conflict.end) as i64;
        // F_OFD_GETLK 恒返回 l_pid = -1（owner 为打开文件描述而非进程）。
        raw.pid = if ofd { -1 } else { conflict.owner_pid };
    } else {
        raw.lock_type = F_UNLCK;
    }
    raw.write(flock_user)?;
    Ok(0)
}

fn fcntl_setlk(
    ctx: &SyscallContext<'_>,
    file: &vfs::file::File,
    flock_user: usize,
    wait: bool,
    ofd: bool,
) -> Result<usize, Errno> {
    let raw = LinuxFlock::read(flock_user)?;
    let lock_type = linux_flock_type(raw.lock_type)?;
    if !file.is_seekable() {
        return Err(Errno::EINVAL);
    }
    let req =
        vfs::record_lock::request_from_parts(file, lock_type, raw.whence, raw.start, raw.len)?;
    validate_record_lock_access(file, &req)?;
    if ofd {
        vfs::record_lock::setlk_ofd(file, file as *const _ as usize, req, wait)?;
    } else {
        vfs::record_lock::setlk(file, record_lock_owner_pid(ctx), req, wait)?;
    }
    Ok(0)
}

fn clock_now_ns(clock_id: usize) -> Result<u64, Errno> {
    match clock_id {
        id if id == crate::vdso::CLOCK_REALTIME => Ok(crate::vdso::realtime_ns()),
        id if id == crate::vdso::CLOCK_MONOTONIC || id == crate::vdso::CLOCK_BOOTTIME => {
            Ok(crate::vdso::monotonic_ns())
        }
        _ => Err(Errno::EINVAL),
    }
}

fn read_timespec_ns_pair(raw: &[u8], off: usize) -> Result<u64, Errno> {
    let sec = i64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[off + 8..off + 16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok((sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64))
}

fn read_itimerspec(user: usize) -> Result<vfs::timerfd::TimerSpec, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 32];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(vfs::timerfd::TimerSpec {
        interval_ns: read_timespec_ns_pair(&raw, 0)?,
        value_ns: read_timespec_ns_pair(&raw, 16)?,
    })
}

fn put_timespec_ns(out: &mut [u8], off: usize, ns: u64) {
    put_i64(out, off, (ns / 1_000_000_000) as i64);
    put_i64(out, off + 8, (ns % 1_000_000_000) as i64);
}

fn write_itimerspec(user: usize, spec: vfs::timerfd::TimerSpec) -> Result<(), Errno> {
    if user == 0 {
        return Ok(());
    }
    let mut raw = [0u8; 32];
    put_timespec_ns(&mut raw, 0, spec.interval_ns);
    put_timespec_ns(&mut raw, 16, spec.value_ns);
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn timerfd_gettime_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let curr_value = ctx.args[1];
    if curr_value == 0 {
        return Err(Errno::EFAULT);
    }
    let file = file_for_fd(fd)?;
    let timer = file
        .downcast_ops::<vfs::timerfd::TimerfdFileOps>()
        .ok_or(Errno::EINVAL)?;
    write_itimerspec(curr_value, timer.get_time(crate::vdso::monotonic_ns()))?;
    Ok(0)
}

fn timerfd_settime_common(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let flags = ctx.args[1];
    if (flags & !TFD_TIMER_SUPPORTED_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let new_value = read_itimerspec(ctx.args[2])?;
    let old_value = ctx.args[3];
    let file = file_for_fd(fd)?;
    let timer = file
        .downcast_ops::<vfs::timerfd::TimerfdFileOps>()
        .ok_or(Errno::EINVAL)?;
    let now_mono = crate::vdso::monotonic_ns();
    let deadline = if new_value.value_ns == 0 {
        None
    } else if (flags & TFD_TIMER_ABSTIME) != 0 {
        // timerfd 内部只保存单调 deadline；绝对实时钟在 syscall 边界换算成
        // “从当前单调时间起还剩多久”，避免 fd 对象依赖全局 realtime offset。
        let clock_now = clock_now_ns(timer.clock_id())?;
        let delta = new_value.value_ns.saturating_sub(clock_now);
        Some(now_mono.saturating_add(delta))
    } else {
        Some(now_mono.saturating_add(new_value.value_ns))
    };
    // Linux：CANCEL_ON_SET 在 settime 时按 (CLOCK_REALTIME + ABSTIME + 标志)
    // 登记/注销；定时器照常 arm，但若此前已被时钟设置取消则返回 ECANCELED。
    timer.update_cancel_registration(flags);
    let old = timer
        .set_deadline(now_mono, deadline, new_value.interval_ns)
        .map_err(|e| e.to_errno())?;
    write_itimerspec(old_value, old)?;
    Ok(0)
}

fn read_timespec_ms_ceil(user: usize) -> Result<i64, Errno> {
    if user == 0 {
        return Ok(-1);
    }
    let mut raw = [0u8; 16];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let mut ms = sec.saturating_mul(1000);
    if nsec != 0 {
        ms = ms.saturating_add(((nsec as u64).saturating_add(999_999) / 1_000_000) as i64);
    }
    Ok(ms)
}

/// 读取相对 timespec，并保留完整纳秒精度；空指针表示无限等待。
fn read_timespec_timeout_ns(user: usize) -> Result<Option<u64>, Errno> {
    if user == 0 {
        return Ok(None);
    }
    let mut raw = [0u8; 16];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(Some(
        (sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(nsec as u64),
    ))
}

fn read_socket_timeout_deadline(user: usize) -> Result<Option<u64>, Errno> {
    if user == 0 {
        return Ok(None);
    }
    let mut raw = [0u8; 16];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    let delta_ns = (sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64);
    Ok(Some(sched::now_ns_direct().saturating_add(delta_ns)))
}
