//! VFS socket POSIX compatibility tests.
//!
//! These tests keep the real socket behavior in `libs/socket` and exercise only
//! the VFS-facing translation: fd allocation, sockaddr_un parsing, errno
//! mapping, control messages, poll events, and pathname socket inode plumbing.

extern crate std;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use ktest::ktest;

use crate::VfsContext;
use crate::cred::{Credentials, Gid, Uid};
use crate::dentry::{Dentry, VfsRoot};
use crate::error::{VfsError, VfsResult};
use crate::fdtable::{Fd, FdFlags, FdTable};
use crate::file::{DirEntry, FileOps, OpenOptions, PollEvents};
use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::limits::VfsLimits;
use crate::mount::{Mount, MountFlags, MountNamespace};
use crate::operation;
use crate::path::Dirfd;
use crate::socket as vsock;
use crate::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use crate::superblock::{InodeCache, Superblock, SuperblockOps};
use crate::sync::Spinlock;

static NEXT_FS_ID: AtomicU64 = AtomicU64::new(0x7666_735f_736f_636b);
static NEXT_NS_ID: AtomicU64 = AtomicU64::new(0x5151);

struct TinyDirOps {
    next_ino: AtomicU64,
    children: Spinlock<BTreeMap<String, Arc<Inode>>>,
}

impl TinyDirOps {
    fn new() -> Self {
        Self {
            next_ino: AtomicU64::new(2),
            children: Spinlock::new(BTreeMap::new()),
        }
    }
}

