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

    /// 从 `offset` 起精确初始化一组页面的前 `valid_len` 字节，并把其余尾部清零。
    ///
    /// 默认实现基于 [`Self::read_at`] 循环处理合法短读。若在填满有效区之前到达
    /// EOF，则返回 `EIO`；实现方可以覆盖本方法，把文件数据直接读入 VM 持有的页。
    fn read_pages_at(
        &self,
        offset: u64,
        pages: &mut [&mut [u8]],
        valid_len: usize,
    ) -> Result<(), Errno> {
        let mut capacity = 0usize;
        for page in pages.iter() {
            if page.is_empty() {
                return Err(Errno::EINVAL);
            }
            capacity = capacity.checked_add(page.len()).ok_or(Errno::EOVERFLOW)?;
        }
        if valid_len > capacity {
            return Err(Errno::EINVAL);
        }
        let valid_len_u64 = u64::try_from(valid_len).map_err(|_| Errno::EOVERFLOW)?;
        offset.checked_add(valid_len_u64).ok_or(Errno::EOVERFLOW)?;

        let mut page_start = 0usize;
        for page in pages.iter_mut() {
            let tail_start = valid_len.saturating_sub(page_start).min(page.len());
            page[tail_start..].fill(0);
            page_start += page.len();
        }

        let mut remaining = valid_len;
        let mut read_offset = offset;
        for page in pages.iter_mut() {
            let page_valid = remaining.min(page.len());
            let mut done = 0usize;
            while done < page_valid {
                let count = self.read_at(read_offset, &mut page[done..page_valid])?;
                if count == 0 || count > page_valid - done {
                    return Err(Errno::EIO);
                }
                done += count;
                read_offset += count as u64;
            }
            remaining -= page_valid;
            if remaining == 0 {
                break;
            }
        }
        Ok(())
    }

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

    /// 底层文件句柄是否可写（对应 Linux `file->f_mode & FMODE_WRITE`）。
    ///
    /// 返回 `None` 表示实现无法提供该信息（如纯内存伪文件），VMA 层按"无附加
    /// 约束"处理。`mprotect(PROT_WRITE)` 对 `MAP_SHARED` 映射要求底层句柄可写，
    /// 否则返回 `EACCES`——这就是 Linux `mprotect` 的 `EACCES` 语义来源。
    fn writable_hint(&self) -> Option<bool> {
        None
    }

    /// 该文件是否为 shmem/tmpfs 文件（Linux `shmem_file_setup` 家族）。
    ///
    /// `MADV_REMOVE` 只对 shmem 文件映射有效，普通文件映射返回 `EINVAL`；
    /// userfaultfd 也以该属性区分匿名/shmem 区与普通文件区。
    fn is_shmem(&self) -> bool {
        false
    }

    /// 在文件中打洞（Linux `fallocate(PUNCH_HOLE|KEEP_SIZE)`）。
    ///
    /// `MADV_REMOVE` 的底层动作：把 `[offset, offset+len)` 的数据释放并读回零。
    /// 默认返回 `EOPNOTSUPP`；tmpfs/shmem 类实现覆盖本方法。
    fn punch_hole(&self, offset: u64, len: u64) -> Result<(), Errno> {
        let _ = (offset, len);
        Err(Errno::EOPNOTSUPP)
    }

    /// SysV shm 对象的全局 id。普通文件返回 None；VM 层只通过这个通用 hook
    /// 识别 shm VMA，不依赖具体的 `general::ipc` 类型，保持依赖方向不反转。
    fn sysv_shm_id(&self) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::FileLike;
    use errno::Errno;

    struct ShortReader {
        data: &'static [u8],
        max_read: usize,
    }

    impl FileLike for ShortReader {
        fn cache_key(&self) -> usize {
            self.data.as_ptr() as usize
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let start = usize::try_from(offset).map_err(|_| Errno::EOVERFLOW)?;
            if start >= self.data.len() {
                return Ok(0);
            }
            let count = buf.len().min(self.max_read).min(self.data.len() - start);
            buf[..count].copy_from_slice(&self.data[start..start + count]);
            Ok(count)
        }

        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, Errno> {
            Err(Errno::EROFS)
        }

        fn sync(&self) -> Result<(), Errno> {
            Ok(())
        }

        fn size(&self) -> u64 {
            self.data.len() as u64
        }
    }

    #[test]
    fn read_pages_retries_short_reads_and_zeros_tail() {
        let reader = ShortReader {
            data: b"0123456789",
            max_read: 2,
        };
        let mut first = [0xff; 4];
        let mut second = [0xff; 4];
        let mut pages: [&mut [u8]; 2] = [&mut first, &mut second];

        reader
            .read_pages_at(1, &mut pages, 6)
            .expect("short reads must be retried");

        assert_eq!(&first, b"1234");
        assert_eq!(&second, &[b'5', b'6', 0, 0]);
    }

    #[test]
    fn read_pages_rejects_early_eof_and_invalid_layout() {
        let reader = ShortReader {
            data: b"abc",
            max_read: 2,
        };
        let mut page = [0xff; 4];
        assert_eq!(
            reader.read_pages_at(0, &mut [&mut page], 4),
            Err(Errno::EIO)
        );

        let mut empty = [];
        assert_eq!(
            reader.read_pages_at(0, &mut [&mut empty], 0),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            reader.read_pages_at(u64::MAX, &mut [&mut page], 2),
            Err(Errno::EOVERFLOW)
        );
    }
}
