//! 进程凭据（Credentials）与能力（Capability）。
//!
//! VFS 层的所有权限检查都以 [`Credentials`] 为输入，而不是读取全局的"当前进程"
//! 状态。这样做有两个好处：
//!
//! 1. **可测试性**：可以在不切换任务上下文的情况下，以任意凭据调用 VFS 函数
//!    进行单元测试；
//! 2. **安全性**：凭据是值类型，持有者无法悄悄修改已传入函数的凭据；任何特权
//!    提升都必须通过显式的 API 完成，而不能隐式发生。
//!
//! ### 能力模型
//!
//! 除 Unix DAC（Discretionary Access Control，基于 uid/gid 的自主访问控制）之外，
//! 本 VFS 层还支持 Linux 兼容的能力位集（`CapSet`）。能力比 root 判断更精细：
//! 例如，`CAP_DAC_OVERRIDE` 只绕过文件权限检查，而不赋予进程任意系统调用权限。
//! 这使得将来实现沙箱（只给进程少量能力）成为可能。
//!
//! ### root 与能力的关系
//!
//! 本实现采用纯能力模型：`has_cap` 只检查 `caps` 位集，**不**对 `euid == 0` 做
//! 特判。root 身份由 [`Credentials::root()`] 构造时将 `caps` 设为 [`CapSet::FULL`]
//! 来体现，而不是在运行时隐式地将 euid=0 等同于全能力。这样保证：
//! - 可以创建"低权限 root"（euid=0 但 caps 受限）；
//! - `has_cap` 的语义简单明确，无隐式规则。
use alloc::vec::Vec;

/// 用户 ID，对应 POSIX `uid_t`（32 位无符号整数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Uid(pub u32);

/// 组 ID，对应 POSIX `gid_t`（32 位无符号整数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gid(pub u32);

impl Uid {
    /// 超级用户（root）的 UID。
    pub const ROOT: Self = Self(0);

    /// 判断是否为 root。
    pub const fn is_root(self) -> bool {
        self.0 == 0
    }
}

impl Gid {
    /// 超级用户（root）的 GID。
    pub const ROOT: Self = Self(0);
}

// ── 能力定义 ──────────────────────────────────────────────────────────────────

/// VFS 层认可的进程能力枚举。
///
/// 此枚举与 Linux `<linux/capability.h>` 中的 `CAP_*` **完全解耦**：
/// 内部位编号（[`Capability::bit`]）按顺序分配（0、1、2…），与 Linux 数值无关。
/// Linux ABI 到此枚举的映射应在 `arch/` 层完成，VFS 内部只使用此枚举。
///
/// ### 扩展原则
///
/// 新增能力时追加变体并在 `bit()` 中分配下一个空闲位，**不要**为了与 Linux
/// 编号对齐而重排已有变体——重排会悄悄改变 `CapSet` 的持久化含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Capability {
    /// 修改文件所有者（`chown`）。
    Chown,
    /// 绕过对文件的 DAC 读/写/执行权限检查（不绕过 MAC）。
    DacOverride,
    /// 绕过对文件/目录的读取和搜索权限限制（可读任意文件）。
    /// 同时允许向任意文件建立硬链接（`fs.protected_hardlinks` 规则）。
    DacReadSearch,
    /// 绕过对文件所有者的检查（可 chmod/chown 任意文件）。
    FOwner,
    /// 在 `chmod`/`chown` 时保留 setuid/setgid 位；设置任意文件的 GID。
    FSetId,
    /// 挂载/卸载文件系统及其他系统管理操作。
    SysAdmin,
    /// 调整进程或系统资源上限，例如把 pipe 容量提升到非特权上限以上。
    SysResource,
    /// 创建特殊文件（`mknod`：块/字符设备节点）。
    MkNod,
    /// 将 Internet socket 绑定到 1--1023 的特权端口。
    NetBindService,
    /// SysV IPC 对象（消息队列/semaphore/shared memory）的权限绕过。
    IpcOwner,
    /// 锁定内存（`mlock`/`shmctl(SHM_LOCK)`）并突破 `RLIMIT_MEMLOCK`。
    IpcLock,
    /// 任意 `ptrace`（进程跟踪与内存/寄存器读写）。
    SysPtrace,
    /// 检查点/恢复类诊断接口（如 `msgrcv(MSG_COPY)`）。
    CheckpointRestore,
    /// 创建原始/数据包套接字（AF_PACKET/SOCK_RAW）并绑定到任意接口（SO_BINDTODEVICE）。
    NetRaw,
    /// 网络管理操作（SO_MARK、SO_PRIORITY > 6 等需要管理员权限的套接字选项）。
    NetAdmin,
}

