//! 进程文件描述符表（File Descriptor Table，FdTable）。
//!
//! FdTable 将整数 fd（文件描述符编号）映射到 `Arc<File>`，是进程与内核 VFS
//! 之间的接口桥梁。系统调用层持有进程的 `FdTable`，通过整数 fd 找到对应的
//! `File` 对象后调用 VFS 操作。
//!
//! ### 设计要点
//!
//! - **fd 号分配**：总是使用当前最小可用的非负整数，与 POSIX 保持一致；
//! - **每进程上限**：区分软限制与硬限制；`open`/`dup` 受当前软限制约束，
//!   `setrlimit` 不得把软限制提升到硬限制以上；
//! - **CLOEXEC 标志**：每个 fd 条目携带 [`FdFlags`]，`execve` 时由系统调用层
//!   调用 [`FdTable::close_on_exec`] 批量关闭所有 CLOEXEC 描述符；
//! - **fork 语义**：`fork` 时复制整张 FdTable（每个 fd 克隆 `Arc<File>`），
//!   父子进程共享 `File` 对象（即共享偏移量）；`clone(CLONE_FILES)` 则共享同
//!   一个 `FdTable` 引用，此时需要在整张表上加锁，此处暂不支持（留待扩展）。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::File;
use crate::vfs::limits::VfsLimits;
use crate::vfs::sync::Spinlock;

/// 每进程默认最大打开文件数的默认值（软限制）。
///
/// 此常量仅作后备参考；实际限制由注入 [`FdTable::new`] 的
/// [`crate::vfs::limits::VfsLimits`] 决定，不同内核配置可提供不同值。
pub const RLIMIT_NOFILE_DEFAULT: u32 = 1024;

/// 每进程绝对最大打开文件数的默认值（硬限制参考值）。
pub const RLIMIT_NOFILE_MAX: u32 = 4096;

/// 文件描述符编号，零开销 newtype，防止与其他 `u32` 语义混淆。
///
/// 系统调用入口（`arch/` 层）负责将用户空间传入的原始整数 `as u32` 后通过
/// [`Fd::from_raw`] 转换；内核内部统一使用 `Fd` 类型，不使用裸 `u32`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fd(u32);

impl Fd {
    /// 标准输入。
    pub const STDIN: Self = Self(0);
    /// 标准输出。
    pub const STDOUT: Self = Self(1);
    /// 标准错误。
    pub const STDERR: Self = Self(2);

    /// 从原始整数构造 `Fd`（仅供 `arch/` 层 syscall 入口使用）。
    pub const fn from_raw(n: u32) -> Self {
        Self(n)
    }

    /// 返回原始整数（仅供 `arch/` 层 syscall 返回值编码使用）。
    pub const fn as_raw(self) -> u32 {
        self.0
    }
}

/// 文件描述符标志（`fcntl(fd, F_GETFD)` / `F_SETFD` 的操作对象）。
///
/// 注意：与打开选项 [`crate::vfs::file::OpenOptions`] 不同，描述符标志是每个
/// fd 的属性，而打开选项是每个 `File` 对象的属性（被 `dup` 的 fd 共享同一
/// `File` 及其打开选项，但各自有独立的描述符标志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FdFlags(pub(crate) u32);

impl FdFlags {
    /// 执行时关闭（`FD_CLOEXEC`）：`execve` 后自动关闭此描述符。
    ///
    /// 强烈建议对所有非标准 fd（0/1/2 以外）默认设置此标志，防止 execve
    /// 后子程序意外继承内核描述符（可能导致文件泄露或权限升级）。
    pub const CLOEXEC: Self = Self(1);

