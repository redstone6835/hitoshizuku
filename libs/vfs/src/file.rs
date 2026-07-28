//! 打开文件描述符（File）与文件操作接口（FileOps）。
//!
//! 当进程调用 `open(2)` 时，内核创建一个 [`File`] 对象并将其编号（fd）返回给
//! 用户空间。此后进程通过 fd 进行所有 I/O 操作，而不再直接接触路径或 Inode。
//!
//! ### 安全设计
//!
//! - **能力冻结**：`File` 在创建时固化了打开标志（[`OpenOptions`]）和调用者凭据
//!   （[`Credentials`]）。后续的 `read`/`write` 调用不再重新检查路径权限，只验证
//!   描述符标志是否允许该操作，从而消除 TOCTOU（检查时间与使用时间之间的竞争）。
//!   凭据以 `Arc<Credentials>` 存储，与 `VfsContext::cred` 保持一致，避免克隆。
//!
//! - **无裸 Inode 暴露**：`File` 持有 `Arc<Inode>` 但不对外暴露。用户空间只能
//!   通过 fd 编号操作文件，无法直接访问内核的 Inode 结构。
//!
//! - **位置串行化**：文件读写偏移量（`pos`）由 `pos_lock` 串行保护。
//!   多个 fd 通过 `dup` 共享同一个 [`File`] 时，普通 `read`/`write`/`lseek`
//!   对打开文件描述的偏移推进保持原子；`pread`/`pwrite` 仍不修改共享偏移。
//!
//! - **release 语义**：[`File`] 实现 `Drop`，在析构时自动调用 [`FileOps::release`]，
//!   保证驱动私有状态（缓冲区、打开计数等）在最后一个引用消失时被正确清理。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use errno::Errno;
use sched::Task;

use crate::vfs::dentry::SmallStr;

use crate::vfs::cred::Credentials;
use crate::vfs::error::VfsResult;
use crate::vfs::inode::{Inode, InodeWriteAccess};
use crate::vfs::stat::FileStat;
use crate::vfs::sync::Spinlock;

static FILE_LIVE: AtomicUsize = AtomicUsize::new(0);
static FILE_CREATED: AtomicUsize = AtomicUsize::new(0);
static FILE_DROPPED: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, Default)]
pub struct FileDiag {
    pub live: usize,
    pub created: usize,
    pub dropped: usize,
}

#[kernel_symbols::export(
    name = "vfs.file.file_diag",
    contract = "kernel.vfs.file-diagnostic@1",
    version = 1,
    capabilities = kernel_symbols::capability::VFS_QUERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC
)]
pub fn file_diag() -> FileDiag {
    FileDiag {
        live: FILE_LIVE.load(Ordering::Acquire),
        created: FILE_CREATED.load(Ordering::Acquire),
        dropped: FILE_DROPPED.load(Ordering::Acquire),
    }
}

// ── 打开选项（Open Options） ──────────────────────────────────────────────────

/// 文件访问模式，对应 `open(2)` `flags` 参数的低 2 位语义。
///
/// 此枚举与任何平台 ABI 数值**完全解耦**。Linux ABI 到本枚举的映射在
/// `arch/` 层的 syscall 入口完成，VFS 内部只使用此枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessMode {
    /// 只读打开（`O_RDONLY`）。
    #[default]
    ReadOnly,
    /// 只写打开（`O_WRONLY`）。
    WriteOnly,
    /// 读写打开（`O_RDWR`）。
    ReadWrite,
}

/// 文件打开选项（平台无关语义表示）。
///
/// 此结构体是平台无关的语义表示：每个字段对应一个独立的 `open(2)` 语义选项，
/// 不携带任何 Linux 或具体架构 ABI 的位编号。
/// Linux ABI 到本结构的解码在 arch 层的 `decode_open_flags` 中完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenOptions {
    /// 访问模式：只读/只写/读写。
    pub access: AccessMode,
    /// 若文件不存在则创建（`O_CREAT`）。
    pub create: bool,
    /// 与 `create` 合用，若文件已存在则失败（`O_EXCL`）。
    pub exclusive: bool,
    /// 打开时截断文件大小为 0（`O_TRUNC`）。
    pub truncate: bool,
    /// 每次 `write` 前原子地将偏移定位到文件末尾（`O_APPEND`）。
    pub append: bool,
    /// 若末端分量是符号链接，不跟随（`O_NOFOLLOW`）。
    pub nofollow: bool,
    /// 要求末端分量必须是目录（`O_DIRECTORY`）。
    pub directory: bool,
    /// 不更新 atime（`O_NOATIME`）。
    pub noatime: bool,
    /// 仅获取路径引用，不可 read/write（`O_PATH`）。
    pub path_only: bool,
    /// 非阻塞 I/O（`O_NONBLOCK`）。
    pub nonblock: bool,
    /// 同步写（`O_SYNC`）。
    pub sync: bool,
    /// 直接 I/O（`O_DIRECT`）。
    pub direct: bool,
    /// 执行后关闭（`O_CLOEXEC`）。
    pub cloexec: bool,
}

impl OpenOptions {
    /// 判断是否可读（`ReadOnly` 或 `ReadWrite`，且非 `path_only`）。
    pub const fn readable(self) -> bool {
        if self.path_only {
            return false;
        }
        matches!(self.access, AccessMode::ReadOnly | AccessMode::ReadWrite)
    }

    /// 判断是否可写（`WriteOnly` 或 `ReadWrite`，且非 `path_only`）。
    pub const fn writable(self) -> bool {
        if self.path_only {
            return false;
        }
        matches!(self.access, AccessMode::WriteOnly | AccessMode::ReadWrite)
    }
}

/// 文件范围预分配操作的语义模式。
///
/// 这是 VFS 内部的类型化表示，不暴露任何具体系统调用 ABI 的位编号。系统调用
/// 层负责把用户态的模式位解码成这里的值，具体文件系统只需要实现自己支持的
/// 语义。未知位不会被静默丢弃，底层实现应返回 [`VfsError::NotSupported`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FallocateMode(u32);

impl FallocateMode {
    /// 不改变文件大小的额外标志：普通预分配，同时允许扩大逻辑文件大小。
    pub const NONE: Self = Self(0);
    /// 只分配存储，不改变逻辑文件大小。
    pub const KEEP_SIZE: Self = Self(1 << 0);
    /// 释放指定范围的已分配页；必须与 [`Self::KEEP_SIZE`] 一起使用。
    pub const PUNCH_HOLE: Self = Self(1 << 1);

