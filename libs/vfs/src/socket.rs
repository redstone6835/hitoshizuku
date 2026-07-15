use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
#[cfg(not(test))]
use sched::current_task;
use sched::{Task, now_ns_public};
use socket::{
    HandleIdentity, PeerIdentity, Readiness, ReceiveOptions, SendOptions, Socket as CoreSocket,
    SocketError, SocketHandle, SocketLinger, SocketShutdown, SocketTimeval, SocketType,
    UnixAddress,
};

use crate::net_socket::{
    InetRecvOptions, InetRecvResult, InetSendOptions, NetSocketFileOps, SocketOptions,
};
use crate::operation;
use crate::vfs::VfsContext;
use crate::vfs::cred::Credentials;
use crate::vfs::dentry::Dentry;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::vfs::mount::{Mount, MountFlags};
use crate::vfs::path::{self, Dirfd, LookupFlags};
use crate::vfs::stat::{DevId, FileMode, FileType, FsId, Timespec};
use crate::vfs::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::vfs::sync::Spinlock;
#[cfg(test)]
use net::NetDeviceId;

pub const AF_UNIX: u16 = 1;

pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM: usize = 2;
pub const SOCK_RAW: usize = 3;
pub const SOCK_SEQPACKET: usize = 5;
pub const SOCK_TYPE_MASK: usize = 0xf;
pub const SOCK_NONBLOCK: usize = 0o00004000;
pub const SOCK_CLOEXEC: usize = 0o02000000;

pub const SOL_SOCKET: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_BROADCAST: i32 = 6;
pub const SO_KEEPALIVE: i32 = 9;
pub const SCM_RIGHTS: i32 = 1;
pub const SCM_CREDENTIALS: i32 = 2;
pub const SO_ERROR: i32 = 4;
pub const SO_DONTROUTE: i32 = 5;
pub const SO_TYPE: i32 = 3;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_LINGER: i32 = 13;
pub const SO_REUSEPORT: i32 = 15;
pub const SO_PASSCRED: i32 = 16;
pub const SO_PEERCRED: i32 = 17;
pub const SO_RCVTIMEO: i32 = 20;
pub const SO_SNDTIMEO: i32 = 21;
pub const SO_ACCEPTCONN: i32 = 30;
pub const SO_PROTOCOL: i32 = 38;
pub const SO_DOMAIN: i32 = 39;
pub const SO_RXQ_OVFL: i32 = 40;

pub const MSG_PEEK: usize = 0x0002;
pub const MSG_TRUNC: usize = 0x0020;
pub const MSG_DONTWAIT: usize = 0x0040;
pub const MSG_EOR: usize = 0x0080;
pub const MSG_WAITALL: usize = 0x0100;
pub const MSG_CTRUNC: usize = 0x0008;
pub const MSG_CMSG_CLOEXEC: usize = 0x40000000;
pub const MSG_NOSIGNAL: usize = 0x4000;
pub const MSG_OOB: usize = 0x0001;
pub const MSG_DONTROUTE: usize = 0x0004;
pub const MSG_CONFIRM: usize = 0x0800;
pub const MSG_MORE: usize = 0x8000;
pub const MSG_ERRQUEUE: usize = 0x2000;

pub const SHUT_RD: usize = 0;
pub const SHUT_WR: usize = 1;
pub const SHUT_RDWR: usize = 2;

const CMSG_ALIGN: usize = 8;
const CMSG_HEADER_LEN: usize = 16;
const MAX_SCM_RIGHTS_FDS: usize = 253;
const MAX_SOCKADDR_LEN: usize = 110;
const SIOCATMARK: usize = 0x8905;

struct SocketFs {
    mount: Arc<Mount>,
    inode: Arc<Inode>,
    dentry: Arc<Dentry>,
}

static SOCKET_FS: Spinlock<Option<SocketFs>> = Spinlock::new(None);

fn get_or_init_socket_fs() -> (Arc<Mount>, Arc<Inode>, Arc<Dentry>) {
    let mut guard = SOCKET_FS.lock();
    if guard.is_none() {
        let sb = Superblock::new(|weak| {
            let root_inode = Inode::new(
                InodeId {
                    fs_id: FsId::new(0x736f636b65746673),
                    ino: 1,
                },
                FileType::Socket,
                DevId::new(0, 0),
                4096,
                None,
                InodeMeta {
                    size: 0,
                    nlink: 1,
                    mode: FileMode::new(0o777),
                    uid: crate::vfs::cred::Uid(0),
                    gid: crate::vfs::cred::Gid(0),
                    atime: Timespec::ZERO,
                    mtime: Timespec::ZERO,
                    ctime: Timespec::ZERO,
                    blocks: 0,
                },
                Arc::new(SocketInodeOps),
                weak.clone(),
            );
            let root_dentry = Dentry::new_positive("", None, root_inode.clone());
            Superblock {
                fs_type: "socketfs",
                fs_id: FsId::new(0x736f636b65746673),
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: InodeCache::new(),
                ops: Box::new(SocketSuperblockOps),
                self_weak: weak.clone(),
            }
        });

        let mount = Mount::new(
            Arc::clone(&sb),
            Arc::clone(&sb.root_dentry),
            Arc::clone(&sb.root_dentry),
            MountFlags::default(),
            None,
        );

        *guard = Some(SocketFs {
            mount: Arc::clone(&mount),
            inode: Arc::clone(&sb.root_inode),
            dentry: Arc::clone(&sb.root_dentry),
        });
    }
    let fs = guard.as_ref().unwrap();
    (
        Arc::clone(&fs.mount),
        Arc::clone(&fs.inode),
        Arc::clone(&fs.dentry),
    )
}

struct SocketInodeOps;

impl InodeOps for SocketInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct SocketSuperblockOps;

impl SuperblockOps for SocketSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<crate::vfs::stat::FsStat> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct SocketHandleRef {
    file: Arc<File>,
    identity: Option<HandleIdentity>,
}

impl SocketHandle for SocketHandleRef {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn identity(&self) -> Option<HandleIdentity> {
        self.identity
    }
}

struct SocketFileOps {
    socket: CoreSocket,
}

impl SocketFileOps {
    fn new(socket: CoreSocket) -> Self {
        Self { socket }
    }

    fn socket(&self) -> CoreSocket {
        self.socket.clone()
    }
}

impl FileOps for SocketFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.socket
            .receive(
                buf,
                ReceiveOptions {
                    nonblocking: true,
                    peek: false,
                    wait_all: false,
                    deadline_ns: None,
                },
            )
            .map(|result| result.length)
            .map_err(map_socket_io_error)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.socket
            .send(
                buf,
                &[],
                None,
                SendOptions {
                    nonblocking: true,
                    sender_identity: None,
                    explicit_credentials: false,
                    end_of_record: false,
                    deadline_ns: None,
                },
            )
            .map_err(map_socket_io_error)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        let ready = self.socket.readiness();
        let mut events = PollEvents::default();
        if ready.has(Readiness::READABLE) {
            events = events.with(PollEvents::POLLIN);
        }
        if ready.has(Readiness::WRITABLE) {
            events = events.with(PollEvents::POLLOUT);
        }
        if ready.has(Readiness::HANGUP) {
            events = events.with(PollEvents::POLLHUP);
        }
        if ready.has(Readiness::READ_HANGUP) {
            events = events.with(PollEvents::POLLRDHUP);
        }
        if ready.has(Readiness::FAULT) {
            events = events.with(PollEvents::POLLERR);
        }
        events.intersect(interest.with(PollEvents::POLLERR).with(PollEvents::POLLHUP))
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        self.socket
            .register_waiter(task, readiness_from_poll(interest))
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.socket.unregister_waiter(task)
    }

    fn io_timeout_deadline(&self, interest: PollEvents) -> Option<u64> {
        if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI) {
            socket_timeval_deadline(self.socket.recv_timeout())
        } else if interest.has(PollEvents::POLLOUT) {
            socket_timeval_deadline(self.socket.send_timeout())
        } else {
            None
        }
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        if _cmd.raw() == SIOCATMARK {
            return Ok(self.socket.sock_at_mark() as usize);
        }
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.socket.close();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_socket_io_error(err: SocketError) -> VfsError {
    match err {
        SocketError::TemporaryUnavailable => VfsError::WouldBlock,
        SocketError::Interrupted => VfsError::Interrupted,
        SocketError::PeerClosed => VfsError::BrokenPipe,
        SocketError::DestinationRequired | SocketError::ConnectionMissing => {
            VfsError::InvalidArgument
        }
        SocketError::PayloadTooLarge => VfsError::FileTooLarge,
        SocketError::ResourceExhausted => VfsError::OutOfMemory,
        SocketError::AccessDenied => VfsError::PermissionDenied,
        _ => VfsError::InvalidArgument,
    }
}

fn map_socket_error(err: SocketError) -> Errno {
    match err {
        SocketError::UnsupportedAddressSpace => Errno::EAFNOSUPPORT,
        SocketError::Unsupported | SocketError::UnsupportedType => Errno::EOPNOTSUPP,
        SocketError::InvalidInput | SocketError::StateMismatch => Errno::EINVAL,
        SocketError::NameTooLong => Errno::ENAMETOOLONG,
        SocketError::NameAlreadyBound => Errno::EADDRINUSE,
        SocketError::NameUnavailable => Errno::EADDRNOTAVAIL,
        SocketError::AlreadyConnected => Errno::EISCONN,
        SocketError::ConnectionMissing => Errno::ENOTCONN,
        SocketError::ListenerRequired => Errno::EINVAL,
        SocketError::DestinationRequired => Errno::EDESTADDRREQ,
        SocketError::TemporaryUnavailable => Errno::EAGAIN,
        SocketError::Interrupted => Errno::EINTR,
        SocketError::PeerClosed => Errno::EPIPE,
        SocketError::ConnectionRejected => Errno::ECONNREFUSED,
        SocketError::PayloadTooLarge => Errno::EMSGSIZE,
        SocketError::ResourceExhausted => Errno::ENOMEM,
        SocketError::AccessDenied => Errno::EACCES,
    }
}

