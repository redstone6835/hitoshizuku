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

use errno::Errno;
use general::mm::{VmSpace, copy_cstr_from_user, copy_from_user, copy_to_user};
use general::vfs::{
    Credentials, Dentry, FdTable, FileMode, Mount, MountNamespace, VfsContext, VfsLimits, VfsRoot,
    build_boot_vfs_parts,
};
use hal::user_context::UserTrapFrame;
use sched::arch_hooks::VmSwitchOps;
use sched::clone_flags::{CloneArgs, CloneFlags};
use sched::process_ops::{ExecPath, ExecRequest, ProcessImageOps, UserContextRef};
use sched::signal::{SigAction, SigActionFlags, SigHandler, SigInfo, SigProcMaskHow, SigSet};
use sched::sync::Spinlock;
use sched::task::{TaskExtCloneHook, TaskExtExitHook, TaskExtKey, TaskPreExitHook};
use sched::{
    TASKEXT_USER_TRAP_FRAME, TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE, TASKEXT_VM_SPACE, Task,
};
use vfs::Arc as VfsArc;

/// acpi / dtb 启动路径在控制台挂载完成后，把 VFS 根部件存这里；
/// [`boot_init`] 再取出来装到 init 任务上。
static BOOT_VFS_PARTS: Spinlock<Option<BootVfsParts>> = Spinlock::new(None);

/// 控制台路径或 devtmpfs 节点名（例如 "/dev/console" 或 "uart0"）。stash 后
/// install_stdio 用它走 openat 路径打开 fd 0/1/2。
static BOOT_CONSOLE_NAME: Spinlock<Option<alloc::string::String>> = Spinlock::new(None);

pub(crate) const TASKEXT_EXEC_PATH: TaskExtKey = sched::TASKEXT_EXEC_PATH;
pub(crate) const TASKEXT_EXEC_ARGS: TaskExtKey = sched::TASKEXT_EXEC_ARGS;
pub(crate) const TASKEXT_EXEC_ENVP: TaskExtKey = sched::TASKEXT_EXEC_ENVP;

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
            _ => Arc::clone(src),
        }
    }
}

static HOOK: KernelExtCloneHook = KernelExtCloneHook;

struct KernelExtExitHook;

impl TaskExtExitHook for KernelExtExitHook {
    fn cleanup_on_exit(&self, task: &Arc<Task>) {
        let _ = task.ext_remove(TASKEXT_USER_TRAP_FRAME);
        let _ = task.ext_remove(TASKEXT_VM_SPACE);
        let _ = task.ext_remove(TASKEXT_VFS_FDTABLE);
        let _ = task.ext_remove(TASKEXT_VFS_CONTEXT);
    }
}

static EXIT_HOOK: KernelExtExitHook = KernelExtExitHook;

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
// 里找 TASKEXT_VM_SPACE payload，若在 → downcast 成 VmSpace 再 activate。
// 没挂（纯 kthread 或 init）→ no-op。