impl Capability {
    /// 返回该能力在 [`CapSet`] 中的位掩码。
    ///
    /// 位编号按变体声明顺序分配，与 Linux `CAP_*` 数值无关。
    const fn bit(self) -> u64 {
        match self {
            Self::Chown => 1 << 0,
            Self::DacOverride => 1 << 1,
            Self::DacReadSearch => 1 << 2,
            Self::FOwner => 1 << 3,
            Self::FSetId => 1 << 4,
            Self::SysAdmin => 1 << 5,
            Self::SysResource => 1 << 6,
            Self::MkNod => 1 << 7,
            Self::NetBindService => 1 << 8,
            Self::IpcOwner => 1 << 9,
            Self::IpcLock => 1 << 10,
            Self::SysPtrace => 1 << 11,
            Self::CheckpointRestore => 1 << 12,
            Self::NetRaw => 1 << 13,
            Self::NetAdmin => 1 << 14,
        }
    }
}

/// 能力位集，用一个 64 位整数表示最多 64 个独立能力标志。
///
/// Linux 实际使用三套 32 位字（permitted/inheritable/effective），这里简化为
/// 单一的 64 位有效能力集，足够内核内部权限检查使用。
///
/// ### 位布局
///
/// 第 `n` 位对应 [`Capability::bit()`] 返回的掩码；位编号顺序由 VFS 内部决定，
/// 与 Linux `CAP_*` 数值解耦（转换在 `arch/` 层完成）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapSet(pub(crate) u64);

impl CapSet {
    /// 空能力集（无任何特权）。
    pub const EMPTY: Self = Self(0);

    /// 全能力集（拥有所有特权），用于 root 凭据构造。
    pub const FULL: Self = Self(u64::MAX);

    /// 构造只包含单个能力的能力集。
    pub const fn single(cap: Capability) -> Self {
        Self(cap.bit())
    }