    /// 从 VFS 内部位集合构造模式。
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// 返回模式的内部位集合。
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// 判断是否包含指定语义位。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 合并两个模式位。
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

// ── 文件定位 ──────────────────────────────────────────────────────────────────

/// `lseek(2)` 的基准点，对应 `SEEK_SET`/`SEEK_CUR`/`SEEK_END`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekFrom {
    /// 从文件开头计算（`SEEK_SET`），`offset` 必须 ≥ 0。
    Start(u64),
    /// 从当前位置计算（`SEEK_CUR`），`offset` 可以为负（向前移动）。
    Current(i64),
    /// 从文件末尾计算（`SEEK_END`），`offset` 通常 ≤ 0。
    End(i64),
    /// 从指定绝对偏移查找下一个已分配数据区（`SEEK_DATA`）。
    Data(u64),
    /// 从指定绝对偏移查找下一个空洞或文件末尾（`SEEK_HOLE`）。
    Hole(u64),
}

// ── I/O 事件掩码（poll/select/epoll） ────────────────────────────────────────

/// I/O 事件掩码，对应 `poll(2)` 的 `events`/`revents` 字段。
///
/// 数值与 Linux `<poll.h>` 保持一致，以便系统调用层直接透传。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PollEvents(pub u16);

impl PollEvents {
    /// 描述符可读（有数据到达，或连接关闭的 EOF）。
    pub const POLLIN: Self = Self(0x0001);
    /// 有高优先级数据可读（out-of-band）。
    pub const POLLPRI: Self = Self(0x0002);
    /// 描述符可写（发送缓冲区有空间）。
    pub const POLLOUT: Self = Self(0x0004);
    /// 发生错误（仅在 `revents` 中出现，`events` 中无意义）。
    pub const POLLERR: Self = Self(0x0008);
    /// 对端关闭连接（仅在 `revents` 中出现）。
    pub const POLLHUP: Self = Self(0x0010);
    /// 描述符无效（仅在 `revents` 中出现，通常表示传入了无效的 fd）。
    pub const POLLNVAL: Self = Self(0x0020);
    /// 对端关闭写半边（Linux POLLRDHUP / EPOLLRDHUP）。
    pub const POLLRDHUP: Self = Self(0x2000);
    /// 非状态型文件的默认就绪集合：读写操作都不会因为等待外部事件而休眠。
    pub const READ_WRITE_READY: Self = Self::POLLIN.with(Self::POLLOUT);

    /// 将两个事件掩码合并。
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// 判断是否包含指定事件。
    pub const fn has(self, event: Self) -> bool {
        self.0 & event.0 != 0
    }
    /// 对两个掩码求交集（常用于 interest ∩ ready）。
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
    /// 从当前掩码中移除指定事件。
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
    /// 返回原始位字。
    pub const fn raw(self) -> u16 {
        self.0
    }
    /// 判断是否没有任何事件就绪。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StatusFlags(u32);

impl StatusFlags {
    const APPEND: u32 = 1 << 0;
    const NONBLOCK: u32 = 1 << 1;
    const SYNC: u32 = 1 << 2;
    const DIRECT: u32 = 1 << 3;

    fn from_open_options(opts: OpenOptions) -> Self {
        let mut bits = 0u32;
        if opts.append {
            bits |= Self::APPEND;
        }
        if opts.nonblock {
            bits |= Self::NONBLOCK;
        }
        if opts.sync {
            bits |= Self::SYNC;
        }
        if opts.direct {
            bits |= Self::DIRECT;
        }
        Self(bits)
    }

    fn apply(self, mut opts: OpenOptions) -> OpenOptions {
        opts.append = (self.0 & Self::APPEND) != 0;
        opts.nonblock = (self.0 & Self::NONBLOCK) != 0;
        opts.sync = (self.0 & Self::SYNC) != 0;
        opts.direct = (self.0 & Self::DIRECT) != 0;
        opts
    }
}

// ── ioctl 命令号（Linux _IOC 编码）──────────────────────────────────────────

/// `ioctl(2)` 命令号的统一表示。
///
/// VFS 只携带和解码命令号；具体命令到设备语义的翻译由对应的 [`FileOps`]
/// 实现完成。这样 syscall 层不需要知道底层设备类型，底层驱动也不需要直接暴露
/// Linux ABI 号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoctlCmd(usize);

impl IoctlCmd {
    pub const IOC_NONE: usize = 0;
    pub const IOC_WRITE: usize = 1;
    pub const IOC_READ: usize = 2;

    const IOC_NRBITS: usize = 8;
    const IOC_TYPEBITS: usize = 8;
    const IOC_SIZEBITS: usize = 14;
    const IOC_DIRBITS: usize = 2;

    const IOC_NRMASK: usize = (1 << Self::IOC_NRBITS) - 1;
    const IOC_TYPEMASK: usize = (1 << Self::IOC_TYPEBITS) - 1;
    const IOC_SIZEMASK: usize = (1 << Self::IOC_SIZEBITS) - 1;
    const IOC_DIRMASK: usize = (1 << Self::IOC_DIRBITS) - 1;

    const IOC_NRSHIFT: usize = 0;
    const IOC_TYPESHIFT: usize = Self::IOC_NRSHIFT + Self::IOC_NRBITS;
    const IOC_SIZESHIFT: usize = Self::IOC_TYPESHIFT + Self::IOC_TYPEBITS;
    const IOC_DIRSHIFT: usize = Self::IOC_SIZESHIFT + Self::IOC_SIZEBITS;

    pub const fn new(raw: usize) -> Self {
        Self(raw)
    }

    pub const fn from_parts(dir: usize, ty: usize, nr: usize, size: usize) -> Self {
        Self(
            ((dir & Self::IOC_DIRMASK) << Self::IOC_DIRSHIFT)
                | ((ty & Self::IOC_TYPEMASK) << Self::IOC_TYPESHIFT)
                | ((nr & Self::IOC_NRMASK) << Self::IOC_NRSHIFT)
                | ((size & Self::IOC_SIZEMASK) << Self::IOC_SIZESHIFT),
        )
    }

    pub const fn raw(self) -> usize {
        self.0
    }

    pub const fn dir(self) -> usize {
        (self.0 >> Self::IOC_DIRSHIFT) & Self::IOC_DIRMASK
    }

    pub const fn ty(self) -> usize {
        (self.0 >> Self::IOC_TYPESHIFT) & Self::IOC_TYPEMASK
    }

