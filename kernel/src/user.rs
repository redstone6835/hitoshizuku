//! 用户 ELF 装载：从 VFS 路径读取 ELF、构建 VmSpace、布用户栈。
//!
//! 主要入口 [`load_user_image_from_path`] 供 ProcessImageOps::execve 调用。
//!
//! 流程：
//! - 解析 ELF；
//! - 建 VmSpace，对主程序 PT_LOAD 段注册 file-backed 按需映射；
//! - 预分配用户栈并布 argc/argv/envp/auxv；
//! - 返回 LoadedUserImage（vm + entry_pc + user_sp）。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use general::mm::{VmSpace, user_pgd_ops};
use general::vfs::{
    self, FdTable, FileMode, VfsContext,
    file::{AccessMode, File, OpenOptions},
    inode::InodeExecAccess,
    path::{Dirfd, LookupFlags},
};
use mm::VmFlags;
use sched::Task;

use elf::{ElfReadAt, ElfReadError, ElfReadLimits, Image, LinuxElfMetadata, read_linux_elf};

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_PAGESZ: usize = 6;
const AT_HWCAP: usize = 16;
const AT_BASE: usize = 7;
const AT_ENTRY: usize = 9;
const AT_UID: usize = 11;
const AT_EUID: usize = 12;
const AT_GID: usize = 13;
const AT_EGID: usize = 14;
const AT_CLKTCK: usize = 17;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;
const AT_EXECFN: usize = 31;
const AT_SYSINFO_EHDR: usize = 33;
const MAX_SHEBANG_DEPTH: usize = 4;

const ELF_PREFIX_READ_SIZE: usize = 4096;

pub struct LoadedUserImage {
    pub vm: Arc<VmSpace>,
    pub entry_pc: usize,
    pub user_sp: usize,
    pub exec_path: String,
    #[cfg(feature = "performance-profile")]
    pub main_image_range: core::ops::Range<usize>,
    #[cfg(feature = "performance-profile")]
    pub interpreter_image: Option<(String, core::ops::Range<usize>)>,
    pub exec_access: Arc<ExecutableAccessSet>,
}

