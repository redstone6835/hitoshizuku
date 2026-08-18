//! LoongArch64 Linux ABI 转换层。
//!
//! 此模块是 VFS 层与 LoongArch64 Linux syscall ABI 之间的唯一接口。
//! **禁止在此模块外部引入任何 LoongArch64 专属常量或位布局**——所有平台相关
//! 知识均集中于此，使 VFS 内层保持平台无关。
//!
//! 每个 `decode_*` 函数将用户空间传入的原始整数映射为 VFS 内部类型；
//! 每个 `encode_*` 函数将 VFS 内部类型序列化为用户空间期望的整数格式。

use general::vfs::file::{AccessMode, OpenOptions};
use general::vfs::mount::MountFlags;
use general::vfs::stat::DevId;

// ── 设备号编码 ────────────────────────────────────────────────────────────────

/// 将 [`DevId`] 编码为 LoongArch64 Linux `dev_t`（64 位）格式。
///
/// Linux 64-bit `dev_t` 位布局（`makedev(major, minor)`）：
/// ```text
/// bits[7:0]   = minor[7:0]
/// bits[19:8]  = major[11:0]
/// bits[31:20] = minor[19:8]  （高 minor 位）
/// bits[51:32] = major[31:12] （高 major 位，LoongArch64 扩展）
/// ```
///
/// 此函数是唯一允许了解该位布局的地方。
pub fn encode_dev_t(dev: DevId) -> u64 {
    let major = dev.major as u64;
    let minor = dev.minor as u64;
    (minor & 0xFF) | ((major & 0xFFF) << 8) | ((minor & !0xFF) << 12) | ((major & 0xFFFFF000) << 32)
}

/// 将 LoongArch64 Linux `dev_t`（64 位）解码为 [`DevId`]。
///
/// `encode_dev_t` 的逆操作，供 `mknod(2)` 等接收 `dev_t` 参数的 syscall 使用。
pub fn decode_dev_t(raw: u64) -> DevId {
    let minor = (raw & 0xFF) | ((raw >> 12) & !0xFF);
    let major = ((raw >> 8) & 0xFFF) | (((raw >> 32) & 0xFFFFF000) as u64);
    DevId::new(major as u32, minor as u32)
}

// ── 挂载标志解码 ──────────────────────────────────────────────────────────────

// ── open(2) 标志解码 ──────────────────────────────────────────────────────────

/// 将 LoongArch64 Linux `open(2)` `flags` 参数解码为平台无关的 VFS [`OpenOptions`]。
///
/// LoongArch64 沿用 Linux `O_*` 数值（与 x86_64 几乎相同，以八进制标注）：
/// ```text
/// O_RDONLY    = 0o0
/// O_WRONLY    = 0o1
/// O_RDWR      = 0o2
/// O_CREAT     = 0o100
/// O_EXCL      = 0o200
/// O_TRUNC     = 0o1000
/// O_APPEND    = 0o2000
/// O_NONBLOCK  = 0o4000
/// O_SYNC      = 0o4010000  （O_DSYNC | __O_SYNC）
/// O_DIRECTORY = 0o200000
/// O_NOFOLLOW  = 0o400000
/// O_CLOEXEC   = 0o2000000
/// O_NOATIME   = 0o1000000
/// O_PATH      = 0o10000000
/// ```
///
/// 此函数是唯一允许引用上述 LoongArch64 Linux `O_*` 数值的地方。
pub fn decode_open_flags(raw: u32) -> OpenOptions {
    let access = match raw & 0b11 {
        1 => AccessMode::WriteOnly,
        2 => AccessMode::ReadWrite,
        _ => AccessMode::ReadOnly,
    };
    OpenOptions {
        access,
        create: raw & 0o100 != 0,
        exclusive: raw & 0o200 != 0,
        truncate: raw & 0o1000 != 0,
        append: raw & 0o2000 != 0,
        nonblock: raw & 0o4000 != 0,
        direct: raw & 0o40000 != 0,
        async_: raw & 0o20000 != 0,
        sync: raw & 0o4010000 != 0,
        directory: raw & 0o200000 != 0,
        nofollow: raw & 0o400000 != 0,
        noatime: raw & 0o1000000 != 0,
        cloexec: raw & 0o2000000 != 0,
        path_only: raw & 0o10000000 != 0,
    }
}

// ── 挂载标志解码 ──────────────────────────────────────────────────────────────

/// 将 LoongArch64 Linux `mount(2)` 的 `mountflags` 原始值解码为 VFS [`MountFlags`]。
///
/// Linux `MS_*` 数值与 VFS 内部位编号解耦：此处是唯一允许引用 Linux `MS_*` 值的地方。
pub fn decode_mount_flags(raw: u32) -> MountFlags {
    let mut flags = MountFlags::default();
    // Linux MS_* values (from <linux/mount.h>)
    if raw & (1 << 0) != 0 {
        flags = flags.with(MountFlags::RDONLY);
    }
    if raw & (1 << 1) != 0 {
        flags = flags.with(MountFlags::NOSUID);
    }
    if raw & (1 << 2) != 0 {
        flags = flags.with(MountFlags::NODEV);
    }
    if raw & (1 << 3) != 0 {
        flags = flags.with(MountFlags::NOEXEC);
    }
    if raw & (1 << 4) != 0 {
        flags = flags.with(MountFlags::SYNCHRONOUS);
    }
    if raw & (1 << 10) != 0 {
        flags = flags.with(MountFlags::NOATIME);
    }
    if raw & (1 << 11) != 0 {
        flags = flags.with(MountFlags::NODIRATIME);
    }
    if raw & (1 << 12) != 0 {
        flags = flags.with(MountFlags::BIND);
    }
    if raw & (1 << 14) != 0 {
        flags = flags.with(MountFlags::REC);
    }
    flags
}
