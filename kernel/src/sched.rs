//! Kernel 层调度 shim。
//!
//! 真正的调度逻辑在 `libs/sched`；本模块负责两件事：
//!
//! 1. 把 **VFS + FdTable** 作为 `TaskExtCloneHook` 的拷贝目标接进 sched——
//!    fork/clone 时按 `CLONE_FS` / `CLONE_FILES` 决定共享还是深拷。
//! 2. [`boot_init`] —— 启动期流程：
//!    注入 arch hook → 注册 ext clone hook → `sched::init` 建 init →
//!    给 init 装 VfsContext + FdTable → 为 CPU 0 启动 idle 内核线程。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use errno::Errno;
use general::mm::{VmSpace, copy_from_user, copy_to_user};
use general::vfs::{
    Credentials, Dentry, FdTable, FileMode, Mount, MountNamespace, VfsContext, VfsLimits, VfsRoot,
    build_boot_vfs_parts,
};
use hal::memory::page_size;
use hal::user_context::UserTrapFrame;
use mm::VmFlags;
use sched::arch_hooks::VmSwitchOps;
use sched::clone_flags::{CloneArgs, CloneFlags};
use sched::process_ops::{ExecRequest, ProcessImageOps, UserContextRef};
use sched::signal::{SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow, SigSet};
use sched::sync::Spinlock;
use sched::task::{
    TaskExitAccountingHook, TaskExtCloneHook, TaskExtExitHook, TaskExtKey, TaskPreExitHook,
};
use sched::{
    SchedParams, TASKEXT_USER_TRAP_FRAME, TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE,
    TASKEXT_VM_SPACE, Task,
};
use vfs::Arc as VfsArc;

/// acpi / dtb 启动路径在控制台挂载完成后，把 VFS 根部件存这里；
/// [`boot_init`] 再取出来装到 init 任务上。
static BOOT_VFS_PARTS: Spinlock<Option<BootVfsParts>> = Spinlock::new(None);
static BOOT_ROOT_IS_INITRAMFS: AtomicBool = AtomicBool::new(false);

/// 控制台路径或 devtmpfs 节点名（例如 "/dev/console" 或 "uart0"）。stash 后
/// install_stdio 用它走 openat 路径打开 fd 0/1/2。
static BOOT_CONSOLE_NAME: Spinlock<Option<alloc::string::String>> = Spinlock::new(None);

pub(crate) const TASKEXT_EXEC_PATH: TaskExtKey = sched::TASKEXT_EXEC_PATH;
pub(crate) const TASKEXT_EXEC_ARGS: TaskExtKey = sched::TASKEXT_EXEC_ARGS;
pub(crate) const TASKEXT_EXEC_ENVP: TaskExtKey = sched::TASKEXT_EXEC_ENVP;
pub(crate) const TASKEXT_EXEC_ACCESS: TaskExtKey = sched::TASKEXT_EXEC_ACCESS;

#[cfg(target_arch = "riscv64")]
type RiscvVectorSignalStack = Spinlock<Vec<Option<arch::riscv64::vector::UserVectorState>>>;

pub fn stash_boot_console_name(name: alloc::string::String) {
    *BOOT_CONSOLE_NAME.lock() = Some(name);
}

pub(crate) fn take_boot_console_name() -> Option<alloc::string::String> {
    BOOT_CONSOLE_NAME.lock().take()
}

/// 暂存 init 任务所需的 VFS 构造材料。字段顺序与 [`VfsContext::new`] 对齐。
struct BootVfsParts {
    cwd: VfsArc<Dentry>,
    cwd_mount: VfsArc<Mount>,
    root: VfsRoot,
    mount_ns: VfsArc<MountNamespace>,
    cred: VfsArc<Credentials>,
    umask: FileMode,
    limits: VfsArc<VfsLimits>,
}

/// 启动路径在 VFS 准备好后调用，把 init 需要的部件交给调度 shim 保管。
/// 调用一次；重复调用以最后一次为准（acpi / dtb 只会走其中一条路径）。
pub fn stash_boot_vfs_parts(
    cwd: VfsArc<Dentry>,
    cwd_mount: VfsArc<Mount>,
    mount_ns: VfsArc<MountNamespace>,
    cred: VfsArc<Credentials>,
    root_is_initramfs: bool,
) {
    let (cwd, cwd_mount, root, mount_ns, cred, umask, limits) =
        build_boot_vfs_parts(cwd, cwd_mount, mount_ns, cred);
    *BOOT_VFS_PARTS.lock() = Some(BootVfsParts {
        cwd,
        cwd_mount,
        root,
        mount_ns,
        cred,
        umask,
        limits,
    });
    BOOT_ROOT_IS_INITRAMFS.store(root_is_initramfs, Ordering::Release);
}

// ── TaskExtCloneHook ─────────────────────────────────────────────────────────

/// fork/clone 时按 `CLONE_FS` / `CLONE_FILES` 决定 VFS / fdtable 是共享 Arc
/// 还是深拷贝；其它扩展键按"全共享"语义复用同一 Arc。
struct KernelExtCloneHook;

impl TaskExtCloneHook for KernelExtCloneHook {
    fn clone_for(
        &self,
        key: TaskExtKey,
        src: &Arc<dyn Any + Send + Sync>,
        flags: CloneFlags,
    ) -> Arc<dyn Any + Send + Sync> {
        match key {
            TASKEXT_VFS_CONTEXT => {
                let s = Arc::clone(src)
                    .downcast::<VfsContext>()
                    .expect("[sched][ext] vfs ctx type mismatch");
                if flags.has(CloneFlags::CLONE_FS) {
                    s
                } else {
                    let forked = s.fork().expect("[sched][ext] VfsContext::fork failed");
                    Arc::new(forked)
                }
            }
            TASKEXT_VFS_FDTABLE => {
                let s = Arc::clone(src)
                    .downcast::<FdTable>()
                    .expect("[sched][ext] fdtable type mismatch");
                if flags.has(CloneFlags::CLONE_FILES) {
                    s
                } else {
                    Arc::new(s.fork())
                }
            }
            TASKEXT_VM_SPACE => {
                let s = Arc::clone(src)
                    .downcast::<VmSpace>()
                    .expect("[sched][ext] VmSpace type mismatch");
                if flags.has(CloneFlags::CLONE_VM) {
                    s
                } else {
                    Arc::new(s.fork())
                }
            }
            #[cfg(target_arch = "riscv64")]
            sched::TASKEXT_RISCV_VECTOR_STATE => arch::riscv64::vector::clone_ext_payload(src),
            #[cfg(target_arch = "riscv64")]
            sched::TASKEXT_RISCV_VECTOR_SIGNAL_STACK => {
                Arc::new(RiscvVectorSignalStack::new(Vec::new()))
            }
            sched::TASKEXT_ELM_EXECUTION => {
                Arc::new(general::elm_guard::ElmTaskExecutionState::new())
            }
            crate::syscalls::ipc::TASKEXT_SEM_UNDO => {
                let table = Arc::clone(src)
                    .downcast::<general::ipc::sem_undo::SemUndoTable>()
                    .expect("[sched][ext] sem undo table type mismatch");
                // CLONE_SYSVSEM 共享撤销表；否则子进程得到空表（Linux 语义：
                // 撤销项不随 fork 继承）。
                if flags.has(CloneFlags::CLONE_SYSVSEM) {
                    table
                } else {
                    Arc::new(general::ipc::sem_undo::SemUndoTable::new())
                }
            }
            crate::syscalls::process::TASKEXT_PRCTL_MISC => {
                let state = Arc::clone(src)
                    .downcast::<crate::syscalls::process::PrctlMiscState>()
                    .expect("[sched][ext] prctl misc state type mismatch");
                // Linux：TSC 模式与 THP 开关随 fork 继承（exec 保留）。
                let child = crate::syscalls::process::PrctlMiscState::new();
                child
                    .tsc_mode
                    .store(state.tsc_mode.load(Ordering::Acquire), Ordering::Release);
                child
                    .thp_disable
                    .store(state.thp_disable.load(Ordering::Acquire), Ordering::Release);
                Arc::new(child)
            }
            crate::syscalls::ipc::TASKEXT_KEYRINGS => {
                let process = Arc::clone(src)
                    .downcast::<general::ipc::keys::ProcessKeyrings>()
                    .expect("[sched][ext] process keyrings type mismatch");
                // CLONE_THREAD 共享 thread keyring；fork 时新建引用集并继承
                // process/session keyring 引用（Linux copy_keys 语义）。
                if flags.has(CloneFlags::CLONE_THREAD) {
                    process
                } else {
                    let child = general::ipc::keys::ProcessKeyrings::new();
                    *child.process.lock() = *process.process.lock();
                    *child.session.lock() = *process.session.lock();
                    Arc::new(child)
                }
            }
            _ => Arc::clone(src),
        }
    }
}

