//! 用户态 trap frame 构造与进入封装。

/// 默认用户栈顶（不含）。
pub fn default_stack_top() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .default_stack_top
    }

    #[cfg(target_arch = "riscv64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .default_stack_top
    }
}

/// 默认用户栈大小。
pub fn default_stack_size() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .default_stack_size
    }

    #[cfg(target_arch = "riscv64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .default_stack_size
    }
}

/// PIE 主程序默认装载基址。
pub fn main_pie_base() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .main_pie_base
    }

    #[cfg(target_arch = "riscv64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .main_pie_base
    }
}

/// ELF interpreter 默认装载基址。
pub fn interp_base() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .interp_base
    }

    #[cfg(target_arch = "riscv64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .interp_base
    }
}

/// vDSO 映射基地址。
pub fn vdso_base() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .vdso_base
    }

    #[cfg(target_arch = "riscv64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .vdso_base
    }
}

/// vDSO 数据页偏移。
pub fn vdso_data_page_offset() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::VDSO_DATA_PAGE_OFFSET
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::VDSO_DATA_PAGE_OFFSET
    }
}

/// vDSO 第一页长度（ELF header + text）。
pub fn vdso_text_page_len() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::VDSO_TEXT_PAGE_SIZE
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::VDSO_TEXT_PAGE_SIZE
    }
}

/// vDSO 总映射长度。
pub fn vdso_total_size() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::VDSO_TOTAL_SIZE
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::VDSO_TOTAL_SIZE
    }
}

/// 生成 vDSO ELF 镜像字节。
pub fn vdso_image() -> alloc::vec::Vec<u8> {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::vdso_image()
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::vdso_image()
    }
}

/// vDSO 中 sigreturn trampoline 的用户态虚拟地址。
pub fn sigreturn_entry_va() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        vdso_base() + arch::loongarch64::vdso::sigreturn_entry_offset()
    }

    #[cfg(target_arch = "riscv64")]
    {
        vdso_base() + arch::riscv64::vdso::sigreturn_entry_offset()
    }
}

/// 注册 LoongArch64 timer tick 时的 vDSO 数据页更新回调。
pub fn register_vdso_tick_hook(hook: fn(u64)) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::register_timer_tick_hook(hook);
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::register_timer_tick_hook(hook);
    }
}

/// 注册 LoongArch64 timer tick 时的网络协议栈 poll 回调。
///
/// 在 vDSO 的 timer tick hook 旁路加一个**独立**的钩子（同样接受
/// `now_ns: u64`）——避免把网络 poll 强塞到 vDSO hook 里引发循环依赖
/// （vDSO 路径应保持精简）。调用方应当只注册一次。
pub fn register_net_poll_hook(hook: fn(u64)) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::register_net_poll_hook(hook);
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::register_net_poll_hook(hook);
    }
}

