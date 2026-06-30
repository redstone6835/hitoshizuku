//! VmSpace —— 进程地址空间的顶层对象。
//!
//! `VmSpace` 负责把纯 VMA 代数、用户页表 ops、用户数据页生命周期三件事收束在
//! general 层。arch 只提供页表机械动作，COW / `MAP_SHARED` / 脏页写回这些策略
//! 都在这里处理，避免未来把 MM 逻辑散到具体架构里。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use errno::Errno;
use mm::{FileLike, VmArea, VmBacking, VmFlags, VmaSet};

use crate::mm::fault::{FaultKind, FaultOutcome, KernelFaultReason};
use crate::mm::ops::{PgdHandle, UserVmLayoutOps, user_pgd_ops, user_vm_layout};

#[inline]
fn vm_layout() -> &'static UserVmLayoutOps {
    user_vm_layout().expect("[mm] user_vm_layout_ops not registered")
}

/// 当前架构注入的用户页粒度。
#[inline]
pub fn page_size() -> usize {
    vm_layout().page_size
}

#[inline]
fn page_base(addr: usize) -> usize {
    let page_size = page_size();
    addr & !(page_size - 1)
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn covered_len(areas: &[VmArea], range: &Range<usize>) -> usize {
    let mut cursor = range.start;
    let mut total = 0usize;
    for area in areas {
        if area.range.start > cursor {
            break;
        }
        let end = area.range.end.min(range.end);
        if end > cursor {
            total += end - cursor;
            cursor = end;
        }
        if cursor >= range.end {
            break;
        }
    }
    total
}

static SHARED_FILE_PAGES: spin::Mutex<BTreeMap<SharedFilePageKey, Weak<ResidentPage>>> =
    spin::Mutex::new(BTreeMap::new());
static SHARED_ANON_PAGES: spin::Mutex<BTreeMap<SharedAnonPageKey, Weak<ResidentPage>>> =
    spin::Mutex::new(BTreeMap::new());
static NEXT_SHARED_ANON_ID: AtomicUsize = AtomicUsize::new(1);
static VM_SPACE_LIVE: AtomicUsize = AtomicUsize::new(0);
static VM_SPACE_CREATED: AtomicUsize = AtomicUsize::new(0);
static VM_SPACE_DROPPED: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, Default)]
pub struct VmSpaceDiag {
    pub live: usize,
    pub created: usize,
    pub dropped: usize,
}

pub fn vm_space_diag() -> VmSpaceDiag {
    VmSpaceDiag {
        live: VM_SPACE_LIVE.load(Ordering::Acquire),
        created: VM_SPACE_CREATED.load(Ordering::Acquire),
        dropped: VM_SPACE_DROPPED.load(Ordering::Acquire),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SharedFilePageKey {
    file_key: usize,
    offset: u64,
}

impl SharedFilePageKey {
    fn new(file: &Arc<dyn FileLike>, offset: u64) -> Self {
        Self {
            file_key: file.cache_key(),
            offset,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SharedAnonPageKey {
    id: usize,
    offset: u64,
}

/// futex 等用户态同步原语使用的稳定地址 key。
///
/// 私有 futex 绑定到当前地址空间；共享 futex 绑定到底层 shared backing，
/// 这样同一文件页或同一 shared-anon 页在不同进程中的不同 VA 也能互相唤醒。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VmFutexKey {
    Private {
        vm: usize,
        page: usize,
        offset: u16,
    },
    SharedFile {
        file_key: usize,
        offset: u64,
        word_offset: u16,
    },
    SharedAnon {
        id: usize,
        offset: u64,
        word_offset: u16,
    },
    Direct {
        paddr: usize,
        word_offset: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageAccess {
    ReadOnly,
    Writable,
    Cow,
    SharedTracked,
}

impl PageAccess {
    fn pte_writable(self) -> bool {
        matches!(self, Self::Writable)
    }
}

#[derive(Clone)]
struct PageMapping {
    page: Arc<ResidentPage>,
    access: PageAccess,
}

enum ResidentPageKind {
    Anon,
    SharedAnon,
    PrivateFile,
    SharedFile {
        file: Arc<dyn FileLike>,
        offset: u64,
    },
    Direct,
}

struct ResidentPage {
    paddr: usize,
    kind: ResidentPageKind,
    dirty: AtomicBool,
}

impl ResidentPage {
    fn new_anon(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::Anon,
            dirty: AtomicBool::new(false),
        })
    }

    fn new_shared_anon(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::SharedAnon,
            dirty: AtomicBool::new(false),
        })
    }

    fn new_private_file(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::PrivateFile,
            dirty: AtomicBool::new(false),
        })
    }

    fn new_shared_file(paddr: usize, file: Arc<dyn FileLike>, offset: u64) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::SharedFile { file, offset },
            dirty: AtomicBool::new(false),
        })
    }

    fn new_direct(paddr: usize) -> Arc<Self> {
        Arc::new(Self {
            paddr,
            kind: ResidentPageKind::Direct,
            dirty: AtomicBool::new(false),
        })
    }

    fn paddr(&self) -> usize {
        self.paddr
    }

    fn is_direct(&self) -> bool {
        matches!(self.kind, ResidentPageKind::Direct)
    }

    fn is_shared_anon(&self) -> bool {
        matches!(self.kind, ResidentPageKind::SharedAnon)
    }

    fn is_sysv_shm(&self) -> bool {
        matches!(&self.kind, ResidentPageKind::SharedFile { file, .. } if file.is_sysv_shm())
    }

    fn is_direct_shared_writable(&self) -> bool {
        self.is_direct() || self.is_shared_anon() || self.is_sysv_shm()
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    fn flush_to_backing(&self) -> Result<(), Errno> {
        if !self.dirty.load(Ordering::Acquire) {
            return Ok(());
        }
        let ResidentPageKind::SharedFile { file, offset } = &self.kind else {
            return Ok(());
        };
        let file_size = file.size();
        if *offset >= file_size {
            return Ok(());
        }
        let page_size = page_size();
        let len = (file_size - *offset).min(page_size as u64) as usize;
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let buf = unsafe { core::slice::from_raw_parts(virt(self.paddr) as *const u8, len) };
        let mut written = 0usize;
        while written < len {
            let n = file.write_at(*offset + written as u64, &buf[written..])?;
            if n == 0 {
                return Err(Errno::EIO);
            }
            written += n;
        }
        file.sync()
    }
}

impl Drop for ResidentPage {
    fn drop(&mut self) {
        if let Err(err) = self.flush_to_backing() {
            log::error!(
                "[mm] failed to flush shared mmap page paddr={:#x}: {:?}",
                self.paddr,
                err
            );
        }
        if !matches!(self.kind, ResidentPageKind::Direct) {
            free_user_page(self.paddr);
        }
    }
}

/// 进程地址空间。
pub struct VmSpace {
    vmas: spin::Mutex<VmaSet>,
    pages: spin::Mutex<BTreeMap<usize, PageMapping>>,
    pgd: PgdHandle,
    brk_start: AtomicUsize,
    brk_current: AtomicUsize,
    mmap_next: AtomicUsize,
    mlock_future: AtomicBool,
    /// 诊断辅助：记录当前已建立页表映射的用户页数。
    mapped_pages: AtomicUsize,
}