impl InodeOps for TinyDirOps {
    fn lookup(&self, _inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn mknod(
        &self,
        inode: &Inode,
        name: &str,
        kind: FileType,
        mode: FileMode,
        dev: DevId,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        let mut children = self.children.lock();
        if children.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        let node = Inode::new(
            InodeId {
                fs_id: inode.fs_id(),
                ino,
            },
            kind,
            dev,
            4096,
            None,
            InodeMeta {
                size: 0,
                nlink: 1,
                mode,
                uid: cred.euid,
                gid: cred.egid,
                atime: Timespec::ZERO,
                mtime: Timespec::ZERO,
                ctime: Timespec::ZERO,
                blocks: 0,
            },
            Arc::new(TinyLeafOps),
            Arc::downgrade(&sb),
        );
        children.insert(name.to_string(), Arc::clone(&node));
        Ok(node)
    }

    fn unlink(&self, _inode: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        let mut children = self.children.lock();
        let Some(existing) = children.get(name) else {
            return Err(VfsError::NotFound);
        };
        if existing.ino() != child.ino() || existing.fs_id() != child.fs_id() {
            return Err(VfsError::InvalidArgument);
        }
        let removed = children.remove(name).unwrap();
        removed.dec_nlink();
        Ok(())
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

struct TinyLeafOps;

impl InodeOps for TinyLeafOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
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

struct TinySbOps;

impl SuperblockOps for TinySbOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Err(VfsError::NotSupported)
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct EmptyFileOps;

impl FileOps for EmptyFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
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
        interest
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct Fixture {
    ctx: VfsContext,
    fdt: FdTable,
}

fn fixture() -> Fixture {
    fixture_with_cred(Credentials::root())
}

fn fixture_with_cred(cred: Credentials) -> Fixture {
    let fs_id = FsId::new(NEXT_FS_ID.fetch_add(1, Ordering::Relaxed));
    let dir_ops = Arc::new(TinyDirOps::new());
    let sb = Superblock::new(|weak| {
        let root_inode = Inode::new(
            InodeId { fs_id, ino: 1 },
            FileType::Directory,
            DevId::new(0, 0),
            4096,
            None,
            InodeMeta {
                size: 0,
                nlink: 1,
                mode: FileMode::new(0o777),
                uid: Uid(0),
                gid: Gid(0),
                atime: Timespec::ZERO,
                mtime: Timespec::ZERO,
                ctime: Timespec::ZERO,
                blocks: 0,
            },
            Arc::clone(&dir_ops) as Arc<dyn InodeOps + Send + Sync>,
            weak.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
        Superblock {
            fs_type: "tiny-socket-testfs",
            fs_id,
            dev_id: None,
            block_size: 4096,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: InodeCache::new(),
            ops: Box::new(TinySbOps),
            self_weak: weak,
        }
    });

    let mount = Mount::new(
        Arc::clone(&sb),
        Arc::clone(&sb.root_dentry),
        Arc::clone(&sb.root_dentry),
        MountFlags::default(),
        None,
    );
    let ns = MountNamespace::new(
        NEXT_NS_ID.fetch_add(1, Ordering::Relaxed),
        Arc::clone(&mount),
    );
    let root = VfsRoot::new(Arc::clone(&sb.root_dentry), Arc::clone(&mount));
    let limits = VfsLimits::default_arc();
    let fdt = FdTable::new(&limits);
    let ctx = VfsContext::new(
        Arc::clone(&sb.root_dentry),
        Arc::clone(&mount),
        root,
        ns,
        Arc::new(cred),
        FileMode::new(0),
        limits,
    );
    Fixture { ctx, fdt }
}

fn abstract_addr(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + name.len());
    out.extend_from_slice(&vsock::AF_UNIX.to_ne_bytes());
    out.push(0);
    out.extend_from_slice(name);
    out
}

fn path_addr(path: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + path.len());
    out.extend_from_slice(&vsock::AF_UNIX.to_ne_bytes());
    out.extend_from_slice(path.as_bytes());
    out.push(0);
    out
}

fn unique_name(prefix: &str) -> String {
    alloc::format!("{}-{}", prefix, NEXT_NS_ID.fetch_add(1, Ordering::Relaxed))
}

fn unique_abstract(prefix: &str) -> Vec<u8> {
    abstract_addr(unique_name(prefix).as_bytes())
}

fn cmsg(level: i32, kind: i32, data: &[u8]) -> Vec<u8> {
    const HEADER: usize = 16;
    const ALIGN: usize = 8;
    let len = HEADER + data.len();
    let total = (len + ALIGN - 1) & !(ALIGN - 1);
    let mut out = alloc::vec![0u8; total];
    out[0..8].copy_from_slice(&len.to_ne_bytes());
    out[8..12].copy_from_slice(&level.to_ne_bytes());
    out[12..16].copy_from_slice(&kind.to_ne_bytes());
    out[16..16 + data.len()].copy_from_slice(data);
    out
}

fn rights_cmsg(fds: &[Fd]) -> Vec<u8> {
    let mut data = Vec::with_capacity(fds.len() * 4);
    for fd in fds {
        data.extend_from_slice(&(fd.as_raw() as i32).to_ne_bytes());
    }
    cmsg(vsock::SOL_SOCKET, vsock::SCM_RIGHTS, &data)
}

fn creds_cmsg(pid: u32, uid: u32, gid: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&pid.to_ne_bytes());
    data.extend_from_slice(&uid.to_ne_bytes());
    data.extend_from_slice(&gid.to_ne_bytes());
    cmsg(vsock::SOL_SOCKET, vsock::SCM_CREDENTIALS, &data)
}

fn parse_cmsgs(control: &[u8]) -> Vec<(i32, i32, Vec<u8>)> {
    const HEADER: usize = 16;
    const ALIGN: usize = 8;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + HEADER <= control.len() {
        let len = usize::from_ne_bytes(control[offset..offset + 8].try_into().unwrap());
        if len < HEADER || offset + len > control.len() {
            break;
        }
        let level = i32::from_ne_bytes(control[offset + 8..offset + 12].try_into().unwrap());
        let kind = i32::from_ne_bytes(control[offset + 12..offset + 16].try_into().unwrap());
        let data = control[offset + HEADER..offset + len].to_vec();
        out.push((level, kind, data));
        offset = (offset + len + ALIGN - 1) & !(ALIGN - 1);
    }
    out
}

fn read_i32(bytes: &[u8]) -> i32 {
    i32::from_ne_bytes(bytes[0..4].try_into().unwrap())
}

fn read_i64_pair(bytes: &[u8]) -> (i64, i64) {
    (
        i64::from_ne_bytes(bytes[0..8].try_into().unwrap()),
        i64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
    )
}

fn make_plain_file(fx: &Fixture) -> Fd {
    let sb = fx.ctx.root.mount().superblock.clone();
    let inode = Inode::new(
        InodeId {
            fs_id: FsId::new(NEXT_FS_ID.fetch_add(1, Ordering::Relaxed)),
            ino: 1,
        },
        FileType::Regular,
        DevId::new(0, 0),
        4096,
        None,
        InodeMeta {
            size: 0,
            nlink: 1,
            mode: FileMode::new(0o666),
            uid: Uid(0),
            gid: Gid(0),
            atime: Timespec::ZERO,
            mtime: Timespec::ZERO,
            ctime: Timespec::ZERO,
            blocks: 0,
        },
        Arc::new(TinyLeafOps),
        Arc::downgrade(&sb),
    );
    let dentry = Dentry::new_positive(
        unique_name("plain").as_str(),
        Some(fx.ctx.root.root()),
        Arc::clone(&inode),
    );
    let file = crate::file::File::new(
        inode,
        OpenOptions {
            access: crate::file::AccessMode::ReadWrite,
            ..Default::default()
        },
        Arc::clone(&fx.ctx.cred),
        Box::new(EmptyFileOps),
        dentry,
        fx.ctx.root.mount(),
    );
    fx.ctx.root.mount().inc_open();
    fx.fdt
        .alloc_fd(Arc::new(file), FdFlags::default())
        .expect("plain fd")
}

#[ktest]
fn socket_creation_validates_domain_type_protocol_and_flags() {
    let fx = fixture();

    assert_eq!(
        vsock::socket(&fx.ctx, &fx.fdt, 2, vsock::SOCK_STREAM, 0),
        Err(Errno::EAFNOSUPPORT)
    );
    assert_eq!(
        vsock::socket(&fx.ctx, &fx.fdt, vsock::AF_UNIX as usize, 99, 0),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        vsock::socket(
            &fx.ctx,
            &fx.fdt,
            vsock::AF_UNIX as usize,
            vsock::SOCK_STREAM | 0x1000_0000,
            0
        ),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        vsock::socket(
            &fx.ctx,
            &fx.fdt,
            vsock::AF_UNIX as usize,
            vsock::SOCK_STREAM,
            1
        ),
        Err(Errno::EOPNOTSUPP)
    );
}

#[ktest]
fn socket_sets_fd_and_file_flags_and_exposes_socket_stat() {
    let fx = fixture();
    let fd = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK | vsock::SOCK_CLOEXEC,
        0,
    )
    .expect("socket");
    let file = fx.fdt.get_file(fd).expect("file");