#[cfg(not(test))]
fn current_identity(ctx: &VfsContext) -> PeerIdentity {
    let pid = current_task().pid_root().unwrap_or(0) as u32;
    let cred = ctx.cred();
    PeerIdentity {
        process: pid,
        user: cred.euid.0,
        group: cred.egid.0,
    }
}

#[cfg(test)]
fn current_identity(ctx: &VfsContext) -> PeerIdentity {
    let cred = ctx.cred();
    PeerIdentity {
        process: 0,
        user: cred.euid.0,
        group: cred.egid.0,
    }
}

fn new_socket_file(socket: CoreSocket, cred: Arc<Credentials>, nonblock: bool) -> Arc<File> {
    let (mount, inode, dentry) = get_or_init_socket_fs();
    let flags = OpenOptions {
        access: crate::vfs::file::AccessMode::ReadWrite,
        nonblock,
        ..Default::default()
    };
    let file = File::new(
        inode,
        flags,
        cred,
        Box::new(SocketFileOps::new(socket)),
        dentry,
        Arc::clone(&mount),
    );
    mount.inc_open();
    Arc::new(file)
}

fn new_net_socket_file(ops: NetSocketFileOps, cred: Arc<Credentials>, nonblock: bool) -> Arc<File> {
    let (mount, inode, dentry) = get_or_init_socket_fs();
    let flags = OpenOptions {
        access: crate::vfs::file::AccessMode::ReadWrite,
        nonblock,
        ..Default::default()
    };
    let file = File::new(
        inode,
        flags,
        cred,
        Box::new(ops),
        dentry,
        Arc::clone(&mount),
    );
    mount.inc_open();
    Arc::new(file)
}

fn new_netlink_socket_file(
    ops: crate::netlink_socket::NetlinkSocketFileOps,
    cred: Arc<Credentials>,
    nonblock: bool,
) -> Arc<File> {
    let (mount, inode, dentry) = get_or_init_socket_fs();
    let flags = OpenOptions {
        access: crate::vfs::file::AccessMode::ReadWrite,
        nonblock,
        ..Default::default()
    };
    let file = File::new(
        inode,
        flags,
        cred,
        Box::new(ops),
        dentry,
        Arc::clone(&mount),
    );
    mount.inc_open();
    Arc::new(file)
}

fn socket_from_file(file: &Arc<File>) -> Result<CoreSocket, Errno> {
    let Some(ops) = file.downcast_ops::<SocketFileOps>() else {
        return Err(Errno::ENOTSOCK);
    };
    Ok(ops.socket())
}

fn socket_handle_identity(file: &Arc<File>) -> Option<HandleIdentity> {
    file.downcast_ops::<SocketFileOps>()
        .map(|ops| HandleIdentity::Socket(ops.socket().id()))
}

fn readiness_from_poll(interest: PollEvents) -> Readiness {
    let mut out = Readiness::empty();
    if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI) {
        out = out.with(Readiness::READABLE);
    }
    if interest.has(PollEvents::POLLOUT) {
        out = out.with(Readiness::WRITABLE);
    }
    if interest.has(PollEvents::POLLHUP) {
        out = out.with(Readiness::HANGUP);
    }
    if interest.has(PollEvents::POLLRDHUP) {
        out = out.with(Readiness::READ_HANGUP);
    }
    if interest.has(PollEvents::POLLERR) {
        out = out.with(Readiness::FAULT);
    }
    out
}

fn file_from_fd(fdt: &FdTable, fd: Fd) -> Result<Arc<File>, Errno> {
    fdt.get_file(fd).ok_or(Errno::EBADF)
}

fn parse_type(raw: usize) -> Result<(SocketType, bool, bool), Errno> {
    let base = raw & SOCK_TYPE_MASK;
    let flags = raw & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC);
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let kind = match base {
        SOCK_STREAM => SocketType::Stream,
        SOCK_DGRAM => SocketType::Datagram,
        SOCK_SEQPACKET => SocketType::Sequenced,
        SOCK_RAW => SocketType::Raw,
        _ => return Err(Errno::EINVAL),
    };
    Ok((kind, (raw & SOCK_NONBLOCK) != 0, (raw & SOCK_CLOEXEC) != 0))
}

pub fn socket(
    ctx: &VfsContext,
    fdt: &FdTable,
    domain: usize,
    ty: usize,
    protocol: usize,
) -> Result<Fd, Errno> {
    // AF_NETLINK 需要接受 SOCK_RAW/SOCK_DGRAM，单独处理
    if domain as u16 == 16 {
        let nonblock = (ty & SOCK_NONBLOCK) != 0;
        let cloexec = (ty & SOCK_CLOEXEC) != 0;
        let fd_flags = if cloexec {
            FdFlags::CLOEXEC
        } else {
            FdFlags::default()
        };
        let ops = crate::netlink_socket::create_netlink_socket(protocol as u32, nonblock);
        let file = new_netlink_socket_file(ops, ctx.cred(), nonblock);
        return fdt.alloc_fd(file, fd_flags).map_err(|e| e.to_errno());
    }

    // 尚未开放 INET fd，必须在解析 type/protocol 前固定失败，避免
    // 非法组合泄漏 EINVAL/EPROTONOSUPPORT 形成跨阶段可观察差异。
    if matches!(domain as u16, crate::addr::AF_INET | crate::addr::AF_INET6) {
        return Err(Errno::EAFNOSUPPORT);
    }

    let (kind, nonblock, cloexec) = parse_type(ty)?;
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let cred = ctx.cred();

    match domain as u16 {
        AF_UNIX => {
            if protocol != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            let socket =
                CoreSocket::new_unix(kind, current_identity(ctx)).map_err(map_socket_error)?;
            let file = new_socket_file(socket, Arc::clone(&cred), nonblock);
            fdt.alloc_fd(file, fd_flags).map_err(|e| e.to_errno())
        }
        17 => {
            // AF_PACKET: 使用 raw socket 实现 L3 级别的数据包收发
            let protocol = protocol as u16;
            let ops =
                crate::net_socket::create_net_socket(17, SOCK_RAW as u16, protocol, nonblock)?;
            let file = new_net_socket_file(ops, Arc::clone(&cred), nonblock);
            fdt.alloc_fd(file, fd_flags).map_err(|e| e.to_errno())
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

pub fn socketpair(
    ctx: &VfsContext,
    fdt: &FdTable,
    domain: usize,
    ty: usize,
    protocol: usize,
) -> Result<(Fd, Fd), Errno> {
    if domain as u16 != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    if protocol != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let (kind, nonblock, cloexec) = parse_type(ty)?;
    let (left, right) =
        CoreSocket::pair_unix(kind, current_identity(ctx)).map_err(map_socket_error)?;
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let cred = ctx.cred();
    let a = fdt
        .alloc_fd(new_socket_file(left, Arc::clone(&cred), nonblock), fd_flags)
        .map_err(|e| e.to_errno())?;
    let b = match fdt.alloc_fd(
        new_socket_file(right, Arc::clone(&cred), nonblock),
        fd_flags,
    ) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = fdt.close_fd(a);
            return Err(err.to_errno());
        }
    };
    Ok((a, b))
}

pub fn bind(ctx: &VfsContext, fdt: &FdTable, fd: Fd, raw_addr: &[u8]) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        return net_ops.bind(raw_addr);
    }
    if let Some(nl_ops) = file.downcast_ops::<crate::netlink_socket::NetlinkSocketFileOps>() {
        return nl_ops.bind(raw_addr);
    }
    let socket = socket_from_file(&file)?;
    let resolved = resolve_bind_address(ctx, raw_addr)?;
    match socket.bind(resolved.address) {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(path) = resolved.created_path {
                let _ = operation::unlink(ctx, &Dirfd::Cwd, &path);
            }
            Err(map_socket_error(err))
        }
    }
}

pub fn unregister_path_socket(fs: u64, ino: u64) {
    socket::unregister_path_socket(socket::PathKey { fs, ino });
}

pub fn listen(fdt: &FdTable, fd: Fd, backlog: usize) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        return net_ops.listen(backlog as u32);
    }
    let socket = socket_from_file(&file)?;
    socket.listen(backlog).map_err(map_socket_error)
}

pub fn accept(
    ctx: &VfsContext,
    fdt: &FdTable,
    fd: Fd,
    flags: usize,
) -> Result<(Fd, Option<Vec<u8>>), Errno> {
    let file = file_from_fd(fdt, fd)?;
    let (nonblock, cloexec) = parse_accept_flags(flags)?;
    if file.flags().path_only {
        return Err(Errno::EBADF);
    }
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };

    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        let accepted = net_ops.accept(file.flags().nonblock || nonblock)?;
        let peer = {
            let mut buf = vec![0u8; 28];
            let len = accepted.getpeername(&mut buf)?;
            buf.truncate(len);
            Some(buf)
        };
        let new_file = new_net_socket_file(accepted, ctx.cred(), nonblock);
        let new_fd = fdt.alloc_fd(new_file, fd_flags).map_err(|e| e.to_errno())?;
        return Ok((new_fd, peer));
    }

    let socket = socket_from_file(&file)?;
    let accepted = socket
        .accept(ReceiveOptions {
            nonblocking: file.flags().nonblock || nonblock,
            peek: false,
            wait_all: false,
            deadline_ns: None,
        })
        .map_err(map_socket_error)?;
    let peer = accepted
        .peer_address()
        .ok()
        .map(|addr| encode_sockaddr_un(&addr));
    let new_fd = fdt
        .alloc_fd(new_socket_file(accepted, ctx.cred(), nonblock), fd_flags)
        .map_err(|e| e.to_errno())?;
    Ok((new_fd, peer))
}