/// exec 探测完成后交给事务层的映像类型。
pub(crate) enum LoadedExecutionImage {
    Tomori {
        image: LoadedUserImage,
        argv: Vec<String>,
        envp: Vec<String>,
    },
    MygoNative {
        image: crate::soyo::LoadedSoyoImage,
        exec_path: String,
        exec_access: Arc<ExecutableAccessSet>,
        argv: Vec<Vec<u8>>,
        envp: Vec<Vec<u8>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutableFormat {
    Soyo,
    Elf,
    Script,
    Unknown,
}

pub(crate) fn detect_executable_format(prefix: &[u8]) -> ExecutableFormat {
    if prefix.starts_with(&soyo::registry::SOYO_MAGIC) {
        ExecutableFormat::Soyo
    } else if prefix.starts_with(b"\x7fELF") {
        ExecutableFormat::Elf
    } else if prefix.starts_with(b"#!") {
        ExecutableFormat::Script
    } else {
        ExecutableFormat::Unknown
    }
}

/// 一次成功执行映像持有的全部 inode 执行租约。
///
/// 主程序和内核装载的动态解释器都保存在同一个集合中。任务 fork 时共享该集合，
/// exec 替换或最后一个相关任务退出时集合析构，从而精确维持 ETXTBSY 生命周期。
pub struct ExecutableAccessSet {
    leases: Vec<InodeExecAccess>,
}

struct LoadedInterpreter {
    bytes: Vec<u8>,
    access: InodeExecAccess,
}

struct PreparedExecutableFile {
    file: Arc<File>,
    prefix: Vec<u8>,
    access: InodeExecAccess,
}

struct LoadedImage {
    entry: usize,
    base: usize,
    end: usize,
    phdr: usize,
    phent: usize,
    phnum: usize,
}

struct VfsElfReader {
    file: Arc<File>,
    size: u64,
}

impl ElfReadAt for VfsElfReader {
    type Error = errno::Errno;

    fn len(&self) -> u64 {
        self.size
    }

    fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), Self::Error> {
        read_exact_file(&self.file, offset, dst)
    }
}

pub fn load_user_image_from_path(
    task: &Arc<Task>,
    path: &str,
    argv: &[String],
    envp: &[String],
) -> Result<LoadedUserImage, errno::Errno> {
    load_user_image_from_path_inner(task, path, argv, envp, 0)
}

pub fn load_user_image_from_file(
    task: &Arc<Task>,
    file: Arc<File>,
    exec_path: &str,
    argv: &[String],
    envp: &[String],
) -> Result<LoadedUserImage, errno::Errno> {
    load_user_image_from_file_inner(task, file, exec_path, argv, envp, 0)
}

pub(crate) fn load_execution_image_from_path(
    task: &Arc<Task>,
    path: &str,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Result<LoadedExecutionImage, errno::Errno> {
    let file = open_file_from_task_vfs(task, path)?;
    load_execution_image_from_file(task, file, path, argv, envp)
}

pub(crate) fn load_execution_image_from_file(
    task: &Arc<Task>,
    file: Arc<File>,
    exec_path: &str,
    argv: Vec<Vec<u8>>,
    envp: Vec<Vec<u8>>,
) -> Result<LoadedExecutionImage, errno::Errno> {
    let prepared = prepare_executable_file(task, file)?;
    match detect_executable_format(&prepared.prefix) {
        ExecutableFormat::Soyo => {
            let image = crate::soyo::load_soyo_image_from_file(Arc::clone(&prepared.file))?;
            return Ok(LoadedExecutionImage::MygoNative {
                image,
                exec_path: String::from(exec_path),
                exec_access: Arc::new(ExecutableAccessSet {
                    leases: alloc::vec![prepared.access],
                }),
                argv,
                envp,
            });
        }
        ExecutableFormat::Elf | ExecutableFormat::Script => {}
        ExecutableFormat::Unknown => return Err(errno::Errno::ENOEXEC),
    }

    let argv = byte_strings_to_text(argv)?;
    let envp = byte_strings_to_text(envp)?;
    let image = load_tomori_image_from_prepared(task, prepared, exec_path, &argv, &envp, 0)?;
    Ok(LoadedExecutionImage::Tomori { image, argv, envp })
}

fn load_user_image_from_path_inner(
    task: &Arc<Task>,
    path: &str,
    argv: &[String],
    envp: &[String],
    shebang_depth: usize,
) -> Result<LoadedUserImage, errno::Errno> {
    let file = match open_file_from_task_vfs(task, path) {
        Ok(file) => file,
        Err(e) => {
            log::debug!("[user] load path={:?} open failed: {:?}", path, e);
            return Err(e);
        }
    };
    load_user_image_from_file_inner(task, file, path, argv, envp, shebang_depth)
}

fn load_user_image_from_file_inner(
    task: &Arc<Task>,
    file: Arc<File>,
    path: &str,
    argv: &[String],
    envp: &[String],
    shebang_depth: usize,
) -> Result<LoadedUserImage, errno::Errno> {
    let prepared = prepare_executable_file(task, file)?;
    load_tomori_image_from_prepared(task, prepared, path, argv, envp, shebang_depth)
}

fn prepare_executable_file(
    task: &Arc<Task>,
    file: Arc<File>,
) -> Result<PreparedExecutableFile, errno::Errno> {
    check_exec_permission(task, &file)?;
    // fanotify：FAN_OPEN_EXEC（通知）与 FAN_OPEN_EXEC_PERM（权限，exec 前裁决）。
    if vfs::fsnotify::perm_enabled() {
        vfs::fsnotify::emit_perm_at(
            file.inode(),
            Some(file.mount()),
            vfs::fsnotify::FAN_OPEN_EXEC_PERM,
        )
        .map_deny()
        .map_err(|e| e.to_errno())?;
    }
    if vfs::fsnotify::is_enabled() {
        vfs::fsnotify::emit_at_with_parents(
            file.inode(),
            Some(file.dentry()),
            Some(file.mount()),
            vfs::fsnotify::FAN_OPEN_EXEC,
            0,
        );
    }
    let access = file
        .inode()
        .acquire_exec_access()
        .map_err(|error| error.to_errno())?;
    let file = if file.flags().readable() {
        file
    } else {
        let cred = task_vfs_context(task)?.cred();
        file.open_exec_view(cred)
            .map_err(|error| error.to_errno())?
    };
    let prefix = load_elf_prefix_from_file(&file)?;
    Ok(PreparedExecutableFile {
        file,
        prefix,
        access,
    })
}

fn load_tomori_image_from_prepared(
    task: &Arc<Task>,
    prepared: PreparedExecutableFile,
    path: &str,
    argv: &[String],
    envp: &[String],
    shebang_depth: usize,
) -> Result<LoadedUserImage, errno::Errno> {
    let PreparedExecutableFile {
        file,
        prefix,
        access: main_exec_access,
    } = prepared;
    if prefix.starts_with(b"#!") {
        let script = parse_shebang(path, argv, &prefix, shebang_depth)?;
        let mut loaded = load_user_image_from_path_inner(
            task,
            &script.interpreter,
            &script.argv,
            envp,
            shebang_depth + 1,
        )?;
        Arc::get_mut(&mut loaded.exec_access)
            .ok_or(errno::Errno::EIO)?
            .leases
            .push(main_exec_access);
        return Ok(loaded);
    }
    if !prefix.starts_with(b"\x7fELF") {
        return Err(errno::Errno::ENOEXEC);
    }

    let exec_image = match load_exec_image_from_file(&file) {
        Ok(image) => image,
        Err(e) => {
            log::debug!("[user] elf metadata parse failed for {:?}: {:?}", path, e);
            if prefix.len() >= 16 {
                log::debug!(
                    "[user]   first 16 bytes: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
                    prefix[0],
                    prefix[1],
                    prefix[2],
                    prefix[3],
                    prefix[4],
                    prefix[5],
                    prefix[6],
                    prefix[7],
                    prefix[8],
                    prefix[9],
                    prefix[10],
                    prefix[11],
                    prefix[12],
                    prefix[13],
                    prefix[14],
                    prefix[15],
                );
            }
            return Err(e);
        }
    };
    if validate_exec_image_result(&exec_image).is_err() {
        return Err(errno::Errno::ENOEXEC);
    }

    let vm = Arc::new(VmSpace::new());
    let main_bias = if exec_image.is_pie() {
        hal::user::main_pie_base()
    } else {
        0
    };
    let main_loaded = load_exec_image(&vm, &exec_image, &file, main_bias, true, "exec")?;
    let exec_path = resolve_exec_path(task, path);
    let mut exec_access = Vec::new();
    exec_access.push(main_exec_access);

    let interpreter_path = exec_image.interpreter().map(String::from);
    let interp_loaded = if let Some(interp) = interpreter_path.as_deref() {
        match load_interpreter_from_task_vfs(task, &exec_path, interp) {
            Ok(loaded_interpreter) => {
                let LoadedInterpreter { mut bytes, access } = loaded_interpreter;
                hal::user::patch_interpreter_image(interp, &mut bytes);
                let interp_img = elf::parse(&bytes).map_err(|_| errno::Errno::ENOEXEC)?;
                validate_user_image_result(&*interp_img).map_err(|_| errno::Errno::ENOEXEC)?;
                let loaded = load_image(&vm, &*interp_img, hal::user::interp_base(), "interp")?;
                exec_access.push(access);
                Some(loaded)
            }
            Err(errno::Errno::ENOENT) if exec_image.can_run_without_interpreter() => None,
            Err(err) => return Err(err),
        }
    } else {
        None
    };
    let entry_pc = interp_loaded
        .as_ref()
        .map(|interp| interp.entry)
        .unwrap_or(main_loaded.entry);
    let at_base = interp_loaded
        .as_ref()
        .map(|interp| interp.base)
        .unwrap_or(0);

    let stack_top = hal::user::default_stack_top();
    let stack_size = hal::user::default_stack_size();
    let stack_bottom = stack_top
        .checked_sub(stack_size)
        .ok_or(errno::Errno::ENOMEM)?;
    let stack_flags = VmFlags::EMPTY
        .with(VmFlags::READ)
        .with(VmFlags::WRITE)
        .with(VmFlags::USER)
        .with(VmFlags::GROWS_DOWN);
    let creds = task.credentials();
    // 规划与写入复用同一套游标运算，避免估算误差让直接用户地址写越过预驻留页。
    let planned_user_sp = layout_user_stack(
        StackLayoutMode::Plan,
        stack_top,
        &main_loaded,
        at_base,
        &exec_path,
        argv,
        envp,
        creds.uid.0,
        creds.euid.0,
        creds.gid.0,
        creds.egid.0,
        hal::user::vdso_base(),
    )?;
    if planned_user_sp < stack_bottom || planned_user_sp >= stack_top {
        return Err(errno::Errno::EINVAL);
    }
    vm.map_anon(stack_bottom..stack_top, stack_flags)?;
    vm.prefault_user_range(planned_user_sp..stack_top, true)?;

    // Map vDSO code page from the synthesized ELF image, then attach the shared
    // data page as a direct read-only mapping for user-space fast paths.
    let vdso_bytes = hal::user::vdso_image();
    let vdso_base = hal::user::vdso_base();
    let vdso_text_len = hal::user::vdso_text_page_len();
    let vdso_code_flags = VmFlags::EMPTY
        .with(VmFlags::READ)
        .with(VmFlags::EXEC)
        .with(VmFlags::USER);
    vm.commit_segment(
        vdso_base,
        vdso_text_len,
        vdso_text_len,
        &vdso_bytes[..vdso_text_len],
        vdso_code_flags,
    )?;
    let vdso_data_flags = VmFlags::EMPTY.with(VmFlags::READ).with(VmFlags::USER);
    let vdso_data_paddr = crate::vdso::shared_data_page_paddr()?;
    vm.map_direct(
        vdso_base + hal::user::vdso_data_page_offset()..vdso_base + hal::user::vdso_total_size(),
        vdso_data_paddr,
        vdso_data_flags,
    )?;

    unsafe {
        let ops = user_pgd_ops().expect("[user] user_pgd_ops not registered");
        (ops.activate)(vm.pgd());
    }

    // RISC-V: 设置 SUM 位允许 S-mode 访问 U=1 的用户页面
    unsafe { hal::user::enable_sum() };

    let user_sp = layout_user_stack(
        StackLayoutMode::Write,
        stack_top,
        &main_loaded,
        at_base,
        &exec_path,
        argv,
        envp,
        creds.uid.0,
        creds.euid.0,
        creds.gid.0,
        creds.egid.0,
        hal::user::vdso_base(),
    )?;
    if user_sp != planned_user_sp {
        return Err(errno::Errno::EIO);
    }

    Ok(LoadedUserImage {
        vm,
        entry_pc,
        user_sp,
        exec_path,
        #[cfg(feature = "performance-profile")]
        main_image_range: main_loaded.base..main_loaded.end,
        #[cfg(feature = "performance-profile")]
        interpreter_image: interpreter_path
            .zip(interp_loaded.as_ref().map(|loaded| loaded.base..loaded.end)),
        exec_access: Arc::new(ExecutableAccessSet {
            leases: exec_access,
        }),
    })
}

fn byte_strings_to_text(values: Vec<Vec<u8>>) -> Result<Vec<String>, errno::Errno> {
    let mut strings = Vec::new();
    strings
        .try_reserve_exact(values.len())
        .map_err(|_| errno::Errno::ENOMEM)?;
    for value in values {
        strings.push(String::from_utf8(value).map_err(|_| errno::Errno::EFAULT)?);
    }
    Ok(strings)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StackLayoutMode {
    Plan,
    Write,
}

impl StackLayoutMode {
    fn writes(self) -> bool {
        self == Self::Write
    }
}

fn layout_user_stack(
    mode: StackLayoutMode,
    stack_top: usize,
    main: &LoadedImage,
    at_base: usize,
    path: &str,
    argv: &[String],
    envp: &[String],
    uid: u32,
    euid: u32,
    gid: u32,
    egid: u32,
    vdso_base: usize,
) -> Result<usize, errno::Errno> {
    const MAX_STACK_ARG_BYTES: usize = 128 * 1024;
    let mut budget = 0usize;
    let argc = if argv.is_empty() { 1 } else { argv.len() };
    budget = budget
        .checked_add(path.len() + 1)
        .ok_or(errno::Errno::EINVAL)?;
    for arg in argv {
        budget = budget
            .checked_add(arg.len() + 1)
            .ok_or(errno::Errno::EINVAL)?;
    }
    for env in envp {
        budget = budget
            .checked_add(env.len() + 1)
            .ok_or(errno::Errno::EINVAL)?;
    }
    budget = budget
        .checked_add(16)
        .and_then(|b| {
            b.checked_add((argc + envp.len() + 4 + 16 * 2) * core::mem::size_of::<usize>())
        })
        .ok_or(errno::Errno::EINVAL)?;
    let stack_budget = core::cmp::min(MAX_STACK_ARG_BYTES, hal::user::default_stack_size() / 2);
    if budget > stack_budget {
        return Err(errno::Errno::EINVAL);
    }

    let mut sp = stack_top;
    let mut argv_ptrs = Vec::new();
    let mut envp_ptrs = Vec::new();
    let execfn_ptr = push_user_string(mode, &mut sp, path.as_bytes());

    if argv.is_empty() {
        argv_ptrs.push(execfn_ptr);
    } else {
        for arg in argv.iter().rev() {
            let ptr = push_user_string(mode, &mut sp, arg.as_bytes());
            argv_ptrs.push(ptr);
        }
        argv_ptrs.reverse();
    }
    for env in envp.iter().rev() {
        let ptr = push_user_string(mode, &mut sp, env.as_bytes());
        envp_ptrs.push(ptr);
    }
    envp_ptrs.reverse();

    sp -= 16;
    let random_ptr = sp;
    if mode.writes() {
        unsafe {
            // Safety: Plan 阶段计算出的完整内容区间已预驻留且可写，两个 u64
            // 都位于该区间内；写入阶段已经激活目标用户页表。
            core::ptr::write_unaligned(random_ptr as *mut u64, 0x6d79676f5f726e64);
            core::ptr::write_unaligned((random_ptr + 8) as *mut u64, 0xfedcba9876543210);
        }
    }

    sp &= !0xf;

    let auxv = [
        (AT_PHDR, main.phdr),
        (AT_PHENT, main.phent),
        (AT_PHNUM, main.phnum),
        (AT_PAGESZ, hal::memory::page_size()),
        (AT_BASE, at_base),
        (AT_ENTRY, main.entry),
        (AT_HWCAP, arch_hwcap()),
        (AT_CLKTCK, 100),
        (AT_UID, uid as usize),
        (AT_EUID, euid as usize),
        (AT_GID, gid as usize),
        (AT_EGID, egid as usize),
        (AT_SECURE, 0),
        (AT_RANDOM, random_ptr),
        (AT_EXECFN, execfn_ptr),
        (AT_SYSINFO_EHDR, vdso_base),
        (AT_NULL, 0),
    ];

    let stack_slots = 1 + argv_ptrs.len() + 1 + envp_ptrs.len() + 1 + auxv.len() * 2;
    if stack_slots % 2 != 0 {
        sp -= 8;
        if mode.writes() {
            // Safety: sp 由共享布局游标产生，指向已预驻留的初始栈内容区间。
            unsafe { core::ptr::write_unaligned(sp as *mut u64, 0) };
        }
    }

    for (key, value) in auxv.iter().rev() {
        sp -= 16;
        if mode.writes() {
            unsafe {
                // Safety: sp 与 sp + 8 均由共享布局游标产生，位于已预驻留区间。
                core::ptr::write_unaligned(sp as *mut u64, *key as u64);
                core::ptr::write_unaligned((sp + 8) as *mut u64, *value as u64);
            }
        }
    }

    sp -= 8;
    if mode.writes() {
        // Safety: sp 由共享布局游标产生，指向已预驻留的初始栈内容区间。
        unsafe { core::ptr::write_unaligned(sp as *mut u64, 0) };
    }
    for ptr in envp_ptrs.iter().rev() {
        sp -= 8;
        if mode.writes() {
            // Safety: sp 由共享布局游标产生，指向已预驻留的初始栈内容区间。
            unsafe { core::ptr::write_unaligned(sp as *mut u64, *ptr as u64) };
        }
    }

    sp -= 8;
    if mode.writes() {
        // Safety: sp 由共享布局游标产生，指向已预驻留的初始栈内容区间。
        unsafe { core::ptr::write_unaligned(sp as *mut u64, 0) };
    }
    for ptr in argv_ptrs.iter().rev() {
        sp -= 8;
        if mode.writes() {
            // Safety: sp 由共享布局游标产生，指向已预驻留的初始栈内容区间。
            unsafe { core::ptr::write_unaligned(sp as *mut u64, *ptr as u64) };
        }
    }

    sp -= 8;
    if mode.writes() {
        // Safety: sp 由共享布局游标产生，指向已预驻留的初始栈内容区间。
        unsafe { core::ptr::write_unaligned(sp as *mut u64, argc as u64) };
    }

    Ok(sp)
}

fn push_user_string(mode: StackLayoutMode, sp: &mut usize, bytes: &[u8]) -> usize {
    *sp -= bytes.len() + 1;
    let ptr = *sp;
    if mode.writes() {
        unsafe {
            // Safety: ptr..ptr+len+1 由 Plan 阶段同一游标计算并已预驻留；源切片
            // 有 bytes.len() 字节，末尾 NUL 仍位于对应字符串保留区间内。
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            core::ptr::write((ptr + bytes.len()) as *mut u8, 0);
        }
    }
    ptr
}

fn arch_hwcap() -> usize {
    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vector::user_hwcap()
    }
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::user_hwcap()
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    {
        0
    }
}

struct ShebangExec {
    interpreter: String,
    argv: Vec<String>,
}

fn parse_shebang(
    path: &str,
    argv: &[String],
    bytes: &[u8],
    depth: usize,
) -> Result<ShebangExec, errno::Errno> {
    if depth >= MAX_SHEBANG_DEPTH {
        return Err(errno::Errno::ELOOP);
    }
    let line_end = bytes
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(bytes.len());
    let mut line = &bytes[2..line_end];
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    line = trim_ascii_start(line);
    if line.is_empty() {
        return Err(errno::Errno::ENOEXEC);
    }

    let interp_end = line
        .iter()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let interpreter =
        core::str::from_utf8(&line[..interp_end]).map_err(|_| errno::Errno::ENOEXEC)?;
    if interpreter.is_empty() {
        return Err(errno::Errno::ENOEXEC);
    }

    let mut new_argv = Vec::new();
    new_argv.push(String::from(interpreter));
    let optional = trim_ascii(trim_ascii_start(&line[interp_end..]));
    if !optional.is_empty() {
        new_argv.push(String::from(
            core::str::from_utf8(optional).map_err(|_| errno::Errno::ENOEXEC)?,
        ));
    }
    new_argv.push(String::from(path));
    if argv.len() > 1 {
        new_argv.extend(argv[1..].iter().cloned());
    }

    Ok(ShebangExec {
        interpreter: String::from(interpreter),
        argv: new_argv,
    })
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if !first.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    bytes = trim_ascii_start(bytes);
    while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn validate_user_image_result(img: &dyn Image<'_>) -> Result<(), ()> {
    if img.arch() != hal::platform::elf_arch() {
        log::info!(
            "[user] validate: arch mismatch got={:?} expect={:?}",
            img.arch(),
            hal::platform::elf_arch()
        );
        return Err(());
    }
    if img.load_vaddr_range().is_none() {
        log::info!("[user] validate: load_vaddr_range is None");
        return Err(());
    }
    Ok(())
}

fn validate_exec_image_result(img: &LinuxElfMetadata) -> Result<(), ()> {
    if img.arch() != hal::platform::elf_arch() {
        log::info!(
            "[user] validate: arch mismatch got={:?} expect={:?}",
            img.arch(),
            hal::platform::elf_arch()
        );
        return Err(());
    }
    if img.load_range().is_none() {
        log::info!("[user] validate: load_vaddr_range is None");
        return Err(());
    }
    Ok(())
}

fn load_exec_image(
    vm: &VmSpace,
    img: &LinuxElfMetadata,
    file: &Arc<File>,
    load_bias: usize,
    update_brk: bool,
    label: &str,
) -> Result<LoadedImage, errno::Errno> {
    let mut max_segment_end: usize = 0;
    for seg in img.load_segments() {
        let flags = seg.permissions.to_vm_flags();
        let vaddr = load_bias
            .checked_add(seg.vaddr)
            .ok_or(errno::Errno::ENOEXEC)?;
        let seg_end = vaddr
            .checked_add(seg.mem_size)
            .ok_or(errno::Errno::ENOEXEC)?;
        log::debug!(
            "[user] {} segment vaddr={:#x} memsz={:#x} filesz={:#x} flags={:?}",
            label,
            vaddr,
            seg.mem_size,
            seg.file_size,
            flags
        );
        vm.commit_file_segment(
            vaddr,
            seg.mem_size,
            seg.file_offset,
            seg.file_size,
            file.clone(),
            flags,
        )?;
        if seg_end > max_segment_end {
            max_segment_end = seg_end;
        }
    }
    if update_brk {
        vm.init_brk_after_load(max_segment_end);
    }
    Ok(LoadedImage {
        entry: load_bias
            .checked_add(img.entry())
            .ok_or(errno::Errno::ENOEXEC)?,
        base: load_bias,
        end: max_segment_end,
        phdr: img
            .program_header_vaddr()
            .and_then(|v| load_bias.checked_add(v))
            .unwrap_or(0),
        phent: img.program_header_entry_size() as usize,
        phnum: img.program_header_count() as usize,
    })
}

fn load_exec_image_from_file(file: &Arc<File>) -> Result<LinuxElfMetadata, errno::Errno> {
    let file_size = file_size(file)?;
    if file_size == 0 {
        return Err(errno::Errno::ENOEXEC);
    }
    let reader = VfsElfReader {
        file: file.clone(),
        size: file_size,
    };
    read_linux_elf(&reader, ElfReadLimits::default()).map_err(|error| match error {
        ElfReadError::Format(_) => errno::Errno::ENOEXEC,
        ElfReadError::Source(error) => error,
        ElfReadError::ResourceExhausted => errno::Errno::ENOMEM,
    })
}

fn read_small_file_range(file: &File, offset: u64, len: usize) -> Result<Vec<u8>, errno::Errno> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| errno::Errno::ENOMEM)?;
    bytes.resize(len, 0);
    if len != 0 {
        read_exact_file(file, offset, &mut bytes)?;
    }
    Ok(bytes)
}

fn load_image(
    vm: &VmSpace,
    img: &dyn Image<'_>,
    load_bias: usize,
    label: &str,
) -> Result<LoadedImage, errno::Errno> {
    let mut max_segment_end: usize = 0;
    for seg in img.segments() {
        let flags = seg.perms.to_vm_flags();
        let vaddr = load_bias
            .checked_add(seg.vaddr)
            .ok_or(errno::Errno::ENOEXEC)?;
        let seg_end = vaddr.checked_add(seg.memsz).ok_or(errno::Errno::ENOEXEC)?;
        log::debug!(
            "[user] {} segment vaddr={:#x} memsz={:#x} filesz={:#x} flags={:?}",
            label,
            vaddr,
            seg.memsz,
            seg.file_size,
            flags
        );
        vm.commit_segment(vaddr, seg.memsz, seg.file_size, seg.data, flags)?;
        if seg_end > max_segment_end {
            max_segment_end = seg_end;
        }
    }
    Ok(LoadedImage {
        entry: load_bias
            .checked_add(img.entry())
            .ok_or(errno::Errno::ENOEXEC)?,
        base: load_bias,
        end: max_segment_end,
        phdr: img
            .phdr_vaddr()
            .and_then(|v| load_bias.checked_add(v))
            .unwrap_or(0),
        phent: img.phdr_entry_size(),
        phnum: img.phdr_count(),
    })
}

fn resolve_exec_path(task: &Arc<Task>, path: &str) -> String {
    let ctx = match task_vfs_context(task) {
        Ok(ctx) => ctx,
        Err(_) => return String::from(path),
    };
    if let Ok(result) = vfs::path::lookup(&ctx, &Dirfd::Cwd, path, LookupFlags::default())
        && let Some(abs) = vfs::namespace_path(&ctx, &result.dentry, &result.mount)
    {
        return abs;
    }
    if path.starts_with('/') {
        return String::from(path);
    }
    vfs::namespace_path(&ctx, &ctx.cwd(), &ctx.cwd_mount())
        .map(|cwd| vfs::join_abs_paths(&cwd, path))
        .unwrap_or_else(|| String::from(path))
}

fn load_interpreter_from_task_vfs(
    task: &Arc<Task>,
    exec_path: &str,
    interp: &str,
) -> Result<LoadedInterpreter, errno::Errno> {
    match load_executable_bytes_from_task_vfs(task, interp) {
        Ok(loaded) => return Ok(loaded),
        Err(errno::Errno::ENOENT) => {}
        Err(err) => return Err(err),
    }

    if !interp.starts_with('/') || !exec_path.starts_with('/') {
        return Err(errno::Errno::ENOENT);
    }

    let mut candidates = Vec::new();
    let mut dir = parent_dir(exec_path);
    loop {
        push_interpreter_candidates(&mut candidates, dir, interp);
        if dir == "/" {
            break;
        }
        dir = parent_dir(dir);
    }

    for candidate in candidates {
        match load_executable_bytes_from_task_vfs(task, &candidate) {
            Ok(loaded) => return Ok(loaded),
            Err(errno::Errno::ENOENT) => continue,
            Err(err) => return Err(err),
        }
    }
    Err(errno::Errno::ENOENT)
}

fn push_interpreter_candidates(candidates: &mut Vec<String>, prefix: &str, interp: &str) {
    if prefix == "/" {
        return;
    }
    let interp_rel = interp.strip_prefix('/').unwrap_or(interp);
    push_unique_candidate(candidates, &vfs::join_abs_paths(prefix, interp_rel));

    if let Some(lib64_rel) = interp.strip_prefix("/lib64/") {
        push_unique_candidate(
            candidates,
            &vfs::join_abs_paths(prefix, &vfs::join_abs_paths("lib", lib64_rel)),
        );
    } else if let Some(lib_rel) = interp.strip_prefix("/lib/") {
        push_unique_candidate(
            candidates,
            &vfs::join_abs_paths(prefix, &vfs::join_abs_paths("lib64", lib_rel)),
        );
    }

    if interp_basename(interp).is_some_and(|name| name.starts_with("ld-musl-")) {
        push_unique_candidate(candidates, &vfs::join_abs_paths(prefix, "lib/libc.so"));
    }
}

fn push_unique_candidate(candidates: &mut Vec<String>, candidate: &str) {
    if !candidates.iter().any(|existing| existing == candidate) {
        candidates.push(String::from(candidate));
    }
}

fn parent_dir(path: &str) -> &str {
    let trimmed = trim_trailing_slashes(path);
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(idx) => &trimmed[..idx],
        None => ".",
    }
}

