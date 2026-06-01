//! 用户 ELF 装载：从 VFS 路径读取 ELF、构建 VmSpace、布用户栈。
//!
//! 主要入口 [`load_user_image_from_path`] 供 ProcessImageOps::execve 调用。
//!
//! 流程：
//! - 解析 ELF；
//! - 建 VmSpace，对每个 PT_LOAD 段调 VmSpace::commit_segment；
//! - 预分配用户栈并布 argc/argv/envp/auxv；
//! - 返回 LoadedUserImage（vm + entry_pc + user_sp）。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use general::mm::{VmSpace, user_pgd_ops};
use general::vfs::{
    self, FdTable, FileMode, VfsContext,
    file::{AccessMode, OpenOptions},
    path::{Dirfd, LookupFlags},
};
use mm::VmFlags;
use sched::Task;

use elf::Image;

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_PAGESZ: usize = 6;
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
const MAX_SHEBANG_DEPTH: usize = 4;

const ELF64_EHDR_SIZE: usize = 64;
const ELF64_PHDR_SIZE: usize = 56;
const ELF64_EHDR_OFF_PHOFF: usize = 0x20;
const ELF64_EHDR_OFF_PHENTSIZE: usize = 0x36;
const ELF64_EHDR_OFF_PHNUM: usize = 0x38;
const ELF64_PHDR_OFF_TYPE: usize = 0x00;
const ELF64_PHDR_OFF_OFFSET: usize = 0x08;
const ELF64_PHDR_OFF_FILESZ: usize = 0x20;
const ELF64_PT_DYNAMIC: u32 = 2;
const ELF64_DYN_ENTRY_SIZE: usize = 16;
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
}

struct LoadedImage {
    entry: usize,
    base: usize,
    phdr: usize,
    phent: usize,
    phnum: usize,
}

pub fn load_user_image_from_path(
    task: &Arc<Task>,
    path: &str,
    argv: &[String],
    envp: &[String],
) -> Result<LoadedUserImage, errno::Errno> {
    load_user_image_from_path_inner(task, path, argv, envp, 0)
}

fn load_user_image_from_path_inner(
    task: &Arc<Task>,
    path: &str,
    argv: &[String],
    envp: &[String],
    shebang_depth: usize,
) -> Result<LoadedUserImage, errno::Errno> {
    let bytes = match load_file_from_task_vfs(task, path) {
        Ok(b) => {
            b
        }
        Err(e) => {
            log::debug!("[user] load path={:?} read failed: {:?}", path, e);
            return Err(e);
        }
    };
    if bytes.starts_with(b"#!") {
        let script = parse_shebang(path, argv, &bytes, shebang_depth)?;
        return load_user_image_from_path_inner(
            task,
            &script.interpreter,
            &script.argv,
            envp,
            shebang_depth + 1,
        );
    }

    let img = match elf::parse(&bytes) {
        Ok(i) => i,
        Err(e) => {
            log::debug!("[user] elf parse failed for {:?}: {:?}", path, e);
            if bytes.len() >= 16 {
                log::debug!(
                    "[user]   first 16 bytes: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
                    bytes[0], bytes[1], bytes[2], bytes[3],
                    bytes[4], bytes[5], bytes[6], bytes[7],
                    bytes[8], bytes[9], bytes[10], bytes[11],
                    bytes[12], bytes[13], bytes[14], bytes[15],
                );
            }
            return Err(errno::Errno::ENOEXEC);
        }
    };
    if validate_user_image_result(&*img).is_err() {
        return Err(errno::Errno::ENOEXEC);
    }

    let vm = Arc::new(VmSpace::new());
    let main_bias = if img.is_pie() {
        hal::user::main_pie_base()
    } else {
        0
    };
    let main_loaded = load_image(&vm, &*img, main_bias, "exec")?;
    let exec_path = resolve_exec_path(task, path);

    let interp_loaded = if let Some(interp) = img.interpreter() {
        match load_interpreter_from_task_vfs(task, &exec_path, interp) {
            Ok(mut interp_bytes) => {
                hal::user::patch_interpreter_image(interp, &mut interp_bytes);
                let interp_img = elf::parse(&interp_bytes).map_err(|_| errno::Errno::ENOEXEC)?;
                validate_user_image_result(&*interp_img).map_err(|_| errno::Errno::ENOEXEC)?;
                Some(load_image(
                    &vm,
                    &*interp_img,
                    hal::user::interp_base(),
                    "interp",
                )?)
            }
            Err(errno::Errno::ENOENT) if elf_can_run_without_interpreter(&bytes) => None,
            Err(err) => return Err(err),
        }
    } else {
        None
    };

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
    vm.commit_segment(stack_bottom, stack_size, 0, &[], stack_flags)?;

    unsafe {
        let ops = user_pgd_ops().expect("[user] user_pgd_ops not registered");
        (ops.activate)(vm.pgd());
    }

    let entry_pc = interp_loaded
        .as_ref()
        .map(|interp| interp.entry)
        .unwrap_or(main_loaded.entry);
    let at_base = interp_loaded
        .as_ref()
        .map(|interp| interp.base)
        .unwrap_or(0);
    let creds = task.credentials();
    let user_sp = layout_user_stack(
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
    )?;

    Ok(LoadedUserImage {
        vm,
        entry_pc,
        user_sp,
        exec_path,
    })
}

