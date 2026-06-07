//! VFS 统一错误类型。
//!
//! [`VfsError`] 是整个 VFS 层对外暴露的唯一错误枚举。它在语义层面描述出错原因，
//! 并可以无损地转换为 [`crate::vfs::errno::Errno`] 供系统调用返回给用户空间。
//!
//! 设计原则：
//! - 每个变体对应一个明确的、可独立处理的故障场景，不使用模糊的 "Other" 兜底；
//! - 变体名称与 POSIX errno 语义对齐，方便移植现有用户程序；
//! - 携带必要的上下文信息（如超出限制时的实际值与上限），减少调试时的信息丢失。

use errno::Errno;

/// VFS 操作的统一错误类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    // ── 路径与命名 ────────────────────────────────────────────────────────────
    /// 路径中的某个分量不存在（ENOENT）。
    ///
    /// 区别于 [`VfsError::NotADirectory`]：此处目标本身不存在，而不是类型不符。
    NotFound,

    /// 路径中某个中间分量不是目录（ENOTDIR）。
    ///
    /// 例如对 "/etc/passwd/foo" 进行 lookup，"passwd" 是普通文件而非目录。
    NotADirectory,

    /// 操作目标是目录，但该操作要求非目录文件（EISDIR）。
    ///
    /// 例如对目录调用 `open(O_WRONLY)`。
    IsADirectory,

    /// 路径名超过系统允许的最大长度（ENAMETOOLONG）。
    NameTooLong,

    /// 符号链接解析深度超过上限（ELOOP）。
    ///
    /// `depth` 是实际解析深度，`limit` 是系统允许的最大值（通常为 40）。
    SymlinkLoop { depth: usize, limit: usize },

    /// 目录非空，无法删除（ENOTEMPTY）。
    DirectoryNotEmpty,

    /// 文件已存在，但操作要求目标不存在（EEXIST）。
    AlreadyExists,

    /// 源与目标不在同一文件系统，无法完成操作（EXDEV）。
    ///
    /// `rename(2)` 和 `link(2)` 跨文件系统时返回此错误；调用方应改用
    /// 复制后删除（copy-then-unlink）的方式。
    CrossDevice,

    // ── 权限与访问控制 ────────────────────────────────────────────────────────
    /// DAC 权限不足：进程对文件的读/写/执行权限不满足（EACCES）。
    ///
    /// 与 [`OperationNotPermitted`](Self::OperationNotPermitted) 的区别：
    /// `PermissionDenied` 是文件权限位检查失败（可通过 chmod 修复），
    /// `OperationNotPermitted` 是操作本身不允许（需要特定 capability）。
    PermissionDenied,

    /// 操作不允许：进程缺少执行该操作所需的 capability（EPERM）。
    ///
    /// 例如：非特权进程尝试 `chown`、`mount`、`mknod` 等需要特定能力的操作。
    OperationNotPermitted,

    /// 文件系统以只读方式挂载，写操作被拒绝（EROFS）。
    ReadOnlyFilesystem,

    /// 对文件描述符执行的操作与其打开标志不兼容（EBADF）。
    ///
    /// 例如对只读描述符调用 `write`，或对非目录描述符调用 `readdir`。
    BadFileDescriptor,

    // ── 资源限制 ──────────────────────────────────────────────────────────────
    /// 内存不足，无法完成操作（ENOMEM）。
    OutOfMemory,

    /// 存储设备空间不足（ENOSPC）。
    NoSpace,

    /// 操作超出文件大小上限（EFBIG）。
    FileTooLarge,

    /// 进程打开的文件描述符数量达到每进程上限（EMFILE）。
    TooManyOpenFiles,

    /// 系统级文件描述符数量达到全局上限（ENFILE）。
    TooManyOpenFilesSystem,

    /// 硬链接数超过文件系统允许的最大值（EMLINK）。
    TooManyLinks,

    // ── I/O 与设备 ───────────────────────────────────────────────────────────
    /// 底层 I/O 错误（EIO）。
    ///
    /// 通常由块设备驱动上报，表示硬件级别的读写失败。
    Io,

    /// 设备不存在或驱动未就绪（ENODEV）。
    NoDevice,

    /// 设备忙，操作无法立即完成（EBUSY）。
    DeviceBusy,

    // ── 操作语义 ──────────────────────────────────────────────────────────────
    /// 传入的参数无效（EINVAL）。
    ///
    /// 包括：非法的 flags 组合、不合理的 offset/count 值、无效的 ioctl 命令等。
    InvalidArgument,

    /// 当前文件系统或驱动不支持该操作（ENOSYS / EOPNOTSUPP）。
    NotSupported,

    /// 文件类型不支持按偏移定位的 I/O（ESPIPE）。
    IllegalSeek,

    /// 操作被信号中断（EINTR）。
    ///
    /// 调用方应当根据 `SA_RESTART` 语义决定是否重试。
    Interrupted,

    /// 文件描述符或资源暂时不可用，可稍后重试（EAGAIN）。
    WouldBlock,

    /// 操作超时（ETIMEDOUT）。
    TimedOut,

    /// 管道写端已关闭，读端返回 EOF；或读端已关闭，写端收到 SIGPIPE（EPIPE）。
    BrokenPipe,

    /// 连接被对端重置（ECONNRESET）。
    ConnectionReset,
}

impl VfsError {
    /// 将 [`VfsError`] 转换为对应的 POSIX errno 值。
    pub fn to_errno(self) -> Errno {
        match self {
            VfsError::NotFound => Errno::ENOENT,
            VfsError::NotADirectory => Errno::ENOTDIR,
            VfsError::IsADirectory => Errno::EISDIR,
            VfsError::NameTooLong => Errno::ENAMETOOLONG,
            VfsError::SymlinkLoop { .. } => Errno::ELOOP,
            VfsError::DirectoryNotEmpty => Errno::ENOTEMPTY,
            VfsError::AlreadyExists => Errno::EEXIST,
            VfsError::CrossDevice => Errno::EXDEV,
            VfsError::PermissionDenied => Errno::EACCES,
            VfsError::OperationNotPermitted => Errno::EPERM,
            VfsError::ReadOnlyFilesystem => Errno::EROFS,
            VfsError::BadFileDescriptor => Errno::EBADF,
            VfsError::OutOfMemory => Errno::ENOMEM,
            VfsError::NoSpace => Errno::ENOSPC,
            VfsError::FileTooLarge => Errno::EFBIG,
            VfsError::TooManyOpenFiles => Errno::EMFILE,
            VfsError::TooManyOpenFilesSystem => Errno::ENFILE,
            VfsError::TooManyLinks => Errno::EMLINK,
            VfsError::Io => Errno::EIO,
            VfsError::NoDevice => Errno::ENODEV,
            VfsError::DeviceBusy => Errno::EBUSY,
            VfsError::InvalidArgument => Errno::EINVAL,
            VfsError::NotSupported => Errno::EOPNOTSUPP,
            VfsError::IllegalSeek => Errno::ESPIPE,
            VfsError::Interrupted => Errno::EINTR,
            VfsError::WouldBlock => Errno::EAGAIN,
            VfsError::TimedOut => Errno::ETIMEDOUT,
            VfsError::BrokenPipe => Errno::EPIPE,
            VfsError::ConnectionReset => Errno::ECONNRESET,
        }
    }
}

/// VFS 操作的通用 Result 类型别名。
pub type VfsResult<T> = Result<T, VfsError>;
