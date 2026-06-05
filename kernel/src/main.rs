#![no_std]
#![no_main]

extern crate alloc;
extern crate allocator;
extern crate hal;

mod acpi;
#[cfg(feature = "bench")]
mod bench;
mod dtb;
mod initramfs;
mod panic;
mod sched;
mod start;
mod stdio;
mod syscalls;
mod user;
mod vdso;

fn main() -> ! {
    log::debug!("[main] jumped into main()");
    hal::user::register_vdso_tick_hook(vdso::update_on_timer_tick);

    // ── 调度子系统：建立 init 任务，准备后续派生 ─────────────────────────────
    let init = sched::boot_init();
    /*
    #[cfg(debug_assertions)]
    sched::smoketest();
    // mm / syscall 子系统烟雾测：在 sched 已就绪、ops 已注入之后跑一遍，
    // 验证 VmSpace 基本操作与 syscall 骨架可用。失败会 panic，kernel 直接挂。
    #[cfg(debug_assertions)]
    general::mm::smoketest::run();
    */

    #[cfg(feature = "kernel-tests")]
    {
        ktest::runner::set_writer(hal::console::early_write_bytes);
        let report = ktest::runner::run_all();
        let _ = report;
    }

    // ── 文件系统挂载 + 性能测试 ────────────────────────────────────────
    // bench::run();

    //log::set_log_level(log::LogLevel::Debug);
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