    assert!(fx.fdt.fd_flags(fd).unwrap().has(FdFlags::CLOEXEC));
    assert!(file.flags().nonblock);
    assert_eq!(
        file.stat().unwrap().mode & FileType::Socket.to_mode_bits(),
        FileType::Socket.to_mode_bits()
    );
}

#[ktest]
fn socketpair_stream_send_recv_poll_shutdown_and_file_io() {
    let fx = fixture();
    let (a, b) = vsock::socketpair(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK | vsock::SOCK_CLOEXEC,
        0,
    )
    .expect("socketpair");

    assert!(fx.fdt.fd_flags(a).unwrap().has(FdFlags::CLOEXEC));
    assert!(fx.fdt.fd_flags(b).unwrap().has(FdFlags::CLOEXEC));

    let b_file = fx.fdt.get_file(b).expect("b file");
    assert!(!b_file.poll(PollEvents::POLLIN).has(PollEvents::POLLIN));

    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, a, b"hello", &[], None, 0),
        Ok(5)
    );
    assert!(b_file.poll(PollEvents::POLLIN).has(PollEvents::POLLIN));

    let mut buf = [0u8; 8];
    let out = vsock::recv(&fx.fdt, b, &mut buf, 0, false, vsock::MSG_DONTWAIT, None).expect("recv");
    assert_eq!(out.len, 5);
    assert_eq!(&buf[..5], b"hello");

    let a_file = fx.fdt.get_file(a).expect("a file");
    assert_eq!(a_file.write(b"file"), Ok(4));
    let mut file_buf = [0u8; 4];
    assert_eq!(b_file.read(&mut file_buf), Ok(4));
    assert_eq!(&file_buf, b"file");

    assert_eq!(vsock::shutdown(&fx.fdt, a, vsock::SHUT_WR), Ok(()));
    assert!(
        b_file
            .poll(PollEvents::POLLRDHUP)
            .has(PollEvents::POLLRDHUP)
    );
    assert_eq!(vsock::shutdown(&fx.fdt, a, 99), Err(Errno::EINVAL));
}