// Safety: PgdHandle 是 arch opaque 句柄；VMA 与 resident page map 均由锁保护。
unsafe impl Send for VmSpace {}
unsafe impl Sync for VmSpace {}

impl VmSpace {
    /// 新建一个空地址空间。必须在 `register_user_pgd` 完成之后调用。
    pub fn new() -> Self {
        let ops = user_pgd_ops().expect("[mm] user_pgd_ops not registered");
        let layout = vm_layout();
        let pgd = (ops.new_pgd_for_user)();
        VM_SPACE_CREATED.fetch_add(1, Ordering::Relaxed);
        VM_SPACE_LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            vmas: spin::Mutex::new(VmaSet::new()),
            pages: spin::Mutex::new(BTreeMap::new()),
            pgd,
            brk_start: AtomicUsize::new(layout.user_heap_base),
            brk_current: AtomicUsize::new(layout.user_heap_base),
            mmap_next: AtomicUsize::new(layout.user_mmap_base),
            mlock_future: AtomicBool::new(false),
            mapped_pages: AtomicUsize::new(0),
        }
    }

    pub fn pgd(&self) -> PgdHandle {
        self.pgd
    }

    pub fn mapped_pages(&self) -> usize {
        self.mapped_pages.load(Ordering::Acquire)
    }

    fn with_future_mlock(&self, flags: VmFlags) -> VmFlags {
        if self.mlock_future.load(Ordering::Acquire) {
            flags.with(VmFlags::LOCKED)
        } else {
            flags
        }
    }

    pub fn current_brk(&self) -> usize {
        self.brk_current.load(Ordering::Acquire)
    }

    /// ELF loader 装载完成后调用：将 brk 起点对齐到主程序数据段末尾。
    pub fn init_brk_after_load(&self, max_segment_end: usize) {
        let page_size = page_size();
        let new_brk = align_up(max_segment_end, page_size).unwrap_or(max_segment_end);
        let brk = new_brk.max(self.brk_start.load(Ordering::Relaxed));
        self.brk_start.store(brk, Ordering::Release);
        self.brk_current.store(brk, Ordering::Release);
    }

    /// 对 PIE 主程序使用的 brk 初始化。
    ///
    /// `user_heap_base` 是架构选择的独立 brk 区域。低地址 PIE 可以自然落在 heap
    /// base 之前；高地址 PIE 则必须把 brk 起点整体放到主程序段之后，不能只更新
    /// current，否则后续 brk shrink 会跨过一大段非 heap 区间。
    pub fn init_brk_after_pie_load(&self, max_segment_end: usize) {
        let page_size = page_size();
        let new_brk = align_up(max_segment_end, page_size).unwrap_or(max_segment_end);
        let brk = new_brk.max(self.brk_start.load(Ordering::Relaxed));
        self.brk_start.store(brk, Ordering::Release);
        self.brk_current.store(brk, Ordering::Release);
    }

    pub fn set_brk(&self, requested: usize) -> usize {
        if requested == 0 {
            return self.current_brk();
        }
        let brk_start = self.brk_start.load(Ordering::Relaxed);
        if requested < brk_start {
            return self.current_brk();
        }

        let old = self.current_brk();
        let page_size = page_size();
        let old_end = align_up(old, page_size).unwrap_or(old);
        let new_end = match align_up(requested, page_size) {
            Some(v) => v,
            None => return old,
        };

        let result = if new_end > old_end {
            self.map_anon(
                old_end..new_end,
                VmFlags::EMPTY
                    .with(VmFlags::READ)
                    .with(VmFlags::WRITE)
                    .with(VmFlags::USER),
            )
        } else if new_end < old_end {
            self.unmap(new_end..old_end)
        } else {
            Ok(())
        };

        if result.is_ok() {
            self.brk_current.store(requested, Ordering::Release);
            requested
        } else {
            old
        }
    }

    pub fn alloc_mmap_range(&self, len: usize) -> Result<Range<usize>, Errno> {
        let layout = vm_layout();
        let page_size = layout.page_size;
        let len = align_up(len, page_size).ok_or(Errno::EINVAL)?;
        if len == 0 {
            return Err(Errno::EINVAL);
        }

        let cursor = align_up(self.mmap_next.load(Ordering::Acquire), page_size)
            .unwrap_or(layout.user_mmap_base)
            .clamp(layout.user_mmap_base, layout.user_mmap_limit);
        let set = self.vmas.lock();
        let candidates = [
            (layout.user_mmap_base, cursor),
            (cursor, layout.user_mmap_limit),
        ];
        for (start, end) in candidates {
            if start >= end {
                continue;
            }
            if let Some(range) = set.find_gap(start..end, len) {
                self.mmap_next.store(range.end, Ordering::Release);
                return Ok(range);
            }
        }
        Err(Errno::ENOMEM)
    }

    pub fn is_range_free(&self, range: Range<usize>) -> bool {
        self.validate_range(&range).is_ok() && self.vmas.lock().is_range_free(&range)
    }

    /// 检查一段用户地址是否被可读用户 VMA 连续覆盖。
    ///
    /// 这个接口不触发缺页，也不承诺页表页已经常驻；它只用于 syscall 在访问用户
    /// 指针前做快速结构性校验，避免退出清理这类不可失败路径卡在明显损坏的链表上。
    pub fn is_user_range_readable(&self, addr: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let range = addr..end;
        let set = self.vmas.lock();
        let mut cursor = range.start;
        for area in set.iter_overlap(&range) {
            if area.range.start > cursor {
                return false;
            }
            if !area.flags.contains_all(VmFlags::USER | VmFlags::READ) {
                return false;
            }
            cursor = cursor.max(area.range.end.min(range.end));
            if cursor >= range.end {
                return true;
            }
        }
        false
    }

    /// 按 `shmdt` 的入口地址查找一整段 SysV shm 映射。
    ///
    /// SysV shm 通过普通 file-backed VMA 接入 VM，因此这里不引入新的 backing
    /// 枚举；只要求底层 [`FileLike`] 暴露 shm id。`mprotect` 可能把同一段映射
    /// 分裂成多个相邻 VMA，所以检查时按文件 offset 把整段重新拼起来，避免把
    /// 其他文件或后来复用的地址误当成可 detach 的 shm。
    pub fn sysv_shm_mapping_at(&self, addr: usize) -> Option<(Range<usize>, i32)> {
        let set = self.vmas.lock();
        let first = set.find(addr)?;
        if first.range.start != addr {
            return None;
        }
        let VmBacking::File { file, offset } = &first.backing else {
            return None;
        };
        if *offset != 0 {
            return None;
        }
        let shmid = file.sysv_shm_id()?;
        let file_size = file.size();
        if file_size == 0 || file_size > usize::MAX as u64 {
            return None;
        }
        let len = align_up(file_size as usize, page_size())?;
        let end = addr.checked_add(len)?;
        let range = addr..end;
        if !set.contains_range(&range) {
            return None;
        }

        let mut cursor = range.start;
        for area in set.iter_overlap(&range) {
            if area.range.start > cursor {
                return None;
            }
            let VmBacking::File {
                file: area_file,
                offset: area_offset,
            } = &area.backing
            else {
                return None;
            };
            if area_file.sysv_shm_id() != Some(shmid) {
                return None;
            }
            let expected_offset = (area.range.start - range.start) as u64;
            if *area_offset != expected_offset {
                return None;
            }
            cursor = cursor.max(area.range.end.min(range.end));
            if cursor >= range.end {
                return Some((range.clone(), shmid));
            }
        }
        None
    }

    /// 根据用户地址生成 futex key。
    ///
    /// `private` 对应 `FUTEX_PRIVATE_FLAG`。未带 private flag 时，也只有真正
    /// `MAP_SHARED`/direct shared backing 才生成跨地址空间 key；普通 private
    /// VMA 仍按本地址空间隔离，避免不同进程相同 VA 错误互唤醒。
    pub fn futex_key_for(&self, uaddr: usize, private: bool) -> Result<VmFutexKey, Errno> {
        if uaddr % 4 != 0 {
            return Err(Errno::EINVAL);
        }
        let page = page_base(uaddr);
        let word_offset = u16::try_from(uaddr - page).map_err(|_| Errno::EINVAL)?;
        if private {
            return Ok(VmFutexKey::Private {
                vm: self as *const Self as usize,
                page,
                offset: word_offset,
            });
        }

        let set = self.vmas.lock();
        let area = set.find(uaddr).ok_or(Errno::EFAULT)?;
        if !area.flags.has(VmFlags::SHARED) && !matches!(area.backing, VmBacking::Direct(_)) {
            return Ok(VmFutexKey::Private {
                vm: self as *const Self as usize,
                page,
                offset: word_offset,
            });
        }
        let page_delta = page.checked_sub(area.range.start).ok_or(Errno::EFAULT)?;
        match &area.backing {
            VmBacking::File { file, offset } => Ok(VmFutexKey::SharedFile {
                file_key: file.cache_key(),
                offset: offset
                    .checked_add(u64::try_from(page_delta).map_err(|_| Errno::EINVAL)?)
                    .ok_or(Errno::EINVAL)?,
                word_offset,
            }),
            VmBacking::SharedAnon { id, offset } => Ok(VmFutexKey::SharedAnon {
                id: *id,
                offset: offset
                    .checked_add(u64::try_from(page_delta).map_err(|_| Errno::EINVAL)?)
                    .ok_or(Errno::EINVAL)?,
                word_offset,
            }),
            VmBacking::Direct(base) => Ok(VmFutexKey::Direct {
                paddr: base.checked_add(page_delta).ok_or(Errno::EINVAL)?,
                word_offset,
            }),
            VmBacking::Anon => Ok(VmFutexKey::Private {
                vm: self as *const Self as usize,
                page,
                offset: word_offset,
            }),
        }
    }

    /// 注册一段匿名 VMA。不立即分配物理页。
    pub fn map_anon(&self, range: Range<usize>, flags: VmFlags) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let backing = if flags.has(VmFlags::SHARED) {
            VmBacking::SharedAnon {
                id: NEXT_SHARED_ANON_ID.fetch_add(1, Ordering::Relaxed),
                offset: 0,
            }
        } else {
            VmBacking::Anon
        };
        let area = VmArea {
            range,
            flags: flags.with(VmFlags::ANON),
            backing,
        };
        self.vmas.lock().insert(area)
    }

    /// 注册一段 file-backed VMA。缺页时按 offset + (addr - range.start) 读文件。
    pub fn map_file(
        &self,
        range: Range<usize>,
        file: Arc<dyn FileLike>,
        offset: u64,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let mapped_file = Arc::clone(&file);
        let area = VmArea {
            range,
            flags,
            backing: VmBacking::File { file, offset },
        };
        self.vmas.lock().insert(area)?;
        mapped_file.on_mapped();
        Ok(())
    }

    /// MAP_FIXED 原子操作：在同一把 VMA 锁内先 unmap 再 insert，消除竞态窗口。
    pub fn map_fixed_anon(&self, range: Range<usize>, flags: VmFlags) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let backing = if flags.has(VmFlags::SHARED) {
            VmBacking::SharedAnon {
                id: NEXT_SHARED_ANON_ID.fetch_add(1, Ordering::Relaxed),
                offset: 0,
            }
        } else {
            VmBacking::Anon
        };
        let area = VmArea {
            range: range.clone(),
            flags: flags.with(VmFlags::ANON),
            backing,
        };
        let removed_areas = {
            let mut vmas = self.vmas.lock();
            let removed_areas = vmas.unmap_range(&range);
            if let Err(err) = vmas.insert(area) {
                drop(vmas);
                Self::notify_file_unmapped(&removed_areas);
                return Err(err);
            }
            removed_areas
        };
        Self::notify_file_unmapped(&removed_areas);
        let removed = self.remove_page_mappings(range.clone());
        for (va, _mapping) in &removed {
            let _ = self.unmap_page(*va);
        }
        Ok(())
    }

    pub fn map_fixed_file(
        &self,
        range: Range<usize>,
        file: Arc<dyn FileLike>,
        offset: u64,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let flags = self.with_future_mlock(flags);
        let mapped_file = Arc::clone(&file);
        let area = VmArea {
            range: range.clone(),
            flags,
            backing: VmBacking::File { file, offset },
        };
        let removed_areas = {
            let mut vmas = self.vmas.lock();
            let removed_areas = vmas.unmap_range(&range);
            if let Err(err) = vmas.insert(area) {
                drop(vmas);
                Self::notify_file_unmapped(&removed_areas);
                return Err(err);
            }
            removed_areas
        };
        Self::notify_file_unmapped(&removed_areas);
        mapped_file.on_mapped();
        let removed = self.remove_page_mappings(range.clone());
        for (va, _mapping) in &removed {
            let _ = self.unmap_page(*va);
        }
        Ok(())
    }

    /// 注册并立即建立一段 direct physical mapping。
    pub fn map_direct(
        &self,
        range: Range<usize>,
        paddr: usize,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let page_size = page_size();
        if paddr % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let area_flags = self.with_future_mlock(flags).with(VmFlags::USER);
        let area = VmArea {
            range: range.clone(),
            flags: area_flags,
            backing: VmBacking::Direct(paddr),
        };
        self.vmas.lock().insert(area)?;

        let mut pages = self.pages.lock();
        let mut va = range.start;
        while va < range.end {
            let off = va - range.start;
            let page = ResidentPage::new_direct(paddr + off);
            let access = access_for_new_page(area_flags, &page);
            self.map_page(va, page.paddr(), pte_flags_for(area_flags, access))?;
            pages.insert(va, PageMapping { page, access });
            va += page_size;
        }
        self.mapped_pages.store(pages.len(), Ordering::Release);
        Ok(())
    }

    /// 取消映射。同时把已 commit 的页表项摘掉；物理页由 resident page refcount 回收。
    pub fn unmap(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let removed_areas = self.vmas.lock().unmap_range(&range);
        Self::notify_file_unmapped(&removed_areas);
        let removed = self.remove_page_mappings(range);
        for (va, _mapping) in &removed {
            self.unmap_page(*va)?;
        }
        drop(removed);
        Ok(())
    }

    /// 调整一段既有映射的大小或位置。
    ///
    /// 这是 `mremap(2)` 的核心实现：VMA 元数据迁移与页表迁移在这里保持一致。
    /// 不支持 `DONTUNMAP` 的双映射语义，因为那需要额外的 resident page 所有权
    /// 标记；普通 shrink / in-place grow / move / fixed move 都在此闭环。
    pub fn mremap(
        &self,
        old_range: Range<usize>,
        new_len: usize,
        may_move: bool,
        fixed_addr: Option<usize>,
    ) -> Result<usize, Errno> {
        self.validate_range(&old_range)?;
        let page_size = page_size();
        if new_len == 0 || new_len % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        let old_len = old_range.end - old_range.start;
        if new_len <= old_len {
            if new_len < old_len {
                self.unmap(old_range.start + new_len..old_range.end)?;
            }
            return Ok(old_range.start);
        }

        let in_place_end = old_range.start.checked_add(new_len).ok_or(Errno::EINVAL)?;
        let in_place_tail = old_range.end..in_place_end;
        if fixed_addr == Some(old_range.start) {
            return if self.extend_mapping_in_place(&old_range, &in_place_tail)? {
                Ok(old_range.start)
            } else {
                Err(Errno::ENOMEM)
            };
        }
        if fixed_addr.is_none() && self.extend_mapping_in_place(&old_range, &in_place_tail)? {
            return Ok(old_range.start);
        }
        if !may_move && fixed_addr.is_none() {
            return Err(Errno::ENOMEM);
        }

        let new_start = if let Some(addr) = fixed_addr {
            addr
        } else {
            self.alloc_mmap_range(new_len)?.start
        };
        let new_end = new_start.checked_add(new_len).ok_or(Errno::EINVAL)?;
        let new_range = new_start..new_end;
        self.validate_range(&new_range)?;
        if ranges_overlap(&old_range, &new_range) && new_range.start != old_range.start {
            return Err(Errno::EINVAL);
        }

        let (removed_target, mapped_tail) = {
            let mut vmas = self.vmas.lock();
            if !vmas.contains_range(&old_range) {
                return Err(Errno::ENOMEM);
            }
            let removed_target = if fixed_addr.is_some() {
                vmas.unmap_range(&new_range)
            } else {
                if !vmas.is_range_free(&new_range) {
                    return Err(Errno::EEXIST);
                }
                Vec::new()
            };
            let old_pieces = vmas.unmap_range(&old_range);
            let old_covered = covered_len(&old_pieces, &old_range);
            if old_covered != old_len {
                return Err(Errno::ENOMEM);
            }

            let mut cursor = new_range.start;
            let mut last_inserted = None;
            for mut area in old_pieces {
                let len = area.range.end - area.range.start;
                area.range = cursor..cursor + len;
                cursor += len;
                last_inserted = Some(area.clone());
                vmas.insert(area)?;
            }

            let mapped_tail = if cursor < new_range.end {
                let last = last_inserted.ok_or(Errno::ENOMEM)?;
                let last_len = last.range.end - last.range.start;
                let backing = last.backing.checked_shift(last_len).ok_or(Errno::EINVAL)?;
                let tail = VmArea {
                    range: cursor..new_range.end,
                    flags: last.flags,
                    backing,
                };
                let files = Self::collect_file_backings(core::iter::once(&tail));
                vmas.insert(tail)?;
                files
            } else {
                Vec::new()
            };
            (removed_target, mapped_tail)
        };
        Self::notify_file_unmapped(&removed_target);
        Self::notify_files_mapped(mapped_tail);

        let removed_pages = self.remove_page_mappings(new_range.clone());
        for (va, _mapping) in &removed_pages {
            self.unmap_page(*va)?;
        }
        drop(removed_pages);
        self.move_page_mappings(old_range.start, new_range.start, old_len)?;
        self.mmap_next.store(new_range.end, Ordering::Release);
        Ok(new_range.start)
    }

    /// 修改权限。要求整个 range 已被 VMA 连续覆盖。
    pub fn mprotect(&self, range: Range<usize>, new_flags: VmFlags) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let mut set = self.vmas.lock();
        if !set.contains_range(&range) {
            return Err(Errno::ENOMEM);
        }
        set.protect_range(&range, new_flags.with(VmFlags::USER));

        let mut pages = self.pages.lock();
        // mprotect 会被动态链接器和 lmbench mmap/munmap 小测频繁调用。
        // range 已按页对齐，直接逐页探测现有映射，避免先收集 key 到 Vec。
        let page_size = page_size();
        let mut va = range.start;
        while va < range.end {
            let Some(area) = set.find(va) else {
                va += page_size;
                continue;
            };
            let Some(mapping) = pages.get_mut(&va) else {
                va += page_size;
                continue;
            };
            mapping.access = access_for_existing_page(area.flags, &mapping.page);
            self.protect_page(va, pte_flags_for(area.flags, mapping.access))?;
            va += page_size;
        }
        Ok(())
    }

    pub fn resident_bitmap(&self, range: Range<usize>) -> Result<Vec<u8>, Errno> {
        self.validate_range(&range)?;
        let page_size = page_size();
        let page_count = (range.end - range.start) / page_size;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let pages = self.pages.lock();
        let mut out = Vec::with_capacity(page_count);
        let mut va = range.start;
        while va < range.end {
            out.push(if pages.contains_key(&va) { 1 } else { 0 });
            va += page_size;
        }
        Ok(out)
    }

    /// 校验一段用户 VMA 是否连续存在，不触发缺页也不改变页表状态。
    pub fn contains_user_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let set = self.vmas.lock();
        if !set.contains_range(&range) {
            return Err(Errno::ENOMEM);
        }
        Ok(())
    }

    /// 丢弃指定范围内已经常驻的页，保留 VMA 语义供后续缺页按 backing 重建。
    pub fn discard_resident_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.contains_user_range(range.clone())?;
        let removed = self.remove_page_mappings(range);
        for (va, _mapping) in &removed {
            self.unmap_page(*va)?;
        }
        Ok(())
    }

    pub fn sync_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.validate_range(&range)?;
        {
            let set = self.vmas.lock();
            if !set.contains_range(&range) {
                return Err(Errno::ENOMEM);
            }
        }
        let pages: Vec<Arc<ResidentPage>> = {
            let pages = self.pages.lock();
            pages
                .range(range)
                .map(|(_va, mapping)| Arc::clone(&mapping.page))
                .collect()
        };
        for page in pages {
            page.flush_to_backing()?;
        }
        Ok(())
    }

    pub fn mlock_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.update_locked_range(range, true)
    }

    pub fn munlock_range(&self, range: Range<usize>) -> Result<(), Errno> {
        self.update_locked_range(range, false)
    }

    pub fn mlock_all_current(&self) {
        let mut set = self.vmas.lock();
        let ranges: Vec<Range<usize>> = set.iter().map(|area| area.range.clone()).collect();
        for range in ranges {
            set.update_flags_range(&range, |flags| flags.with(VmFlags::LOCKED));
        }
    }

    pub fn set_mlock_future(&self, enabled: bool) {
        self.mlock_future.store(enabled, Ordering::Release);
    }

    pub fn munlock_all(&self) {
        self.mlock_future.store(false, Ordering::Release);
        let mut set = self.vmas.lock();
        let ranges: Vec<Range<usize>> = set.iter().map(|area| area.range.clone()).collect();
        for range in ranges {
            set.update_flags_range(&range, |flags| flags.without(VmFlags::LOCKED));
        }
    }

    fn update_locked_range(&self, range: Range<usize>, locked: bool) -> Result<(), Errno> {
        self.validate_range(&range)?;
        let mut set = self.vmas.lock();
        if !set.contains_range(&range) {
            return Err(Errno::ENOMEM);
        }
        if locked {
            set.update_flags_range(&range, |flags| flags.with(VmFlags::LOCKED));
        } else {
            set.update_flags_range(&range, |flags| flags.without(VmFlags::LOCKED));
        }
        Ok(())
    }

    /// fork：克隆 VMA 元数据，已驻留的页按 private-COW / shared 语义重建页表。
    pub fn fork(&self) -> Self {
        let ops = user_pgd_ops().expect("[mm] user_pgd_ops not registered");
        let new_pgd = (ops.new_pgd_for_user)();
        let cloned_set = self.vmas.lock().deep_clone_metadata();
        let cloned_file_backings = Self::collect_file_backings(cloned_set.iter());
        let mut child_pages = BTreeMap::new();
        let mut child_maps = Vec::new();

        {
            let mut parent_pages = self.pages.lock();
            for (va, mapping) in parent_pages.iter_mut() {
                let Some(area) = cloned_set.find(*va) else {
                    continue;
                };
                let old_access = mapping.access;
                mapping.access = access_after_fork(area.flags, &mapping.page);
                if old_access != mapping.access {
                    self.protect_page_no_flush(*va, pte_flags_for(area.flags, mapping.access))
                        .expect("[mm] fork parent protect failed");
                }
                let child_mapping = mapping.clone();
                child_maps.push((
                    *va,
                    child_mapping.page.clone(),
                    area.flags,
                    child_mapping.access,
                ));
                child_pages.insert(*va, child_mapping);
            }
        }
        if !child_maps.is_empty() {
            self.flush_full_user_tlb();
        }

        for (va, page, flags, access) in &child_maps {
            unsafe {
                (ops.map)(
                    new_pgd,
                    *va,
                    page.paddr(),
                    pte_flags_for(*flags, *access).with(VmFlags::USER),
                );
            }
        }

        Self::notify_files_mapped(cloned_file_backings);

        let mapped_pages = child_pages.len();
        VM_SPACE_CREATED.fetch_add(1, Ordering::Relaxed);
        VM_SPACE_LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            vmas: spin::Mutex::new(cloned_set),
            pages: spin::Mutex::new(child_pages),
            pgd: new_pgd,
            brk_start: AtomicUsize::new(self.brk_start.load(Ordering::Relaxed)),
            brk_current: AtomicUsize::new(self.current_brk()),
            mmap_next: AtomicUsize::new(self.mmap_next.load(Ordering::Acquire)),
            mlock_future: AtomicBool::new(self.mlock_future.load(Ordering::Acquire)),
            mapped_pages: AtomicUsize::new(mapped_pages),
        }
    }

    /// 切到本地址空间（`schedule_once` 调；写 PGDL 并 flush TLB）。
    pub fn activate(&self) {
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.activate)(self.pgd) };
        }
    }

    /// page-fault 分派进来的入口。按 VMA backing / 权限决定该做什么。
    pub fn handle_fault(&self, addr: usize, kind: FaultKind) -> FaultOutcome {
        if user_pgd_ops().is_none() {
            return FaultOutcome::Kernel(KernelFaultReason::NotInitialized);
        }
        let page = page_base(addr);
        let set = self.vmas.lock();
        let Some(area) = set.find(page) else {
            drop(set);
            let mut set = self.vmas.lock();
            let Some((_added, flags)) = set.grow_down_to(page, vm_layout().max_grows_down_bytes)
            else {
                return FaultOutcome::Segv;
            };
            drop(set);
            return self.commit_fault_page(page, VmBacking::Anon, flags, page, kind);
        };
        if !permits(area.flags, kind) {
            return FaultOutcome::Segv;
        }
        let backing = area.backing.clone();
        let flags = area.flags;
        let area_start = area.range.start;
        drop(set);

        {
            let mut pages = self.pages.lock();
            if let Some(mapping) = pages.get_mut(&page) {
                return self.handle_resident_fault(page, flags, kind, mapping);
            }
        }

        self.commit_fault_page(page, backing, flags, area_start, kind)
    }

    /// 取得从用户地址读取的一页内连续窗口。
    ///
    /// 这个接口面向大块 I/O / bulk copy 热路径：先通过 VmSpace 完成权限检查、
    /// lazy fault-in 和 COW，再把 resident page 的物理页转成内核直映 slice。
    /// 因此闭包内访问的是内核地址，不需要走 arch uaccess 的逐元素 fixup。
    ///
    /// # Safety
    ///
    /// 调用方必须保证闭包不会保存传入 slice；用户映射可能被其它线程并发改变，
    /// 本函数只通过 resident page 的 Arc 保证底层物理页在闭包期间不被释放。
    pub unsafe fn with_user_read_slice<R>(
        &self,
        user: usize,
        max_len: usize,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<R, Errno> {
        let (_page, kva, len) = self.user_page_window(user, max_len, FaultKind::Load)?;
        let slice = unsafe { core::slice::from_raw_parts(kva as *const u8, len) };
        Ok(f(slice))
    }

    /// 取得写入用户地址的一页内连续窗口。
    ///
    /// Store fault 会在返回前解析 COW / shared dirty 状态。闭包返回后再次标脏，
    /// 覆盖 VFS 在闭包内写入用户页但没有显式 fault 的场景。
    ///
    /// # Safety
    ///
    /// 同 [`Self::with_user_read_slice`]。调用方还必须保证闭包不会制造跨线程可见
    /// 的长期 `&mut [u8]` 别名。
    pub unsafe fn with_user_write_slice<R>(
        &self,
        user: usize,
        max_len: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Result<R, Errno> {
        let (page, kva, len) = self.user_page_window(user, max_len, FaultKind::Store)?;
        let slice = unsafe { core::slice::from_raw_parts_mut(kva as *mut u8, len) };
        let result = f(slice);
        page.mark_dirty();
        Ok(result)
    }

    /// 立即为一个 ELF 段分配并填充物理页。
    pub fn commit_segment(
        &self,
        vaddr: usize,
        memsz: usize,
        file_size: usize,
        data: &[u8],
        flags: VmFlags,
    ) -> Result<(), Errno> {
        if memsz == 0 {
            return Ok(());
        }
        if file_size > memsz || data.len() != file_size {
            return Err(Errno::EINVAL);
        }
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;

        let page_size = page_size();
        let start = page_base(vaddr);
        let end_unaligned = vaddr.checked_add(memsz).ok_or(Errno::EINVAL)?;
        let end = align_up(end_unaligned, page_size).ok_or(Errno::EINVAL)?;
        let area_flags = flags.with(VmFlags::USER).with(VmFlags::ANON);

        self.map_anon(start..end, area_flags)?;

        let file_end_vaddr = vaddr + file_size;
        let mut pages = self.pages.lock();
        let mut page_va = start;
        while page_va < end {
            let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let copy_start_va = page_va.max(vaddr);
            let copy_end_va = (page_va + page_size).min(file_end_vaddr);
            if copy_end_va > copy_start_va {
                let seg_off = copy_start_va - vaddr;
                let len = copy_end_va - copy_start_va;
                let dst_off_in_page = copy_start_va - page_va;
                let kva = virt_fn(paddr) + dst_off_in_page;
                unsafe {
                    core::ptr::copy_nonoverlapping(data.as_ptr().add(seg_off), kva as *mut u8, len);
                }
            }
            let page = ResidentPage::new_anon(paddr);
            let access = access_for_new_page(area_flags, &page);
            self.map_page(page_va, page.paddr(), pte_flags_for(area_flags, access))?;
            pages.insert(page_va, PageMapping { page, access });
            page_va += page_size;
        }
        self.mapped_pages.store(pages.len(), Ordering::Release);
        Ok(())
    }

    /// 立即为一个 ELF 段分配并从文件按页填充。
    ///
    /// loader 不能为了装载大可执行文件把整个 ELF 读进内核堆。这个入口只在
    /// 当前页需要文件内容时读取最多一页，BSS 和页内尾部仍由零页分配保证清零。
    pub fn commit_file_segment(
        &self,
        vaddr: usize,
        memsz: usize,
        file_offset: u64,
        file_size: usize,
        file: &dyn FileLike,
        flags: VmFlags,
    ) -> Result<(), Errno> {
        if memsz == 0 {
            return Ok(());
        }
        if file_size > memsz {
            return Err(Errno::EINVAL);
        }
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;

        let page_size = page_size();
        let start = page_base(vaddr);
        let end_unaligned = vaddr.checked_add(memsz).ok_or(Errno::EINVAL)?;
        let end = align_up(end_unaligned, page_size).ok_or(Errno::EINVAL)?;
        let file_end_vaddr = vaddr.checked_add(file_size).ok_or(Errno::EINVAL)?;
        let area_flags = flags.with(VmFlags::USER).with(VmFlags::ANON);

        self.map_anon(start..end, area_flags)?;

        let mut pages = self.pages.lock();
        let mut page_va = start;
        while page_va < end {
            let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
            let result = (|| {
                let copy_start_va = page_va.max(vaddr);
                let copy_end_va = (page_va + page_size).min(file_end_vaddr);
                if copy_end_va <= copy_start_va {
                    return Ok(());
                }

                let seg_off = copy_start_va - vaddr;
                let len = copy_end_va - copy_start_va;
                let dst_off_in_page = copy_start_va - page_va;
                let kva = virt_fn(paddr) + dst_off_in_page;
                let dst = unsafe { core::slice::from_raw_parts_mut(kva as *mut u8, len) };
                let mut done = 0usize;
                while done < len {
                    let read_off = file_offset
                        .checked_add((seg_off + done) as u64)
                        .ok_or(Errno::EINVAL)?;
                    let n = file.read_at(read_off, &mut dst[done..])?;
                    if n == 0 {
                        return Err(Errno::ENOEXEC);
                    }
                    done += n;
                }
                Ok(())
            })();
            if let Err(err) = result {
                free_user_page(paddr);
                return Err(err);
            }

            let page = ResidentPage::new_anon(paddr);
            let access = access_for_new_page(area_flags, &page);
            self.map_page(page_va, page.paddr(), pte_flags_for(area_flags, access))?;
            pages.insert(page_va, PageMapping { page, access });
            page_va += page_size;
        }
        self.mapped_pages.store(pages.len(), Ordering::Release);
        Ok(())
    }

    fn validate_range(&self, range: &Range<usize>) -> Result<(), Errno> {
        let page_size = page_size();
        if range.start % page_size != 0 || range.end % page_size != 0 {
            return Err(Errno::EINVAL);
        }
        if range.start >= range.end {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn user_page_window(
        &self,
        user: usize,
        max_len: usize,
        kind: FaultKind,
    ) -> Result<(Arc<ResidentPage>, usize, usize), Errno> {
        if max_len == 0 || user.checked_add(max_len - 1).is_none() {
            return Err(Errno::EFAULT);
        }
        match self.handle_fault(user, kind) {
            FaultOutcome::Fixed => {}
            FaultOutcome::Segv | FaultOutcome::Kernel(_) => return Err(Errno::EFAULT),
        }

        let page_va = page_base(user);
        let offset = user - page_va;
        let len = max_len.min(page_size() - offset);
        let page = {
            let pages = self.pages.lock();
            pages
                .get(&page_va)
                .map(|mapping| Arc::clone(&mapping.page))
                .ok_or(Errno::EFAULT)?
        };
        let virt_fn = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EFAULT)?;
        Ok((Arc::clone(&page), virt_fn(page.paddr()) + offset, len))
    }

    fn commit_fault_page(
        &self,
        page_va: usize,
        backing: VmBacking,
        flags: VmFlags,
        area_start: usize,
        kind: FaultKind,
    ) -> FaultOutcome {
        let page = match backing {
            VmBacking::Anon => alloc_zeroed_user_page()
                .map(ResidentPage::new_anon)
                .ok_or(Errno::ENOMEM),
            VmBacking::SharedAnon { id, offset } => {
                let object_off = offset + (page_va - area_start) as u64;
                shared_anon_page(id, object_off)
            }
            VmBacking::File { file, offset } => {
                let file_off = offset + (page_va - area_start) as u64;
                if flags.has(VmFlags::SHARED) {
                    shared_file_page(file, file_off)
                } else {
                    load_file_page(&*file, file_off).map(ResidentPage::new_private_file)
                }
            }
            VmBacking::Direct(base) => {
                let paddr = base + (page_va - area_start);
                Ok(ResidentPage::new_direct(paddr))
            }
        };
        let page = match page {
            Ok(page) => page,
            Err(err) => return fault_from_errno(err),
        };
        let mut access = access_for_new_page(flags, &page);
        if page.is_sysv_shm() && flags.has(VmFlags::WRITE) {
            // SysV shm is a shared memory object, not a regular file mapping.
            // Keep it writable across fork, but conservatively flush it back if
            // the last resident page disappears before another attach faults it.
            page.mark_dirty();
        }
        if is_write_fault(kind) && matches!(access, PageAccess::SharedTracked) {
            page.mark_dirty();
            access = PageAccess::Writable;
        }

        let mut pages = self.pages.lock();
        if let Some(mapping) = pages.get_mut(&page_va) {
            return self.handle_resident_fault(page_va, flags, kind, mapping);
        }
        if let Err(err) = self.map_page(page_va, page.paddr(), pte_flags_for(flags, access)) {
            return fault_from_errno(err);
        }
        pages.insert(page_va, PageMapping { page, access });
        self.mapped_pages.store(pages.len(), Ordering::Release);
        FaultOutcome::Fixed
    }

    fn handle_resident_fault(
        &self,
        page_va: usize,
        flags: VmFlags,
        kind: FaultKind,
        mapping: &mut PageMapping,
    ) -> FaultOutcome {
        if matches!(kind, FaultKind::Privilege) {
            return match self.protect_page(page_va, pte_flags_for(flags, mapping.access)) {
                Ok(()) => FaultOutcome::Fixed,
                Err(err) => fault_from_errno(err),
            };
        }
        if !is_write_fault(kind) {
            return FaultOutcome::Fixed;
        }
        match mapping.access {
            PageAccess::Writable => FaultOutcome::Fixed,
            PageAccess::SharedTracked => {
                mapping.page.mark_dirty();
                mapping.access = PageAccess::Writable;
                match self.protect_page(page_va, pte_flags_for(flags, mapping.access)) {
                    Ok(()) => FaultOutcome::Fixed,
                    Err(err) => fault_from_errno(err),
                }
            }
            PageAccess::Cow => {
                let new_page = match clone_page_to_anon(&mapping.page) {
                    Ok(page) => page,
                    Err(err) => return fault_from_errno(err),
                };
                if let Err(err) = self.replace_page(
                    page_va,
                    new_page.paddr(),
                    pte_flags_for(flags, PageAccess::Writable),
                ) {
                    return fault_from_errno(err);
                }
                mapping.page = new_page;
                mapping.access = PageAccess::Writable;
                FaultOutcome::Fixed
            }
            PageAccess::ReadOnly => FaultOutcome::Segv,
        }
    }

    fn map_page(&self, vaddr: usize, paddr: usize, flags: VmFlags) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.map)(self.pgd, vaddr, paddr, flags.with(VmFlags::USER));
            (ops.invalidate_range)(self.pgd, vaddr, page_size);
        }
        Ok(())
    }

    fn unmap_page(&self, vaddr: usize) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.unmap)(self.pgd, vaddr, page_size);
            (ops.invalidate_range)(self.pgd, vaddr, page_size);
        }
        Ok(())
    }

    fn protect_page(&self, vaddr: usize, flags: VmFlags) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.protect)(self.pgd, vaddr, page_size, flags.with(VmFlags::USER));
            // mprotect 会在 pthread 创建路径把预留栈从 PROT_NONE 改为 RW。
            // 权限位修改后必须刷掉旧 TLB，否则用户态可能继续命中旧的不可访问权限。
            (ops.invalidate_range)(self.pgd, vaddr, page_size);
        }
        Ok(())
    }

    fn protect_page_no_flush(&self, vaddr: usize, flags: VmFlags) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.protect)(self.pgd, vaddr, page_size, flags.with(VmFlags::USER));
        }
        Ok(())
    }

    fn invalidate_user_range(&self, vaddr: usize, len: usize) {
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.invalidate_range)(self.pgd, vaddr, len) };
        }
    }

    fn flush_full_user_tlb(&self) {
        // vaddr=1, len=usize::MAX 会溢出，触发 arch 层全局 flush（with_asid(asid, None)）。
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.invalidate_range)(self.pgd, 1, usize::MAX) };
        }
    }

    fn replace_page(&self, vaddr: usize, paddr: usize, flags: VmFlags) -> Result<(), Errno> {
        let ops = user_pgd_ops().ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        unsafe {
            (ops.unmap)(self.pgd, vaddr, page_size);
            (ops.invalidate_range)(self.pgd, vaddr, page_size);
            (ops.map)(self.pgd, vaddr, paddr, flags.with(VmFlags::USER));
            (ops.invalidate_range)(self.pgd, vaddr, page_size);
        }
        Ok(())
    }

    fn remove_page_mappings(&self, range: Range<usize>) -> Vec<(usize, PageMapping)> {
        let mut pages = self.pages.lock();
        let keys: Vec<usize> = pages.range(range).map(|(k, _)| *k).collect();
        let mut removed = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(mapping) = pages.remove(&key) {
                removed.push((key, mapping));
            }
        }
        self.mapped_pages.store(pages.len(), Ordering::Release);
        removed
    }

    fn move_page_mappings(
        &self,
        old_start: usize,
        new_start: usize,
        len: usize,
    ) -> Result<(), Errno> {
        let old_range = old_start..old_start + len;
        let moved = self.remove_page_mappings(old_range);
        let set = self.vmas.lock();
        let mut pages = self.pages.lock();
        for (old_va, mapping) in moved {
            self.unmap_page(old_va)?;
            let new_va = new_start + (old_va - old_start);
            let area = set.find(new_va).ok_or(Errno::ENOMEM)?;
            self.map_page(
                new_va,
                mapping.page.paddr(),
                pte_flags_for(area.flags, mapping.access),
            )?;
            pages.insert(new_va, mapping);
        }
        self.mapped_pages.store(pages.len(), Ordering::Release);
        Ok(())
    }

    fn extend_mapping_in_place(
        &self,
        old_range: &Range<usize>,
        tail_range: &Range<usize>,
    ) -> Result<bool, Errno> {
        if tail_range.start >= tail_range.end {
            return Ok(true);
        }
        let mapped_tail = {
            let mut vmas = self.vmas.lock();
            if !vmas.contains_range(old_range) {
                return Err(Errno::ENOMEM);
            }
            if !vmas.is_range_free(tail_range) {
                return Ok(false);
            }
            let last = vmas
                .find(old_range.end - page_size())
                .cloned()
                .ok_or(Errno::ENOMEM)?;
            let shift = last.range.end - last.range.start;
            let backing = last.backing.checked_shift(shift).ok_or(Errno::EINVAL)?;
            let tail = VmArea {
                range: tail_range.clone(),
                flags: last.flags,
                backing,
            };
            let files = Self::collect_file_backings(core::iter::once(&tail));
            vmas.insert(tail)?;
            files
        };
        Self::notify_files_mapped(mapped_tail);
        Ok(true)
    }

    /// 收集 VMA 上的 file backing，生命周期 hook 统一在锁外调用。
    ///
    /// 这样 VMA 树只负责描述已经生效的映射变化，SysV shm 等特殊 FileLike 在
    /// hook 内维护 attach 计数时，不会反向持有或阻塞 VM 内部锁。
    fn collect_file_backings<'a>(
        areas: impl IntoIterator<Item = &'a VmArea>,
    ) -> Vec<Arc<dyn FileLike>> {
        let mut files = Vec::new();
        for area in areas {
            if let VmBacking::File { file, .. } = &area.backing {
                files.push(Arc::clone(file));
            }
        }
        files
    }

    fn notify_files_mapped(files: Vec<Arc<dyn FileLike>>) {
        for file in files {
            file.on_mapped();
        }
    }

    fn notify_file_unmapped(areas: &[VmArea]) {
        let files = Self::collect_file_backings(areas.iter());
        for file in files {
            file.on_unmapped();
        }
    }
}