pub fn connect(ctx: &VfsContext, fdt: &FdTable, fd: Fd, raw_addr: &[u8]) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        return net_ops.connect(raw_addr, file.flags().nonblock);
    }
    let socket = socket_from_file(&file)?;
    socket.validate_connect_ready().map_err(map_socket_error)?;
    let address = resolve_connect_address(ctx, raw_addr)?;
    socket
        .connect(
            address,
            current_identity(ctx),
            SendOptions {
                nonblocking: file.flags().nonblock,
                sender_identity: None,
                explicit_credentials: false,
                end_of_record: false,
                deadline_ns: None,
            },
        )
        .map_err(map_socket_error)
}

pub fn getsockname(fdt: &FdTable, fd: Fd) -> Result<Vec<u8>, Errno> {
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        let mut buf = vec![0u8; 28];
        let len = net_ops.getsockname(&mut buf)?;
        buf.truncate(len);
        return Ok(buf);
    }
    let socket = socket_from_file(&file)?;
    Ok(encode_sockaddr_un(&socket.local_address()))
}

pub fn getpeername(fdt: &FdTable, fd: Fd) -> Result<Vec<u8>, Errno> {
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        let mut buf = vec![0u8; 28];
        let len = net_ops.getpeername(&mut buf)?;
        buf.truncate(len);
        return Ok(buf);
    }
    let socket = socket_from_file(&file)?;
    let addr = socket.peer_address().map_err(map_socket_error)?;
    Ok(encode_sockaddr_un(&addr))
}

pub fn shutdown(fdt: &FdTable, fd: Fd, how: usize) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        return net_ops.shutdown(how as u32);
    }
    let socket = socket_from_file(&file)?;
    let how = match how {
        SHUT_RD => SocketShutdown::Read,
        SHUT_WR => SocketShutdown::Write,
        SHUT_RDWR => SocketShutdown::Both,
        _ => return Err(Errno::EINVAL),
    };
    socket.shutdown(how).map_err(map_socket_error)
}

pub struct RecvOutput {
    pub len: usize,
    pub address: Option<Vec<u8>>,
    pub control: Vec<u8>,
    pub msg_flags: usize,
}

pub fn send(
    ctx: &VfsContext,
    fdt: &FdTable,
    fd: Fd,
    data: &[u8],
    control: &[u8],
    raw_addr: Option<&[u8]>,
    flags: usize,
) -> Result<usize, Errno> {
    validate_send_flags(flags)?;
    let file = file_from_fd(fdt, fd)?;
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        if !control.is_empty() {
            return Err(Errno::ENOPROTOOPT);
        }
        if (flags & MSG_OOB) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        return net_ops.sendto(
            data,
            raw_addr,
            InetSendOptions {
                nonblocking: file.flags().nonblock || (flags & MSG_DONTWAIT) != 0,
                deadline_ns: None,
            },
        );
    }
    if let Some(nl_ops) = file.downcast_ops::<crate::netlink_socket::NetlinkSocketFileOps>() {
        return nl_ops.write_at(data, 0).map_err(|e| e.to_errno());
    }
    let socket = socket_from_file(&file)?;
    let decoded = decode_send_control(ctx, fdt, &file, &socket, control)?;
    let address = match raw_addr {
        Some(raw) => Some(resolve_connect_address(ctx, raw)?),
        None => None,
    };
    socket
        .send(
            data,
            &decoded.handles,
            address,
            SendOptions {
                nonblocking: file.flags().nonblock || (flags & MSG_DONTWAIT) != 0,
                sender_identity: Some(current_identity(ctx)),
                explicit_credentials: decoded.credentials.is_some(),
                end_of_record: (flags & MSG_EOR) != 0,
                deadline_ns: None,
            },
        )
        .map_err(map_socket_error)
}

pub fn recv(
    fdt: &FdTable,
    fd: Fd,
    data: &mut [u8],
    control_len: usize,
    want_addr: bool,
    flags: usize,
    deadline_ns: Option<u64>,
) -> Result<RecvOutput, Errno> {
    validate_recv_flags(flags)?;
    let file = file_from_fd(fdt, fd)?;

    if let Some(nl_ops) = file.downcast_ops::<crate::netlink_socket::NetlinkSocketFileOps>() {
        let len = nl_ops.recv(
            data,
            file.flags().nonblock || (flags & MSG_DONTWAIT) != 0,
            deadline_ns,
        )?;
        // 返回 sockaddr_nl（12 字节）：family=AF_NETLINK(16), pad=0, pid=0, groups=0
        let address = if want_addr {
            let mut sa = vec![0u8; 12];
            sa[0..2].copy_from_slice(&16u16.to_ne_bytes()); // nl_family = AF_NETLINK
            Some(sa)
        } else {
            None
        };
        return Ok(RecvOutput {
            len,
            address,
            control: Vec::new(),
            msg_flags: 0,
        });
    }

    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        if (flags & MSG_OOB) != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        if (flags & MSG_ERRQUEUE) != 0 {
            return Err(Errno::EAGAIN);
        }
        let result = net_ops.recvfrom(
            data,
            InetRecvOptions {
                nonblocking: file.flags().nonblock || (flags & MSG_DONTWAIT) != 0,
                peek: (flags & MSG_PEEK) != 0,
                wait_all: (flags & MSG_WAITALL) != 0
                    && (flags & MSG_PEEK) == 0
                    && net_ops.sock_type() == crate::net_socket::SOCK_STREAM_PUB,
                trunc: (flags & MSG_TRUNC) != 0,
                deadline_ns,
            },
        )?;
        let address = if want_addr {
            result.remote.and_then(|ep| {
                let mut buf = vec![0u8; 28];
                crate::addr::encode_inet_sockaddr(&ep, net_ops.family(), &mut buf)
                    .ok()
                    .map(|sz| {
                        buf.truncate(sz);
                        buf
                    })
            })
        } else {
            None
        };
        let (control, control_truncated) =
            encode_inet_receive_control(net_ops, &result, control_len);
        let mut msg_flags = result.msg_flags;
        if control_truncated {
            msg_flags |= MSG_CTRUNC;
        }
        return Ok(RecvOutput {
            len: result.len,
            address,
            control,
            msg_flags,
        });
    }

    let socket = socket_from_file(&file)?;
    let nonblocking = file.flags().nonblock || (flags & MSG_DONTWAIT) != 0;
    let peek = (flags & MSG_PEEK) != 0;
    let wait_all = (flags & MSG_WAITALL) != 0
        && !peek
        && control_len == 0
        && socket.socket_type() == SocketType::Stream;
    let result = socket
        .receive(
            data,
            ReceiveOptions {
                nonblocking,
                peek,
                wait_all,
                deadline_ns,
            },
        )
        .map_err(map_socket_error)?;
    let cloexec = (flags & MSG_CMSG_CLOEXEC) != 0;
    let mut msg_flags = 0usize;
    if result.data_truncated {
        msg_flags |= MSG_TRUNC;
    }
    if socket.socket_type() == SocketType::Sequenced && result.length != 0 && !result.data_truncated
    {
        msg_flags |= MSG_EOR;
    }
    let (control, control_truncated) = encode_receive_control(fdt, &result, control_len, cloexec)?;
    if control_truncated {
        msg_flags |= MSG_CTRUNC;
    }
    Ok(RecvOutput {
        len: result.length,
        address: if want_addr {
            result.sender.as_ref().map(encode_sockaddr_un)
        } else {
            None
        },
        control,
        msg_flags,
    })
}

pub fn getsockopt(fdt: &FdTable, fd: Fd, level: i32, optname: i32) -> Result<Vec<u8>, Errno> {
    let file = file_from_fd(fdt, fd)?;

    // AF_NETLINK socket
    if file
        .downcast_ops::<crate::netlink_socket::NetlinkSocketFileOps>()
        .is_some()
    {
        return netlink_getsockopt(level, optname);
    }
    // AF_INET/AF_INET6 socket
    if let Some(net_ops) = file.downcast_ops::<NetSocketFileOps>() {
        return inet_getsockopt(net_ops, level, optname);
    }

    let socket = socket_from_file(&file)?;
    if level != SOL_SOCKET {
        return Err(Errno::ENOPROTOOPT);
    }
    match optname {
        SO_DOMAIN => Ok((AF_UNIX as i32).to_ne_bytes().to_vec()),
        SO_PROTOCOL => Ok(0i32.to_ne_bytes().to_vec()),
        SO_TYPE => {
            let raw = match socket.socket_type() {
                SocketType::Stream => SOCK_STREAM as i32,
                SocketType::Datagram => SOCK_DGRAM as i32,
                SocketType::Sequenced => SOCK_SEQPACKET as i32,
                SocketType::Raw => SOCK_RAW as i32,
            };
            Ok(raw.to_ne_bytes().to_vec())
        }
        SO_SNDBUF => Ok(clamp_i32(socket.send_buffer_size()).to_ne_bytes().to_vec()),
        SO_RCVBUF => Ok(clamp_i32(socket.recv_buffer_size()).to_ne_bytes().to_vec()),
        SO_REUSEADDR => Ok(i32::from(socket.reuse_addr()).to_ne_bytes().to_vec()),
        SO_REUSEPORT => Ok(i32::from(socket.reuse_port()).to_ne_bytes().to_vec()),
        SO_LINGER => Ok(encode_linger(socket.linger())),
        SO_PEERCRED => {
            let cred = socket.peer_identity().map_err(map_socket_error)?;
            let mut out = Vec::with_capacity(12);
            out.extend_from_slice(&cred.process.to_ne_bytes());
            out.extend_from_slice(&cred.user.to_ne_bytes());
            out.extend_from_slice(&cred.group.to_ne_bytes());
            Ok(out)
        }
        SO_PASSCRED => Ok(i32::from(socket.passcred_enabled()).to_ne_bytes().to_vec()),
        SO_RCVTIMEO => Ok(encode_timeval(socket.recv_timeout())),
        SO_SNDTIMEO => Ok(encode_timeval(socket.send_timeout())),
        SO_ACCEPTCONN => Ok(i32::from(socket.is_listener()).to_ne_bytes().to_vec()),
        SO_ERROR => Ok(socket
            .take_last_error()
            .map(|err| i32::from(map_socket_error(err)))
            .unwrap_or(0)
            .to_ne_bytes()
            .to_vec()),
        _ => Err(Errno::ENOPROTOOPT),
    }
}

