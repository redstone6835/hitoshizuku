//! userfaultfd —— 用户态缺页处理。
//!
//! Linux userfaultfd 允许进程把一段**私有匿名**地址空间的缺页交给用户态处理：
//!
//! - `userfaultfd(2)` 创建 fd；`UFFDIO_API` 握手后 `UFFDIO_REGISTER` 登记区域；
//! - 登记区域内的缺页（MISSING 模式）或写保护缺页（WP 模式）不再由内核直接
//!   填充，而是入队 `uffd_msg` 事件并**挂起**触发任务；
//! - 用户态 `read()` 事件、执行 `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` /
//!   `UFFDIO_WRITEPROTECT` 解决缺页后，被挂起的任务被唤醒并重试访问。
//!
//! 本实现覆盖匿名私有映射的 MISSING、WP 与 MINOR 三种模式；shmem/hugetlb/file
//! 区域在 `UFFDIO_REGISTER` 时返回 `EINVAL`（与不支持这些后端的 Linux 行为一致）。
//! MINOR 模式的 `UFFDIO_CONTINUE` 因无共享页缓存而退化为对非驻留页安装零页
//! （见 [`crate::mm::vm_space::VmSpace::uffd_continue`] 的说明）。
//! 挂起采用 [`WaitQueue::wait_event`]，语义上近似 Linux 的 TASK_KILLABLE：
//! 普通信号不打断等待，但 fd 关闭/注销会唤醒等待者并让其退回普通缺页路径。
//!
//! 依赖方向：本模块在 `general` 内，`FileOps` 由 `libs/vfs` 提供；`VmSpace`
//! 通过本模块的 [`UffdRegion`] 表参与缺页拦截。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::ops::{ControlFlow, Range};
use core::sync::atomic::{AtomicBool, Ordering};

use errno::Errno;
use sched::{Task, WaitQueue};

