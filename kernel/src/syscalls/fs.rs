//! 文件系统相关 syscall。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;

use errno::Errno;
use general::mm::{copy_cstr_from_user, copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use general::vfs::{current_fdtable, current_vfs_context, namespace_path};
use sched::{SigProcMaskHow, SigSet};
use vfs::cred::{Gid, Uid};
use vfs::error::VfsError;
use vfs::fdtable::{Fd, FdFlags};
use vfs::file::{AccessMode, IoctlCmd, OpenOptions, PollEvents, SeekFrom};
use vfs::mount::MountFlags;
use vfs::operation;
use vfs::path::Dirfd;
use vfs::socket as vfs_socket;
use vfs::stat::{DevId, FileMode, FileStat, FileType, FsStat, Timespec};
use hal::abi::{ decode_dev_t, encode_dev_t };

/// 单次最多从用户态拷到内核临时缓冲的字节数。
const COPY_CHUNK: usize = 2048;
const PATH_MAX: usize = 4096;
const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_EACCESS: usize = 0x200;
const AT_NO_AUTOMOUNT: usize = 0x800;
const AT_EMPTY_PATH: usize = 0x1000;
const AT_STATX_FORCE_SYNC: usize = 0x2000;
const AT_STATX_DONT_SYNC: usize = 0x4000;

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
const O_DIRECT: usize = 0o00040000;
const O_NOATIME: usize = 0o01000000;
const O_CLOEXEC: usize = 0o02000000;
const O_PATH: usize = 0o10000000;
const O_SYNC: usize = 0o4010000;

const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_DUPFD_CLOEXEC: usize = 1030;
const FD_CLOEXEC: usize = 1;
const FIONBIO: usize = 0x5421;

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

const MSGHDR_SIZE_64: usize = 56;
const MMSGHDR_SIZE_64: usize = 64;
const EPOLL_EVENT_SIZE_64: usize = 12;
const PSELECT6_SIGSET_ARG_SIZE_64: usize = 16;

pub(super) fn sys_write(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let file = file_for_fd(fd)?;
    write_from_user(&file, buf, len)
}

pub(super) fn sys_read(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let file = file_for_fd(fd)?;
    read_to_user(&file, buf, len, None)
}

pub(super) fn sys_pread64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let offset = nonnegative_i64_arg(ctx.args[3])?;
    let file = file_for_fd(fd)?;
    read_to_user(&file, buf, len, Some(offset))
}

pub(super) fn sys_pwrite64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let offset = nonnegative_i64_arg(ctx.args[3])?;
    let file = file_for_fd(fd)?;
    write_from_user_at(&file, buf, len, Some(offset))
}

