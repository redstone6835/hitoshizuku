//! 用户态 trap frame 构造与进入封装。

/// 用户线程入口意外返回时使用的架构最小退出序列。
pub fn exit_stub_code() -> &'static [u8] {
    arch::user_exit_stub_code()
}

/// Linux 64 位 `epoll_event.data` 在当前架构上的字节偏移。
pub const fn linux_epoll_event_data_offset() -> usize {
    arch::linux_epoll_event_data_offset()
}

/// 默认用户栈顶（不含）。
#[kernel_symbols::export(name = "hal.user.default_stack_top", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
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

    #[cfg(target_arch = "x86_64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .default_stack_top
    }
}

/// 默认用户栈大小。
#[kernel_symbols::export(name = "hal.user.default_stack_size", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
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

    #[cfg(target_arch = "x86_64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .default_stack_size
    }
}

/// PIE 主程序默认装载基址。
#[kernel_symbols::export(name = "hal.user.main_pie_base", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
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

    #[cfg(target_arch = "x86_64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .main_pie_base
    }
}

/// ELF interpreter 默认装载基址。
#[kernel_symbols::export(name = "hal.user.interp_base", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
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

    #[cfg(target_arch = "x86_64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .interp_base
    }
}

/// vDSO 映射基地址。
#[kernel_symbols::export(name = "hal.user.vdso_base", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
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

    #[cfg(target_arch = "x86_64")]
    {
        general::mm::user_vm_layout()
            .expect("[hal] user VM layout is not registered")
            .vdso_base
    }
}

/// vDSO 数据页偏移。
#[kernel_symbols::export(name = "hal.user.vdso_data_page_offset", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn vdso_data_page_offset() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::VDSO_DATA_PAGE_OFFSET
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::VDSO_DATA_PAGE_OFFSET
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::VDSO_DATA_PAGE_OFFSET
    }
}

/// vDSO 第一页长度（ELF header + text）。
#[kernel_symbols::export(name = "hal.user.vdso_text_page_len", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn vdso_text_page_len() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::VDSO_TEXT_PAGE_SIZE
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::VDSO_TEXT_PAGE_SIZE
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::VDSO_TEXT_PAGE_SIZE
    }
}

/// vDSO 总映射长度。
#[kernel_symbols::export(name = "hal.user.vdso_total_size", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn vdso_total_size() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::VDSO_TOTAL_SIZE
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::VDSO_TOTAL_SIZE
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::VDSO_TOTAL_SIZE
    }
}

/// 生成 vDSO ELF 镜像字节。
#[kernel_symbols::export(name = "hal.user.vdso_image", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
pub fn vdso_image() -> alloc::vec::Vec<u8> {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::vdso_image()
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::vdso_image()
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::vdso_image()
    }
}

/// vDSO 中 sigreturn trampoline 的用户态虚拟地址。
#[kernel_symbols::export(name = "hal.user.sigreturn_entry_va", contract = "kernel.hal.user-layout@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn sigreturn_entry_va() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        vdso_base() + arch::loongarch64::vdso::sigreturn_entry_offset()
    }

    #[cfg(target_arch = "riscv64")]
    {
        vdso_base() + arch::riscv64::vdso::sigreturn_entry_offset()
    }

    #[cfg(target_arch = "x86_64")]
    {
        vdso_base() + arch::x86_64::vdso::sigreturn_entry_offset()
    }
}

/// 用户信号处理函数返回地址在栈上占用的前缀长度。
///
/// RISC-V/LoongArch 使用 link register，通用信号投递路径可以把 restorer
/// 放入该寄存器；x86-64 没有 link register，Linux ABI 则要求调用者栈顶
/// 先保存返回地址。因此这项差异必须留在 HAL，kernel 只消费尺寸和编码结果。
pub const fn signal_return_prefix_size() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        core::mem::size_of::<usize>()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// 从 signal handler 入口观察到的 `%rsp` 对齐余数。
///
/// x86-64 signal delivery synthesizes a call frame by placing the restorer at
/// the stack top.  SysV therefore requires the handler to observe
/// `RSP % 16 == 8`; link-register architectures have no stack return word.
pub const fn signal_handler_stack_entry_bias() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        core::mem::size_of::<usize>()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// 将架构需要的 signal restorer 栈前缀编码到用户帧缓冲区。
///
/// 返回实际写入的字节数；link-register 架构返回 0。调用者可以用返回值
/// 与 [`signal_return_prefix_size`] 做一致性断言，而不在 kernel 中引入
/// `target_arch` 分支。
pub fn encode_signal_return_prefix(restorer: usize, out: &mut [u8]) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let size = core::mem::size_of::<usize>();
        if out.len() < size {
            return 0;
        }
        out[..size].copy_from_slice(&restorer.to_ne_bytes());
        size
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (restorer, out);
        0
    }
}

