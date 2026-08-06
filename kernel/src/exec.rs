//! 进程映像替换事务。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use errno::Errno;
use general::mm::{copy_cstr_from_user, copy_from_user, VmSpace};
use general::vfs::{FdTable, VfsContext};
use hal::user_context::UserTrapFrame;
use native_abi::ExecPhase;
use native_abi::UserAbiKind;
use sched::group::{ProcessPersonalityState, ThreadGroupExecGuard};
use sched::process_ops::{ExecPath, ExecRequest, UserContextRef};
use sched::{
    PreparedSignalActions, SharedSignal, Task, TASKEXT_EXEC_ACCESS, TASKEXT_EXEC_ARGS,
    TASKEXT_EXEC_ENVP, TASKEXT_EXEC_PATH, TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE,
    TASKEXT_VM_SPACE,
};

use crate::syscalls::{cleanup_task_for_exec, ExecCleanupScratch};
use crate::user::{ExecutableAccessSet, LoadedUserImage};

const EXEC_PATH_MAX: usize = 4096;
const EXEC_MAX_STRINGS: usize = 4096;
const EXEC_MAX_ARG_BYTES: usize = 128 * 1024;

/// 已完成格式解析、映射和权限封口的新映像。
pub(crate) struct PreparedImage {
    vm: Arc<VmSpace>,
    exec_access: Arc<ExecutableAccessSet>,
    #[cfg(feature = "performance-profile")]
    main_profile: (u64, usize, usize),
    #[cfg(feature = "performance-profile")]
    interpreter_profile: (u64, usize, usize),
}

/// 将在提交时一次发布的进程 ABI 身份。
pub(crate) struct PreparedPersonality {
    state: ProcessPersonalityState,
}

/// 已按目标 personality 规则构造完毕的进程资源。
pub(crate) struct PreparedResources {
    fdtable: Option<Arc<FdTable>>,
    signal_actions: PreparedSignalActions,
}

/// 新进程启动块以及 procfs 可见的执行元数据。
pub(crate) struct PreparedStartup {
    exec_path: Arc<String>,
    argv: Arc<Vec<String>>,
    envp: Arc<Vec<String>>,
    comm: [u8; sched::TASK_COMM_LEN],
}

/// 首线程返回用户态所需的完整架构状态。
pub(crate) struct PreparedInitialThread {
    frame: UserTrapFrame,
    kernel_stack_top: usize,
}

struct ObservedFdTable {
    table: Arc<FdTable>,
    generation: u64,
}

/// pure prepare 读取的旧进程身份，commit 必须在 exec lock 下重新核对。
pub(crate) struct ExecSnapshot {
    exec_generation: u64,
    source_abi: UserAbiKind,
    vm: Option<Arc<VmSpace>>,
    fdtable: Option<ObservedFdTable>,
    vfs_context: Option<Arc<VfsContext>>,
    vfs_generation: u64,
    exec_access: Arc<dyn core::any::Any + Send + Sync>,
    shared_signal: Arc<SharedSignal>,
    signal_generation: u64,
}