    pub const fn nr(self) -> usize {
        (self.0 >> Self::IOC_NRSHIFT) & Self::IOC_NRMASK
    }

    pub const fn size(self) -> usize {
        (self.0 >> Self::IOC_SIZESHIFT) & Self::IOC_SIZEMASK
    }
}

// ── 目录项（readdir 结果） ────────────────────────────────────────────────────

/// `readdir(3)` / `getdents(2)` 返回的单个目录项。
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// 该条目的 inode 号。
    pub ino: u64,
    /// 条目名称（不含路径分隔符）。
    ///
    /// 使用 [`SmallStr`] 存储：≤15 字节的常见名称（`"."`, `".."`, `"bin"` 等）
    /// 零堆分配；超过阈值时透明退化为堆字符串。
    pub name: SmallStr,
    /// 文件类型（用于 `d_type` 字段；部分文件系统填 `Unknown` 需调用方 stat）。
    pub kind: crate::vfs::stat::FileType,
}

// ── 打开文件描述符 ────────────────────────────────────────────────────────────

/// 打开的文件描述符，持有对文件系统对象的引用与操作方法。
///
/// 一个 `File` 实例在调用 `open`/`openat` 时创建，并被包装进进程的文件描述符表
/// (`FdTable`) 中，以 `Arc<File>` 持有。`dup`/`dup2` 克隆的是 `Arc` 而不是
/// `File` 本身，因此多个 fd 可以共享同一个 `File`（共享偏移量）。
///
/// 析构时（最后一个 `Arc<File>` drop）自动调用 [`FileOps::release`]，确保驱动
/// 私有状态被正确清理，无需调用方手动关闭。
pub struct File {
    /// 该文件对应的 Inode（含文件系统元数据与操作）。
    ///
    /// 持有 Arc 确保即使文件被 `unlink` 删除，只要仍有打开的 fd，inode 就不会
    /// 被回收（POSIX 语义：文件数据在最后一个 fd 关闭时才真正消失）。
    pub(crate) inode: Arc<Inode>,

    /// 打开时使用的选项（已经过 VFS 层验证与规范化）。
    ///
    /// 后续 I/O 操作只依据此字段判断权限，不再重新查询路径。
    pub(crate) flags: OpenOptions,

    /// 可通过 `fcntl(F_SETFL)` 变更的状态位（当前支持 APPEND/NONBLOCK/SYNC）。
    status_flags: AtomicU32,

    /// `fcntl(F_SETOWN[_EX])` 维护的异步通知接收者类型。
    owner_type: AtomicI32,

    /// `fcntl(F_SETOWN[_EX])` 维护的异步通知接收者 pid。
    owner_pid: AtomicI32,

    /// `fcntl(F_SETSIG)` 维护的异步通知信号号。
    owner_sig: AtomicI32,

    /// 当前文件读写偏移量（字节）。
    pub(crate) pos: AtomicU64,

    /// 指向此打开文件描述的 fd 数量。
    ///
    /// 该计数独立于 `Arc` 强引用：epoll watch、VMA 和内核临时引用都不属于
    /// 用户可见 fd，不能参与“最后一个描述符已关闭”的判断。
    fd_references: AtomicUsize,

    /// 监听此打开文件描述最后一个 fd 关闭事件的对象。
    description_close_observers: Spinlock<Vec<Weak<File>>>,

    /// 串行化会读取或推进共享偏移的操作。
    pos_lock: Spinlock<()>,

    /// 打开时保存的进程凭据快照（共享引用）。
    ///
    /// 使用 `Arc<Credentials>` 与 `VfsContext::cred` 保持一致：`open` 时从
    /// context 克隆 Arc（增加引用计数），不复制整个 `Credentials` 结构。
    /// 凭据冻结确保后续 I/O 不受进程后续 `setuid`/`setgid` 的影响。
    pub(crate) cred: Arc<Credentials>,

    /// 文件系统特定的打开状态与操作（如缓冲区、设备私有数据等）。
    pub(crate) ops: Box<dyn FileOps + Send + Sync>,

    /// 此文件对应的 Dentry（路径解析时的定位信息）。
    ///
    /// 当 `Dirfd::Fd(file)` 作为 `openat` 等系统调用的基准目录时，路径解析器
    /// 需要知道此 File 对应哪个 Dentry（而非仅有 Inode），以便从该 Dentry 继续
    /// 向下解析相对路径。`O_PATH` 描述符主要用于此目的。
    pub(crate) dentry: Arc<crate::vfs::dentry::Dentry>,

    /// 此文件所在的挂载点。
    ///
    /// `File::drop` 时自动调用 `mount.dec_open()`，保证卸载安全检查（`is_busy()`）
    /// 的准确性：只要有打开的 fd，挂载点就不会被误判为空闲而被强制卸载。
    pub(crate) mount: Arc<crate::vfs::mount::Mount>,

    /// 普通文件的写访问租约。
    ///
    /// 只有 VFS 规范打开路径会设置该字段；设备、管道、套接字等合成描述符不参与
    /// 可执行文件写入排斥。租约跟随打开文件描述而不是 fd，因此 `dup` 不重复计数。
    _write_access: Option<InodeWriteAccess>,
}

