#![no_std]
#![no_main]

extern crate alloc;
extern crate allocator;
extern crate hal;

mod acpi;
#[cfg(any(
    feature = "bench",
    feature = "block-bench",
    feature = "allocator-bench"
))]
mod bench;
mod device_init;
mod dtb;
mod initramfs;
mod net_runtime;
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
mod net_tests;
mod panic;
mod sched;
mod start;
mod stdio;
mod syscalls;
mod tty_poll;
mod user;
mod vdso;

fn main() -> ! {
    log::debug!("[main] jumped into main()");
    hal::user::register_vdso_tick_hook(vdso::update_on_timer_tick);
    // ── 调度子系统：建立 init 任务，准备后续派生 ─────────────────────────────
    let init = sched::boot_init();
    // PnP 早于调度器接管的网络 queue 在这里统一创建固定 affinity worker。
    net_runtime::start_workers();
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

    #[cfg(any(
        feature = "kernel-tests",
        feature = "network-tests",
        feature = "allocator-tests"
    ))]
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