fn layout_user_stack(
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
    let execfn_ptr = push_user_string(&mut sp, path.as_bytes());

    if argv.is_empty() {
        argv_ptrs.push(execfn_ptr);
    } else {
        for arg in argv.iter().rev() {
            let ptr = push_user_string(&mut sp, arg.as_bytes());
            argv_ptrs.push(ptr);
        }
        argv_ptrs.reverse();
    }
    for env in envp.iter().rev() {
        let ptr = push_user_string(&mut sp, env.as_bytes());
        envp_ptrs.push(ptr);
    }
    envp_ptrs.reverse();

    sp -= 16;
    let random_ptr = sp;
    unsafe {
        core::ptr::write_unaligned(random_ptr as *mut u64, 0x6d79676f5f726e64);
        core::ptr::write_unaligned((random_ptr + 8) as *mut u64, 0xfedcba9876543210);
    }

    sp &= !0xf;

    let auxv = [
        (AT_PHDR, main.phdr),
        (AT_PHENT, main.phent),
        (AT_PHNUM, main.phnum),
        (AT_PAGESZ, hal::memory::page_size()),
        (AT_BASE, at_base),
        (AT_ENTRY, main.entry),
        (AT_CLKTCK, 100),
        (AT_UID, uid as usize),
        (AT_EUID, euid as usize),
        (AT_GID, gid as usize),
        (AT_EGID, egid as usize),
        (AT_SECURE, 0),
        (AT_RANDOM, random_ptr),
        (AT_EXECFN, execfn_ptr),
        (AT_NULL, 0),
    ];

    let stack_slots = 1 + argv_ptrs.len() + 1 + envp_ptrs.len() + 1 + auxv.len() * 2;
    if stack_slots % 2 != 0 {
        sp -= 8;
        unsafe { core::ptr::write_unaligned(sp as *mut u64, 0) };
    }

    for (key, value) in auxv.iter().rev() {
        sp -= 16;
        unsafe {
            core::ptr::write_unaligned(sp as *mut u64, *key as u64);
            core::ptr::write_unaligned((sp + 8) as *mut u64, *value as u64);
        }
    }

    sp -= 8;
    unsafe { core::ptr::write_unaligned(sp as *mut u64, 0) };
    for ptr in envp_ptrs.iter().rev() {
        sp -= 8;
        unsafe { core::ptr::write_unaligned(sp as *mut u64, *ptr as u64) };
    }

    sp -= 8;
    unsafe { core::ptr::write_unaligned(sp as *mut u64, 0) };
    for ptr in argv_ptrs.iter().rev() {
        sp -= 8;
        unsafe { core::ptr::write_unaligned(sp as *mut u64, *ptr as u64) };
    }

    sp -= 8;
    unsafe { core::ptr::write_unaligned(sp as *mut u64, argc as u64) };

    Ok(sp)
}