static HOOK: KernelExtCloneHook = KernelExtCloneHook;

struct KernelExtExitHook;

impl TaskExtExitHook for KernelExtExitHook {
    fn cleanup_on_exit(&self, task: &Arc<Task>) {
        crate::native_runtime::record_task_exit(task);
        #[cfg(target_arch = "riscv64")]
        arch::riscv64::vector::clear_for_task(task);
        #[cfg(target_arch = "riscv64")]
        {
            let _ = task.ext_remove(sched::TASKEXT_RISCV_VECTOR_SIGNAL_STACK);
        }
        let _ = task.ext_remove(TASKEXT_USER_TRAP_FRAME);
        let _ = task.ext_remove(crate::native_runtime::TASKEXT_NATIVE_THREAD);
        let _ = task.ext_remove(sched::TASKEXT_ELM_EXECUTION);
        let _ = task.ext_remove(TASKEXT_EXEC_ACCESS);
        // 退出扩展通常在任务已经切离 CPU 后清理，但测试/早期退出路径也可能
        // 在当前任务仍驻留时到达这里。先切回内核根页表，再释放 VmSpace，避免
        // RISC-V 的 CURRENT_USER_PGD 指向即将回收的页表。
        if sched::try_current_task_ref()
            .is_some_and(|current| core::ptr::eq(current, task.as_ref()))
        {
            hal::sched::activate_kernel_address_space();
        }
        let _ = task.ext_remove(TASKEXT_VM_SPACE);
        let _ = task.ext_remove(TASKEXT_VFS_FDTABLE);
        let _ = task.ext_remove(TASKEXT_VFS_CONTEXT);
    }
}

static EXIT_HOOK: KernelExtExitHook = KernelExtExitHook;

struct KernelExitAccountingHook;

impl TaskExitAccountingHook for KernelExitAccountingHook {
    fn account_on_exit(&self, task: &Task) {
        crate::acct::account_task_exit(task);
    }
}

static EXIT_ACCOUNTING_HOOK: KernelExitAccountingHook = KernelExitAccountingHook;

struct KernelPreExitHook;

impl TaskPreExitHook for KernelPreExitHook {
    fn cleanup_before_exit(&self, task: &Arc<Task>) {
        crate::syscalls::cleanup_task_before_exit(task);
    }
}

static PRE_EXIT_HOOK: KernelPreExitHook = KernelPreExitHook;

// ── VmSwitchOps ──────────────────────────────────────────────────────────────
//
// sched 在 schedule_once 切换到 next 前调此回调；本函数从 next 的 ext 表
// 里找 TASKEXT_VM_SPACE payload，若在 -> downcast 成 VmSpace 再 activate。
// 没挂（idle、纯 kthread 或 init）时必须切回内核页表，不能继续沿用
// 上一个用户任务可能即将回收的 PGD。

fn vm_on_switch(next: &Arc<Task>) {
    let activated = next
        .ext_with(TASKEXT_VM_SPACE, |payload| {
            let Some(vm) = payload.downcast_ref::<VmSpace>() else {
                return false;
            };
            vm.activate();
            true
        })
        .unwrap_or(false);
    if activated {
        return;
    }

    // 内核线程和 idle 没有用户地址空间。RISC-V 若在这里保持上一个用户
    // satp，旧 VmSpace 释放后其根页会被重新分配，内核随后会在悬空页表下
    // 执行；所有架构统一在切换前恢复内核根。
    hal::sched::activate_kernel_address_space();
}

static VM_SWITCH_OPS: VmSwitchOps = VmSwitchOps {
    on_switch: vm_on_switch,
};

// ── TaskCpuStateOps ─────────────────────────────────────────────────────────
//
// rseq 的 cpu_id 字段属于用户态 ABI；sched 底层只负责告诉 kernel 某个任务
// 即将在哪个 CPU 上运行，具体用户内存写入必须留在 kernel/mm 侧完成。

const RSEQ_CPU_ID_START_OFFSET: usize = 0;
const RSEQ_CPU_ID_OFFSET: usize = 4;

fn publish_task_cpu_state(task: &Arc<Task>, cpu_id: usize) {
    if !task.rseq_registered() {
        return;
    }
    let registration = task.rseq_registration();
    if !registration.registered {
        return;
    }
    let Ok(cpu) = u32::try_from(cpu_id) else {
        return;
    };
    task.publish_rseq_cpu(cpu_id);
    let Some(start_addr) = registration.ptr.checked_add(RSEQ_CPU_ID_START_OFFSET) else {
        return;
    };
    let Some(current_addr) = registration.ptr.checked_add(RSEQ_CPU_ID_OFFSET) else {
        return;
    };
    if copy_to_user(start_addr, &cpu.to_ne_bytes()).is_err()
        || copy_to_user(current_addr, &cpu.to_ne_bytes()).is_err()
    {
        log::debug!(
            "[sched][rseq] publish cpu failed pid={:?} cpu={}",
            task.pid_root(),
            cpu_id
        );
    }
}

static TASK_CPU_STATE_OPS: sched::arch_hooks::TaskCpuStateOps =
    sched::arch_hooks::TaskCpuStateOps {
        publish_current_cpu: publish_task_cpu_state,
    };

// ── ProcessImageOps ─────────────────────────────────────────────────────────
//
// sched 拥有 exec/clone/sigreturn 的状态机；真正解释用户指针、构造 trap frame、
// 替换 VmSpace 的实现留在 kernel/hal 侧。

const SIGFRAME_MAGIC: u64 = 0x4d59474f_53494746; // "MYGOSIGF"
const SIGFRAME_HEADER_SIZE: usize = 64;
const SIGFRAME_SIGINFO_SIZE: usize = 128;
const SIGFRAME_UCONTEXT_SIGMASK_OFF: usize = 40;
const SIGFRAME_UCONTEXT_MCONTEXT_OFF: usize = 176;
const SIGFRAME_MCONTEXT_SIZE: usize = 272;
const SIGFRAME_UCONTEXT_SIZE: usize = SIGFRAME_UCONTEXT_MCONTEXT_OFF + SIGFRAME_MCONTEXT_SIZE;
const SIGFRAME_SIGINFO_OFF: usize = SIGFRAME_HEADER_SIZE;
const SIGFRAME_UCONTEXT_OFF: usize = SIGFRAME_SIGINFO_OFF + SIGFRAME_SIGINFO_SIZE;
const SIGFRAME_TRAP_OFF: usize = SIGFRAME_UCONTEXT_OFF + SIGFRAME_UCONTEXT_SIZE;
// 当前 LoongArch64 用户陷阱帧约 552 字节。信号路径不用堆分配，但也不能在
// 64KiB 内核栈上放 16KiB 大对象；2KiB 给后续寄存器扩展留出余量。
const SIGFRAME_TRAP_BUF_SIZE: usize = 2048;
const SIGFRAME_STACK_BUF_SIZE: usize = SIGFRAME_TRAP_OFF + SIGFRAME_TRAP_BUF_SIZE;

fn task_vm_space(task: &Arc<Task>) -> Option<Arc<VmSpace>> {
    task.ext_lookup(TASKEXT_VM_SPACE)?
        .downcast::<VmSpace>()
        .ok()
}

fn task_fdtable(task: &Arc<Task>) -> Option<Arc<FdTable>> {
    task.ext_lookup(TASKEXT_VFS_FDTABLE)?
        .downcast::<FdTable>()
        .ok()
}

pub(crate) fn task_exec_path(task: &Arc<Task>) -> Option<String> {
    task.ext_lookup(TASKEXT_EXEC_PATH)?
        .downcast::<String>()
        .ok()
        .map(|path| (*path).clone())
}

fn install_exec_path(task: &Arc<Task>, path: &str) {
    let _ = task.ext_remove(TASKEXT_EXEC_PATH);
    task.ext_install(TASKEXT_EXEC_PATH, Arc::new(String::from(path)));
}