fn trim_trailing_slashes(path: &str) -> &str {
    let mut end = path.len();
    while end > 1 && path.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

fn interp_basename(path: &str) -> Option<&str> {
    let trimmed = trim_trailing_slashes(path);
    trimmed.rsplit('/').next()
}

fn task_vfs_context(task: &Arc<Task>) -> Result<Arc<VfsContext>, errno::Errno> {
    task.ext_lookup(sched::TASKEXT_VFS_CONTEXT)
        .ok_or(errno::Errno::ENOENT)?
        .downcast::<VfsContext>()
        .map_err(|_| errno::Errno::EINVAL)
}

fn open_file_from_task_vfs(task: &Arc<Task>, path: &str) -> Result<Arc<File>, errno::Errno> {
    let ctx = task_vfs_context(task)?;
    let tmp_fdt = FdTable::new_default();
    let flags = OpenOptions {
        access: AccessMode::ReadOnly,
        ..OpenOptions::default()
    };
    let fd = vfs::operation::openat(&ctx, &tmp_fdt, &Dirfd::Cwd, path, flags, FileMode::new(0))
        .map_err(|err| err.to_errno())?;
    let file = tmp_fdt.get_file(fd).ok_or(errno::Errno::EBADF)?;
    let _ = tmp_fdt.close_fd(fd);
    Ok(file)
}

fn load_elf_prefix_from_file(file: &File) -> Result<Vec<u8>, errno::Errno> {
    let size = file_size(file)?;
    if size == 0 {
        return Err(errno::Errno::ENOEXEC);
    }
    let len = core::cmp::min(size, ELF_PREFIX_READ_SIZE as u64) as usize;
    read_small_file_range(file, 0, len)
}

pub(crate) fn file_size(file: &File) -> Result<u64, errno::Errno> {
    let size = file.stat().map_err(|e| e.to_errno())?.size;
    u64::try_from(size).map_err(|_| errno::Errno::EFBIG)
}

pub(crate) fn read_exact_file(
    file: &File,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), errno::Errno> {
    let mut done = 0usize;
    while done < buf.len() {
        let read_off = offset
            .checked_add(done as u64)
            .ok_or(errno::Errno::ENOEXEC)?;
        let n = file
            .read_at(&mut buf[done..], read_off)
            .map_err(|err| err.to_errno())?;
        if n == 0 {
            return Err(errno::Errno::ENOEXEC);
        }
        done += n;
    }
    Ok(())
}

pub(crate) fn load_file_from_task_vfs(
    task: &Arc<Task>,
    path: &str,
) -> Result<Vec<u8>, errno::Errno> {
    let file = open_file_from_task_vfs(task, path)?;
    read_entire_file(&file)
}

fn load_executable_bytes_from_task_vfs(
    task: &Arc<Task>,
    path: &str,
) -> Result<LoadedInterpreter, errno::Errno> {
    let file = open_file_from_task_vfs(task, path)?;
    check_exec_permission(task, &file)?;
    let access = file
        .inode()
        .acquire_exec_access()
        .map_err(|error| error.to_errno())?;
    let bytes = read_entire_file(&file)?;
    Ok(LoadedInterpreter { bytes, access })
}

fn check_exec_permission(task: &Arc<Task>, file: &Arc<File>) -> Result<(), errno::Errno> {
    if file.inode().kind() != vfs::stat::FileType::Regular {
        return Err(errno::Errno::EACCES);
    }
    if file
        .mount()
        .flags_snapshot()
        .has(vfs::mount::MountFlags::NOEXEC)
    {
        return Err(errno::Errno::EACCES);
    }
    let stat = file.inode().stat().map_err(|error| error.to_errno())?;
    let ctx = task_vfs_context(task)?;
    if !ctx.cred().can_exec(
        vfs::cred::Uid(stat.uid),
        vfs::cred::Gid(stat.gid),
        FileMode::new(stat.mode as u16),
        false,
    ) {
        return Err(errno::Errno::EACCES);
    }
    Ok(())
}

fn read_entire_file(file: &Arc<File>) -> Result<Vec<u8>, errno::Errno> {
    let size = usize::try_from(file_size(file)?).map_err(|_| errno::Errno::EFBIG)?;
    if size == 0 {
        return Err(errno::Errno::ENOEXEC);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| errno::Errno::ENOMEM)?;
    bytes.resize(size, 0);
    let mut off = 0usize;
    while off < size {
        let n = file
            .read_at(&mut bytes[off..], off as u64)
            .map_err(|err| err.to_errno())?;
        if n == 0 {
            break;
        }
        off += n;
    }
    bytes.truncate(off);
    Ok(bytes)
}
