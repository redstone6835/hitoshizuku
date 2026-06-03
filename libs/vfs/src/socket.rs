use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, current_task, now_ns_public};
use socket::{
    SocketError, SocketHandle, PeerIdentity, Readiness, ReceiveOptions,
    SendOptions, SocketShutdown, Socket as CoreSocket, SocketLinger, SocketTimeval,
    SocketType, UnixAddress,
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

pub const AF_UNIX: u16 = 1;

pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM: usize = 2;
pub const SOCK_SEQPACKET: usize = 5;
pub const SOCK_TYPE_MASK: usize = 0xf;
pub const SOCK_NONBLOCK: usize = 0o00004000;
pub const SOCK_CLOEXEC: usize = 0o02000000;

pub const SOL_SOCKET: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SCM_RIGHTS: i32 = 1;
pub const SCM_CREDENTIALS: i32 = 2;
pub const SO_ERROR: i32 = 4;
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

pub const MSG_PEEK: usize = 0x0002;
pub const MSG_TRUNC: usize = 0x0020;
pub const MSG_DONTWAIT: usize = 0x0040;
pub const MSG_EOR: usize = 0x0080;
pub const MSG_WAITALL: usize = 0x0100;
pub const MSG_CTRUNC: usize = 0x0008;
pub const MSG_CMSG_CLOEXEC: usize = 0x40000000;
pub const MSG_NOSIGNAL: usize = 0x4000;

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
}

impl SocketHandle for SocketHandleRef {
    fn as_any(&self) -> &dyn Any {
        self
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

fn current_identity(ctx: &VfsContext) -> PeerIdentity {
    let pid = current_task().pid_root().unwrap_or(0) as u32;
    PeerIdentity {
        process: pid,
        user: ctx.cred.euid.0,
        group: ctx.cred.egid.0,
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

fn socket_from_file(file: &Arc<File>) -> Result<CoreSocket, Errno> {
    let Some(ops) = file.downcast_ops::<SocketFileOps>() else {
        return Err(Errno::ENOTSOCK);
    };
    Ok(ops.socket())
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
    if domain as u16 != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    if protocol != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let (kind, nonblock, cloexec) = parse_type(ty)?;
    let socket = CoreSocket::new_unix(kind, current_identity(ctx)).map_err(map_socket_error)?;
    let file = new_socket_file(socket, Arc::clone(&ctx.cred), nonblock);
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    fdt.alloc_fd(file, fd_flags).map_err(|e| e.to_errno())
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
    let a = fdt
        .alloc_fd(
            new_socket_file(left, Arc::clone(&ctx.cred), nonblock),
            fd_flags,
        )
        .map_err(|e| e.to_errno())?;
    let b = match fdt.alloc_fd(
        new_socket_file(right, Arc::clone(&ctx.cred), nonblock),
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
    let socket = socket_from_file(&file)?;
    let address = resolve_bind_address(ctx, raw_addr)?;
    socket.bind(address).map_err(map_socket_error)
}

pub fn listen(fdt: &FdTable, fd: Fd, backlog: usize) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
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
    let socket = socket_from_file(&file)?;
    let (nonblock, cloexec) = parse_accept_flags(flags)?;
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
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    let new_fd = fdt
        .alloc_fd(
            new_socket_file(accepted, Arc::clone(&ctx.cred), nonblock),
            fd_flags,
        )
        .map_err(|e| e.to_errno())?;
    Ok((new_fd, peer))
}

pub fn connect(ctx: &VfsContext, fdt: &FdTable, fd: Fd, raw_addr: &[u8]) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
    let socket = socket_from_file(&file)?;
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
    let socket = socket_from_file(&file)?;
    Ok(encode_sockaddr_un(&socket.local_address()))
}

pub fn getpeername(fdt: &FdTable, fd: Fd) -> Result<Vec<u8>, Errno> {
    let file = file_from_fd(fdt, fd)?;
    let socket = socket_from_file(&file)?;
    let addr = socket.peer_address().map_err(map_socket_error)?;
    Ok(encode_sockaddr_un(&addr))
}

pub fn shutdown(fdt: &FdTable, fd: Fd, how: usize) -> Result<(), Errno> {
    let file = file_from_fd(fdt, fd)?;
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
    let socket = socket_from_file(&file)?;
    let decoded = decode_send_control(ctx, fdt, &file, control)?;
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

fn validate_send_flags(flags: usize) -> Result<(), Errno> {
    let allowed = MSG_DONTWAIT | MSG_NOSIGNAL | MSG_EOR;
    if (flags & !allowed) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn validate_recv_flags(flags: usize) -> Result<(), Errno> {
    let allowed = MSG_DONTWAIT | MSG_PEEK | MSG_CMSG_CLOEXEC | MSG_TRUNC | MSG_WAITALL;
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

fn resolve_bind_address(ctx: &VfsContext, raw: &[u8]) -> Result<UnixAddress, Errno> {
    match parse_sockaddr_un(raw)? {
        ParsedUnixAddress::Unnamed => Err(Errno::EINVAL),
        ParsedUnixAddress::Abstract(name) => Ok(UnixAddress::Abstract(name)),
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
            if !ctx.cred.can_write(meta.uid, meta.gid, meta.mode) {
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
                        out.push(Arc::new(SocketHandleRef { file }) as socket::SharedHandle);
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
