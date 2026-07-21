//! 架构相关能力的注入点。
//!
//! 调度器主逻辑（入队、挑选、唤醒）与 ISA 无关，但"新内核线程的寄存器初始状态"
//! 和"保存当前寄存器上下文并跳转到另一段上下文"这两件事天生必须落到汇编。
//! 我们复用项目里 `allocator` / `console` / `log` 已经验证过的做法：
//!
//! - 所有实现由 `arch` 侧提供，通过 [`register`] 一次性装入 `AtomicPtr`；
//! - 调度器调用处用 [`ops`] 读取，Release/Acquire 配对保证看到的 vtable
//!   与实现指针一致；
//! - `libs/sched` 本身对具体 arch 零依赖，ARCHITECTURE.md §4 的要求。
//!
//! ## `ArchContextOps`
//!
//! 只包含跨架构必要的**最小**契约：
//!
//! - [`context_size`] / [`context_align`]：`libs/sched` 按此为每个 Task 的
//!   上下文存储区分配对齐缓冲；
//! - [`init_kernel_context`]：把新 kernel 线程的"首次被切入"状态写进缓冲——
//!   通常是：初始栈顶 + 入口地址 + 一次 `switch_context` 恢复后能 `ret`
//!   到入口函数的寄存器布局；
//! - [`switch_context`]：保存当前寄存器、从目标缓冲恢复、跳走。**由汇编实现**。
//!
//! trap frame（用户态 syscall/irq 栈上布局）由已有 `general::TaskOps` 管，
//! 与本模块职责不重叠：这里只负责**内核线程的纯内核上下文切换**。用户
//! 线程的 kernel-to-kernel 切换同样走这条通道。

use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

/// 新内核线程的入口函数签名。`arg` 通过 ABI 规定的第一个参数寄存器传入。
pub type KernelEntry = unsafe extern "C" fn(arg: usize) -> !;

/// 架构侧必须实现的上下文切换契约。
///
/// 函数指针通过 [`register`] 注入；实现必须 `Send + Sync`（事实上所有函数
/// 指针都满足）。`init_kernel_context` 可以是普通 Rust ABI（只做数据写入）；
/// `switch_context` 必须是 `extern "C"`，因为实现是 `#[naked]` 汇编、依赖
/// 固定的参数寄存器约定。
#[repr(C)]
pub struct ArchContextOps {
    /// 单个上下文保存区所需字节数。必须覆盖被调用保存寄存器 + ra + sp。
    pub context_size: usize,
    /// 保存区对齐要求。典型为 16 字节（ABI 栈对齐）。
    pub context_align: usize,
    /// 初始化一个新内核线程的上下文缓冲。
    ///
    /// - `ctx`：指向 `context_size` 字节、按 `context_align` 对齐的缓冲；
    /// - `stack_top`：该线程内核栈的**逻辑栈顶**（高地址端，首次 push 前
    ///   的值）；调用方保证 `stack_top` 已按 ABI 对齐；
    /// - `entry`：首次被调度时要跳入的函数；
    /// - `arg`：传给 `entry` 的第一个参数。
    ///
    /// # Safety
    /// - `ctx` 必须满足 size/align 要求且是唯一写者；
    /// - `stack_top` 指向的内存范围必须属于调用方新分配的内核栈，在该 Task
    ///   活跃期间不被回收。
    pub init_kernel_context:
        unsafe fn(ctx: NonNull<u8>, stack_top: usize, entry: KernelEntry, arg: usize),
    /// 切换内核上下文：把当前寄存器保存进 `prev`，从 `next` 恢复后跳走。
    ///
    /// 必须 `extern "C"` —— 实现通常是 `#[naked]` 汇编，依赖确定的参数寄存器。
    ///
    /// # Safety
    /// - `prev`、`next` 必须都是之前由 [`init_kernel_context`] 初始化过的
    ///   缓冲，或当前线程用于"保存再回来"的合法缓冲；
    /// - 调用方必须持有调度锁（避免同一 ctx 同时被两个核保存）；
    /// - 函数返回后，调用方看到的是被切出前的世界；如果 `next` 是从未跑过
    ///   的新线程，控制流将跳到其 entry，**不会返回**。
    pub switch_context: unsafe extern "C" fn(prev: NonNull<u8>, next: NonNull<u8>),
}

