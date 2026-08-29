//! x86_64 Linux ABI 的位布局转换。
//!
//! VFS/general 只接触平台无关类型；Linux 的 `dev_t`、`open(2)` 和 mount
//! 标志在这里集中解码，避免把 x86 UAPI 数值泄漏到通用逻辑。

use general::dev::char::CharDevice;
use general::vfs::file::{AccessMode, OpenOptions};
use general::vfs::mount::MountFlags;
use general::vfs::stat::DevId;

#[inline]
pub fn encode_dev_t(dev: DevId) -> u64 {
    let major = dev.major as u64;
    let minor = dev.minor as u64;
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

#[inline]
pub fn decode_dev_t(raw: u64) -> DevId {
    let major = ((raw >> 8) & 0xfff) | ((raw >> 32) & !0xfff);
    let minor = (raw & 0xff) | ((raw >> 12) & !0xff);
    DevId::new(major as u32, minor as u32)
}

const O_ACCMODE: u32 = 0o3;
const O_WRONLY: u32 = 0o1;
const O_RDWR: u32 = 0o2;
const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_NONBLOCK: u32 = 0o4000;
const O_ASYNC: u32 = 0o20000;
const O_DIRECT: u32 = 0o40000;
const O_DIRECTORY: u32 = 0o200000;
const O_NOFOLLOW: u32 = 0o400000;
const O_NOATIME: u32 = 0o1000000;
const O_CLOEXEC: u32 = 0o2000000;
const O_SYNC: u32 = 0o4010000;
const O_PATH: u32 = 0o10000000;

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
        direct: raw & O_DIRECT != 0,
        async_: raw & O_ASYNC != 0,
        directory: raw & O_DIRECTORY != 0,
        nofollow: raw & O_NOFOLLOW != 0,
        noatime: raw & O_NOATIME != 0,
        cloexec: raw & O_CLOEXEC != 0,
        path_only: raw & O_PATH != 0,
    }
}

#[inline]
pub fn decode_mount_flags(raw: u32) -> MountFlags {
    let mut flags = MountFlags::default();
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

#[inline]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_number_roundtrip() {
        let dev = DevId::new(0x1234, 0x56789);
        assert_eq!(decode_dev_t(encode_dev_t(dev)), dev);
    }
}