    pub const fn has(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
    pub const fn without(self, flag: Self) -> Self {
        Self(self.0 & !flag.0)
    }
    /// 返回原始标志位，仅供 ABI 序列化边界使用（如 `arch/` 层的 `fcntl` 返回值编码）。
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// 文件描述符表中的单个条目。
struct FdEntry {
    /// 指向打开文件的共享引用。多个 fd（`dup`/`dup2`）可共享同一 `File`。
    file: Arc<File>,
    /// 该描述符的标志（目前只有 CLOEXEC）。
    flags: FdFlags,
}

struct RemovedFd {
    fd: u32,
    entry: FdEntry,
    last_file_reference: bool,
}

fn notify_fd_closed(fd: u32, entry: &FdEntry) {
    entry.file.on_fd_closed(fd);
}

#[inline]
const fn bitmap_words(limit: u32) -> usize {
    (limit as usize).div_ceil(64)
}

/// 文件描述符表内部数据，整体受自旋锁保护。
///
/// 使用平铺 `Vec<Option<FdEntry>>` 替代 `BTreeMap<u32, FdEntry>`：
/// - `alloc_fd`/`get_file`/`close_fd` 全部 O(1)（数组索引）
/// - 连续内存布局，CPU cache 命中率高
/// - 位图仅用于 O(1) 最小空闲 fd 查找
///
/// `entries` 仍保持懒扩容：即使硬限制较高，真正的 `FdEntry` 存储也只会随已使用 fd
/// 区间增长；位图则按硬限制一次性分配，以便 O(1) 查找最小空闲 fd。
struct FdTableInner {
    /// 平铺数组：entries[fd] = Some(entry) 表示 fd 已打开。
    entries: Vec<Option<FdEntry>>,
    /// 当前已打开的描述符数量（避免遍历计数）。
    count: usize,
    /// 当前软限制：后续 `open`/`dup` 不能分配出 `fd >= limit` 的新描述符。
    limit: u32,
    /// 当前进程允许提升到的硬限制上界。
    hard_limit: u32,
    /// fd 分配位图：第 i 位为 1 表示 fd=i 已被占用。
    bitmap: Vec<u64>,
    /// 观察本 fdtable 中 fd 关闭/替换事件的文件（典型是 epoll fd）。
    close_observers: Vec<Weak<File>>,
}

impl FdTableInner {
    fn new(limit: u32, hard_limit: u32) -> Self {
        Self {
            entries: Vec::new(),
            count: 0,
            limit,
            hard_limit,
            bitmap: alloc::vec![0u64; bitmap_words(hard_limit)],
            close_observers: Vec::new(),
        }
    }

    /// 在位图中标记 fd 为已占用。
    #[inline]
    fn bitmap_set(&mut self, fd: u32) {
        let word = fd as usize / 64;
        let bit = fd as usize % 64;
        if word < self.bitmap.len() {
            self.bitmap[word] |= 1u64 << bit;
        }
    }

    /// 在位图中标记 fd 为空闲。
    #[inline]
    fn bitmap_clear(&mut self, fd: u32) {
        let word = fd as usize / 64;
        let bit = fd as usize % 64;
        if word < self.bitmap.len() {
            self.bitmap[word] &= !(1u64 << bit);
        }
    }

    /// 查找最小空闲 fd（位图中第一个 0 位）。
    #[inline]
    fn bitmap_find_free(&self) -> Option<u32> {
        if self.hard_limit == 0 {
            return None;
        }
        for (i, &word) in self.bitmap.iter().enumerate() {
            let valid_mask = self.valid_bits_mask(i);
            let free_bits = (!word) & valid_mask;
            if free_bits != 0 {
                let bit = free_bits.trailing_zeros();
                let fd = i as u32 * 64 + bit;
                return Some(fd);
            }
        }
        None
    }

    #[inline]
    fn valid_bits_mask(&self, word_idx: usize) -> u64 {
        let start_fd = word_idx * 64;
        let remaining = self.hard_limit.saturating_sub(start_fd as u32) as usize;
        match remaining {
            0 => 0,
            64.. => u64::MAX,
            bits => (1u64 << bits) - 1,
        }
    }

    /// 确保 entries 数组至少能容纳 fd 索引。
    #[inline]
    fn ensure_capacity(&mut self, fd: u32) {
        let needed = fd as usize + 1;
        if self.entries.len() < needed {
            self.entries.resize_with(needed, || None);
        }
    }

    /// O(1) 获取条目引用。
    #[inline]
    fn get(&self, fd: u32) -> Option<&FdEntry> {
        self.entries.get(fd as usize).and_then(|e| e.as_ref())
    }

    /// O(1) 获取条目可变引用。
    #[inline]
    fn get_mut(&mut self, fd: u32) -> Option<&mut FdEntry> {
        self.entries.get_mut(fd as usize).and_then(|e| e.as_mut())
    }

    fn contains_file(&self, file: &Arc<File>) -> bool {
        for (i, &word) in self.bitmap.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let bit = w.trailing_zeros();
                let fd = i as u32 * 64 + bit;
                w &= w - 1;
                if let Some(entry) = self.get(fd)
                    && Arc::ptr_eq(&entry.file, file)
                {
                    return true;
                }
            }
        }
        false
    }