impl Drop for VmSpace {
    fn drop(&mut self) {
        VM_SPACE_DROPPED.fetch_add(1, Ordering::Relaxed);
        VM_SPACE_LIVE.fetch_sub(1, Ordering::Relaxed);
        let files = {
            let vmas = self.vmas.lock();
            Self::collect_file_backings(vmas.iter())
        };
        for file in files {
            file.on_unmapped();
        }
        self.pages.lock().clear();
        if let Some(ops) = user_pgd_ops() {
            unsafe { (ops.drop_pgd)(self.pgd) };
        }
    }
}

fn access_for_new_page(flags: VmFlags, page: &ResidentPage) -> PageAccess {
    if page.is_direct_shared_writable() {
        return if flags.has(VmFlags::WRITE) {
            PageAccess::Writable
        } else {
            PageAccess::ReadOnly
        };
    }
    if !flags.has(VmFlags::WRITE) {
        PageAccess::ReadOnly
    } else if flags.has(VmFlags::SHARED) {
        PageAccess::SharedTracked
    } else {
        PageAccess::Writable
    }
}

fn access_for_existing_page(flags: VmFlags, page: &Arc<ResidentPage>) -> PageAccess {
    if page.is_direct_shared_writable() {
        return if flags.has(VmFlags::WRITE) {
            PageAccess::Writable
        } else {
            PageAccess::ReadOnly
        };
    }
    if !flags.has(VmFlags::WRITE) {
        PageAccess::ReadOnly
    } else if flags.has(VmFlags::SHARED) {
        PageAccess::SharedTracked
    } else if Arc::strong_count(page) > 1 {
        PageAccess::Cow
    } else {
        PageAccess::Writable
    }
}