/// 注册 timer tick 时的 TTY 输入泵回调。
///
/// 终端控制字符需要在没有用户 read 调用时也能触发信号；该 hook 供 kernel
/// 把 VFS 兼容层的 TTY 行规程接入架构 timer 路径。
pub fn register_tty_poll_hook(hook: fn(u64)) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::register_tty_poll_hook(hook);
    }

    #[cfg(target_arch = "riscv64")]
    {
        let _ = hook;
        todo!("riscv64 HAL tty poll hook is not implemented")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CloneRegisterArgs {
    pub flags: u64,
    pub stack: usize,
    pub parent_tid: usize,
    pub child_tid: usize,
    pub tls: usize,
}

/// 将传统的 clone(2) 寄存器参数解码为命名字段。
pub fn decode_clone_register_args(args: [usize; 6]) -> CloneRegisterArgs {
    #[cfg(target_arch = "loongarch64")]
    {
        CloneRegisterArgs {
            flags: args[0] as u64,
            stack: args[1],
            parent_tid: args[2],
            // clone: $a3=child_tid, $a4=tls
            child_tid: args[3],
            tls: args[4],
        }
    }

    #[cfg(target_arch = "riscv64")]
    {
        CloneRegisterArgs {
            flags: args[0] as u64,
            stack: args[1],
            parent_tid: args[2],
            child_tid: args[4],
            tls: args[3],
        }
    }
}

/// 在映射 ELF 解释器之前，给架构一个机会修复已知的用户空间 ABI 适配层。
pub fn patch_interpreter_image(interp: &str, bytes: &mut [u8]) {
    #[cfg(target_arch = "loongarch64")]
    {
        loongarch64_patch_interpreter_image(interp, bytes);
    }

    #[cfg(target_arch = "riscv64")]
    {
        let _ = (interp, bytes);
    }
}

/// 启用 SUM（Supervisor User Memory access），允许 S-mode 直接访问 U 标记的页面。
///
/// RISC-V 专用；LoongArch 上为空操作。
pub unsafe fn enable_sum() {
    #[cfg(target_arch = "loongarch64")]
    {
    }

    #[cfg(target_arch = "riscv64")]
    unsafe {
        arch::riscv64::set_sum();
    }
}

/// 构造当前架构的用户 trap frame，并切入用户态执行。
///
/// # Safety
///
/// `kernel_stack_top` 必须是当前任务持有的内核栈顶；`entry_pc` 与 `user_sp`
/// 必须指向当前已激活用户地址空间中的合法入口和用户栈。
pub unsafe fn enter_user_mode(
    entry_pc: usize,
    user_sp: usize,
    arg0: usize,
    kernel_stack_top: usize,
) -> ! {
    #[cfg(target_arch = "loongarch64")]
    {
        // Safety: 由本函数的调用契约保证。
        unsafe { loongarch64_enter_user_mode(entry_pc, user_sp, arg0, kernel_stack_top) }
    }

    #[cfg(target_arch = "riscv64")]
    {
        unsafe { riscv64_enter_user_mode(entry_pc, user_sp, arg0, kernel_stack_top) }
    }
}

#[cfg(target_arch = "loongarch64")]
unsafe fn loongarch64_enter_user_mode(
    entry_pc: usize,
    user_sp: usize,
    arg0: usize,
    kernel_stack_top: usize,
) -> ! {
    use general::{TaskOps, TrapFramePtr};

    <arch::LoongArch64TaskOps as TaskOps>::set_kernel_trap_stack(kernel_stack_top);

    let mut frame = arch::TrapFrame::default();
    let frame_ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
    <arch::LoongArch64TaskOps as TaskOps>::init_user_trap_frame(frame_ptr, entry_pc, user_sp, arg0);

    // Safety: `frame` remains alive until `resume_to_trap_frame` switches to user mode
    // and never returns to this stack frame.
    unsafe { <arch::LoongArch64TaskOps as TaskOps>::resume_to_trap_frame(frame_ptr) }
}

#[cfg(target_arch = "riscv64")]
unsafe fn riscv64_enter_user_mode(
    entry_pc: usize,
    user_sp: usize,
    arg0: usize,
    kernel_stack_top: usize,
) -> ! {
    use general::{TaskOps, TrapFramePtr};

    <arch::Riscv64TaskOps as TaskOps>::set_kernel_trap_stack(kernel_stack_top);

    let mut frame = arch::TrapFrame::default();
    let frame_ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
    <arch::Riscv64TaskOps as TaskOps>::init_user_trap_frame(frame_ptr, entry_pc, user_sp, arg0);

    unsafe { <arch::Riscv64TaskOps as TaskOps>::resume_to_trap_frame(frame_ptr) }
}

#[cfg(target_arch = "loongarch64")]
fn loongarch64_patch_interpreter_image(interp: &str, bytes: &mut [u8]) {
    if !interp_basename(interp).starts_with("ld-musl-") {
        return;
    }

    const SCHED_STUBS: [(&[u8], u16); 4] = [
        (b"sched_setparam", 118),
        (b"sched_setscheduler", 119),
        (b"sched_getscheduler", 120),
        (b"sched_getparam", 121),
    ];

    for (name, nr) in SCHED_STUBS {
        let Some(off) = elf64_dynsym_file_offset(bytes, name, 16) else {
            continue;
        };
        patch_loongarch64_enosys_stub(bytes, off, nr);
    }
}

#[cfg(target_arch = "loongarch64")]
fn patch_loongarch64_enosys_stub(bytes: &mut [u8], off: usize, nr: u16) {
    const ENOSYS_STUB_PREFIX: [u8; 12] = [
        0x63, 0xc0, 0xff, 0x02, // addi.d sp, sp, -16
        0x04, 0x68, 0xbf, 0x02, // li.w a0, -38
        0x61, 0x20, 0xc0, 0x29, // st.d ra, sp, 8
    ];
    let Some(prefix) = bytes.get(off..off + ENOSYS_STUB_PREFIX.len()) else {
        return;
    };
    if prefix != ENOSYS_STUB_PREFIX {
        return;
    }

    let mut patch = [0u8; 16];
    write_u32(&mut patch, 0, 0x0280_000b | ((nr as u32) << 10)); // li.w a7, nr
    write_u32(&mut patch, 4, 0x002b_0000); // syscall 0
    write_u32(&mut patch, 8, 0x0040_8084); // slli.w a0, a0, 0
    write_u32(&mut patch, 12, 0x4c00_0020); // ret
    bytes[off..off + patch.len()].copy_from_slice(&patch);
}

#[cfg(target_arch = "loongarch64")]
fn interp_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(target_arch = "loongarch64")]
fn elf64_dynsym_file_offset(bytes: &[u8], name: &[u8], patch_len: usize) -> Option<usize> {
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(4)? != 2 || *bytes.get(5)? != 1 {
        return None;
    }

    const EHDR_OFF_PHOFF: usize = 0x20;
    const EHDR_OFF_SHOFF: usize = 0x28;
    const EHDR_OFF_PHENTSIZE: usize = 0x36;
    const EHDR_OFF_PHNUM: usize = 0x38;
    const EHDR_OFF_SHENTSIZE: usize = 0x3a;
    const EHDR_OFF_SHNUM: usize = 0x3c;
    const SHDR_TYPE_DYNSYM: u32 = 11;
    const SHDR_OFF_TYPE: usize = 0x04;
    const SHDR_OFF_OFFSET: usize = 0x18;
    const SHDR_OFF_SIZE: usize = 0x20;
    const SHDR_OFF_LINK: usize = 0x28;
    const SHDR_OFF_ENTSIZE: usize = 0x38;
    const SYM_OFF_NAME: usize = 0x00;
    const SYM_OFF_VALUE: usize = 0x08;

    let shoff = read_u64(bytes, EHDR_OFF_SHOFF)? as usize;
    let shentsize = read_u16(bytes, EHDR_OFF_SHENTSIZE)? as usize;
    let shnum = read_u16(bytes, EHDR_OFF_SHNUM)? as usize;
    if shoff == 0 || shentsize < 64 || shnum == 0 {
        return None;
    }

    for idx in 0..shnum {
        let sh = shoff.checked_add(idx.checked_mul(shentsize)?)?;
        if read_u32(bytes, sh + SHDR_OFF_TYPE)? != SHDR_TYPE_DYNSYM {
            continue;
        }

        let sym_off = read_u64(bytes, sh + SHDR_OFF_OFFSET)? as usize;
        let sym_size = read_u64(bytes, sh + SHDR_OFF_SIZE)? as usize;
        let sym_entsize = read_u64(bytes, sh + SHDR_OFF_ENTSIZE)? as usize;
        let str_idx = read_u32(bytes, sh + SHDR_OFF_LINK)? as usize;
        if sym_entsize < 24 || str_idx >= shnum {
            continue;
        }

        let str_sh = shoff.checked_add(str_idx.checked_mul(shentsize)?)?;
        let str_off = read_u64(bytes, str_sh + SHDR_OFF_OFFSET)? as usize;
        let str_size = read_u64(bytes, str_sh + SHDR_OFF_SIZE)? as usize;
        let count = sym_size / sym_entsize;

        for sym_idx in 0..count {
            let sym = sym_off.checked_add(sym_idx.checked_mul(sym_entsize)?)?;
            let name_off = read_u32(bytes, sym + SYM_OFF_NAME)? as usize;
            if !elf_str_eq(bytes, str_off, str_size, name_off, name) {
                continue;
            }
            let value = read_u64(bytes, sym + SYM_OFF_VALUE)? as usize;
            return elf64_vaddr_to_file_offset(
                bytes,
                read_u64(bytes, EHDR_OFF_PHOFF)? as usize,
                read_u16(bytes, EHDR_OFF_PHENTSIZE)? as usize,
                read_u16(bytes, EHDR_OFF_PHNUM)? as usize,
                value,
                patch_len,
            );
        }
    }

    None
}

