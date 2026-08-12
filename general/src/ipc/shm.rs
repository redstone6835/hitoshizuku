//! SysV shared memory 的通用管理器。
//!
//! 本模块不直接处理用户地址、页表或 syscall ABI；它只维护 shm 段的身份、
//! 权限、引用计数和共享 backing object。VM 层可以把 [`ShmObject`] 当成
//! [`mm::FileLike`] 来建立 MAP_SHARED 风格的页缓存。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::cmp::min;
use core::sync::atomic::{AtomicUsize, Ordering};

use errno::Errno;
use mm::FileLike;
use spin::Mutex;
use vfs::cred::{Capability, Credentials, Gid, Uid};
use vfs::stat::FileMode;

/// SysV `IPC_PRIVATE` key。
pub const IPC_PRIVATE: i32 = 0;
/// `shmget` 创建不存在的 key。
pub const IPC_CREAT: u32 = 0o1000;
/// 与 [`IPC_CREAT`] 同用时要求 key 不存在。
pub const IPC_EXCL: u32 = 0o2000;
/// `shmctl` 删除命令。
pub const IPC_RMID: u32 = 0;
/// `shmctl` 元数据更新命令。
pub const IPC_SET: u32 = 1;
/// `shmctl` 元数据查询命令。
pub const IPC_STAT: u32 = 2;
/// `shmctl` 系统限制查询命令。
pub const IPC_INFO: u32 = 3;
/// Linux 兼容 ABI 标志；general 层只集中定义，不解释用户结构布局。
pub const IPC_64: u32 = 0x0100;
/// `shmat` 只读映射。
pub const SHM_RDONLY: u32 = 0o10000;
/// `shmat` 地址按 SHMLBA 向下取整；地址策略由 syscall/VM 层处理。
pub const SHM_RND: u32 = 0o20000;
/// `shmat` 允许替换已有映射；地址策略由 syscall/VM 层处理。
pub const SHM_REMAP: u32 = 0o40000;
/// `shmat` 请求可执行映射。
pub const SHM_EXEC: u32 = 0o100000;

/// sparse backing 的块大小。集中定义，避免读写路径出现裸常量。
pub const SHM_SPARSE_BLOCK_SIZE: usize = 4096;

const FIRST_SHM_ID: i32 = 1;
const SHM_CACHE_KEY_TAG: usize = 1usize << (usize::BITS as usize - 1);

static NEXT_SHM_CACHE_KEY: AtomicUsize = AtomicUsize::new(1);

/// SysV shm id。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShmId(pub i32);

/// SysV shm key。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShmKey(pub i32);

impl ShmKey {
    /// `IPC_PRIVATE` 每次都创建新段，不进入 key 查找表。
    pub const PRIVATE: Self = Self(IPC_PRIVATE);
}

/// shm 段权限元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmPerm {
    pub key: ShmKey,
    pub uid: Uid,
    pub gid: Gid,
    pub cuid: Uid,
    pub cgid: Gid,
    pub mode: FileMode,
}

impl ShmPerm {
    pub fn new(key: ShmKey, mode: FileMode, cred: &Credentials) -> Self {
        let mode = mode.mask(FileMode::PERM_MASK);
        Self {
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            cgid: cred.egid,
            mode,
        }
    }
}

/// shm 管理器限制。默认值集中在这里，逻辑代码只读取该结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmLimits {
    pub min_segment_size: u64,
    pub max_segment_size: u64,
    pub max_segments: usize,
    pub max_total_pages: usize,
}

impl ShmLimits {
    pub const DEFAULT_MIN_SEGMENT_SIZE: u64 = 1;
    pub const DEFAULT_MAX_SEGMENT_SIZE: u64 = 1 << 30;
    pub const DEFAULT_MAX_SEGMENTS: usize = 4096;
    pub const DEFAULT_MAX_TOTAL_PAGES: usize = 1 << 20;

    pub const fn new(
        min_segment_size: u64,
        max_segment_size: u64,
        max_segments: usize,
        max_total_pages: usize,
    ) -> Self {
        Self {
            min_segment_size,
            max_segment_size,
            max_segments,
            max_total_pages,
        }
    }
}

impl Default for ShmLimits {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_MIN_SEGMENT_SIZE,
            Self::DEFAULT_MAX_SEGMENT_SIZE,
            Self::DEFAULT_MAX_SEGMENTS,
            Self::DEFAULT_MAX_TOTAL_PAGES,
        )
    }
}