fn vm_on_switch(next: &Arc<Task>) {
    if let Some(payload) = next.ext_lookup(TASKEXT_VM_SPACE) {
        if let Ok(vm) = payload.downcast::<VmSpace>() {
            vm.activate();
        }
    }
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
    let registration = task.rseq_registration();
    if !registration.registered {
        return;
    }
    let Ok(cpu) = u32::try_from(cpu_id) else {
        return;
    };
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

const EXEC_PATH_MAX: usize = 4096;
const EXEC_MAX_STRINGS: usize = 256;
const EXEC_MAX_ARG_BYTES: usize = 128 * 1024;

const SIGFRAME_MAGIC: u64 = 0x4d59474f_53494746; // "MYGOSIGF"
const SIGFRAME_HEADER_SIZE: usize = 64;
const SIGFRAME_SIGINFO_SIZE: usize = 128;
const SIGFRAME_UCONTEXT_HEAD_SIZE: usize = 64;
const SIGFRAME_SIGINFO_OFF: usize = SIGFRAME_HEADER_SIZE;
const SIGFRAME_UCONTEXT_OFF: usize = SIGFRAME_SIGINFO_OFF + SIGFRAME_SIGINFO_SIZE;
const SIGFRAME_TRAP_OFF: usize = SIGFRAME_UCONTEXT_OFF + SIGFRAME_UCONTEXT_HEAD_SIZE;

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

fn read_user_usize(user: usize) -> Result<usize, Errno> {
    let mut raw = [0u8; size_of::<usize>()];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(usize::from_ne_bytes(raw))
}

fn write_user_pid_t(user: usize, value: sched::pid::PidT) -> Result<(), Errno> {
    copy_to_user(user, &value.to_ne_bytes()).map_err(|e| e.as_errno())
}

fn collect_user_string_array(
    table_user: usize,
    used_bytes: &mut usize,
) -> Result<Vec<String>, Errno> {
    let mut out = Vec::new();
    if table_user == 0 {
        return Ok(out);
    }

    for idx in 0..EXEC_MAX_STRINGS {
        let ptr_addr = table_user
            .checked_add(idx.checked_mul(size_of::<usize>()).ok_or(Errno::EINVAL)?)
            .ok_or(Errno::EINVAL)?;
        let str_user = read_user_usize(ptr_addr)?;
        if str_user == 0 {
            return Ok(out);
        }
        let remaining = EXEC_MAX_ARG_BYTES
            .checked_sub(*used_bytes)
            .ok_or(Errno::EINVAL)?;
        if remaining == 0 {
            return Err(Errno::EINVAL);
        }
        let s = copy_cstr_from_user(str_user, remaining).map_err(|e| e.as_errno())?;
        *used_bytes = used_bytes.checked_add(s.len() + 1).ok_or(Errno::EINVAL)?;
        if *used_bytes > EXEC_MAX_ARG_BYTES {
            return Err(Errno::EINVAL);
        }
        out.push(s);
    }
    Err(Errno::EINVAL)
}

fn activate_task_vm(task: &Arc<Task>) {
    if let Some(vm) = task_vm_space(task) {
        vm.activate();
    }
}

unsafe extern "C" fn user_clone_entry(_arg: usize) -> ! {
    let frame = {
        let me = sched::current_task();
        activate_task_vm(&me);
        let kstack_top = me
            .kernel_stack_top()
            .expect("[sched][clone] user child missing kernel stack");
        hal::user_context::set_kernel_trap_stack(kstack_top);

        let payload = me
            .ext_remove(TASKEXT_USER_TRAP_FRAME)
            .expect("[sched][clone] user child missing saved trap frame");
        let frame = payload
            .downcast::<UserTrapFrame>()
            .expect("[sched][clone] saved trap frame type mismatch");
        *frame
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

    let old_vm = task_vm_space(task);
    let path = match request.path {
        ExecPath::User(path_user) => {
            copy_cstr_from_user(path_user, EXEC_PATH_MAX).map_err(|e| e.as_errno())?
        }
        ExecPath::Kernel(path) => path,
    };
    let mut used = path.len().checked_add(1).ok_or(Errno::EINVAL)?;
    let argv = collect_user_string_array(request.argv_user, &mut used)?;
    let envp = collect_user_string_array(request.envp_user, &mut used)?;

    let loaded = match crate::user::load_user_image_from_path(task, &path, &argv, &envp) {
        Ok(loaded) => loaded,
        Err(err) => {
            if err == Errno::ENOEXEC {
                // shell 执行无 shebang 的脚本时会先尝试 execve，收到
                // ENOEXEC 后回退为解释执行；这是用户态正常探测路径。
                log::debug!("[exec] load failed: path={:?} err={:?}", path, err);
            } else {
                log::info!("[exec] load failed: path={:?} err={:?}", path, err);
            }
            if let Some(vm) = old_vm {
                vm.activate();
            }
            return Err(err);
        }
    };

    let _ = task.ext_remove(TASKEXT_VM_SPACE);
    task.ext_install(TASKEXT_VM_SPACE, loaded.vm.clone());
    loaded.vm.activate();
    install_exec_metadata(task, &loaded.exec_path, &argv, &envp);
    if let Some(fdt) = task_fdtable(task) {
        fdt.close_on_exec();
    }

    // exec 时将 caught 信号重置为 SIG_DFL
    task.thread_group().shared_signal().reset_handlers_for_exec();

    let frame = UserTrapFrame::init_user(loaded.entry_pc, loaded.user_sp, 0);
    frame.apply_to_context(user_ctx.as_usize());
    Ok(())
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
    let trap_end = trap_off.checked_add(trap_len).ok_or(Errno::EINVAL)?;
    if trap_off < SIGFRAME_HEADER_SIZE
        || trap_len != UserTrapFrame::encoded_len()
        || trap_end > total
        || total > 16 * 1024
    {
        return Err(Errno::EINVAL);
    }

    let mut bytes = Vec::new();
    bytes.resize(trap_len, 0);
    copy_from_user(sp.checked_add(trap_off).ok_or(Errno::EINVAL)?, &mut bytes)
        .map_err(|e| e.as_errno())?;
    let restored = UserTrapFrame::read_bytes(&bytes).ok_or(Errno::EINVAL)?;
    task.signal
        .block(SigSet::from_raw(old_mask), SigProcMaskHow::SetMask);
    restored.apply_to_context(user_ctx.as_usize());
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
    let old_mask = task.signal.blocked_snapshot();
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

    let mut frame_bytes = Vec::new();
    frame_bytes.resize(total, 0);
    write_u64(&mut frame_bytes, 0, SIGFRAME_MAGIC);
    write_u64(&mut frame_bytes, 8, total as u64);
    write_u64(&mut frame_bytes, 16, old_mask.raw());
    write_u64(&mut frame_bytes, 24, SIGFRAME_TRAP_OFF as u64);
    write_u64(&mut frame_bytes, 32, trap_len as u64);
    write_u64(&mut frame_bytes, 40, SIGFRAME_SIGINFO_OFF as u64);
    write_u64(&mut frame_bytes, 48, SIGFRAME_UCONTEXT_OFF as u64);
    write_u64(&mut frame_bytes, 56, info.sig.raw() as u64);

    write_siginfo(
        &mut frame_bytes[SIGFRAME_SIGINFO_OFF..][..SIGFRAME_SIGINFO_SIZE],
        info,
    );
    write_u64(&mut frame_bytes, SIGFRAME_UCONTEXT_OFF, 0); // uc_flags
    write_u64(&mut frame_bytes, SIGFRAME_UCONTEXT_OFF + 8, 0); // uc_link
    write_u64(&mut frame_bytes, SIGFRAME_UCONTEXT_OFF + 40, old_mask.raw()); // uc_sigmask
    if !saved.write_bytes(&mut frame_bytes[SIGFRAME_TRAP_OFF..]) {
        return Err(Errno::EINVAL);
    }
    copy_to_user(new_sp, &frame_bytes).map_err(|e| e.as_errno())?;

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
    execve: process_execve,
    clone_user_context: process_clone_user_context,
    sigreturn: process_sigreturn,
    setup_signal_frame: process_setup_signal_frame,
};

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

    // 4. 注入 pre-exit hook。robust futex / clear-child-tid 必须在释放 VM 前完成。
    sched::register_pre_exit_hook(&PRE_EXIT_HOOK);

    // 5. 注入用户进程镜像 ops。sched 只依赖这张表，不直接依赖 ELF/MM/trap。
    sched::register_process_image_ops(&PROCESS_IMAGE_OPS);

    // 6. 注入 VmSwitchOps：schedule_once 切换前据此激活用户页表。注册点必须
    //    在 sched::init 之前，这样即便 init 之外的 kthread 启动也会被回调。
    sched::arch_hooks::register_vm_switch(&VM_SWITCH_OPS);

    // 7. 注入任务 CPU 状态发布 hook：调度器切到用户任务前用它刷新 rseq。
    sched::arch_hooks::register_task_cpu_state(&TASK_CPU_STATE_OPS);

    // 8. 建 init。sched::init 内部会 assert arch_hooks 已注入。
    let init = sched::init();

    // 7. 把启动期 stash 的 VFS 部件挂到 init 任务上。acpi / dtb 路径若没走过
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
        log::info!("[sched][boot] init ext: vfs ctx + fdtable + stdio installed");
    } else {
        log::info!("[sched][boot] BOOT_VFS_PARTS empty — init has no vfs ext");
    }

    // 8. 为 CPU 0 启动独立 idle 内核线程。`pick_next` 返 None 时 schedule_once
    //    会回落到这个 idle，main() 后续显式让渡时也按它兜底。
    sched::spawn_idle_for(0);

    // 9. 注册全套 syscall 实现（kernel::syscalls::register_all 把 fs/process/
    //    mm/signal 四类实现写进 general::syscall 的全局表）。
    crate::syscalls::register_all();

    init
}

