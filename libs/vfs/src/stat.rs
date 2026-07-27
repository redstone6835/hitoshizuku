//! 文件元数据类型。
//!
//! 本模块定义了描述文件系统对象属性的所有基础类型，包括：
//! - [`FileType`]：文件种类（普通文件、目录、符号链接等）；
//! - [`FileMode`]：Unix 权限位（rwxrwxrwx + setuid/setgid/sticky）；
//! - [`Timespec`]：纳秒精度的时间戳；
//! - [`DevId`]：设备号（主设备号 + 次设备号）；
//! - [`FileStat`]：`stat(2)` 系统调用的返回结构。
//!
//! 这些类型是 VFS 层与具体文件系统驱动之间的契约，也是用户空间 `stat`/`fstat`
//! 系统调用的数据来源，因此其字段布局和语义必须与 POSIX 规范严格对齐。

use core::sync::atomic::{AtomicUsize, Ordering};

static REALTIME_CLOCK: AtomicUsize = AtomicUsize::new(0);

/// 安装 VFS 元数据使用的 Unix realtime 时钟。
pub fn install_realtime_clock(clock: fn() -> u64) {
    REALTIME_CLOCK.store(clock as usize, Ordering::Release);
}

/// 文件类型，对应 `stat.st_mode` 的高 4 位（`S_IFMT` 掩码部分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// 普通文件（regular file），包含任意字节序列。
    Regular,
    /// 目录，包含若干目录项（dentry）。
    Directory,
    /// 符号链接，存储目标路径字符串。
    Symlink,
    /// 字符设备，按字节流访问硬件（如 UART、tty）。
    CharDevice,
    /// 块设备，按块随机访问存储媒介（如磁盘、SD 卡）。
    BlockDevice,
    /// 命名管道（FIFO），用于进程间单向字节流通信。
    Fifo,
    /// Unix 域套接字，用于同机进程间双向通信。
    Socket,
}

impl FileType {
    /// 返回该类型对应的 `st_mode` 高位编码（已移至正确位置，可直接与权限位 OR）。
    pub fn to_mode_bits(self) -> u32 {
        match self {
            FileType::Regular => 0o100000,
            FileType::Directory => 0o040000,
            FileType::Symlink => 0o120000,
            FileType::CharDevice => 0o020000,
            FileType::BlockDevice => 0o060000,
            FileType::Fifo => 0o010000,
            FileType::Socket => 0o140000,
        }
    }
}

/// Unix 权限位，封装 `st_mode` 的低 12 位。
///
/// 布局（低到高）：
/// - [2:0]  other：其他用户的 r/w/x；
/// - [5:3]  group：所属组的 r/w/x；
/// - [8:6]  owner：所有者的 r/w/x；
/// - [9]    sticky bit（目录时限制删除，可执行文件时已废弃）；
/// - [10]   setgid（执行时以文件所属组身份运行）；
/// - [11]   setuid（执行时以文件所有者身份运行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileMode(pub(crate) u16);

impl FileMode {
    // ── 基本权限位常量 ──────────────────────────────────────────────────────
    /// 所有者读权限。
    pub const IRUSR: Self = Self(0o400);
    /// 所有者写权限。
    pub const IWUSR: Self = Self(0o200);
    /// 所有者执行/搜索权限。
    pub const IXUSR: Self = Self(0o100);
    /// 所属组读权限。
    pub const IRGRP: Self = Self(0o040);
    /// 所属组写权限。
    pub const IWGRP: Self = Self(0o020);
    /// 所属组执行/搜索权限。
    pub const IXGRP: Self = Self(0o010);
    /// 其他用户读权限。
    pub const IROTH: Self = Self(0o004);
    /// 其他用户写权限。
    pub const IWOTH: Self = Self(0o002);
    /// 其他用户执行/搜索权限。
    pub const IXOTH: Self = Self(0o001);
    /// setuid 位。
    pub const ISUID: Self = Self(0o4000);
    /// setgid 位。
    pub const ISGID: Self = Self(0o2000);
    /// sticky 位。
    pub const ISVTX: Self = Self(0o1000);