#[ktest]
fn datagram_socketpair_preserves_message_boundaries_and_truncation_flags() {
    let fx = fixture();
    let (a, b) = vsock::socketpair(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_DGRAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socketpair");

    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, a, b"abcdef", &[], None, 0),
        Ok(6)
    );
    let mut short = [0u8; 3];
    let out =
        vsock::recv(&fx.fdt, b, &mut short, 0, true, vsock::MSG_DONTWAIT, None).expect("recv");
    assert_eq!(out.len, 3);
    assert_eq!(&short, b"abc");
    assert!(out.msg_flags & vsock::MSG_TRUNC != 0);
    assert!(out.address.is_none());

    let mut empty = [0u8; 1];
    assert_eq!(
        vsock::recv(&fx.fdt, b, &mut empty, 0, false, vsock::MSG_DONTWAIT, None).map(|out| out.len),
        Err(Errno::EAGAIN)
    );
}

#[ktest]
fn abstract_stream_bind_listen_connect_accept_and_peercred() {
    let fx = fixture_with_cred(Credentials::unprivileged(Uid(1234), Gid(5678)));
    let addr = unique_abstract("vfs-abstract-stream");
    let listener = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("listener");
    let client = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("client");

    assert_eq!(vsock::bind(&fx.ctx, &fx.fdt, listener, &addr), Ok(()));
    assert_eq!(vsock::listen(&fx.fdt, listener, 4), Ok(()));
    assert_eq!(vsock::connect(&fx.ctx, &fx.fdt, client, &addr), Ok(()));

    let listener_file = fx.fdt.get_file(listener).expect("listener file");
    assert!(
        listener_file
            .poll(PollEvents::POLLIN)
            .has(PollEvents::POLLIN)
    );
    let (accepted, peer) =
        vsock::accept(&fx.ctx, &fx.fdt, listener, vsock::SOCK_CLOEXEC).expect("accept");
    assert!(peer.is_some());
    assert!(fx.fdt.fd_flags(accepted).unwrap().has(FdFlags::CLOEXEC));

    let cred = vsock::getsockopt(&fx.fdt, accepted, vsock::SOL_SOCKET, vsock::SO_PEERCRED)
        .expect("peercred");
    assert_eq!(u32::from_ne_bytes(cred[0..4].try_into().unwrap()), 0);
    assert_eq!(u32::from_ne_bytes(cred[4..8].try_into().unwrap()), 1234);
    assert_eq!(u32::from_ne_bytes(cred[8..12].try_into().unwrap()), 5678);

    let sockname = vsock::getsockname(&fx.fdt, listener).expect("sockname");
    assert_eq!(sockname, addr);
    let peername = vsock::getpeername(&fx.fdt, client).expect("peername");
    assert_eq!(peername, addr);
}