pub(super) fn sys_writev(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    let mut total = 0usize;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov, i)?;
        match write_from_user(&file, base, len) {
            Ok(n) => {
                total = total.checked_add(n).ok_or(Errno::EINVAL)?;
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

pub(super) fn sys_readv(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let iov = ctx.args[1];
    let iovcnt = ctx.args[2];
    if iovcnt > 1024 {
        return Err(Errno::EINVAL);
    }
    let file = file_for_fd(fd)?;
    let mut total = 0usize;
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov, i)?;
        match read_to_user(&file, base, len, None) {
            Ok(n) => {
                total = total.checked_add(n).ok_or(Errno::EINVAL)?;
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

pub(super) fn sys_close(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    fdt.close_fd(fd).map_err(|e| e.to_errno())?;
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
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
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
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let stat_user = ctx.args[2];
    let flags = ctx.args[3];

    let st = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        let fd = fd_arg(raw_dirfd)?;
        operation::fstat(&fdt, fd).map_err(|e| e.to_errno())?
    } else {
        let dirfd = dirfd_arg(raw_dirfd, &fdt)?;
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
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let flags = ctx.args[2];
    let statx_user = ctx.args[4];

    const ALLOWED_FLAGS: usize = AT_SYMLINK_NOFOLLOW
        | AT_NO_AUTOMOUNT
        | AT_EMPTY_PATH
        | AT_STATX_FORCE_SYNC
        | AT_STATX_DONT_SYNC;
    if (flags & !ALLOWED_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }

    let st = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        if raw_dirfd as i32 == AT_FDCWD {
            operation::fstatat(&vfs_ctx, &Dirfd::Cwd, ".", false).map_err(|e| e.to_errno())?
        } else {
            let fd = fd_arg(raw_dirfd)?;
            operation::fstat(&fdt, fd).map_err(|e| e.to_errno())?
        }
    } else {
        let dirfd = dirfd_arg(raw_dirfd, &fdt)?;
        operation::fstatat(&vfs_ctx, &dirfd, &path, (flags & AT_SYMLINK_NOFOLLOW) != 0)
            .map_err(|e| e.to_errno())?
    };
    write_linux_statx(statx_user, &st)?;
    Ok(0)
}

pub(super) fn sys_readlinkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
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
    let mut path = namespace_path(&vfs_ctx, &vfs_ctx.cwd(), &vfs_ctx.cwd_mount())
        .unwrap_or_else(|| String::from("/"));
    if path.is_empty() {
        path.push('/');
    }
    let needed = path.len().checked_add(1).ok_or(Errno::ERANGE)?;
    if size < needed {
        return Err(Errno::ERANGE);
    }
    copy_to_user(user, path.as_bytes()).map_err(|e| e.as_errno())?;
    copy_to_user(user + path.len(), &[0]).map_err(|e| e.as_errno())?;
    Ok(user)
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
        .dup2_fd(old_fd, new_fd, fd_flags)
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
            file.set_status_flags(
                (arg & O_APPEND) != 0,
                (arg & O_NONBLOCK) != 0,
                (arg & O_SYNC) != 0,
                (arg & O_DIRECT) != 0,
            );
            Ok(0)
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
        file.set_status_flags(flags.append, enabled, flags.sync, flags.direct);
        return Ok(0);
    }
    file.ioctl(cmd, ctx.args[2])
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
        vfs::pipe::new_pipe(vfs_ctx.cred.clone(), nonblock).map_err(|e| e.to_errno())?;

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
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let mode = FileMode::new((ctx.args[2] & 0o7777) as u16);
    operation::mkdirat(&vfs_ctx, &dirfd, &path, mode).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_unlinkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    const AT_REMOVEDIR: usize = 0x200;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let flags = ctx.args[2];
    if (flags & AT_REMOVEDIR) != 0 {
        operation::rmdir(&vfs_ctx, &dirfd, &path).map_err(|e| e.to_errno())?;
    } else {
        operation::unlink(&vfs_ctx, &dirfd, &path).map_err(|e| e.to_errno())?;
    }
    Ok(0)
}

pub(super) fn sys_renameat2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let old_dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let old_path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let new_dirfd = dirfd_arg(ctx.args[2], &fdt)?;
    let new_path = copy_cstr_from_user(ctx.args[3], PATH_MAX).map_err(|e| e.as_errno())?;
    let flags = ctx.args[4];
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    operation::renameat(&vfs_ctx, &old_dirfd, &old_path, &new_dirfd, &new_path)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_linkat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let old_dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let old_path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let new_dirfd = dirfd_arg(ctx.args[2], &fdt)?;
    let new_path = copy_cstr_from_user(ctx.args[3], PATH_MAX).map_err(|e| e.as_errno())?;
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
    let target = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
    let dirfd = dirfd_arg(ctx.args[1], &fdt)?;
    let link_path = copy_cstr_from_user(ctx.args[2], PATH_MAX).map_err(|e| e.as_errno())?;
    operation::symlinkat(&vfs_ctx, &target, &dirfd, &link_path).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_mknodat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
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
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let mode = FileMode::new((ctx.args[2] & 0o7777) as u16);
    operation::fchmodat(&vfs_ctx, &dirfd, &path, mode, false).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_fchownat(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let (uid, gid) = decode_optional_owner(ctx.args[2] as u32, ctx.args[3] as u32);
    let flags = ctx.args[4];
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
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let times_user = ctx.args[2];
    let flags = ctx.args[3];
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW) != 0;

    let (atime, mtime) = if times_user == 0 {
        (None, None)
    } else {
        let mut raw = [0u8; 32];
        copy_from_user(times_user, &mut raw).map_err(|e| e.as_errno())?;
        let read_ts = |off: usize| -> Option<Timespec> {
            let sec = i64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
            let nsec = i64::from_le_bytes(raw[off + 8..off + 16].try_into().unwrap());
            if sec == 0x3fffffff && nsec == 0x3fffffff {
                None
            } else if sec == 0x3ffffffe && nsec == 0x3ffffffe {
                Some(Timespec::ZERO)
            } else {
                Some(Timespec {
                    secs: sec,
                    nsecs: nsec as u32,
                })
            }
        };
        (read_ts(0), read_ts(16))
    };

    operation::utimensat(&vfs_ctx, &dirfd, &path, atime, mtime, no_follow)
        .map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_truncate(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
    let size = nonnegative_i64_arg(ctx.args[1])?;
    let dirfd = Dirfd::Cwd;
    operation::truncate(&vfs_ctx, &dirfd, &path, size).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_ftruncate(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let size = nonnegative_i64_arg(ctx.args[1])?;
    let file = file_for_fd(fd)?;
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
    file.sync().map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_getdents64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let dirent = ctx.args[1];
    let count = ctx.args[2];
    let file = file_for_fd(fd)?;

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
    let path = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
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
    let path = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
    let dirfd = Dirfd::Cwd;
    let result = vfs::path::lookup(&vfs_ctx, &dirfd, &path, vfs::path::LookupFlags::DIRECTORY)
        .map_err(|e| e.to_errno())?;
    let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
    let st = inode.stat().map_err(|e| e.to_errno())?;
    if !vfs_ctx.cred.can_exec(
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
    if !vfs_ctx.cred.can_exec(
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
    let path = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
    operation::chroot(&vfs_ctx, &Dirfd::Cwd, &path).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_mount(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let source = copy_optional_cstr_from_user(ctx.args[0], PATH_MAX)?;
    let target = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let fs_type = copy_optional_cstr_from_user(ctx.args[2], 64)?;
    let mount_flags_raw = ctx.args[3];
    // 接受实现的标志位以及常见的可忽略兼容位：SILENT/RELATIME。
    const KNOWN_MOUNT_FLAGS: usize = (1 << 0)
        | (1 << 1)
        | (1 << 2)
        | (1 << 3)
        | (1 << 4)
        | (1 << 10)
        | (1 << 11)
        | (1 << 12)
        | (1 << 15)
        | (1 << 21);
    if (mount_flags_raw & !KNOWN_MOUNT_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let data = copy_optional_cstr_from_user(ctx.args[4], 4096)?;

    let mut flags = MountFlags::RDONLY.without(MountFlags::RDONLY);
    if (mount_flags_raw & 1) != 0 {
        flags = flags.with(MountFlags::RDONLY);
    }
    if (mount_flags_raw & 2) != 0 {
        flags = flags.with(MountFlags::NOSUID);
    }
    if (mount_flags_raw & 4) != 0 {
        flags = flags.with(MountFlags::NODEV);
    }
    if (mount_flags_raw & 8) != 0 {
        flags = flags.with(MountFlags::NOEXEC);
    }
    if (mount_flags_raw & 16) != 0 {
        flags = flags.with(MountFlags::SYNCHRONOUS);
    }
    if (mount_flags_raw & 1024) != 0 {
        flags = flags.with(MountFlags::NOATIME);
    }
    if (mount_flags_raw & 2048) != 0 {
        flags = flags.with(MountFlags::NODIRATIME);
    }
    if (mount_flags_raw & 4096) != 0 {
        flags = flags.with(MountFlags::BIND);
    }

    let dev = if source.is_empty() {
        None
    } else {
        Some(source.as_str())
    };
    let dirfd = Dirfd::Cwd;
    if fs_type.is_empty() || fs_type == "auto" {
        return mount_autodetect(&vfs_ctx, &dirfd, &target, flags, dev, &data);
    }
    operation::mount(&vfs_ctx, &dirfd, &target, &fs_type, flags, dev, &data)
        .map_err(|e| e.to_errno())?;
    Ok(0)
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
    let new_root = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
    let put_old = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    operation::pivot_root(&vfs_ctx, &new_root, &put_old).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_umount2(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let _fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let path = copy_cstr_from_user(ctx.args[0], PATH_MAX).map_err(|e| e.as_errno())?;
    let flags = ctx.args[1];
    let force = (flags & 1) != 0;
    let dirfd = Dirfd::Cwd;
    operation::umount(&vfs_ctx, &dirfd, &path, force).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_sync(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(0)
}

pub(super) fn sys_syncfs(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let file = file_for_fd(fd)?;
    file.sync().map_err(|e| e.to_errno())?;
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
    let _flags = ctx.args[5];

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
    let mode = ctx.args[1];
    let offset = nonnegative_i64_arg(ctx.args[2])?;
    let len = nonnegative_i64_arg(ctx.args[3])?;
    if mode != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let file = file_for_fd(fd)?;
    if !file.flags().writable() {
        return Err(Errno::EINVAL);
    }
    file.fallocate(offset, len).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_readahead(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(0)
}

pub(super) fn sys_fadvise64(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Ok(0)
}

pub(super) fn sys_flock(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_close_range(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let first = ctx.args[0] as u32;
    let last = ctx.args[1] as u32;
    let flags = ctx.args[2];

    const CLOSE_RANGE_CLOEXEC: usize = 1 << 2;
    if (flags & !CLOSE_RANGE_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    if first > last {
        return Err(Errno::EINVAL);
    }

    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let cloexec = (flags & CLOSE_RANGE_CLOEXEC) != 0;
    fdt.close_range(first, last, cloexec);
    Ok(0)
}

pub(super) fn sys_eventfd2(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timerfd_create(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timerfd_settime(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_timerfd_gettime(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_signalfd4(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_epoll_create1(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let flags = ctx.args[0];
    if (flags & !O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fd = vfs::epoll::create(&fdt, vfs_ctx.cred.clone(), (flags & O_CLOEXEC) != 0)?;
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
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let epfd = fd_arg(ctx.args[0])?;
    let events_user = ctx.args[1];
    let maxevents = ctx.args[2];
    let timeout_ms = ctx.args[3] as i32 as i64;
    let sigmask = read_direct_sigmask(ctx.args[4], ctx.args[5])?;
    if maxevents == 0 {
        return Err(Errno::EINVAL);
    }
    let _mask_guard = TemporarySigmask::install(sigmask);
    let ready = vfs::epoll::wait(&fdt, epfd, maxevents, timeout_ms)?;
    write_epoll_events(events_user, &ready)?;
    Ok(ready.len())
}

pub(super) fn sys_socket(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = vfs_socket::socket(&vfs_ctx, &fdt, _ctx.args[0], _ctx.args[1], _ctx.args[2])?;
    Ok(fd.as_raw() as usize)
}

pub(super) fn sys_socketpair(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
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
    vfs_socket::bind(&vfs_ctx, &fdt, fd, &addr)?;
    Ok(0)
}

pub(super) fn sys_listen(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let backlog = (ctx.args[1] as i32).max(0) as usize;
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
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let len = ctx.args[2];
    let mut data = vec![0u8; len];
    copy_from_user(ctx.args[1], &mut data).map_err(|e| e.as_errno())?;
    let addr = if ctx.args[4] == 0 {
        None
    } else {
        Some(copy_sockaddr_from_user(ctx.args[4], ctx.args[5])?)
    };
    let sent = vfs_socket::send(&vfs_ctx, &fdt, fd, &data, &[], addr.as_deref(), ctx.args[3])
        .map_err(|err| {
            if err == Errno::EPIPE && (ctx.args[3] & vfs_socket::MSG_NOSIGNAL) == 0 {
                deliver_sigpipe();
            }
            err
        })?;
    Ok(sent)
}

pub(super) fn sys_recvfrom(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let len = ctx.args[2];
    let mut data = vec![0u8; len];
    let want_addr = ctx.args[4] != 0 && ctx.args[5] != 0;
    let out = vfs_socket::recv(&fdt, fd, &mut data, 0, want_addr, ctx.args[3], None)?;
    if out.len != 0 {
        copy_to_user(ctx.args[1], &data[..out.len]).map_err(|e| e.as_errno())?;
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
    let mut sent_count = 0usize;
    for index in 0..vlen {
        let user = msgvec_ptr(msgvec_user, index)?;
        let hdr = read_mmsghdr(user)?;
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
    let total = iov_total_len(hdr.iov, hdr.iovlen)?;
    let mut data = vec![0u8; total];
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
    let mut recv_count = 0usize;
    for index in 0..vlen {
        let user = msgvec_ptr(msgvec_user, index)?;
        let mut hdr = read_mmsghdr(user)?;
        let total = iov_total_len(hdr.msg_hdr.iov, hdr.msg_hdr.iovlen)?;
        let mut data = vec![0u8; total];
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

pub(super) fn sys_setsockopt(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let value = copy_user_region(ctx.args[3], ctx.args[4])?;
    vfs_socket::setsockopt(&fdt, fd, ctx.args[1] as i32, ctx.args[2] as i32, &value)?;
    Ok(0)
}

pub(super) fn sys_getsockopt(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    let value = vfs_socket::getsockopt(&fdt, fd, ctx.args[1] as i32, ctx.args[2] as i32)?;
    copy_optval_to_user(ctx.args[3], ctx.args[4], &value)?;
    Ok(0)
}

pub(super) fn sys_shutdown(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
    vfs_socket::shutdown(&fdt, fd, ctx.args[1])?;
    Ok(0)
}

pub(super) fn sys_ppoll(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fds_user = ctx.args[0];
    let nfds = ctx.args[1];
    let timeout_user = ctx.args[2];
    let sigmask = read_direct_sigmask(ctx.args[3], ctx.args[4])?;

    const POLLFD_SIZE: usize = 8;
    const MAX_POLLFDS: usize = 1024;
    if nfds > MAX_POLLFDS {
        return Err(Errno::EINVAL);
    }
    let total_bytes = nfds.checked_mul(POLLFD_SIZE).ok_or(Errno::EINVAL)?;
    let mut pollfds = vec![0u8; total_bytes];
    copy_from_user(fds_user, &mut pollfds).map_err(|e| e.as_errno())?;

    let timeout_ms = read_timespec_ms(timeout_user)?;
    let _mask_guard = TemporarySigmask::install(sigmask);

    let deadline = timeout_deadline(timeout_ms);
    loop {
        let mut count = 0usize;
        let mut waiters: Vec<(Arc<vfs::file::File>, PollEvents)> = Vec::new();

        for i in 0..nfds {
            let off = i * POLLFD_SIZE;
            let fd_raw = i32::from_le_bytes(pollfds[off..off + 4].try_into().unwrap());
            let events = u16::from_le_bytes(pollfds[off + 4..off + 6].try_into().unwrap());
            if fd_raw < 0 {
                pollfds[off + 6..off + 8].copy_from_slice(&0u16.to_le_bytes());
                continue;
            }

            if let Ok(file) = file_for_fd(Fd::from_raw(fd_raw as u32)) {
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
        }

        if count != 0 {
            copy_to_user(fds_user, &pollfds).map_err(|e| e.as_errno())?;
            return Ok(count);
        }

        if timeout_expired(deadline) || timeout_ms == 0 {
            // 确保 revents 已写回到用户空间
            copy_to_user(fds_user, &pollfds).map_err(|e| e.as_errno())?;
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

    const MAX_SELECT_FDS: usize = 1024;
    if nfds > MAX_SELECT_FDS {
        return Err(Errno::EINVAL);
    }

    let set_len = nfds.div_ceil(8);
    let read_in = copy_fdset_from_user(readfds_user, set_len)?;
    let write_in = copy_fdset_from_user(writefds_user, set_len)?;
    let except_in = copy_fdset_from_user(exceptfds_user, set_len)?;
    let mut read_out = vec![0u8; set_len];
    let mut write_out = vec![0u8; set_len];
    let mut except_out = vec![0u8; set_len];
    let timeout_ms = read_timespec_ms(timeout_user)?;
    let _mask_guard = TemporarySigmask::install(sigmask);
    let deadline = timeout_deadline(timeout_ms);

    loop {
        clear_fdset(&mut read_out);
        clear_fdset(&mut write_out);
        clear_fdset(&mut except_out);
        let mut count = 0usize;
        let mut waiters: Vec<(Arc<vfs::file::File>, PollEvents)> = Vec::new();

        for fd_num in 0..nfds {
            let want_read = fdset_test(&read_in, fd_num);
            let want_write = fdset_test(&write_in, fd_num);
            let want_except = fdset_test(&except_in, fd_num);
            if !want_read && !want_write && !want_except {
                continue;
            }
            let file = file_for_fd(Fd::from_raw(fd_num as u32))?;
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
                fdset_set(&mut read_out, fd_num);
                fd_ready = true;
            }
            if want_write && ready.has(PollEvents::POLLOUT.with(PollEvents::POLLERR)) {
                fdset_set(&mut write_out, fd_num);
                fd_ready = true;
            }
            if want_except && ready.has(PollEvents::POLLPRI.with(PollEvents::POLLERR)) {
                fdset_set(&mut except_out, fd_num);
                fd_ready = true;
            }
            if fd_ready {
                count += 1;
            } else if !interest.is_empty() {
                waiters.push((file, interest));
            }
        }

        if count != 0 {
            copy_fdset_to_user(readfds_user, &read_out)?;
            copy_fdset_to_user(writefds_user, &write_out)?;
            copy_fdset_to_user(exceptfds_user, &except_out)?;
            return Ok(count);
        }

        if timeout_expired(deadline) || timeout_ms == 0 {
            copy_fdset_to_user(readfds_user, &read_out)?;
            copy_fdset_to_user(writefds_user, &write_out)?;
            copy_fdset_to_user(exceptfds_user, &except_out)?;
            return Ok(0);
        }

        wait_on_poll_sources(&waiters, deadline)?;
    }
}

fn timeout_deadline(timeout_ms: i64) -> Option<u64> {
    if timeout_ms >= 0 {
        Some(sched::now_ns_public().saturating_add((timeout_ms as u64).saturating_mul(1_000_000)))
    } else {
        None
    }
}

fn timeout_expired(deadline: Option<u64>) -> bool {
    deadline.is_some_and(|dl| sched::now_ns_public() >= dl)
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
    const POLL_RECHECK_NS: u64 = 10_000_000;
    let task = sched::current_task();
    if has_unblocked_signal(&task) {
        return Err(Errno::EINTR);
    }
    if timeout_expired(deadline) {
        return Ok(());
    }

    let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
    let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);

    let mut registered_waiter = false;
    for (file, interest) in sources {
        registered_waiter |= file.poll_add_waiter(&task, *interest);
    }
    // 当前网络 waiter 仍是全局粗粒度唤醒，存在丢失精确事件的窗口。
    // poll/select 不能因此永久睡眠；即使没有收到 waiter 唤醒，也按短
    // 周期重新检查 fd readiness，直到原始 timeout 到期。
    let recheck_deadline = {
        let now = sched::now_ns_public();
        let quantum = now.saturating_add(POLL_RECHECK_NS);
        Some(deadline.map_or(quantum, |dl| dl.min(quantum)))
    };
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
        return sched::operation::sched_yield();
    }

    sched::schedule_once(sched::now_ns_public());
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
        let task = sched::current_task();
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

fn nonnegative_i64_arg(raw: usize) -> Result<u64, Errno> {
    let value = raw as isize as i64;
    if value < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(value as u64)
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

fn file_for_fd(fd: Fd) -> Result<Arc<vfs::file::File>, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    fdt.get_file(fd).ok_or(Errno::EBADF)
}

fn synthetic_readlink_target(
    ctx: &SyscallContext<'_>,
    path: &str,
) -> Result<Option<String>, Errno> {
    match path {
        "/proc/self/exe" | "/proc/thread-self/exe" => crate::sched::task_exec_path(&ctx.task)
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
    let n = bytes.len().min(size - 1);
    copy_to_user(buf, &bytes[..n]).map_err(|e| e.as_errno())?;
    // 尾加 NUL，兼容 POSIX readlink(2) 语义
    copy_to_user(buf + n, &[0u8]).map_err(|e| e.as_errno())?;
    Ok(n)
}

fn faccessat_common(ctx: &mut SyscallContext<'_>, has_flags: bool) -> Result<usize, Errno> {
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let raw_dirfd = ctx.args[0];
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let mode = ctx.args[2];
    let flags = if has_flags { ctx.args[3] } else { 0 };
    if (mode & !(R_OK | W_OK | X_OK)) != 0 {
        return Err(Errno::EINVAL);
    }
    if (flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)) != 0 {
        return Err(Errno::EINVAL);
    }

    let st = if path.is_empty() && (flags & AT_EMPTY_PATH) != 0 {
        let fd = fd_arg(raw_dirfd)?;
        operation::fstat(&fdt, fd).map_err(|e| e.to_errno())?
    } else {
        let dirfd = dirfd_arg(raw_dirfd, &fdt)?;
        operation::fstatat(&vfs_ctx, &dirfd, &path, (flags & AT_SYMLINK_NOFOLLOW) != 0)
            .map_err(|e| e.to_errno())?
    };
    if mode == F_OK || access_mode_allowed(ctx, &st, mode, flags) {
        Ok(0)
    } else {
        Err(Errno::EACCES)
    }
}

fn access_mode_allowed(ctx: &SyscallContext<'_>, st: &FileStat, mode: usize, flags: usize) -> bool {
    let creds = ctx.task.credentials();
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
        sync: (raw & O_SYNC) != 0,
        direct: (raw & O_DIRECT) != 0,
        cloexec: (raw & O_CLOEXEC) != 0,
    })
}

fn write_from_user(file: &vfs::file::File, user: usize, len: usize) -> Result<usize, Errno> {
    write_from_user_at(file, user, len, None)
}

fn write_from_user_at(
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
) -> Result<usize, Errno> {
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
        let n = match if offset.is_some() {
            file.write_at(&tmp[..chunk], pos)
        } else {
            file.write(&tmp[..chunk])
        } {
            Ok(n) => n,
            Err(VfsError::WouldBlock) if written > 0 => return Ok(written),
            Err(VfsError::WouldBlock) if file.flags().nonblock => return Err(Errno::EAGAIN),
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

fn read_to_user(
    file: &vfs::file::File,
    user: usize,
    len: usize,
    offset: Option<u64>,
) -> Result<usize, Errno> {
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
            Err(VfsError::WouldBlock) if file.flags().nonblock => return Err(Errno::EAGAIN),
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

fn wait_for_file_readiness(file: &vfs::file::File, interest: PollEvents) -> Result<(), Errno> {
    const IO_RECHECK_NS: u64 = 10_000_000;
    let task = sched::current_task();
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
        let now = sched::now_ns_public();
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

    if registered || deadline_armed {
        sched::schedule_once(sched::now_ns_public());
        if registered {
            file.poll_remove_waiter(&task);
        }
        if deadline_armed {
            sched::cancel_sleep_deadline(&task);
        }
        restore_current_task_after_wait(&task);
    } else {
        restore_current_task_after_wait(&task);
        sched::operation::sched_yield()?;
    }

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
    let task = sched::current_task();
    let creds = task.credentials();
    let info = sched::SigInfo {
        sig: sched::SignalNumber::SIGPIPE,
        code: 0,
        sender_pid: task.pid_root().unwrap_or(0),
        sender_uid: creds.uid,
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
        data: u64::from_le_bytes(raw[4..12].try_into().unwrap()),
    })
}

fn write_epoll_events(user: usize, events: &[vfs::epoll::EpollEvent]) -> Result<(), Errno> {
    for (index, event) in events.iter().enumerate() {
        let mut raw = [0u8; EPOLL_EVENT_SIZE_64];
        raw[0..4].copy_from_slice(&event.events.to_le_bytes());
        raw[4..12].copy_from_slice(&event.data.to_le_bytes());
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
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut out = vec![0u8; len];
    copy_from_user(user, &mut out).map_err(|e| e.as_errno())?;
    Ok(out)
}

fn copy_sockaddr_from_user(user: usize, len: usize) -> Result<Vec<u8>, Errno> {
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    copy_user_region(user, len)
}

fn read_socklen_user(user: usize) -> Result<usize, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut raw = [0u8; 4];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(u32::from_le_bytes(raw) as usize)
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

fn iov_total_len(iov: usize, iovcnt: usize) -> Result<usize, Errno> {
    let mut total = 0usize;
    for i in 0..iovcnt {
        let (_, len) = read_iovec(iov, i)?;
        total = total.checked_add(len).ok_or(Errno::EINVAL)?;
    }
    Ok(total)
}

fn copy_send_iovecs(iov: usize, iovcnt: usize) -> Result<Vec<u8>, Errno> {
    let total = iov_total_len(iov, iovcnt)?;
    let mut out = Vec::with_capacity(total);
    for i in 0..iovcnt {
        let (base, len) = read_iovec(iov, i)?;
        if len == 0 {
            continue;
        }
        let start = out.len();
        out.resize(start + len, 0);
        copy_from_user(base, &mut out[start..start + len]).map_err(|e| e.as_errno())?;
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
    let (new_fd, addr) = vfs_socket::accept(&vfs_ctx, &fdt, fd, flags)?;
    if ctx.args[1] != 0 && ctx.args[2] != 0 {
        copy_sockaddr_to_user(ctx.args[1], ctx.args[2], addr.as_deref())?;
    }
    Ok(new_fd.as_raw() as usize)
}

fn getsockname_common(ctx: &mut SyscallContext<'_>, peer: bool) -> Result<usize, Errno> {
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let fd = fd_arg(ctx.args[0])?;
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

fn write_linux_statx(user: usize, st: &FileStat) -> Result<(), Errno> {
    let mut out = [0u8; 256];
    let rdev = statx_dev_components(st.rdev);
    let dev = statx_dev_components(st.dev);
    put_u32(&mut out, 0, STATX_BASIC_STATS);
    put_u32(&mut out, 4, st.blksize);
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
    put_statx_timestamp(&mut out, 96, st.ctime);
    put_statx_timestamp(&mut out, 112, st.mtime);
    put_u32(&mut out, 128, rdev.major);
    put_u32(&mut out, 132, rdev.minor);
    put_u32(&mut out, 136, dev.major);
    put_u32(&mut out, 140, dev.minor);
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
    put_i64(&mut out, 0, st.fs_type as i64);
    put_i64(&mut out, 8, st.block_size as i64);
    put_u64(&mut out, 16, st.total_blocks);
    put_u64(&mut out, 24, st.free_blocks);
    put_u64(&mut out, 32, st.avail_blocks);
    put_u64(&mut out, 40, st.total_inodes);
    put_u64(&mut out, 48, st.free_inodes);
    put_u64(&mut out, 56, st.fs_id);
    put_i64(&mut out, 64, st.name_max as i64);
    put_i64(&mut out, 72, st.block_size as i64);
    copy_to_user(user, &out).map_err(|e| e.as_errno())
}

fn read_timespec_ms(user: usize) -> Result<i64, Errno> {
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
    Ok(sec.saturating_mul(1000).saturating_add(nsec / 1_000_000))
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
    Ok(Some(sched::now_ns_public().saturating_add(delta_ns)))
}
