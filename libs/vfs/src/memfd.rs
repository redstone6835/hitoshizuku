//! memfd-backed anonymous regular files.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::vfs::anon;
use crate::vfs::cred::Credentials;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::inode::{Inode, InodeOps};
use crate::vfs::stat::{FileMode, FileType};
use crate::vfs::sync::Spinlock;

pub const F_SEAL_SEAL: u32 = 0x0001;
pub const F_SEAL_SHRINK: u32 = 0x0002;
pub const F_SEAL_GROW: u32 = 0x0004;
pub const F_SEAL_WRITE: u32 = 0x0008;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
pub const F_SEAL_ALL: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

const MEMFD_PAGE_SIZE: usize = 4096;
const MEMFD_PAGE_SIZE_U64: u64 = MEMFD_PAGE_SIZE as u64;

struct MemfdPage {
    index: u64,
    data: Vec<u8>,
}

struct MemfdFileData {
    size: u64,
    pages: Vec<MemfdPage>,
}

impl MemfdFileData {
    const fn new() -> Self {
        Self {
            size: 0,
            pages: Vec::new(),
        }
    }

    fn blocks(&self) -> u64 {
        (self.pages.len() as u64 * MEMFD_PAGE_SIZE_U64).div_ceil(512)
    }

    fn truncate(&mut self, new_size: u64) {
        if new_size < self.size {
            let keep_pages = new_size.div_ceil(MEMFD_PAGE_SIZE_U64);
            self.pages.retain(|page| page.index < keep_pages);
            if new_size % MEMFD_PAGE_SIZE_U64 != 0 {
                let tail_index = new_size / MEMFD_PAGE_SIZE_U64;
                let tail_offset = (new_size % MEMFD_PAGE_SIZE_U64) as usize;
                if let Some(pos) = self.page_pos(tail_index).ok() {
                    self.pages[pos].data[tail_offset..].fill(0);
                }
            }
        }
        self.size = new_size;
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> usize {
        if offset >= self.size || buf.is_empty() {
            return 0;
        }
        let end = offset.saturating_add(buf.len() as u64).min(self.size);
        let n = (end - offset) as usize;
        let out = &mut buf[..n];
        out.fill(0);

        let first_page = offset / MEMFD_PAGE_SIZE_U64;
        let last_page = (end - 1) / MEMFD_PAGE_SIZE_U64;
        for page_index in first_page..=last_page {
            let Ok(pos) = self.page_pos(page_index) else {
                continue;
            };
            let page_start = page_index * MEMFD_PAGE_SIZE_U64;
            let copy_start = offset.max(page_start);
            let copy_end = end.min(page_start + MEMFD_PAGE_SIZE_U64);
            let src_start = (copy_start - page_start) as usize;
            let dst_start = (copy_start - offset) as usize;
            let len = (copy_end - copy_start) as usize;
            out[dst_start..dst_start + len]
                .copy_from_slice(&self.pages[pos].data[src_start..src_start + len]);
        }
        n
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(VfsError::FileTooLarge)?;
        if end > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }

        let mut written = 0usize;
        while written < buf.len() {
            let file_off = offset + written as u64;
            let page_index = file_off / MEMFD_PAGE_SIZE_U64;
            let page_offset = (file_off % MEMFD_PAGE_SIZE_U64) as usize;
            let chunk = (MEMFD_PAGE_SIZE - page_offset).min(buf.len() - written);
            let page = match self.get_or_create_page(page_index) {
                Ok(page) => page,
                Err(_) if written != 0 => {
                    self.size = self.size.max(offset + written as u64);
                    return Ok(written);
                }
                Err(err) => return Err(err),
            };
            page[page_offset..page_offset + chunk].copy_from_slice(&buf[written..written + chunk]);
            written += chunk;
        }

        self.size = self.size.max(end);
        Ok(written)
    }

    fn page_pos(&self, index: u64) -> Result<usize, usize> {
        self.pages.binary_search_by_key(&index, |page| page.index)
    }

    fn get_or_create_page(&mut self, index: u64) -> VfsResult<&mut [u8]> {
        match self.page_pos(index) {
            Ok(pos) => Ok(self.pages[pos].data.as_mut_slice()),
            Err(pos) => {
                self.pages
                    .try_reserve(1)
                    .map_err(|_| VfsError::OutOfMemory)?;
                let mut data = Vec::new();
                data.try_reserve_exact(MEMFD_PAGE_SIZE)
                    .map_err(|_| VfsError::OutOfMemory)?;
                data.resize(MEMFD_PAGE_SIZE, 0);
                self.pages.insert(pos, MemfdPage { index, data });
                Ok(self.pages[pos].data.as_mut_slice())
            }
        }
    }
}

struct MemfdInner {
    file: MemfdFileData,
    seals: u32,
    inode: Option<Weak<Inode>>,
}

struct MemfdState {
    inner: Spinlock<MemfdInner>,
    allow_sealing: bool,
    waiters: WaitQueue,
}