#[cfg(target_arch = "loongarch64")]
fn elf64_vaddr_to_file_offset(
    bytes: &[u8],
    phoff: usize,
    phentsize: usize,
    phnum: usize,
    vaddr: usize,
    len: usize,
) -> Option<usize> {
    const PT_LOAD: u32 = 1;
    const PHDR_OFF_TYPE: usize = 0x00;
    const PHDR_OFF_OFFSET: usize = 0x08;
    const PHDR_OFF_VADDR: usize = 0x10;
    const PHDR_OFF_FILESZ: usize = 0x20;

    if phoff == 0 || phentsize < 56 || phnum == 0 {
        return None;
    }
    let vend = vaddr.checked_add(len)?;

    for idx in 0..phnum {
        let ph = phoff.checked_add(idx.checked_mul(phentsize)?)?;
        if read_u32(bytes, ph + PHDR_OFF_TYPE)? != PT_LOAD {
            continue;
        }
        let file_off = read_u64(bytes, ph + PHDR_OFF_OFFSET)? as usize;
        let seg_vaddr = read_u64(bytes, ph + PHDR_OFF_VADDR)? as usize;
        let file_size = read_u64(bytes, ph + PHDR_OFF_FILESZ)? as usize;
        let file_end_vaddr = seg_vaddr.checked_add(file_size)?;
        if vaddr < seg_vaddr || vend > file_end_vaddr {
            continue;
        }
        return file_off.checked_add(vaddr - seg_vaddr);
    }

    None
}

#[cfg(target_arch = "loongarch64")]
fn elf_str_eq(bytes: &[u8], str_off: usize, str_size: usize, name_off: usize, name: &[u8]) -> bool {
    if name_off >= str_size {
        return false;
    }
    let Some(start) = str_off.checked_add(name_off) else {
        return false;
    };
    let Some(max_end) = str_off.checked_add(str_size) else {
        return false;
    };
    let Some(end) = start.checked_add(name.len()) else {
        return false;
    };
    if end >= max_end {
        return false;
    }
    bytes.get(start..end) == Some(name) && bytes.get(end) == Some(&0)
}

#[cfg(target_arch = "loongarch64")]
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(off..off + 2)?.try_into().ok()?,
    ))
}

#[cfg(target_arch = "loongarch64")]
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(off..off + 4)?.try_into().ok()?,
    ))
}

#[cfg(target_arch = "loongarch64")]
fn read_u64(bytes: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(off..off + 8)?.try_into().ok()?,
    ))
}

#[cfg(target_arch = "loongarch64")]
fn write_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}