/// 单个 shm 段的只读快照，供 `IPC_STAT` 或调试路径使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShmMetadata {
    pub id: ShmId,
    pub key: ShmKey,
    pub perm: ShmPerm,
    pub size: u64,
    pub nattch: usize,
    pub marked_for_removal: bool,
    pub atime: i64,
    pub dtime: i64,
    pub ctime: i64,
    pub cpid: i32,
    pub lpid: i32,
}

/// `IPC_SET` 可更新的字段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShmMetadataUpdate {
    pub uid: Option<Uid>,
    pub gid: Option<Gid>,
    pub mode: Option<FileMode>,
}

/// `IPC_INFO` 风格的管理器快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmSystemInfo {
    pub limits: ShmLimits,
    pub used_segments: usize,
    pub total_pages: usize,
}

/// shm 段的共享 backing object。
///
/// sparse block 只在写入时分配；未写过的 hole 在读取时自然补零。
pub struct ShmObject {
    id: ShmId,
    size: u64,
    cache_key: usize,
    state: Weak<Mutex<ShmState>>,
    storage: Mutex<BTreeMap<u64, Box<[u8; SHM_SPARSE_BLOCK_SIZE]>>>,
    mapped_count: AtomicUsize,
}

impl ShmObject {
    pub fn new(size: u64) -> Self {
        Self::new_inner(ShmId(0), size, Weak::new())
    }

    fn managed(id: ShmId, size: u64, state: Weak<Mutex<ShmState>>) -> Self {
        Self::new_inner(id, size, state)
    }

    fn new_inner(id: ShmId, size: u64, state: Weak<Mutex<ShmState>>) -> Self {
        let seq = NEXT_SHM_CACHE_KEY.fetch_add(1, Ordering::Relaxed);
        Self {
            id,
            size,
            cache_key: SHM_CACHE_KEY_TAG | seq,
            state,
            storage: Mutex::new(BTreeMap::new()),
            mapped_count: AtomicUsize::new(0),
        }
    }

    pub fn id(&self) -> ShmId {
        self.id
    }

    pub fn len(&self) -> u64 {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// 当前由对象 hook 观察到的映射数量；manager 中的 nattch 才是 SysV 元数据来源。
    pub fn mapped_count(&self) -> usize {
        self.mapped_count.load(Ordering::Acquire)
    }

    fn record_mapped(&self) -> usize {
        self.mapped_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn record_unmapped(&self) -> usize {
        let mut current = self.mapped_count.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return 0;
            }
            match self.mapped_count.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return current - 1,
                Err(next) => current = next,
            }
        }
    }

    fn account_manager_attach(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut state = state.lock();
        if let Some(entry) = state.by_id.get_mut(&self.id) {
            entry.nattch = entry.nattch.saturating_add(1);
        }
    }

    fn account_manager_detach(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut state = state.lock();
        let remove_now = if let Some(entry) = state.by_id.get_mut(&self.id) {
            entry.nattch = entry.nattch.saturating_sub(1);
            entry.nattch == 0 && entry.marked_for_removal
        } else {
            false
        };
        if remove_now {
            remove_entry_locked(&mut state, self.id);
        }
    }
}

impl FileLike for ShmObject {
    fn cache_key(&self) -> usize {
        self.cache_key
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }

        let readable = min(buf.len() as u64, self.size - offset) as usize;
        buf[..readable].fill(0);

        // sparse hole 默认是零；只需要把已分配块覆盖到输出缓冲区。
        let storage = self.storage.lock();
        let mut copied = 0;
        while copied < readable {
            let file_off = offset + copied as u64;
            let block_index = file_off / SHM_SPARSE_BLOCK_SIZE as u64;
            let block_off = (file_off % SHM_SPARSE_BLOCK_SIZE as u64) as usize;
            let chunk = min(readable - copied, SHM_SPARSE_BLOCK_SIZE - block_off);
            if let Some(block) = storage.get(&block_index) {
                buf[copied..copied + chunk].copy_from_slice(&block[block_off..block_off + chunk]);
            }
            copied += chunk;
        }

