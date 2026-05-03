//! file-backed VMA 使用的最小文件接口。
//!
//! 定义在 libs/mm 内部，由 [`libs/vfs`] 为其 `File` 类型实现。这样 libs/mm
//! 不必依赖 vfs；依赖方向永远是 `libs/vfs → libs/mm`，不形成循环。
//!
//! 只保留 loader / demand paging / `MAP_SHARED` 写回真正需要的最小方法。

use errno::Errno;

/// file-backed VMA 的最小契约。`Send + Sync` 保证 VMA 可跨 task / 跨核共享。
pub trait FileLike: Send + Sync {
    /// 供 shared file page cache 使用的稳定文件身份。实现应尽量返回 inode /
    /// vnode 级身份，而不是单个打开描述符身份。
    fn cache_key(&self) -> usize;

    /// 从 `offset` 处读最多 `buf.len()` 字节到 `buf`，返回实际读取字节数。
    /// 短读允许；EOF 时返 0。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>;

    /// 从 `offset` 处写最多 `buf.len()` 字节，返回实际写入字节数。
    /// 仅 `MAP_SHARED` 脏页回写路径调用。
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, Errno>;

    /// 把已写入的数据尽力同步到底层存储。
    fn sync(&self) -> Result<(), Errno>;

    /// 文件长度（字节）。由 VMA 解析 page fault 决定是否需要"zero-fill 尾部"。
    fn size(&self) -> u64;
}