    // ── 组合常量 ────────────────────────────────────────────────────────────
    /// setuid + setgid（创建文件时根据 CAP_FSETID 清除）。
    pub const SUID_SGID: Self = Self(0o6000);
    /// 低 9 位权限掩码（rwxrwxrwx），用于 umask 运算。
    pub const PERM_MASK: Self = Self(0o777);
    /// 任意执行位（owner|group|other 的 x 位之一），用于 DAC_OVERRIDE 检查。
    pub const ANY_EXEC: Self = Self(0o111);

    /// 构造一个新的 `FileMode`。
    pub const fn new(bits: u16) -> Self {
        Self(bits)
    }

    /// 返回原始权限位。
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// 返回原始权限位（与标志位类型 API 统一）。
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// 判断是否包含指定权限位。
    pub const fn has(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    /// 判断是否包含指定权限位中的任意一位。
    pub const fn has_any(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// 添加指定权限位。
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }

    /// 移除指定权限位。
    pub const fn without(self, flag: Self) -> Self {
        Self(self.0 & !flag.0)
    }

    /// 按位与掩码（保留 self 中 mask 也有的位），与 CapSet::mask 对齐。
    pub const fn mask(self, m: Self) -> Self {
        Self(self.0 & m.0)
    }

    /// 判断权限位是否为空。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    // ── 便捷权限查询 ────────────────────────────────────────────────────────
    /// 判断所有者是否有读权限。
    pub const fn owner_read(self) -> bool {
        self.has(Self::IRUSR)
    }
    /// 判断所有者是否有写权限。
    pub const fn owner_write(self) -> bool {
        self.has(Self::IWUSR)
    }
    /// 判断所有者是否有执行权限。
    pub const fn owner_exec(self) -> bool {
        self.has(Self::IXUSR)
    }
    /// 判断所属组是否有读权限。
    pub const fn group_read(self) -> bool {
        self.has(Self::IRGRP)
    }
    /// 判断所属组是否有写权限。
    pub const fn group_write(self) -> bool {
        self.has(Self::IWGRP)
    }
    /// 判断所属组是否有执行权限。
    pub const fn group_exec(self) -> bool {
        self.has(Self::IXGRP)
    }
    /// 判断其他用户是否有读权限。
    pub const fn other_read(self) -> bool {
        self.has(Self::IROTH)
    }
    /// 判断其他用户是否有写权限。
    pub const fn other_write(self) -> bool {
        self.has(Self::IWOTH)
    }
    /// 判断其他用户是否有执行权限。
    pub const fn other_exec(self) -> bool {
        self.has(Self::IXOTH)
    }
    /// 判断是否设置了 setuid 位。
    pub const fn setuid(self) -> bool {
        self.has(Self::ISUID)
    }
    /// 判断是否设置了 setgid 位。
    pub const fn setgid(self) -> bool {
        self.has(Self::ISGID)
    }
    /// 判断是否设置了 sticky 位。
    pub const fn sticky(self) -> bool {
        self.has(Self::ISVTX)
    }
}

/// 纳秒精度时间戳，与 POSIX `struct timespec` 对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timespec {
    /// 自 Unix 纪元（1970-01-01T00:00:00Z）起的秒数。
    pub secs: i64,
    /// 纳秒分量，取值范围 [0, 999_999_999]。
    pub nsecs: u32,
}

impl Timespec {
    /// 零时间戳（Unix 纪元）。
    pub const ZERO: Self = Self { secs: 0, nsecs: 0 };

    /// 从纳秒值构造时间戳。
    pub const fn from_nanos(nanos: u64) -> Self {
        Self {
            secs: (nanos / 1_000_000_000) as i64,
            nsecs: (nanos % 1_000_000_000) as u32,
        }
    }

    /// 获取当前 Unix realtime；平台尚未安装时钟时退回零时间戳。
    pub fn now() -> Self {
        let raw = REALTIME_CLOCK.load(Ordering::Acquire);
        if raw == 0 {
            return Self::ZERO;
        }
        // Safety: install_realtime_clock 只会存入签名为 fn() -> u64 的函数指针。
        let clock = unsafe { core::mem::transmute::<usize, fn() -> u64>(raw) };
        Self::from_nanos(clock())
    }
}

