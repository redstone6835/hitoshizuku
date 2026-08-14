//! arch 注入的契约：用户虚拟地址布局、页表、用户内存访问、fault 解码。
//!
//! 沿用项目里 `sched::arch_hooks` / `allocator` 已经验证过的"AtomicPtr<T> +
//! Release/Acquire"注入模式。本模块**只**定义 Ops 结构与 register/取值入口，
//! 不带任何业务实现——arch 侧 fill 之、上层（VmSpace、user_access、fault
//! dispatcher）read 之。

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use mm::{UserAccessError, VmFlags};

use crate::TrapFramePtr;

// ── UserVmLayoutOps ──────────────────────────────────────────────────────────

/// 用户虚拟地址空间布局契约。
///
/// 这些值决定用户态基础页粒度、brk/mmap 自动分配区间、栈默认位置以及动态
/// ELF 装载基址。它们属于架构 ABI / VA 布局选择，general 只消费语义，不
/// 固化具体地址。
#[repr(C)]
pub struct UserVmLayoutOps {
    pub page_size: usize,
    pub max_grows_down_bytes: usize,
    pub user_heap_base: usize,
    pub user_mmap_base: usize,
    pub user_mmap_limit: usize,
    pub default_stack_top: usize,
    pub default_stack_size: usize,
    pub main_pie_base: usize,
    pub interp_base: usize,
    pub vdso_base: usize,
}

unsafe impl Sync for UserVmLayoutOps {}
unsafe impl Send for UserVmLayoutOps {}

static USER_VM_LAYOUT_OPS: AtomicPtr<UserVmLayoutOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_user_vm_layout(ops: &'static UserVmLayoutOps) {
    assert!(
        ops.page_size != 0 && ops.page_size.is_power_of_two(),
        "[mm] user page size must be a non-zero power of two"
    );
    assert!(
        ops.page_size >= allocator::PAGE_SIZE
            && ops.page_size % allocator::PAGE_SIZE == 0
            && (ops.page_size / allocator::PAGE_SIZE).is_power_of_two(),
        "[mm] user page size must be a power-of-two multiple of allocator page granule"
    );
    assert!(
        ops.max_grows_down_bytes % ops.page_size == 0,
        "[mm] grows-down limit must be page aligned"
    );
    assert!(
        ops.user_heap_base % ops.page_size == 0,
        "[mm] user heap base must be page aligned"
    );
    assert!(
        ops.user_mmap_base % ops.page_size == 0 && ops.user_mmap_limit % ops.page_size == 0,
        "[mm] user mmap bounds must be page aligned"
    );
    assert!(
        ops.user_mmap_base < ops.user_mmap_limit,
        "[mm] user mmap bounds are invalid"
    );
    assert!(
        ops.default_stack_top % ops.page_size == 0
            && ops.default_stack_size != 0
            && ops.default_stack_size % ops.page_size == 0,
        "[mm] user stack layout must be page aligned"
    );
    assert!(
        ops.main_pie_base % ops.page_size == 0 && ops.interp_base % ops.page_size == 0,
        "[mm] ELF load bases must be page aligned"
    );
    assert!(
        ops.vdso_base % ops.page_size == 0 && ops.vdso_base != 0,
        "[mm] vDSO base must be page aligned and non-zero"
    );
    USER_VM_LAYOUT_OPS.store(ops as *const _ as *mut _, Ordering::Release);
}

pub fn user_vm_layout() -> Option<&'static UserVmLayoutOps> {
    let ptr = USER_VM_LAYOUT_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_user_vm_layout 仅接受 'static 指针；Acquire/Release 配对。
        Some(unsafe { &*(ptr as *const UserVmLayoutOps) })
    }
}