fn access_after_fork(flags: VmFlags, page: &Arc<ResidentPage>) -> PageAccess {
    if page.is_direct_shared_writable() {
        return if flags.has(VmFlags::WRITE) {
            PageAccess::Writable
        } else {
            PageAccess::ReadOnly
        };
    }
    if !flags.has(VmFlags::WRITE) {
        PageAccess::ReadOnly
    } else if flags.has(VmFlags::SHARED) {
        PageAccess::SharedTracked
    } else {
        PageAccess::Cow
    }
}

fn pte_flags_for(flags: VmFlags, access: PageAccess) -> VmFlags {
    let flags = flags.with(VmFlags::USER);
    if access.pte_writable() {
        flags
    } else {
        flags.without(VmFlags::WRITE)
    }
}

fn shared_file_page(file: Arc<dyn FileLike>, file_off: u64) -> Result<Arc<ResidentPage>, Errno> {
    let key = SharedFilePageKey::new(&file, file_off);
    {
        let mut cache = SHARED_FILE_PAGES.lock();
        if let Some(weak) = cache.get(&key) {
            if let Some(page) = weak.upgrade() {
                return Ok(page);
            }
            cache.remove(&key);
        }
    }
    let paddr = load_file_page(&*file, file_off)?;
    let page = ResidentPage::new_shared_file(paddr, Arc::clone(&file), file_off);
    SHARED_FILE_PAGES.lock().insert(key, Arc::downgrade(&page));
    Ok(page)
}