fn push_user_string(sp: &mut usize, bytes: &[u8]) -> usize {
    *sp -= bytes.len() + 1;
    let ptr = *sp;
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        core::ptr::write((ptr + bytes.len()) as *mut u8, 0);
    }
    ptr
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

fn elf_can_run_without_interpreter(bytes: &[u8]) -> bool {
    let dynamic = match elf_dynamic_range(bytes) {
        Some(range) => range,
        None => return true,
    };

    let mut has_needed = false;
    let mut rela_size = 0u64;
    let mut rel_size = 0u64;
    let mut plt_rel_size = 0u64;
    let mut has_jmprel = false;

    for ent in bytes[dynamic].chunks_exact(ELF64_DYN_ENTRY_SIZE) {
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

fn elf_dynamic_range(bytes: &[u8]) -> Option<core::ops::Range<usize>> {
    if bytes.len() < ELF64_EHDR_SIZE {
        return None;
    }
    let phoff = usize::try_from(read_u64_at(bytes, ELF64_EHDR_OFF_PHOFF)).ok()?;
    let phentsize = read_u16_at(bytes, ELF64_EHDR_OFF_PHENTSIZE) as usize;
    let phnum = read_u16_at(bytes, ELF64_EHDR_OFF_PHNUM) as usize;
    if phentsize < ELF64_PHDR_SIZE {
        return None;
    }
    for idx in 0..phnum {
        let off = phoff.checked_add(idx.checked_mul(phentsize)?)?;
        let end = off.checked_add(phentsize)?;
        if end > bytes.len() {
            return None;
        }
        let ph = &bytes[off..end];
        if read_u32_at(ph, ELF64_PHDR_OFF_TYPE) != ELF64_PT_DYNAMIC {
            continue;
        }
        let dyn_off = usize::try_from(read_u64_at(ph, ELF64_PHDR_OFF_OFFSET)).ok()?;
        let dyn_size = usize::try_from(read_u64_at(ph, ELF64_PHDR_OFF_FILESZ)).ok()?;
        let dyn_end = dyn_off.checked_add(dyn_size)?;
        if dyn_end > bytes.len() {
            return None;
        }
        return Some(dyn_off..dyn_end);
    }
    None
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
        log::info!("[user] validate: arch mismatch got={:?} expect={:?}", img.arch(), hal::platform::elf_arch());
        return Err(());
    }
    if img.load_vaddr_range().is_none() {
        log::info!("[user] validate: load_vaddr_range is None");
        return Err(());
    }
    Ok(())
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
    vm.init_brk_after_load(max_segment_end);
    Ok(LoadedImage {
        entry: load_bias
            .checked_add(img.entry())
            .ok_or(errno::Errno::ENOEXEC)?,
        base: load_bias,
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
) -> Result<Vec<u8>, errno::Errno> {
    match load_file_from_task_vfs(task, interp) {
        Ok(bytes) => return Ok(bytes),
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
        match load_file_from_task_vfs(task, &candidate) {
            Ok(bytes) => return Ok(bytes),
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

fn load_file_from_task_vfs(task: &Arc<Task>, path: &str) -> Result<Vec<u8>, errno::Errno> {
    let ctx = task_vfs_context(task)?;
    let tmp_fdt = FdTable::new_default();
    let flags = OpenOptions {
        access: AccessMode::ReadOnly,
        ..OpenOptions::default()
    };
    let fd = vfs::operation::openat(&ctx, &tmp_fdt, &Dirfd::Cwd, path, flags, FileMode::new(0))
        .map_err(|err| err.to_errno())?;
    let file = tmp_fdt.get_file(fd).ok_or(errno::Errno::EBADF)?;
    let size = file.stat().map_err(|e| e.to_errno())?.size as usize;
    if size == 0 {
        let _ = tmp_fdt.close_fd(fd);
        return Err(errno::Errno::ENOEXEC);
    }
    let mut bytes = Vec::new();
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
    let _ = tmp_fdt.close_fd(fd);
    Ok(bytes)
}