/// 用户态页表句柄。general 不知道 arch 怎么编码——只把它当 opaque non-null
/// 指针传递。arch 内部把它解释成自己定义的 `UserPgdInner` 结构指针。
///
/// 设计点：
/// - `NonNull<()>` 保留"非空"不变式，方便 `Option<PgdHandle>` 的 layout 优化；
/// - 手动 `Send + Sync`：句柄本身只是一个标识，跨核传递的安全性由 arch 的
///   `UserPgdOps::activate` 保证（写入 PGDL 需要原子或关中断）。
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct PgdHandle(NonNull<()>);

// Safety: PgdHandle 只是 arch 私有结构的标识；上层不解引用。跨核同步由
// activate / map / unmap 路径自身保证（arch 实现内部会做 TLB 同步）。
unsafe impl Send for PgdHandle {}
unsafe impl Sync for PgdHandle {}

impl PgdHandle {
    /// 由 arch 内部构造。general 端只在测试 / 调试时直接拿到裸指针。
    #[inline]
    pub fn from_raw(ptr: NonNull<()>) -> Self {
        Self(ptr)
    }

    #[inline]
    pub fn as_raw(self) -> NonNull<()> {
        self.0
    }

    #[inline]
    pub fn as_usize(self) -> usize {
        self.0.as_ptr() as usize
    }
}

impl core::fmt::Debug for PgdHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PgdHandle({:#x})", self.as_usize())
    }
}

// ── UserPgdOps ───────────────────────────────────────────────────────────────

/// 用户态页表操作契约。arch 提供整张 vtable；VmSpace 调用时永远走这里，
/// 不直接接触 arch crate。
#[repr(C)]
pub struct UserPgdOps {
    /// 新分配一个用户态 PGD（含克隆内核半映射）。
    pub new_pgd_for_user: fn() -> PgdHandle,

    /// 释放 PGD（连同它独占的页表页）。VmSpace::Drop 调用。
    ///
    /// # Safety
    /// `handle` 必须是 [`new_pgd_for_user`] 返回过且尚未释放的句柄。调用后
    /// 任何对该 handle 的访问都是 UB。
    pub drop_pgd: unsafe fn(handle: PgdHandle),

    /// 清零一段尚未发布的用户物理页 direct-map 虚拟地址。
    ///
    /// arch 可以在这里使用 `cbo.zero`、向量存储等 ISA 专用清页实现；general
    /// 保证范围由分配器独占，且在回调完成前不会写入 resident ledger 或页表。
    ///
    /// # Safety
    /// `vaddr` 必须按用户基础页对齐并覆盖至少 `len` 个可写字节；`len` 必须是
    /// 用户基础页大小的整数倍。
    pub zero_user_pages: unsafe fn(vaddr: usize, len: usize),

    /// 在 `vaddr` 处映射一页 4 KiB 物理页 `paddr`，权限 `flags`。
    ///
    /// # Safety
    /// `vaddr` 必须 4K 对齐；`paddr` 同；`flags` 必须含 [`VmFlags::USER`]。
    pub map: unsafe fn(
        handle: PgdHandle,
        vaddr: usize,
        paddr: usize,
        flags: VmFlags,
    ) -> Result<(), crate::MapError>,

    /// 从 `vaddr` 起连续安装一批基础页，物理页地址由 `paddrs` 逐页给出。
    ///
    /// 返回值中的 `mapped` 表示错误前已经生效的连续前缀。调用方必须为这部分
    /// 页面建立 resident 所有权账本，不能因为批次后缀失败而直接释放它们。
    ///
    /// # Safety
    /// `handle` 必须合法；`vaddr` 和所有 `paddrs` 必须按基础页对齐；目标叶
    /// PTE 必须为空，`flags` 必须含 [`VmFlags::USER`]。
    pub map_pages: unsafe fn(
        handle: PgdHandle,
        vaddr: usize,
        paddrs: &[usize],
        flags: VmFlags,
    ) -> crate::MapBatchResult,