impl MemfdState {
    fn new(allow_sealing: bool) -> Self {
        let seals = if allow_sealing { 0 } else { F_SEAL_SEAL };
        Self {
            inner: Spinlock::new(MemfdInner {
                file: MemfdFileData::new(),
                seals,
                inode: None,
            }),
            allow_sealing,
            waiters: WaitQueue::new(),
        }
    }

    fn bind_inode(&self, inode: &Arc<Inode>) {
        self.inner.lock().inode = Some(Arc::downgrade(inode));
    }

    fn update_inode_size(inner: &MemfdInner) {
        if let Some(inode) = inner.inode.as_ref().and_then(Weak::upgrade) {
            inode.set_size_and_blocks(inner.file.size, inner.file.blocks());
        }
    }

    fn add_seals(&self, seals: u32) -> Result<(), Errno> {
        if !self.allow_sealing || (seals & !F_SEAL_ALL) != 0 {
            return Err(Errno::EINVAL);
        }
        let mut inner = self.inner.lock();
        if (inner.seals & F_SEAL_SEAL) != 0 {
            return Err(Errno::EPERM);
        }
        inner.seals |= seals;
        Ok(())
    }

    fn seals(&self) -> u32 {
        self.inner.lock().seals
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        if size > usize::MAX as u64 {
            return Err(VfsError::FileTooLarge);
        }
        let mut inner = self.inner.lock();
        let old_size = inner.file.size;
        if size < old_size && (inner.seals & F_SEAL_SHRINK) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        if size > old_size && (inner.seals & F_SEAL_GROW) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        inner.file.truncate(size);
        Self::update_inode_size(&inner);
        drop(inner);
        self.waiters.wake_all();
        Ok(())
    }
}

struct MemfdInodeOps {
    state: Arc<MemfdState>,
}

impl InodeOps for MemfdInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn truncate(&self, _inode: &Inode, size: u64) -> VfsResult<()> {
        self.state.truncate(size)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct MemfdFileOps {
    state: Arc<MemfdState>,
}

impl MemfdFileOps {
    pub fn add_seals(&self, seals: u32) -> Result<(), Errno> {
        self.state.add_seals(seals)
    }

    pub fn seals(&self) -> u32 {
        self.state.seals()
    }
}

impl FileOps for MemfdFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let inner = self.state.inner.lock();
        Ok(inner.file.read_at(buf, offset))
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let mut inner = self.state.inner.lock();
        if (inner.seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE)) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        let offset = if offset == u64::MAX {
            inner.file.size
        } else {
            if offset > usize::MAX as u64 {
                return Err(VfsError::FileTooLarge);
            }
            offset
        };
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(VfsError::FileTooLarge)?;
        if end > inner.file.size && (inner.seals & F_SEAL_GROW) != 0 {
            return Err(VfsError::OperationNotPermitted);
        }
        let written = inner.file.write_at(buf, offset)?;
        Self::state_update_inode_size(&inner);
        drop(inner);
        if written != 0 {
            self.state.waiters.wake_all();
        }
        Ok(written)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        let mut ready = PollEvents::POLLIN.with(PollEvents::POLLOUT);
        if (self.state.inner.lock().seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE)) != 0 {
            ready = ready.without(PollEvents::POLLOUT);
        }
        ready.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, _interest: PollEvents) -> bool {
        self.state.waiters.enqueue(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.state.waiters.remove(task);
    }

    fn fallocate(&self, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.checked_add(len).ok_or(VfsError::FileTooLarge)?;
        // memfd/tmpfs backing is sparse: reserve logical size here and allocate
        // real pages lazily when data is written or a shared mmap page is dirtied.
        self.state.truncate(end)
    }

    fn ioctl(&self, _cmd: IoctlCmd, _arg: usize) -> Result<usize, Errno> {
        Err(Errno::ENOTTY)
    }

    fn release(&self) {
        self.state.waiters.wake_all();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl MemfdFileOps {
    fn state_update_inode_size(inner: &MemfdInner) {
        MemfdState::update_inode_size(inner);
    }
}

pub fn create(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    allow_sealing: bool,
    cloexec: bool,
) -> Result<Fd, Errno> {
    let state = Arc::new(MemfdState::new(allow_sealing));
    let inode_ops = Arc::new(MemfdInodeOps {
        state: Arc::clone(&state),
    });
    let file_ops = Box::new(MemfdFileOps {
        state: Arc::clone(&state),
    });
    let file_flags = OpenOptions {
        access: AccessMode::ReadWrite,
        ..Default::default()
    };
    let file = anon::new_private_file(
        cred,
        file_flags,
        FileType::Regular,
        FileMode::new(0o600),
        0,
        inode_ops,
        file_ops,
    );
    state.bind_inode(file.inode());
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    fdt.alloc_fd(file, fd_flags).map_err(|e| e.to_errno())
}