/// 文件系统实例标识符，用于区分不同的已挂载文件系统（跨 FS 操作检查）。
///
/// 与 [`DevId`] 不同：`FsId` 是纯内核内部概念，唯一标识一次挂载产生的
/// 文件系统实例；`DevId` 是设备号，面向用户空间 `stat(2)` 的 `st_dev` 字段。
/// 两者可能相同（块设备 FS 通常将设备号作为 FS ID），也可能不同（内存 FS
/// 使用分配的唯一 u64；同一设备在不同命名空间多次挂载时 FS ID 各不相同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FsId(pub u64);

impl FsId {
    /// 构造文件系统实例标识符。
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
    /// 返回原始 u64 值。
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// 设备号，封装 Linux `dev_t`（64 位）的主次设备号。
///
/// Linux 内核使用 `MKDEV(major, minor)` 宏将主次设备号打包为单个 `dev_t`。
/// 这里分开存储，避免主次设备号位域约定的歧义（不同内核版本有所不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct DevId {
    /// 主设备号（major）：标识驱动程序或设备类型。
    pub major: u32,
    /// 次设备号（minor）：在同一驱动下区分具体设备实例。
    pub minor: u32,
}

impl DevId {
    /// 构造设备号。
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

/// `stat(2)` / `fstat(2)` 系统调用返回的文件元数据结构。
///
/// 字段与 POSIX `struct stat` 严格对应，供系统调用层直接拷贝到用户空间。
#[derive(Debug, Clone, Copy)]
pub struct FileStat {
    /// 文件所在设备的设备号（`st_dev`）。
    pub dev: DevId,
    /// 文件在文件系统内的唯一 inode 编号（`st_ino`）。
    pub ino: u64,
    /// 文件种类与权限位的组合（`st_mode`），由 [`FileType::to_mode_bits`] 与
    /// [`FileMode::bits`] 按位 OR 得到。
    pub mode: u32,
    /// 硬链接数（`st_nlink`）。
    pub nlink: u32,
    /// 所有者用户 ID（`st_uid`）。
    pub uid: u32,
    /// 所属组 ID（`st_gid`）。
    pub gid: u32,
    /// 对于设备文件，记录其设备号（`st_rdev`）；其他文件类型此字段为 0。
    pub rdev: DevId,
    /// 文件字节大小（`st_size`）；对目录，含义由具体文件系统决定。
    pub size: i64,
    /// 最优 I/O 块大小（`st_blksize`），通常与文件系统块大小一致。
    pub blksize: u32,
    /// 已分配的 512 字节块数（`st_blocks`），包含元数据块但不含空洞。
    pub blocks: u64,
    /// 最后访问时间（`st_atim`）。
    pub atime: Timespec,
    /// 最后内容修改时间（`st_mtim`）。
    pub mtime: Timespec,
    /// 最后元数据变更时间（`st_ctim`）；注意不是创建时间。
    pub ctime: Timespec,
}

/// 文件系统全局统计信息，对应 `statfs(2)` 的返回值。
#[derive(Debug, Clone, Copy)]
pub struct FsStat {
    /// 文件系统类型的魔数（如 ext4 = `0xEF53`，tmpfs = `0x01021994`）。
    pub fs_type: u64,
    /// 文件系统基本块大小（字节）。
    pub block_size: u64,
    /// 总块数。
    pub total_blocks: u64,
    /// 空闲块数（包含为 root 保留的块）。
    pub free_blocks: u64,
    /// 非特权用户可用的空闲块数。
    pub avail_blocks: u64,
    /// 总 inode 数（0 表示文件系统不限制 inode 数量）。
    pub total_inodes: u64,
    /// 空闲 inode 数。
    pub free_inodes: u64,
    /// 文件系统 ID（通常为设备号或随机值）。
    pub fs_id: u64,
    /// 文件名最大长度（字节，不含 NUL 终止符）。
    ///
    /// 对应 POSIX `NAME_MAX`（如 ext4 为 255）。注意：`pathconf(_PC_NAME_MAX)`
    /// 返回的也是不含 NUL 的长度。
    pub name_max: u32,
}
