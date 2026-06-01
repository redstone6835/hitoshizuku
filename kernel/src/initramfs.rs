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
use general::vfs::stat::FileMode;
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
    Vfs( #[allow(dead_code)] vfs::error::VfsError),
}

impl From<vfs::error::VfsError> for InitramfsError {
    fn from(value: vfs::error::VfsError) -> Self {
        Self::Vfs(value)
    }
}

#[cfg(feature = "embedded-initramfs")]
pub fn embedded_image() -> Option<InitramfsImage> {
    let bytes = include_bytes!("../../build/initramfs-la.cpio");
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

        if &header[0..6] != b"070701" && &header[0..6] != b"070702" {
            return Err(InitramfsError::BadArchive);
        }

        let mode = read_hex(header, 14)?;
        let file_size = read_hex(header, 54)? as usize;
        let rdev_major = read_hex(header, 78)?;
        let rdev_minor = read_hex(header, 86)?;
        let name_size = read_hex(header, 94)? as usize;
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

        let name = core::str::from_utf8(raw_name).map_err(|_| InitramfsError::Utf8)?;
        if name == "TRAILER!!!" {
            return Ok(());
        }
        let Some(path) = normalize_archive_path(name) else {
            continue;
        };

        ensure_parent_dirs(ctx, &path)?;
        match mode & 0o170000 {
            0o040000 => ensure_dir(ctx, &path, mode_bits(mode))?,
            0o100000 => create_regular(ctx, &path, mode_bits(mode), data)?,
            0o120000 => {
                let target = core::str::from_utf8(data).map_err(|_| InitramfsError::Utf8)?;
                vfs::operation::symlinkat(ctx, target, &Dirfd::Cwd, &path)?;
            }
            0o020000 | 0o060000 | 0o010000 | 0o140000 => {
                let _ = (rdev_major, rdev_minor);
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
        ensure_dir(ctx, &cur, FileMode::new(0o755))?;
    }
    Ok(())
}

fn ensure_dir(ctx: &VfsContext, path: &str, mode: FileMode) -> Result<(), InitramfsError> {
    match vfs::path::lookup(ctx, &Dirfd::Cwd, path, LookupFlags::DIRECTORY) {
        Ok(_) => Ok(()),
        Err(vfs::error::VfsError::NotFound) => {
            vfs::operation::mkdirat(ctx, &Dirfd::Cwd, path, mode)?;
            Ok(())
        }
        Err(err) => Err(InitramfsError::Vfs(err)),
    }
}

fn create_regular(
    ctx: &VfsContext,
    path: &str,
    mode: FileMode,
    data: &[u8],
) -> Result<(), InitramfsError> {
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
