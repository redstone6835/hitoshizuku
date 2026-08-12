//! VFS 导出的 ELM provider 规格。
//!
//! VFS 的路径、读写与文件对象语义不写入 ELM Core。Core 只登记这些
//! provider 入口，后续真实实现由 VFS 在本模块内逐步补齐。

use alloc::sync::Arc;
use core::str;

use elm_model::{
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_NOT_FOUND, ELM_CALL_STATUS_OK,
    ELM_CALL_STATUS_PROVIDER_FAULT, ELM_CALL_STATUS_UNSUPPORTED, ELM_CTL_ABI_VERSION,
    ELM_FRAME_PAYLOAD_LEN, ELM_KERNEL_PROVIDER_FLAG_NONE, ELM_MGR_API_KIND_SUBSYSTEM, ElmCallFrame,
    ElmKernelProviderSpec, ElmPortAccessPolicy, ElmReplyFrame, FlowDirection, FlowMode,
};
use errno::Errno;

use crate::VfsContext;
use crate::error::VfsError;
use crate::path::{self, Dirfd, LookupFlags};
use crate::stat::FileStat;

pub const ELM_VFS_LOOKUP_OPCODE_QUERY: u32 = 1;
pub const ELM_VFS_LOOKUP_DIRFD_CWD: u32 = 0;
pub const ELM_VFS_LOOKUP_PATH_LEN: usize = 240;
pub const ELM_VFS_LOOKUP_REPLY_PATH_LEN: usize = 128;
pub const ELM_VFS_LOOKUP_REQUEST_HEADER_LEN: usize = 16;
pub const ELM_VFS_LOOKUP_REPLY_FIXED_LEN: usize = 116;
pub const ELM_VFS_LOOKUP_FLAG_NONE: u16 = 0;
pub const ELM_VFS_LOOKUP_REPLY_FLAG_NONE: u16 = 0;
pub const ELM_VFS_LOOKUP_CAP_QUERY: u64 = 1 << 0;

pub const ELM_VFS_LOOKUP_F_NO_FOLLOW: u32 = LookupFlags::NO_FOLLOW.raw();
pub const ELM_VFS_LOOKUP_F_DIRECTORY: u32 = LookupFlags::DIRECTORY.raw();
pub const ELM_VFS_LOOKUP_F_NO_SYMLINKS: u32 = LookupFlags::NO_SYMLINKS.raw();
pub const ELM_VFS_LOOKUP_F_NO_MOUNT_LAST: u32 = LookupFlags::NO_MOUNT_LAST.raw();

const ELM_VFS_LOOKUP_ALLOWED_FLAGS: u32 = ELM_VFS_LOOKUP_F_NO_FOLLOW
    | ELM_VFS_LOOKUP_F_DIRECTORY
    | ELM_VFS_LOOKUP_F_NO_SYMLINKS
    | ELM_VFS_LOOKUP_F_NO_MOUNT_LAST;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmVfsLookupRequest {
    pub abi_version: u16,
    pub flags: u16,
    pub dirfd_kind: u32,
    pub lookup_flags: u32,
    pub path_len: u16,
    pub reserved: u16,
    pub path: [u8; ELM_VFS_LOOKUP_PATH_LEN],
}

impl ElmVfsLookupRequest {
    pub fn new(path: &str) -> Self {
        let mut out = Self {
            abi_version: ELM_CTL_ABI_VERSION,
            flags: ELM_VFS_LOOKUP_FLAG_NONE,
            dirfd_kind: ELM_VFS_LOOKUP_DIRFD_CWD,
            lookup_flags: 0,
            path_len: 0,
            reserved: 0,
            path: [0; ELM_VFS_LOOKUP_PATH_LEN],
        };
        let bytes = path.as_bytes();
        let len = bytes.len().min(ELM_VFS_LOOKUP_PATH_LEN);
        out.path[..len].copy_from_slice(&bytes[..len]);
        out.path_len = len as u16;
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmVfsLookupReply {
    pub abi_version: u16,
    pub flags: u16,
    pub errno: i32,
    pub file_type: u32,
    pub mode: u32,
    pub ino: u64,
    pub size: i64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub rdev_major: u32,
    pub rdev_minor: u32,
    pub blksize: u32,
    pub blocks: u64,
    pub atime_secs: i64,
    pub atime_nsecs: u32,
    pub mtime_secs: i64,
    pub mtime_nsecs: u32,
    pub ctime_secs: i64,
    pub ctime_nsecs: u32,
    pub resolved_path_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub resolved_path: [u8; ELM_VFS_LOOKUP_REPLY_PATH_LEN],
}

impl ElmVfsLookupReply {
    fn error(errno: Errno) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            flags: ELM_VFS_LOOKUP_REPLY_FLAG_NONE,
            errno: errno.as_i32(),
            file_type: 0,
            mode: 0,
            ino: 0,
            size: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            dev_major: 0,
            dev_minor: 0,
            rdev_major: 0,
            rdev_minor: 0,
            blksize: 0,
            blocks: 0,
            atime_secs: 0,
            atime_nsecs: 0,
            mtime_secs: 0,
            mtime_nsecs: 0,
            ctime_secs: 0,
            ctime_nsecs: 0,
            resolved_path_len: 0,
            reserved0: 0,
            reserved1: 0,
            resolved_path: [0; ELM_VFS_LOOKUP_REPLY_PATH_LEN],
        }
    }

