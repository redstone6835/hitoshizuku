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
use core::convert::TryFrom;

use general::mm::{VmSpace, user_pgd_ops};
use general::vfs::{
    self, FdTable, FileMode, VfsContext,
    file::{AccessMode, File, OpenOptions},
    inode::InodeExecAccess,
    path::{Dirfd, LookupFlags},
};
use mm::VmFlags;
use sched::Task;

use elf::{Arch, Image, SegmentPerms};

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

const ELF64_EHDR_SIZE: usize = 64;
const ELF64_PHDR_SIZE: usize = 56;
const ELF_PREFIX_READ_SIZE: usize = 4096;
const MAX_ELF_PHDR_BYTES: usize = 256 * 1024;
const MAX_ELF_INTERP_BYTES: usize = 4096;
const MAX_ELF_DYNAMIC_BYTES: usize = 1024 * 1024;
const ELF64_EHDR_OFF_TYPE: usize = 0x10;
const ELF64_EHDR_OFF_MACHINE: usize = 0x12;
const ELF64_EHDR_OFF_ENTRY: usize = 0x18;
const ELF64_EHDR_OFF_PHOFF: usize = 0x20;
const ELF64_EHDR_OFF_PHENTSIZE: usize = 0x36;
const ELF64_EHDR_OFF_PHNUM: usize = 0x38;
const ELF64_PHDR_OFF_TYPE: usize = 0x00;
const ELF64_PHDR_OFF_FLAGS: usize = 0x04;
const ELF64_PHDR_OFF_OFFSET: usize = 0x08;
const ELF64_PHDR_OFF_VADDR: usize = 0x10;
const ELF64_PHDR_OFF_FILESZ: usize = 0x20;
const ELF64_PHDR_OFF_MEMSZ: usize = 0x28;
const ELF64_PHDR_OFF_ALIGN: usize = 0x30;
const ELF64_PT_DYNAMIC: u32 = 2;
const ELF64_ET_EXEC: u16 = 2;
const ELF64_ET_DYN: u16 = 3;
const ELF64_PT_LOAD: u32 = 1;
const ELF64_PT_INTERP: u32 = 3;
const ELF64_PT_PHDR: u32 = 6;
const ELF64_PF_X: u32 = 1 << 0;
const ELF64_PF_W: u32 = 1 << 1;
const ELF64_PF_R: u32 = 1 << 2;
const ELF64_DYN_ENTRY_SIZE: usize = 16;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EM_RISCV: u16 = 243;
const EM_LOONGARCH: u16 = 258;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_REL: u64 = 17;
const DT_RELSZ: u64 = 18;
const DT_JMPREL: u64 = 23;
const DT_PLTRELSZ: u64 = 2;

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

struct LoadedImage {
    entry: usize,
    base: usize,
    end: usize,
    phdr: usize,
    phent: usize,
    phnum: usize,
}

struct ExecImage {
    entry: usize,
    arch: Arch,
    is_pie: bool,
    phent: usize,
    phnum: usize,
    phdr_vaddr: Option<usize>,
    load_range_seen: bool,
    interpreter: Option<String>,
    can_run_without_interpreter: bool,
    segments: Vec<ExecSegment>,
}

#[derive(Clone)]
struct ExecSegment {
    vaddr: usize,
    memsz: usize,
    file_offset: u64,
    file_size: usize,
    perms: SegmentPerms,
}

#[derive(Clone, Copy)]
struct ExecPhdr {
    ty: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
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
    check_exec_permission(task, &file)?;
    let main_exec_access = file
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
    let main_bias = if exec_image.is_pie {
        hal::user::main_pie_base()
    } else {
        0
    };
    let main_loaded = load_exec_image(&vm, &exec_image, &file, main_bias, true, "exec")?;
    let exec_path = resolve_exec_path(task, path);
    let mut exec_access = Vec::new();
    exec_access.push(main_exec_access);