// Safety: 仅包含 `usize` 与函数指针，全部 POD。
unsafe impl Sync for ArchContextOps {}
unsafe impl Send for ArchContextOps {}

/// 注入点。值为 `*const ArchContextOps`，`null` 表示未装载。
static ARCH_OPS: AtomicPtr<ArchContextOps> = AtomicPtr::new(core::ptr::null_mut());

fn register_once<T>(slot: &AtomicPtr<T>, ptr: *mut T, name: &str) {
    match slot.compare_exchange(
        core::ptr::null_mut(),
        ptr,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(prev) if prev == ptr => {}
        Err(_) => panic!("[sched] {} already registered", name),
    }
}

/// 由 `arch` 层在启动早期调用一次，装入跨架构切换契约。
///
/// 约定 `ops` 指向 `'static` 数据（由 `arch` 侧定义成 `static`）——本函数不取
/// 任何所有权，仅保存裸指针以便后续 Acquire 读取。
pub fn register(ops: &'static ArchContextOps) {
    assert!(ops.context_size != 0, "[sched] arch context size is zero");
    assert!(
        ops.context_align != 0 && ops.context_align.is_power_of_two(),
        "[sched] arch context align is invalid"
    );
    assert!(
        Layout::from_size_align(ops.context_size, ops.context_align).is_ok(),
        "[sched] arch context layout is invalid"
    );
    register_once(&ARCH_OPS, ops as *const _ as *mut _, "ArchContextOps");
}

/// 读取当前已注入的契约。未注入时返回 `None`。
///
/// 使用方式：
/// ```ignore
/// let ops = sched::arch_hooks::ops().expect("[sched] arch ops not registered");
/// let size = ops.context_size;
/// ```
pub fn ops() -> Option<&'static ArchContextOps> {
    let ptr = ARCH_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: 唯一写入路径 [`register`] 要求 `'static`，一旦非空即永久有效。
        // Acquire load 与 register 的 Release store 配对，保证 vtable 字段可见。
        Some(unsafe { &*(ptr as *const ArchContextOps) })
    }
}

/// 便捷访问：已注入时返回，未注入 panic。在调度热路径使用。
pub fn ops_or_panic() -> &'static ArchContextOps {
    ops().expect("[sched] ArchContextOps not registered — arch init missing")
}

// ── ArchTimeOps ──────────────────────────────────────────────────────────────
//
// 时间戳源 + 当前 CPU id。两件事都被调度核心高频访问（tick 推进虚拟时间、
// 选 per-CPU runqueue 槽），但都属于 ISA 局域的能力，必须按本 crate 一贯的
// "AtomicPtr 注入 + Acquire 读取"模式从 arch 取。
//
// 未注册时由 [`time_or_zero_cpu`] / [`time_now_ns_or_zero`] 给出全 0 / cpu=0 的
// 安全回退，方便启动期 sched::init() 之前的代码路径仍然可调用 sched 的访问器。

/// 跨架构的时间 / CPU 索引契约。
#[repr(C)]
pub struct ArchTimeOps {
    /// 单调纳秒时间戳。配合 EEVDF 推进虚拟时间。
    pub now_ns: fn() -> u64,
    /// 当前 CPU 的逻辑 id。`0..NR_CPUS`。
    ///
    /// AP 启动后必须返回稳定且连续的逻辑 CPU id。
    /// 回退到 boot CPU 的槽位。
    pub current_cpu_id: fn() -> usize,
}

// Safety: 仅函数指针，POD。
unsafe impl Sync for ArchTimeOps {}
unsafe impl Send for ArchTimeOps {}

static TIME_OPS: AtomicPtr<ArchTimeOps> = AtomicPtr::new(core::ptr::null_mut());