pub fn setsockopt(
    fdt: &FdTable,
    fd: Fd,
    level: i32,
    optname: i32,
    value: &[u8],
) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;

    if file
        .downcast_ops::<crate::netlink_socket::NetlinkSocketFileOps>()
        .is_some()
    {
        return netlink_setsockopt(level, optname, value);
    }
    if file.downcast_ops::<NetSocketFileOps>().is_some() {
        let net_ops = file.downcast_ops::<NetSocketFileOps>().unwrap();
        return inet_setsockopt(net_ops, level, optname, value);
    }

    let socket = socket_from_file(&file)?;
    if level != SOL_SOCKET {
        return Err(Errno::ENOPROTOOPT);
    }
    match optname {
        SO_REUSEADDR => {
            socket.set_reuse_addr(parse_bool_opt(value)?);
            Ok(())
        }
        SO_SNDBUF => {
            socket.set_send_buffer_size(parse_positive_i32_opt(value)? as usize);
            Ok(())
        }
        SO_RCVBUF => {
            socket.set_recv_buffer_size(parse_positive_i32_opt(value)? as usize);
            Ok(())
        }
        SO_LINGER => {
            socket.set_linger(parse_linger(value)?);
            Ok(())
        }
        SO_REUSEPORT => {
            socket.set_reuse_port(parse_bool_opt(value)?);
            Ok(())
        }
        SO_PASSCRED => {
            socket.set_passcred(parse_bool_opt(value)?);
            Ok(())
        }
        SO_RCVTIMEO => {
            socket.set_recv_timeout(parse_timeval(value)?);
            Ok(())
        }
        SO_SNDTIMEO => {
            socket.set_send_timeout(parse_timeval(value)?);
            Ok(())
        }
        _ => Err(Errno::ENOPROTOOPT),
    }
}

fn parse_accept_flags(flags: usize) -> Result<(bool, bool), Errno> {
    let allowed = SOCK_NONBLOCK | SOCK_CLOEXEC;
    if (flags & !allowed) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(((flags & SOCK_NONBLOCK) != 0, (flags & SOCK_CLOEXEC) != 0))
}

// ── AF_INET / AF_INET6 sockopt ──────────────────────────────────────────────

const SOL_IP: i32 = 0;
const SOL_TCP: i32 = 6;
const SOL_IPV6: i32 = 41;

const TCP_NODELAY: i32 = 1;
const TCP_MAXSEG: i32 = 2;
const TCP_CORK: i32 = 3;
const TCP_KEEPIDLE: i32 = 4;
const TCP_KEEPINTVL: i32 = 5;
const TCP_KEEPCNT: i32 = 6;
const TCP_INFO: i32 = 11;
const TCP_CONGESTION: i32 = 13;
const TCP_DEFER_ACCEPT: i32 = 9;
const TCP_QUICKACK: i32 = 12;
const TCP_USER_TIMEOUT: i32 = 18;
const TCP_FASTOPEN: i32 = 23;
const TCP_NOTSENT_LOWAT: i32 = 25;

const IP_TOS: i32 = 1;
const IP_TTL: i32 = 2;
const IP_HDRINCL: i32 = 3;
const IP_OPTIONS: i32 = 4;
const IP_PKTINFO: i32 = 8;
const IP_RECVERR: i32 = 11;
const IP_RECVTTL: i32 = 12;
const IP_RECVTOS: i32 = 13;
const IP_FREEBIND: i32 = 15;
const IP_MULTICAST_IF: i32 = 32;
const IP_MULTICAST_TTL: i32 = 33;
const IP_MULTICAST_LOOP: i32 = 34;
const IP_ADD_MEMBERSHIP: i32 = 35;
const IP_DROP_MEMBERSHIP: i32 = 36;

const IPV6_V6ONLY: i32 = 26;
const IPV6_UNICAST_HOPS: i32 = 16;
const IPV6_RECVPKTINFO: i32 = 49;
const IPV6_PKTINFO: i32 = 50;
const IPV6_RECVHOPLIMIT: i32 = 51;
const IPV6_HOPLIMIT: i32 = 52;
const IPV6_TCLASS: i32 = 67;
const IPV6_ADD_MEMBERSHIP: i32 = 20;
const IPV6_DROP_MEMBERSHIP: i32 = 21;
const IPV6_RECVERR: i32 = 25;
const IPV6_MULTICAST_HOPS: i32 = 18;
const IPV6_MULTICAST_IF: i32 = 17;
const IPV6_MULTICAST_LOOP: i32 = 19;

const SO_TIMESTAMP: i32 = 29;
const SO_BINDTODEVICE: i32 = 25;
const SO_MARK: i32 = 36;
const SO_PRIORITY: i32 = 12;
const SO_OOBINLINE: i32 = 10;

// ── 选项值解析辅助 ──────────────────────────────────────────────────────────

fn parse_int_opt(value: &[u8]) -> Result<i32, Errno> {
    if value.len() < 4 {
        return Err(Errno::EINVAL);
    }
    Ok(i32::from_ne_bytes([value[0], value[1], value[2], value[3]]))
}

/// 解析 `struct timeval { tv_sec; tv_usec; }`（每个字段 8 字节，LP64 ABI）。
fn parse_timeval_ns(value: &[u8]) -> u64 {
    if value.len() < 16 {
        return 0;
    }
    let secs = i64::from_ne_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ])
    .max(0) as u64;
    let usecs = i64::from_ne_bytes([
        value[8], value[9], value[10], value[11], value[12], value[13], value[14], value[15],
    ])
    .max(0) as u64;
    secs.saturating_mul(1_000_000_000)
        .saturating_add(usecs.saturating_mul(1_000))
}

fn timeval_from_ns(ns: u64) -> [u8; 16] {
    let secs = (ns / 1_000_000_000) as i64;
    let usecs = ((ns % 1_000_000_000) / 1_000) as i64;
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&secs.to_ne_bytes());
    buf[8..16].copy_from_slice(&usecs.to_ne_bytes());
    buf
}

