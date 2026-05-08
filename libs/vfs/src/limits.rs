//! VFS 运行时限制配置。
//!
//! 将所有原本散落在各模块中的魔法数字（`SYMLINK_MAX_DEPTH`、`PATH_MAX`、
//! `RLIMIT_NOFILE_*`）集中为一个结构体，允许不同平台或内核配置在初始化时
//! 提供非 Linux 默认值，而无需修改 VFS 核心代码。
//!
//! ### 默认值说明
//!
//! 默认值与 Linux 保持一致（已在注释中标注），但这只是构造上的便利，
//! 不代表这些数字在 VFS 语义上有任何特殊含义。

use alloc::sync::Arc;

/// VFS 运行时限制集合，通过 [`VfsContext`](crate::vfs::VfsContext) 注入到所有操作中。
///
/// 所有字段均为只读配置（内核启动后不再修改），因此不需要加锁。
#[derive(Debug, Clone)]
pub struct VfsLimits {
    /// 路径解析中允许跟随的符号链接最大深度。
    ///
    /// POSIX 要求至少支持 8 层；Linux 取 40；嵌入式目标可设为更小值。
    pub symlink_max_depth: usize,

    /// 路径字符串的最大字节数（不含 NUL 终止符）。
    ///
    /// POSIX 最小值为 255（`_POSIX_PATH_MAX`）；Linux 取 4096（`PATH_MAX`）。
    pub path_max: usize,

    /// 每进程默认最大打开文件数（`RLIMIT_NOFILE` 软限制默认值）。
    pub nofile_default: u32,

    /// 每进程绝对最大打开文件数（`RLIMIT_NOFILE` 硬限制，非特权进程无法超过）。
    pub nofile_max: u32,
}

impl Default for VfsLimits {
    /// 返回与 Linux 默认配置一致的限制值。
    fn default() -> Self {
        Self {
            symlink_max_depth: 40,
            path_max: 4096,
            nofile_default: 1024,
            nofile_max: 4096,
        }
    }
}

impl VfsLimits {
    /// 构造自定义限制，适用于嵌入式或资源受限环境。
    pub const fn new(
        symlink_max_depth: usize,
        path_max: usize,
        nofile_default: u32,
        nofile_max: u32,
    ) -> Self {
        Self {
            symlink_max_depth,
            path_max,
            nofile_default,
            nofile_max,
        }
    }

    /// 返回 Arc 包装的默认限制（便于注入 VfsContext）。
    pub fn default_arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}