/// 注入 [`ArchTimeOps`]。
pub fn register_time(ops: &'static ArchTimeOps) {
    register_once(&TIME_OPS, ops as *const _ as *mut _, "ArchTimeOps");
}

/// 取已注入的时间契约。
pub fn time() -> Option<&'static ArchTimeOps> {
    let ptr = TIME_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_time 仅接受 'static；非空指针永久有效。Acquire 与
        // register_time 的 Release 配对。
        Some(unsafe { &*(ptr as *const ArchTimeOps) })
    }
}

// ── ArchDeadlineTimerOps ────────────────────────────────────────────────────

/// 调度器软件截止时间到架构本地定时器的重编程契约。
///
/// 调度核心只发布归属于当前 CPU 的定时等待中最早的绝对纳秒截止时间，不假设
/// 底层定时器是周期模式还是比较器模式。架构实现必须保证下一次本地定时器中断
/// 不晚于该截止时间，同时仍需保留常规调度 tick 的最大间隔。deadline 在登记
/// 时绑定到本地 CPU，避免多个 CPU 为同一等待同时触发中断并争用全局队列。
#[repr(C)]
pub struct ArchDeadlineTimerOps {
    /// 重编程当前 CPU 的本地定时器。
    ///
    /// `Some(deadline_ns)` 使用与 [`ArchTimeOps::now_ns`] 相同的绝对时间域；
    /// `None` 表示当前没有软件截止时间，架构应恢复常规调度 tick。调用本函数
    /// 时调度器不会持有定时等待队列锁。
    pub reprogram: fn(deadline_ns: Option<u64>),
}

// Safety: 仅包含函数指针。
unsafe impl Sync for ArchDeadlineTimerOps {}
unsafe impl Send for ArchDeadlineTimerOps {}

static DEADLINE_TIMER_OPS: AtomicPtr<ArchDeadlineTimerOps> = AtomicPtr::new(core::ptr::null_mut());

/// 注入架构本地 deadline timer 契约。
pub fn register_deadline_timer(ops: &'static ArchDeadlineTimerOps) {
    register_once(
        &DEADLINE_TIMER_OPS,
        ops as *const _ as *mut _,
        "ArchDeadlineTimerOps",
    );
}

/// 读取已注入的 deadline timer 契约。
pub fn deadline_timer() -> Option<&'static ArchDeadlineTimerOps> {
    let ptr = DEADLINE_TIMER_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_deadline_timer 仅接受 'static；Acquire 与注册时的
        // Release 配对，因此函数表一旦可见便永久有效。
        Some(unsafe { &*(ptr as *const ArchDeadlineTimerOps) })
    }
}

// ── CpuControlOps ────────────────────────────────────────────────────────────

/// 跨 CPU 调度控制契约。AP/真实 IPI 未接入时可以不注册；sched 会只置本地
/// resched 标志，等待后续 CPU 在调度边界主动消费。
#[repr(C)]
pub struct CpuControlOps {
    pub send_resched: fn(cpu_id: usize),
    /// 向目标 CPU 发送 membarrier rendezvous IPI。返回 false 表示没有成功投递。
    pub send_membarrier: fn(cpu_id: usize) -> bool,
    /// 在当前 CPU 上服务不能推迟到普通中断返回路径的架构请求。
    ///
    /// 调度器和依赖调度器的子系统会在自旋锁等待期间调用本钩子。回调可能运行在
    /// 中断关闭且调用方正等待任意内核锁的上下文中，因此必须满足以下约束：
    ///
    /// - 只能执行有界的原子操作、内存屏障和本地 TLB/I-cache 失效；
    /// - 不得分配、阻塞、获取锁、触发调度或调用日志设施；
    /// - 没有待处理请求时必须快速返回。
    pub poll_urgent: fn(),
    pub is_online: fn(cpu_id: usize) -> bool,
}

unsafe impl Sync for CpuControlOps {}
unsafe impl Send for CpuControlOps {}