#[ktest]
fn seqpacket_accept_recv_sets_eor_and_preserves_records() {
    let fx = fixture();
    let addr = unique_abstract("vfs-seqpacket");
    let listener = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_SEQPACKET | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("listener");
    let client = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_SEQPACKET | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("client");

    assert_eq!(vsock::bind(&fx.ctx, &fx.fdt, listener, &addr), Ok(()));
    assert_eq!(vsock::listen(&fx.fdt, listener, 2), Ok(()));
    assert_eq!(vsock::connect(&fx.ctx, &fx.fdt, client, &addr), Ok(()));
    let (accepted, _) = vsock::accept(&fx.ctx, &fx.fdt, listener, 0).expect("accept");

    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, client, b"one", &[], None, vsock::MSG_EOR),
        Ok(3)
    );
    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, client, b"two", &[], None, vsock::MSG_EOR),
        Ok(3)
    );
    let mut buf = [0u8; 8];
    let first = vsock::recv(
        &fx.fdt,
        accepted,
        &mut buf,
        0,
        false,
        vsock::MSG_DONTWAIT,
        None,
    )
    .expect("first");
    assert_eq!(first.len, 3);
    assert_eq!(&buf[..3], b"one");
    assert!(first.msg_flags & vsock::MSG_EOR != 0);
    let second = vsock::recv(
        &fx.fdt,
        accepted,
        &mut buf,
        0,
        false,
        vsock::MSG_DONTWAIT,
        None,
    )
    .expect("second");
    assert_eq!(second.len, 3);
    assert_eq!(&buf[..3], b"two");
}

#[ktest]
fn sockopts_roundtrip_values_and_reject_bad_inputs() {
    let fx = fixture();
    let fd = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socket");

    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_DOMAIN).unwrap()),
        vsock::AF_UNIX as i32
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_TYPE).unwrap()),
        vsock::SOCK_STREAM as i32
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_PROTOCOL).unwrap()),
        0
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_ACCEPTCONN).unwrap()),
        0
    );

    assert_eq!(
        vsock::setsockopt(
            &fx.fdt,
            fd,
            vsock::SOL_SOCKET,
            vsock::SO_REUSEADDR,
            &1i32.to_ne_bytes()
        ),
        Ok(())
    );
    assert_eq!(
        vsock::setsockopt(
            &fx.fdt,
            fd,
            vsock::SOL_SOCKET,
            vsock::SO_REUSEPORT,
            &1i32.to_ne_bytes()
        ),
        Ok(())
    );
    assert_eq!(
        vsock::setsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_PASSCRED, &[1]),
        Ok(())
    );
    assert_eq!(
        vsock::setsockopt(
            &fx.fdt,
            fd,
            vsock::SOL_SOCKET,
            vsock::SO_SNDBUF,
            &4096i32.to_ne_bytes()
        ),
        Ok(())
    );
    assert_eq!(
        vsock::setsockopt(
            &fx.fdt,
            fd,
            vsock::SOL_SOCKET,
            vsock::SO_RCVBUF,
            &8192i32.to_ne_bytes()
        ),
        Ok(())
    );
    let mut linger = Vec::new();
    linger.extend_from_slice(&1i32.to_ne_bytes());
    linger.extend_from_slice(&7i32.to_ne_bytes());
    assert_eq!(
        vsock::setsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_LINGER, &linger),
        Ok(())
    );
    let mut timeout = Vec::new();
    timeout.extend_from_slice(&3i64.to_ne_bytes());
    timeout.extend_from_slice(&250i64.to_ne_bytes());
    assert_eq!(
        vsock::setsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_RCVTIMEO, &timeout),
        Ok(())
    );

    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_REUSEADDR).unwrap()),
        1
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_REUSEPORT).unwrap()),
        1
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_PASSCRED).unwrap()),
        1
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_SNDBUF).unwrap()),
        4096
    );
    assert_eq!(
        read_i32(&vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_RCVBUF).unwrap()),
        8192
    );
    assert_eq!(
        read_i64_pair(
            &vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_RCVTIMEO).unwrap()
        ),
        (3, 250)
    );

    assert_eq!(
        vsock::setsockopt(&fx.fdt, fd, 999, vsock::SO_PASSCRED, &[1]),
        Err(Errno::ENOPROTOOPT)
    );
    assert_eq!(
        vsock::setsockopt(
            &fx.fdt,
            fd,
            vsock::SOL_SOCKET,
            vsock::SO_SNDBUF,
            &0i32.to_ne_bytes()
        ),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        vsock::setsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, vsock::SO_RCVTIMEO, &[0; 8]),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        vsock::getsockopt(&fx.fdt, fd, vsock::SOL_SOCKET, 999),
        Err(Errno::ENOPROTOOPT)
    );
}

