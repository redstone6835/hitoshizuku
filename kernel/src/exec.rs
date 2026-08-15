//! 进程映像替换事务。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use errno::Errno;
use general::TaskOps;
use general::mm::{VmSpace, copy_cstr_bytes_from_user, copy_cstr_from_user, copy_from_user};
use general::vfs::{FdTable, VfsContext};
use hal::user_context::UserTrapFrame;
use native_abi::ExecPhase;
use native_abi::UserAbiKind;
use sched::group::{ProcessPersonalityState, ThreadGroupExecGuard};
use sched::process_ops::{ExecPath, ExecRequest, UserContextRef};
use sched::{
    PreparedSignalActions, SharedSignal, TASKEXT_EXEC_ACCESS, TASKEXT_EXEC_ARGS, TASKEXT_EXEC_ENVP,
    TASKEXT_EXEC_PATH, TASKEXT_VFS_CONTEXT, TASKEXT_VFS_FDTABLE, TASKEXT_VM_SPACE, Task,
};

use crate::syscalls::{ExecCleanupScratch, cleanup_task_for_exec};
use crate::user::{ExecutableAccessSet, LoadedExecutionImage, LoadedUserImage};

const EXEC_PATH_MAX: usize = 4096;
const EXEC_MAX_STRINGS: usize = 4096;
const EXEC_MAX_ARG_BYTES: usize = 128 * 1024;