#[kernel_symbols::export]
impl File {
    /// 构造一个新的打开文件描述符。
    ///
    /// 由 VFS 层在 `InodeOps::open` 返回后调用；`dentry` 是打开的文件对应的 Dentry，
    /// 用于 `Dirfd::Fd` 场景下的路径解析基准。
    #[kernel_symbols::export(
        name = "vfs.file.File.new",
        contract = "kernel.vfs.file@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 1 << 3
    )]
    pub fn new(
        inode: Arc<Inode>,
        flags: OpenOptions,
        cred: Arc<Credentials>,
        ops: Box<dyn FileOps + Send + Sync>,
        dentry: Arc<crate::vfs::dentry::Dentry>,
        mount: Arc<crate::vfs::mount::Mount>,
    ) -> Self {
        Self::new_inner(inode, flags, cred, ops, dentry, mount, None)
    }

    /// 使用已经获取的普通文件写访问租约构造打开文件描述。
    pub(crate) fn new_with_write_access(
        inode: Arc<Inode>,
        flags: OpenOptions,
        cred: Arc<Credentials>,
        ops: Box<dyn FileOps + Send + Sync>,
        dentry: Arc<crate::vfs::dentry::Dentry>,
        mount: Arc<crate::vfs::mount::Mount>,
        write_access: InodeWriteAccess,
    ) -> Self {
        Self::new_inner(inode, flags, cred, ops, dentry, mount, Some(write_access))
    }

    fn new_inner(
        inode: Arc<Inode>,
        flags: OpenOptions,
        cred: Arc<Credentials>,
        ops: Box<dyn FileOps + Send + Sync>,
        dentry: Arc<crate::vfs::dentry::Dentry>,
        mount: Arc<crate::vfs::mount::Mount>,
        write_access: Option<InodeWriteAccess>,
    ) -> Self {
        FILE_CREATED.fetch_add(1, Ordering::Relaxed);
        FILE_LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            inode,
            flags,
            status_flags: AtomicU32::new(StatusFlags::from_open_options(flags).0),
            owner_type: AtomicI32::new(0),
            owner_pid: AtomicI32::new(0),
            owner_sig: AtomicI32::new(0),
            pos: AtomicU64::new(0),
            fd_references: AtomicUsize::new(0),
            description_close_observers: Spinlock::new(Vec::new()),
            pos_lock: Spinlock::new(()),
            cred,
            ops,
            dentry,
            mount,
            _write_access: write_access,
        }
    }

    /// 返回此文件所在挂载点的共享引用。
    #[kernel_symbols::export(
        name = "vfs.file.File.mount",
        contract = "kernel.vfs.file@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn mount(&self) -> &Arc<crate::vfs::mount::Mount> {
        &self.mount
    }

    /// 返回此文件对应的 Dentry。
    #[kernel_symbols::export(
        name = "vfs.file.File.dentry",
        contract = "kernel.vfs.file@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn dentry(&self) -> &Arc<crate::vfs::dentry::Dentry> {
        &self.dentry
    }

    /// 返回当前读写偏移量。
    #[kernel_symbols::export(
        name = "vfs.file.File.pos",
        contract = "kernel.vfs.file@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn pos(&self) -> u64 {
        self.pos.load(Ordering::Acquire)
    }

    pub(crate) fn acquire_fd_reference(&self) {
        let previous = self.fd_references.fetch_add(1, Ordering::AcqRel);
        assert!(previous != usize::MAX, "打开文件描述的 fd 引用计数已耗尽");
    }

    pub(crate) fn release_fd_reference(&self) -> bool {
        let previous = self.fd_references.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "打开文件描述的 fd 引用计数下溢");
        previous == 1
    }

    pub(crate) fn register_description_close_observer(&self, observer: &Arc<File>) {
        let mut observers = self.description_close_observers.lock();
        observers.retain(|weak| weak.upgrade().is_some());
        if observers.iter().any(|weak| {
            weak.upgrade()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, observer))
        }) {
            return;
        }
        observers.push(Arc::downgrade(observer));
    }

    pub(crate) fn notify_description_closed(file: &Arc<File>) {
        let observers = {
            let mut registered = file.description_close_observers.lock();
            let mut observers = Vec::new();
            registered.retain(|weak| {
                if let Some(observer) = weak.upgrade() {
                    observers.push(observer);
                    true
                } else {
                    false
                }
            });
            observers
        };
        for observer in observers {
            observer.on_file_description_closed(file);
        }
    }

    /// 返回打开选项。
    #[kernel_symbols::export(
        name = "vfs.file.File.flags",
        contract = "kernel.vfs.file@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn flags(&self) -> OpenOptions {
        StatusFlags(self.status_flags.load(Ordering::Acquire)).apply(self.flags)
    }

    pub fn set_status_flags(&self, append: bool, nonblock: bool, sync: bool, direct: bool) {
        let mut bits = 0u32;
        if append {
            bits |= StatusFlags::APPEND;
        }
        if nonblock {
            bits |= StatusFlags::NONBLOCK;
        }
        if sync {
            bits |= StatusFlags::SYNC;
        }
        if direct {
            bits |= StatusFlags::DIRECT;
        }
        self.status_flags.store(bits, Ordering::Release);
        self.ops.set_status_flags(self.flags());
    }

    pub fn owner(&self) -> (i32, i32) {
        (
            self.owner_type.load(Ordering::Acquire),
            self.owner_pid.load(Ordering::Acquire),
        )
    }

    pub fn set_owner(&self, owner_type: i32, owner_pid: i32) {
        self.owner_type.store(owner_type, Ordering::Release);
        self.owner_pid.store(owner_pid, Ordering::Release);
    }

    pub fn owner_sig(&self) -> i32 {
        self.owner_sig.load(Ordering::Acquire)
    }

    pub fn set_owner_sig(&self, sig: i32) {
        self.owner_sig.store(sig, Ordering::Release);
    }

    /// 返回打开时冻结的凭据。
    pub fn cred(&self) -> &Arc<Credentials> {
        &self.cred
    }

    /// 返回对应 Inode 的共享引用。
    pub fn inode(&self) -> &Arc<Inode> {
        &self.inode
    }

    /// 为内核执行映像装载器创建一个可读文件视图。
    ///
    /// `O_PATH` 描述符本身禁止普通读写，但 `execveat(AT_EMPTY_PATH)` 必须能够从其
    /// 指向的 inode 装载映像。调用方必须先完成执行权限和写入排斥检查；本方法只
    /// 重新建立驱动文件操作对象，不重新解析路径，也不向用户态暴露新的描述符。
    pub fn open_exec_view(&self, cred: Arc<Credentials>) -> VfsResult<Arc<Self>> {
        if self.inode.kind() != crate::vfs::stat::FileType::Regular {
            return Err(crate::vfs::error::VfsError::PermissionDenied);
        }
        let flags = OpenOptions {
            access: AccessMode::ReadOnly,
            ..OpenOptions::default()
        };
        let ops = self.inode.ops.open(&self.inode, &flags, &cred)?;
        let file = Self::new(
            Arc::clone(&self.inode),
            flags,
            cred,
            ops,
            Arc::clone(&self.dentry),
            Arc::clone(&self.mount),
        );
        self.mount.inc_open();
        Ok(Arc::new(file))
    }

    /// 读取数据到 `buf`，从当前偏移量开始，读完后推进偏移量。
    ///
    /// 对 `O_PATH` 描述符调用 `read` 将返回 `VfsError::BadFileDescriptor`。
    #[kernel_symbols::export(
        name = "vfs.file.File.read",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::VfsRead);
        if !self.flags().readable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        let _pos_guard = self.pos_lock.lock();
        let offset = self.pos.load(Ordering::Acquire);
        let n = self.ops.read_at(buf, offset)?;
        self.pos
            .store(offset.saturating_add(n as u64), Ordering::Release);
        #[cfg(feature = "performance-profile")]
        profile.set_bytes(n);
        Ok(n)
    }

    /// 将 `buf` 中的数据写入文件，从当前偏移量（或文件末尾，若 `O_APPEND`）开始。
    #[kernel_symbols::export(
        name = "vfs.file.File.write",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn write(&self, buf: &[u8]) -> VfsResult<usize> {
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::VfsWrite);
        let flags = self.flags();
        if !flags.writable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        let _pos_guard = self.pos_lock.lock();
        let _data_mutation = (!buf.is_empty()).then(|| self.inode.begin_data_mutation());
        if flags.append {
            let n = self.ops.write_at(buf, u64::MAX)?;
            let new_eof = self.inode.size();
            self.pos.store(new_eof, Ordering::Release);
            #[cfg(feature = "performance-profile")]
            profile.set_bytes(n);
            Ok(n)
        } else {
            let offset = self.pos.load(Ordering::Acquire);
            let n = self.ops.write_at(buf, offset)?;
            self.pos
                .store(offset.saturating_add(n as u64), Ordering::Release);
            #[cfg(feature = "performance-profile")]
            profile.set_bytes(n);
            Ok(n)
        }
    }

    /// 在指定偏移量处读取，不改变描述符的当前偏移量（`pread64`）。
    #[kernel_symbols::export(
        name = "vfs.file.File.read_at",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO
    )]
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::VfsRead);
        if !self.flags().readable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        if !self.ops.is_seekable() {
            return Err(crate::vfs::error::VfsError::IllegalSeek);
        }
        let n = self.ops.read_at(buf, offset)?;
        #[cfg(feature = "performance-profile")]
        profile.set_bytes(n);
        Ok(n)
    }

    /// 从指定偏移精确初始化一组页面，不改变描述符的当前偏移量。
    ///
    /// 前 `valid_len` 字节必须来自文件，剩余页面尾部由底层实现清零。该接口供
    /// VM 批量缺页路径使用，普通文件系统可以覆盖 [`FileOps::read_pages_at`]
    /// 以避免中间缓冲和二次复制。
    pub fn read_pages_at(
        &self,
        offset: u64,
        pages: &mut [&mut [u8]],
        valid_len: usize,
    ) -> VfsResult<()> {
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::VfsRead);
        if !self.flags().readable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        if !self.ops.is_seekable() {
            return Err(crate::vfs::error::VfsError::IllegalSeek);
        }
        self.ops.read_pages_at(offset, pages, valid_len)?;
        #[cfg(feature = "performance-profile")]
        profile.set_bytes(valid_len);
        Ok(())
    }

    /// 在指定偏移量处写入，不改变描述符的当前偏移量（`pwrite64`）。
    #[kernel_symbols::export(
        name = "vfs.file.File.write_at",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "performance-profile")]
        let mut profile = profiling::scope(profiling::Event::VfsWrite);
        if !self.flags().writable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        if !self.ops.is_seekable() {
            return Err(crate::vfs::error::VfsError::IllegalSeek);
        }
        let _data_mutation = (!buf.is_empty()).then(|| self.inode.begin_data_mutation());
        let n = self.ops.write_at(buf, offset)?;
        #[cfg(feature = "performance-profile")]
        profile.set_bytes(n);
        Ok(n)
    }

    /// 移动文件偏移量（`lseek`）。返回移动后的绝对偏移量。
    #[kernel_symbols::export(
        name = "vfs.file.File.seek",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn seek(&self, from: SeekFrom) -> VfsResult<u64> {
        let _pos_guard = self.pos_lock.lock();
        let new_pos = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(n) => {
                let cur = self.pos.load(Ordering::Acquire);
                if n >= 0 {
                    cur.checked_add(n as u64)
                        .ok_or(crate::vfs::error::VfsError::InvalidArgument)?
                } else {
                    let abs = n.unsigned_abs();
                    if abs > cur {
                        return Err(crate::vfs::error::VfsError::InvalidArgument);
                    }
                    cur - abs
                }
            }
            SeekFrom::End(n) => {
                let size = self.inode.size();
                if n >= 0 {
                    size.checked_add(n as u64)
                        .ok_or(crate::vfs::error::VfsError::InvalidArgument)?
                } else {
                    let abs = n.unsigned_abs();
                    if abs > size {
                        return Err(crate::vfs::error::VfsError::InvalidArgument);
                    }
                    size - abs
                }
            }
            SeekFrom::Data(offset) => self.ops.seek_data(offset, self.inode.size())?,
            SeekFrom::Hole(offset) => self.ops.seek_hole(offset, self.inode.size())?,
        };
        self.pos.store(new_pos, Ordering::Release);
        Ok(new_pos)
    }

    /// 枚举目录项（`getdents`），逐条调用 `sink`。仅对 `FileType::Directory` 有效。
    ///
    /// 从当前内部游标（`pos`）起，将每条 [`DirEntry`] 传给 `sink`：
    /// - `sink` 返回 `ControlFlow::Continue(())` 时继续枚举下一条；
    /// - `sink` 返回 `ControlFlow::Break(())` 时提前停止枚举（满缓冲区场景）。
    ///
    /// 函数返回枚举实际停止时的新游标值（可用于下次 `readdir` 的起始位置）。
    /// 游标语义由驱动自定义（通常为已枚举条目数；不同文件系统实现可能不同）。
    pub fn readdir(&self, sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        #[cfg(feature = "performance-profile")]
        let _profile = profiling::scope(profiling::Event::VfsGetdents);
        if !self.flags().readable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        if self.inode.kind != crate::vfs::stat::FileType::Directory {
            return Err(crate::vfs::error::VfsError::NotADirectory);
        }
        let _pos_guard = self.pos_lock.lock();
        let pos = self.pos.load(Ordering::Acquire);
        let new_pos = self.ops.readdir(pos, sink)?;
        self.pos.store(new_pos, Ordering::Release);
        Ok(new_pos)
    }

    /// 修改文件大小，委托给底层文件系统以同步数据容器和 inode 元数据。
    #[kernel_symbols::export(
        name = "vfs.file.File.truncate",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn truncate(&self, size: u64) -> VfsResult<()> {
        if !self.flags().writable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        self.mount.check_writable()?;
        let _data_mutation = self.inode.begin_data_mutation();
        self.inode.ops.truncate(&self.inode, size)?;
        Ok(())
    }

    /// 按指定模式调整文件范围的底层存储。是否支持由具体文件系统决定。
    pub fn fallocate(&self, mode: FallocateMode, offset: u64, len: u64) -> VfsResult<()> {
        if !self.flags().writable() {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        if len == 0 {
            return Err(crate::vfs::error::VfsError::InvalidArgument);
        }
        if offset.checked_add(len).is_none() {
            return Err(crate::vfs::error::VfsError::FileTooLarge);
        }
        if !self.ops.is_seekable() {
            return Err(crate::vfs::error::VfsError::IllegalSeek);
        }
        self.mount.check_writable()?;
        let _data_mutation = self.inode.begin_data_mutation();
        self.ops.fallocate(mode, offset, len)?;
        Ok(())
    }

    /// 将文件操作对象向下转型为具体驱动类型 `T`。
    ///
    /// 这是从通用 [`File`] 恢复 [`crate::dev::char::DriverControl`] 能力的安全路径，
    /// 与 [`crate::dev::char::CharDev::downcast_driver`] 语义完全对称。
    ///
    /// 对于字符设备，先转型为 [`crate::vfs::dev::CharDevAdapter`]，再调用其
    /// [`downcast_driver`](crate::vfs::dev::CharDevAdapter::downcast_driver)：
    ///
    /// ```rust,ignore
    /// if let Some(adapter) = file.downcast_ops::<CharDevAdapter>() {
    ///     if let Some(uart) = adapter.downcast_driver::<Uart16550>() {
    ///         uart.control(UartRequest::SetBaudRate { clock_hz: 100_000_000, baud: 9600 })?;
    ///     }
    /// }
    /// ```
    ///
    /// 通过 `dyn FileOps` 的通用路径不应调用设备特定命令——类型安全由此保证。
    pub fn downcast_ops<T: 'static>(&self) -> Option<&T> {
        self.ops.as_any().downcast_ref::<T>()
    }

    /// 将文件内容刷入底层存储（`fsync`）：等待数据和元数据均落盘。
    #[kernel_symbols::export(
        name = "vfs.file.File.sync",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn sync(&self) -> VfsResult<()> {
        self.check_syncable()?;
        self.ops.sync()?;
        self.inode.ops.sync_metadata(&self.inode)
    }

    /// 仅将数据刷盘，不保证元数据（如 mtime）同步（`fdatasync`）。
    pub fn datasync(&self) -> VfsResult<()> {
        self.check_syncable()?;
        self.ops.sync()
    }

    fn check_syncable(&self) -> VfsResult<()> {
        if self.flags().path_only {
            return Err(crate::vfs::error::VfsError::BadFileDescriptor);
        }
        match self.inode.kind() {
            crate::vfs::stat::FileType::Regular
            | crate::vfs::stat::FileType::Directory
            | crate::vfs::stat::FileType::BlockDevice => Ok(()),
            crate::vfs::stat::FileType::Symlink
            | crate::vfs::stat::FileType::CharDevice
            | crate::vfs::stat::FileType::Fifo
            | crate::vfs::stat::FileType::Socket => {
                Err(crate::vfs::error::VfsError::InvalidArgument)
            }
        }
    }

    /// 获取文件当前元数据快照（`fstat`）。
    #[kernel_symbols::export(
        name = "vfs.file.File.stat",
        contract = "kernel.vfs.file@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_QUERY
    )]
    pub fn stat(&self) -> VfsResult<FileStat> {
        self.inode.stat()
    }

    /// 检查文件当前就绪的 I/O 事件（`poll(2)`/`select(2)`/`epoll` 的核心）。
    ///
    /// `interest` 指定调用方感兴趣的事件掩码；返回值为当前已就绪的事件子集
    /// （`interest` 与实际就绪事件的交集）。若无就绪事件，调用方应将此
    /// 描述符加入内核等待队列（等待队列由调度器层实现，此处不涉及）。
    #[kernel_symbols::export(
        name = "vfs.file.File.poll",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO
    )]
    pub fn poll(&self, interest: PollEvents) -> PollEvents {
        let ready = self.ops.poll(interest);
        // POLLERR/POLLHUP/POLLNVAL 始终返回，不受 interest 过滤（POSIX 语义）。
        let always = PollEvents::POLLERR
            .with(PollEvents::POLLHUP)
            .with(PollEvents::POLLNVAL);
        ready.intersect(interest.with(always))
    }

    pub fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        self.ops.poll_add_waiter(task, interest)
    }

    pub fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.ops.poll_remove_waiter(task)
    }

    /// 判断该打开文件描述是否允许加入 epoll 实例。
    ///
    /// `poll(2)` 会把没有专用等待源的普通文件视为立即可读写，但 Linux 的
    /// `epoll_ctl(2)` 只接纳底层明确提供事件轮询能力的文件。该能力必须由
    /// 具体 `FileOps` 显式声明，不能根据 inode 类型或当前就绪结果推断。
    pub fn is_epollable(&self) -> bool {
        self.ops.is_epollable()
    }

    /// 执行由具体打开文件描述实现的 `fcntl(2)` 命令。
    ///
    /// 通用描述符标志和记录锁仍由系统调用层统一处理；只有必须访问底层对象
    /// 私有状态的命令才下沉到这里，例如 pipe 容量调整。
    pub fn fcntl(&self, cmd: usize, arg: usize, cred: &Credentials) -> Result<usize, Errno> {
        self.ops.fcntl(cmd, arg, cred)
    }

    /// 返回可供事件订阅器直接监听的就绪源。
    ///
    /// 新的 epoll 实现以该对象的代际通知为准；旧式 `poll_add_waiter` 仍保留给
    /// 不具备稳定就绪源的文件类型和普通阻塞 I/O。
    pub fn poll_source(&self) -> Option<&crate::poll_source::PollSource> {
        self.ops.poll_source()
    }

    pub fn on_fd_closed(&self, fd: u32) {
        self.ops.on_fd_closed(fd)
    }

    pub fn on_file_description_closed(&self, file: &Arc<File>) {
        self.ops.on_file_description_closed(file)
    }

    pub fn io_timeout_deadline(&self, interest: PollEvents) -> Option<u64> {
        self.ops.io_timeout_deadline(interest)
    }

    pub fn is_seekable(&self) -> bool {
        self.ops.is_seekable()
    }

    /// 执行设备或文件系统特定的控制命令（`ioctl(2)`）。
    #[kernel_symbols::export(
        name = "vfs.file.File.ioctl",
        contract = "kernel.vfs.file-io@1",
        version = 1,
        capabilities = kernel_symbols::capability::VFS_IO,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if self.flags.path_only {
            return Err(Errno::EBADF);
        }
        self.ops.ioctl(cmd, arg)
    }
}