    /// 发布一段从“无叶 PTE”变为“有效叶 PTE”的新映射。
    ///
    /// 本回调只保证先前页表写对当前 CPU 的硬件页表遍历可见，并清除当前 CPU
    /// 可能缓存的无效 translation；不会等待其它 CPU，也不能用于替换、解除映射
    /// 或权限变更。其它正在运行同一地址空间的 CPU 若命中旧无效状态，会在缺页
    /// 重试路径调用同一回调完成本地收敛。
    ///
    /// # Safety
    /// `handle` 必须合法；区间必须按基础页对齐，且调用方必须确认其中所有新写入
    /// 的叶 PTE 此前均无有效映射。
    pub publish_new_mapping: unsafe fn(handle: PgdHandle, vaddr: usize, len: usize),

    /// 解除 `[vaddr, vaddr+len)` 区间的映射。`len` 必须是 4K 倍数。
    ///
    /// # Safety
    /// 区间需位于用户半空间且属于本 PGD 拥有的页。
    /// 页表修改完成后不会自动执行跨 CPU TLB 失效；调用方必须在不持有内部
    /// 自旋锁的情况下单独调用 [`Self::invalidate_range`]。
    pub unmap: unsafe fn(handle: PgdHandle, vaddr: usize, len: usize),

    /// 改变现有映射的权限。
    /// 页表修改完成后不会自动执行跨 CPU TLB 失效；调用方必须在不持有内部
    /// 自旋锁的情况下单独调用 [`Self::invalidate_range`]。
    pub protect: unsafe fn(handle: PgdHandle, vaddr: usize, len: usize, flags: VmFlags),

    /// fork 时把 src 的 [`range`] 区间已映射页拷到 dst。保底"全深拷"语义；
    /// 后续接 COW 时本字段签名不变，只是 arch 实现里多个引用计数路径。
    ///
    /// # Safety
    /// src / dst 都必须是合法 PGD；range 4K 对齐；调用方保证 dst 当前没有
    /// 其它写入者。
    pub clone_for_fork: unsafe fn(src: PgdHandle, dst: PgdHandle, range: core::ops::Range<usize>),

    /// 把当前 CPU 切到此 PGD（写 PGDL，flush 必要 TLB）。
    ///
    /// # Safety
    /// 通常只在 `schedule_once` 切换 task 之后立刻调用。
    pub activate: unsafe fn(handle: PgdHandle),

    /// 把当前 CPU 切回内核页表。
    ///
    /// idle 和纯内核线程没有用户 `VmSpace`，但不能继续沿用上一个
    /// 用户任务的 PGD；该 PGD 可能在退出回收路径中立即释放。
    ///
    /// # Safety
    /// 通常只在调度器已决定切向无用户地址空间任务时调用。
    pub activate_kernel: unsafe fn(),

    /// 让 `[vaddr, vaddr+len)` 的 TLB 项失效。
    pub invalidate_range: unsafe fn(handle: PgdHandle, vaddr: usize, len: usize),

    /// 统计 `[vaddr, vaddr+len)` 内当前实际存在的 4 KiB 用户页映射数。
    pub count_mapped: unsafe fn(handle: PgdHandle, vaddr: usize, len: usize) -> usize,
}

// Safety: 仅函数指针。
unsafe impl Sync for UserPgdOps {}
unsafe impl Send for UserPgdOps {}

/// 用户页表更新的发布范围。
///
/// 新建映射没有可越过资源回收边界的旧有效 translation，只需当前 CPU 本地
/// 收敛；其它更新可能遗留仍可访问旧物理页或旧权限的 TLB 项，必须同步所有曾经
/// 激活过该地址空间的 CPU。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UserPteUpdate {
    NewMapping,
    ExistingMapping,
}

impl UserPteUpdate {
    /// 按更新类型选择本地发布或同步跨核失效。
    ///
    /// # Safety
    /// 调用方必须满足所选 [`UserPgdOps`] 回调的句柄、地址范围和映射状态契约。
    pub(super) unsafe fn publish(
        self,
        ops: &UserPgdOps,
        handle: PgdHandle,
        vaddr: usize,
        len: usize,
    ) {
        match self {
            Self::NewMapping => unsafe { (ops.publish_new_mapping)(handle, vaddr, len) },
            Self::ExistingMapping => unsafe { (ops.invalidate_range)(handle, vaddr, len) },
        }
    }
}