/// 注册 LoongArch64 timer tick 时的 vDSO 数据页更新回调。
#[kernel_symbols::export(name = "hal.user.register_vdso_tick_hook", contract = "kernel.hal.user-hook@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 0)]
pub fn register_vdso_tick_hook(hook: fn(u64)) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::register_timer_tick_hook(hook);
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::register_timer_tick_hook(hook);
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::register_timer_tick_hook(hook);
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

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::register_net_poll_hook(hook);
    }
}

/// 注册 timer tick 时的 TTY 输入泵回调。
///
/// 终端控制字符需要在没有用户 read 调用时也能触发信号；该 hook 供 kernel
/// 把 VFS 兼容层的 TTY 行规程接入架构 timer 路径。
#[kernel_symbols::export(name = "hal.user.register_tty_poll_hook", contract = "kernel.hal.user-hook@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE, retained_args = 1 << 0)]
pub fn register_tty_poll_hook(hook: fn(u64)) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::vdso::register_tty_poll_hook(hook);
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::vdso::register_tty_poll_hook(hook);
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::vdso::register_tty_poll_hook(hook);
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
#[kernel_symbols::export(name = "hal.user.decode_clone_register_args", contract = "kernel.hal.user-abi@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn decode_clone_register_args(args: [usize; 6]) -> CloneRegisterArgs {
    #[cfg(target_arch = "loongarch64")]
    {
        CloneRegisterArgs {
            flags: args[0] as u64,
            stack: args[1],
            parent_tid: args[2],
            // musl LoongArch64 clone.s 调用内核的 old clone ABI：
            // flags, stack, parent_tidptr, child_tidptr, tls。
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

    #[cfg(target_arch = "x86_64")]
    {
        CloneRegisterArgs {
            flags: args[0] as u64,
            stack: args[1],
            parent_tid: args[2],
            child_tid: args[3],
            tls: args[4],
        }
    }
}

/// 在映射 ELF 解释器之前，给架构一个机会修复已知的用户空间 ABI 适配层。
#[kernel_symbols::export(name = "hal.user.patch_interpreter_image", contract = "kernel.hal.user-abi@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn patch_interpreter_image(interp: &str, bytes: &mut [u8]) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::loongarch64::patch_interpreter_image(interp, bytes);
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::riscv64::patch_interpreter_image(interp, bytes);
    }

    #[cfg(target_arch = "x86_64")]
    {
        arch::x86_64::patch_interpreter_image(interp, bytes);
    }
}

/// 启用 SUM（Supervisor User Memory access），允许 S-mode 直接访问 U 标记的页面。
///
/// RISC-V 专用；LoongArch 上为空操作。
pub unsafe fn enable_sum() {
    #[cfg(target_arch = "loongarch64")]
    {}

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

    #[cfg(target_arch = "x86_64")]
    {
        unsafe { x86_64_enter_user_mode(entry_pc, user_sp, arg0, kernel_stack_top) }
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

    let mut frame = arch::TrapFrame::default();
    let frame_ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
    <arch::Riscv64TaskOps as TaskOps>::init_user_trap_frame(frame_ptr, entry_pc, user_sp, arg0);
    // The naked resume stub consumes this field through `frame_ptr`, outside Rust's
    // alias analysis. Keep the ABI handoff explicit so the store cannot be elided.
    unsafe { core::ptr::addr_of_mut!(frame.kstack_top).write_volatile(kernel_stack_top) };

    unsafe { <arch::Riscv64TaskOps as TaskOps>::resume_to_trap_frame(frame_ptr) }
}

#[cfg(target_arch = "x86_64")]
unsafe fn x86_64_enter_user_mode(
    entry_pc: usize,
    user_sp: usize,
    arg0: usize,
    kernel_stack_top: usize,
) -> ! {
    use general::{TaskOps, TrapFramePtr};

    <arch::X86_64TaskOps as TaskOps>::set_kernel_trap_stack(kernel_stack_top);
    let mut frame = arch::TrapFrame::default();
    let frame_ptr = TrapFramePtr::new(&mut frame as *mut _ as usize);
    <arch::X86_64TaskOps as TaskOps>::init_user_trap_frame(frame_ptr, entry_pc, user_sp, arg0);
    frame.kernel_stack_top = kernel_stack_top;
    unsafe { <arch::X86_64TaskOps as TaskOps>::resume_to_trap_frame(frame_ptr) }
}
