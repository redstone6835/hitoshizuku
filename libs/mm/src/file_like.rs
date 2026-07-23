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

    /// 私有干净页强缓存使用的、内核生命周期内不复用的文件身份。
    ///
    /// 默认不提供，避免把对象地址误当成长生命周期缓存键。返回 `Some` 的实现
    /// 必须保证该值在所有 `FileLike` 实现之间全局唯一，且旧对象释放后也不会
    /// 分配给新对象；否则缓存可能把旧文件内容交给地址复用后的新文件。
    fn private_page_cache_key(&self) -> Option<usize> {
        None
    }

    /// 可供私有干净页缓存使用的稳定内容代际。
    ///
    /// 返回 `Some` 的实现必须在内容开始变化前先让本方法返回 `None`，并保证旧代际
    /// 不会再次出现。无法提供这一保证的 FileLike 始终返回 `None`。
    fn private_page_cache_generation(&self) -> Option<u64> {
        None
    }

    /// 永久停止为该文件发布新的私有干净页缓存。
    ///
    /// 可写 `MAP_SHARED` 可能长期绕过 VFS 直接修改物理页，无法为每次 store
    /// 发布短暂代际。VM 在这类映射生效前调用本 hook；实现应永久返回 `None`。
    /// 该 hook 可能在 VMA 锁内执行，必须无阻塞且不能回调 VM。
    fn disable_private_page_cache(&self) {}

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