        Ok(readable)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, Errno> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }

        let writable = min(buf.len() as u64, self.size - offset) as usize;
        let mut storage = self.storage.lock();
        let mut written = 0;
        while written < writable {
            let file_off = offset + written as u64;
            let block_index = file_off / SHM_SPARSE_BLOCK_SIZE as u64;
            let block_off = (file_off % SHM_SPARSE_BLOCK_SIZE as u64) as usize;
            let chunk = min(writable - written, SHM_SPARSE_BLOCK_SIZE - block_off);
            let block = storage
                .entry(block_index)
                .or_insert_with(|| Box::new([0; SHM_SPARSE_BLOCK_SIZE]));
            block[block_off..block_off + chunk].copy_from_slice(&buf[written..written + chunk]);
            written += chunk;
        }

        Ok(writable)
    }

    fn sync(&self) -> Result<(), Errno> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn on_mapped(&self) {
        self.record_mapped();
        self.account_manager_attach();
    }

    fn on_unmapped(&self) {
        self.record_unmapped();
        self.account_manager_detach();
    }

    fn is_sysv_shm(&self) -> bool {
        self.id.0 > 0
    }

    fn sysv_shm_id(&self) -> Option<i32> {
        (self.id.0 > 0).then_some(self.id.0)
    }
}

struct ShmEntry {
    object: Arc<ShmObject>,
    perm: ShmPerm,
    nattch: usize,
    marked_for_removal: bool,
    pages: usize,
    atime: i64,
    dtime: i64,
    ctime: i64,
    cpid: i32,
    lpid: i32,
}

struct ShmState {
    by_id: BTreeMap<ShmId, ShmEntry>,
    by_key: BTreeMap<ShmKey, ShmId>,
    next_id: i32,
    total_pages: usize,
}

impl ShmState {
    fn new() -> Self {
        Self {
            by_id: BTreeMap::new(),
            by_key: BTreeMap::new(),
            next_id: FIRST_SHM_ID,
            total_pages: 0,
        }
    }
}

/// SysV shm manager。通常由内核全局持有一份。
pub struct ShmManager {
    limits: ShmLimits,
    state: Arc<Mutex<ShmState>>,
}