use crate::vfs::anon;
use crate::vfs::cred::Credentials;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fdtable::{Fd, FdFlags, FdTable};
use crate::vfs::file::{AccessMode, DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use crate::vfs::poll_source::PollSource;
use crate::vfs::sync::Spinlock;

use crate::mm::vm_space::VmSpace;

// ── UAPI 常量（与 Linux <linux/userfaultfd.h> 对齐） ─────────────────────────

/// `UFFDIO_API` 协议的 API 版本。
pub const UFFD_API: u64 = 0xAA;
/// 本实现支持的 feature 位：无（不支持事件/线程 ID 等扩展）。
pub const UFFD_FEATURES_SUPPORTED: u64 = 0;

/// ioctl 请求号（nr 段），用于组装 ioctls 位掩码。
const NR_REGISTER: u64 = 0x00;
const NR_UNREGISTER: u64 = 0x01;
const NR_WAKE: u64 = 0x02;
const NR_COPY: u64 = 0x03;
const NR_ZEROPAGE: u64 = 0x04;
const NR_WRITEPROTECT: u64 = 0x05;
const NR_CONTINUE: u64 = 0x06;
const NR_API: u64 = 0x3F;

/// 组装 Linux `_IOWR(type, nr, size)` ioctl 命令字。
const fn iowr(nr: u64, size: u64) -> usize {
    ((3u64 << 30) | (size << 16) | (0xAAu64 << 8) | nr) as usize
}

/// 组装 Linux `_IOW(type, nr, size)` ioctl 命令字。
const fn iow(nr: u64, size: u64) -> usize {
    ((1u64 << 30) | (size << 16) | (0xAAu64 << 8) | nr) as usize
}

pub const UFFDIO_API: usize = iowr(NR_API, 24);
pub const UFFDIO_REGISTER: usize = iowr(NR_REGISTER, 32);
pub const UFFDIO_UNREGISTER: usize = iow(NR_UNREGISTER, 16);
pub const UFFDIO_WAKE: usize = iow(NR_WAKE, 16);
pub const UFFDIO_COPY: usize = iowr(NR_COPY, 40);
pub const UFFDIO_ZEROPAGE: usize = iowr(NR_ZEROPAGE, 32);
pub const UFFDIO_WRITEPROTECT: usize = iowr(NR_WRITEPROTECT, 24);
pub const UFFDIO_CONTINUE: usize = iowr(NR_CONTINUE, 32);

/// ioctls 位掩码（按 nr 位）。
const BIT_REGISTER: u64 = 1 << NR_REGISTER;
const BIT_UNREGISTER: u64 = 1 << NR_UNREGISTER;
const BIT_WAKE: u64 = 1 << NR_WAKE;
const BIT_COPY: u64 = 1 << NR_COPY;
const BIT_ZEROPAGE: u64 = 1 << NR_ZEROPAGE;
const BIT_WRITEPROTECT: u64 = 1 << NR_WRITEPROTECT;
const BIT_CONTINUE: u64 = 1 << NR_CONTINUE;

pub const UFFD_API_RANGE_IOCTLS: u64 =
    BIT_WAKE | BIT_COPY | BIT_ZEROPAGE | BIT_WRITEPROTECT | BIT_CONTINUE;
pub const UFFD_API_REGISTER_IOCTLS: u64 = UFFD_API_RANGE_IOCTLS | BIT_UNREGISTER;
pub const UFFD_API_IOCTLS: u64 = UFFD_API_REGISTER_IOCTLS | BIT_REGISTER;

/// `UFFDIO_REGISTER` 模式位。
pub(crate) const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
pub(crate) const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;
pub const UFFDIO_REGISTER_MODE_MINOR: u64 = 1 << 2;
const REGISTER_MODES_SUPPORTED: u64 =
    UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_WP | UFFDIO_REGISTER_MODE_MINOR;

/// `UFFDIO_COPY` 模式位。
const UFFDIO_COPY_MODE_DONTWAKE: u64 = 1 << 0;
const UFFDIO_COPY_MODE_WP: u64 = 1 << 1;

/// `UFFDIO_WRITEPROTECT` 模式位。
const UFFDIO_WRITEPROTECT_MODE_WP: u64 = 1 << 0;
const UFFDIO_WRITEPROTECT_MODE_DONTWAKE: u64 = 1 << 1;

/// `uffd_msg.event` 取值。
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

/// `uffd_msg.pagefault.flags` 取值。
pub(crate) const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1;
pub(crate) const UFFD_PAGEFAULT_FLAG_WP: u64 = 1 << 1;
pub(crate) const UFFD_PAGEFAULT_FLAG_MINOR: u64 = 1 << 2;

/// 缺页事件消息（Linux `struct uffd_msg`，32 字节）。
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct UffdMsg {
    pub event: u8,
    reserved1: u8,
    reserved2: u16,
    reserved3: u32,
    // union { struct pagefault {...}; ... }
    pf_flags: u64,
    pf_address: u64,
    feat_ptid: u32,
    ptid: u32,
}

impl UffdMsg {
    pub const SIZE: usize = 32;

    pub(crate) fn pagefault(flags: u64, address: usize) -> Self {
        Self {
            event: UFFD_EVENT_PAGEFAULT,
            reserved1: 0,
            reserved2: 0,
            reserved3: 0,
            pf_flags: flags,
            pf_address: address as u64,
            feat_ptid: 0,
            ptid: 0,
        }
    }

    fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0] = self.event;
        out[8..16].copy_from_slice(&self.pf_flags.to_le_bytes());
        out[16..24].copy_from_slice(&self.pf_address.to_le_bytes());
        out[24..28].copy_from_slice(&self.feat_ptid.to_le_bytes());
        out[28..32].copy_from_slice(&self.ptid.to_le_bytes());
        out
    }
}

/// VmSpace 侧的登记项：区域 + 模式 + 状态对象强引用。
#[derive(Clone)]
pub(crate) struct UffdRegion {
    pub range: Range<usize>,
    pub mode: u64,
    pub state: Arc<UffdState>,
}

/// UffdState 侧的登记项：目标地址空间弱引用（ioctl 定位 + close 清理）。
struct UffdRegistration {
    vm: Weak<VmSpace>,
    range: Range<usize>,
}

/// 单个 userfaultfd 的打开状态。
pub struct UffdState {
    events: Spinlock<VecDeque<UffdMsg>>,
    wait_queue: WaitQueue,
    poll_source: PollSource,
    alive: AtomicBool,
    /// 是否已完成 `UFFDIO_API` 握手（Linux 要求先握手才能用其它 ioctl）。
    api_done: AtomicBool,
    registrations: Spinlock<Vec<UffdRegistration>>,
}