// ── UserAccessOps ────────────────────────────────────────────────────────────

/// 用户内存访问原语。arch 实现内部使用 `__ex_table` 做缺页 fixup。
#[repr(C)]
pub struct UserAccessOps {
    /// # Safety
    /// `dst` 必须有 `len` 字节可写；`src_user` 由用户进程地址空间提供，可能
    /// 任何值（包括 NULL / 越界）——arch 内部捕获 fault 并返回 `Err`。
    pub copy_from_user:
        unsafe fn(dst: *mut u8, src_user: usize, len: usize) -> Result<(), UserAccessError>,

    /// # Safety
    /// 对偶 of [`copy_from_user`]。
    pub copy_to_user:
        unsafe fn(dst_user: usize, src: *const u8, len: usize) -> Result<(), UserAccessError>,

    /// 读取 C 字符串长度（不含 NUL），最多扫 `max` 字节。
    ///
    /// # Safety
    /// 同上；遇到 NUL 终止，否则在 `max` 字节后返回 `TooLong`。
    pub strnlen_user: unsafe fn(start_user: usize, max: usize) -> Result<usize, UserAccessError>,
}

unsafe impl Sync for UserAccessOps {}
unsafe impl Send for UserAccessOps {}

// ── FaultDecodeOps ───────────────────────────────────────────────────────────

/// 从 trap frame 提取缺页 / 用户访问 fault 信息。
#[repr(C)]
pub struct FaultDecodeOps {
    /// 缺页类型（读 / 写 / 取指 / 权限不足）。
    pub fault_kind: fn(TrapFramePtr) -> crate::mm::FaultKind,
    /// 触发缺页的虚拟地址（BADV）。
    pub fault_addr: fn(TrapFramePtr) -> usize,
    /// 缺页发生时硬件特权级是否为用户态。
    pub fault_from_user: fn(TrapFramePtr) -> bool,
    /// 内核态访问用户 buffer 触发的故障：查 `__ex_table`，命中改写 ERA → 返 true。
    /// 没匹配返 false，由上层判定为真内核 bug。
    pub try_fixup_kernel_access: fn(TrapFramePtr) -> bool,
}

unsafe impl Sync for FaultDecodeOps {}
unsafe impl Send for FaultDecodeOps {}

// ── 注入点（AtomicPtr 模式） ──────────────────────────────────────────────────

static USER_PGD_OPS: AtomicPtr<UserPgdOps> = AtomicPtr::new(core::ptr::null_mut());
static USER_ACCESS_OPS: AtomicPtr<UserAccessOps> = AtomicPtr::new(core::ptr::null_mut());
static FAULT_DECODE_OPS: AtomicPtr<FaultDecodeOps> = AtomicPtr::new(core::ptr::null_mut());

macro_rules! reg_and_get {
    ($reg:ident, $get:ident, $static:ident, $ty:ty) => {
        pub fn $reg(ops: &'static $ty) {
            // Safety: AtomicPtr::store 与 load 配对的 Release/Acquire 保证 vtable
            // 的字段写入对所有读者可见。
            $static.store(ops as *const _ as *mut _, Ordering::Release);
        }
        pub fn $get() -> Option<&'static $ty> {
            let ptr = $static.load(Ordering::Acquire);
            if ptr.is_null() {
                None
            } else {
                // Safety: 仅由 register_* 写入 'static 引用，永不变；
                // Acquire 读与 Release 写配对。
                Some(unsafe { &*(ptr as *const $ty) })
            }
        }
    };
}