    fn from_stat(stat: FileStat) -> Self {
        Self {
            abi_version: ELM_CTL_ABI_VERSION,
            flags: ELM_VFS_LOOKUP_REPLY_FLAG_NONE,
            errno: Errno::ESUCCESS.as_i32(),
            file_type: stat.mode & 0o170000,
            mode: stat.mode,
            ino: stat.ino,
            size: stat.size,
            nlink: stat.nlink,
            uid: stat.uid,
            gid: stat.gid,
            dev_major: stat.dev.major,
            dev_minor: stat.dev.minor,
            rdev_major: stat.rdev.major,
            rdev_minor: stat.rdev.minor,
            blksize: stat.blksize,
            blocks: stat.blocks,
            atime_secs: stat.atime.secs,
            atime_nsecs: stat.atime.nsecs,
            mtime_secs: stat.mtime.secs,
            mtime_nsecs: stat.mtime.nsecs,
            ctime_secs: stat.ctime.secs,
            ctime_nsecs: stat.ctime.nsecs,
            // TODO(elm): 在 VFS 内部开放可见路径序列化入口后填充规范路径。
            resolved_path_len: 0,
            reserved0: 0,
            reserved1: 0,
            resolved_path: [0; ELM_VFS_LOOKUP_REPLY_PATH_LEN],
        }
    }
}

const VFS_PROVIDERS: [ElmKernelProviderSpec; 3] = [
    ElmKernelProviderSpec::new(
        "elm.vfs",
        "lookup",
        "elm.vfs.lookup@1",
        ELM_MGR_API_KIND_SUBSYSTEM,
        ELM_VFS_LOOKUP_OPCODE_QUERY,
        ELM_VFS_LOOKUP_CAP_QUERY,
        "vfs.lookup@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
        ELM_KERNEL_PROVIDER_FLAG_NONE,
        vfs_lookup_invoke,
        None,
        None,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.vfs",
        "read",
        "elm.vfs.read@1",
        "vfs.read@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.vfs",
        "write",
        "elm.vfs.write@1",
        "vfs.write@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
];

pub fn providers() -> &'static [ElmKernelProviderSpec] {
    // TODO(elm): 将 read/write 接到文件句柄租约和 typed I/O 请求。
    &VFS_PROVIDERS
}

fn vfs_lookup_invoke(frame: ElmCallFrame) -> ElmReplyFrame {
    if frame.opcode != ELM_VFS_LOOKUP_OPCODE_QUERY || frame.flags != 0 {
        return lookup_error_reply(frame, ELM_CALL_STATUS_INVALID, Errno::EINVAL);
    }

    let request = match parse_lookup_request(&frame) {
        Ok(request) => request,
        Err(errno) => return lookup_error_reply(frame, ELM_CALL_STATUS_INVALID, errno),
    };
    if request.dirfd_kind != ELM_VFS_LOOKUP_DIRFD_CWD {
        return lookup_error_reply(frame, ELM_CALL_STATUS_UNSUPPORTED, Errno::EOPNOTSUPP);
    }

    let Some(ctx) = current_vfs_context() else {
        return lookup_error_reply(frame, ELM_CALL_STATUS_NOT_FOUND, Errno::EBADF);
    };
    let lookup_flags = LookupFlags(request.lookup_flags);
    let path = match str::from_utf8(request.path_bytes()) {
        Ok(path) => path,
        Err(_) => return lookup_error_reply(frame, ELM_CALL_STATUS_INVALID, Errno::EINVAL),
    };

    let result = match path::lookup(&ctx, &Dirfd::Cwd, path, lookup_flags) {
        Ok(result) => result,
        Err(err) => {
            return lookup_error_reply(frame, call_status_from_vfs_error(err), err.to_errno());
        }
    };
    let Some(inode) = result.dentry.inode() else {
        return lookup_error_reply(frame, ELM_CALL_STATUS_NOT_FOUND, Errno::ENOENT);
    };
    match inode.stat() {
        Ok(stat) => lookup_success_reply(frame, ElmVfsLookupReply::from_stat(stat)),
        Err(err) => lookup_error_reply(frame, call_status_from_vfs_error(err), err.to_errno()),
    }
}

fn current_vfs_context() -> Option<Arc<VfsContext>> {
    if !sched::is_ready() {
        return None;
    }
    let payload = sched::current_task().ext_lookup(sched::TASKEXT_VFS_CONTEXT)?;
    payload.downcast::<VfsContext>().ok()
}

