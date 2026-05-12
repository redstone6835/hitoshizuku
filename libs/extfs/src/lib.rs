//! ext2 / ext3 / ext4 只读驱动(从零手写,不依赖任何现成 ext crate)。
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
//! | linear + HTree 目录 | 是 | 是 | 是 | 读路径走线性扫描(HTree 仅加速查找,不影响正确性) |
//! | fast / slow symlink | 是 | 是 | 是 | ≤60 字节走 i_block;其它走数据块 |
//! | extra_isize 时间戳 | — | — | 是 | 读取并忽略纳秒 |
//! | METADATA_CSUM 校验 | — | — | 是 | 读侧验证超级块/inode 的 crc32c |
//!
//! **不支持**(mount 时显式拒绝):
//! - 日志未回放(`NEEDS_RECOVERY` 位)——要求先 clean umount 或 fsck;
//! - `ENCRYPT` / `VERITY` / `CASEFOLD` / `PROJECT` 等增量 incompat 位;
//! - 任何写路径(driver flags 永远为只读)。
//!
//! ## 分层
//!
//! - [`state::ExtFsDriver`] 实现 `vfs::superblock::FsDriver`;
//! - [`state::FsState`] 持 backend + 超级块 + 块组描述符数组;
//! - [`inode::ExtInodeOps`] 是文件/目录/符号链接共用的 InodeOps,按
//!   `i_mode` 派发;
//! - [`file::ExtFileOps`] 承接 `read_at` / `readdir`(按 inode 类型区分)。

#![no_std]

extern crate alloc;

mod alloc_mod;
mod bgd;
mod crc;
mod dir;
mod dir_wr;
mod extent;
mod extent_wr;
mod file;
mod inode;
mod inode_wr;
mod layout;
mod map;
mod map_wr;
mod sb;
mod state;
mod symlink;

pub use state::{BlockBackend, BlockBackendError, ExtFsDriver};