fn install_exec_metadata(task: &Arc<Task>, path: &str, argv: &[String], envp: &[String]) {
    install_exec_path(task, path);
    let comm = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path);
    task.set_comm(comm.as_bytes());

    let _ = task.ext_remove(TASKEXT_EXEC_ARGS);
    task.ext_install(TASKEXT_EXEC_ARGS, Arc::new(argv.to_vec()));

    let _ = task.ext_remove(TASKEXT_EXEC_ENVP);
    task.ext_install(TASKEXT_EXEC_ENVP, Arc::new(envp.to_vec()));
}

#[cfg(feature = "performance-profile")]
pub(crate) fn profile_image_id(path: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash.max(1)
}

#[cfg(feature = "performance-profile")]
fn install_profile_images(task: &Arc<Task>, loaded: &crate::user::LoadedUserImage) {
    let main = (
        profile_image_id(&loaded.exec_path),
        loaded.main_image_range.start,
        loaded.main_image_range.end,
    );
    let interpreter = loaded
        .interpreter_image
        .as_ref()
        .map(|(path, range)| (profile_image_id(path), range.start, range.end))
        .unwrap_or((0, 0, 0));
    task.set_profile_images(main, interpreter);
}

fn install_exec_access(task: &Arc<Task>, access: Arc<crate::user::ExecutableAccessSet>) {
    let _ = task.ext_remove(TASKEXT_EXEC_ACCESS);
    task.ext_install(TASKEXT_EXEC_ACCESS, access);
}

fn write_user_pid_t(user: usize, value: sched::pid::PidT) -> Result<(), Errno> {
    copy_to_user(user, &value.to_ne_bytes()).map_err(|e| e.as_errno())
}

fn activate_task_vm(task: &Arc<Task>) {
    if let Some(vm) = task_vm_space(task) {
        vm.activate();
    }
}