fn parse_lookup_request(frame: &ElmCallFrame) -> Result<ParsedLookupRequest<'_>, Errno> {
    let len = frame.payload_len as usize;
    if len < ELM_VFS_LOOKUP_REQUEST_HEADER_LEN {
        return Err(Errno::EINVAL);
    }

    let payload = &frame.payload[..len];
    let abi_version = read_u16(payload, 0);
    let flags = read_u16(payload, 2);
    let dirfd_kind = read_u32(payload, 4);
    let lookup_flags = read_u32(payload, 8);
    let path_len = read_u16(payload, 12) as usize;
    let reserved = read_u16(payload, 14);

    if abi_version != ELM_CTL_ABI_VERSION
        || flags != ELM_VFS_LOOKUP_FLAG_NONE
        || reserved != 0
        || path_len > ELM_VFS_LOOKUP_PATH_LEN
        || lookup_flags & !ELM_VFS_LOOKUP_ALLOWED_FLAGS != 0
        || len < ELM_VFS_LOOKUP_REQUEST_HEADER_LEN + path_len
    {
        return Err(Errno::EINVAL);
    }

    let path =
        &payload[ELM_VFS_LOOKUP_REQUEST_HEADER_LEN..ELM_VFS_LOOKUP_REQUEST_HEADER_LEN + path_len];
    if path.is_empty() || path.contains(&0) {
        return Err(Errno::EINVAL);
    }

    Ok(ParsedLookupRequest {
        dirfd_kind,
        lookup_flags,
        path,
    })
}

struct ParsedLookupRequest<'a> {
    dirfd_kind: u32,
    lookup_flags: u32,
    path: &'a [u8],
}

impl<'a> ParsedLookupRequest<'a> {
    fn path_bytes(&self) -> &'a [u8] {
        self.path
    }
}

fn lookup_success_reply(frame: ElmCallFrame, reply: ElmVfsLookupReply) -> ElmReplyFrame {
    let mut payload = [0u8; ELM_FRAME_PAYLOAD_LEN];
    write_lookup_reply(&mut payload, &reply);
    ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        ELM_CALL_STATUS_OK,
        &payload[..ELM_VFS_LOOKUP_REPLY_FIXED_LEN + reply.resolved_path_len as usize],
    )
}

fn lookup_error_reply(frame: ElmCallFrame, status: i32, errno: Errno) -> ElmReplyFrame {
    let mut payload = [0u8; ELM_FRAME_PAYLOAD_LEN];
    let reply = ElmVfsLookupReply::error(errno);
    write_lookup_reply(&mut payload, &reply);
    ElmReplyFrame::new(
        frame.binding_id,
        frame.call_id,
        status,
        &payload[..ELM_VFS_LOOKUP_REPLY_FIXED_LEN],
    )
}

fn call_status_from_vfs_error(err: VfsError) -> i32 {
    match err {
        VfsError::NotFound => ELM_CALL_STATUS_NOT_FOUND,
        VfsError::InvalidArgument | VfsError::NameTooLong | VfsError::FileTooLarge => {
            ELM_CALL_STATUS_INVALID
        }
        VfsError::NotSupported => ELM_CALL_STATUS_UNSUPPORTED,
        _ => ELM_CALL_STATUS_PROVIDER_FAULT,
    }
}

fn write_lookup_reply(out: &mut [u8], reply: &ElmVfsLookupReply) {
    write_u16(out, 0, reply.abi_version);
    write_u16(out, 2, reply.flags);
    write_i32(out, 4, reply.errno);
    write_u32(out, 8, reply.file_type);
    write_u32(out, 12, reply.mode);
    write_u64(out, 16, reply.ino);
    write_i64(out, 24, reply.size);
    write_u32(out, 32, reply.nlink);
    write_u32(out, 36, reply.uid);
    write_u32(out, 40, reply.gid);
    write_u32(out, 44, reply.dev_major);
    write_u32(out, 48, reply.dev_minor);
    write_u32(out, 52, reply.rdev_major);
    write_u32(out, 56, reply.rdev_minor);
    write_u32(out, 60, reply.blksize);
    write_u64(out, 64, reply.blocks);
    write_i64(out, 72, reply.atime_secs);
    write_u32(out, 80, reply.atime_nsecs);
    write_i64(out, 84, reply.mtime_secs);
    write_u32(out, 92, reply.mtime_nsecs);
    write_i64(out, 96, reply.ctime_secs);
    write_u32(out, 104, reply.ctime_nsecs);
    write_u16(out, 108, reply.resolved_path_len);
    write_u16(out, 110, reply.reserved0);
    write_u32(out, 112, reply.reserved1);
    let path_len = (reply.resolved_path_len as usize).min(ELM_VFS_LOOKUP_REPLY_PATH_LEN);
    out[ELM_VFS_LOOKUP_REPLY_FIXED_LEN..ELM_VFS_LOOKUP_REPLY_FIXED_LEN + path_len]
        .copy_from_slice(&reply.resolved_path[..path_len]);
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(out: &mut [u8], offset: usize, value: i32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_i64(out: &mut [u8], offset: usize, value: i64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