    /// O(1) 插入条目，返回旧条目（若有）。
    #[inline]
    fn insert(&mut self, fd: u32, entry: FdEntry) -> Option<FdEntry> {
        self.ensure_capacity(fd);
        let old = self.entries[fd as usize].take();
        self.entries[fd as usize] = Some(entry);
        self.bitmap_set(fd);
        if old.is_none() {
            self.count += 1;
        }
        old
    }

    /// O(1) 移除条目。
    #[inline]
    fn remove(&mut self, fd: u32) -> Option<FdEntry> {
        let entry = self.entries.get_mut(fd as usize)?.take();
        if entry.is_some() {
            self.bitmap_clear(fd);
            self.count -= 1;
        }
        entry
    }
}

/// 进程文件描述符表。
///
/// 每个进程（或线程组，不使用 `CLONE_FILES` 时）拥有独立的一张 FdTable。
pub struct FdTable {
    inner: Spinlock<FdTableInner>,
}

impl FdTable {
    /// 构造一个空的描述符表，从 `limits` 中读取初始软硬限制。
    pub fn new(limits: &VfsLimits) -> Self {
        Self {
            inner: Spinlock::new(FdTableInner::new(
                core::cmp::min(limits.nofile_default, limits.nofile_max),
                limits.nofile_max,
            )),
        }
    }

    /// 构造一个使用 [`RLIMIT_NOFILE_DEFAULT`] 默认值的描述符表（便于测试）。
    pub fn new_default() -> Self {
        Self {
            inner: Spinlock::new(FdTableInner::new(RLIMIT_NOFILE_DEFAULT, RLIMIT_NOFILE_MAX)),
        }
    }

    /// 分配最小可用 fd，将 `file` 注册为该描述符的对应文件。
    pub fn alloc_fd(&self, file: Arc<File>, flags: FdFlags) -> VfsResult<Fd> {
        let mut inner = self.inner.lock();
        let fd = inner.bitmap_find_free().ok_or(VfsError::TooManyOpenFiles)?;
        if fd >= inner.limit {
            return Err(VfsError::TooManyOpenFiles);
        }
        inner.insert(fd, FdEntry { file, flags });
        Ok(Fd(fd))
    }