#[kernel_symbols::export]
impl ShmManager {
    #[kernel_symbols::export(name = "general.ipc.ShmManager.new", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn new(limits: ShmLimits) -> Self {
        Self {
            limits,
            state: Arc::new(Mutex::new(ShmState::new())),
        }
    }

    #[kernel_symbols::export(name = "general.ipc.ShmManager.limits", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC)]
    pub fn limits(&self) -> ShmLimits {
        self.limits
    }

    /// SysV `shmget` 语义：`IPC_PRIVATE` 总是创建新段；普通 key 可查找或创建。
    #[kernel_symbols::export(name = "general.ipc.ShmManager.shmget", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn shmget(
        &self,
        key: ShmKey,
        size: u64,
        flags: u32,
        cred: &Credentials,
    ) -> Result<ShmId, Errno> {
        let mut state = self.state.lock();

        if key != ShmKey::PRIVATE {
            if let Some(id) = state.by_key.get(&key).copied() {
                let entry = state.by_id.get(&id).ok_or(Errno::EINVAL)?;
                if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                    return Err(Errno::EEXIST);
                }
                if size > 0 && size > entry.object.len() {
                    return Err(Errno::EINVAL);
                }
                check_mode_request(cred, &entry.perm, flags)?;
                return Ok(id);
            }

            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
        }

        self.create_locked(&mut state, key, size, mode_from_flags(flags), cred)
    }

    /// 创建一个不进入 key 表的私有段。
    #[kernel_symbols::export(name = "general.ipc.ShmManager.create_private", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn create_private(
        &self,
        size: u64,
        mode: FileMode,
        cred: &Credentials,
    ) -> Result<ShmId, Errno> {
        let mut state = self.state.lock();
        self.create_locked(&mut state, ShmKey::PRIVATE, size, mode, cred)
    }

    /// 校验并取出一次 attach 可映射的 backing。
    ///
    /// 真正的 attach 计数由 VM 在 VMA 成功插入后通过 [`FileLike::on_mapped`]
    /// 提交；如果在 syscall 层提前增加，`map_file` 失败或 `fork` 复制时都会让
    /// `shm_nattch` 偏离真实地址空间状态。
    #[kernel_symbols::export(name = "general.ipc.ShmManager.attach", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
    pub fn attach(
        &self,
        id: ShmId,
        flags: u32,
        cred: &Credentials,
    ) -> Result<Arc<ShmObject>, Errno> {
        let object = {
            let mut state = self.state.lock();
            let entry = state.by_id.get_mut(&id).ok_or(Errno::EINVAL)?;
            if entry.marked_for_removal {
                return Err(Errno::EIDRM);
            }
            check_attach_request(cred, &entry.perm, flags)?;
            Arc::clone(&entry.object)
        };
        Ok(object)
    }

    /// 手工释放一次 attach。正常 VM 路径不应调用它；`munmap`、`shmdt`、fork 和
    /// 进程退出都通过 FileLike hook 自动同步计数。
    #[kernel_symbols::export(name = "general.ipc.ShmManager.detach", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn detach(&self, id: ShmId) -> Result<(), Errno> {
        let remove_now = {
            let mut state = self.state.lock();
            let entry = state.by_id.get_mut(&id).ok_or(Errno::EINVAL)?;
            if entry.nattch == 0 {
                return Err(Errno::EINVAL);
            }
            entry.nattch -= 1;
            entry.nattch == 0 && entry.marked_for_removal
        };
        if remove_now {
            let mut state = self.state.lock();
            remove_entry_locked(&mut state, id);
        }
        Ok(())
    }

    /// 记录 SysV 元数据中的最近 attach 操作信息。
    ///
    /// 这不改变 `nattch`；计数只能由 VM hook 维护，否则 `munmap` 或进程退出这类
    /// 非 `shmdt` 路径会漏减。
    pub fn note_attach(&self, id: ShmId, pid: i32, now_sec: i64) {
        if let Some(entry) = self.state.lock().by_id.get_mut(&id) {
            entry.atime = now_sec;
            entry.lpid = pid;
        }
    }

    /// 记录最近 detach 操作信息。普通 `munmap`/进程退出无法提供 pid，因此这里只由
    /// `shmdt` syscall 路径调用，计数仍由 VM hook 负责。
    pub fn note_detach(&self, id: ShmId, pid: i32, now_sec: i64) {
        if let Some(entry) = self.state.lock().by_id.get_mut(&id) {
            entry.dtime = now_sec;
            entry.lpid = pid;
        }
    }

    pub fn note_create(&self, id: ShmId, pid: i32, now_sec: i64) {
        if let Some(entry) = self.state.lock().by_id.get_mut(&id) {
            entry.cpid = pid;
            entry.ctime = now_sec;
        }
    }

    pub fn note_change(&self, id: ShmId, now_sec: i64) {
        if let Some(entry) = self.state.lock().by_id.get_mut(&id) {
            entry.ctime = now_sec;
        }
    }

    #[kernel_symbols::export(name = "general.ipc.ShmManager.stat", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC)]
    pub fn stat(&self, id: ShmId, cred: &Credentials) -> Result<ShmMetadata, Errno> {
        let state = self.state.lock();
        let entry = state.by_id.get(&id).ok_or(Errno::EINVAL)?;
        if !cred.can_read(entry.perm.uid, entry.perm.gid, entry.perm.mode) {
            return Err(Errno::EACCES);
        }
        Ok(metadata_from_entry(id, entry))
    }

    #[kernel_symbols::export(name = "general.ipc.ShmManager.set", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn set(
        &self,
        id: ShmId,
        update: ShmMetadataUpdate,
        cred: &Credentials,
    ) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let entry = state.by_id.get_mut(&id).ok_or(Errno::EINVAL)?;
        check_control_owner(cred, &entry.perm)?;
        if let Some(uid) = update.uid {
            entry.perm.uid = uid;
        }
        if let Some(gid) = update.gid {
            entry.perm.gid = gid;
        }
        if let Some(mode) = update.mode {
            entry.perm.mode = mode.mask(FileMode::PERM_MASK);
        }
        Ok(())
    }

    /// `IPC_RMID`：先从 key 表摘除；有 attach 时延迟到最后一次 detach 再释放。
    #[kernel_symbols::export(name = "general.ipc.ShmManager.remove", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
    pub fn remove(&self, id: ShmId, cred: &Credentials) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let (key, remove_now) = {
            let entry = state.by_id.get_mut(&id).ok_or(Errno::EINVAL)?;
            check_control_owner(cred, &entry.perm)?;
            let remove_now = entry.nattch == 0;
            if !remove_now {
                entry.marked_for_removal = true;
            }
            (entry.perm.key, remove_now)
        };

        state.by_key.remove(&key);
        if remove_now {
            remove_entry_locked(&mut state, id);
        }
        Ok(())
    }

    #[kernel_symbols::export(name = "general.ipc.ShmManager.info", contract = "kernel.ipc.sysv-shm@1", version = 1, capabilities = kernel_symbols::capability::IPC, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_DIAGNOSTIC)]
    pub fn info(&self) -> ShmSystemInfo {
        let state = self.state.lock();
        ShmSystemInfo {
            limits: self.limits,
            used_segments: state.by_id.len(),
            total_pages: state.total_pages,
        }
    }

    fn create_locked(
        &self,
        state: &mut ShmState,
        key: ShmKey,
        size: u64,
        mode: FileMode,
        cred: &Credentials,
    ) -> Result<ShmId, Errno> {
        validate_size(size, self.limits)?;
        if state.by_id.len() >= self.limits.max_segments {
            return Err(Errno::ENOSPC);
        }

        let pages = pages_for(size);
        let total_pages = state.total_pages.checked_add(pages).ok_or(Errno::ENOSPC)?;
        if total_pages > self.limits.max_total_pages {
            return Err(Errno::ENOSPC);
        }

        let id = allocate_id(state, self.limits.max_segments)?;
        let object = Arc::new(ShmObject::managed(id, size, Arc::downgrade(&self.state)));
        let perm = ShmPerm::new(key, mode, cred);
        state.by_id.insert(
            id,
            ShmEntry {
                object,
                perm,
                nattch: 0,
                marked_for_removal: false,
                pages,
                atime: 0,
                dtime: 0,
                ctime: 0,
                cpid: 0,
                lpid: 0,
            },
        );
        if key != ShmKey::PRIVATE {
            state.by_key.insert(key, id);
        }
        state.total_pages = total_pages;
        Ok(id)
    }
}

