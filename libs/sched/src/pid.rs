//! PID 命名层：为 Linux ABI 兼容提供的整数索引。
//!
//! 调度核心完全不依赖 PID；本模块只在**需要对外暴露数字名字**时登场——
//! 典型调用者是 syscall 入口（`getpid`、`kill`、`waitpid`）和 `/proc`。
//!
//! ## 模型
//!
//! - 每个 [`PidNamespace`] 内维护一张 [`PidRegistry`]：slot 数组 + 自由链表。
//! - slot 存的是 `Weak<Task>`：登记不保活，任务生命由父的 `children` 决定。
//! - generation 仅用于内部调试 / 可选的强校验 API；对外暴露的 `pid_t`
//!   就是 slot index，允许 Linux 风格的 PID 重用。
//! - namespace 嵌套形成一棵树。`Task` 在若干祖先 ns 内各有一个 `pid_t`，
//!   存放在 [`crate::task::Task::pid_in_ns`] 中。
//!
//! ## ABI 对齐
//!
//! Linux `pid_t = i32 >= 1`，0 通常作为"本进程组/会话"的语义占位，
//! 负数在 syscall 层有独立含义。本模块分配出来的 pid 始终 `>= 1`。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use errno::Errno;

use crate::sync::Spinlock;
use crate::task::Task;

/// Linux `pid_t` 等价物。0 与负值由调用方（syscall 层）保留给特殊语义。
pub type PidT = i32;

/// `pid <= 0` 为非法。
pub const PID_INVALID: PidT = 0;

/// 单个 namespace 里最多可分配的 pid 数量。对齐 Linux 的可配上限；
/// 真要调大改常量即可，不影响数据结构。
pub const DEFAULT_PID_MAX: PidT = 32768;

#[derive(Clone)]
struct Slot {
    /// 占用该槽的任务；空槽固定为 `Weak::new()`。占用态由 `occupied` 标记。
    task: Weak<Task>,
    /// 该槽目前的 generation；任务每次占用 +1。仅用于内部调试接口。
    generation: u32,
    /// 自由链表：指向下一个空 slot 的 index（`None` 表示链尾）。仅 `!occupied` 时有效。
    next_free: Option<u32>,
    /// true = 已分配给某任务（即便任务已被 drop，仍需显式 release 才归还）。
    occupied: bool,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            task: Weak::new(),
            generation: 0,
            next_free: None,
            occupied: false,
        }
    }
}

/// 单个命名空间内的 pid 分配器。
///
/// 所有公开 API 都在内部短临界区里完成；不会在持锁时调用可能再次进入注册表的
/// 函数（例如 `task.state()` 只触摸原子）。
pub struct PidRegistry {
    inner: Spinlock<RegistryInner>,
}

struct RegistryInner {
    slots: Vec<Slot>,
    free_head: Option<u32>,
    /// 下次扫描起点（对齐 Linux 的 `last_pid`）；回绕到 1。
    next_hint: u32,
    /// 本 ns 允许的最大 slot 数。
    pid_max: u32,
}

impl PidRegistry {
    /// 以 `pid_max` 作为上限新建注册表。pid 1 永远被 init 占用；分配从 hint 往后扫。
    pub fn new(pid_max: PidT) -> Self {
        let cap = pid_max.max(2) as usize;
        let mut slots = Vec::with_capacity(cap.min(64));
        // index 0 占位（对应 pid=0，永远不可分配）。
        slots.push(Slot::empty());
        slots[0].occupied = true;
        Self {
            inner: Spinlock::new(RegistryInner {
                slots,
                free_head: None,
                next_hint: 1,
                pid_max: pid_max.max(2) as u32,
            }),
        }
    }

    /// 为 `task` 分配一个 pid。返回 `None` 表示用尽。
    ///
    /// 分配策略：优先从自由链表取回；否则从 `next_hint` 开始往 `pid_max` 扫描
    /// 并按需 `push` 新 slot。两种路径都保证 `pid_t >= 1`。
    pub fn allocate(&self, task: &Arc<Task>) -> Option<PidT> {
        let mut inner = self.inner.lock();

        if let Some(idx) = inner.free_head.take() {
            let next = inner.slots[idx as usize].next_free.take();
            inner.free_head = next;
            let slot = &mut inner.slots[idx as usize];
            slot.generation = slot.generation.wrapping_add(1);
            slot.task = Arc::downgrade(task);
            slot.occupied = true;
            return Some(idx as PidT);
        }

        let pid_max = inner.pid_max;
        let len = inner.slots.len() as u32;
        if len < pid_max {
            let idx = len;
            inner.slots.push(Slot {
                task: Arc::downgrade(task),
                generation: 1,
                next_free: None,
                occupied: true,
            });
            inner.next_hint = idx.saturating_add(1);
            return Some(idx as PidT);
        }
        None
    }

    /// 为 `task` 占用调用者指定的 pid。该入口服务 clone3 `set_tid_size=1`；
    /// 多 namespace set_tid 需要在更高层先建立完整 namespace 栈后再扩展。
    pub fn allocate_specific(&self, task: &Arc<Task>, pid: PidT) -> Result<PidT, Errno> {
        if pid <= PID_INVALID {
            return Err(Errno::EINVAL);
        }
        let idx = pid as u32;
        let mut inner = self.inner.lock();
        if idx >= inner.pid_max {
            return Err(Errno::EINVAL);
        }

        if (idx as usize) < inner.slots.len() {
            if inner.slots[idx as usize].occupied {
                return Err(Errno::EEXIST);
            }
            remove_from_free_list(&mut inner, idx);
            let slot = &mut inner.slots[idx as usize];
            slot.generation = slot.generation.wrapping_add(1);
            slot.task = Arc::downgrade(task);
            slot.next_free = None;
            slot.occupied = true;
            inner.next_hint = idx.saturating_add(1).max(1);
            return Ok(pid);
        }

        while inner.slots.len() < idx as usize {
            let free_idx = inner.slots.len() as u32;
            let old_head = inner.free_head;
            inner.slots.push(Slot {
                task: Weak::new(),
                generation: 0,
                next_free: old_head,
                occupied: false,
            });
            inner.free_head = Some(free_idx);
        }
        inner.slots.push(Slot {
            task: Arc::downgrade(task),
            generation: 1,
            next_free: None,
            occupied: true,
        });
        inner.next_hint = idx.saturating_add(1).max(1);
        Ok(pid)
    }