#[cfg(target_arch = "riscv64")]
fn push_riscv_vector_signal_snapshot(
    task: &Arc<Task>,
    user_ctx: UserContextRef,
) -> Result<(), Errno> {
    let tf = unsafe { &mut *(user_ctx.as_usize() as *mut arch::riscv64::TrapFrame) };
    let snapshot = arch::riscv64::vector::snapshot_current_for_signal(tf);
    let stack = if let Some(stack) = task
        .ext_lookup(sched::TASKEXT_RISCV_VECTOR_SIGNAL_STACK)
        .and_then(|payload| payload.downcast::<RiscvVectorSignalStack>().ok())
    {
        stack
    } else {
        let stack = Arc::new(RiscvVectorSignalStack::new(Vec::new()));
        task.ext_install(sched::TASKEXT_RISCV_VECTOR_SIGNAL_STACK, stack.clone());
        stack
    };
    let mut guard = stack.lock();
    guard.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
    guard.push(snapshot);
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn pop_riscv_vector_signal_snapshot(task: &Arc<Task>, user_ctx: UserContextRef) {
    let Some(stack) = task
        .ext_lookup(sched::TASKEXT_RISCV_VECTOR_SIGNAL_STACK)
        .and_then(|payload| payload.downcast::<RiscvVectorSignalStack>().ok())
    else {
        return;
    };
    let Some(snapshot) = stack.lock().pop() else {
        return;
    };
    let tf = unsafe { &mut *(user_ctx.as_usize() as *mut arch::riscv64::TrapFrame) };
    arch::riscv64::vector::restore_signal_snapshot(tf, snapshot);
}

unsafe extern "C" fn user_clone_entry(_arg: usize) -> ! {
    let frame = {
        let me = sched::current_task_direct();
        activate_task_vm(&me);

        // 子任务可能在"已入队、尚未首次运行"的窗口里被 exit_group / SIGKILL
        // 杀掉。`exit_task` 只对"不是任何 CPU 的 current"的任务做扩展清理，而
        // 排队中的新子任务恰好满足这个条件，于是它的 trap frame 会先被摘走。
        // 这不是错误状态：任务已经被标记退出，正确处理是直接走内核线程退出
        // 路径，而不是 panic，更不能带着空 frame 返回用户态。
        let Some(payload) = me.ext_remove(TASKEXT_USER_TRAP_FRAME) else {
            log::debug!(
                "[sched][clone] child terminated before first user return: pid={:?} state={:?}",
                me.pid_root(),
                me.state(),
            );
            sched::kthread_finish(sched::ExitCode(0));
        };
        let frame = payload
            .downcast::<UserTrapFrame>()
            .expect("[sched][clone] saved trap frame type mismatch");
        let mut frame = *frame;
        let kstack_top = me
            .kernel_stack_top()
            .expect("[sched][clone] user child missing kernel stack");
        frame.set_current_address_space();
        frame.set_kernel_stack_top(kstack_top);
        frame
    };

    unsafe { frame.resume() }
}

fn process_execve(
    task: &Arc<Task>,
    request: ExecRequest,
    user_ctx: UserContextRef,
) -> Result<(), Errno> {
    if user_ctx.is_none() {
        return Err(Errno::EINVAL);
    }
    let prepared = crate::exec::prepare_exec(task, request)?;
    crate::exec::commit_exec(task, prepared, user_ctx)
}

fn process_spawn_user_process(
    _parent: &Arc<Task>,
    child: &Arc<Task>,
    path: &str,
    argv: &[String],
    envp: &[String],
) -> Result<(), Errno> {
    let loaded = match crate::user::load_user_image_from_path(child, path, argv, envp) {
        Ok(loaded) => loaded,
        Err(error) => {
            activate_task_vm(&sched::current_task_direct());
            return Err(error);
        }
    };

    let _ = child.ext_remove(TASKEXT_VM_SPACE);
    child.ext_install(TASKEXT_VM_SPACE, loaded.vm.clone());
    install_exec_access(child, Arc::clone(&loaded.exec_access));
    install_exec_metadata(child, &loaded.exec_path, argv, envp);
    #[cfg(feature = "performance-profile")]
    install_profile_images(child, &loaded);
    if let Some(fdt) = task_fdtable(child) {
        fdt.close_on_exec(child.pid_root().unwrap_or(0));
    }
    child
        .thread_group()
        .shared_signal()
        .reset_handlers_for_exec();

    child.into_kernel_thread(user_clone_entry, 0);
    let kstack_top = child
        .kernel_stack_top()
        .expect("[sched][spawn-user] child missing kernel stack");
    let mut frame = UserTrapFrame::init_user(loaded.entry_pc, loaded.user_sp, 0);
    frame.set_kernel_stack_top(kstack_top);
    let _ = child.ext_remove(TASKEXT_USER_TRAP_FRAME);
    child.ext_install(TASKEXT_USER_TRAP_FRAME, Arc::new(frame));

    // 装载器会激活新地址空间以布置用户栈；返回调用者前必须恢复当前任务页表。
    activate_task_vm(&sched::current_task_direct());
    Ok(())
}

/// 为尚未进入运行队列的 Native 子进程安装完整映像与首次用户上下文。
pub(crate) fn prepare_native_child(
    child: &Arc<Task>,
    image: crate::soyo::PreparedSoyoImage,
) -> Result<(), Errno> {
    if child.state() != sched::TaskState::New {
        return Err(Errno::EINVAL);
    }
    let kernel_stack_top = child.ensure_kernel_stack();
    let frame = crate::exec::prepare_native_initial_frame(
        image.entry_pc,
        image.user_sp,
        image.start_info_address,
        image.start_info_size,
        image.image_base,
        image.tls_base,
        image.bootstrap_process,
        kernel_stack_top,
    );

    let vm: Arc<dyn core::any::Any + Send + Sync> = image.vm.clone();
    child.ext_install(TASKEXT_VM_SPACE, vm);
    // Native child 从零建立用户态资源，不继承父侧 Linux fd、cwd 或 root。
    let _ = child.ext_remove(TASKEXT_VFS_FDTABLE);
    let _ = child.ext_remove(TASKEXT_VFS_CONTEXT);
    child.set_comm(b"soyo-child");
    child.into_kernel_thread(user_clone_entry, 0);
    child.ext_install(TASKEXT_USER_TRAP_FRAME, Arc::new(frame));

    let personality: Arc<dyn core::any::Any + Send + Sync> = image.personality;
    let group = child.thread_group();
    let mut exec = group.lock_exec();
    if exec.phase() != native_abi::ExecPhase::Running || !exec.has_only_member(child) {
        return Err(Errno::EBUSY);
    }
    exec.install_personality(sched::ProcessPersonalityState::MygoNative(personality));
    exec.advance_generation();
    drop(exec);

    // SOYO 映射期间可能切换过活动页表，返回父调用现场前必须恢复当前地址空间。
    activate_task_vm(&sched::current_task());
    Ok(())
}

/// 为尚未进入运行队列的 Native 线程安装共享地址空间和首次用户上下文。
pub(crate) fn prepare_native_thread(
    child: &Arc<Task>,
    vm: Arc<VmSpace>,
    entry: usize,
    stack_top: usize,
    argument: usize,
    tls_base: usize,
) -> Result<(), Errno> {
    if child.state() != sched::TaskState::New
        || child.thread_group().user_abi_kind() != native_abi::UserAbiKind::MygoNative
    {
        return Err(Errno::EINVAL);
    }
    let kernel_stack_top = child.ensure_kernel_stack();
    let mut frame = UserTrapFrame::init_user(entry, stack_top, argument);
    frame.set_tls(tls_base);
    frame.set_kernel_stack_top(kernel_stack_top);

    let vm_payload: Arc<dyn core::any::Any + Send + Sync> = vm;
    child.ext_install(TASKEXT_VM_SPACE, vm_payload);
    let _ = child.ext_remove(TASKEXT_VFS_FDTABLE);
    let _ = child.ext_remove(TASKEXT_VFS_CONTEXT);
    child.set_comm(b"soyo-thread");
    child.into_kernel_thread(user_clone_entry, 0);
    child.ext_install(TASKEXT_USER_TRAP_FRAME, Arc::new(frame));
    Ok(())
}

/// `mq_notify(SIGEV_THREAD)`：在注册者进程上下文创建线程执行通知函数。
///
/// 语义对齐 POSIX/Linux：helper 是注册者的同进程新线程（共享 mm/fs/files/
/// sighand），从 `function(value)` 开始执行；函数返回后经用户态退出桩调用
/// `exit(0)` 结束该线程，不影响进程其它线程。
pub(crate) fn spawn_mq_notify_thread(registrant: &Arc<Task>, function: usize, value: usize) {
    use sched::clone_flags::{CloneArgs, CloneFlags};

    if registrant.is_kernel_task() {
        return;
    }
    let args = CloneArgs {
        flags: CloneFlags(
            CloneFlags::CLONE_VM
                | CloneFlags::CLONE_FS
                | CloneFlags::CLONE_FILES
                | CloneFlags::CLONE_SIGHAND
                | CloneFlags::CLONE_THREAD,
        ),
        pidfd: 0,
        stack: 0,
        stack_size: 0,
        parent_tid: 0,
        child_tid: 0,
        tls: 0,
        exit_signal: 0,
        set_tid: 0,
        set_tid_size: 0,
        requested_pid: 0,
        cgroup: 0,
    };
    let child = sched::spawn::clone_task(registrant, args, sched::SchedParams::default_fair());
    if child.state() == sched::TaskState::Dead {
        log::warning!(
            "[mq][SIGEV_THREAD] clone failed for registrant pid={:?}",
            registrant.pid_root(),
        );
        return;
    }
    let Some(vm) = registrant
        .ext_lookup(TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
    else {
        log::debug!("[mq][SIGEV_THREAD] registrant has no vm, drop notification");
        return;
    };
    // 通知线程的用户栈 + 退出桩（一页，可执行）。
    let Ok(stack_range) = vm.alloc_mmap_range(page_size()) else {
        return;
    };
    let stack_flags = VmFlags::EMPTY
        .with(VmFlags::READ)
        .with(VmFlags::WRITE)
        .with(VmFlags::EXEC)
        .with(VmFlags::USER);
    if vm.map_anon(stack_range.clone(), stack_flags).is_err() {
        let _ = vm.unmap(stack_range.clone());
        return;
    }
    let stub_addr = stack_range.start;
    if vm.copy_user_bytes_out(stub_addr, exit_stub_code()).is_err() {
        let _ = vm.unmap(stack_range);
        return;
    }

    let kernel_stack_top = child.ensure_kernel_stack();
    let mut frame = UserTrapFrame::init_user(function, stack_range.end, value);
    frame.set_ra(stub_addr);
    frame.set_kernel_stack_top(kernel_stack_top);
    frame.set_current_address_space();
    child.set_comm(b"mq-notify");
    child.into_kernel_thread(user_clone_entry, 0);
    child.ext_install(TASKEXT_USER_TRAP_FRAME, Arc::new(frame));
    let _ = sched::spawn::activate_task(&child);
}

/// 用户态退出桩：`exit(0)` 的两条指令（架构相关机器码）。
///
/// loongarch64：`ori $a7, $zero, 93` + `syscall 0`（SYS_exit）。
/// riscv64：`addi a7, zero, 93` + `ecall`（SYS_exit）。
fn exit_stub_code() -> &'static [u8] {
    #[cfg(target_arch = "loongarch64")]
    {
        &[0x0b, 0x74, 0x81, 0x03, 0x00, 0x00, 0x2b, 0x00]
    }
    #[cfg(target_arch = "riscv64")]
    {
        &[0x93, 0x08, 0xd0, 0x05, 0x73, 0x00, 0x00, 0x00]
    }
}

fn process_clone_user_context(
    parent: &Arc<Task>,
    child: &Arc<Task>,
    args: CloneArgs,
    user_ctx: UserContextRef,
) -> Result<(), Errno> {
    if user_ctx.is_none() {
        return Err(Errno::EINVAL);
    }

    let mut frame = UserTrapFrame::from_context(user_ctx.as_usize());
    frame.advance_pc();
    frame.set_ret(0);
    if args.stack != 0 {
        let sp = if args.stack_size != 0 {
            args.stack
                .checked_add(args.stack_size)
                .ok_or(Errno::EINVAL)?
        } else {
            args.stack
        };
        frame.set_sp(sp);
    }
    if args.flags.has(CloneFlags::CLONE_SETTLS) {
        frame.set_tls(args.tls);
    }

    let child_tid = child.pid_root().ok_or(Errno::EAGAIN)?;
    if args.flags.has(CloneFlags::CLONE_PARENT_SETTID) && args.parent_tid != 0 {
        write_user_pid_t(args.parent_tid, child_tid)?;
    }
    if args.flags.has(CloneFlags::CLONE_CHILD_SETTID) && args.child_tid != 0 {
        // 切换到子进程页表写 child_tid；缺页处理依赖 current_task_vm_space()，
        // 必须临时把 parent 的 TASKEXT_VM_SPACE 换成 child 的，否则硬件用 child
        // 的 PGD 但缺页 handler 修改 parent 的页表，导致修复不生效而陷入死循环。
        let saved_vm = if !args.flags.has(CloneFlags::CLONE_VM) {
            let child_vm = task_vm_space(child);
            if let Some(ref vm) = child_vm {
                vm.activate();
            }
            let old = parent.ext_remove(TASKEXT_VM_SPACE);
            if let Some(ref vm) = child_vm {
                parent.ext_install(TASKEXT_VM_SPACE, vm.clone());
            }
            old
        } else {
            None
        };
        let result = write_user_pid_t(args.child_tid, child_tid);
        if !args.flags.has(CloneFlags::CLONE_VM) {
            parent.ext_remove(TASKEXT_VM_SPACE);
            if let Some(old) = saved_vm {
                parent.ext_install(TASKEXT_VM_SPACE, old);
            }
            if let Some(ref vm) = task_vm_space(parent) {
                vm.activate();
            }
        }
        result?;
    }
    if args.flags.has(CloneFlags::CLONE_CHILD_CLEARTID) {
        child.set_clear_child_tid(args.child_tid);
    }

    let _ = child.ext_remove(TASKEXT_USER_TRAP_FRAME);
    child.ext_install(TASKEXT_USER_TRAP_FRAME, Arc::new(frame));
    child.into_kernel_thread(user_clone_entry, 0);
    Ok(())
}

fn process_sigreturn(task: &Arc<Task>, user_ctx: UserContextRef) -> Result<(), Errno> {
    if user_ctx.is_none() {
        return Err(Errno::EINVAL);
    }

    let current = UserTrapFrame::from_context(user_ctx.as_usize());
    let sp = current.sp();
    let mut header = [0u8; SIGFRAME_HEADER_SIZE];
    copy_from_user(sp, &mut header).map_err(|e| e.as_errno())?;
    if read_u64(&header, 0) != SIGFRAME_MAGIC {
        return Err(Errno::EINVAL);
    }
    let total = read_u64(&header, 8) as usize;
    let old_mask = read_u64(&header, 16);
    let trap_off = read_u64(&header, 24) as usize;
    let trap_len = read_u64(&header, 32) as usize;
    let ucontext_off = read_u64(&header, 48) as usize;
    let abi_pc = read_u64(&header, 56) as usize;
    let trap_end = trap_off.checked_add(trap_len).ok_or(Errno::EINVAL)?;
    let ucontext_mcontext = ucontext_off
        .checked_add(SIGFRAME_UCONTEXT_MCONTEXT_OFF)
        .ok_or(Errno::EINVAL)?;
    let ucontext_mcontext_end = ucontext_mcontext
        .checked_add(SIGFRAME_MCONTEXT_SIZE)
        .ok_or(Errno::EINVAL)?;
    if trap_off < SIGFRAME_HEADER_SIZE
        || trap_len != UserTrapFrame::encoded_len()
        || trap_end > total
        || ucontext_off < SIGFRAME_HEADER_SIZE
        || ucontext_mcontext_end > total
        || total > 16 * 1024
    {
        return Err(Errno::EINVAL);
    }

    let mut mask_raw = [0u8; 8];
    let mask_addr = sp
        .checked_add(ucontext_off)
        .and_then(|base| base.checked_add(SIGFRAME_UCONTEXT_SIGMASK_OFF))
        .ok_or(Errno::EINVAL)?;
    let restore_mask = copy_from_user(mask_addr, &mut mask_raw)
        .map(|_| u64::from_le_bytes(mask_raw))
        .unwrap_or(old_mask);

    // 信号返回是 lmbench lat_sig 的热路径；陷阱帧大小固定，避免每次
    // rt_sigreturn 都走堆分配。
    let mut trap_storage = [0u8; SIGFRAME_TRAP_BUF_SIZE];
    if trap_len > trap_storage.len() {
        return Err(Errno::EINVAL);
    }
    let trap_bytes = &mut trap_storage[..trap_len];
    copy_from_user(sp.checked_add(trap_off).ok_or(Errno::EINVAL)?, trap_bytes)
        .map_err(|e| e.as_errno())?;
    let mut restored = UserTrapFrame::read_bytes(trap_bytes).ok_or(Errno::EINVAL)?;
    let mut mcontext_storage = [0u8; SIGFRAME_MCONTEXT_SIZE];
    copy_from_user(
        sp.checked_add(ucontext_mcontext).ok_or(Errno::EINVAL)?,
        &mut mcontext_storage,
    )
    .map_err(|e| e.as_errno())?;
    let mcontext_pc = read_u64(&mcontext_storage, 0) as usize;
    if mcontext_pc != abi_pc {
        if !restored.apply_linux_mcontext(&mcontext_storage) {
            return Err(Errno::EINVAL);
        }
    }
    task.signal
        .block(SigSet::from_raw(restore_mask), SigProcMaskHow::SetMask);
    restored.apply_to_context(user_ctx.as_usize());
    #[cfg(target_arch = "riscv64")]
    pop_riscv_vector_signal_snapshot(task, user_ctx);
    Ok(())
}

fn process_setup_signal_frame(
    task: &Arc<Task>,
    info: SigInfo,
    action: SigAction,
    user_ctx: UserContextRef,
) -> Result<(), Errno> {
    if user_ctx.is_none() {
        return Err(Errno::ENOSYS);
    }
    let SigHandler::Handler(handler_pc) = action.handler else {
        return Err(Errno::EINVAL);
    };

    // LoongArch64 用户态 libc 传入的 sa_restorer 在当前内核地址布局下不一定
    // 是可执行入口（例如 musl 会传 0x2000）。信号返回必须统一落到本内核
    // 映射的 vDSO trampoline，再由它发起 rt_sigreturn syscall。
    let restorer = hal::user::sigreturn_entry_va();

    let saved = UserTrapFrame::from_context(user_ctx.as_usize());
    let old_mask = task
        .signal
        .take_sigsuspend_saved_blocked()
        .unwrap_or_else(|| task.signal.blocked_snapshot());
    let trap_len = UserTrapFrame::encoded_len();
    let total = SIGFRAME_TRAP_OFF
        .checked_add(trap_len)
        .ok_or(Errno::EINVAL)?;
    let new_sp = if action.flags.has(SigActionFlags::SA_ONSTACK) {
        let altstack = task.sigaltstack();
        if !altstack.disabled && !altstack.contains(saved.sp()) {
            let top = altstack
                .sp
                .checked_add(altstack.size)
                .ok_or(Errno::EINVAL)?;
            let sp = top.checked_sub(total).ok_or(Errno::ENOMEM)? & !0xf;
            if sp < altstack.sp {
                return Err(Errno::ENOMEM);
            }
            sp
        } else {
            saved.sp().checked_sub(total).ok_or(Errno::EINVAL)? & !0xf
        }
    } else {
        saved.sp().checked_sub(total).ok_or(Errno::EINVAL)? & !0xf
    };

    // 信号投递同样在 lat_sig 中高频触发；sigframe 大小由固定头部和陷阱帧
    // 构成，使用小型栈缓冲避免 allocator 锁竞争，同时控制内核栈占用。
    let mut frame_storage = [0u8; SIGFRAME_STACK_BUF_SIZE];
    if total > frame_storage.len() {
        return Err(Errno::EINVAL);
    }
    let frame_bytes = &mut frame_storage[..total];
    write_u64(frame_bytes, 0, SIGFRAME_MAGIC);
    write_u64(frame_bytes, 8, total as u64);
    write_u64(frame_bytes, 16, old_mask.raw());
    write_u64(frame_bytes, 24, SIGFRAME_TRAP_OFF as u64);
    write_u64(frame_bytes, 32, trap_len as u64);
    write_u64(frame_bytes, 40, SIGFRAME_SIGINFO_OFF as u64);
    write_u64(frame_bytes, 48, SIGFRAME_UCONTEXT_OFF as u64);
    write_u64(frame_bytes, 56, 0);

    write_siginfo(
        &mut frame_bytes[SIGFRAME_SIGINFO_OFF..][..SIGFRAME_SIGINFO_SIZE],
        info,
    );
    write_u64(frame_bytes, SIGFRAME_UCONTEXT_OFF, 0); // uc_flags
    write_u64(frame_bytes, SIGFRAME_UCONTEXT_OFF + 8, 0); // uc_link
    write_u64(
        frame_bytes,
        SIGFRAME_UCONTEXT_OFF + SIGFRAME_UCONTEXT_SIGMASK_OFF,
        old_mask.raw(),
    );
    let mcontext_start = SIGFRAME_UCONTEXT_OFF + SIGFRAME_UCONTEXT_MCONTEXT_OFF;
    if !saved.write_linux_mcontext(&mut frame_bytes[mcontext_start..][..SIGFRAME_MCONTEXT_SIZE]) {
        return Err(Errno::EINVAL);
    }
    let mut abi_pc = saved.pc();
    if info.sig.raw() >= 32 && (saved.ret() as isize) == -(Errno::EINTR.as_i32() as isize) {
        // musl 的取消信号 handler 用 ucontext PC 判断线程是否正处于
        // __syscall_cp 的可取消区间。syscall dispatcher 已经把真实返回 PC
        // 推到 syscall 之后；ABI ucontext 需要暴露 syscall 指令位置，而
        // 内核私有 trap 副本仍保留真实返回位置供 rt_sigreturn 使用。
        if let Some(syscall_pc) = saved.signal_interrupted_syscall_pc() {
            write_u64(frame_bytes, mcontext_start, syscall_pc as u64);
            abi_pc = syscall_pc;
        }
    }
    write_u64(frame_bytes, 56, abi_pc as u64);
    if !saved.write_bytes(&mut frame_bytes[SIGFRAME_TRAP_OFF..]) {
        return Err(Errno::EINVAL);
    }
    copy_to_user(new_sp, &frame_bytes).map_err(|e| e.as_errno())?;
    #[cfg(target_arch = "riscv64")]
    push_riscv_vector_signal_snapshot(task, user_ctx)?;

    let mut handler_mask = old_mask.union(action.mask);
    if !action.flags.has(SigActionFlags::SA_NODEFER) {
        handler_mask = handler_mask.with(info.sig);
    }
    task.signal.block(handler_mask, SigProcMaskHow::SetMask);
    if action.flags.has(SigActionFlags::SA_RESETHAND) {
        task.shared_signal()
            .set_action(info.sig, SigAction::default_new());
    }

    let mut next = saved;
    next.set_pc(handler_pc);
    next.set_sp(new_sp);
    next.set_args(
        info.sig.raw() as usize,
        new_sp + SIGFRAME_SIGINFO_OFF,
        new_sp + SIGFRAME_UCONTEXT_OFF,
    );
    next.set_ra(restorer);
    next.apply_to_context(user_ctx.as_usize());
    Ok(())
}

fn write_siginfo(out: &mut [u8], info: SigInfo) {
    if let Some(raw) = info.raw {
        let n = out.len().min(raw.len());
        out[..n].copy_from_slice(&raw[..n]);
        return;
    }
    write_i32(out, 0, info.sig.raw() as i32);
    write_i32(out, 4, 0);
    write_i32(out, 8, info.code);
    write_i32(out, 16, info.sender_pid);
    write_u32(out, 20, info.sender_uid.0);
}

fn read_u64(bytes: &[u8], off: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[off..off + 8]);
    u64::from_le_bytes(raw)
}