/// 已完成格式解析、映射和权限封口的新映像。
pub(crate) struct PreparedImage {
    vm: Arc<VmSpace>,
    exec_access: Arc<ExecutableAccessSet>,
    /// exec 后的新凭据（setuid/setgid 位、能力转换；`None` = 不变）。
    exec_credentials: Option<Arc<sched::ids::Credentials>>,
    sync_icache: bool,
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
    detach_vfs: bool,
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

struct PreparedLoad {
    vm: Arc<VmSpace>,
    exec_access: Arc<ExecutableAccessSet>,
    exec_credentials: Option<Arc<sched::ids::Credentials>>,
    sync_icache: bool,
    personality: ProcessPersonalityState,
    fdtable: Option<Arc<FdTable>>,
    detach_vfs: bool,
    exec_path: String,
    argv: Vec<String>,
    envp: Vec<String>,
    frame: UserTrapFrame,
    #[cfg(feature = "performance-profile")]
    main_profile: (u64, usize, usize),
    #[cfg(feature = "performance-profile")]
    interpreter_profile: (u64, usize, usize),
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
    Credentials,
    ExecPath,
    Arguments,
    Environment,
    Personality,
    InitialThread,
    UserContext,
}

const INSTALL_STEPS: [InstallStep; 10] = [
    InstallStep::FileDescriptors,
    InstallStep::AddressSpace,
    InstallStep::ExecutableAccess,
    InstallStep::Credentials,
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

fn abort_exec_transition_before_ponr(
    guard: &mut ThreadGroupExecGuard<'_>,
    error: Errno,
) -> Result<(), Errno> {
    guard.set_phase(ExecPhase::Running);
    Err(error)
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
    Ok(())
}

/// 在全部安装步骤与 PI handoff 完成后发布新一代映像。
fn finish_exec_commit(
    guard: &mut ThreadGroupExecGuard<'_>,
    handoffs_applied: bool,
) -> Result<(), Errno> {
    if !handoffs_applied {
        guard.set_phase(ExecPhase::Terminating);
        return Err(Errno::ENOMEM);
    }
    guard.advance_generation();
    guard.set_phase(ExecPhase::Running);
    Ok(())
}

fn release_before_diverging<T, R>(resources: T, next: impl FnOnce() -> R) -> R {
    drop(resources);
    next()
}

struct PlannedExecThreadTransition {
    siblings: Vec<Arc<Task>>,
    replaced_leader: Option<Arc<Task>>,
}

fn plan_exec_thread_transition(
    guard: &ThreadGroupExecGuard<'_>,
    task: &Arc<Task>,
    source_abi: UserAbiKind,
    target_abi: UserAbiKind,
) -> Result<PlannedExecThreadTransition, Errno> {
    if task.thread_group().group_exit_status().is_some() {
        return Err(Errno::EBUSY);
    }
    if source_abi != UserAbiKind::TomoriLinux || target_abi != UserAbiKind::TomoriLinux {
        if !guard.has_only_member(task) {
            return Err(Errno::EBUSY);
        }
        return Ok(PlannedExecThreadTransition {
            siblings: Vec::new(),
            replaced_leader: None,
        });
    }

    let mut siblings = guard.try_member_snapshot().map_err(|_| Errno::ENOMEM)?;
    if !siblings.iter().any(|member| Arc::ptr_eq(member, task)) {
        return Err(Errno::EAGAIN);
    }
    siblings.retain(|member| !Arc::ptr_eq(member, task));
    let leader = task.thread_group().leader().ok_or(Errno::EAGAIN)?;
    let replaced_leader = if Arc::ptr_eq(&leader, task) {
        None
    } else if siblings.iter().any(|sibling| Arc::ptr_eq(sibling, &leader)) {
        Some(leader)
    } else {
        return Err(Errno::EAGAIN);
    };
    if let Some(old_leader) = replaced_leader.as_ref() {
        sched::operation::prepare_exec_leader_identity(task, old_leader)?;
    }
    Ok(PlannedExecThreadTransition {
        siblings,
        replaced_leader,
    })
}

fn complete_exec_thread_transition(
    task: &Arc<Task>,
    transition: PlannedExecThreadTransition,
) -> Result<(), Errno> {
    for sibling in transition.siblings.iter() {
        let preserve_leader_identity = transition
            .replaced_leader
            .as_ref()
            .is_some_and(|leader| Arc::ptr_eq(leader, sibling));
        sched::operation::request_exec_sibling_exit(sibling, preserve_leader_identity);
    }
    for sibling in transition.siblings.iter() {
        let preserve_leader_identity = transition
            .replaced_leader
            .as_ref()
            .is_some_and(|leader| Arc::ptr_eq(leader, sibling));
        sibling.exit_waiters.wait_event(task, || {
            matches!(
                sibling.state(),
                sched::TaskState::Zombie | sched::TaskState::Dead
            ) && sibling.running_cpu().is_none()
                && sibling.exit_extensions_cleanup_complete()
        });
        if sibling.state() == sched::TaskState::Zombie {
            let _ = sched::operation::complete_exec_sibling_exit_if_requested(sibling);
        }
        let expected = if preserve_leader_identity {
            sched::TaskState::Zombie
        } else {
            sched::TaskState::Dead
        };
        if sibling.state() != expected {
            return Err(Errno::EAGAIN);
        }
    }
    if let Some(old_leader) = transition.replaced_leader.as_ref() {
        sched::operation::adopt_exec_leader_identity(task, old_leader)?;
    }
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

fn copy_user_cstring_bytes(user: usize, max: usize) -> Result<Vec<u8>, Errno> {
    copy_cstr_bytes_from_user(user, max).map_err(|error| error.as_errno())
}

fn collect_user_byte_string_array(
    table_user: usize,
    used_bytes: &mut usize,
) -> Result<Vec<Vec<u8>>, Errno> {
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
        let value = copy_user_cstring_bytes(string_user, remaining)?;
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

pub(crate) fn prepare_native_initial_frame(
    entry_pc: usize,
    user_sp: usize,
    start_info_address: usize,
    start_info_size: usize,
    image_base: usize,
    tls_base: usize,
    bootstrap_process: u64,
    kernel_stack_top: usize,
) -> UserTrapFrame {
    let mut frame = UserTrapFrame::init_user(entry_pc, user_sp, start_info_address);
    frame.set_args(start_info_address, start_info_size, image_base);
    frame.set_arg3(usize::try_from(bootstrap_process).unwrap_or(0));
    frame.set_tls(tls_base);
    frame.set_ra(0);
    frame.set_kernel_stack_top(kernel_stack_top);
    frame
}

/// 提交一个已经完全准备好的 Native 到 Native 映像替换。
pub(crate) fn commit_native_replace(
    task: &Arc<Task>,
    image: crate::soyo::PreparedSoyoImage,
    user_context: UserContextRef,
) -> Result<(), Errno> {
    if user_context.is_none() || task.user_abi_kind() != UserAbiKind::MygoNative {
        return Err(Errno::EINVAL);
    }
    let old_vm = task_vm_space(task).ok_or(Errno::EAGAIN)?;
    let kernel_stack_top = task.ensure_kernel_stack();
    let mut frame = prepare_native_initial_frame(
        image.entry_pc,
        image.user_sp,
        image.start_info_address,
        image.start_info_size,
        image.image_base,
        image.tls_base,
        image.bootstrap_process,
        kernel_stack_top,
    );

    let group = task.thread_group();
    let mut guard = group.lock_exec();
    if guard.phase() != ExecPhase::Running
        || !guard.has_only_member(task)
        || group.group_exit_status().is_some()
    {
        return Err(Errno::EBUSY);
    }
    guard.set_phase(ExecPhase::Transitioning);

    let erased_vm: Arc<dyn core::any::Any + Send + Sync> = Arc::clone(&image.vm) as Arc<_>;
    if task.ext_replace(TASKEXT_VM_SPACE, erased_vm).is_err() {
        guard.set_phase(ExecPhase::Running);
        return Err(Errno::EAGAIN);
    }
    reset_signal_state_for_exec(task, UserAbiKind::MygoNative);
    let personality: Arc<dyn core::any::Any + Send + Sync> = image.personality;
    guard.install_personality(ProcessPersonalityState::MygoNative(personality));
    image.vm.activate();
    frame.set_current_address_space();
    unsafe {
        *(user_context.as_usize() as *mut UserTrapFrame) = frame;
    }
    guard.advance_generation();
    guard.set_phase(ExecPhase::Running);
    drop(guard);
    drop(group);
    drop(old_vm);
    Ok(())
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
    let argv = collect_user_byte_string_array(request.argv_user, &mut used_bytes)?;
    let envp = collect_user_byte_string_array(request.envp_user, &mut used_bytes)?;

    let load_result = if let Some(file) = file {
        crate::user::load_execution_image_from_file(task, file, &path, argv, envp)
    } else {
        crate::user::load_execution_image_from_path(task, &path, argv, envp)
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

    let kernel_stack_top = task.ensure_kernel_stack();
    let loaded = match loaded {
        LoadedExecutionImage::Tomori { image, argv, envp, file_owner } => {
            let prepared_fdtable = observed
                .fdtable
                .as_ref()
                .map(|entry| entry.table.fork_for_exec().map(Arc::new))
                .transpose()
                .map_err(|error| error.to_errno())?;
            let mut frame = UserTrapFrame::init_user(image.entry_pc, image.user_sp, 0);
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
            } = image;
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
            let exec_credentials = compute_exec_credentials(task, file_owner);
            PreparedLoad {
                vm,
                exec_access,
                exec_credentials,
                sync_icache: false,
                personality: ProcessPersonalityState::TomoriLinux,
                fdtable: prepared_fdtable,
                detach_vfs: false,
                exec_path,
                argv,
                envp,
                frame,
                #[cfg(feature = "performance-profile")]
                main_profile,
                #[cfg(feature = "performance-profile")]
                interpreter_profile,
            }
        }
        LoadedExecutionImage::MygoNative {
            image,
            exec_path,
            exec_access,
            argv,
            envp,
        } => {
            let descriptors = observed
                .fdtable
                .as_ref()
                .map(|entry| {
                    let snapshot = entry
                        .table
                        .snapshot_descriptors()
                        .map_err(|error| error.to_errno())?;
                    if snapshot.generation() != entry.generation {
                        return Err(Errno::EAGAIN);
                    }
                    Ok(snapshot)
                })
                .transpose()?;
            let image = crate::soyo::prepare_soyo_runtime_with_vfs(
                image,
                &argv,
                &envp,
                descriptors.as_ref(),
                &[],
                observed.vfs_context.clone(),
            )?;
            let frame = prepare_native_initial_frame(
                image.entry_pc,
                image.user_sp,
                image.start_info_address,
                image.start_info_size,
                image.image_base,
                image.tls_base,
                image.bootstrap_process,
                kernel_stack_top,
            );
            #[cfg(feature = "performance-profile")]
            let main_profile = (
                crate::sched::profile_image_id(&exec_path),
                image.image_base,
                image.image_end,
            );
            let personality: Arc<dyn core::any::Any + Send + Sync> = image.personality;
            PreparedLoad {
                vm: image.vm,
                exec_access,
                exec_credentials: None,
                sync_icache: true,
                personality: ProcessPersonalityState::MygoNative(personality),
                fdtable: None,
                detach_vfs: true,
                exec_path,
                argv: Vec::new(),
                envp: Vec::new(),
                frame,
                #[cfg(feature = "performance-profile")]
                main_profile,
                #[cfg(feature = "performance-profile")]
                interpreter_profile: (0, 0, 0),
            }
        }
    };
    let cleanup = ExecCleanupScratch::prepare()?;
    let comm = prepare_comm(&loaded.exec_path);

    Ok(PreparedExec {
        image: PreparedImage {
            vm: loaded.vm,
            exec_access: loaded.exec_access,
            exec_credentials: loaded.exec_credentials,
            sync_icache: loaded.sync_icache,
            #[cfg(feature = "performance-profile")]
            main_profile: loaded.main_profile,
            #[cfg(feature = "performance-profile")]
            interpreter_profile: loaded.interpreter_profile,
        },
        personality: PreparedPersonality {
            state: loaded.personality,
        },
        resources: PreparedResources {
            fdtable: loaded.fdtable,
            detach_vfs: loaded.detach_vfs,
            signal_actions: observed.shared_signal.prepare_actions_for_exec(),
        },
        startup: PreparedStartup {
            exec_path: Arc::new(loaded.exec_path),
            argv: Arc::new(loaded.argv),
            envp: Arc::new(loaded.envp),
            comm,
        },
        initial_thread: PreparedInitialThread {
            frame: loaded.frame,
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

fn reset_signal_state_for_exec(task: &Arc<Task>, target_abi: UserAbiKind) {
    if target_abi == UserAbiKind::MygoNative {
        task.signal.reset_for_native_exec();
    }
}

fn terminate_after_ponr(task: &Arc<Task>, error: Errno) -> ! {
    log::emergency!(
        "[exec] post-PONR install failure: pid={:?} err={:?}",
        task.pid_root(),
        error
    );
    sched::operation::exit_group(127)
}

fn terminate_commit_after_ponr(
    task: &Arc<Task>,
    error: Errno,
    prepared: PreparedExec,
    fdtable_source: Option<(Arc<FdTable>, u64)>,
) -> ! {
    release_before_diverging((prepared, fdtable_source), || {
        terminate_after_ponr(task, error)
    })
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
    revalidate_before_ponr(&mut guard, prepared.observed.exec_generation, || {
        prepared.observed.revalidate(task)
    })?;

    let target_abi = prepared.personality.state.user_abi_kind();
    let thread_transition =
        plan_exec_thread_transition(&guard, task, prepared.observed.source_abi, target_abi)?;
    let dethreaded = !thread_transition.siblings.is_empty();

    // owner 判断必须在 Transitioning 和兄弟线程退出之前完成。此时 exec guard
    // 仍阻止本线程组 clone；若任务快照分配失败，旧映像仍可安全返回 ENOMEM。
    let private_fdtable_source = match prepared.observed.fdtable.as_ref() {
        Some(observed) => {
            !crate::syscalls::try_fdtable_has_other_live_owner(task, &observed.table)?
        }
        None => false,
    };
    if target_abi == UserAbiKind::MygoNative
        && prepared.observed.fdtable.is_some()
        && !private_fdtable_source
    {
        return Err(Errno::EBUSY);
    }

    // Transitioning 阻止新线程加入并冻结 signal consumer。等待兄弟线程时不能
    // 持有 exec/VFS/signal/FdTable 锁，否则目标线程的退出清理可能永久阻塞。
    guard.set_phase(ExecPhase::Transitioning);
    drop(guard);
    if let Err(error) = complete_exec_thread_transition(task, thread_transition) {
        let mut failure_guard = group.lock_exec();
        failure_guard.set_phase(ExecPhase::Terminating);
        drop(failure_guard);
        drop(group);
        terminate_commit_after_ponr(task, error, prepared, None);
    }

    let mut guard = group.lock_exec();
    if group.group_exit_status().is_some() {
        guard.set_phase(ExecPhase::Terminating);
        drop(guard);
        drop(group);
        terminate_commit_after_ponr(task, Errno::EBUSY, prepared, None);
    }
    let only_member = guard.has_only_member(task);
    if guard.phase() != ExecPhase::Transitioning
        || guard.generation() != prepared.observed.exec_generation
        || !only_member
    {
        if !dethreaded && guard.phase() == ExecPhase::Transitioning && only_member {
            return abort_exec_transition_before_ponr(&mut guard, Errno::EAGAIN);
        }
        guard.set_phase(ExecPhase::Terminating);
        drop(guard);
        drop(group);
        terminate_commit_after_ponr(task, Errno::EAGAIN, prepared, None);
    }

    let vfs_source = prepared.observed.vfs_context.clone();
    let vfs_lease = vfs_source.as_ref().map(|context| context.lock_for_exec());
    if let Err(error) = prepared.observed.revalidate(task) {
        if !dethreaded {
            return abort_exec_transition_before_ponr(&mut guard, error);
        }
        guard.set_phase(ExecPhase::Terminating);
        drop(vfs_lease);
        drop(guard);
        drop(vfs_source);
        drop(group);
        terminate_commit_after_ponr(task, error, prepared, None);
    }

    // 共享 FdTable 可由其它任务并发修改。代际匹配后把表锁持有到资源指针交换，
    // 使最后一次重验与发布之间没有 TOCTOU 窗口。
    let fdtable_lease_source = prepared
        .observed
        .fdtable
        .as_ref()
        .map(|observed| (Arc::clone(&observed.table), observed.generation));
    let fdtable_source = fdtable_lease_source.clone();
    let mut fdtable_lease = fdtable_lease_source
        .as_ref()
        .and_then(|(table, generation)| table.lock_generation(*generation));
    if fdtable_lease_source.is_some() && fdtable_lease.is_none() {
        if !dethreaded {
            return abort_exec_transition_before_ponr(&mut guard, Errno::EAGAIN);
        }
        guard.set_phase(ExecPhase::Terminating);
        drop(fdtable_lease);
        drop(vfs_lease);
        drop(guard);
        drop(vfs_source);
        drop(fdtable_lease_source);
        drop(group);
        terminate_commit_after_ponr(task, Errno::EAGAIN, prepared, fdtable_source);
    }
    let signal_source = Arc::clone(&prepared.observed.shared_signal);
    let signal_actions_lease = signal_source.lock_actions_for_exec();
    if !signal_actions_lease.is_current(&prepared.resources.signal_actions) {
        if !dethreaded {
            return abort_exec_transition_before_ponr(&mut guard, Errno::EAGAIN);
        }
        guard.set_phase(ExecPhase::Terminating);
        drop(fdtable_lease);
        drop(signal_actions_lease);
        drop(vfs_lease);
        drop(guard);
        drop(signal_source);
        drop(vfs_source);
        drop(fdtable_lease_source);
        drop(group);
        terminate_commit_after_ponr(task, Errno::EAGAIN, prepared, fdtable_source);
    }
    match (fdtable_source.as_ref(), task_fdtable(task)) {
        (Some((observed, _)), Some(current)) if Arc::ptr_eq(observed, &current) => {}
        (None, None) => {}
        _ => {
            if !dethreaded {
                return abort_exec_transition_before_ponr(&mut guard, Errno::EAGAIN);
            }
            guard.set_phase(ExecPhase::Terminating);
            drop(fdtable_lease);
            drop(signal_actions_lease);
            drop(vfs_lease);
            drop(guard);
            drop(signal_source);
            drop(vfs_source);
            drop(fdtable_lease_source);
            drop(group);
            terminate_commit_after_ponr(task, Errno::EAGAIN, prepared, fdtable_source);
        }
    }

    // 旧地址空间清理仍在这里保持激活，完成后才进入映像安装序列。
    cleanup_task_for_exec(task, &mut prepared.cleanup);
    if prepared.cleanup.has_pi_handoff_overflow() {
        guard.set_phase(ExecPhase::Terminating);
        drop(fdtable_lease);
        drop(signal_actions_lease);
        drop(vfs_lease);
        drop(guard);
        drop(signal_source);
        drop(vfs_source);
        drop(fdtable_lease_source);
        drop(group);
        terminate_commit_after_ponr(task, Errno::ENOMEM, prepared, fdtable_source);
    }

    // 这是最后一个可失败检查之后的发布点；从这里开始绝不返回旧映像。
    let result = drive_install_steps(&mut guard, |guard, step| {
        match step {
            InstallStep::FileDescriptors => {
                if let Some(fdtable) = prepared.resources.fdtable.as_ref() {
                    fdtable.activate_fd_references();
                    replace_required_extension(task, TASKEXT_VFS_FDTABLE, fdtable)?;
                } else if prepared.resources.detach_vfs {
                    let _ = task.ext_remove(TASKEXT_VFS_FDTABLE);
                    let _ = task.ext_remove(TASKEXT_VFS_CONTEXT);
                }
                drop(fdtable_lease.take());
            }
            InstallStep::AddressSpace => {
                replace_required_extension(task, TASKEXT_VM_SPACE, &prepared.image.vm)?;
                prepared.image.vm.activate();
                // frame 在 pure prepare 中构造，此时旧地址空间仍保持激活；提交新 VM
                // 后必须刷新架构地址空间令牌，避免返回路径重新装回旧页表。
                prepared.initial_thread.frame.set_current_address_space();
                if prepared.image.sync_icache {
                    arch::CurrentTaskOps::sync_icache();
                }
            }
            InstallStep::ExecutableAccess => {
                replace_required_extension(task, TASKEXT_EXEC_ACCESS, &prepared.image.exec_access)?;
            }
            InstallStep::Credentials => {
                if let Some(credentials) = prepared.image.exec_credentials.as_ref() {
                    install_exec_credentials(task, Arc::clone(credentials))?;
                }
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
                reset_signal_state_for_exec(task, target_abi);
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
        drop(fdtable_lease);
        drop(signal_actions_lease);
        drop(vfs_lease);
        drop(guard);
        drop(signal_source);
        drop(vfs_source);
        drop(fdtable_lease_source);
        drop(group);
        terminate_commit_after_ponr(task, error, prepared, fdtable_source);
    }
    if let Err(error) = finish_exec_commit(&mut guard, prepared.cleanup.apply_pi_handoffs()) {
        drop(fdtable_lease);
        drop(signal_actions_lease);
        drop(vfs_lease);
        drop(guard);
        drop(signal_source);
        drop(vfs_source);
        drop(fdtable_lease_source);
        drop(group);
        terminate_commit_after_ponr(task, error, prepared, fdtable_source);
    }
    if private_fdtable_source && prepared.resources.fdtable.is_some() {
        if let Some((source, _)) = fdtable_source.as_ref() {
            source.suppress_drop_notifications_for_exec();
        }
    }
    ptrace_notify_exec(task);
    Ok(())
}

/// `PTRACE_O_TRACEEXEC`：exec 完成事件（消息为 0）。
fn ptrace_notify_exec(task: &Arc<Task>) {
    const PTRACE_O_TRACEEXEC: u64 = 0x0000_0010;
    const PTRACE_EVENT_EXEC: u16 = 4;
    if !task.is_ptrace_traced() || task.ptrace_options() & PTRACE_O_TRACEEXEC == 0 {
        return;
    }
    task.set_ptrace_event_msg(0);
    task.set_ptrace_stop_event(PTRACE_EVENT_EXEC);
    task.clear_ptrace_last_siginfo();
    sched::operation::ptrace_mark_stopped(task, sched::SignalNumber::SIGTRAP);
}

/// Linux `commit_creds` 的 exec 凭据语义（无文件能力时）。
///
/// - `PR_SET_NO_NEW_PRIVS`：完全跳过权限提升；
/// - `SECBIT_NO_SETUID_FIXUP`：跳过 setuid/setgid 位；
/// - `S_ISUID`/`S_ISGID`：euid/egid 切换为文件属主；euid 变化时 suid 同步、
///   `dumpable = 0`；
/// - 能力转换（`prepare_kernel_cred`/`bprm` 公式，`fP=fI=fE=0`）：
///   `pP' = bset & (pI | pP)`，`pE' = pE & pP'`；setuid 生效（secureexec）
///   且未设 `SECBIT_KEEP_CAPS` 时 `pP' = bset & pI`（丢弃原有 permitted）；
/// - `PR_SET_KEEPCAPS` 与 `SECBIT_KEEP_CAPS` 等价。
fn compute_exec_credentials(
    task: &Arc<Task>,
    file_owner: Option<(u32, u32, u16)>,
) -> Option<Arc<sched::ids::Credentials>> {
    use sched::ids::{CapSet, Gid, Uid};

    const SECBIT_KEEP_CAPS: u32 = 1 << 0;
    const SECBIT_NO_SETUID_FIXUP: u32 = 1 << 2;
    const S_ISUID: u16 = 0o4000;
    const S_ISGID: u16 = 0o2000;

    let old = task.credentials();
    if task.no_new_privs() {
        return None;
    }
    let securebits = old.securebits;
    let mut new = (*old).clone();

    let mut secureexec = false;
    if securebits & SECBIT_NO_SETUID_FIXUP == 0 {
        if let Some((file_uid, file_gid, mode)) = file_owner {
            if mode & S_ISUID != 0 {
                new.euid = Uid(file_uid);
                secureexec = true;
            }
            if mode & S_ISGID != 0 {
                new.egid = Gid(file_gid);
                secureexec = true;
            }
        }
    }
    if new.euid != old.euid {
        new.suid = new.euid;
        task.set_dumpable(0);
        secureexec = true;
    }
    if new.egid != old.egid {
        new.fsgid = new.egid;
        secureexec = true;
    }
    if !secureexec
        && new.uid == old.uid
        && new.euid == old.euid
        && new.suid == old.suid
        && new.fsuid == old.fsuid
        && new.gid == old.gid
        && new.egid == old.egid
        && new.sgid == old.sgid
        && new.fsgid == old.fsgid
        && new.caps.raw() == old.caps.raw()
    {
        return None;
    }

    // 能力转换。
    let bset = old.cap_bset;
    let inherited = old.cap_inheritable;
    let old_permitted = old.cap_permitted;
    let effective = old.caps;
    let keep_caps = task.keepcaps() || securebits & SECBIT_KEEP_CAPS != 0;
    let inherited_or_permitted = CapSet::from_raw(inherited.raw() | old_permitted.raw());
    let new_permitted = if secureexec && !keep_caps {
        bset.mask(inherited)
    } else {
        bset.mask(inherited_or_permitted)
    };
    let new_effective = effective.mask(new_permitted);
    new.cap_permitted = new_permitted;
    new.caps = new_effective;
    // 无 ambient 能力；inheritable 保持不变（Linux pI' = pI）。

    Some(Arc::new(new))
}

/// 安装 exec 凭据：sched 凭据 + VFS 上下文凭据原子替换。
fn install_exec_credentials(
    task: &Arc<Task>,
    credentials: Arc<sched::ids::Credentials>,
) -> Result<(), Errno> {
    task.set_credentials(Arc::clone(&credentials));
    if let Some(vfs_ctx) = general::vfs::current_vfs_context() {
        vfs_ctx.set_cred(Arc::new(crate::syscalls::vfs_cred_from_sched(&credentials)));
    }
    let _ = task;
    Ok(())
}

#[cfg(feature = "kernel-tests")]
mod tests;