    /// 根据 pid 查找任务的弱引用。`pid <= 0` 或越界都返回 `None`。
    pub fn lookup(&self, pid: PidT) -> Option<Weak<Task>> {
        if pid <= PID_INVALID {
            return None;
        }
        let inner = self.inner.lock();
        let slot = inner.slots.get(pid as usize)?;
        if !slot.occupied {
            return None;
        }
        Some(slot.task.clone())
    }

    /// 归还 pid。调用点：父 reap zombie 子时。
    /// 在 slot 归还到自由链表前，Weak 也被清空——下次 allocate 重用时不会
    /// 看到陈旧任务。
    pub fn release(&self, pid: PidT) {
        if pid <= PID_INVALID {
            return;
        }
        let mut inner = self.inner.lock();
        let idx = pid as usize;
        if idx == 0 || idx >= inner.slots.len() {
            return;
        }
        if !inner.slots[idx].occupied {
            return;
        }
        inner.slots[idx].task = Weak::new();
        inner.slots[idx].occupied = false;
        let old_head = inner.free_head;
        inner.slots[idx].next_free = old_head;
        inner.free_head = Some(idx as u32);
    }

    /// 当前已分配的 pid 数（不含 pid=0 占位）。
    pub fn allocated(&self) -> usize {
        let inner = self.inner.lock();
        inner
            .slots
            .iter()
            .enumerate()
            .filter(|(i, s)| *i != 0 && s.occupied)
            .count()
    }

    /// 把当前已占用的 `(pid, Weak<Task>)` 全部拷一份返回。持锁时间仅 Vec 分配
    /// + Weak 克隆；调用方在锁外遍历消费，避免锁嵌套。
    ///
    /// 典型调用场景：`kill(-1, sig)` 需要枚举 ns 内全部进程。
    pub fn snapshot(&self) -> Vec<(PidT, Weak<Task>)> {
        let inner = self.inner.lock();
        let mut out = Vec::with_capacity(inner.slots.len().saturating_sub(1));
        for (idx, slot) in inner.slots.iter().enumerate() {
            if idx == 0 || !slot.occupied {
                continue;
            }
            out.push((idx as PidT, slot.task.clone()));
        }
        out
    }
}

fn remove_from_free_list(inner: &mut RegistryInner, idx: u32) {
    let mut current = inner.free_head;
    let mut prev = None;
    while let Some(cur) = current {
        if cur == idx {
            let next = inner.slots[cur as usize].next_free;
            if let Some(prev_idx) = prev {
                inner.slots[prev_idx as usize].next_free = next;
            } else {
                inner.free_head = next;
            }
            inner.slots[cur as usize].next_free = None;
            return;
        }
        prev = current;
        current = inner.slots[cur as usize].next_free;
    }
}

/// PID 命名空间。嵌套形成一棵树：子 ns 的任务同时在所有祖先 ns 中有各自的 pid。
///
/// 本骨架版本**不处理**越界 unshare / pid 重映射语义——这些只在 syscall 层
/// 组装出来。本模块提供"容器"和"一次分配，多 ns 可见"的 Bookkeeping。
pub struct PidNamespace {
    /// 父 ns。根 ns 的 parent 为 `None`。
    parent: Option<Arc<PidNamespace>>,
    /// 本 ns 从根向下的深度。根为 0。
    depth: u32,
    /// 本 ns 的 pid 分配器。
    registry: PidRegistry,
    /// init ns 里的 pid=1 任务（首次分配后写入）；便于 syscall 层快速取得
    /// "本 ns 的 1 号进程"。`AtomicI32` 避免再绕一次锁。
    ns_init_pid: AtomicI32,
}

impl PidNamespace {
    /// 根 namespace。
    pub fn new_root() -> Arc<Self> {
        Arc::new(Self {
            parent: None,
            depth: 0,
            registry: PidRegistry::new(DEFAULT_PID_MAX),
            ns_init_pid: AtomicI32::new(PID_INVALID),
        })
    }

    /// 新建子 namespace（对应 `CLONE_NEWPID`）。
    pub fn new_child(parent: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(Arc::clone(parent)),
            depth: parent.depth + 1,
            registry: PidRegistry::new(DEFAULT_PID_MAX),
            ns_init_pid: AtomicI32::new(PID_INVALID),
        })
    }

    pub fn parent(&self) -> Option<&Arc<PidNamespace>> {
        self.parent.as_ref()
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn registry(&self) -> &PidRegistry {
        &self.registry
    }

    /// 首次登记 ns-init。允许重复写入同值；写入不同值视作配置错误。
    pub fn set_ns_init_pid(&self, pid: PidT) {
        let prev = self.ns_init_pid.swap(pid, Ordering::AcqRel);
        debug_assert!(
            prev == PID_INVALID || prev == pid,
            "[sched][pid] ns init pid overwritten: {} -> {}",
            prev,
            pid,
        );
    }

    pub fn ns_init_pid(&self) -> PidT {
        self.ns_init_pid.load(Ordering::Acquire)
    }
}
