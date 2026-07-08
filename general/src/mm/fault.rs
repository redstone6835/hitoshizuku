//! page-fault 的通用分派入口。
//!
//! arch 的 trap handler 命中缺页族时调用 [`dispatch_page_fault`]。本模块：
//!
//! 1. 用注入的 [`super::ops::FaultDecodeOps`] 从 trap frame 提取类型 / 地址 /
//!    来源权级；
//! 2. 若是**内核态**访问用户 buffer 触发：先查 `__ex_table`，命中即返回
//!    `Fixed`（arch 已经把 ERA 改到 fixup 标签，调用方仅需让 ertn 生效）；
//! 3. 若是**用户态**触发：从当前 task 的 ext 表里取 `VmSpace`，交给它做
//!    demand paging / 栈生长 / 访问权限判定。
//!
//! 未注入 ops 时返回 `Kernel(NotInitialized)`——启动早期误调时不要隐式 SIGSEGV。

use alloc::sync::Arc;
use core::any::Any;

use crate::TrapFramePtr;
use crate::mm::ops::{fault_decode_ops, user_pgd_ops};
use crate::mm::vm_space::VmSpace;

/// 缺页的语义分类。内部 arch 侧按硬件异常码翻译出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// 读访问：页表项缺失或无效。
    Load,
    /// 写访问：页表项缺失或无效。
    Store,
    /// 取指：页表项缺失或无效。
    Exec,
    /// 权限：读不允许。
    PermRead,
    /// 权限：尝试写只读页。
    PermWrite,
    /// 权限：执行不允许。
    PermExec,
    /// 权限等级异常：硬件未提供读/写/取指细分，按 VMA 权限重新校正 PTE。
    Privilege,
}

/// 缺页处理结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOutcome {
    /// 已修复：调用方返回 trap frame 让硬件重试即可。
    Fixed,
    /// 应向当前线程投 `SIGSEGV`（或 `SIGBUS`）。
    Segv,
    /// 真内核 bug：无法恢复。
    Kernel(KernelFaultReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelFaultReason {
    /// 早期 trap：ops 还没注入完毕。
    NotInitialized,
    /// 内核态访问用户 buffer 且 `__ex_table` 未命中——真正的非法访问。
    UncaughtKernelAccess,
    /// 当前 task 没挂 VmSpace，但却发生用户态 fault。
    NoVmSpace,
}

/// 由 arch trap handler 在 page-fault 分支调用。
pub fn dispatch_page_fault(tf: TrapFramePtr) -> FaultOutcome {
    let Some(decoder) = fault_decode_ops() else {
        return FaultOutcome::Kernel(KernelFaultReason::NotInitialized);
    };

    let from_user = (decoder.fault_from_user)(tf);
    let addr = (decoder.fault_addr)(tf);
    let kind = (decoder.fault_kind)(tf);
    if !from_user {
        // 内核态访问用户 buffer 时，也允许按当前进程 VMA 进行 lazy fault-in。
        if let Some(vm) = current_task_vm_space() {
            if matches!(vm.handle_fault(addr, kind), FaultOutcome::Fixed) {
                return FaultOutcome::Fixed;
            }
        }

        // MM 无法修复时，再尝试用 __ex_table 把 uaccess 归约为 EFAULT。
        if (decoder.try_fixup_kernel_access)(tf) {
            return FaultOutcome::Fixed;
        }
        return FaultOutcome::Kernel(KernelFaultReason::UncaughtKernelAccess);
    }

    let Some(vm) = current_task_vm_space() else {
        return FaultOutcome::Kernel(KernelFaultReason::NoVmSpace);
    };
    let outcome = vm.handle_fault(addr, kind);
    outcome
}

/// 从当前 task 的 ext 表里取 VmSpace 的 Arc。需要 sched 已就绪。
fn current_task_vm_space() -> Option<Arc<VmSpace>> {
    if !sched::is_ready() {
        return None;
    }
    let task = sched::current_task();
    let payload: Arc<dyn Any + Send + Sync> = task.ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

/// 上层（或 smoketest）想直接走"内核态 fixup"这一分支时的快路径。
pub fn try_kernel_fixup(tf: TrapFramePtr) -> bool {
    fault_decode_ops()
        .map(|ops| (ops.try_fixup_kernel_access)(tf))
        .unwrap_or(false)
}

/// 便利：确认 user_pgd_ops 已注入，否则任何 VmSpace 操作都会 panic。
/// smoketest 入口用。
pub fn user_pgd_ready() -> bool {
    user_pgd_ops().is_some()
}