static CPU_CONTROL_OPS: AtomicPtr<CpuControlOps> = AtomicPtr::new(core::ptr::null_mut());

/// 支持 SMP 的架构在接通 AP 和 reschedule IPI 后注册该接口。
/// `send_resched` 只负责通知目标 CPU，实际切换仍在安全的调度边界完成。
pub fn register_cpu_control(ops: &'static CpuControlOps) {
    register_once(&CPU_CONTROL_OPS, ops as *const _ as *mut _, "CpuControlOps");
}

pub fn cpu_control() -> Option<&'static CpuControlOps> {
    let ptr = CPU_CONTROL_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_cpu_control only stores 'static ops.
        Some(unsafe { &*(ptr as *const CpuControlOps) })
    }
}

#[inline]
fn dispatch_urgent_work(ops: Option<&CpuControlOps>) {
    if let Some(ops) = ops {
        (ops.poll_urgent)();
    }
}

/// 在当前 CPU 上协作处理架构级紧急请求。
///
/// 该函数专供不能安全打开中断或让出 CPU 的自旋等待路径使用。未注册
/// [`CpuControlOps`] 时为无操作。
#[kernel_symbols::export(
    name = "sched.arch_hooks.poll_urgent_work",
    contract = "kernel.sched.control@1",
    version = 1,
    capabilities = kernel_symbols::capability::SCHED_QUERY,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
#[inline(never)]
pub fn poll_urgent_work() {
    dispatch_urgent_work(cpu_control());
}

#[cfg(test)]
mod cpu_control_tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{CpuControlOps, dispatch_urgent_work};

    static POLLS: AtomicUsize = AtomicUsize::new(0);

    fn no_cpu_action(_: usize) {}
    fn no_membarrier(_: usize) -> bool {
        false
    }
    fn poll() {
        POLLS.fetch_add(1, Ordering::Relaxed);
    }
    fn offline(_: usize) -> bool {
        false
    }

    static TEST_OPS: CpuControlOps = CpuControlOps {
        send_resched: no_cpu_action,
        send_membarrier: no_membarrier,
        poll_urgent: poll,
        is_online: offline,
    };

    #[test]
    fn urgent_dispatch_is_optional_and_invokes_registered_hook() {
        POLLS.store(0, Ordering::Relaxed);
        dispatch_urgent_work(None);
        assert_eq!(POLLS.load(Ordering::Relaxed), 0);
        dispatch_urgent_work(Some(&TEST_OPS));
        assert_eq!(POLLS.load(Ordering::Relaxed), 1);
    }
}

// ── ArchTrapOps ──────────────────────────────────────────────────────────────
//
// 上下文切换发生后，必须把"内核态 trap 入口栈顶"指向新任务的内核栈，否则
// 切到 next 后第一条 trap 会写到旧 task 的栈上。这个动作必须由 sched
// 在 switch_context 之后执行，保持
// "切换 + 设栈"成为原子序列。

/// 切换后由 sched 调用的"trap 入口栈"安装契约。
#[repr(C)]
pub struct ArchTrapOps {
    /// 把内核 trap 入口栈顶设为 `stack_top`（高地址端）。
    ///
    /// # Safety
    /// `stack_top` 必须落在当前线程（即即将运行的 next）持有的内核栈范围内，
    /// 且按当前架构 ABI 对齐。本函数只写硬件寄存器，不做范围检查。
    pub set_kernel_trap_stack: unsafe fn(stack_top: usize),
}

// Safety: 仅函数指针。
unsafe impl Sync for ArchTrapOps {}
unsafe impl Send for ArchTrapOps {}

static TRAP_OPS: AtomicPtr<ArchTrapOps> = AtomicPtr::new(core::ptr::null_mut());

/// 注入 [`ArchTrapOps`]。
pub fn register_trap(ops: &'static ArchTrapOps) {
    register_once(&TRAP_OPS, ops as *const _ as *mut _, "ArchTrapOps");
}

