#![no_std]
#![no_main]

extern crate alloc;
extern crate allocator;
extern crate hal;

use core::alloc::{GlobalAlloc, Layout};

mod acct;
mod acpi;
mod adjtimex;
#[cfg(any(
    feature = "bench",
    feature = "block-bench",
    feature = "allocator-bench"
))]
mod bench;
mod boot_root;
mod device_init;
mod dtb;
mod elm;
mod exec;
mod initramfs;
mod integrated_components;
#[cfg(feature = "kcsan")]
mod kcsan_runtime;
#[path = "native_abi/mod.rs"]
mod native_runtime;
mod net_runtime;
mod net_stack;
#[cfg(any(feature = "kernel-tests", feature = "network-tests"))]
mod net_tests;
mod ns;
mod panic;
mod rseq;
mod sched;
mod soyo;
mod start;
mod stdio;
mod syscalls;
#[cfg(any(feature = "kernel-tests", feature = "smp-tests"))]
mod tests;
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

#[cfg(any(
    feature = "kernel-tests",
    feature = "soyo-tests",
    feature = "network-tests",
    feature = "allocator-tests",
    feature = "smp-tests"
))]
const RUN_KTESTS_BEFORE_RUNTIME_COMPONENTS: bool = cfg!(all(
    feature = "allocator-tests",
    not(any(
        feature = "kernel-tests",
        feature = "soyo-tests",
        feature = "network-tests",
        feature = "smp-tests"
    ))
));

#[cfg(any(
    feature = "kernel-tests",
    feature = "soyo-tests",
    feature = "network-tests",
    feature = "allocator-tests",
    feature = "smp-tests"
))]
fn run_ktests() {
    ktest::runner::set_writer(hal::console::early_write_bytes);
    let _ = ktest::runner::run_all();
}

#[cfg(feature = "performance-profile")]
fn external_profile_counter(cpu: usize, event: profiling::Event) -> u64 {
    let urgent = ::sched::arch_hooks::urgent_profile_counter(cpu, event);
    let slab = match event {
        profiling::Event::SlabCacheHit => Some(allocator::SlabProfileCounter::CacheHit),
        profiling::Event::SlabCacheMiss => Some(allocator::SlabProfileCounter::CacheMiss),
        profiling::Event::SlabRefill => Some(allocator::SlabProfileCounter::Refill),
        profiling::Event::SlabFlush => Some(allocator::SlabProfileCounter::Flush),
        profiling::Event::SlabSlowPath => Some(allocator::SlabProfileCounter::SlowPath),
        _ => None,
    }
    .map_or(0, |counter| {
        allocator::KERNEL_ALLOCATOR.slab_profile_counter(cpu, counter)
    });
    urgent.saturating_add(slab)
}

fn main() -> ! {
    log::debug!("[main] jumped into main()");
    hal::user::register_vdso_tick_hook(vdso::update_on_timer_tick);
    vfs::stat::install_realtime_clock(vdso::realtime_ns);
    // ── 调度子系统：建立 init 任务，准备后续派生 ─────────────────────────────
    let init = sched::boot_init();
    #[cfg(feature = "performance-profile")]
    {
        profiling::install(
            hal::time::stable_counter_raw,
            ::sched::current_cpu_id,
            ::sched::current_task_cpu_time_ns,
            ::sched::current_task_id,
            ::sched::current_profile_span_id,
            ::sched::set_current_profile_span_id,
            hal::time::stable_counter_hz(),
        );
        profiling::install_task_session(::sched::current_profile_session_id);
        profiling::install_task_image(::sched::current_profile_image);
        profiling::install_external_event_counter(external_profile_counter);
    }
    let secondary_cpus = hal::sched::start_secondary_cpus();
    sched::install_firmware_topology();
    log::info!(
        "[smp] CPU startup complete: detected={} started={} failed={} online_mask={:#x} active_mask={:#x}",
        secondary_cpus.detected,
        secondary_cpus.started,
        secondary_cpus.failed,
        ::sched::online_cpu_mask(),
        ::sched::active_cpu_mask(),
    );
    #[cfg(any(
        feature = "kernel-tests",
        feature = "soyo-tests",
        feature = "network-tests",
        feature = "allocator-tests",
        feature = "smp-tests"
    ))]
    if RUN_KTESTS_BEFORE_RUNTIME_COMPONENTS {
        run_ktests();
    }
    #[cfg(feature = "kcsan")]
    {
        // AP 在建立架构 per-CPU 状态前也会经过已插桩代码。等全部 AP 完成
        // 启动后再启用检测器，避免调试延迟干扰启动超时或读取未就绪的 tp。
        kcsan_runtime::install();
        kcsan_runtime::start_reporter();
    }
    let network_boot_ready = device_init::install_network_boot_config();
    let integrated =
        integrated_components::initialize_phase(integrated_components::IntegratedPhase::Runtime)
            .unwrap_or_else(|error| panic!("[kernel] 集成组件初始化失败: {error}"));
    if integrated != 0 {
        log::info!(
            "[kernel] initialized {} integrated component(s)",
            integrated
        );
    }
    elm::init_builtin_mgr();
    let build_bound = elm::load_build_bound_modules(&init)
        .unwrap_or_else(|error| panic!("[kernel] BuildBound ELM 自动装载失败: {error}"));
    if build_bound != 0 {
        log::info!("[kernel] activated {} BuildBound ELM(s)", build_bound);
    }
    net_stack::start_host();
    // 网络 host 允许没有设备启动；网络密钥或 stack 配置不可用时，保持 host
    // 的无 stack 状态并继续启动 init，避免可选网络功能拖垮基本系统。
    if network_boot_ready {
        // BuildBound driver 已激活时会在此首次 attach，后续动态装载则通过 ELM
        // 管理路径触发 reconcile。
        net_runtime::start_workers();
    } else {
        log::warning!("[kernel] network runtime skipped; continuing without network workers");
    }
    if device_init::retry_deferred_boot_console(&init) {
        log::info!("[kernel] deferred boot console activated after BuildBound loading");
    }
    // 注册 TTY 输入泵——控制字符不能依赖前台任务主动 read 终端，否则
    // `sleep` 这类程序运行时 Ctrl-C 会滞留在 UART FIFO。poller 需要
    // 调度器 init/idle 完成后才能派生内核线程。
    tty_poll::register();

    elm::synchronize_smp_runtime();
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
        feature = "soyo-tests",
        feature = "network-tests",
        feature = "allocator-tests",
        feature = "smp-tests"
    ))]
    if !RUN_KTESTS_BEFORE_RUNTIME_COMPONENTS {
        run_ktests();
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
    let target_architecture = hal::platform::architecture_id();
    assert_eq!(
        context.boot.architecture, target_architecture,
        "[main] StartContext architecture does not match the compiled kernel"
    );
    general::set_start_architecture(context.boot.architecture);
    general::set_start_cmdline(context.boot.command_line);
    match context.firmware_source() {
        general::StartFirmwareSource::Acpi => acpi::kernel_start_init(context),
        general::StartFirmwareSource::Dtb => dtb::kernel_start_init(context),
    }
    main()
}