fn write_u64(bytes: &mut [u8], off: usize, value: u64) {
    bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(bytes: &mut [u8], off: usize, value: i32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

static PROCESS_IMAGE_OPS: ProcessImageOps = ProcessImageOps {
    spawn_user_process: process_spawn_user_process,
    execve: process_execve,
    clone_user_context: process_clone_user_context,
    sigreturn: process_sigreturn,
    prepare_user_return: crate::rseq::prepare_user_return,
    setup_signal_frame: process_setup_signal_frame,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FirmwareSchedGroupKey {
    Socket(u32),
    Cluster {
        socket_id: Option<u32>,
        path: Vec<u32>,
    },
    Core {
        socket_id: Option<u32>,
        cluster_path: Vec<u32>,
        core_id: u32,
    },
    Cpu(u32),
}

struct FirmwareSchedGroup {
    key: FirmwareSchedGroupKey,
    parent: Option<FirmwareSchedGroupKey>,
    mask: sched::CpuMask,
    depth: usize,
}

#[derive(Clone, Copy)]
struct FirmwareSchedDomain {
    span: sched::CpuMask,
    parent: Option<usize>,
    level: u8,
    capacity: u64,
}

fn add_firmware_sched_group(
    groups: &mut Vec<FirmwareSchedGroup>,
    key: FirmwareSchedGroupKey,
    parent: Option<FirmwareSchedGroupKey>,
    cpu: sched::CpuId,
    depth: usize,
) -> Result<(), Errno> {
    if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
        if group.parent != parent || group.depth != depth {
            return Err(Errno::EINVAL);
        }
        group.mask = group.mask.union(cpu.mask());
        return Ok(());
    }
    groups.push(FirmwareSchedGroup {
        key,
        parent,
        mask: cpu.mask(),
        depth,
    });
    Ok(())
}

fn firmware_cpu_capacities(
    cpus: &[(sched::CpuId, &general::dev::cpu::CpuTopologyEntry)],
) -> Vec<(sched::CpuId, u64)> {
    let complete = !cpus.is_empty()
        && cpus
            .iter()
            .all(|(_, cpu)| cpu.capacity_dmips_mhz.is_some_and(|capacity| capacity != 0));
    let maximum = complete
        .then(|| {
            cpus.iter()
                .filter_map(|(_, cpu)| cpu.capacity_dmips_mhz)
                .max()
                .unwrap_or(1)
        })
        .unwrap_or(1);
    cpus.iter()
        .map(|(cpu_id, cpu)| {
            let capacity = if complete {
                u64::from(cpu.capacity_dmips_mhz.unwrap_or(maximum))
                    .saturating_mul(sched::SCHED_CAPACITY_SCALE)
                    / u64::from(maximum)
            } else {
                sched::SCHED_CAPACITY_SCALE
            };
            (*cpu_id, capacity.max(1))
        })
        .collect()
}

fn firmware_domain_capacity(
    span: sched::CpuMask,
    capacities: &[(sched::CpuId, u64)],
) -> Result<u64, Errno> {
    let mut total = 0u64;
    for cpu in span.iter() {
        let capacity = capacities
            .iter()
            .find(|(candidate, _)| *candidate == cpu)
            .map(|(_, capacity)| *capacity)
            .ok_or(Errno::EINVAL)?;
        total = total.checked_add(capacity).ok_or(Errno::EOVERFLOW)?;
    }
    (total != 0).then_some(total).ok_or(Errno::EINVAL)
}

/// 把固件 CPU socket/cluster/core/thread 关系转换成调度器的稳定层级域。
///
/// cluster 编号按完整 ancestry 解释；不同父 cluster 下的同名 `coreN` 不会合并。
/// thread 由每 CPU 叶域表达。连续单子树产生的相同 span 会折叠，因此任意深度的
/// cluster 链不会耗尽调度器固定域表。
fn firmware_sched_topology_from(
    entries: &[general::dev::cpu::CpuTopologyEntry],
) -> Result<Option<sched::SchedTopology>, Errno> {
    let mut cpus = Vec::new();
    let mut firmware_mask = sched::CpuMask::EMPTY;
    for cpu in entries {
        let logical_id = usize::try_from(cpu.logical_id).map_err(|_| Errno::EINVAL)?;
        let Some(cpu_id) = sched::CpuId::new(logical_id) else {
            // 架构启动路径同样只启动 MAX_CPUS 个 CPU；固件多余节点不应让已支持
            // CPU 的拓扑整体失效。
            continue;
        };
        if firmware_mask.contains(cpu_id) {
            return Err(Errno::EINVAL);
        }
        firmware_mask = firmware_mask.union(cpu_id.mask());
        cpus.push((cpu_id, cpu));
    }
    if cpus.is_empty() {
        return Ok(None);
    }
    let has_hierarchy = cpus.iter().any(|(_, cpu)| {
        cpu.socket_id.is_some()
            || !cpu.cluster_path.is_empty()
            || cpu.core_id.is_some()
            || cpu.thread_id.is_some()
    });
    let has_capacity = cpus
        .iter()
        .all(|(_, cpu)| cpu.capacity_dmips_mhz.is_some_and(|capacity| capacity != 0));
    if !has_hierarchy && !has_capacity {
        return Ok(None);
    }

    let capacities = firmware_cpu_capacities(&cpus);
    let mut groups = Vec::new();
    for (cpu_id, cpu) in &cpus {
        let mut parent = None;
        let mut depth = 0usize;

        if let Some(socket_id) = cpu.socket_id {
            depth += 1;
            let key = FirmwareSchedGroupKey::Socket(socket_id);
            add_firmware_sched_group(&mut groups, key.clone(), parent.clone(), *cpu_id, depth)?;
            parent = Some(key);
        }

        for cluster_depth in 1..=cpu.cluster_path.len() {
            depth += 1;
            let key = FirmwareSchedGroupKey::Cluster {
                socket_id: cpu.socket_id,
                path: cpu.cluster_path[..cluster_depth].to_vec(),
            };
            add_firmware_sched_group(&mut groups, key.clone(), parent.clone(), *cpu_id, depth)?;
            parent = Some(key);
        }

        if let Some(core_id) = cpu.core_id {
            depth += 1;
            let key = FirmwareSchedGroupKey::Core {
                socket_id: cpu.socket_id,
                cluster_path: cpu.cluster_path.to_vec(),
                core_id,
            };
            add_firmware_sched_group(&mut groups, key.clone(), parent.clone(), *cpu_id, depth)?;
            parent = Some(key);
        }

        depth += 1;
        add_firmware_sched_group(
            &mut groups,
            FirmwareSchedGroupKey::Cpu(cpu.logical_id),
            parent,
            *cpu_id,
            depth,
        )?;
    }

    groups.sort_by(|left, right| {
        left.depth
            .cmp(&right.depth)
            .then_with(|| left.key.cmp(&right.key))
    });

    let mut resolved: Vec<(FirmwareSchedGroupKey, Option<usize>)> = Vec::new();
    let mut built: Vec<FirmwareSchedDomain> = Vec::new();
    for group in groups {
        let parent = match group.parent.as_ref() {
            Some(parent_key) => resolved
                .iter()
                .find(|(key, _)| key == parent_key)
                .map(|(_, domain)| *domain)
                .ok_or(Errno::EINVAL)?,
            None => None,
        };
        let duplicate_parent_span = parent.is_some_and(|parent| built[parent].span == group.mask);
        let domain = if group.mask == firmware_mask || duplicate_parent_span {
            parent
        } else {
            if built.len() + 1 >= sched::MAX_SCHED_DOMAINS {
                return Err(Errno::E2BIG);
            }
            let level = parent
                .map(|parent| built[parent].level)
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Errno::E2BIG)?;
            let domain = built.len();
            built.push(FirmwareSchedDomain {
                span: group.mask,
                parent,
                level,
                capacity: firmware_domain_capacity(group.mask, &capacities)?,
            });
            Some(domain)
        };
        resolved.push((group.key, domain));
    }

    let mut domains = Vec::with_capacity(built.len() + 1);
    domains.push(sched::SchedDomain::root());
    for domain in built {
        let id = domains.len();
        let parent = domain.parent.map_or(0, |parent| parent + 1);
        domains.push(sched::SchedDomain::with_capacity(
            id,
            domain.span,
            domain.level,
            Some(parent),
            domain.capacity,
        )?);
    }
    sched::SchedTopology::from_domains(&domains).map(Some)
}

fn firmware_sched_topology() -> Result<Option<sched::SchedTopology>, Errno> {
    firmware_sched_topology_from(&general::dev::cpu::snapshot_topology())
}

#[cfg(feature = "kernel-tests")]
mod firmware_topology_tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use general::dev::cpu::CpuTopologyEntry;
    use ktest::ktest;

    use super::firmware_sched_topology_from;

    fn cpu(
        logical_id: u32,
        cluster_path: &[u32],
        core_id: Option<u32>,
        thread_id: Option<u32>,
        capacity: Option<u32>,
    ) -> CpuTopologyEntry {
        CpuTopologyEntry {
            logical_id,
            reg: u64::from(logical_id),
            phandle: Some(logical_id + 1),
            interrupt_controller_phandles: Vec::new().into_boxed_slice(),
            compatible: Vec::new(),
            socket_id: None,
            cluster_path: cluster_path.to_vec().into_boxed_slice(),
            core_id,
            thread_id,
            capacity_dmips_mhz: capacity,
        }
    }

    #[ktest]
    fn nested_cluster_local_core_ids_remain_scoped() {
        let entries = vec![
            cpu(0, &[0], Some(0), Some(0), None),
            cpu(1, &[0], Some(0), Some(1), None),
            cpu(2, &[1], Some(0), Some(0), None),
            cpu(3, &[1], Some(0), Some(1), None),
        ];
        let topology = firmware_sched_topology_from(&entries)
            .expect("valid firmware topology")
            .expect("firmware topology present");

        let cpu0 = sched::CpuId::new(0).unwrap();
        let cpu2 = sched::CpuId::new(2).unwrap();
        let leaf0 = topology.domain_for_cpu(cpu0).unwrap();
        let leaf2 = topology.domain_for_cpu(cpu2).unwrap();
        let near0 = topology.domain(leaf0.parent().unwrap()).unwrap();
        let near2 = topology.domain(leaf2.parent().unwrap()).unwrap();
        assert_eq!(near0.span().bits(), 0b0011);
        assert_eq!(near2.span().bits(), 0b1100);
        assert_ne!(near0.id(), near2.id());
    }

    #[ktest]
    fn firmware_capacity_is_normalized_and_used() {
        let entries = vec![
            cpu(0, &[], None, None, Some(1)),
            cpu(1, &[], None, None, Some(2)),
        ];
        let topology = firmware_sched_topology_from(&entries)
            .expect("valid capacities")
            .expect("capacity topology present");

        assert_eq!(
            topology.cpu_capacity(sched::CpuId::new(0).unwrap()),
            sched::SCHED_CAPACITY_SCALE / 2
        );
        assert_eq!(
            topology.cpu_capacity(sched::CpuId::new(1).unwrap()),
            sched::SCHED_CAPACITY_SCALE
        );
    }

    #[ktest]
    fn firmware_topology_ignores_cpu_ids_beyond_kernel_capacity() {
        let entries = vec![
            cpu(0, &[0], Some(0), None, None),
            cpu(1, &[0], Some(1), None, None),
            cpu(sched::NR_CPUS as u32, &[1], Some(0), None, None),
        ];
        let topology = firmware_sched_topology_from(&entries)
            .expect("unsupported firmware CPUs must be cropped")
            .expect("supported topology present");

        assert!(
            topology
                .domain_for_cpu(sched::CpuId::new(0).unwrap())
                .is_some()
        );
        assert!(
            topology
                .domain_for_cpu(sched::CpuId::new(1).unwrap())
                .is_some()
        );
    }
}