    /// 判断是否拥有指定能力。
    pub const fn has(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    /// 向当前集合添加指定能力。
    pub const fn with(self, cap: Capability) -> Self {
        Self(self.0 | cap.bit())
    }

    /// 从当前集合中移除指定能力（能力降级）。
    pub const fn without(self, cap: Capability) -> Self {
        Self(self.0 & !cap.bit())
    }

    /// 将两个能力集合并（求并集）。
    pub const fn merge(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 取两个能力集的交集。
    pub const fn mask(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// 判断能力集是否为空（无任何特权）。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 返回原始位字，仅供 ABI 序列化边界使用（如 `arch/` 层的 syscall 编码）。
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// 从原始位字构造能力集，仅供 ABI 反序列化边界使用（如 `arch/` 层的 syscall 解码）。
    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }
}

/// 进程凭据，描述进程的身份与特权。
///
/// 在 Linux 内核中，每个任务结构（`task_struct`）持有一个 `struct cred` 指针，
/// 凭据在 `execve`/`setuid` 等调用时被原子替换。这里的 [`Credentials`] 是对应的
/// Rust 表示，被 [`crate::vfs::VfsContext`] 以 `Arc<Credentials>` 持有，允许
/// 多线程（同一进程的多个线程）共享同一份凭据，且 `setuid` 后替换 Arc 即可对
/// 所有共享线程立即生效。
///
/// ### 多组支持
///
/// POSIX 允许进程同时属于多个附加组（`getgroups(2)`）。这里用 `Vec<Gid>` 存储
/// 附加组列表，权限检查时会依次匹配。
#[derive(Debug, Clone)]
pub struct Credentials {
    /// 真实用户 ID（real UID）：进程的"真正"所有者，`kill` 信号检查使用此值。
    pub uid: Uid,
    /// 有效用户 ID（effective UID）：用于大多数权限检查，`setuid` 程序执行时
    /// 会将此值切换为文件所有者的 UID。
    pub euid: Uid,
    /// 保存的 set-user-ID（saved UID）：供 `seteuid` 临时放弃再恢复特权使用。
    pub suid: Uid,
    /// 文件系统 UID：VFS DAC 和所有者判断使用它，而不是 effective UID。
    pub fsuid: Uid,
    /// 真实组 ID。
    pub gid: Gid,
    /// 有效组 ID：用于文件所属组的权限检查。
    pub egid: Gid,
    /// 保存的 set-group-ID。
    pub sgid: Gid,
    /// 文件系统 GID：VFS DAC 的 group 档位判断使用它，而不是 effective GID。
    pub fsgid: Gid,
    /// 附加组列表（supplementary groups）。
    pub groups: Vec<Gid>,
    /// 进程当前的有效能力位集。
    ///
    /// 权限检查统一通过此位集判断，不对 `euid == 0` 做隐式特判。
    /// root 凭据由 [`Credentials::root()`] 在构造时设置 `caps = CapSet::FULL`。
    pub caps: CapSet,
}

/// DAC 权限检查类型。
#[derive(Clone, Copy)]
enum PermissionKind {
    Read,
    Write,
    Exec { is_dir: bool },
}

impl Credentials {
    /// 构造一个完全无特权的凭据（所有 uid/gid 字段均使用指定值，能力集为空）。
    ///
    /// 平台层用此方法构造"nobody"等内置账号，避免将 65534 等 Linux 惯例值硬编码
    /// 到 VFS 内部。
    pub fn unprivileged(uid: Uid, gid: Gid) -> Self {
        Self {
            uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
            groups: Vec::new(),
            caps: CapSet::EMPTY,
        }
    }

    /// 构造传统 `nobody` 凭据（uid=gid=65534）。
    ///
    /// 65534 是 Linux 惯例值；非 Linux 平台应改用 [`Credentials::unprivileged`]
    /// 并传入平台特定的 uid/gid。
    #[inline]
    pub fn nobody() -> Self {
        Self::unprivileged(Uid(65534), Gid(65534))
    }

    /// 构造一个拥有全部能力的 root 凭据（uid=gid=0，caps=FULL）。
    ///
    /// 能力通过 `caps = CapSet::FULL` 体现，运行时权限检查不对 euid 做特判。
    pub fn root() -> Self {
        Self {
            uid: Uid(0),
            euid: Uid(0),
            suid: Uid(0),
            fsuid: Uid(0),
            gid: Gid(0),
            egid: Gid(0),
            sgid: Gid(0),
            fsgid: Gid(0),
            groups: Vec::new(),
            caps: CapSet::FULL,
        }
    }

    /// 判断凭据是否拥有指定能力。
    ///
    /// 仅检查 `caps` 位集，不对 `euid == 0` 做隐式特判。
    /// root 凭据通过构造时设置 `caps = CapSet::FULL` 自然拥有所有能力。
    pub fn has_cap(&self, cap: Capability) -> bool {
        self.caps.has(cap)
    }

    /// DAC 权限检查的统一内部实现。
    ///
    /// 三级匹配逻辑（owner → group → other）只写一次，通过 `PermissionKind`
    /// 枚举选择对应的能力和权限位。
    fn check_permission(
        &self,
        file_uid: Uid,
        file_gid: Gid,
        mode: crate::vfs::stat::FileMode,
        kind: PermissionKind,
    ) -> bool {
        use crate::vfs::stat::FileMode;

        // 能力检查
        match kind {
            PermissionKind::Read => {
                if self.has_cap(Capability::DacReadSearch) || self.has_cap(Capability::DacOverride)
                {
                    return true;
                }
            }
            PermissionKind::Write => {
                if self.has_cap(Capability::DacOverride) {
                    return true;
                }
            }
            PermissionKind::Exec { is_dir } => {
                if self.has_cap(Capability::DacOverride)
                    && (is_dir || mode.has_any(FileMode::ANY_EXEC))
                {
                    return true;
                }
            }
        }

        // DAC 三级匹配
        let (owner_bit, group_bit, other_bit) = match kind {
            PermissionKind::Read => (FileMode::IRUSR, FileMode::IRGRP, FileMode::IROTH),
            PermissionKind::Write => (FileMode::IWUSR, FileMode::IWGRP, FileMode::IWOTH),
            PermissionKind::Exec { .. } => (FileMode::IXUSR, FileMode::IXGRP, FileMode::IXOTH),
        };

        if self.fsuid == file_uid {
            return mode.has(owner_bit);
        }
        if self.fsgid == file_gid || self.groups.contains(&file_gid) {
            return mode.has(group_bit);
        }
        mode.has(other_bit)
    }

    /// 判断凭据是否能够读取指定所有者/组/权限位的文件。
    pub fn can_read(&self, file_uid: Uid, file_gid: Gid, mode: crate::vfs::stat::FileMode) -> bool {
        self.check_permission(file_uid, file_gid, mode, PermissionKind::Read)
    }

    /// 判断凭据是否能够写入指定文件。
    pub fn can_write(
        &self,
        file_uid: Uid,
        file_gid: Gid,
        mode: crate::vfs::stat::FileMode,
    ) -> bool {
        self.check_permission(file_uid, file_gid, mode, PermissionKind::Write)
    }

    /// 判断凭据是否能够执行/搜索指定文件或目录。
    ///
    /// `is_dir`：目标是否为目录（搜索权限）。
    pub fn can_exec(
        &self,
        file_uid: Uid,
        file_gid: Gid,
        mode: crate::vfs::stat::FileMode,
        is_dir: bool,
    ) -> bool {
        self.check_permission(file_uid, file_gid, mode, PermissionKind::Exec { is_dir })
    }

    /// 判断凭据是否为文件所有者（用于 `chown`/`chmod` 等元数据修改操作）。
    ///
    /// 规则：`fsuid == file_uid` 或持有 `Capability::FOwner`。
    pub fn is_owner(&self, file_uid: Uid) -> bool {
        self.fsuid == file_uid || self.has_cap(Capability::FOwner)
    }
}