#[ktest]
fn scm_rights_installs_received_fds_and_honors_cloexec() {
    let fx = fixture();
    let (a, b) = vsock::socketpair(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_DGRAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socketpair");
    let plain = make_plain_file(&fx);
    let rights = rights_cmsg(&[plain]);

    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, a, b"x", &rights, None, 0),
        Ok(1)
    );
    let mut data = [0u8; 1];
    let out = vsock::recv(
        &fx.fdt,
        b,
        &mut data,
        64,
        false,
        vsock::MSG_DONTWAIT | vsock::MSG_CMSG_CLOEXEC,
        None,
    )
    .expect("recv");
    assert_eq!(out.len, 1);
    assert_eq!(&data, b"x");

    let cmsgs = parse_cmsgs(&out.control);
    assert_eq!(cmsgs.len(), 1);
    assert_eq!(cmsgs[0].0, vsock::SOL_SOCKET);
    assert_eq!(cmsgs[0].1, vsock::SCM_RIGHTS);
    assert_eq!(cmsgs[0].2.len(), 4);
    let received_fd = Fd::from_raw(i32::from_ne_bytes(cmsgs[0].2[0..4].try_into().unwrap()) as u32);
    assert!(fx.fdt.get_file(received_fd).is_some());
    assert!(fx.fdt.fd_flags(received_fd).unwrap().has(FdFlags::CLOEXEC));
}

#[ktest]
fn scm_rights_rejects_direct_socket_handle_cycle() {
    let fx = fixture();
    let (a, _b) = vsock::socketpair(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socketpair");
    let rights = rights_cmsg(&[a]);

    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, a, b"x", &rights, None, 0),
        Err(Errno::EINVAL)
    );
}

#[ktest]
fn scm_credentials_are_generated_by_passcred_and_explicit_credentials_are_checked() {
    let fx = fixture_with_cred(Credentials::unprivileged(Uid(44), Gid(55)));
    let (a, b) = vsock::socketpair(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_DGRAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socketpair");

    assert_eq!(
        vsock::setsockopt(&fx.fdt, b, vsock::SOL_SOCKET, vsock::SO_PASSCRED, &[1]),
        Ok(())
    );
    assert_eq!(vsock::send(&fx.ctx, &fx.fdt, a, b"c", &[], None, 0), Ok(1));
    let mut buf = [0u8; 1];
    let out =
        vsock::recv(&fx.fdt, b, &mut buf, 64, false, vsock::MSG_DONTWAIT, None).expect("recv");
    let cmsgs = parse_cmsgs(&out.control);
    assert_eq!(cmsgs.len(), 1);
    assert_eq!(cmsgs[0].0, vsock::SOL_SOCKET);
    assert_eq!(cmsgs[0].1, vsock::SCM_CREDENTIALS);
    assert_eq!(u32::from_ne_bytes(cmsgs[0].2[0..4].try_into().unwrap()), 0);
    assert_eq!(u32::from_ne_bytes(cmsgs[0].2[4..8].try_into().unwrap()), 44);
    assert_eq!(
        u32::from_ne_bytes(cmsgs[0].2[8..12].try_into().unwrap()),
        55
    );

    let zero_creds = creds_cmsg(0, 0, 0);
    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, a, b"z", &zero_creds, None, 0),
        Ok(1)
    );
    let bad_creds = creds_cmsg(123, 44, 55);
    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, a, b"z", &bad_creds, None, 0),
        Err(Errno::EPERM)
    );
}