/// 取已注入的 trap 契约。
pub fn trap() -> Option<&'static ArchTrapOps> {
    let ptr = TRAP_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_trap 仅接受 'static；Acquire/Release 配对。
        Some(unsafe { &*(ptr as *const ArchTrapOps) })
    }
}

// ── VmSwitchOps ──────────────────────────────────────────────────────────────
//
// 让 sched 在 schedule_once 选定 next 后、内核上下文切换之前触发"用户地址空间
// 切换"——但 sched 本身不认识 VmSpace（保持 libs/sched 不依赖 general）。
// 上层（kernel）启动期注册一个回调，回调内部做 ext_lookup + downcast +
// VmSpace::activate。普通内核线程没挂 VmSpace 时回调返 None，no-op。

use alloc::sync::Arc;

/// schedule_once 切换到 next 前调一次的回调。`next` 即 [`crate::Task`] 的
/// `Arc`；回调按需读取 ext 表。
#[repr(C)]
pub struct VmSwitchOps {
    pub on_switch: fn(next: &Arc<crate::task::Task>),
}

unsafe impl Sync for VmSwitchOps {}
unsafe impl Send for VmSwitchOps {}

static VM_SWITCH_OPS: AtomicPtr<VmSwitchOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_vm_switch(ops: &'static VmSwitchOps) {
    register_once(&VM_SWITCH_OPS, ops as *const _ as *mut _, "VmSwitchOps");
}

pub fn vm_switch() -> Option<&'static VmSwitchOps> {
    let ptr = VM_SWITCH_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_vm_switch 仅接受 'static；Acquire/Release 配对。
        Some(unsafe { &*(ptr as *const VmSwitchOps) })
    }
}

// ── TaskCpuStateOps ─────────────────────────────────────────────────────────
//
// 调度核心在切到某个任务前已经知道它即将运行在哪个 CPU；用户态可观察的
// per-task CPU 状态（例如 rseq 的 cpu_id 字段）需要由 kernel 层按当前地址空间
// 更新。sched 不理解用户指针和地址空间，只在无锁边界触发这个回调。

/// 任务即将运行在指定 CPU 时的外部状态发布契约。
#[repr(C)]
pub struct TaskCpuStateOps {
    pub publish_current_cpu: fn(task: &Arc<crate::task::Task>, cpu_id: usize),
}

unsafe impl Sync for TaskCpuStateOps {}
unsafe impl Send for TaskCpuStateOps {}

static TASK_CPU_STATE_OPS: AtomicPtr<TaskCpuStateOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_task_cpu_state(ops: &'static TaskCpuStateOps) {
    register_once(
        &TASK_CPU_STATE_OPS,
        ops as *const _ as *mut _,
        "TaskCpuStateOps",
    );
}

pub fn task_cpu_state() -> Option<&'static TaskCpuStateOps> {
    let ptr = TASK_CPU_STATE_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_task_cpu_state 仅接受 'static；Acquire/Release 配对。
        Some(unsafe { &*(ptr as *const TaskCpuStateOps) })
    }
}

// ── ArchIdleOps ──────────────────────────────────────────────────────────────

/// 架构相关的 idle 等待契约。
///
/// 调度器不知道具体 ISA 的低功耗/等待中断指令，也不知道内核态 idle 是否需要
/// 临时打开本地中断。由 arch 注入这个 hook，保持 sched crate 不直接依赖
/// LoongArch/RISC-V CSR 细节。
#[repr(C)]
pub struct ArchIdleOps {
    pub idle_relax: fn(),
}

unsafe impl Sync for ArchIdleOps {}
unsafe impl Send for ArchIdleOps {}

static IDLE_OPS: AtomicPtr<ArchIdleOps> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_idle(ops: &'static ArchIdleOps) {
    register_once(&IDLE_OPS, ops as *const _ as *mut _, "ArchIdleOps");
}

pub fn idle() -> Option<&'static ArchIdleOps> {
    let ptr = IDLE_OPS.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safety: register_idle only stores 'static ops.
        Some(unsafe { &*(ptr as *const ArchIdleOps) })
    }
}