/// 在架构层完成 boot CPU 优先的 logical-id 重排后安装固件调度拓扑。
pub fn install_firmware_topology() {
    match firmware_sched_topology() {
        Ok(Some(topology)) => {
            let domains = topology.len();
            sched::scheduler_state::SCHEDULER.install_topology(topology);
            log::info!(
                "[sched][boot] installed firmware CPU topology: domains={}",
                domains
            );
        }
        Ok(None) => log::info!("[sched][boot] no firmware CPU topology; using Root->Cpu"),
        Err(error) => log::warning!(
            "[sched][boot] invalid firmware CPU topology ({:?}); using Root->Cpu",
            error
        ),
    }
}

/// 启动期入口：注入 arch hook → 注册 ext clone hook → 建 init →
/// 给 init 装 VfsContext + FdTable → 启动 CPU 0 的 idle 内核线程。
pub fn boot_init() -> Arc<Task> {
    // 1. arch 侧装入上下文切换 / 时间 / trap-stack / mm / syscall 五套契约。
    hal::sched::register_arch_hooks();

    // 2. 注入 ext clone hook，必须在 sched::init 之前——否则 init 任务后续
    //    任何 fork/clone 都会落到无 hook 的"全共享"分支。
    sched::register_ext_clone_hook(&HOOK);

    // 3. 注入 ext exit hook，让 wait/reap 能在 kernel 上下文释放 VM/FDT 等大对象。
    sched::register_ext_exit_hook(&EXIT_HOOK);

    // 4. 在 exit waiter 唤醒前输出进程记账，保证 wait 返回时记录已经可见。
    sched::register_exit_accounting_hook(&EXIT_ACCOUNTING_HOOK);

    // 5. 注入 pre-exit hook。robust futex / clear-child-tid 必须在释放 VM 前完成。
    sched::register_pre_exit_hook(&PRE_EXIT_HOOK);

    // 6. 注入用户进程镜像 ops。sched 只依赖这张表，不直接依赖 ELF/MM/trap。
    sched::register_process_image_ops(&PROCESS_IMAGE_OPS);

    // 7. 注入 VmSwitchOps：schedule_once 切换前据此激活用户页表。注册点必须
    //    在 sched::init 之前，这样即便 init 之外的 kthread 启动也会被回调。
    sched::arch_hooks::register_vm_switch(&VM_SWITCH_OPS);

    // 8. 注入任务 CPU 状态发布 hook：调度器切到用户任务前用它刷新 rseq。
    sched::arch_hooks::register_task_cpu_state(&TASK_CPU_STATE_OPS);

    // 9. 建 init。sched::init 内部会 assert arch_hooks 已注入。
    let init = sched::init();

    // allocator 的自旋锁可能与内核堆回收触发的全核 TLB shootdown 形成锁环。
    // 调度器和架构紧急回调就绪后，让所有 allocator 竞争路径协作消费请求。
    allocator::KERNEL_ALLOCATOR
        .bind_urgent_poll(sched::urgent_pending_slots(), sched::poll_urgent_work);

    // 10. 把启动期 stash 的 VFS 部件挂到 init 任务上。acpi / dtb 路径若没走过
    //    （理论上不会）就跳过——调度 / 信号路径不依赖 ext，仅 VFS syscall 受影响。
    if let Some(parts) = BOOT_VFS_PARTS.lock().take() {
        let vfs_ctx = Arc::new(VfsContext::new(
            parts.cwd,
            parts.cwd_mount,
            parts.root,
            parts.mount_ns,
            VfsArc::clone(&parts.cred),
            parts.umask,
            VfsArc::clone(&parts.limits),
        ));
        let fdtable = Arc::new(FdTable::new(&parts.limits));

        // 给 init 预装 fd 0/1/2。要在 FdTable 挂到 init 之前装——install_stdio
        // 用同一个 FdTable 对象，挂完之后再 ext_install 会共享。
        crate::stdio::install_from_stash(&vfs_ctx, &fdtable, take_boot_console_name());

        init.ext_install(TASKEXT_VFS_CONTEXT, vfs_ctx);
        init.ext_install(TASKEXT_VFS_FDTABLE, fdtable);
        // 根命名空间（uts/ipc/time/cgroup/pid）。
        let root_ns: Arc<dyn core::any::Any + Send + Sync> = crate::ns::NsProxy::root();
        init.ext_install(crate::ns::TASKEXT_NS, root_ns);
        log::info!("[sched][boot] init ext: vfs ctx + fdtable + stdio + ns installed");
    } else {
        log::info!("[sched][boot] BOOT_VFS_PARTS empty — init has no vfs ext");
    }

    // 11. 为 CPU 0 启动独立 idle 内核线程。`pick_next` 返 None 时 schedule_once
    //    会回落到这个 idle，main() 后续显式让渡时也按它兜底。
    sched::spawn_idle_for(0);

    // 9. 注册全套 syscall 实现（kernel::syscalls::register_all 把 fs/process/
    //    mm/signal 四类实现写进 general::syscall 的全局表）。
    crate::syscalls::register_all();
    // pid 命名空间：子进程的命名空间由 kernel 的 NsProxy.pending_pid 决定。
    sched::spawn::register_child_pid_ns_hook(|parent| {
        crate::ns::task_ns(parent).pending_pid.lock().take()
    });
    crate::native_runtime::register();

    init
}

