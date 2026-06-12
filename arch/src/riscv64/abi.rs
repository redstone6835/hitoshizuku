//! RISC-V64 Linux ABI 位布局转换。
//!
//! 本模块集中 Linux 用户空间 ABI 的位布局知识（`O_*`、`MS_*`、`dev_t` 编码），
//! 将裸整数解码为 `general` 层定义的语义类型。VFS 内层只看平台无关类型，不接触
//! 原始数值常量。
//!
//! RISC-V64 使用 asm-generic 的 ABI 编号，与 ARM64 等新架构一致。

use general::dev::char::CharDevice;
use general::vfs::file::{AccessMode, OpenOptions};
use general::vfs::mount::MountFlags;
use general::vfs::stat::DevId;

// ── dev_t 编解码（对应 Linux new_encode_dev / new_decode_dev） ──────────────

/// 编码 `DevId` → Linux 64-bit `dev_t`。
///
/// # 参数
/// - `dev`: 平台无关的设备号（major + minor）。
///
/// # 返回值
/// Linux `new_encode_dev` 格式的 64 位设备号。
#[inline]
pub fn encode_dev_t(dev: DevId) -> u64 {
    let (maj, min) = (dev.major as u64, dev.minor as u64);
    (min & 0xFF)
        | ((maj & 0xFFF) << 8)
        | ((min & !0xFFu64) << 12)
        | ((maj & !0xFFFu64) << 32)
}

/// 解码 Linux 64-bit `dev_t` → `DevId`。
///
/// # 参数
/// - `raw`: Linux `new_encode_dev` 格式的 64 位设备号。
///
/// # 返回值
/// 平台无关的 `DevId`。
#[inline]
pub fn decode_dev_t(raw: u64) -> DevId {
    let major = ((raw >> 8) & 0xFFF) | ((raw >> 32) & !0xFFFu64);
    let minor = (raw & 0xFF) | ((raw >> 12) & !0xFFu64);
    DevId::new(major as u32, minor as u32)
}

// ── open(2) flags（asm-generic/fcntl.h） ────────────────────────────────────

// RISC-V64 使用 asm-generic 编号，值与 ARM64、x86-64 一致。
// 参考: include/uapi/asm-generic/fcntl.h
const O_ACCMODE: u32 = 0o3;
const O_WRONLY: u32 = 0o1;
const O_RDWR: u32 = 0o2;
const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_NONBLOCK: u32 = 0o4000;
// O_SYNC = __O_SYNC | O_DSYNC，两者需同时判断才是完整的 synchronized I/O
const O_SYNC: u32 = 0o4010000;
const O_DIRECTORY: u32 = 0o200000;
const O_NOFOLLOW: u32 = 0o400000;
const O_NOATIME: u32 = 0o1000000;
const O_CLOEXEC: u32 = 0o2000000;
const O_PATH: u32 = 0o10000000;

/// 解码 `open(2)` flags → [`OpenOptions`]。
///
/// # 参数
/// - `raw`: 用户态传入的 `open(2)` flags 位域。
///
/// # 返回值
/// 平台无关的 `OpenOptions` 结构体。
#[inline]
pub fn decode_open_flags(raw: u32) -> OpenOptions {
    OpenOptions {
        access: match raw & O_ACCMODE {
            O_WRONLY => AccessMode::WriteOnly,
            O_RDWR => AccessMode::ReadWrite,
            _ => AccessMode::ReadOnly,
        },
        create: raw & O_CREAT != 0,
        exclusive: raw & O_EXCL != 0,
        truncate: raw & O_TRUNC != 0,
        append: raw & O_APPEND != 0,
        nonblock: raw & O_NONBLOCK != 0,
        sync: raw & O_SYNC != 0,
        direct: raw & 0o40000 != 0,
        directory: raw & O_DIRECTORY != 0,
        nofollow: raw & O_NOFOLLOW != 0,
        noatime: raw & O_NOATIME != 0,
        cloexec: raw & O_CLOEXEC != 0,
        path_only: raw & O_PATH != 0,
    }
}

// ── mount(2) flags ──────────────────────────────────────────────────────────

/// Linux `MS_*` 到 VFS [`MountFlags`] 的映射表。
/// 参考: include/uapi/linux/mount.h
const MOUNT_FLAG_MAP: &[(u32, MountFlags)] = &[
    (1 << 0, MountFlags::RDONLY),
    (1 << 1, MountFlags::NOSUID),
    (1 << 2, MountFlags::NODEV),
    (1 << 3, MountFlags::NOEXEC),
    (1 << 4, MountFlags::SYNCHRONOUS),
    (1 << 10, MountFlags::NOATIME),
    (1 << 11, MountFlags::NODIRATIME),
    (1 << 12, MountFlags::BIND),
    (1 << 14, MountFlags::REC),
];

/// 解码 `mount(2)` mountflags → [`MountFlags`]。
///
/// # 参数
/// - `raw`: 用户态传入的 `mount(2)` mountflags 位域。
///
/// # 返回值
/// 平台无关的 `MountFlags`。
#[inline]
pub fn decode_mount_flags(raw: u32) -> MountFlags {
    let mut out = MountFlags::default();
    for &(linux_bit, vfs_flag) in MOUNT_FLAG_MAP {
        if raw & linux_bit != 0 {
            out = out.with(vfs_flag);
        }
    }
    out
}

// ── 字符设备号映射（<linux/major.h>） ────────────────────────────────────────

/// 根据固件名称和驱动能力，映射字符设备到 Linux major:minor。
///
/// 通过 `fw_name` + 驱动能力（`is_tty()`）判定，不依赖闭枚举。
/// 未识别设备默认分配 major=0。
///
/// # 参数
/// - `dev`: 字符设备引用。
///
/// # 返回值
/// 对应的 Linux 设备号；未识别设备返回 `0:0`。
pub fn char_dev_to_dev_id(dev: &CharDevice) -> DevId {
    match dev.fw_name() {
        "null" => DevId::new(1, 3),
        "zero" => DevId::new(1, 5),
        "random" | "urandom" => DevId::new(1, 8),
        "console" => DevId::new(5, 1),
        _ if dev.is_tty() => DevId::new(4, 64),
        _ => DevId::new(0, 0),
    }
}