impl Drop for File {
    /// 析构时自动调用 `FileOps::release`，释放驱动私有的打开状态。
    ///
    /// 这保证了无论通过何种路径（正常 `close`、进程退出、`execve` 的 CLOEXEC）
    /// 关闭描述符，驱动的清理逻辑都会被调用且只调用一次（Arc 的唯一性保证）。
    fn drop(&mut self) {
        // `flock(2)` 的锁属于打开文件描述；最后一个 Arc<File> 消失时自动释放，
        // 避免进程异常退出后留下无法唤醒的 advisory lock。
        crate::vfs::flock::unlock_file_ref(self);
        FILE_DROPPED.fetch_add(1, Ordering::Relaxed);
        FILE_LIVE.fetch_sub(1, Ordering::Relaxed);
        self.ops.release();
        // 自动递减挂载引用计数，保证 is_busy() 正确反映活跃 fd 数量。
        self.mount.dec_open();
    }
}

// ── libs/mm::FileLike 适配 ───────────────────────────────────────────────────
//
// 让 VMA 的 file backing 可以持 `Arc<File>` 作为 `Arc<dyn FileLike>`。方向
// 是 `libs/vfs → libs/mm`（被允许），反向不成立。只实现 loader / demand
// paging 实际用的两个方法；错误按"读失败一律 Errno::EIO"降级——这里没有
// 语义上"短读 / 超时 / 权限拒绝"的细分诉求，缺页处理拿到错误直接 SIGBUS。

