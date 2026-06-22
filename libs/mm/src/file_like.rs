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

    /// VMA 成功绑定到该 file backing 后调用。普通文件默认无动作；SysV shm
    /// 等伪文件可用它维护 attach 计数。
    fn on_mapped(&self) {}

    /// VMA 从地址空间摘除时调用。默认无动作，保持普通 mmap 语义不变。
    fn on_unmapped(&self) {}

    /// 标记该 FileLike 是否代表 SysV shm 对象。默认 false，避免普通文件受影响。
    fn is_sysv_shm(&self) -> bool {
        false
    }

    /// SysV shm 对象的全局 id。普通文件返回 None；VM 层只通过这个通用 hook
    /// 识别 shm VMA，不依赖具体的 `general::ipc` 类型，保持依赖方向不反转。
    fn sysv_shm_id(&self) -> Option<i32> {
        None
    }
}
