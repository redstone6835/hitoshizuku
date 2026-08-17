//! ext2 / ext3 / ext4 读写驱动(从零手写,不依赖任何现成 ext crate)。
//!
//! ## 支持与不支持
//!
//! | 功能 | ext2 | ext3 | ext4 | 备注 |
//! |------|------|------|------|------|
//! | 超级块解析 + feature 门禁 | 是 | 是 | 是 | [`sb`] |
//! | 32/64 位块组描述符 | 是 | 是 | 是 | [`bgd`] |
//! | 传统间接块寻址 | 是 | 是 | 是 | ext4 不用 extent 的文件也走这里 |
//! | extent tree | — | — | 是 | [`extent`],支持内节点 + 叶节点 |
//! | inline_data | — | — | 是 | 最小支持:inode.i_block 直接承载 |
//! | linear + HTree 目录 | 是 | 是 | 是 | 读路径走线性扫描(跳过索引块,正确性不受影响) |
//! | fast / slow symlink | 是 | 是 | 是 | ≤60 字节走 i_block;其它走数据块 |
//! | extra_isize 时间戳 | — | — | 是 | 读取并忽略纳秒 |
//! | METADATA_CSUM 校验 | — | — | 是 | 读侧验证超级块/块组描述符/inode/extent 节点/目录叶 |
//! | JBD2 日志恢复 | — | 是 | 是 | [`journal`]:scan/revoke/replay,v1/v2/v3 校验、撕裂提交、回绕 |
//! | fast commit 回放 | — | — | 是 | [`fc`]:HEAD/TAIL/CREAT/LINK/UNLINK/INODE/ADD_RANGE/DEL_RANGE |
//! | 孤儿 inode 清理 | — | 是 | 是 | [`orphan`]:s_last_orphan 链表 + orphan file |
//! | 读写(create/mkdir/unlink/rename/write/truncate/...) | 是 | 是 | 是 | 写路径不走日志(writeback 语义),崩溃依赖 s_state + fsck |
//! | MMP | — | — | 挂载时检查 | 仅接受 CLEAN 序列;无运行时心跳 |
//! | CASEFOLD | — | — | 挂载 | 带标志目录按 ASCII 大小写不敏感(完整 Unicode 未实现) |
//! | ENCRYPT | — | — | 挂载 | 加密 inode 读写返回 `NotSupported`(与无密钥 Linux 一致) |
//! | VERITY | — | — | 挂载 | verity inode 写/截断返回 `ReadOnlyFilesystem` |
//! | BIGALLOC / READONLY / HAS_SNAPSHOT / SHARED_BLOCKS | — | — | 强制只读 | 未知 ro_compat 位同样退化为只读 |
//!
//! **显式拒绝**(mount 返回 `NotSupported`/`InvalidArgument`):
//! - `COMPRESSION` / `DIRDATA`(Linux 也从未实现)/ `META_BG`(已被 64BIT 取代);
//! - `JOURNAL_DEV`(日志设备本身不是文件系统);外部日志设备;
//! - fast commit 遇到 depth>0 的 extent 树(安全失败,请先 fsck);
//! - 任何未知 incompat 位。
//!
//! ## 分层
//!
//! - [`state::ExtFsDriver`] 实现 `vfs::superblock::FsDriver`;
//! - [`state::FsState`] 持 backend + 超级块 + 块组描述符数组;
//! - [`journal`] 在挂载时回放 JBD2 日志并复位日志头;
//! - [`orphan`] 在挂载时清理孤儿 inode;
//! - [`inode::ExtInodeOps`] 是文件/目录/符号链接共用的 InodeOps,按
//!   `i_mode` 派发;
//! - [`file::ExtRegFileOps`] 承接 `read_at` / `write_at` / `readdir`(按 inode 类型区分)。

#![no_std]

extern crate alloc;

mod alloc_mod;
mod bgd;
mod crc;
mod dir;
mod dir_wr;
mod extent;
mod extent_wr;
mod fc;
mod file;
mod inode;
mod inode_wr;
mod xattr;
mod journal;
mod layout;
mod map;
mod map_wr;
mod orphan;
mod sb;
mod state;
mod symlink;

pub use state::{BlockBackend, BlockBackendError, ExtFsDriver};

/// 强制链接器保留 Ext 驱动直接符号目录。
#[doc(hidden)]
pub fn kernel_symbol_catalog_anchor() -> usize {
    ExtFsDriver::bind_backend as usize
}

#[cfg(test)]
mod tests;