const RAMDISK_INIT: &str = "/init";
pub(crate) const INIT_CANDIDATES: [&str; 4] = ["/sbin/init", "/etc/init", "/bin/init", "/bin/sh"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InitCommandLine<'a> {
    pub(crate) rdinit: Option<&'a str>,
    pub(crate) init: Option<&'a str>,
}

pub(crate) fn parse_init_command_line(cmdline: Option<&[u8]>) -> InitCommandLine<'_> {
    let Some(cmdline) = cmdline else {
        return InitCommandLine::default();
    };
    let cmdline = general::cmdline::Cmdline::new(cmdline);
    InitCommandLine {
        rdinit: cmdline.find("rdinit"),
        init: cmdline.find("init"),
    }
}

pub(crate) fn ramdisk_init_command(cmdline: Option<&[u8]>) -> &str {
    parse_init_command_line(cmdline)
        .rdinit
        .unwrap_or(RAMDISK_INIT)
}

fn load_init_process(
    init: &Arc<Task>,
    path: &str,
    init_args: &[String],
    envp: &[String],
) -> Result<(crate::user::LoadedUserImage, Vec<String>), Errno> {
    let mut argv = Vec::with_capacity(init_args.len() + 1);
    argv.push(String::from(path));
    argv.extend(init_args.iter().cloned());
    let loaded = crate::user::load_user_image_from_path(init, path, &argv, envp)?;
    Ok((loaded, argv))
}