    let interpreter_path = exec_image.interpreter.clone();
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
            Err(errno::Errno::ENOENT) if exec_image.can_run_without_interpreter => None,
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

fn dynamic_can_run_without_interpreter(dynamic: Option<&[u8]>) -> bool {
    let Some(dynamic) = dynamic else {
        return true;
    };
    let mut has_needed = false;
    let mut rela_size = 0u64;
    let mut rel_size = 0u64;
    let mut plt_rel_size = 0u64;
    let mut has_jmprel = false;

    for ent in dynamic.chunks_exact(ELF64_DYN_ENTRY_SIZE) {
        let tag = read_u64_at(ent, 0);
        let val = read_u64_at(ent, 8);
        match tag {
            DT_NULL => break,
            DT_NEEDED => has_needed = true,
            DT_RELASZ => rela_size = val,
            DT_RELSZ => rel_size = val,
            DT_PLTRELSZ => plt_rel_size = val,
            DT_JMPREL => has_jmprel = val != 0,
            DT_RELA | DT_REL => {}
            _ => {}
        }
    }

    !has_needed && rela_size == 0 && rel_size == 0 && plt_rel_size == 0 && !has_jmprel
}

fn read_u16_at(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn read_u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn read_u64_at(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
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

fn validate_exec_image_result(img: &ExecImage) -> Result<(), ()> {
    if img.arch != hal::platform::elf_arch() {
        log::info!(
            "[user] validate: arch mismatch got={:?} expect={:?}",
            img.arch,
            hal::platform::elf_arch()
        );
        return Err(());
    }
    if !img.load_range_seen {
        log::info!("[user] validate: load_vaddr_range is None");
        return Err(());
    }
    Ok(())
}

fn load_exec_image(
    vm: &VmSpace,
    img: &ExecImage,
    file: &Arc<File>,
    load_bias: usize,
    update_brk: bool,
    label: &str,
) -> Result<LoadedImage, errno::Errno> {
    let mut max_segment_end: usize = 0;
    for seg in &img.segments {
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
        vm.commit_file_segment(
            vaddr,
            seg.memsz,
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
            .checked_add(img.entry)
            .ok_or(errno::Errno::ENOEXEC)?,
        base: load_bias,
        end: max_segment_end,
        phdr: img
            .phdr_vaddr
            .and_then(|v| load_bias.checked_add(v))
            .unwrap_or(0),
        phent: img.phent,
        phnum: img.phnum,
    })
}

fn load_exec_image_from_file(file: &Arc<File>) -> Result<ExecImage, errno::Errno> {
    let file_size = file_size(file)?;
    if file_size == 0 {
        return Err(errno::Errno::ENOEXEC);
    }

    let mut ehdr = [0u8; ELF64_EHDR_SIZE];
    read_exact_file(file, 0, &mut ehdr)?;
    validate_elf_ident(&ehdr)?;

    let ty = read_u16_at(&ehdr, ELF64_EHDR_OFF_TYPE);
    if ty != ELF64_ET_EXEC && ty != ELF64_ET_DYN {
        return Err(errno::Errno::ENOEXEC);
    }
    let arch = map_elf_machine(read_u16_at(&ehdr, ELF64_EHDR_OFF_MACHINE));
    let entry = usize::try_from(read_u64_at(&ehdr, ELF64_EHDR_OFF_ENTRY))
        .map_err(|_| errno::Errno::ENOEXEC)?;
    let phoff = read_u64_at(&ehdr, ELF64_EHDR_OFF_PHOFF);
    let phentsize = read_u16_at(&ehdr, ELF64_EHDR_OFF_PHENTSIZE) as usize;
    let phnum = read_u16_at(&ehdr, ELF64_EHDR_OFF_PHNUM) as usize;
    if phentsize != ELF64_PHDR_SIZE {
        return Err(errno::Errno::ENOEXEC);
    }
    let phdr_bytes_len = phentsize.checked_mul(phnum).ok_or(errno::Errno::ENOEXEC)?;
    if phdr_bytes_len > MAX_ELF_PHDR_BYTES {
        return Err(errno::Errno::ENOEXEC);
    }
    let phdr_end = phoff
        .checked_add(u64::try_from(phdr_bytes_len).map_err(|_| errno::Errno::ENOEXEC)?)
        .ok_or(errno::Errno::ENOEXEC)?;
    if phdr_end > file_size {
        return Err(errno::Errno::ENOEXEC);
    }

    let mut phdr_bytes = Vec::new();
    phdr_bytes
        .try_reserve_exact(phdr_bytes_len)
        .map_err(|_| errno::Errno::ENOMEM)?;
    phdr_bytes.resize(phdr_bytes_len, 0);
    read_exact_file(file, phoff, &mut phdr_bytes)?;

    let mut phdrs = Vec::new();
    phdrs
        .try_reserve_exact(phnum)
        .map_err(|_| errno::Errno::ENOMEM)?;
    for idx in 0..phnum {
        let off = idx * phentsize;
        phdrs.push(decode_exec_phdr(&phdr_bytes[off..off + ELF64_PHDR_SIZE]));
    }

    validate_load_segments(file_size, &phdrs)?;
    validate_phdr_table(phoff, phdr_end, file_size, &phdrs)?;
    validate_entry(entry, &phdrs)?;

    let interpreter = read_interp(file, file_size, &phdrs)?;
    let dynamic = read_dynamic(file, file_size, &phdrs)?;
    let can_run_without_interpreter = dynamic_can_run_without_interpreter(dynamic.as_deref());
    let phdr_vaddr = find_exec_phdr_vaddr(phoff, phdr_end, &phdrs);

    let mut segments = Vec::new();
    let mut load_range_seen = false;
    for ph in &phdrs {
        if ph.ty != ELF64_PT_LOAD || ph.memsz == 0 {
            continue;
        }
        segments
            .try_reserve_exact(1)
            .map_err(|_| errno::Errno::ENOMEM)?;
        segments.push(ExecSegment {
            vaddr: usize::try_from(ph.vaddr).map_err(|_| errno::Errno::ENOEXEC)?,
            memsz: usize::try_from(ph.memsz).map_err(|_| errno::Errno::ENOEXEC)?,
            file_offset: ph.offset,
            file_size: usize::try_from(ph.filesz).map_err(|_| errno::Errno::ENOEXEC)?,
            perms: perms_from_exec_phdr(ph),
        });
        load_range_seen = true;
    }

    Ok(ExecImage {
        entry,
        arch,
        is_pie: ty == ELF64_ET_DYN,
        phent: phentsize,
        phnum,
        phdr_vaddr,
        load_range_seen,
        interpreter,
        can_run_without_interpreter,
        segments,
    })
}

fn validate_elf_ident(ehdr: &[u8]) -> Result<(), errno::Errno> {
    if ehdr.get(0..4) != Some(b"\x7fELF") {
        return Err(errno::Errno::ENOEXEC);
    }
    if ehdr[4] != 2 || ehdr[5] != 1 {
        return Err(errno::Errno::ENOEXEC);
    }
    Ok(())
}

fn map_elf_machine(machine: u16) -> Arch {
    match machine {
        EM_LOONGARCH => Arch::LoongArch64,
        EM_RISCV => Arch::Riscv64,
        EM_X86_64 => Arch::X86_64,
        EM_AARCH64 => Arch::Aarch64,
        other => Arch::Unknown(other),
    }
}

fn decode_exec_phdr(bytes: &[u8]) -> ExecPhdr {
    ExecPhdr {
        ty: read_u32_at(bytes, ELF64_PHDR_OFF_TYPE),
        flags: read_u32_at(bytes, ELF64_PHDR_OFF_FLAGS),
        offset: read_u64_at(bytes, ELF64_PHDR_OFF_OFFSET),
        vaddr: read_u64_at(bytes, ELF64_PHDR_OFF_VADDR),
        filesz: read_u64_at(bytes, ELF64_PHDR_OFF_FILESZ),
        memsz: read_u64_at(bytes, ELF64_PHDR_OFF_MEMSZ),
        align: read_u64_at(bytes, ELF64_PHDR_OFF_ALIGN),
    }
}

fn perms_from_exec_phdr(ph: &ExecPhdr) -> SegmentPerms {
    let mut perms = SegmentPerms::EMPTY;
    if ph.flags & ELF64_PF_R != 0 {
        perms = perms.with(SegmentPerms::READ);
    }
    if ph.flags & ELF64_PF_W != 0 {
        perms = perms.with(SegmentPerms::WRITE);
    }
    if ph.flags & ELF64_PF_X != 0 {
        perms = perms.with(SegmentPerms::EXEC);
    }
    perms
}

fn validate_load_segments(file_size: u64, phdrs: &[ExecPhdr]) -> Result<(), errno::Errno> {
    for ph in phdrs {
        if ph.ty != ELF64_PT_LOAD {
            continue;
        }
        validate_load_alignment(ph)?;
        if ph.filesz > ph.memsz {
            return Err(errno::Errno::ENOEXEC);
        }
        checked_file_range(file_size, ph.offset, ph.filesz)?;
        checked_vaddr_range(ph.vaddr, ph.memsz)?;
    }
    validate_load_overlaps(phdrs)
}

fn validate_load_alignment(ph: &ExecPhdr) -> Result<(), errno::Errno> {
    if ph.align <= 1 {
        return Ok(());
    }
    if !ph.align.is_power_of_two() {
        return Err(errno::Errno::ENOEXEC);
    }
    if ph.vaddr % ph.align != ph.offset % ph.align {
        return Err(errno::Errno::ENOEXEC);
    }
    Ok(())
}

fn validate_load_overlaps(phdrs: &[ExecPhdr]) -> Result<(), errno::Errno> {
    for i in 0..phdrs.len() {
        let left = phdrs[i];
        if left.ty != ELF64_PT_LOAD || left.memsz == 0 {
            continue;
        }
        let left_range = checked_vaddr_range(left.vaddr, left.memsz)?;
        for right in phdrs.iter().skip(i + 1).copied() {
            if right.ty != ELF64_PT_LOAD || right.memsz == 0 {
                continue;
            }
            let right_range = checked_vaddr_range(right.vaddr, right.memsz)?;
            if left_range.0 < right_range.1 && right_range.0 < left_range.1 {
                return Err(errno::Errno::ENOEXEC);
            }
        }
    }
    Ok(())
}

fn validate_phdr_table(
    phoff: u64,
    phdr_end: u64,
    file_size: u64,
    phdrs: &[ExecPhdr],
) -> Result<(), errno::Errno> {
    let mut seen = false;
    for ph in phdrs {
        if ph.ty != ELF64_PT_PHDR {
            continue;
        }
        if seen || ph.filesz > ph.memsz {
            return Err(errno::Errno::ENOEXEC);
        }
        seen = true;
        checked_file_range(file_size, ph.offset, ph.filesz)?;
        let seg_end = ph
            .offset
            .checked_add(ph.filesz)
            .ok_or(errno::Errno::ENOEXEC)?;
        if ph.offset > phoff || seg_end < phdr_end {
            return Err(errno::Errno::ENOEXEC);
        }
        checked_vaddr_range(ph.vaddr, ph.memsz)?;
    }
    Ok(())
}

fn validate_entry(entry: usize, phdrs: &[ExecPhdr]) -> Result<(), errno::Errno> {
    for ph in phdrs {
        if ph.ty != ELF64_PT_LOAD || ph.memsz == 0 || ph.flags & ELF64_PF_X == 0 {
            continue;
        }
        let range = checked_vaddr_range(ph.vaddr, ph.memsz)?;
        if range.0 <= entry && entry < range.1 {
            return Ok(());
        }
    }
    Err(errno::Errno::ENOEXEC)
}

fn read_interp(
    file: &File,
    file_size: u64,
    phdrs: &[ExecPhdr],
) -> Result<Option<String>, errno::Errno> {
    for ph in phdrs {
        if ph.ty != ELF64_PT_INTERP {
            continue;
        }
        checked_file_range(file_size, ph.offset, ph.filesz)?;
        let len = usize::try_from(ph.filesz).map_err(|_| errno::Errno::ENOEXEC)?;
        if len <= 1 || len > MAX_ELF_INTERP_BYTES {
            return Err(errno::Errno::ENOEXEC);
        }
        let raw = read_small_file_range(file, ph.offset, len)?;
        if raw.last() != Some(&0) {
            return Err(errno::Errno::ENOEXEC);
        }
        let path = &raw[..raw.len() - 1];
        if path.is_empty() || path.contains(&0) {
            return Err(errno::Errno::ENOEXEC);
        }
        let path = core::str::from_utf8(path).map_err(|_| errno::Errno::ENOEXEC)?;
        return Ok(Some(String::from(path)));
    }
    Ok(None)
}

fn read_dynamic(
    file: &File,
    file_size: u64,
    phdrs: &[ExecPhdr],
) -> Result<Option<Vec<u8>>, errno::Errno> {
    for ph in phdrs {
        if ph.ty != ELF64_PT_DYNAMIC {
            continue;
        }
        checked_file_range(file_size, ph.offset, ph.filesz)?;
        let len = usize::try_from(ph.filesz).map_err(|_| errno::Errno::ENOEXEC)?;
        if len > MAX_ELF_DYNAMIC_BYTES {
            return Err(errno::Errno::ENOEXEC);
        }
        return Ok(Some(read_small_file_range(file, ph.offset, len)?));
    }
    Ok(None)
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

fn find_exec_phdr_vaddr(phoff: u64, phdr_end: u64, phdrs: &[ExecPhdr]) -> Option<usize> {
    for ph in phdrs {
        if ph.ty == ELF64_PT_PHDR {
            return phdr_table_vaddr_in_segment(phoff, phdr_end, ph.offset, ph.filesz, ph.vaddr);
        }
    }
    for ph in phdrs {
        if ph.ty != ELF64_PT_LOAD {
            continue;
        }
        if let Some(vaddr) =
            phdr_table_vaddr_in_segment(phoff, phdr_end, ph.offset, ph.filesz, ph.vaddr)
        {
            return Some(vaddr);
        }
    }
    None
}

fn phdr_table_vaddr_in_segment(
    table_start: u64,
    table_end: u64,
    seg_offset: u64,
    seg_filesz: u64,
    seg_vaddr: u64,
) -> Option<usize> {
    let seg_end = seg_offset.checked_add(seg_filesz)?;
    if seg_offset > table_start || seg_end < table_end {
        return None;
    }
    let delta = table_start.checked_sub(seg_offset)?;
    usize::try_from(seg_vaddr.checked_add(delta)?).ok()
}

fn checked_file_range(file_size: u64, offset: u64, size: u64) -> Result<(), errno::Errno> {
    let end = offset.checked_add(size).ok_or(errno::Errno::ENOEXEC)?;
    if end > file_size {
        return Err(errno::Errno::ENOEXEC);
    }
    Ok(())
}

fn checked_vaddr_range(vaddr: u64, size: u64) -> Result<(usize, usize), errno::Errno> {
    let end = vaddr.checked_add(size).ok_or(errno::Errno::ENOEXEC)?;
    let start = usize::try_from(vaddr).map_err(|_| errno::Errno::ENOEXEC)?;
    let end = usize::try_from(end).map_err(|_| errno::Errno::ENOEXEC)?;
    Ok((start, end))
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

fn file_size(file: &File) -> Result<u64, errno::Errno> {
    let size = file.stat().map_err(|e| e.to_errno())?.size;
    u64::try_from(size).map_err(|_| errno::Errno::EFBIG)
}

fn read_exact_file(file: &File, offset: u64, buf: &mut [u8]) -> Result<(), errno::Errno> {
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
