//! Initramfs unpacking.
//!
//! The boot path accepts the Linux `newc` CPIO format.  The archive is unpacked
//! through the normal VFS entry points so tmpfs remains the only filesystem that
//! needs to know how files and directories are represented.

use alloc::format;
use alloc::string::String;

use general::vfs::fdtable::FdTable;
use general::vfs::file::{AccessMode, OpenOptions};
use general::vfs::path::{Dirfd, LookupFlags};
use general::vfs::stat::{DevId, FileMode, FileType};
use general::vfs::{self, VfsContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitramfsSource {
    Embedded,
    External,
}

#[derive(Debug, Clone, Copy)]
pub struct InitramfsImage {
    pub bytes: &'static [u8],
    pub source: InitramfsSource,
}

#[derive(Debug)]
pub enum InitramfsError {
    BadArchive,
    UnsupportedEntry,
    Utf8,
    Vfs(#[allow(dead_code)] vfs::error::VfsError),
}

impl From<vfs::error::VfsError> for InitramfsError {
    fn from(value: vfs::error::VfsError) -> Self {
        Self::Vfs(value)
    }
}

#[cfg(feature = "embedded-initramfs")]
pub fn embedded_image() -> Option<InitramfsImage> {
    let bytes = include_bytes!(env!("MYGO_INITRAMFS_CPIO"));
    (!bytes.is_empty()).then_some(InitramfsImage {
        bytes,
        source: InitramfsSource::Embedded,
    })
}

#[cfg(not(feature = "embedded-initramfs"))]
pub fn embedded_image() -> Option<InitramfsImage> {
    None
}

pub fn unpack_newc(image: InitramfsImage, ctx: &VfsContext) -> Result<(), InitramfsError> {
    let mut cursor = 0usize;
    let bytes = image.bytes;

    loop {
        if cursor == bytes.len() {
            return Err(InitramfsError::BadArchive);
        }
        if bytes.len().saturating_sub(cursor) < 110 {
            return Err(InitramfsError::BadArchive);
        }
        let header = &bytes[cursor..cursor + 110];
        cursor += 110;

        let checksum_present = &header[0..6] == b"070702";
        if &header[0..6] != b"070701" && !checksum_present {
            return Err(InitramfsError::BadArchive);
        }

        let mode = read_hex(header, 14)?;
        let file_size = read_hex(header, 54)? as usize;
        let rdev_major = read_hex(header, 78)?;
        let rdev_minor = read_hex(header, 86)?;
        let name_size = read_hex(header, 94)? as usize;
        let expected_checksum = read_hex(header, 102)?;
        if name_size == 0 {
            return Err(InitramfsError::BadArchive);
        }

        let name_end = cursor
            .checked_add(name_size)
            .ok_or(InitramfsError::BadArchive)?;
        if name_end > bytes.len() {
            return Err(InitramfsError::BadArchive);
        }
        let raw_name = &bytes[cursor..name_end - 1];
        if bytes[name_end - 1] != 0 {
            return Err(InitramfsError::BadArchive);
        }
        cursor = align4(name_end);

        let data_end = cursor
            .checked_add(file_size)
            .ok_or(InitramfsError::BadArchive)?;
        if data_end > bytes.len() {
            return Err(InitramfsError::BadArchive);
        }
        let data = &bytes[cursor..data_end];
        cursor = align4(data_end);
        if checksum_present
            && mode & 0o170000 == 0o100000
            && data
                .iter()
                .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
                != expected_checksum
        {
            return Err(InitramfsError::BadArchive);
        }

        let name = core::str::from_utf8(raw_name).map_err(|_| InitramfsError::Utf8)?;
        if name == "TRAILER!!!" {
            // Linux accepts concatenated newc members.  Skip the zero padding
            // between trailers and the next member, while allowing a final
            // zero-filled tail to terminate the archive normally.
            while cursor < bytes.len() && bytes[cursor] == 0 {
                cursor += 1;
            }
            if cursor == bytes.len() {
                return Ok(());
            }
            if bytes.len() - cursor < 6
                || (&bytes[cursor..cursor + 6] != b"070701"
                    && &bytes[cursor..cursor + 6] != b"070702")
            {
                return Err(InitramfsError::BadArchive);
            }
            continue;
        }
        let Some(path) = normalize_archive_path(name) else {
            continue;
        };

        ensure_parent_dirs(ctx, &path)?;
        match mode & 0o170000 {
            0o040000 => create_directory(ctx, &path, mode_bits(mode))?,
            0o100000 => create_regular(ctx, &path, mode_bits(mode), data)?,
            0o120000 => {
                let target = core::str::from_utf8(data).map_err(|_| InitramfsError::Utf8)?;
                clean_path(ctx, &path, None)?;
                vfs::operation::symlinkat(ctx, target, &Dirfd::Cwd, &path)?;
            }
            0o020000 | 0o060000 | 0o010000 | 0o140000 => {
                let kind = match mode & 0o170000 {
                    0o020000 => FileType::CharDevice,
                    0o060000 => FileType::BlockDevice,
                    0o010000 => FileType::Fifo,
                    0o140000 => FileType::Socket,
                    _ => unreachable!(),
                };
                clean_path(ctx, &path, Some(kind))?;
                if path_kind(ctx, &path)? != Some(kind) {
                    vfs::operation::mknodat(
                        ctx,
                        &Dirfd::Cwd,
                        &path,
                        kind,
                        mode_bits(mode),
                        DevId::new(rdev_major, rdev_minor),
                    )?;
                }
                vfs::operation::fchmodat(ctx, &Dirfd::Cwd, &path, mode_bits(mode), true)?;
            }
            _ => return Err(InitramfsError::UnsupportedEntry),
        }
    }
}

fn read_hex(header: &[u8], off: usize) -> Result<u32, InitramfsError> {
    let mut value = 0u32;
    for &b in header.get(off..off + 8).ok_or(InitramfsError::BadArchive)? {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err(InitramfsError::BadArchive),
        };
        value = (value << 4) | digit as u32;
    }
    Ok(value)
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn mode_bits(mode: u32) -> FileMode {
    FileMode::new((mode & 0o7777) as u16)
}

fn normalize_archive_path(name: &str) -> Option<String> {
    let path = name.trim_start_matches("./").trim_start_matches('/');
    if path.is_empty() || path == "." {
        None
    } else {
        Some(format!("/{path}"))
    }
}

fn ensure_parent_dirs(ctx: &VfsContext, path: &str) -> Result<(), InitramfsError> {
    let trimmed = path.trim_start_matches('/');
    let Some((parent, _)) = trimmed.rsplit_once('/') else {
        return Ok(());
    };
    if parent.is_empty() {
        return Ok(());
    }

    let mut cur = String::new();
    for component in parent.split('/') {
        if component.is_empty() {
            continue;
        }
        cur.push('/');
        cur.push_str(component);
        ensure_parent_dir(ctx, &cur)?;
    }
    Ok(())
}

fn path_kind(ctx: &VfsContext, path: &str) -> Result<Option<FileType>, InitramfsError> {
    match vfs::path::lookup(ctx, &Dirfd::Cwd, path, LookupFlags::NO_FOLLOW) {
        Ok(result) => Ok(Some(
            result
                .dentry
                .inode()
                .ok_or(vfs::error::VfsError::NotFound)?
                .kind(),
        )),
        Err(vfs::error::VfsError::NotFound) => Ok(None),
        Err(err) => Err(InitramfsError::Vfs(err)),
    }
}

/// 与 Linux initramfs `clean_path()` 一样，对末端分量做 no-follow 类型比较。
/// 类型不一致时先移除旧对象，避免普通文件覆盖符号链接时写入链接目标。
fn clean_path(
    ctx: &VfsContext,
    path: &str,
    expected: Option<FileType>,
) -> Result<(), InitramfsError> {
    let Some(existing) = path_kind(ctx, path)? else {
        return Ok(());
    };
    if expected == Some(existing) {
        return Ok(());
    }
    if existing == FileType::Directory {
        vfs::operation::rmdir(ctx, &Dirfd::Cwd, path)?;
    } else {
        vfs::operation::unlink(ctx, &Dirfd::Cwd, path)?;
    }
    Ok(())
}

fn ensure_parent_dir(ctx: &VfsContext, path: &str) -> Result<(), InitramfsError> {
    match vfs::path::lookup(ctx, &Dirfd::Cwd, path, LookupFlags::DIRECTORY) {
        Ok(_) => Ok(()),
        Err(vfs::error::VfsError::NotFound) => {
            vfs::operation::mkdirat(ctx, &Dirfd::Cwd, path, FileMode::new(0o755))?;
            Ok(())
        }
        Err(err) => Err(InitramfsError::Vfs(err)),
    }
}

fn create_directory(ctx: &VfsContext, path: &str, mode: FileMode) -> Result<(), InitramfsError> {
    clean_path(ctx, path, Some(FileType::Directory))?;
    if path_kind(ctx, path)?.is_none() {
        vfs::operation::mkdirat(ctx, &Dirfd::Cwd, path, mode)?;
    }
    vfs::operation::fchmodat(ctx, &Dirfd::Cwd, path, mode, false)?;
    Ok(())
}

fn create_regular(
    ctx: &VfsContext,
    path: &str,
    mode: FileMode,
    data: &[u8],
) -> Result<(), InitramfsError> {
    clean_path(ctx, path, Some(FileType::Regular))?;
    let fdt = FdTable::new_default();
    let fd = vfs::operation::openat(
        ctx,
        &fdt,
        &Dirfd::Cwd,
        path,
        OpenOptions {
            access: AccessMode::WriteOnly,
            create: true,
            truncate: true,
            ..OpenOptions::default()
        },
        mode,
    )?;
    let file = fdt
        .get_file(fd)
        .ok_or(vfs::error::VfsError::BadFileDescriptor)?;
    vfs::operation::fchmod(ctx, &fdt, fd, mode)?;

    let mut written = 0usize;
    while written < data.len() {
        let n = file.write_at(&data[written..], written as u64)?;
        if n == 0 {
            return Err(InitramfsError::Vfs(vfs::error::VfsError::InvalidArgument));
        }
        written += n;
    }
    let _ = fdt.close_fd(fd);
    Ok(())
}
