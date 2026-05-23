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
use vfs::error::VfsError;
use vfs::fdtable::{Fd, FdFlags};
use vfs::file::{AccessMode, DirEntry, IoctlCmd, OpenOptions, PollEvents, SeekFrom};
use vfs::mount::MountFlags;
use vfs::operation;
use vfs::path::Dirfd;
use vfs::stat::{DevId, FileMode, FileStat, FileType, FsStat, Timespec};

/// 单次最多从用户态拷到内核临时缓冲的字节数。
const COPY_CHUNK: usize = 256;
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
const F_DUPFD_CLOEXEC: usize = 1030;
const FD_CLOEXEC: usize = 1;

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
    let offset = ctx.args[3] as u64;
    let file = file_for_fd(fd)?;
    read_to_user(&file, buf, len, Some(offset))
}

pub(super) fn sys_pwrite64(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let buf = ctx.args[1];
    let len = ctx.args[2];
    let offset = ctx.args[3] as u64;
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
        _ => Err(Errno::EINVAL),
    }
}

pub(super) fn sys_ioctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let file = file_for_fd(fd_arg(ctx.args[0])?)?;
    let cmd = IoctlCmd::new(ctx.args[1] & u32::MAX as usize);
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
    copy_to_user(fds_user, fds_bytes).map_err(|e| e.as_errno())?;

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
    let dev_id = DevId::new(
        ((dev >> 8) & 0xfff) as u32,
        (dev & 0xff | ((dev >> 12) & !0xff)) as u32,
    );
    operation::mknodat(&vfs_ctx, &dirfd, &path, kind, file_mode, dev_id)
        .map_err(|e| e.to_errno())?;
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
    use vfs::cred::{Gid, Uid};
    let vfs_ctx = current_vfs_context().ok_or(Errno::EBADF)?;
    let fdt = current_fdtable().ok_or(Errno::EBADF)?;
    let dirfd = dirfd_arg(ctx.args[0], &fdt)?;
    let path = copy_cstr_from_user(ctx.args[1], PATH_MAX).map_err(|e| e.as_errno())?;
    let uid_raw = ctx.args[2] as u32;
    let gid_raw = ctx.args[3] as u32;
    let flags = ctx.args[4];
    let no_follow = (flags & AT_SYMLINK_NOFOLLOW) != 0;
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
    operation::fchownat(&vfs_ctx, &dirfd, &path, uid, gid, no_follow).map_err(|e| e.to_errno())?;
    Ok(0)
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
    let size = ctx.args[1] as u64;
    let dirfd = Dirfd::Cwd;
    operation::truncate(&vfs_ctx, &dirfd, &path, size).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_ftruncate(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fd = fd_arg(ctx.args[0])?;
    let size = ctx.args[1] as u64;
    let file = file_for_fd(fd)?;
    if !file.flags().writable() {
        return Err(Errno::EINVAL);
    }
    file.inode().set_size(size);
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
    file.readdir(&mut |entry| {
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
        if copy_to_user(dirent + buf_pos, &raw).is_err() {
            return ControlFlow::Break(());
        }
        buf_pos += reclen;
        ControlFlow::Continue(())
    })
    .map_err(|e| e.to_errno())?;

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
    let _mode = ctx.args[1];
    let offset = ctx.args[2] as u64;
    let len = ctx.args[3] as u64;
    let file = file_for_fd(fd)?;
    if !file.flags().writable() {
        return Err(Errno::EINVAL);
    }
    let size = file.stat().map_err(|e| e.to_errno())?.size as u64;
    let end = offset.saturating_add(len);
    if end > size {
        file.inode().set_size(end);
    }
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

pub(super) fn sys_epoll_create1(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_epoll_ctl(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_epoll_pwait(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_socket(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_bind(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_listen(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_accept(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_connect(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_getsockname(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_sendmsg(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_recvmsg(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_setsockopt(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_shutdown(_ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    Err(Errno::ENOSYS)
}

pub(super) fn sys_ppoll(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let fds_user = ctx.args[0];
    let nfds = ctx.args[1];
    let timeout_user = ctx.args[2];
    let _sigmask_user = ctx.args[3];

    const POLLFD_SIZE: usize = 8;
    const MAX_POLLFDS: usize = 1024;
    if nfds > MAX_POLLFDS {
        return Err(Errno::EINVAL);
    }
    let total_bytes = nfds.checked_mul(POLLFD_SIZE).ok_or(Errno::EINVAL)?;
    let mut pollfds = vec![0u8; total_bytes];
    copy_from_user(fds_user, &mut pollfds).map_err(|e| e.as_errno())?;

    let timeout_ms = read_timespec_ms(timeout_user);

    let deadline = if timeout_ms >= 0 {
        Some(sched::now_ns_public() + (timeout_ms as u64) * 1_000_000)
    } else {
        None
    };
    loop {
        let mut any_ready = false;

        for i in 0..nfds {
            let off = i * POLLFD_SIZE;
            let fd_raw = i32::from_le_bytes(pollfds[off..off + 4].try_into().unwrap());
            let events = u16::from_le_bytes(pollfds[off + 4..off + 6].try_into().unwrap());

            if let Ok(file) = file_for_fd(Fd::from_raw(fd_raw as u32)) {
                let interest = PollEvents(events);
                let ready = file.poll(interest);
                if ready.0 != 0 {
                    pollfds[off + 6..off + 8].copy_from_slice(&ready.0.to_le_bytes());
                    any_ready = true;
                } else {
                    pollfds[off + 6..off + 8].copy_from_slice(&0u16.to_le_bytes());
                }
            } else {
                pollfds[off + 6..off + 8].copy_from_slice(&PollEvents::POLLNVAL.0.to_le_bytes());
                any_ready = true;
            }
        }

        if any_ready {
            copy_to_user(fds_user, &pollfds).map_err(|e| e.as_errno())?;
            let mut count = 0usize;
            for i in 0..nfds {
                let off = i * POLLFD_SIZE;
                let revents = u16::from_le_bytes(pollfds[off + 6..off + 8].try_into().unwrap());
                if revents != 0 {
                    count += 1;
                }
            }
            return Ok(count);
        }

        if let Some(dl) = deadline {
            if sched::now_ns_public() >= dl {
                copy_to_user(fds_user, &pollfds).map_err(|e| e.as_errno())?;
                return Ok(0);
            }
        }

        if timeout_ms == 0 {
            // 确保 revents 已写回到用户空间
            copy_to_user(fds_user, &pollfds).map_err(|e| e.as_errno())?;
            return Ok(0);
        }

        sched::operation::sched_yield()?;
    }
}

pub(super) fn sys_pselect6(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let _nfds = ctx.args[0];
    let _readfds = ctx.args[1];
    let _writefds = ctx.args[2];
    let _exceptfds = ctx.args[3];
    let _timeout = ctx.args[4];
    let _sigmask = ctx.args[5];
    Err(Errno::ENOSYS)
}

fn fd_arg(raw: usize) -> Result<Fd, Errno> {
    let fd = raw as isize;
    if fd < 0 {
        return Err(Errno::EBADF);
    }
    Ok(Fd::from_raw(fd as u32))
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
        sync: false,
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
        copy_from_user(user_ptr, &mut tmp[..chunk]).map_err(|e| e.as_errno())?;
        let n = match if offset.is_some() {
            file.write_at(&tmp[..chunk], pos)
        } else {
            file.write(&tmp[..chunk])
        } {
            Ok(n) => n,
            Err(VfsError::WouldBlock) if written > 0 => return Ok(written),
            Err(VfsError::WouldBlock) if file.flags().nonblock => return Err(Errno::EAGAIN),
            Err(VfsError::WouldBlock) => {
                sched::operation::sched_yield()?;
                continue;
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
                sched::operation::sched_yield()?;
                continue;
            }
            Err(e) => return Err(e.to_errno()),
        };
        if n == 0 {
            break;
        }
        copy_to_user(user_ptr, &tmp[..n]).map_err(|e| e.as_errno())?;
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

fn write_linux_stat(user: usize, st: &FileStat) -> Result<(), Errno> {
    let mut out = [0u8; 128];
    put_u64(&mut out, 0, encode_dev(st.dev));
    put_u64(&mut out, 8, st.ino);
    put_u32(&mut out, 16, st.mode);
    put_u32(&mut out, 20, st.nlink);
    put_u32(&mut out, 24, st.uid);
    put_u32(&mut out, 28, st.gid);
    put_u64(&mut out, 32, encode_dev(st.rdev));
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
    put_u32(&mut out, 128, st.rdev.major);
    put_u32(&mut out, 132, st.rdev.minor);
    put_u32(&mut out, 136, st.dev.major);
    put_u32(&mut out, 140, st.dev.minor);
    copy_to_user(user, &out).map_err(|e| e.as_errno())
}

fn put_statx_timestamp(out: &mut [u8], off: usize, ts: Timespec) {
    put_i64(out, off, ts.secs);
    put_u32(out, off + 8, ts.nsecs);
}

fn encode_dev(dev: DevId) -> u64 {
    let major = dev.major as u64;
    let minor = dev.minor as u64;
    ((major & 0xfff) << 8) | (minor & 0xff) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
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

fn read_timespec_ms(user: usize) -> i64 {
    if user == 0 {
        return -1;
    }
    let mut raw = [0u8; 16];
    if copy_from_user(user, &mut raw).is_err() {
        return -1;
    }
    let sec = i64::from_le_bytes(raw[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    if sec < 0 || nsec < 0 || nsec >= 1_000_000_000 {
        return -1;
    }
    sec.saturating_mul(1000).saturating_add(nsec / 1_000_000)
}