const INIT_CANDIDATES: [&str; 3] = ["/init", "/sbin/init", "/bin/init"];

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
    let mut last_error = Errno::ENOENT;

    for path in INIT_CANDIDATES {
        let argv = [String::from(path)];
        match crate::user::load_user_image_from_path(init, path, &argv, &envp) {
            Ok(loaded) => {
                log::info!("[sched][init] starting user init '{}'", path);
                enter_loaded_user_image(init, loaded, &argv, &envp)
            }
            Err(err) => {
                last_error = err;
                log::info!("[sched][init] cannot start '{}': {:?}", path, err);
            }
        }
    }

    panic!(
        "[sched][init] failed to start init from {:?}: last error {:?}",
        INIT_CANDIDATES, last_error
    );
}

fn enter_loaded_user_image(
    task: &Arc<Task>,
    loaded: crate::user::LoadedUserImage,
    argv: &[String],
    envp: &[String],
) -> ! {
    let exec_path = loaded.exec_path.clone();
    let _ = task.ext_remove(TASKEXT_VM_SPACE);
    task.ext_install(TASKEXT_VM_SPACE, loaded.vm.clone());
    install_exec_metadata(task, &exec_path, argv, envp);
    if let Some(fdt) = task_fdtable(task) {
        fdt.close_on_exec();
    }

    let kstack_top = task.ensure_kernel_stack();
    loaded.vm.activate();
    let frame = UserTrapFrame::init_user(loaded.entry_pc, loaded.user_sp, 0);
    hal::user_context::set_kernel_trap_stack(kstack_top);
    unsafe { frame.resume() }
}

/// 启动期自检：数据结构 + pid + 真实上下文切换 + POSIX 动词场景 + ext fork。
#[cfg(debug_assertions)]
pub fn smoketest() {
    sched::operation::smoketest::run();
}

#[cfg(not(debug_assertions))]
pub fn smoketest() {}
