//! FAT12/16/32 文件系统驱动(从零手写,不依赖任何外部 FAT crate)。
//!
//! ## 设计概览
//!
//! - [`BlockBackend`] 是文件系统与存储设备之间的同步 I/O 契约,调用方负责
//!   把它适配到实际的块设备驱动上(例如项目内的 `BlockDev`)。
//! - [`FatFsDriver`] 实现 [`vfs::superblock::FsDriver`],挂载时产生一个
//!   [`Superblock`](vfs::superblock::Superblock),其 `ops` 字段
//!   (`FatFsSuperblockOps`)持有共享的 [`state::FsState`] 让所有 inode 使用。
//! - Inode 按文件类型分两种 ops:[`inode::DirInodeOps`] 与
//!   [`inode::FileInodeOps`];File 打开后由 [`file::DirFileOps`] 或
//!   [`file::RegFileOps`] 承接 I/O。
//!
//! ## 覆盖范围
//!
//! 支持 FAT12 / FAT16 / FAT32(自动按簇数判别),带如下特性:
//! - BPB 校验、FSInfo 维护、`.` / `..` 条目、LFN(含 unicode checksum)、
//!   SFN 冲突检测下的 `~N` 混叠、目录扩容、文件截断、O_APPEND 原子追加。
//! - 所有 FAT 表读写都走共享的 FAT 扇区缓存(最小 LRU),卸载前 flush。
//!
//! ## 不做
//!
//! - 不做 exFAT(不同格式);不做日志(FAT 本就不带)。时间戳按 Linux vfat 语义
//!   真实化:写入用 `Timespec::now()` 编码为 DOS 日期/时间(2 秒粒度),读取解码回
//!   `stat` 的 atime/mtime/ctime;未安装实时时钟时编码结果自然落到 1980-01-01。

#![no_std]

extern crate alloc;

mod bpb;
mod dir;
mod fat;
mod file;
mod inode;
mod lfn;
mod name;
mod state;
mod sync_layer;
mod time;

pub use state::{BlockBackend, BlockBackendError, FatFsDriver};

/// 强制链接器保留 FAT 驱动直接符号目录。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    FatFsDriver::bind_backend as usize
}

#[cfg(test)]
mod tests;