fn shared_anon_page(id: usize, offset: u64) -> Result<Arc<ResidentPage>, Errno> {
    let key = SharedAnonPageKey { id, offset };
    {
        let mut cache = SHARED_ANON_PAGES.lock();
        if let Some(weak) = cache.get(&key) {
            if let Some(page) = weak.upgrade() {
                return Ok(page);
            }
            cache.remove(&key);
        }
    }
    let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
    let page = ResidentPage::new_shared_anon(paddr);
    SHARED_ANON_PAGES.lock().insert(key, Arc::downgrade(&page));
    Ok(page)
}

fn load_file_page(file: &dyn FileLike, file_off: u64) -> Result<usize, Errno> {
    let file_size = file.size();
    if file_off >= file_size {
        return Err(Errno::EINVAL);
    }
    let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
    let result = (|| {
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        let page_size = page_size();
        let len = (file_size - file_off).min(page_size as u64) as usize;
        let kbuf = unsafe { core::slice::from_raw_parts_mut(virt(paddr) as *mut u8, page_size) };
        file.read_at(file_off, &mut kbuf[..len])?;
        Ok(())
    })();
    if result.is_err() {
        free_user_page(paddr);
    }
    result.map(|()| paddr)
}

fn clone_page_to_anon(source: &ResidentPage) -> Result<Arc<ResidentPage>, Errno> {
    let paddr = alloc_zeroed_user_page().ok_or(Errno::ENOMEM)?;
    let result = (|| {
        let virt = allocator::KERNEL_ALLOCATOR
            .load_phys_to_virt()
            .ok_or(Errno::EINVAL)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                virt(source.paddr()) as *const u8,
                virt(paddr) as *mut u8,
                page_size(),
            );
        }
        Ok(())
    })();
    if result.is_err() {
        free_user_page(paddr);
    }
    result.map(|()| ResidentPage::new_anon(paddr))
}