impl Default for ShmManager {
    fn default() -> Self {
        Self::new(ShmLimits::default())
    }
}

fn validate_size(size: u64, limits: ShmLimits) -> Result<(), Errno> {
    if size < limits.min_segment_size || size > limits.max_segment_size {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn pages_for(size: u64) -> usize {
    size.div_ceil(SHM_SPARSE_BLOCK_SIZE as u64) as usize
}

fn allocate_id(state: &mut ShmState, max_segments: usize) -> Result<ShmId, Errno> {
    for _ in 0..max_segments {
        let raw = state.next_id;
        state.next_id = if state.next_id == i32::MAX {
            FIRST_SHM_ID
        } else {
            state.next_id + 1
        };
        let id = ShmId(raw);
        if !state.by_id.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(Errno::ENOSPC)
}

fn remove_entry_locked(state: &mut ShmState, id: ShmId) {
    if let Some(entry) = state.by_id.remove(&id) {
        state.total_pages = state.total_pages.saturating_sub(entry.pages);
        state.by_key.remove(&entry.perm.key);
    }
}

fn metadata_from_entry(id: ShmId, entry: &ShmEntry) -> ShmMetadata {
    ShmMetadata {
        id,
        key: entry.perm.key,
        perm: entry.perm,
        size: entry.object.len(),
        nattch: entry.nattch,
        marked_for_removal: entry.marked_for_removal,
        atime: entry.atime,
        dtime: entry.dtime,
        ctime: entry.ctime,
        cpid: entry.cpid,
        lpid: entry.lpid,
    }
}

fn mode_from_flags(flags: u32) -> FileMode {
    FileMode::new((flags as u16) & FileMode::PERM_MASK.bits())
}

fn check_mode_request(cred: &Credentials, perm: &ShmPerm, flags: u32) -> Result<(), Errno> {
    let requested = mode_from_flags(flags);
    if requested.has_any(FileMode::IRUSR.with(FileMode::IRGRP).with(FileMode::IROTH))
        && !cred.can_read(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    if requested.has_any(FileMode::IWUSR.with(FileMode::IWGRP).with(FileMode::IWOTH))
        && !cred.can_write(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    if requested.has_any(FileMode::IXUSR.with(FileMode::IXGRP).with(FileMode::IXOTH))
        && !cred.can_exec(perm.uid, perm.gid, perm.mode, false)
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

fn check_attach_request(cred: &Credentials, perm: &ShmPerm, flags: u32) -> Result<(), Errno> {
    if !cred.can_read(perm.uid, perm.gid, perm.mode) {
        return Err(Errno::EACCES);
    }
    if flags & SHM_RDONLY == 0 && !cred.can_write(perm.uid, perm.gid, perm.mode) {
        return Err(Errno::EACCES);
    }
    if flags & SHM_EXEC != 0 && !cred.can_exec(perm.uid, perm.gid, perm.mode, false) {
        return Err(Errno::EACCES);
    }
    Ok(())
}

fn check_control_owner(cred: &Credentials, perm: &ShmPerm) -> Result<(), Errno> {
    if cred.is_owner(perm.uid) || cred.is_owner(perm.cuid) || cred.has_cap(Capability::SysAdmin) {
        return Ok(());
    }
    Err(Errno::EPERM)
}