fn enter_init_process(
    init: &Arc<Task>,
    path: &str,
    envp: &[String],
    loaded: crate::user::LoadedUserImage,
    argv: &[String],
) -> ! {
    log::info!("[sched][init] starting user init '{}'", path);
    enter_loaded_user_image(init, loaded, argv, envp)
}

/// 提取独立 `--` 之后交给 PID 1 的参数。Linux 的 `set_init_arg()` 会把这些
/// token 作为 argv，而不是当作内核参数或环境变量；这里按 Linux `next_arg()`
/// 的双引号规则分词并返回拥有的字符串，避免修改固件提供的只读快照。
pub(crate) fn init_args_after_delimiter(cmdline: Option<&[u8]>) -> Vec<String> {
    let Some(bytes) = cmdline else {
        return Vec::new();
    };
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    let mut cursor = 0usize;
    let mut after_delimiter = false;
    let mut args = Vec::new();

    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let start = cursor;
        let mut quote = false;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if byte == b'"' {
                quote = !quote;
            } else if byte.is_ascii_whitespace() && !quote {
                break;
            }
            cursor += 1;
        }
        let token = &bytes[start..cursor];
        if !after_delimiter {
            if token == b"--" || token == b"\"--\"" {
                after_delimiter = true;
            }
            continue;
        }

        args.push(decode_linux_init_arg(token));
    }
    args
}

fn decode_linux_init_arg(token: &[u8]) -> String {
    let token = if token.first() == Some(&b'"') {
        let token = &token[1..];
        token.strip_suffix(b"\"").unwrap_or(token)
    } else if let Some(equals) = token.iter().position(|&byte| byte == b'=')
        && token.get(equals + 1) == Some(&b'"')
        && token.last() == Some(&b'"')
    {
        let mut value = Vec::with_capacity(token.len().saturating_sub(2));
        value.extend_from_slice(&token[..=equals]);
        value.extend_from_slice(&token[equals + 2..token.len() - 1]);
        return String::from_utf8_lossy(&value).into_owned();
    } else {
        token
    };
    String::from_utf8_lossy(token).into_owned()
}

/// Replace the boot init task with the first user-space init image that exists.
///
/// This keeps PID 1 as the real user init process instead of spawning init as a
/// child of the kernel idle loop.
pub fn start_init_process(init: &Arc<Task>) -> ! {
    let envp = [
        String::from("PATH=/bin:/sbin:/usr/bin:/usr/sbin"),
        String::from("HOME=/"),
        String::from("TERM=linux"),
    ];
    let commands = parse_init_command_line(general::start_cmdline());
    let init_args = init_args_after_delimiter(general::start_cmdline());

    if BOOT_ROOT_IS_INITRAMFS.load(Ordering::Acquire) {
        let path = ramdisk_init_command(general::start_cmdline());
        match load_init_process(init, path, &init_args, &envp) {
            Ok((loaded, argv)) => enter_init_process(init, path, &envp, loaded, &argv),
            Err(err) => log::error!(
                "[sched][init] failed to execute ramdisk init '{}': {:?}",
                path,
                err
            ),
        }
    }

    if let Some(path) = commands.init {
        match load_init_process(init, path, &init_args, &envp) {
            Ok((loaded, argv)) => enter_init_process(init, path, &envp, loaded, &argv),
            Err(err) => panic!("[sched][init] requested init '{}' failed: {:?}", path, err),
        }
    }

    for path in INIT_CANDIDATES {
        match load_init_process(init, path, &init_args, &envp) {
            Ok((loaded, argv)) => enter_init_process(init, path, &envp, loaded, &argv),
            Err(Errno::ENOENT) => {}
            Err(err) => log::error!(
                "[sched][init] '{}' exists but could not be executed: {:?}",
                path,
                err
            ),
        }
    }

    panic!("[sched][init] no working init found; try passing init= to the kernel");
}

fn enter_loaded_user_image(
    task: &Arc<Task>,
    loaded: crate::user::LoadedUserImage,
    argv: &[String],
    envp: &[String],
) -> ! {
    let exec_path = loaded.exec_path.clone();
    // 保留旧地址空间的最后一个引用，直到新页表已经安装。否则 ext_remove
    // 会立即 drop 旧 VmSpace，而 RISC-V 仍可能把它记在 CURRENT_USER_PGD 中。
    let old_vm = task.ext_remove(TASKEXT_VM_SPACE);
    task.ext_install(TASKEXT_VM_SPACE, loaded.vm.clone());
    install_exec_access(task, Arc::clone(&loaded.exec_access));
    install_exec_metadata(task, &exec_path, argv, envp);
    #[cfg(feature = "performance-profile")]
    install_profile_images(task, &loaded);
    if let Some(fdt) = task_fdtable(task) {
        fdt.close_on_exec(task.pid_root().unwrap_or(0));
    }

    let kstack_top = task.ensure_kernel_stack();
    loaded.vm.activate();
    drop(old_vm);
    hal::user_context::set_kernel_trap_stack(kstack_top);
    let mut frame = UserTrapFrame::init_user(loaded.entry_pc, loaded.user_sp, 0);
    frame.set_kernel_stack_top(kstack_top);
    unsafe { frame.resume() }
}

/// 启动期自检：数据结构 + pid + 真实上下文切换 + POSIX 动词场景 + ext fork。
#[cfg(debug_assertions)]
pub fn smoketest() {
    sched::operation::smoketest::run();
}

#[cfg(not(debug_assertions))]
pub fn smoketest() {}