    pub fn register_close_observer(&self, file: &Arc<File>) {
        let mut inner = self.inner.lock();
        inner
            .close_observers
            .retain(|weak| weak.upgrade().is_some());
        if inner.close_observers.iter().any(|weak| {
            weak.upgrade()
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, file))
        }) {
            return;
        }
        inner.close_observers.push(Arc::downgrade(file));
    }

    fn notify_fd_closed(&self, removed: &RemovedFd) {
        notify_fd_closed(removed.fd, &removed.entry);
        if !removed.last_file_reference {
            return;
        }
        let observers = {
            let mut inner = self.inner.lock();
            let mut observers = Vec::new();
            inner.close_observers.retain(|weak| {
                if let Some(file) = weak.upgrade() {
                    observers.push(file);
                    true
                } else {
                    false
                }
            });
            observers
        };
        for observer in observers {
            observer.on_file_description_closed(&removed.entry.file);
        }
    }

    /// 在指定 fd 编号上安装 `file`（用于 `dup2`/`dup3`/标准 fd 初始化）。
    pub fn install_fd(&self, fd: Fd, file: Arc<File>, flags: FdFlags) -> VfsResult<()> {
        let fd = fd.0;
        let old = {
            let mut inner = self.inner.lock();
            if fd >= inner.limit {
                return Err(VfsError::BadFileDescriptor);
            }
            let old = inner.insert(fd, FdEntry { file, flags });
            old.map(|entry| RemovedFd {
                fd,
                last_file_reference: !inner.contains_file(&entry.file),
                entry,
            })
        };
        if let Some(old) = old.as_ref() {
            self.notify_fd_closed(old);
        }
        drop(old);
        Ok(())
    }

    /// 关闭指定 fd。
    pub fn close_fd(&self, fd: Fd) -> VfsResult<()> {
        let removed = {
            let mut inner = self.inner.lock();
            let removed = inner.remove(fd.0);
            removed.map(|entry| RemovedFd {
                fd: fd.0,
                last_file_reference: !inner.contains_file(&entry.file),
                entry,
            })
        };
        let Some(removed) = removed else {
            return Err(VfsError::BadFileDescriptor);
        };
        self.notify_fd_closed(&removed);
        drop(removed);
        Ok(())
    }

    /// 获取 fd 对应的 `File` 共享引用（O(1) 数组索引）。
    pub fn get_file(&self, fd: Fd) -> Option<Arc<File>> {
        self.inner.lock().get(fd.0).map(|e| Arc::clone(&e.file))
    }

    /// 复制描述符（`dup`）。
    pub fn dup_fd(&self, old_fd: Fd) -> VfsResult<Fd> {
        self.dup_fd_from(old_fd, 0, FdFlags::default())
    }

    /// 从 `min_fd` 起复制描述符，供 `fcntl(F_DUPFD*)` 使用。
    pub fn dup_fd_from(&self, old_fd: Fd, min_fd: u32, flags: FdFlags) -> VfsResult<Fd> {
        let mut inner = self.inner.lock();
        let file = inner
            .get(old_fd.0)
            .map(|e| Arc::clone(&e.file))
            .ok_or(VfsError::BadFileDescriptor)?;
        let mut new_fd = min_fd;
        while new_fd < inner.limit {
            let word = (new_fd / 64) as usize;
            let bit = new_fd % 64;
            if word >= inner.bitmap.len() || (inner.bitmap[word] & (1u64 << bit)) == 0 {
                break;
            }
            new_fd += 1;
        }
        if new_fd >= inner.limit {
            return Err(VfsError::TooManyOpenFiles);
        }
        inner.insert(new_fd, FdEntry { file, flags });
        Ok(Fd(new_fd))
    }

    /// 复制描述符到指定编号（`dup2`/`dup3`）。
    pub fn dup2_fd(&self, old_fd: Fd, new_fd: Fd, flags: FdFlags) -> VfsResult<Fd> {
        if old_fd == new_fd {
            return if self.inner.lock().get(old_fd.0).is_some() {
                Ok(new_fd)
            } else {
                Err(VfsError::BadFileDescriptor)
            };
        }
        let replaced = {
            let mut inner = self.inner.lock();
            if new_fd.0 >= inner.limit {
                return Err(VfsError::BadFileDescriptor);
            }
            let file = inner
                .get(old_fd.0)
                .map(|e| Arc::clone(&e.file))
                .ok_or(VfsError::BadFileDescriptor)?;
            let old = inner.insert(new_fd.0, FdEntry { file, flags });
            old.map(|entry| RemovedFd {
                fd: new_fd.0,
                last_file_reference: !inner.contains_file(&entry.file),
                entry,
            })
        };
        if let Some(replaced) = replaced.as_ref() {
            self.notify_fd_closed(replaced);
        }
        drop(replaced);
        Ok(new_fd)
    }

    /// 获取描述符级 flags（目前只有 CLOEXEC）。
    pub fn fd_flags(&self, fd: Fd) -> VfsResult<FdFlags> {
        self.inner
            .lock()
            .get(fd.0)
            .map(|e| e.flags)
            .ok_or(VfsError::BadFileDescriptor)
    }

    /// 设置描述符级 flags（目前只保留 CLOEXEC 位）。
    pub fn set_fd_flags(&self, fd: Fd, flags: FdFlags) -> VfsResult<()> {
        let mut inner = self.inner.lock();
        let entry = inner.get_mut(fd.0).ok_or(VfsError::BadFileDescriptor)?;
        entry.flags = flags;
        Ok(())
    }

    /// 获取描述符标志。
    pub fn get_flags(&self, fd: Fd) -> VfsResult<FdFlags> {
        self.inner
            .lock()
            .get(fd.0)
            .map(|e| e.flags)
            .ok_or(VfsError::BadFileDescriptor)
    }

    /// 设置描述符标志。
    pub fn set_flags(&self, fd: Fd, flags: FdFlags) -> VfsResult<()> {
        match self.inner.lock().get_mut(fd.0) {
            Some(e) => {
                e.flags = flags;
                Ok(())
            }
            None => Err(VfsError::BadFileDescriptor),
        }
    }

    /// 关闭所有标记了 `CLOEXEC` 的描述符（`execve` 时调用）。
    ///
    /// 通过位图遍历就地提取，**不在锁内做任何堆分配**。
    /// 提取出的 `FdEntry` 在锁外统一 drop，避免 `File::drop` → `FileOps::release`
    /// 在持锁状态下执行。
    pub fn close_on_exec(&self) {
        // 栈上固定大小缓冲区，避免按硬限制做线性堆分配。
        // 无论进程硬限制多大，每轮都只在锁外批量 drop 最多 64 个条目。
        const BATCH: usize = 64;
        let mut batch_buf: [Option<RemovedFd>; BATCH] = core::array::from_fn(|_| None);

        loop {
            let mut batch_count = 0;
            {
                let mut inner = self.inner.lock();
                // 遍历位图找已占用的 fd
                for word_idx in 0..inner.bitmap.len() {
                    let mut word = inner.bitmap[word_idx];
                    while word != 0 {
                        let bit = word.trailing_zeros();
                        let fd = word_idx as u32 * 64 + bit;
                        word &= word - 1; // 清除最低位

                        let should_remove = inner
                            .get(fd)
                            .is_some_and(|entry| entry.flags.has(FdFlags::CLOEXEC));
                        if should_remove && let Some(removed) = inner.remove(fd) {
                            let last_file_reference = !inner.contains_file(&removed.file);
                            batch_buf[batch_count] = Some(RemovedFd {
                                fd,
                                entry: removed,
                                last_file_reference,
                            });
                            batch_count += 1;
                            if batch_count >= BATCH {
                                break;
                            }
                        }
                    }
                    if batch_count >= BATCH {
                        break;
                    }
                }
            }
            // 锁已释放，安全 drop
            for entry in batch_buf[..batch_count].iter_mut() {
                if let Some(removed) = entry.take() {
                    self.notify_fd_closed(&removed);
                    drop(removed);
                }
            }
            if batch_count < BATCH {
                break; // 没有更多 CLOEXEC fd
            }
        }
    }

    pub fn snapshot_fds(&self) -> Vec<(u32, Arc<File>)> {
        let inner = self.inner.lock();
        let mut out = Vec::with_capacity(inner.count);
        for (i, &word) in inner.bitmap.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let bit = w.trailing_zeros();
                let fd = i as u32 * 64 + bit;
                w &= w - 1;
                if let Some(entry) = inner.get(fd) {
                    out.push((fd, Arc::clone(&entry.file)));
                }
            }
        }
        out
    }

    /// 返回当前打开的描述符数量。
    pub fn len(&self) -> usize {
        self.inner.lock().count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 调整每进程打开文件数软限制。
    ///
    /// 已经打开的 fd 不会因为软限制下调而被关闭；新分配的 fd 只需满足
    /// `fd < limit` 即可。
    pub fn set_limit(&self, new_limit: u32) -> VfsResult<()> {
        let mut inner = self.inner.lock();
        if new_limit > inner.hard_limit {
            return Err(VfsError::OperationNotPermitted);
        }
        inner.limit = new_limit;
        Ok(())
    }

    pub fn close_range(&self, first: u32, last: u32, cloexec_only: bool) {
        if cloexec_only {
            let mut inner = self.inner.lock();
            if first >= inner.hard_limit {
                return;
            }
            let upper = last.min(inner.hard_limit.saturating_sub(1));
            for word_idx in 0..inner.bitmap.len() {
                let word_start = word_idx as u32 * 64;
                if word_start > upper {
                    break;
                }
                let mut word = inner.bitmap[word_idx];
                while word != 0 {
                    let bit = word.trailing_zeros();
                    let fd = word_idx as u32 * 64 + bit;
                    word &= word - 1;
                    if fd < first || fd > upper {
                        continue;
                    }
                    if let Some(entry) = inner.get_mut(fd) {
                        entry.flags = entry.flags.with(FdFlags::CLOEXEC);
                    }
                }
            }
            return;
        }

        const BATCH: usize = 64;
        let mut batch_buf: [Option<RemovedFd>; BATCH] = core::array::from_fn(|_| None);
        loop {
            let mut batch_count = 0;
            {
                let mut inner = self.inner.lock();
                if first >= inner.hard_limit {
                    break;
                }
                let upper = last.min(inner.hard_limit.saturating_sub(1));
                for word_idx in 0..inner.bitmap.len() {
                    let word_start = word_idx as u32 * 64;
                    if word_start > upper {
                        break;
                    }
                    let mut word = inner.bitmap[word_idx];
                    while word != 0 {
                        let bit = word.trailing_zeros();
                        let fd = word_idx as u32 * 64 + bit;
                        word &= word - 1;
                        if fd < first || fd > upper {
                            continue;
                        }
                        if let Some(removed) = inner.remove(fd) {
                            let last_file_reference = !inner.contains_file(&removed.file);
                            batch_buf[batch_count] = Some(RemovedFd {
                                fd,
                                entry: removed,
                                last_file_reference,
                            });
                            batch_count += 1;
                            if batch_count >= BATCH {
                                break;
                            }
                        }
                    }
                    if batch_count >= BATCH {
                        break;
                    }
                }
            }
            for entry in batch_buf[..batch_count].iter_mut() {
                if let Some(removed) = entry.take() {
                    self.notify_fd_closed(&removed);
                    drop(removed);
                }
            }
            if batch_count < BATCH {
                break;
            }
        }
    }

    /// 为 `fork` 创建当前描述符表的深拷贝。
    pub fn fork(&self) -> Self {
        let inner = self.inner.lock();
        let new_entries: Vec<Option<FdEntry>> = inner
            .entries
            .iter()
            .map(|opt| {
                opt.as_ref().map(|e| FdEntry {
                    file: Arc::clone(&e.file),
                    flags: e.flags,
                })
            })
            .collect();
        FdTable {
            inner: Spinlock::new(FdTableInner {
                entries: new_entries,
                count: inner.count,
                limit: inner.limit,
                hard_limit: inner.hard_limit,
                bitmap: inner.bitmap.clone(),
                close_observers: inner.close_observers.clone(),
            }),
        }
    }
}