impl UffdState {
    fn new() -> Self {
        Self {
            events: Spinlock::new(VecDeque::new()),
            wait_queue: WaitQueue::new(),
            poll_source: PollSource::new(PollEvents::POLLOUT),
            alive: AtomicBool::new(true),
            api_done: AtomicBool::new(false),
            registrations: Spinlock::new(Vec::new()),
        }
    }

    fn api_handshake_done(&self) -> bool {
        self.api_done.load(Ordering::Acquire)
    }

    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// 登记区域：在状态侧记录目标地址空间，供 ioctl 定位与 close 清理。
    fn add_registration(&self, vm: &Arc<VmSpace>, range: Range<usize>) {
        self.registrations.lock().push(UffdRegistration {
            vm: Arc::downgrade(vm),
            range,
        });
    }

    /// 查询包含 `addr` 的登记目标（UFFDIO_COPY / ZEROPAGE / WRITEPROTECT 用）。
    fn registration_for(&self, addr: usize) -> Option<(Arc<VmSpace>, Range<usize>)> {
        let registrations = self.registrations.lock();
        registrations
            .iter()
            .find(|registration| registration.range.contains(&addr))
            .and_then(|registration| {
                registration
                    .vm
                    .upgrade()
                    .map(|vm| (vm, registration.range.clone()))
            })
    }

    /// 入队一次缺页事件并唤醒 read 等待者。
    pub(crate) fn enqueue_fault(&self, flags: u64, address: usize) {
        let mut events = self.events.lock();
        events.push_back(UffdMsg::pagefault(flags, address));
        let readiness = PollEvents::POLLIN.with(PollEvents::POLLOUT);
        drop(events);
        self.poll_source.publish(readiness);
        self.wait_queue.wake_all();
    }

    /// 在缺页等待中挂起当前任务，直到 `condition` 为真。
    ///
    /// 调度器未就绪（启动期自检）时退化为自旋。
    pub(crate) fn wait_fault(&self, mut condition: impl FnMut() -> bool) {
        if sched::is_ready() {
            let task = sched::current_task();
            self.wait_queue.wait_event(&task, &mut condition);
        } else {
            while !condition() {
                core::hint::spin_loop();
            }
        }
    }

    fn wake_all(&self) {
        self.wait_queue.wake_all();
    }

    /// fd 关闭：标记失效、从所有仍存活的地址空间摘除登记、唤醒全部等待者。
    fn release_with(&self, self_arc: &Arc<UffdState>) {
        self.alive.store(false, Ordering::Release);
        let registrations = core::mem::take(&mut *self.registrations.lock());
        for registration in registrations {
            if let Some(vm) = registration.vm.upgrade() {
                vm.uffd_remove_state(self_arc, &registration.range);
            }
        }
        self.wait_queue.wake_all();
    }
}

/// userfaultfd 文件操作。
pub struct UffdFileOps {
    state: Arc<UffdState>,
}

impl UffdFileOps {
    pub fn new() -> Self {
        Self {
            state: Arc::new(UffdState::new()),
        }
    }

    fn take_events(&self, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.len() < UffdMsg::SIZE {
            return Err(VfsError::InvalidArgument);
        }
        let (bytes, readiness, version) = {
            let mut events = self.state.events.lock();
            if events.is_empty() {
                return Err(VfsError::WouldBlock);
            }
            let count = buf.len() / UffdMsg::SIZE;
            let mut bytes = 0usize;
            for slot in 0..count {
                let Some(msg) = events.pop_front() else {
                    break;
                };
                let raw = msg.to_bytes();
                buf[slot * UffdMsg::SIZE..(slot + 1) * UffdMsg::SIZE].copy_from_slice(&raw);
                bytes += UffdMsg::SIZE;
            }
            let readiness = if events.is_empty() {
                PollEvents::POLLOUT
            } else {
                PollEvents::POLLIN.with(PollEvents::POLLOUT)
            };
            (bytes, readiness, self.state.poll_source.reserve_version())
        };
        self.state.poll_source.publish_versioned(readiness, version);
        Ok(bytes)
    }

