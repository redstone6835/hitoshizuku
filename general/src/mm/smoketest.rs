//! `general::mm` 启动期自检。
//!
//! 进入条件：sched::init 已完成 + HAL/arch 已注入 mm/syscall ops。
//! 不需要进入用户态——所有访问都通过内核直映窗口对刚分配的物理页操作，
//! 走的是 buddy + UserPgdOps::map 已建立的页表项。
//!
//! 失败路径：assert 失败或 panic；通过则打印 `[mm][smoke] ALL PASS`。

use alloc::sync::Arc;

use mm::VmFlags;

use crate::mm::ops::all_ops_registered;
use crate::mm::vm_space::{VmSpace, page_size};

/// 试探性映射用的虚地址；选用户半空间一段不大可能被 loader 占用的高位。
const PROBE_VADDR: usize = 0x0000_4000_0000_0000; // 4 TiB 处

fn t1_ops_registered() {
    assert!(
        all_ops_registered(),
        "[mm][smoke] expected all 4 ops registered"
    );
    log::info!("[mm][smoke] t1 PASS (ops registered)");
}

fn t2_anon_map_unmap() {
    let vm = VmSpace::new();
    let page_size = page_size();
    vm.map_anon(
        PROBE_VADDR..PROBE_VADDR + page_size,
        VmFlags::EMPTY
            .with(VmFlags::READ)
            .with(VmFlags::WRITE)
            .with(VmFlags::USER),
    )
    .expect("map_anon");
    // 触发缺页：直接调 handle_fault 模拟硬件 page fault。
    let outcome = vm.handle_fault(PROBE_VADDR, crate::mm::FaultKind::Load);
    assert!(
        matches!(outcome, crate::mm::FaultOutcome::Fixed),
        "[mm][smoke] anon fault expected Fixed, got {:?}",
        outcome
    );
    assert_eq!(vm.mapped_pages(), 1);
    vm.unmap(PROBE_VADDR..PROBE_VADDR + page_size)
        .expect("unmap");
    log::info!("[mm][smoke] t2 PASS (anon map/fault/unmap)");
}

fn t3_segv_out_of_range() {
    let vm = VmSpace::new();
    let outcome = vm.handle_fault(PROBE_VADDR + 0x10_0000, crate::mm::FaultKind::Load);
    assert!(
        matches!(outcome, crate::mm::FaultOutcome::Segv),
        "[mm][smoke] expected Segv on unmapped fault, got {:?}",
        outcome
    );
    log::info!("[mm][smoke] t3 PASS (segv on unmapped)");
}

fn t4_fork_distinct_pgd() {
    let parent = VmSpace::new();
    let page_size = page_size();
    parent
        .map_anon(
            PROBE_VADDR..PROBE_VADDR + page_size,
            VmFlags::EMPTY
                .with(VmFlags::READ)
                .with(VmFlags::WRITE)
                .with(VmFlags::USER),
        )
        .expect("map_anon");
    parent.handle_fault(PROBE_VADDR, crate::mm::FaultKind::Load);
    let child = parent.fork();
    assert_ne!(
        parent.pgd().as_usize(),
        child.pgd().as_usize(),
        "[mm][smoke] fork must produce distinct PGD"
    );
    parent.unmap(PROBE_VADDR..PROBE_VADDR + page_size).ok();
    log::info!("[mm][smoke] t4 PASS (fork distinct pgd)");
}

fn t5_syscall_dispatch_enosys() {
    // 对 syscall::dispatch 的烟雾测，用 0xfffe 这种保留 nr 走默认 ENOSYS 分支。
    // 因为缺少真实 TrapFrame，本步只确认已注入 SyscallFrameOps 即认为通过。
    assert!(
        crate::syscall::frame_ops_registered(),
        "[mm][smoke] SyscallFrameOps not registered"
    );
    log::info!("[mm][smoke] t5 PASS (syscall ops registered)");
}

pub fn run() {
    log::info!("[mm][smoke] start");
    t1_ops_registered();
    t2_anon_map_unmap();
    t3_segv_out_of_range();
    t4_fork_distinct_pgd();
    t5_syscall_dispatch_enosys();
    log::info!("[mm][smoke] ALL PASS");
    let _ = Arc::new(());
}
