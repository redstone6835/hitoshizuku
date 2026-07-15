#![no_std]
#![no_main]

extern crate alloc;
extern crate allocator;
extern crate hal;

use core::alloc::{GlobalAlloc, Layout};

mod acpi;
#[cfg(any(
    feature = "bench",
    feature = "block-bench",
    feature = "allocator-bench"
))]
mod bench;
mod device_init;
mod dtb;
mod elm;
mod initramfs;
mod integrated_components;
mod net_poll;
mod panic;
mod sched;
mod start;
mod stdio;
mod syscalls;
mod tty_poll;
mod user;
mod vdso;

struct KernelGlobalAllocator;

unsafe impl GlobalAlloc for KernelGlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Safety: GlobalAlloc 调用方保证 layout 合法，真实所有权由唯一内核分配器维护。
        unsafe { allocator::KERNEL_ALLOCATOR.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // Safety: GlobalAlloc 调用方保证 pointer/layout 来自同一个全局分配器。
        unsafe { allocator::KERNEL_ALLOCATOR.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Safety: GlobalAlloc 调用方保证旧分配及新尺寸满足 realloc 契约。
        unsafe { allocator::KERNEL_ALLOCATOR.realloc(pointer, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Safety: GlobalAlloc 调用方保证 layout 合法。
        unsafe { allocator::KERNEL_ALLOCATOR.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static KERNEL_GLOBAL_ALLOCATOR: KernelGlobalAllocator = KernelGlobalAllocator;

fn main() -> ! {
    log::debug!("[main] jumped into main()");
    hal::user::register_vdso_tick_hook(vdso::update_on_timer_tick);
    // 注册协议栈 tick 钩子——每个 timer tick 推一帧 `net::stack().poll()`，
    // 否则整个协议栈不会推进任何状态（RX 帧进不来、TCP 状态机不前进、
    // soft-remove 的 socket 永远占着槽位）。详见 [`net_poll`] 模块。
    net_poll::register();

    // ── 调度子系统：建立 init 任务，准备后续派生 ─────────────────────────────
    let init = sched::boot_init();
    let integrated = integrated_components::initialize_all()
        .unwrap_or_else(|error| panic!("[kernel] 集成组件初始化失败: {error}"));
    if integrated != 0 {
        log::info!(
            "[kernel] initialized {} integrated component(s)",
            integrated
        );
    }
    elm::init_builtin_mgr();
    // 注册 TTY 输入泵——控制字符不能依赖前台任务主动 read 终端，否则
    // `sleep` 这类程序运行时 Ctrl-C 会滞留在 UART FIFO。poller 需要
    // 调度器 init/idle 完成后才能派生内核线程。
    tty_poll::register();
    /*
    #[cfg(debug_assertions)]
    sched::smoketest();
    // mm / syscall 子系统烟雾测：在 sched 已就绪、ops 已注入之后跑一遍，
    // 验证 VmSpace 基本操作与 syscall 骨架可用。失败会 panic，kernel 直接挂。
    #[cfg(debug_assertions)]
    general::mm::smoketest::run();
    */

    #[cfg(any(feature = "kernel-tests", feature = "allocator-tests"))]
    {
        ktest::runner::set_writer(hal::console::early_write_bytes);
        let report = ktest::runner::run_all();
        let _ = report;
    }

    // ── 文件系统挂载 + 性能测试 ────────────────────────────────────────
    #[cfg(all(
        feature = "allocator-bench",
        not(any(feature = "bench", feature = "block-bench"))
    ))]
    bench::run_allocator_only();
    #[cfg(feature = "bench")]
    bench::run();
    #[cfg(feature = "block-bench")]
    bench::run_block_device();

    log::set_log_level(log::LogLevel::Info);
    sched::start_init_process(&init)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kernel_start_init(context: *const general::StartContext) -> ! {
    log::debug!("[main] jumped into __kernel_start_init()");
    let context = unsafe {
        context
            .as_ref()
            .expect("[main] __kernel_start_init received a null StartContext")
    };
    context
        .validate()
        .unwrap_or_else(|err| panic!("[main] invalid StartContext: {}", err));
    match context.firmware_source() {
        general::StartFirmwareSource::Acpi => acpi::kernel_start_init(context),
        general::StartFirmwareSource::Dtb => dtb::kernel_start_init(context),
    }
    main()
}