fn inet_getsockopt(net_ops: &NetSocketFileOps, level: i32, optname: i32) -> Result<Vec<u8>, Errno> {
    if level == SOL_TCP && net_ops.sock_type() != SOCK_STREAM as u16 {
        return Err(Errno::ENOPROTOOPT);
    }
    let opts = net_ops.options().lock();
    match level {
        SOL_SOCKET => match optname {
            SO_DOMAIN => Ok((net_ops.family() as i32).to_ne_bytes().to_vec()),
            SO_TYPE => Ok((net_ops.sock_type() as i32).to_ne_bytes().to_vec()),
            SO_PROTOCOL => Ok(0i32.to_ne_bytes().to_vec()),
            SO_ERROR => Ok(net_ops.take_last_error_code().to_ne_bytes().to_vec()),
            SO_SNDBUF => Ok(opts.sndbuf.to_ne_bytes().to_vec()),
            SO_RCVBUF => Ok(opts.rcvbuf.to_ne_bytes().to_vec()),
            SO_KEEPALIVE => Ok((opts.keepalive as i32).to_ne_bytes().to_vec()),
            SO_BROADCAST => Ok((opts.broadcast as i32).to_ne_bytes().to_vec()),
            SO_REUSEADDR => Ok((opts.reuseaddr as i32).to_ne_bytes().to_vec()),
            SO_REUSEPORT => Ok((opts.reuseport as i32).to_ne_bytes().to_vec()),
            SO_LINGER => {
                let mut buf = [0u8; 8];
                buf[0..4].copy_from_slice(&(opts.linger_on as i32).to_ne_bytes());
                buf[4..8].copy_from_slice(&(opts.linger_secs as i32).to_ne_bytes());
                Ok(buf.to_vec())
            }
            SO_RCVTIMEO => {
                let ns = net_ops
                    .recv_timeout_ns()
                    .load(core::sync::atomic::Ordering::Relaxed);
                Ok(timeval_from_ns(ns).to_vec())
            }
            SO_SNDTIMEO => {
                let ns = net_ops
                    .send_timeout_ns()
                    .load(core::sync::atomic::Ordering::Relaxed);
                Ok(timeval_from_ns(ns).to_vec())
            }
            SO_TIMESTAMP => Ok((opts.timestamp as i32).to_ne_bytes().to_vec()),
            SO_DONTROUTE => Ok((opts.dontroute as i32).to_ne_bytes().to_vec()),
            SO_RXQ_OVFL => Ok((opts.rxq_ovfl as i32).to_ne_bytes().to_vec()),
            SO_MARK => Ok((opts.mark as i32).to_ne_bytes().to_vec()),
            SO_PRIORITY => Ok(opts.priority.to_ne_bytes().to_vec()),
            SO_OOBINLINE => Ok((opts.oobinline as i32).to_ne_bytes().to_vec()),
            SO_BINDTODEVICE => Ok(vec![0u8; 16]), // TODO: 记录并返回绑定设备名，影响路由选择（需多接口支持）
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_TCP => match optname {
            TCP_NODELAY => Ok((opts.nodelay as i32).to_ne_bytes().to_vec()),
            TCP_CORK => Ok((opts.cork as i32).to_ne_bytes().to_vec()), // TODO: TCP_CORK 仅存储未应用，smoltcp 无 cork 模式
            TCP_MAXSEG => Ok(1460i32.to_ne_bytes().to_vec()), // TODO: 从 smoltcp TCP socket 的 MSS 状态读取
            TCP_KEEPIDLE => Ok((opts.keepidle as i32).to_ne_bytes().to_vec()),
            TCP_KEEPINTVL => Ok((opts.keepintvl as i32).to_ne_bytes().to_vec()), // TODO: 仅存储，smoltcp 不支持单独设置 intvl
            TCP_KEEPCNT => Ok((opts.keepcnt as i32).to_ne_bytes().to_vec()), // TODO: 仅存储，smoltcp 不支持探测次数
            TCP_INFO => Ok(tcp_info_minimal(net_ops)), // TODO: 仅填最小状态字段，缺少 RTT/cwnd/retrans 等完整 TCP 统计
            TCP_CONGESTION => Ok(b"cubic\0".to_vec()), // TODO: smoltcp 仅支持 Reno/Cubic 切换，未对接
            TCP_DEFER_ACCEPT => Ok((opts.defer_accept as i32).to_ne_bytes().to_vec()), // TODO: 仅存储，smoltcp 无 defer accept
            TCP_QUICKACK => Ok((opts.quickack as i32).to_ne_bytes().to_vec()), // TODO: 仅存储，smoltcp 无 quickack
            TCP_USER_TIMEOUT => Ok((opts.user_timeout as i32).to_ne_bytes().to_vec()), // TODO: 仅存储，可对接 smoltcp 的 set_timeout
            TCP_FASTOPEN => Ok(0i32.to_ne_bytes().to_vec()), // TODO: smoltcp 不支持 TFO
            TCP_NOTSENT_LOWAT => Ok((-1i32).to_ne_bytes().to_vec()), // TODO: smoltcp 不支持低水位通知
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_IP => match optname {
            IP_TTL => Ok((opts.ttl as i32).to_ne_bytes().to_vec()),
            IP_TOS => Ok((opts.tos as i32).to_ne_bytes().to_vec()),
            IP_MULTICAST_TTL => Ok((opts.mcast_ttl as i32).to_ne_bytes().to_vec()),
            IP_MULTICAST_LOOP => Ok((opts.mcast_loop as i32).to_ne_bytes().to_vec()),
            IP_MULTICAST_IF => Ok(vec![0u8; 4]), // TODO: 返回当前组播出接口 index，需要 IGMP 和多接口路由
            IP_PKTINFO => Ok((opts.pktinfo as i32).to_ne_bytes().to_vec()),
            IP_HDRINCL => Ok((opts.hdrincl as i32).to_ne_bytes().to_vec()),
            IP_OPTIONS => Ok(Vec::new()), // TODO: 返回当前 IP options（smoltcp 不支持 IP options）
            IP_RECVERR => Ok((opts.recverr as i32).to_ne_bytes().to_vec()),
            IP_RECVTTL => Ok((opts.recvttl as i32).to_ne_bytes().to_vec()),
            IP_RECVTOS => Ok((opts.recvtos as i32).to_ne_bytes().to_vec()),
            IP_FREEBIND => Ok((opts.freebind as i32).to_ne_bytes().to_vec()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_IPV6 => match optname {
            IPV6_V6ONLY => Ok((opts.v6only as i32).to_ne_bytes().to_vec()),
            IPV6_UNICAST_HOPS => Ok((opts.hops_v6 as i32).to_ne_bytes().to_vec()),
            IPV6_RECVPKTINFO => Ok((opts.recv_pktinfo_v6 as i32).to_ne_bytes().to_vec()),
            IPV6_RECVHOPLIMIT => Ok((opts.recv_hoplimit_v6 as i32).to_ne_bytes().to_vec()),
            IPV6_RECVERR => Ok((opts.recverr_v6 as i32).to_ne_bytes().to_vec()),
            IPV6_TCLASS => Ok(opts.tclass.to_ne_bytes().to_vec()),
            IPV6_MULTICAST_HOPS => Ok((opts.mcast_hops_v6 as i32).to_ne_bytes().to_vec()),
            IPV6_MULTICAST_IF => Ok(0i32.to_ne_bytes().to_vec()), // TODO: 返回当前 IPv6 组播出接口 index
            IPV6_MULTICAST_LOOP => Ok((opts.mcast_loop as i32).to_ne_bytes().to_vec()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        _ => Err(Errno::ENOPROTOOPT),
    }
}

fn tcp_info_minimal(net_ops: &NetSocketFileOps) -> Vec<u8> {
    // Linux struct tcp_info 在不同内核版本里持续追加字段。iperf3 只要求
    // getsockopt(TCP_INFO) 成功，并能读取开头的 state/基本计数；这里返回
    // 104 字节的旧版基础布局，未实现的统计字段保持 0。
    const TCP_INFO_MIN_LEN: usize = 104;
    let mut info = vec![0u8; TCP_INFO_MIN_LEN];
    let _ = net_ops;
    info[0] = 7; // TCP_CLOSE；当前不存在可达的 INET fd。
    // tcpi_snd_mss / tcpi_rcv_mss，避免用户程序把 0 当成异常路径。
    info[16..20].copy_from_slice(&1460u32.to_ne_bytes());
    info[20..24].copy_from_slice(&1460u32.to_ne_bytes());
    info
}

fn inet_setsockopt(
    net_ops: &NetSocketFileOps,
    level: i32,
    optname: i32,
    value: &[u8],
) -> Result<(), Errno> {
    if level == SOL_TCP && net_ops.sock_type() != SOCK_STREAM as u16 {
        return Err(Errno::ENOPROTOOPT);
    }
    let mut opts = net_ops.options().lock();
    let _handle = net_ops.get_handle_for_opts();
    match level {
        SOL_SOCKET => match optname {
            SO_KEEPALIVE => {
                opts.keepalive = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_BROADCAST => {
                opts.broadcast = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_REUSEADDR => {
                opts.reuseaddr = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_REUSEPORT => {
                opts.reuseport = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_LINGER => {
                if value.len() >= 8 {
                    opts.linger_on =
                        i32::from_ne_bytes([value[0], value[1], value[2], value[3]]) != 0;
                    opts.linger_secs =
                        i32::from_ne_bytes([value[4], value[5], value[6], value[7]]) as u32;
                }
                Ok(())
            }
            SO_RCVTIMEO => {
                let ns = parse_timeval_ns(value);
                net_ops
                    .recv_timeout_ns()
                    .store(ns, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            SO_SNDTIMEO => {
                let ns = parse_timeval_ns(value);
                net_ops
                    .send_timeout_ns()
                    .store(ns, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            SO_SNDBUF => {
                opts.sndbuf = parse_int_opt(value)?.max(1);
                Ok(())
            }
            SO_RCVBUF => {
                opts.rcvbuf = parse_int_opt(value)?.max(1);
                Ok(())
            }
            SO_TIMESTAMP => {
                opts.timestamp = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_DONTROUTE => {
                // 当前路由层还没有“只走直连链路”的独立策略；先保存开关
                // 并按 no-op 成功返回，避免 netperf/traceroute 把兼容缺口
                // 当成致命错误。
                opts.dontroute = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_RXQ_OVFL => {
                // Linux 用该选项请求在控制消息中报告 socket RX 溢出计数。
                // 本栈暂未生成 ancillary data，但接受并保存该开关可兼容
                // netperf 的 enable_enobufs 探测路径。
                opts.rxq_ovfl = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_MARK => {
                opts.mark = parse_int_opt(value)? as u32;
                Ok(())
            }
            SO_PRIORITY => {
                opts.priority = parse_int_opt(value)?;
                Ok(())
            }
            SO_OOBINLINE => {
                opts.oobinline = parse_int_opt(value)? != 0;
                Ok(())
            }
            SO_BINDTODEVICE => Ok(()), // TODO: 记录绑定设备名并影响路由选择（需多接口路由基础设施）
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_TCP => match optname {
            TCP_NODELAY => {
                opts.nodelay = parse_int_opt(value)? != 0;
                Ok(())
            }
            TCP_CORK => {
                opts.cork = parse_int_opt(value)? != 0;
                Ok(())
            } // TODO: 应用到 smoltcp — 需要延迟发送小段，smoltcp 无 cork 模式
            TCP_KEEPIDLE => {
                opts.keepidle = parse_int_opt(value)? as u32;
                Ok(())
            }
            TCP_KEEPINTVL => {
                opts.keepintvl = parse_int_opt(value)? as u32;
                Ok(())
            } // TODO: 传播到 smoltcp — 当前仅存储，smoltcp 不支持单独设置探测间隔
            TCP_KEEPCNT => {
                opts.keepcnt = parse_int_opt(value)? as u32;
                Ok(())
            } // TODO: 传播到 smoltcp — 当前仅存储，smoltcp 不支持探测次数
            TCP_CONGESTION => Ok(()), // TODO: 对接 smoltcp 拥塞算法切换（仅 Reno/Cubic）
            TCP_DEFER_ACCEPT => {
                opts.defer_accept = parse_int_opt(value)? as u32;
                Ok(())
            } // TODO: smoltcp 无 defer accept 模式
            TCP_QUICKACK => {
                opts.quickack = parse_int_opt(value)? != 0;
                Ok(())
            } // TODO: smoltcp 无 quickack 控制
            TCP_USER_TIMEOUT => {
                opts.user_timeout = parse_int_opt(value)? as u32;
                Ok(())
            } // TODO: 可对接 smoltcp set_timeout()
            TCP_FASTOPEN => Ok(()),   // TODO: smoltcp 不支持 TFO
            TCP_NOTSENT_LOWAT => Ok(()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_IP => match optname {
            IP_TTL => {
                let ttl = parse_int_opt(value)?;
                if !(1..=255).contains(&ttl) {
                    return Err(Errno::EINVAL);
                }
                opts.ttl = ttl as u8;
                Ok(())
            }
            IP_TOS => {
                let tos = parse_int_opt(value)?;
                if !(0..=255).contains(&tos) {
                    return Err(Errno::EINVAL);
                }
                opts.tos = tos as u8;
                Ok(())
            }
            IP_MULTICAST_TTL => {
                opts.mcast_ttl = parse_int_opt(value)? as u8;
                Ok(())
            }
            IP_MULTICAST_LOOP => {
                opts.mcast_loop = parse_int_opt(value)? != 0;
                Ok(())
            }
            IP_MULTICAST_IF => Ok(()),
            IP_ADD_MEMBERSHIP | IP_DROP_MEMBERSHIP => Err(Errno::EAFNOSUPPORT),
            IP_PKTINFO => {
                opts.pktinfo = parse_int_opt(value)? != 0;
                Ok(())
            }
            IP_HDRINCL => {
                opts.hdrincl = parse_int_opt(value)? != 0;
                Ok(())
            }
            IP_RECVERR => {
                // Linux 使用该选项把异步网络错误送入 MSG_ERRQUEUE。
                // 当前栈尚未实现错误队列；先保存开关并返回成功，避免 netperf
                // 的 enable_enobufs 探测路径因为 ENOPROTOOPT 中断。
                opts.recverr = parse_int_opt(value)? != 0;
                Ok(())
            }
            IP_RECVTTL => {
                opts.recvttl = parse_int_opt(value)? != 0;
                Ok(())
            }
            IP_RECVTOS => {
                opts.recvtos = parse_int_opt(value)? != 0;
                Ok(())
            }
            IP_FREEBIND => {
                opts.freebind = parse_int_opt(value)? != 0;
                Ok(())
            }
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_IPV6 => match optname {
            IPV6_V6ONLY => {
                opts.v6only = parse_int_opt(value)? != 0;
                Ok(())
            }
            IPV6_UNICAST_HOPS => {
                opts.hops_v6 = parse_int_opt(value)? as u8;
                Ok(())
            }
            IPV6_RECVPKTINFO => {
                opts.recv_pktinfo_v6 = parse_int_opt(value)? != 0;
                Ok(())
            }
            IPV6_RECVHOPLIMIT => {
                opts.recv_hoplimit_v6 = parse_int_opt(value)? != 0;
                Ok(())
            }
            IPV6_RECVERR => {
                // IPv6 error queue 语义同 IP_RECVERR；目前作为兼容开关保存。
                opts.recverr_v6 = parse_int_opt(value)? != 0;
                Ok(())
            }
            IPV6_TCLASS => {
                let tclass = parse_int_opt(value)?;
                if !(-1..=255).contains(&tclass) {
                    return Err(Errno::EINVAL);
                }
                opts.tclass = tclass;
                Ok(())
            }
            IPV6_ADD_MEMBERSHIP | IPV6_DROP_MEMBERSHIP => Err(Errno::EAFNOSUPPORT),
            IPV6_MULTICAST_HOPS | IPV6_MULTICAST_IF | IPV6_MULTICAST_LOOP => Ok(()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        _ => Err(Errno::ENOPROTOOPT),
    }
}

// ── AF_NETLINK sockopt ──────────────────────────────────────────────────────

const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;
const NETLINK_DROP_MEMBERSHIP: i32 = 2;

fn netlink_getsockopt(level: i32, optname: i32) -> Result<Vec<u8>, Errno> {
    match level {
        SOL_SOCKET => match optname {
            SO_DOMAIN => Ok(16i32.to_ne_bytes().to_vec()), // AF_NETLINK
            SO_TYPE => Ok((SOCK_RAW as i32).to_ne_bytes().to_vec()),
            SO_PROTOCOL => Ok(0i32.to_ne_bytes().to_vec()),
            SO_SNDBUF | SO_RCVBUF => Ok(212992i32.to_ne_bytes().to_vec()),
            SO_ERROR => Ok(0i32.to_ne_bytes().to_vec()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_NETLINK => match optname {
            NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => Ok(0i32.to_ne_bytes().to_vec()),
            _ => Err(Errno::ENOPROTOOPT),
        },
        _ => Err(Errno::ENOPROTOOPT),
    }
}

fn netlink_setsockopt(level: i32, optname: i32, _value: &[u8]) -> Result<(), Errno> {
    match level {
        SOL_SOCKET => match optname {
            SO_SNDBUF | SO_RCVBUF | SO_PASSCRED => Ok(()), // TODO: netlink SO_SNDBUF/SO_RCVBUF 应调整内核缓冲区大小；SO_PASSCRED 应开启凭证传递
            _ => Err(Errno::ENOPROTOOPT),
        },
        SOL_NETLINK => match optname {
            NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => Ok(()), // TODO: 实现 netlink 组播组加入/退出，影响接收哪些广播消息
            _ => Err(Errno::ENOPROTOOPT),
        },
        _ => Err(Errno::ENOPROTOOPT),
    }
}

fn validate_send_flags(flags: usize) -> Result<(), Errno> {
    let allowed =
        MSG_DONTWAIT | MSG_NOSIGNAL | MSG_EOR | MSG_MORE | MSG_OOB | MSG_DONTROUTE | MSG_CONFIRM;
    // TODO: 实际处理 MSG_MORE（TCP cork 语义，合并小段后发送）— 需要 smoltcp cork 支持
    // TODO: 实际处理 MSG_OOB（TCP urgent data）— smoltcp 不支持 urgent pointer
    // TODO: 实际处理 MSG_DONTROUTE（绕过路由表直接发往链路层）— 单接口场景无意义
    // TODO: 实际处理 MSG_CONFIRM（确认链路层邻居可达）— smoltcp 内部自动管理 ARP
    if (flags & !allowed) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn validate_recv_flags(flags: usize) -> Result<(), Errno> {
    let allowed = MSG_DONTWAIT
        | MSG_PEEK
        | MSG_CMSG_CLOEXEC
        | MSG_TRUNC
        | MSG_WAITALL
        | MSG_OOB
        | MSG_ERRQUEUE;
    // TODO: 实际处理 MSG_OOB（TCP urgent data 接收）— smoltcp 不支持 urgent pointer
    // TODO: 实际处理 MSG_ERRQUEUE（接收 IP 层错误队列）— smoltcp 无错误队列机制
    if (flags & !allowed) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

enum ParsedUnixAddress {
    Unnamed,
    Abstract(Vec<u8>),
    Path { path: String, display: Vec<u8> },
}

struct DecodedControl {
    handles: Vec<socket::SharedHandle>,
    credentials: Option<PeerIdentity>,
}

struct ResolvedBindAddress {
    address: UnixAddress,
    created_path: Option<String>,
}

fn parse_sockaddr_un(raw: &[u8]) -> Result<ParsedUnixAddress, Errno> {
    if raw.len() < 2 || raw.len() > MAX_SOCKADDR_LEN {
        return Err(Errno::EINVAL);
    }
    let family = u16::from_ne_bytes([raw[0], raw[1]]);
    if family != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    let path = &raw[2..];
    if path.is_empty() {
        return Ok(ParsedUnixAddress::Unnamed);
    }
    if path[0] == 0 {
        return Ok(ParsedUnixAddress::Abstract(path[1..].to_vec()));
    }
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let display = path[..end].to_vec();
    if display.is_empty() {
        return Ok(ParsedUnixAddress::Unnamed);
    }
    let name = core::str::from_utf8(&display).map_err(|_| Errno::EINVAL)?;
    Ok(ParsedUnixAddress::Path {
        path: name.into(),
        display,
    })
}

fn resolve_bind_address(ctx: &VfsContext, raw: &[u8]) -> Result<ResolvedBindAddress, Errno> {
    match parse_sockaddr_un(raw)? {
        ParsedUnixAddress::Unnamed => Err(Errno::EINVAL),
        ParsedUnixAddress::Abstract(name) => Ok(ResolvedBindAddress {
            address: UnixAddress::Abstract(name),
            created_path: None,
        }),
        ParsedUnixAddress::Path { path, display } => {
            match operation::mknodat(
                ctx,
                &Dirfd::Cwd,
                &path,
                FileType::Socket,
                FileMode::new(0o777),
                DevId::new(0, 0),
            ) {
                Ok(()) => {}
                Err(VfsError::AlreadyExists) => return Err(Errno::EADDRINUSE),
                Err(err) => return Err(err.to_errno()),
            }
            let result = path::lookup(ctx, &Dirfd::Cwd, &path, LookupFlags::default())
                .map_err(|e| e.to_errno())?;
            let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
            Ok(ResolvedBindAddress {
                address: UnixAddress::Path {
                    key: socket::PathKey {
                        fs: inode.fs_id().raw(),
                        ino: inode.ino(),
                    },
                    display,
                },
                created_path: Some(path),
            })
        }
    }
}

fn resolve_connect_address(ctx: &VfsContext, raw: &[u8]) -> Result<UnixAddress, Errno> {
    if raw.len() >= 2 {
        let family = u16::from_ne_bytes([raw[0], raw[1]]);
        if family == 0 {
            return Ok(UnixAddress::Unnamed);
        }
    }
    match parse_sockaddr_un(raw)? {
        ParsedUnixAddress::Unnamed => Err(Errno::EINVAL),
        ParsedUnixAddress::Abstract(name) => Ok(UnixAddress::Abstract(name)),
        ParsedUnixAddress::Path { path, display } => {
            let result = path::lookup(ctx, &Dirfd::Cwd, &path, LookupFlags::default())
                .map_err(|e| e.to_errno())?;
            let inode = result.dentry.inode().ok_or(Errno::ENOENT)?;
            if inode.kind() != FileType::Socket {
                return Err(Errno::ENOTSOCK);
            }
            let meta = inode.meta_snapshot();
            if !ctx.cred().can_write(meta.uid, meta.gid, meta.mode) {
                return Err(Errno::EACCES);
            }
            Ok(UnixAddress::Path {
                key: socket::PathKey {
                    fs: inode.fs_id().raw(),
                    ino: inode.ino(),
                },
                display,
            })
        }
    }
}

fn encode_sockaddr_un(addr: &UnixAddress) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&AF_UNIX.to_ne_bytes());
    match addr {
        UnixAddress::Unnamed => {}
        UnixAddress::Abstract(name) => {
            out.push(0);
            out.extend_from_slice(name);
        }
        UnixAddress::Path { display, .. } => {
            out.extend_from_slice(display);
            out.push(0);
        }
    }
    out
}

fn decode_send_control(
    ctx: &VfsContext,
    fdt: &FdTable,
    source_file: &Arc<File>,
    source_socket: &CoreSocket,
    control: &[u8],
) -> Result<DecodedControl, Errno> {
    let identity = current_identity(ctx);
    let mut out = Vec::new();
    let mut credentials = None;
    let mut offset = 0usize;
    while offset + CMSG_HEADER_LEN <= control.len() {
        let len = usize::from_ne_bytes(
            control[offset..offset + 8]
                .try_into()
                .map_err(|_| Errno::EINVAL)?,
        );
        let level = i32::from_ne_bytes(
            control[offset + 8..offset + 12]
                .try_into()
                .map_err(|_| Errno::EINVAL)?,
        );
        let kind = i32::from_ne_bytes(
            control[offset + 12..offset + 16]
                .try_into()
                .map_err(|_| Errno::EINVAL)?,
        );
        if len < CMSG_HEADER_LEN {
            return Err(Errno::EINVAL);
        }
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if end > control.len() {
            return Err(Errno::EINVAL);
        }
        if level == SOL_SOCKET {
            match kind {
                SCM_RIGHTS => {
                    let data = &control[offset + CMSG_HEADER_LEN..end];
                    if data.len() % 4 != 0 {
                        return Err(Errno::EINVAL);
                    }
                    for raw_fd in data.chunks_exact(4) {
                        if out.len() >= MAX_SCM_RIGHTS_FDS {
                            return Err(Errno::EINVAL);
                        }
                        let fd = i32::from_ne_bytes(raw_fd.try_into().unwrap());
                        let file = fdt.get_file(Fd::from_raw(fd as u32)).ok_or(Errno::EBADF)?;
                        if Arc::ptr_eq(&file, source_file) {
                            return Err(Errno::EINVAL);
                        }
                        let identity = socket_handle_identity(&file);
                        if identity.is_some_and(|identity| {
                            source_socket.would_create_handle_cycle(identity)
                        }) {
                            return Err(Errno::EINVAL);
                        }
                        out.push(
                            Arc::new(SocketHandleRef { file, identity }) as socket::SharedHandle
                        );
                    }
                }
                SCM_CREDENTIALS => {
                    let data = &control[offset + CMSG_HEADER_LEN..end];
                    if data.len() != 12 {
                        return Err(Errno::EINVAL);
                    }
                    let pid = u32::from_ne_bytes(data[0..4].try_into().unwrap());
                    let uid = u32::from_ne_bytes(data[4..8].try_into().unwrap());
                    let gid = u32::from_ne_bytes(data[8..12].try_into().unwrap());
                    let all_zero = pid == 0 && uid == 0 && gid == 0;
                    if !all_zero
                        && (pid != identity.process
                            || uid != identity.user
                            || gid != identity.group)
                    {
                        return Err(Errno::EPERM);
                    }
                    credentials = Some(identity);
                }
                _ => return Err(Errno::ENOPROTOOPT),
            }
        }
        offset = align_cmsg(end);
    }
    Ok(DecodedControl {
        handles: out,
        credentials,
    })
}

fn parse_bool_opt(value: &[u8]) -> Result<bool, Errno> {
    match value.len() {
        1 => Ok(value[0] != 0),
        4.. => Ok(i32::from_ne_bytes(value[0..4].try_into().unwrap()) != 0),
        _ => Err(Errno::EINVAL),
    }
}

fn parse_positive_i32_opt(value: &[u8]) -> Result<i32, Errno> {
    if value.len() < 4 {
        return Err(Errno::EINVAL);
    }
    let parsed = i32::from_ne_bytes(value[0..4].try_into().unwrap());
    if parsed <= 0 {
        return Err(Errno::EINVAL);
    }
    Ok(parsed)
}

fn parse_linger(value: &[u8]) -> Result<SocketLinger, Errno> {
    if value.len() < 8 {
        return Err(Errno::EINVAL);
    }
    let enabled = i32::from_ne_bytes(value[0..4].try_into().unwrap()) != 0;
    let seconds = i32::from_ne_bytes(value[4..8].try_into().unwrap());
    if seconds < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(SocketLinger {
        enabled,
        seconds: seconds as u32,
    })
}

fn encode_linger(value: SocketLinger) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&i32::from(value.enabled).to_ne_bytes());
    out.extend_from_slice(&(value.seconds as i32).to_ne_bytes());
    out
}

fn parse_timeval(value: &[u8]) -> Result<Option<SocketTimeval>, Errno> {
    if value.len() < 16 {
        return Err(Errno::EINVAL);
    }
    let secs = i64::from_ne_bytes(value[0..8].try_into().unwrap());
    let micros = i64::from_ne_bytes(value[8..16].try_into().unwrap());
    if secs < 0 || !(0..1_000_000).contains(&micros) {
        return Err(Errno::EINVAL);
    }
    if secs == 0 && micros == 0 {
        return Ok(None);
    }
    Ok(Some(SocketTimeval { secs, micros }))
}

fn encode_timeval(value: Option<SocketTimeval>) -> Vec<u8> {
    let value = value.unwrap_or(SocketTimeval { secs: 0, micros: 0 });
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&value.secs.to_ne_bytes());
    out.extend_from_slice(&value.micros.to_ne_bytes());
    out
}

fn socket_timeval_deadline(value: Option<SocketTimeval>) -> Option<u64> {
    let value = value?;
    if value.secs < 0 || value.micros < 0 {
        return None;
    }
    let secs = value.secs as u64;
    let micros = value.micros as u64;
    let delta = secs
        .saturating_mul(1_000_000_000)
        .saturating_add(micros.saturating_mul(1_000));
    Some(now_ns_public().saturating_add(delta))
}

fn clamp_i32(value: usize) -> i32 {
    value.min(i32::MAX as usize) as i32
}

fn install_received_handles(
    fdt: &FdTable,
    handles: &[socket::SharedHandle],
    cloexec: bool,
) -> Result<Vec<i32>, Errno> {
    let mut out = Vec::with_capacity(handles.len());
    let mut installed = Vec::with_capacity(handles.len());
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    for handle in handles {
        let Some(file_ref) = handle.as_any().downcast_ref::<SocketHandleRef>() else {
            for fd in installed {
                let _ = fdt.close_fd(fd);
            }
            return Err(Errno::EINVAL);
        };
        let fd = match fdt.alloc_fd(Arc::clone(&file_ref.file), fd_flags) {
            Ok(fd) => fd,
            Err(err) => {
                for fd in installed {
                    let _ = fdt.close_fd(fd);
                }
                return Err(err.to_errno());
            }
        };
        installed.push(fd);
        out.push(fd.as_raw() as i32);
    }
    Ok(out)
}

fn encode_rights(fds: &[i32]) -> Vec<u8> {
    if fds.is_empty() {
        return Vec::new();
    }
    let payload_len = fds.len() * 4;
    let cmsg_len = CMSG_HEADER_LEN + payload_len;
    let total = align_cmsg(cmsg_len);
    let mut out = vec![0u8; total];
    out[0..8].copy_from_slice(&cmsg_len.to_ne_bytes());
    out[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    out[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    let mut offset = CMSG_HEADER_LEN;
    for fd in fds {
        out[offset..offset + 4].copy_from_slice(&fd.to_ne_bytes());
        offset += 4;
    }
    out
}

fn encode_credentials(identity: PeerIdentity) -> Vec<u8> {
    let cmsg_len = CMSG_HEADER_LEN + 12;
    let total = align_cmsg(cmsg_len);
    let mut out = vec![0u8; total];
    out[0..8].copy_from_slice(&cmsg_len.to_ne_bytes());
    out[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    out[12..16].copy_from_slice(&SCM_CREDENTIALS.to_ne_bytes());
    out[16..20].copy_from_slice(&identity.process.to_ne_bytes());
    out[20..24].copy_from_slice(&identity.user.to_ne_bytes());
    out[24..28].copy_from_slice(&identity.group.to_ne_bytes());
    out
}

fn encode_cmsg(level: i32, kind: i32, payload: &[u8]) -> Vec<u8> {
    let cmsg_len = CMSG_HEADER_LEN + payload.len();
    let total = align_cmsg(cmsg_len);
    let mut out = vec![0u8; total];
    out[0..8].copy_from_slice(&cmsg_len.to_ne_bytes());
    out[8..12].copy_from_slice(&level.to_ne_bytes());
    out[12..16].copy_from_slice(&kind.to_ne_bytes());
    out[CMSG_HEADER_LEN..CMSG_HEADER_LEN + payload.len()].copy_from_slice(payload);
    out
}

fn encode_inet_pktinfo(result: &InetRecvResult) -> Option<Vec<u8>> {
    let local = result.local?;
    let net::IpAddr::V4(addr) = local.addr else {
        return None;
    };
    let ifindex = result
        .interface_id
        .map(|id| id.raw().saturating_add(1) as i32)
        .unwrap_or(0);
    let mut payload = [0u8; 12];
    payload[0..4].copy_from_slice(&ifindex.to_ne_bytes());
    // Linux in_pktinfo: ipi_spec_dst 与 ipi_addr 都填入本报文目的地址。
    payload[4..8].copy_from_slice(&addr.0);
    payload[8..12].copy_from_slice(&addr.0);
    Some(encode_cmsg(SOL_IP, IP_PKTINFO, &payload))
}

fn encode_inet_ttl(result: &InetRecvResult) -> Option<Vec<u8>> {
    let local = result.local?;
    if !matches!(local.addr, net::IpAddr::V4(_)) {
        return None;
    }
    let ttl = result.hop_limit? as i32;
    Some(encode_cmsg(SOL_IP, IP_TTL, &ttl.to_ne_bytes()))
}

fn encode_inet_tos(result: &InetRecvResult) -> Option<Vec<u8>> {
    let local = result.local?;
    if !matches!(local.addr, net::IpAddr::V4(_)) {
        return None;
    }
    let tos = result.traffic_class?;
    Some(encode_cmsg(SOL_IP, IP_TOS, &[tos]))
}

fn encode_inet6_pktinfo(result: &InetRecvResult) -> Option<Vec<u8>> {
    let local = result.local?;
    let net::IpAddr::V6(addr) = local.addr else {
        return None;
    };
    let ifindex = result
        .interface_id
        .map(|id| id.raw().saturating_add(1) as u32)
        .unwrap_or(0);
    let mut payload = [0u8; 20];
    payload[0..16].copy_from_slice(&addr.0);
    payload[16..20].copy_from_slice(&ifindex.to_ne_bytes());
    Some(encode_cmsg(SOL_IPV6, IPV6_PKTINFO, &payload))
}

fn encode_inet6_hoplimit(result: &InetRecvResult) -> Option<Vec<u8>> {
    let local = result.local?;
    if !matches!(local.addr, net::IpAddr::V6(_)) {
        return None;
    }
    let hoplimit = result.hop_limit? as i32;
    Some(encode_cmsg(
        SOL_IPV6,
        IPV6_HOPLIMIT,
        &hoplimit.to_ne_bytes(),
    ))
}

fn append_receive_cmsg(out: &mut Vec<u8>, control_len: usize, cmsg: Vec<u8>) -> bool {
    if out.len() + cmsg.len() <= control_len {
        out.extend_from_slice(&cmsg);
        false
    } else {
        true
    }
}

fn encode_inet_receive_control(
    net_ops: &NetSocketFileOps,
    result: &InetRecvResult,
    control_len: usize,
) -> (Vec<u8>, bool) {
    let opts = net_ops.options().lock().clone();
    encode_inet_receive_control_with_options(&opts, result, control_len)
}

fn encode_inet_receive_control_with_options(
    opts: &SocketOptions,
    result: &InetRecvResult,
    control_len: usize,
) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    if opts.pktinfo {
        if let Some(cmsg) = encode_inet_pktinfo(result) {
            truncated |= append_receive_cmsg(&mut out, control_len, cmsg);
        }
    }
    if !truncated && opts.recvttl {
        if let Some(cmsg) = encode_inet_ttl(result) {
            truncated |= append_receive_cmsg(&mut out, control_len, cmsg);
        }
    }
    if !truncated && opts.recvtos {
        if let Some(cmsg) = encode_inet_tos(result) {
            truncated |= append_receive_cmsg(&mut out, control_len, cmsg);
        }
    }
    if !truncated && opts.recv_pktinfo_v6 {
        if let Some(cmsg) = encode_inet6_pktinfo(result) {
            truncated |= append_receive_cmsg(&mut out, control_len, cmsg);
        }
    }
    if !truncated && opts.recv_hoplimit_v6 {
        if let Some(cmsg) = encode_inet6_hoplimit(result) {
            truncated |= append_receive_cmsg(&mut out, control_len, cmsg);
        }
    }
    (out, truncated)
}

fn rights_capacity_remaining(remaining: usize) -> usize {
    if remaining < CMSG_HEADER_LEN + 4 {
        0
    } else {
        (remaining - CMSG_HEADER_LEN) / 4
    }
}

fn encode_receive_control(
    fdt: &FdTable,
    result: &socket::ReceiveResult,
    control_len: usize,
    cloexec: bool,
) -> Result<(Vec<u8>, bool), Errno> {
    let mut out = Vec::new();
    let mut truncated = false;

    if let Some(identity) = result.sender_identity {
        let creds = encode_credentials(identity);
        if creds.len() <= control_len {
            out.extend_from_slice(&creds);
        } else {
            truncated = true;
        }
    }

    let remaining = control_len.saturating_sub(out.len());
    let keep = result
        .handles
        .len()
        .min(rights_capacity_remaining(remaining));
    if keep < result.handles.len() {
        truncated = true;
    }
    if keep != 0 {
        let fds = install_received_handles(fdt, &result.handles[..keep], cloexec)?;
        let rights = encode_rights(&fds);
        if out.len() + rights.len() <= control_len {
            out.extend_from_slice(&rights);
        } else {
            for fd in fds {
                let _ = fdt.close_fd(Fd::from_raw(fd as u32));
            }
            truncated = true;
        }
    }

    Ok((out, truncated))
}

const fn align_cmsg(value: usize) -> usize {
    (value + (CMSG_ALIGN - 1)) & !(CMSG_ALIGN - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cmsgs(control: &[u8]) -> Vec<(i32, i32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset + CMSG_HEADER_LEN <= control.len() {
            let len =
                usize::from_ne_bytes(control[offset..offset + 8].try_into().expect("cmsg len"));
            if len < CMSG_HEADER_LEN || offset + len > control.len() {
                break;
            }
            let level = i32::from_ne_bytes(
                control[offset + 8..offset + 12]
                    .try_into()
                    .expect("cmsg level"),
            );
            let kind = i32::from_ne_bytes(
                control[offset + 12..offset + 16]
                    .try_into()
                    .expect("cmsg kind"),
            );
            out.push((
                level,
                kind,
                control[offset + CMSG_HEADER_LEN..offset + len].to_vec(),
            ));
            offset = align_cmsg(offset + len);
        }
        out
    }

    #[test]
    fn inet_receive_control_encodes_ipv4_pktinfo_ttl_tos() {
        let mut opts = SocketOptions::default();
        opts.pktinfo = true;
        opts.recvttl = true;
        opts.recvtos = true;
        let result = InetRecvResult {
            len: 4,
            remote: None,
            local: Some(net::Endpoint {
                addr: net::IpAddr::V4(net::Ipv4Addr::new(10, 1, 2, 3)),
                port: 7783,
            }),
            interface_id: Some(NetDeviceId(4)),
            hop_limit: Some(37),
            traffic_class: Some(0xbb),
            msg_flags: 0,
        };

        let (control, truncated) = encode_inet_receive_control_with_options(&opts, &result, 128);
        assert!(!truncated);
        let cmsgs = parse_cmsgs(&control);
        assert_eq!(cmsgs.len(), 3);

        assert_eq!(cmsgs[0].0, SOL_IP);
        assert_eq!(cmsgs[0].1, IP_PKTINFO);
        assert_eq!(cmsgs[0].2.len(), 12);
        assert_eq!(i32::from_ne_bytes(cmsgs[0].2[0..4].try_into().unwrap()), 5);
        assert_eq!(&cmsgs[0].2[4..8], &[10, 1, 2, 3]);
        assert_eq!(&cmsgs[0].2[8..12], &[10, 1, 2, 3]);

        assert_eq!(cmsgs[1].0, SOL_IP);
        assert_eq!(cmsgs[1].1, IP_TTL);
        assert_eq!(i32::from_ne_bytes(cmsgs[1].2[0..4].try_into().unwrap()), 37);

        assert_eq!(cmsgs[2].0, SOL_IP);
        assert_eq!(cmsgs[2].1, IP_TOS);
        assert_eq!(cmsgs[2].2, vec![0xbb]);
    }

    #[test]
    fn inet_receive_control_encodes_ipv6_pktinfo_hoplimit() {
        let mut opts = SocketOptions::default();
        opts.recv_pktinfo_v6 = true;
        opts.recv_hoplimit_v6 = true;
        let addr = net::Ipv6Addr::new([0x2001, 0xdb8, 0, 0, 0, 0, 0, 7]);
        let result = InetRecvResult {
            len: 4,
            remote: None,
            local: Some(net::Endpoint {
                addr: net::IpAddr::V6(addr),
                port: 7783,
            }),
            interface_id: Some(NetDeviceId(8)),
            hop_limit: Some(63),
            traffic_class: Some(0),
            msg_flags: 0,
        };

        let (control, truncated) = encode_inet_receive_control_with_options(&opts, &result, 128);
        assert!(!truncated);
        let cmsgs = parse_cmsgs(&control);
        assert_eq!(cmsgs.len(), 2);

        assert_eq!(cmsgs[0].0, SOL_IPV6);
        assert_eq!(cmsgs[0].1, IPV6_PKTINFO);
        assert_eq!(cmsgs[0].2.len(), 20);
        assert_eq!(&cmsgs[0].2[0..16], &addr.0);
        assert_eq!(
            u32::from_ne_bytes(cmsgs[0].2[16..20].try_into().unwrap()),
            9
        );

        assert_eq!(cmsgs[1].0, SOL_IPV6);
        assert_eq!(cmsgs[1].1, IPV6_HOPLIMIT);
        assert_eq!(i32::from_ne_bytes(cmsgs[1].2[0..4].try_into().unwrap()), 63);
    }

    #[test]
    fn inet_receive_control_sets_truncation_after_last_fitting_cmsg() {
        let mut opts = SocketOptions::default();
        opts.pktinfo = true;
        opts.recvttl = true;
        let result = InetRecvResult {
            len: 4,
            remote: None,
            local: Some(net::Endpoint {
                addr: net::IpAddr::V4(net::Ipv4Addr::new(10, 1, 2, 3)),
                port: 7783,
            }),
            interface_id: Some(NetDeviceId(4)),
            hop_limit: Some(37),
            traffic_class: Some(0),
            msg_flags: 0,
        };

        let pktinfo_len = align_cmsg(CMSG_HEADER_LEN + 12);
        let (control, truncated) =
            encode_inet_receive_control_with_options(&opts, &result, pktinfo_len);
        assert!(truncated);
        let cmsgs = parse_cmsgs(&control);
        assert_eq!(cmsgs.len(), 1);
        assert_eq!(cmsgs[0].1, IP_PKTINFO);
    }
}