    fn ioctl_api(&self, arg: usize) -> Result<usize, Errno> {
        // struct uffdio_api { api, features, ioctls } —— 24 字节。
        let mut raw = [0u8; 24];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let api = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let features = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        if api != UFFD_API {
            return Err(Errno::EINVAL);
        }
        // 只接受我们支持的 feature 位。
        if features & !UFFD_FEATURES_SUPPORTED != 0 {
            return Err(Errno::EINVAL);
        }
        raw[0..8].copy_from_slice(&UFFD_API.to_le_bytes());
        raw[8..16].copy_from_slice(&features.to_le_bytes());
        raw[16..24].copy_from_slice(&UFFD_API_IOCTLS.to_le_bytes());
        crate::mm::copy_to_user(arg, &raw).map_err(|e| e.as_errno())?;
        self.state.api_done.store(true, Ordering::Release);
        Ok(0)
    }

    fn ioctl_register(&self, vm: &Arc<VmSpace>, arg: usize) -> Result<usize, Errno> {
        // struct uffdio_register { range {start,len}, mode, ioctls } —— 32 字节。
        let mut raw = [0u8; 32];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let start = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let mode = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        if mode & !REGISTER_MODES_SUPPORTED != 0 || mode == 0 {
            return Err(Errno::EINVAL);
        }
        let start_usize = usize::try_from(start).map_err(|_| Errno::EINVAL)?;
        let len_usize = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        let range = vm.uffd_register(start_usize, len_usize, mode, &self.state)?;
        self.state.add_registration(&vm, range);
        raw[24..32].copy_from_slice(&UFFD_API_REGISTER_IOCTLS.to_le_bytes());
        crate::mm::copy_to_user(arg, &raw).map_err(|e| e.as_errno())?;
        Ok(0)
    }

    fn ioctl_unregister(&self, vm: &Arc<VmSpace>, arg: usize) -> Result<usize, Errno> {
        let mut raw = [0u8; 16];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let start = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let start_usize = usize::try_from(start).map_err(|_| Errno::EINVAL)?;
        let len_usize = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        vm.uffd_unregister(start_usize, len_usize, &self.state)?;
        Ok(0)
    }

    fn ioctl_wake(&self, arg: usize) -> Result<usize, Errno> {
        let mut raw = [0u8; 16];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        self.state.wake_all();
        Ok(0)
    }

    fn ioctl_copy(&self, vm: &Arc<VmSpace>, arg: usize) -> Result<usize, Errno> {
        // struct uffdio_copy { dst, src, len, mode, copy } —— 40 字节。
        let mut raw = [0u8; 40];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let dst = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let src = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let len = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        let mode = u64::from_le_bytes(raw[24..32].try_into().unwrap());
        if mode & !(UFFDIO_COPY_MODE_DONTWAKE | UFFDIO_COPY_MODE_WP) != 0 {
            return Err(Errno::EINVAL);
        }
        let dst_usize = usize::try_from(dst).map_err(|_| Errno::EINVAL)?;
        let src_usize = usize::try_from(src).map_err(|_| Errno::EINVAL)?;
        let len_usize = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        let installed = vm.uffd_copy(
            dst_usize,
            src_usize,
            len_usize,
            mode & UFFDIO_COPY_MODE_WP != 0,
        )?;
        raw[32..40].copy_from_slice(&(installed as i64).to_le_bytes());
        crate::mm::copy_to_user(arg, &raw).map_err(|e| e.as_errno())?;
        if mode & UFFDIO_COPY_MODE_DONTWAKE == 0 {
            self.state.wake_all();
        }
        Ok(0)
    }

    fn ioctl_zeropage(&self, vm: &Arc<VmSpace>, arg: usize) -> Result<usize, Errno> {
        // struct uffdio_zeropage { range {start,len}, mode, zeropage } —— 32 字节。
        let mut raw = [0u8; 32];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let start = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let mode = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        if mode != 0 {
            return Err(Errno::EINVAL);
        }
        let start_usize = usize::try_from(start).map_err(|_| Errno::EINVAL)?;
        let len_usize = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        let installed = vm.uffd_zeropage(start_usize, len_usize)?;
        raw[24..32].copy_from_slice(&(installed as i64).to_le_bytes());
        crate::mm::copy_to_user(arg, &raw).map_err(|e| e.as_errno())?;
        self.state.wake_all();
        Ok(0)
    }

