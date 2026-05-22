#![no_std]
#![no_main]

extern crate alloc;
extern crate allocator;
extern crate arch;
extern crate hal;

mod acpi;
mod bench;
mod cmdline;
mod dtb;
mod initramfs;
mod panic;
mod sched;
mod start;
mod stdio;
mod syscalls;
mod user;

fn main() -> ! {
    log::debug!("[main] jumped into main()");

    // ── 调度子系统：建立 init 任务，准备后续派生 ─────────────────────────────
    let init = sched::boot_init();

    // ── 文件系统挂载 + 性能测试 ────────────────────────────────────────
    // bench::run();

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