reg_and_get!(register_user_pgd, user_pgd_ops, USER_PGD_OPS, UserPgdOps);
reg_and_get!(
    register_user_access,
    user_access_ops,
    USER_ACCESS_OPS,
    UserAccessOps
);
reg_and_get!(
    register_fault_decode,
    fault_decode_ops,
    FAULT_DECODE_OPS,
    FaultDecodeOps
);

/// 让外部测试代码确认属于 general::mm 的 Ops 是否都到位。启动期 smoketest 用。
/// VmSwitchOps 不在此列——它归 `sched::arch_hooks`，由 sched 自行检查。
pub fn all_ops_registered() -> bool {
    user_vm_layout().is_some()
        && user_pgd_ops().is_some()
        && user_access_ops().is_some()
        && fault_decode_ops().is_some()
}

#[cfg(test)]
mod tests {
    use core::ops::Range;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{PgdHandle, UserPgdOps, UserPteUpdate};
    use mm::VmFlags;

    static LOCAL_PUBLICATIONS: AtomicUsize = AtomicUsize::new(0);
    static REMOTE_INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);

    fn new_pgd() -> PgdHandle {
        PgdHandle::from_raw(NonNull::dangling())
    }

    unsafe fn ignore_handle(_: PgdHandle) {}
    unsafe fn ignore_zero_user_pages(_: usize, _: usize) {}
    unsafe fn ignore_activate_kernel() {}
    unsafe fn ignore_map(
        _: PgdHandle,
        _: usize,
        _: usize,
        _: VmFlags,
    ) -> Result<(), crate::MapError> {
        Ok(())
    }
    unsafe fn ignore_map_pages(
        _: PgdHandle,
        _: usize,
        paddrs: &[usize],
        _: VmFlags,
    ) -> crate::MapBatchResult {
        crate::MapBatchResult {
            mapped: paddrs.len(),
            error: None,
        }
    }
    unsafe fn record_local(_: PgdHandle, _: usize, _: usize) {
        LOCAL_PUBLICATIONS.fetch_add(1, Ordering::Relaxed);
    }
    unsafe fn ignore_protect(_: PgdHandle, _: usize, _: usize, _: VmFlags) {}
    unsafe fn ignore_clone(_: PgdHandle, _: PgdHandle, _: Range<usize>) {}
    unsafe fn record_remote(_: PgdHandle, _: usize, _: usize) {
        REMOTE_INVALIDATIONS.fetch_add(1, Ordering::Relaxed);
    }
    unsafe fn count_none(_: PgdHandle, _: usize, _: usize) -> usize {
        0
    }

    fn fake_ops() -> UserPgdOps {
        UserPgdOps {
            new_pgd_for_user: new_pgd,
            drop_pgd: ignore_handle,
            zero_user_pages: ignore_zero_user_pages,
            map: ignore_map,
            map_pages: ignore_map_pages,
            publish_new_mapping: record_local,
            unmap: record_remote,
            protect: ignore_protect,
            clone_for_fork: ignore_clone,
            activate: ignore_handle,
            activate_kernel: ignore_activate_kernel,
            invalidate_range: record_remote,
            count_mapped: count_none,
        }
    }

    #[test]
    fn user_pte_update_routes_local_and_remote_publication() {
        LOCAL_PUBLICATIONS.store(0, Ordering::Relaxed);
        REMOTE_INVALIDATIONS.store(0, Ordering::Relaxed);
        let ops = fake_ops();
        let handle = (ops.new_pgd_for_user)();

        // Safety: fake 回调不解引用句柄；地址范围仅用于验证 vtable 路由。
        unsafe {
            UserPteUpdate::NewMapping.publish(&ops, handle, 0x1000, 0x1000);
            UserPteUpdate::ExistingMapping.publish(&ops, handle, 0x2000, 0x1000);
        }

        assert_eq!(LOCAL_PUBLICATIONS.load(Ordering::Relaxed), 1);
        assert_eq!(REMOTE_INVALIDATIONS.load(Ordering::Relaxed), 1);
    }
}