    fn ioctl_continue(&self, vm: &Arc<VmSpace>, arg: usize) -> Result<usize, Errno> {
        // struct uffdio_continue { range {start,len}, mode, mapped } —— 32 字节。
        let mut raw = [0u8; 32];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let start = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let mode = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        if mode != 0 {
            return Err(Errno::EINVAL);
        }
        let start_usize = usize::try_from(start).map_err(|_| Errno::EINVAL)?;
        let len_usize = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        let installed = vm.uffd_continue(start_usize, len_usize)?;
        raw[24..32].copy_from_slice(&(installed as i64).to_le_bytes());
        crate::mm::copy_to_user(arg, &raw).map_err(|e| e.as_errno())?;
        self.state.wake_all();
        Ok(0)
    }

    fn ioctl_writeprotect(&self, vm: &Arc<VmSpace>, arg: usize) -> Result<usize, Errno> {
        // struct uffdio_writeprotect { range {start,len}, mode } —— 24 字节。
        let mut raw = [0u8; 24];
        crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
        let start = u64::from_le_bytes(raw[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let mode = u64::from_le_bytes(raw[16..24].try_into().unwrap());
        if mode & !(UFFDIO_WRITEPROTECT_MODE_WP | UFFDIO_WRITEPROTECT_MODE_DONTWAKE) != 0 {
            return Err(Errno::EINVAL);
        }
        let start_usize = usize::try_from(start).map_err(|_| Errno::EINVAL)?;
        let len_usize = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
        vm.uffd_writeprotect(
            start_usize,
            len_usize,
            mode & UFFDIO_WRITEPROTECT_MODE_WP != 0,
            &self.state,
        )?;
        if mode & UFFDIO_WRITEPROTECT_MODE_DONTWAKE == 0 {
            self.state.wake_all();
        }
        Ok(0)
    }

    /// 解析 ioctl 目标地址空间：`VmSpace` 参数为调用者地址空间（REGISTER/
    /// UNREGISTER 只作用于调用者自身，与 Linux 一致）；COPY/ZEROPAGE/
    /// WRITEPROTECT 通过登记表定位到注册时所在的地址空间。
    fn ioctl_target(
        &self,
        caller: &Arc<VmSpace>,
        cmd: usize,
        arg: usize,
    ) -> Result<Arc<VmSpace>, Errno> {
        match cmd {
            UFFDIO_REGISTER | UFFDIO_UNREGISTER => Ok(Arc::clone(caller)),
            UFFDIO_COPY | UFFDIO_ZEROPAGE | UFFDIO_WRITEPROTECT | UFFDIO_CONTINUE => {
                // 这些结构体的首 8 字节都是目标地址（dst / range.start）。
                let mut raw = [0u8; 8];
                crate::mm::copy_from_user(arg, &mut raw).map_err(|e| e.as_errno())?;
                let start = u64::from_le_bytes(raw);
                let addr = usize::try_from(start).map_err(|_| Errno::EINVAL)?;
                self.state
                    .registration_for(addr)
                    .map(|(vm, _)| vm)
                    .ok_or(Errno::EINVAL)
            }
            _ => unreachable!("ioctl_target 只接受需要目标地址空间的命令"),
        }
    }
}

/// 取当前任务的 VmSpace（ioctl 在 syscall 上下文执行）。
fn current_task_vm_space() -> Option<Arc<VmSpace>> {
    if !sched::is_ready() {
        return None;
    }
    let task = sched::current_task();
    let payload = task.ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

impl FileOps for UffdFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.take_events(buf)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Linux uffd 不支持 write。
        Err(VfsError::InvalidArgument)
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
        self.state.poll_source.snapshot().0.intersect(interest)
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLOUT) {
            self.state.wait_queue.enqueue(task);
        }
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.state.wait_queue.remove(task);
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn poll_source(&self) -> Option<&PollSource> {
        Some(&self.state.poll_source)
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        let raw_cmd = cmd.raw();
        match raw_cmd {
            UFFDIO_API => self.ioctl_api(arg),
            UFFDIO_REGISTER | UFFDIO_UNREGISTER | UFFDIO_COPY | UFFDIO_ZEROPAGE
            | UFFDIO_WRITEPROTECT | UFFDIO_CONTINUE | UFFDIO_WAKE => {
                // Linux：未完成 API 握手前其它 ioctl 一律 EINVAL。
                if !self.state.api_handshake_done() {
                    return Err(Errno::EINVAL);
                }
                if raw_cmd == UFFDIO_WAKE {
                    return self.ioctl_wake(arg);
                }
                let caller = current_task_vm_space().ok_or(Errno::EINVAL)?;
                let vm = self.ioctl_target(&caller, raw_cmd, arg)?;
                match raw_cmd {
                    UFFDIO_REGISTER => self.ioctl_register(&vm, arg),
                    UFFDIO_UNREGISTER => self.ioctl_unregister(&vm, arg),
                    UFFDIO_COPY => self.ioctl_copy(&vm, arg),
                    UFFDIO_ZEROPAGE => self.ioctl_zeropage(&vm, arg),
                    UFFDIO_WRITEPROTECT => self.ioctl_writeprotect(&vm, arg),
                    UFFDIO_CONTINUE => self.ioctl_continue(&vm, arg),
                    _ => unreachable!(),
                }
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn release(&self) {
        self.state.release_with(&self.state);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 创建 userfaultfd 并注册到当前 fd 表。
pub fn create_uffd_fd(
    fdt: &FdTable,
    cred: Arc<Credentials>,
    nonblock: bool,
    cloexec: bool,
) -> Result<Fd, Errno> {
    let file_flags = OpenOptions {
        access: AccessMode::ReadWrite,
        nonblock,
        ..Default::default()
    };
    let fd_flags = if cloexec {
        FdFlags::CLOEXEC
    } else {
        FdFlags::default()
    };
    anon::create_fd(
        fdt,
        cred,
        file_flags,
        fd_flags,
        Box::new(UffdFileOps::new()),
    )
    .map_err(|err| err.to_errno())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uffd_ioctl_numbers_match_linux_uapi() {
        // 与 Linux <linux/userfaultfd.h> 的 _IOWR/_IOW 展开值逐一核对。
        assert_eq!(UFFDIO_API, 0xC018_AA3F);
        assert_eq!(UFFDIO_REGISTER, 0xC020_AA00);
        assert_eq!(UFFDIO_UNREGISTER, 0x4010_AA01);
        assert_eq!(UFFDIO_WAKE, 0x4010_AA02);
        assert_eq!(UFFDIO_COPY, 0xC028_AA03);
        assert_eq!(UFFDIO_ZEROPAGE, 0xC020_AA04);
        assert_eq!(UFFDIO_WRITEPROTECT, 0xC018_AA05);
        assert_eq!(UFFDIO_CONTINUE, 0xC020_AA06);
    }

    #[test]
    fn ioctl_masks_match_linux_uapi() {
        assert_eq!(UFFD_API_RANGE_IOCTLS, 0x7C); // (1<<2..=6)
        assert_eq!(UFFD_API_REGISTER_IOCTLS, 0x7E); // | (1<<1) UNREGISTER
        assert_eq!(UFFD_API_IOCTLS, 0x7F); // | (1<<0) REGISTER
    }

    #[test]
    fn pagefault_msg_is_32_bytes_with_expected_layout() {
        let msg = UffdMsg::pagefault(
            UFFD_PAGEFAULT_FLAG_WRITE | UFFD_PAGEFAULT_FLAG_WP,
            0x1234_5000,
        );
        let raw = msg.to_bytes();
        assert_eq!(raw.len(), 32);
        assert_eq!(raw[0], UFFD_EVENT_PAGEFAULT);
        assert_eq!(&raw[1..8], &[0; 7]);
        assert_eq!(u64::from_le_bytes(raw[8..16].try_into().unwrap()), 3);
        assert_eq!(
            u64::from_le_bytes(raw[16..24].try_into().unwrap()),
            0x1234_5000
        );
    }

    #[test]
    fn registered_mode_bits_match_linux() {
        assert_eq!(UFFDIO_REGISTER_MODE_MISSING, 1 << 0);
        assert_eq!(UFFDIO_REGISTER_MODE_WP, 1 << 1);
        assert_eq!(UFFDIO_REGISTER_MODE_MINOR, 1 << 2);
    }
}