fn fault_from_errno(err: Errno) -> FaultOutcome {
    match err {
        Errno::ENOMEM => FaultOutcome::Kernel(KernelFaultReason::UncaughtKernelAccess),
        _ => FaultOutcome::Segv,
    }
}

fn is_write_fault(kind: FaultKind) -> bool {
    matches!(kind, FaultKind::Store | FaultKind::PermWrite)
}

/// flags 是否允许该类访问。
fn permits(flags: VmFlags, kind: FaultKind) -> bool {
    match kind {
        FaultKind::Load | FaultKind::PermRead => flags.has(VmFlags::READ),
        FaultKind::Store | FaultKind::PermWrite => flags.has(VmFlags::WRITE),
        FaultKind::Exec | FaultKind::PermExec => flags.has(VmFlags::EXEC),
        FaultKind::Privilege => flags.permissions().bits() != 0,
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    Some(value.checked_add(align - 1)? & !(align - 1))
}

fn alloc_zeroed_user_page() -> Option<usize> {
    let order = user_page_order()?;
    let size = page_size();
    // 用户物理页必须进入 allocator registry；否则 fork/munmap/drop 路径无法被
    // allocator 审计发现泄漏或重复释放。
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(allocator::PhysicalAllocRequest::new(
            size,
            allocator::PAGE_SIZE,
        ))
        .ok()?;
    let Some(virt) = allocator::KERNEL_ALLOCATOR.load_phys_to_virt() else {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation);
        return None;
    };
    if allocation.order != order || allocation.size != size {
        let _ = allocator::KERNEL_ALLOCATOR.try_free_physical(allocation);
        return None;
    }
    unsafe { core::ptr::write_bytes(virt(allocation.paddr) as *mut u8, 0, size) };
    Some(allocation.paddr)
}

fn free_user_page(paddr: usize) {
    if let Err(err) = allocator::KERNEL_ALLOCATOR.try_free_physical_addr(paddr) {
        log::error!(
            "[mm] failed to free tracked user page paddr={:#x}: {:?}",
            paddr,
            err
        );
    }
}

fn user_page_order() -> Option<usize> {
    let page_size = page_size();
    if page_size < allocator::PAGE_SIZE || page_size % allocator::PAGE_SIZE != 0 {
        return None;
    }
    let allocator_pages = page_size / allocator::PAGE_SIZE;
    if !allocator_pages.is_power_of_two() {
        return None;
    }
    Some(allocator_pages.trailing_zeros() as usize)
}

/// 获取 Vec<Range<usize>> 视图，方便调试打印 / smoketest。
pub fn dump_vmas(vm: &VmSpace) -> Vec<(Range<usize>, VmFlags)> {
    vm.vmas
        .lock()
        .iter()
        .map(|a| (a.range.clone(), a.flags))
        .collect()
}