/// 一次映像替换所需的全部预构造状态。
pub(crate) struct PreparedExec {
    pub image: PreparedImage,
    pub personality: PreparedPersonality,
    pub resources: PreparedResources,
    pub startup: PreparedStartup,
    pub initial_thread: PreparedInitialThread,
    pub cleanup: ExecCleanupScratch,
    pub observed: ExecSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallStep {
    FileDescriptors,
    AddressSpace,
    ExecutableAccess,
    ExecPath,
    Arguments,
    Environment,
    Personality,
    InitialThread,
    UserContext,
}

const INSTALL_STEPS: [InstallStep; 9] = [
    InstallStep::FileDescriptors,
    InstallStep::AddressSpace,
    InstallStep::ExecutableAccess,
    InstallStep::ExecPath,
    InstallStep::Arguments,
    InstallStep::Environment,
    InstallStep::Personality,
    InstallStep::InitialThread,
    InstallStep::UserContext,
];

/// 在发布 `Transitioning` 前完成全部可失败重验；本函数不修改线程组状态。
fn revalidate_before_ponr(
    guard: &mut ThreadGroupExecGuard<'_>,
    observed_generation: u64,
    revalidate_resources: impl FnOnce() -> Result<(), Errno>,
) -> Result<(), Errno> {
    if guard.phase() != ExecPhase::Running {
        return Err(Errno::EBUSY);
    }
    if guard.generation() != observed_generation {
        return Err(Errno::EAGAIN);
    }
    revalidate_resources()
}

/// 驱动 PONR 后的固定安装序列，并集中执行失败策略和最终发布。
fn drive_install_steps<E>(
    guard: &mut ThreadGroupExecGuard<'_>,
    mut install: impl FnMut(&mut ThreadGroupExecGuard<'_>, InstallStep) -> Result<(), E>,
) -> Result<(), E> {
    for step in INSTALL_STEPS {
        if let Err(error) = install(guard, step) {
            guard.set_phase(ExecPhase::Terminating);
            return Err(error);
        }
    }
    guard.advance_generation();
    guard.set_phase(ExecPhase::Running);
    Ok(())
}

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

fn task_vfs_context(task: &Arc<Task>) -> Option<Arc<VfsContext>> {
    task.ext_lookup(TASKEXT_VFS_CONTEXT)?
        .downcast::<VfsContext>()
        .ok()
}

fn read_user_usize(user: usize) -> Result<usize, Errno> {
    let mut raw = [0u8; size_of::<usize>()];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;
    Ok(usize::from_ne_bytes(raw))
}

fn collect_user_string_array(
    table_user: usize,
    used_bytes: &mut usize,
) -> Result<Vec<String>, Errno> {
    let mut strings = Vec::new();
    if table_user == 0 {
        return Ok(strings);
    }
    strings
        .try_reserve(EXEC_MAX_STRINGS.min(64))
        .map_err(|_| Errno::ENOMEM)?;
    for index in 0..EXEC_MAX_STRINGS {
        let pointer_address = table_user
            .checked_add(index.checked_mul(size_of::<usize>()).ok_or(Errno::EINVAL)?)
            .ok_or(Errno::EINVAL)?;
        let string_user = read_user_usize(pointer_address)?;
        if string_user == 0 {
            return Ok(strings);
        }
        let remaining = EXEC_MAX_ARG_BYTES
            .checked_sub(*used_bytes)
            .ok_or(Errno::EINVAL)?;
        if remaining == 0 {
            return Err(Errno::EINVAL);
        }
        let value =
            copy_cstr_from_user(string_user, remaining).map_err(|error| error.as_errno())?;
        *used_bytes = used_bytes
            .checked_add(value.len() + 1)
            .ok_or(Errno::EINVAL)?;
        if *used_bytes > EXEC_MAX_ARG_BYTES {
            return Err(Errno::EINVAL);
        }
        strings.push(value);
    }
    Err(Errno::EINVAL)
}

fn prepare_comm(path: &str) -> [u8; sched::TASK_COMM_LEN] {
    let name = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .as_bytes();
    let mut comm = [0u8; sched::TASK_COMM_LEN];
    let length = name.len().min(sched::TASK_COMM_LEN - 1);
    comm[..length].copy_from_slice(&name[..length]);
    comm
}

fn same_optional_arc<T>(current: Option<Arc<T>>, observed: &Option<Arc<T>>) -> bool {
    match (current, observed) {
        (Some(current), Some(observed)) => Arc::ptr_eq(&current, observed),
        (None, None) => true,
        _ => false,
    }
}

impl ExecSnapshot {
    fn revalidate(&self, task: &Arc<Task>) -> Result<(), Errno> {
        if task.user_abi_kind() != self.source_abi
            || !same_optional_arc(task_vm_space(task), &self.vm)
            || !same_optional_arc(task_vfs_context(task), &self.vfs_context)
            || task_vfs_context(task)
                .is_some_and(|context| context.generation() != self.vfs_generation)
            || !task.ext_is_current(TASKEXT_EXEC_ACCESS, &self.exec_access)
            || !Arc::ptr_eq(&task.shared_signal(), &self.shared_signal)
            || task.shared_signal().actions_generation() != self.signal_generation
        {
            return Err(Errno::EAGAIN);
        }
        match (&self.fdtable, task_fdtable(task)) {
            (Some(observed), Some(current))
                if Arc::ptr_eq(&observed.table, &current)
                    && current.is_generation_current(observed.generation) => {}
            (None, None) => {}
            _ => return Err(Errno::EAGAIN),
        }
        for key in [TASKEXT_EXEC_PATH, TASKEXT_EXEC_ARGS, TASKEXT_EXEC_ENVP] {
            if task.ext_lookup(key).is_none() {
                return Err(Errno::EAGAIN);
            }
        }
        Ok(())
    }
}

/// 构造一个完整但尚未对旧进程可见的 ELF/Tomori 替换事务。
pub(crate) fn prepare_exec(task: &Arc<Task>, request: ExecRequest) -> Result<PreparedExec, Errno> {
    let group = task.thread_group();
    if group.exec_phase() != ExecPhase::Running {
        return Err(Errno::EBUSY);
    }
    let old_vm = task_vm_space(task);
    let observed_exec_access = task.ext_lookup(TASKEXT_EXEC_ACCESS).ok_or(Errno::EAGAIN)?;
    let observed_fdtable = task_fdtable(task).map(|table| {
        let generation = table.generation();
        ObservedFdTable { table, generation }
    });
    let observed = ExecSnapshot {
        exec_generation: group.exec_generation(),
        source_abi: task.user_abi_kind(),
        vm: old_vm.clone(),
        fdtable: observed_fdtable,
        vfs_context: task_vfs_context(task),
        vfs_generation: task_vfs_context(task)
            .as_ref()
            .map_or(0, |context| context.generation()),
        exec_access: observed_exec_access,
        shared_signal: task.shared_signal(),
        signal_generation: task.shared_signal().actions_generation(),
    };
    if observed.source_abi != UserAbiKind::TomoriLinux {
        return Err(Errno::ENOEXEC);
    }

    let (path, file) = match request.path {
        ExecPath::User(path_user) => (
            copy_cstr_from_user(path_user, EXEC_PATH_MAX).map_err(|error| error.as_errno())?,
            None,
        ),
        ExecPath::Kernel(path) => (path, None),
        ExecPath::FileDescriptor(fd_raw) => {
            let fdtable = observed
                .fdtable
                .as_ref()
                .map(|entry| &entry.table)
                .ok_or(Errno::EBADF)?;
            let file = fdtable
                .get_file(vfs::fdtable::Fd::from_raw(fd_raw))
                .ok_or(Errno::EBADF)?;
            let vfs_context = observed.vfs_context.as_ref().ok_or(Errno::EBADF)?;
            let display_path =
                general::vfs::namespace_path(vfs_context, file.dentry(), file.mount())
                    .unwrap_or_else(|| alloc::format!("/proc/self/fd/{fd_raw}"));
            (display_path, Some(file))
        }
    };
    let mut used_bytes = path.len().checked_add(1).ok_or(Errno::EINVAL)?;
    let argv = collect_user_string_array(request.argv_user, &mut used_bytes)?;
    let envp = collect_user_string_array(request.envp_user, &mut used_bytes)?;

    let load_result = if let Some(file) = file {
        crate::user::load_user_image_from_file(task, file, &path, &argv, &envp)
    } else {
        crate::user::load_user_image_from_path(task, &path, &argv, &envp)
    };
    if let Some(vm) = old_vm.as_ref() {
        vm.activate();
    }
    let loaded = match load_result {
        Ok(loaded) => loaded,
        Err(error) => {
            if matches!(error, Errno::ENOEXEC | Errno::ENOENT) {
                log::debug!("[exec] load failed: path={:?} err={:?}", path, error);
            } else {
                log::info!("[exec] load failed: path={:?} err={:?}", path, error);
            }
            return Err(error);
        }
    };

    let prepared_fdtable = observed
        .fdtable
        .as_ref()
        .map(|entry| entry.table.fork_for_exec().map(Arc::new))
        .transpose()
        .map_err(|error| error.to_errno())?;
    let cleanup = ExecCleanupScratch::prepare()?;
    let kernel_stack_top = task.ensure_kernel_stack();
    let mut frame = UserTrapFrame::init_user(loaded.entry_pc, loaded.user_sp, 0);
    frame.set_kernel_stack_top(kernel_stack_top);
    let LoadedUserImage {
        vm,
        exec_path,
        exec_access,
        #[cfg(feature = "performance-profile")]
        main_image_range,
        #[cfg(feature = "performance-profile")]
        interpreter_image,
        ..
    } = loaded;
    let comm = prepare_comm(&exec_path);
    #[cfg(feature = "performance-profile")]
    let main_profile = (
        crate::sched::profile_image_id(&exec_path),
        main_image_range.start,
        main_image_range.end,
    );
    #[cfg(feature = "performance-profile")]
    let interpreter_profile = interpreter_image
        .as_ref()
        .map(|(path, range)| (crate::sched::profile_image_id(path), range.start, range.end))
        .unwrap_or((0, 0, 0));

    Ok(PreparedExec {
        image: PreparedImage {
            vm,
            exec_access,
            #[cfg(feature = "performance-profile")]
            main_profile,
            #[cfg(feature = "performance-profile")]
            interpreter_profile,
        },
        personality: PreparedPersonality {
            state: ProcessPersonalityState::TomoriLinux,
        },
        resources: PreparedResources {
            fdtable: prepared_fdtable,
            signal_actions: observed.shared_signal.prepare_actions_for_exec(),
        },
        startup: PreparedStartup {
            exec_path: Arc::new(exec_path),
            argv: Arc::new(argv),
            envp: Arc::new(envp),
            comm,
        },
        initial_thread: PreparedInitialThread {
            frame,
            kernel_stack_top,
        },
        cleanup,
        observed,
    })
}

fn replace_required_extension<T: core::any::Any + Send + Sync>(
    task: &Arc<Task>,
    key: sched::TaskExtKey,
    value: &Arc<T>,
) -> Result<(), Errno> {
    let erased: Arc<dyn core::any::Any + Send + Sync> = value.clone();
    task.ext_replace(key, erased)
        .map(|_| ())
        .map_err(|_| Errno::EIO)
}

fn terminate_after_ponr(task: &Arc<Task>, error: Errno) -> ! {
    log::emergency!(
        "[exec] post-PONR install failure: pid={:?} err={:?}",
        task.pid_root(),
        error
    );
    sched::operation::exit_group(127)
}

/// 重验并原子发布一个已经完成全部可失败工作的映像替换事务。
pub(crate) fn commit_exec(
    task: &Arc<Task>,
    mut prepared: PreparedExec,
    user_context: UserContextRef,
) -> Result<(), Errno> {
    if user_context.is_none() {
        return Err(Errno::EINVAL);
    }
    let group = task.thread_group();
    let mut guard = group.lock_exec();
    let _vfs_lease = prepared
        .observed
        .vfs_context
        .as_ref()
        .map(|context| context.lock_for_exec());
    revalidate_before_ponr(&mut guard, prepared.observed.exec_generation, || {
        prepared.observed.revalidate(task)
    })?;

    // 共享 FdTable 可由其它任务并发修改。代际匹配后把表锁持有到资源指针交换，
    // 使最后一次重验与发布之间没有 TOCTOU 窗口。
    let fdtable_source = prepared
        .observed
        .fdtable
        .as_ref()
        .map(|observed| (Arc::clone(&observed.table), observed.generation));
    let private_fdtable_source = prepared.observed.fdtable.as_ref().is_some_and(|observed| {
        // 此时强引用包括任务扩展、快照和下方的 source clone；其它 CLONE_FILES
        // 持有者会留下额外引用。
        Arc::strong_count(&observed.table) == 3
    });
    let mut fdtable_lease = match fdtable_source.as_ref() {
        Some((table, generation)) => Some(table.lock_generation(*generation).ok_or(Errno::EAGAIN)?),
        None => None,
    };
    let signal_actions_lease = prepared.observed.shared_signal.lock_actions_for_exec();
    if !signal_actions_lease.is_current(&prepared.resources.signal_actions) {
        return Err(Errno::EAGAIN);
    }
    match (fdtable_source.as_ref(), task_fdtable(task)) {
        (Some((observed, _)), Some(current)) if Arc::ptr_eq(observed, &current) => {}
        (None, None) => {}
        _ => return Err(Errno::EAGAIN),
    }

    // Transitioning 先冻结 signal consumer；旧地址空间清理仍在这里保持激活，
    // 完成后才进入不可回退的映像安装序列。
    guard.set_phase(ExecPhase::Transitioning);
    cleanup_task_for_exec(task, &mut prepared.cleanup);
    if prepared.cleanup.has_pi_handoff_overflow() {
        drop(fdtable_lease.take());
        drop(guard);
        terminate_after_ponr(task, Errno::ENOMEM);
    }

    // 这是最后一个可失败检查之后的发布点；从这里开始绝不返回旧映像。
    let result = drive_install_steps(&mut guard, |guard, step| {
        match step {
            InstallStep::FileDescriptors => {
                if let Some(fdtable) = prepared.resources.fdtable.as_ref() {
                    fdtable.activate_fd_references();
                    replace_required_extension(task, TASKEXT_VFS_FDTABLE, fdtable)?;
                }
                drop(fdtable_lease.take());
            }
            InstallStep::AddressSpace => {
                replace_required_extension(task, TASKEXT_VM_SPACE, &prepared.image.vm)?;
                prepared.image.vm.activate();
            }
            InstallStep::ExecutableAccess => {
                replace_required_extension(task, TASKEXT_EXEC_ACCESS, &prepared.image.exec_access)?;
            }
            InstallStep::ExecPath => {
                replace_required_extension(task, TASKEXT_EXEC_PATH, &prepared.startup.exec_path)?;
                task.set_comm(&prepared.startup.comm);
            }
            InstallStep::Arguments => {
                replace_required_extension(task, TASKEXT_EXEC_ARGS, &prepared.startup.argv)?;
            }
            InstallStep::Environment => {
                replace_required_extension(task, TASKEXT_EXEC_ENVP, &prepared.startup.envp)?;
                #[cfg(feature = "performance-profile")]
                task.set_profile_images(
                    prepared.image.main_profile,
                    prepared.image.interpreter_profile,
                );
            }
            InstallStep::Personality => {
                guard.install_personality(prepared.personality.state.clone());
            }
            InstallStep::InitialThread => {
                #[cfg(target_arch = "riscv64")]
                arch::riscv64::vector::clear_for_task(task);
                task.clear_rseq_registration();
                task.clear_sigaltstack();
                if !signal_actions_lease.install(&prepared.resources.signal_actions) {
                    return Err(Errno::EIO);
                }
                hal::user_context::set_kernel_trap_stack(prepared.initial_thread.kernel_stack_top);
            }
            InstallStep::UserContext => {
                prepared
                    .initial_thread
                    .frame
                    .apply_to_context(user_context.as_usize());
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        let _ = prepared.cleanup.apply_pi_handoffs();
        // exit_group 不返回，必须显式释放仍锁住源 fdtable 的租约；否则退出清理
        // 会再次获取同一把锁而永久自锁。
        drop(fdtable_lease.take());
        drop(guard);
        terminate_after_ponr(task, error);
    }
    if !prepared.cleanup.apply_pi_handoffs() {
        drop(fdtable_lease.take());
        drop(guard);
        terminate_after_ponr(task, Errno::ENOMEM);
    }
    if private_fdtable_source {
        if let Some((source, _)) = fdtable_source.as_ref() {
            source.suppress_drop_notifications_for_exec();
        }
    }
    Ok(())
}

#[cfg(feature = "kernel-tests")]
mod tests;