#[ktest]
fn receive_control_truncation_does_not_install_partial_rights() {
    let fx = fixture();
    let (a, b) = vsock::socketpair(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_DGRAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socketpair");
    let plain_a = make_plain_file(&fx);
    let plain_b = make_plain_file(&fx);
    let before = fx.fdt.len();

    assert_eq!(
        vsock::send(
            &fx.ctx,
            &fx.fdt,
            a,
            b"x",
            &rights_cmsg(&[plain_a, plain_b]),
            None,
            0
        ),
        Ok(1)
    );
    let mut buf = [0u8; 1];
    let out =
        vsock::recv(&fx.fdt, b, &mut buf, 16, false, vsock::MSG_DONTWAIT, None).expect("recv");
    assert!(out.msg_flags & vsock::MSG_CTRUNC != 0);
    assert!(out.control.is_empty());
    assert_eq!(fx.fdt.len(), before);
}

#[ktest]
fn pathname_bind_connect_unlink_and_double_bind_cleanup() {
    let fx = fixture();
    let path = unique_name("sock-path");
    let raw = path_addr(&path);
    let listener = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("listener");
    let client = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("client");

    assert_eq!(vsock::bind(&fx.ctx, &fx.fdt, listener, &raw), Ok(()));
    let stat = operation::fstatat(&fx.ctx, &Dirfd::Cwd, &path, false).expect("stat");
    assert_eq!(
        stat.mode & FileType::Socket.to_mode_bits(),
        FileType::Socket.to_mode_bits()
    );
    assert_eq!(vsock::listen(&fx.fdt, listener, 1), Ok(()));
    assert_eq!(vsock::connect(&fx.ctx, &fx.fdt, client, &raw), Ok(()));
    let (accepted, _) = vsock::accept(&fx.ctx, &fx.fdt, listener, 0).expect("accept");
    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, client, b"p", &[], None, 0),
        Ok(1)
    );
    let mut data = [0u8; 1];
    assert_eq!(
        vsock::recv(
            &fx.fdt,
            accepted,
            &mut data,
            0,
            false,
            vsock::MSG_DONTWAIT,
            None
        )
        .unwrap()
        .len,
        1
    );
    assert_eq!(&data, b"p");

    assert_eq!(operation::unlink(&fx.ctx, &Dirfd::Cwd, &path), Ok(()));
    let late = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_STREAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("late");
    assert_eq!(
        vsock::connect(&fx.ctx, &fx.fdt, late, &raw),
        Err(Errno::ENOENT)
    );

    let one = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_DGRAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("one");
    let first_path = unique_name("sock-first");
    let second_path = unique_name("sock-second");
    assert_eq!(
        vsock::bind(&fx.ctx, &fx.fdt, one, &path_addr(&first_path)),
        Ok(())
    );
    assert_eq!(
        vsock::bind(&fx.ctx, &fx.fdt, one, &path_addr(&second_path)),
        Err(Errno::EADDRINUSE)
    );
    assert!(operation::fstatat(&fx.ctx, &Dirfd::Cwd, &second_path, false).is_err());
}

#[ktest]
fn invalid_sockaddr_and_wrong_fd_errors_are_mapped_at_vfs_boundary() {
    let fx = fixture();
    let fd = vsock::socket(
        &fx.ctx,
        &fx.fdt,
        vsock::AF_UNIX as usize,
        vsock::SOCK_DGRAM | vsock::SOCK_NONBLOCK,
        0,
    )
    .expect("socket");
    let plain = make_plain_file(&fx);
    let mut wrong_family = Vec::new();
    wrong_family.extend_from_slice(&2u16.to_ne_bytes());
    wrong_family.extend_from_slice(b"name");

    assert_eq!(vsock::bind(&fx.ctx, &fx.fdt, fd, &[0]), Err(Errno::EINVAL));
    assert_eq!(
        vsock::bind(&fx.ctx, &fx.fdt, fd, &wrong_family),
        Err(Errno::EAFNOSUPPORT)
    );
    assert_eq!(vsock::listen(&fx.fdt, plain, 1), Err(Errno::ENOTSOCK));
    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, Fd::from_raw(999), b"x", &[], None, 0),
        Err(Errno::EBADF)
    );
    assert_eq!(
        vsock::send(&fx.ctx, &fx.fdt, fd, b"x", &[], None, 0x8000_0000),
        Err(Errno::EINVAL)
    );
    let mut buf = [0u8; 1];
    assert_eq!(
        vsock::recv(&fx.fdt, fd, &mut buf, 0, false, 0x8000_0000, None).map(|out| out.len),
        Err(Errno::EINVAL)
    );
}