impl ::mm::FileLike for File {
    fn cache_key(&self) -> usize {
        Arc::as_ptr(&self.inode) as usize
    }

    fn private_page_cache_key(&self) -> Option<usize> {
        self.inode.private_page_cache_key()
    }

    fn private_page_cache_generation(&self) -> Option<u64> {
        self.inode.private_page_cache_generation()
    }

    fn disable_private_page_cache(&self) {
        self.inode.disable_private_page_cache();
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, errno::Errno> {
        // File::read_at 会检查 readable flag；loader / mmap 场景打开时必然带
        // O_RDONLY，返错说明文件描述符状态异常——映回 EIO。
        File::read_at(self, buf, offset).map_err(|_| errno::Errno::EIO)
    }

    fn read_pages_at(
        &self,
        offset: u64,
        pages: &mut [&mut [u8]],
        valid_len: usize,
    ) -> Result<(), errno::Errno> {
        File::read_pages_at(self, offset, pages, valid_len).map_err(|error| error.to_errno())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, errno::Errno> {
        File::write_at(self, buf, offset).map_err(|_| errno::Errno::EIO)
    }

    fn sync(&self) -> Result<(), errno::Errno> {
        File::sync(self).map_err(|_| errno::Errno::EIO)
    }

    fn size(&self) -> u64 {
        File::stat(self).map(|s| s.size as u64).unwrap_or(0)
    }
}

// ── 文件操作接口 ──────────────────────────────────────────────────────────────

/// 文件系统特定的文件操作接口（对应 Linux `struct file_operations`）。
///
/// 具体文件系统驱动为每种文件类型提供一份实现，由 [`crate::vfs::inode::InodeOps::open`]
/// 创建并注入到 [`File::ops`] 中。
///
/// ### 并发模型
///
/// 所有方法取 `&self`（共享引用），要求驱动内部通过 `Spinlock` 或其他内部可变
/// 性原语保护可变状态。这与 `Sync` 约束配合，允许多线程共享同一 `File`。
pub trait FileOps {
    /// 在指定 `offset` 处读取最多 `buf.len()` 字节，返回实际读取字节数。
    ///
    /// 返回 `Ok(0)` 表示到达文件末尾（EOF）。不应修改文件的当前偏移量，
    /// 偏移量的推进由 [`File::read`] 统一处理。
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize>;

    /// 从 `offset` 起精确填充页面前 `valid_len` 字节，并把剩余尾部清零。
    ///
    /// 默认路径循环调用 [`Self::read_at`]；文件系统可覆盖它以直接填充调用方页。
    fn read_pages_at(
        &self,
        offset: u64,
        pages: &mut [&mut [u8]],
        valid_len: usize,
    ) -> VfsResult<()> {
        read_pages_at_default(self, offset, pages, valid_len)
    }

    /// 在指定 `offset` 处写入 `buf`，返回实际写入字节数。
    ///
    /// ### 特殊值 `offset = u64::MAX`（O_APPEND 语义）
    ///
    /// 当 `offset` 为 `u64::MAX` 时，驱动必须**原子地**将数据追加到文件末尾：
    /// 即在持有 inode 写保护（如 `inode.meta` 锁或驱动内部锁）的情况下，
    /// 同时完成"读取当前 EOF → 在 EOF 处写入"这两步，消除 TOCTOU 竞争。
    ///
    /// VFS 层在 O_APPEND 写入时传入此值，而不是在 VFS 层预读 `inode.meta.size`
    /// 再传给驱动——那样做会因锁被提前释放而引入竞争（两次写入可能获得相同的
    /// EOF 偏移量，导致数据被覆盖而非追加）。
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize>;

    /// 枚举从 `pos` 起的目录条目，逐条传给 `sink`（用于 `getdents`）。
    ///
    /// 对非目录文件返回 `VfsError::NotADirectory`。`pos` 是由 [`File::readdir`] 传入
    /// 的目录游标，驱动应从该位置起枚举条目：
    /// - `sink` 返回 `ControlFlow::Continue(())` 时继续；
    /// - `sink` 返回 `ControlFlow::Break(())` 时停止（通常表示目标缓冲区已满）。
    ///
    /// 返回枚举结束后的新游标值（`Continue` 路径枚举完所有条目时为最终位置；
    /// `Break` 路径为最后一条**未**被 sink 消费的条目对应的游标）。
    /// 调用方（[`File::readdir`]）负责将此值写回 `File::pos`。
    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64>;

    /// 将该文件的脏缓冲区刷入存储。
    fn sync(&self) -> VfsResult<()>;

    /// 检查当前就绪的 I/O 事件（`poll(2)`/`epoll` 的底层接口）。
    ///
    /// 返回当前已就绪的事件掩码。驱动应结合内部状态（接收缓冲区是否有数据、
    /// 发送缓冲区是否有空间等）决定返回哪些事件。若无就绪事件，返回空掩码。
    fn poll(&self, interest: PollEvents) -> PollEvents;

    /// 将一个任务登记为当前文件的 I/O 等待者。
    ///
    /// 返回 `true` 表示实现方已经把该任务挂到了自身等待源上，调用方随后可以睡眠；
    /// 返回 `false` 表示该文件类型没有专门的唤醒机制，调用方需要自行退化处理。
    fn poll_add_waiter(&self, _task: &Arc<Task>, _interest: PollEvents) -> bool {
        false
    }

    /// 显式移除之前登记的等待者。
    fn poll_remove_waiter(&self, _task: &Arc<Task>) {}

    /// 是否允许该打开文件描述加入 epoll。
    ///
    /// 默认关闭，避免普通文件、目录以及仅为兼容 `poll(2)` 返回立即就绪的
    /// 对象被误接纳。真正的事件源应在实现中显式返回 `true`。
    fn is_epollable(&self) -> bool {
        false
    }

    /// 处理文件类型私有的 `fcntl(2)` 命令。
    fn fcntl(&self, _cmd: usize, _arg: usize, _cred: &Credentials) -> Result<usize, Errno> {
        Err(Errno::EINVAL)
    }

    /// 返回稳定的就绪状态发布源；默认文件类型不提供事件订阅能力。
    fn poll_source(&self) -> Option<&crate::poll_source::PollSource> {
        None
    }

    /// 动态状态位（`F_SETFL`）发生变化时通知底层驱动。
    fn set_status_flags(&self, _flags: OpenOptions) {}

    /// 某个 fd 号从 fdtable 中关闭或被替换时调用。
    ///
    /// 这是描述符级通知，不等同于 [`FileOps::release`]；同一个 `File` 可能仍被
    /// 其他 dup 出来的 fd 或 epoll/SCM_RIGHTS 持有。
    fn on_fd_closed(&self, _fd: u32) {}

    /// fdtable 中某个打开文件描述的最后一个 fd 被关闭或替换时调用。
    fn on_file_description_closed(&self, _file: &Arc<File>) {}

    /// 返回普通 read/write 等待对应的超时 deadline。
    ///
    /// `poll(2)`/`epoll_wait(2)` 的显式 timeout 不走这里；这个 hook 只服务于
    /// socket `SO_RCVTIMEO`/`SO_SNDTIMEO` 这类描述符自身的阻塞 I/O 超时。
    fn io_timeout_deadline(&self, _interest: PollEvents) -> Option<u64> {
        None
    }

    /// 是否支持 `lseek(2)`。
    fn is_seekable(&self) -> bool {
        true
    }

    /// 查找指定偏移之后的第一个已分配数据字节。
    ///
    /// 不具备稀疏区间信息的文件类型默认不声明支持，避免把设备、目录或匿名对象
    /// 错误地解释为普通稠密文件。
    fn seek_data(&self, _offset: u64, _file_size: u64) -> VfsResult<u64> {
        Err(crate::vfs::error::VfsError::NotSupported)
    }

    /// 查找指定偏移之后的第一个空洞字节或逻辑文件末尾。
    fn seek_hole(&self, _offset: u64, _file_size: u64) -> VfsResult<u64> {
        Err(crate::vfs::error::VfsError::NotSupported)
    }

    /// 按指定模式调整文件范围的底层存储。默认表示该文件类型不支持该操作。
    fn fallocate(&self, _mode: FallocateMode, _offset: u64, _len: u64) -> VfsResult<()> {
        Err(crate::vfs::error::VfsError::NotSupported)
    }

    /// 执行文件或设备控制命令。
    ///
    /// 默认返回 `ENOTTY`，表示该文件类型没有可处理的 ioctl。具体设备适配层可在
    /// 这里把 Linux ioctl ABI 翻译成自身的 typed control / metadata 操作。
    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    /// 文件描述符最后一次引用消失时调用，由 [`File`] 的 `Drop` 自动触发。
    ///
    /// 驱动应在此释放所有打开状态（如引用计数递减、缓冲区释放等）。
    ///
    /// 与 `Inode::evict` 的区别：`release` 在每次最终 `close`（Arc 计数归零）
    /// 时调用；`evict` 只在 inode 引用计数降至零时调用一次。
    fn release(&self);

    /// 返回 `self` 的 `&dyn Any` 引用，用于向下转型到具体驱动类型。
    ///
    /// 这是 [`crate::dev::char::DriverControl`] 在通用路径上的"类型恢复桥梁"，
    /// 与 [`crate::dev::char::CharDriver::as_any`] 语义完全对称。
    ///
    /// 实现者只需写 `fn as_any(&self) -> &dyn Any { self }`。
    fn as_any(&self) -> &dyn core::any::Any;
}

/// [`FileOps::read_pages_at`] 的通用精确读取实现。
///
/// 单独暴露此函数，便于文件系统在无法使用自身对齐快速路径时回退，同时避免
/// 通过 trait 默认方法形成递归调用。
pub fn read_pages_at_default<T: FileOps + ?Sized>(
    ops: &T,
    offset: u64,
    pages: &mut [&mut [u8]],
    valid_len: usize,
) -> VfsResult<()> {
    let mut capacity = 0usize;
    for page in pages.iter() {
        if page.is_empty() {
            return Err(crate::vfs::error::VfsError::InvalidArgument);
        }
        capacity = capacity
            .checked_add(page.len())
            .ok_or(crate::vfs::error::VfsError::FileTooLarge)?;
    }
    if valid_len > capacity {
        return Err(crate::vfs::error::VfsError::InvalidArgument);
    }
    let valid_len_u64 =
        u64::try_from(valid_len).map_err(|_| crate::vfs::error::VfsError::FileTooLarge)?;
    offset
        .checked_add(valid_len_u64)
        .ok_or(crate::vfs::error::VfsError::FileTooLarge)?;

    let mut page_start = 0usize;
    for page in pages.iter_mut() {
        let tail_start = valid_len.saturating_sub(page_start).min(page.len());
        page[tail_start..].fill(0);
        page_start += page.len();
    }

    let mut remaining = valid_len;
    let mut read_offset = offset;
    for page in pages.iter_mut() {
        let page_valid = remaining.min(page.len());
        let mut done = 0usize;
        while done < page_valid {
            let count = ops.read_at(&mut page[done..page_valid], read_offset)?;
            if count == 0 || count > page_valid - done {
                return Err(crate::vfs::error::VfsError::Io);
            }
            done += count;
            read_offset += count as u64;
        }
        remaining -= page_valid;
        if remaining == 0 {
            break;
        }
    }
    Ok(())
}
