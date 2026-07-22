//! LoongArch64 当前逻辑 ASID 跟踪与 shootdown 目标筛选。
//!
//! 每个 CPU 发布“下一段执行上下文所属的逻辑 ASID”：用户地址空间使用
//! `UserPgdInner` 的单调软件 ASID，内核线程和 idle 使用 0。该值不直接替代
//! `CSR_ASID`，而是让页表失效方排除已经切走、且切换路径必定执行完整本地
//! `invtlb` 的 CPU。
//!
//! 并发协议由两侧的全序屏障闭环：
//!
//! 1. 失效方先写 PTE，再执行 [`Ordering::SeqCst`] fence，之后读取历史激活位和
//!    每 CPU 逻辑 ASID；
//! 2. 切换方先发布历史激活位，再以 [`Ordering::SeqCst`] 写入逻辑 ASID，随后
//!    `activate_with_asid_roots` 以 `dbar 0` 开始、安装页表根和硬件 ASID，并在
//!    返回前完整失效本核 TLB；
//! 3. 若扫描看到目标 ASID，目标 CPU 会收到 shootdown；若扫描仍看到旧 ASID，
//!    则扫描在全序上早于新 ASID 发布，后续激活全刷必然覆盖本次 PTE 写；
//! 4. 切离旧 ASID 时可以先发布新 ASID/0，因为调度器已经发布 next 为当前任务，
//!    此后不会再访问 prev 的用户地址；切换完成前仍会执行完整本地失效。
//!
//! 因而不存在“扫描漏掉目标，同时目标的最后一次全刷又早于 PTE 写”的交错。

use core::sync::atomic::{AtomicUsize, Ordering, fence};

pub(crate) const KERNEL_LOGICAL_ASID: usize = 0;

/// 每个 CPU 独占一个缓存行，避免上下文切换时的 SeqCst 发布在核间制造伪共享。
#[repr(align(64))]
struct CurrentAsidSlot(AtomicUsize);

impl CurrentAsidSlot {
    const fn new() -> Self {
        Self(AtomicUsize::new(KERNEL_LOGICAL_ASID))
    }
}

pub(crate) struct CurrentAsidTracker<const CPU_COUNT: usize> {
    slots: [CurrentAsidSlot; CPU_COUNT],
}

impl<const CPU_COUNT: usize> CurrentAsidTracker<CPU_COUNT> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { CurrentAsidSlot::new() }; CPU_COUNT],
        }
    }

    /// 发布当前 CPU 即将激活的逻辑 ASID。
    ///
    /// 调用方必须紧接着执行包含全屏障和完整本地 TLB 失效的地址空间激活，且在
    /// 激活完成前不得执行新地址空间的用户访问。
    pub(crate) fn publish_before_full_flush(&self, cpu: usize, asid: usize) {
        self.slots
            .get(cpu)
            .expect("[arch][mm] logical CPU exceeds ASID tracker")
            .0
            .store(asid, Ordering::SeqCst);
    }

    /// 读取指定 CPU 最近发布的逻辑 ASID，供 shootdown 超时诊断使用。
    pub(crate) fn current(&self, cpu: usize) -> Option<usize> {
        self.slots
            .get(cpu)
            .map(|slot| slot.0.load(Ordering::SeqCst))
    }

    /// 在 PTE 更新后筛出仍运行目标逻辑 ASID 的历史 CPU。
    ///
    /// fence 必须位于候选位图和 ASID 扫描之前，形成“PTE 写 → 扫描”顺序；候选
    /// 位图单调增长，即使本次读取漏掉刚开始激活的 CPU，后者也会在发布 ASID 后
    /// 执行完整本地失效。
    pub(crate) fn target_mask_after_pte_update(
        &self,
        historically_active: &AtomicUsize,
        target_asid: usize,
    ) -> usize {
        fence(Ordering::SeqCst);
        let mut candidates = historically_active.load(Ordering::SeqCst);
        let mut targets = 0usize;
        while candidates != 0 {
            let cpu = candidates.trailing_zeros() as usize;
            candidates &= candidates - 1;
            if cpu < CPU_COUNT && self.slots[cpu].0.load(Ordering::SeqCst) == target_asid {
                targets |= 1usize << cpu;
            }
        }
        targets
    }
}
