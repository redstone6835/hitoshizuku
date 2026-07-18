//! RISC-V64 引导加载（QEMU 直启路径）。
//!
//! 本模块负责 QEMU 直启路径的早期初始化，由汇编引导代码在完成 BSS 清零和
//! 临时栈建立之后跳转至此。RISC-V QEMU virt 机器通过 OpenSBI 启动，不经过
//! UEFI，固件信息完全来自 DTB。
//!
//! ```text
//! 启动流程（__kernel_arch_loader）：
//!
//!   ┌─────────────────────┐
//!   │ install trap vector  │  使 panic 可输出 backtrace
//!   ├─────────────────────┤
//!   │ register timestamp   │  日志带时间
//!   ├─────────────────────┤
//!   │ bind log sink        │  UART 可打印
//!   ├─────────────────────┤
//!   │ init boot allocator  │  后续可 alloc
//!   ├─────────────────────┤
//!   │ snapshot DTB         │  固件信息持久化
//!   ├─────────────────────┤
//!   │ build StartContext   │  统一接口
//!   ├─────────────────────┤
//!   │ → __kernel_start_init│  不返回
//!   └─────────────────────┘
//! ```

use core::cell::UnsafeCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::riscv64::early_console;
use crate::riscv64::heap_vm;
use crate::riscv64::sbi;
use crate::riscv64::specific::{kernel_timestamp_ns, phys_to_virt, virt_to_phys};
use crate::riscv64::time;
use crate::riscv64::trap;
use general::dtb::Dtb;
use general::{
    StartAddressOps, StartAllocatorOps, StartArchitecture, StartBootInfo, StartBootProtocol,
    StartContext, StartFirmware, StartMemory, StartMemoryMap, StartMemoryRegion,
    StartMemoryRegionKind, StartPhysRange,
};

// ── DTB 访问 ──────────────────────────────────────────────────────────────────

/// DTB 快照缓冲区容量（4 MiB），仅当 DTB 在 MMIO 区域时使用。
const DTB_BUF_SIZE: usize = 4096 * 1024;

/// DTB 快照的有效长度，0 表示使用零拷贝路径。
static DTB_VALID_LEN: AtomicUsize = AtomicUsize::new(0);

/// DTB 快照缓冲区的 Sync 包装（UnsafeCell 本身非 Sync）。
struct DtbBuffer(UnsafeCell<[u8; DTB_BUF_SIZE]>);
unsafe impl Sync for DtbBuffer {}
static DTB_BUFFER: DtbBuffer = DtbBuffer(UnsafeCell::new([0u8; DTB_BUF_SIZE]));

/// 启动 hart ID，由 loader 设置后供 allocator 的 cpu_id 回调使用。
static HART_ID: AtomicUsize = AtomicUsize::new(0);

/// DTB 中的 RAM 最终会与此启动映射求交集。这样在动态 direct map 落地前，
/// 物理分配器不会拿到当前页表无法访问的 4 GiB 以上页面。
static RISCV_BOOT_MEMORY_MAP: [StartMemoryRegion; 1] = [StartMemoryRegion::new(
    StartPhysRange::new(
        heap_vm::KERNEL_DIRECT_MAP_PHYS_START,
        heap_vm::KERNEL_DIRECT_MAP_PHYS_END,
    ),
    StartMemoryRegionKind::UsableRam,
    0,
)];

/// 返回内核 DTB 视图（始终从内核缓冲区读取）。
fn kernel_dtb() -> Option<Dtb<'static>> {
    let len = DTB_VALID_LEN.load(Ordering::Acquire);
    if len == 0 {
        return None;
    }
    let slice = unsafe { core::slice::from_raw_parts(DTB_BUFFER.0.get().cast::<u8>(), len) };
    Dtb::from_bytes(slice)
}

/// 将固件提供的 DTB 拷贝到内核静态缓冲区。
///
/// 所有地址范围统一走拷贝路径，不保留零拷贝引用——DTB 原址在 buddy
/// allocator 接管后会被覆盖，零拷贝会导致 `&str`/`&[u8]` 引用失效。
fn store_kernel_dtb(dtb_paddr: usize) -> Result<Dtb<'static>, &'static str> {
    if dtb_paddr == 0 {
        return Err("missing DTB address");
    }

    // 0x4000_0000..0x8000_0000：不在任何映射内
    if dtb_paddr >= 0x4000_0000 && dtb_paddr < 0x8000_0000 {
        return Err("DTB at unsupported address (not in identity map or RAM)");
    }

    let vaddr = if dtb_paddr >= 0x8000_0000 {
        phys_to_virt(dtb_paddr)
    } else {
        dtb_paddr // MMIO 区域走 identity mapping
    };

    let fw_dtb = unsafe { Dtb::from_ptr(vaddr) }.ok_or("invalid DTB magic")?;
    let bytes = fw_dtb.as_bytes();
    if bytes.len() > DTB_BUF_SIZE {
        return Err("DTB too large for kernel buffer");
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            DTB_BUFFER.0.get().cast::<u8>(),
            bytes.len(),
        );
    }
    DTB_VALID_LEN.store(bytes.len(), Ordering::Release);
    kernel_dtb().ok_or("DTB copy verification failed")
}

// ── 日志 sink ─────────────────────────────────────────────────────────────────

/// 行缓冲区：格式化一条日志记录后整行输出到早期串口。
struct LineBuf {
    buf: [u8; 1280],
    len: usize,
}

impl LineBuf {
    const fn new() -> Self {
        Self {
            buf: [0u8; 1280],
            len: 0,
        }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl fmt::Write for LineBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let avail = self.buf.len() - self.len;
        if avail == 0 {
            return Ok(());
        }
        let n = s.len().min(avail);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        // 截断标记：如果写满了且输入还有剩余，在末尾覆盖 "..."
        if n < s.len() && self.len >= 3 {
            self.buf[self.len - 3..self.len].copy_from_slice(b"...");
        }
        Ok(())
    }
}

/// 格式化日志行到行缓冲，格式：`[secs.micros] message`。
fn format_log_line(record: &log::LogRecord<'_>) -> LineBuf {
    let (secs, nanos) = log::format_timestamp(record.timestamp);
    let mut buf = LineBuf::new();
    let _ = writeln!(
        &mut buf,
        "[{:6}.{:06}] {}",
        secs,
        nanos / 1000,
        record.message
    );
    buf
}

// ── ISA 扩展检测 ──────────────────────────────────────────────────────────────

/// 从 DTB 的 `/cpus/cpu@*` 节点解析 `riscv,isa` 属性，检测硬件扩展支持。
fn detect_isa_extensions(dtb: &Dtb<'_>) {
    use crate::riscv64::specific::{CBO_BLOCK_SIZE, HAS_ZICBOZ};
    use core::sync::atomic::Ordering;

    let root = match dtb.root() {
        Some(r) => r,
        None => return,
    };
    let cpus = match root.find_child("cpus") {
        Some(c) => c,
        None => return,
    };
    // 取第一个 cpu 节点的 riscv,isa 属性
    for cpu_node in cpus.children() {
        if !cpu_node.base_name_bytes().starts_with(b"cpu") {
            continue;
        }
        if let Some(isa_prop) = cpu_node.find_property("riscv,isa") {
            let isa_bytes = isa_prop.value();
            // riscv,isa 值是以 NUL 结尾的字符串
            if contains_extension(isa_bytes, b"zicboz") {
                HAS_ZICBOZ.store(true, Ordering::Release);
                log::info!("[loader] ISA: Zicboz detected");
            }
            if contains_extension(isa_bytes, b"sstc") {
                crate::riscv64::time::set_sstc_available(true);
                log::info!("[loader] ISA: Sstc detected; timer uses stimecmp");
            }
            crate::riscv64::vector::detect_vector_from_isa(isa_bytes);
            // 检查 riscv,cboz-block-size（如果有）
            if let Some(bs_prop) = cpu_node.find_property("riscv,cboz-block-size") {
                let val = bs_prop.value();
                if val.len() >= 4 {
                    let bs = u32::from_be_bytes([val[0], val[1], val[2], val[3]]) as usize;
                    if bs.is_power_of_two() && bs >= 16 {
                        CBO_BLOCK_SIZE.store(bs, Ordering::Release);
                    }
                }
            }
            break; // 只需检测一个 hart，假设所有 hart 同构
        }
    }
}

/// 检查 ISA 字符串中是否包含指定扩展名（不区分大小写）。
fn contains_extension(isa: &[u8], ext: &[u8]) -> bool {
    // ISA 字符串格式："rv64imafdc_zicboz_zicbom..."
    // 扩展由 '_' 分隔（multi-letter extensions）
    let isa_len = isa.iter().position(|&b| b == 0).unwrap_or(isa.len());
    let isa = &isa[..isa_len];
    for chunk in isa.split(|&b| b == b'_') {
        if chunk.eq_ignore_ascii_case(ext) {
            return true;
        }
    }
    false
}

/// 读取 DTB 属性开头的大端 `u32`；不足 4 字节时返回 `None`。
fn read_be_u32_prop(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.get(..4)?.try_into().ok()?))
}

fn read_cells(value: &[u8], cells: usize) -> Option<usize> {
    if cells == 0 || cells > 2 || value.len() < cells * 4 {
        return None;
    }
    let mut result = 0u64;
    for chunk in value[..cells * 4].chunks_exact(4) {
        result = (result << 32) | u32::from_be_bytes(chunk.try_into().ok()?) as u64;
    }
    usize::try_from(result).ok()
}

fn dtb_cstr(value: &[u8]) -> &[u8] {
    let end = value
        .iter()
        .position(|&byte| byte == 0 || byte == b':')
        .unwrap_or(value.len());
    &value[..end]
}

fn find_dtb_node_by_absolute_path<'a>(
    dtb: &Dtb<'a>,
    path: &[u8],
) -> Option<(general::dtb::DtbNode<'a>, general::dtb::DtbNode<'a>, bool)> {
    if path.first().copied() != Some(b'/') {
        return None;
    }

    let root = dtb.root()?;
    let mut parent = root;
    let mut current = root;
    let mut depth = 0usize;
    for component in path[1..].split(|&byte| byte == b'/') {
        if component.is_empty() {
            continue;
        }
        parent = current;
        current = current
            .children()
            .find(|child| child.name_bytes() == component)?;
        depth += 1;
    }
    Some((parent, current, depth == 1))
}

fn dtb_compatible_contains(node: general::dtb::DtbNode<'_>, expected: &[u8]) -> bool {
    node.find_property("compatible").is_some_and(|property| {
        property
            .value()
            .split(|&byte| byte == 0)
            .any(|value| value == expected)
    })
}

/// 在完整 platform parser 接管前，仅解析 early console 所需的 stdout-path 与首个 reg。
/// 对带非空 bus `ranges` 的复杂地址转换保持 QEMU fallback，避免在 arch 重复通用解析器。
fn configure_early_console_from_dtb(dtb: &Dtb<'static>) {
    let Some(root) = dtb.root() else {
        return;
    };
    let Some(chosen) = root.find_child("chosen") else {
        return;
    };
    let Some(stdout) = chosen.find_property("stdout-path") else {
        return;
    };

    let mut path = dtb_cstr(stdout.value());
    if path.first().copied() != Some(b'/') {
        let Some(alias_name) = core::str::from_utf8(path).ok() else {
            return;
        };
        let Some(aliases) = root.find_child("aliases") else {
            return;
        };
        let Some(alias) = aliases.find_property(alias_name) else {
            return;
        };
        path = dtb_cstr(alias.value());
    }

    let Some((parent, serial, parent_is_root)) = find_dtb_node_by_absolute_path(dtb, path) else {
        return;
    };
    if !dtb_compatible_contains(serial, b"ns16550a")
        && !dtb_compatible_contains(serial, b"ns16550")
        && !dtb_compatible_contains(serial, b"uart16550")
    {
        log::warning!("[loader] DTB stdout is not NS16550-compatible; keeping QEMU fallback");
        return;
    }
    // 根节点的 reg 已经是 CPU 物理地址；子总线只有空 ranges 才明确表示恒等映射。
    // 缺失 ranges 表示地址不可向父总线转换，非空 ranges 则需要完整 cell 翻译。
    if !parent_is_root
        && !parent
            .find_property("ranges")
            .is_some_and(|ranges| ranges.value().is_empty())
    {
        log::warning!(
            "[loader] early console bus needs address translation; keeping QEMU fallback"
        );
        return;
    }

    let address_cells = parent
        .find_property("#address-cells")
        .and_then(|prop| read_be_u32_prop(prop.value()))
        .map(|value| value as usize)
        .unwrap_or(2);
    let Some(reg) = serial.find_property("reg") else {
        return;
    };
    let Some(phys_base) = read_cells(reg.value(), address_cells) else {
        return;
    };
    if early_console::configure_physical_base(phys_base) {
        log::info!("[loader] early console relocated from DTB to {phys_base:#x}");
    } else {
        log::warning!("[loader] DTB stdout UART {phys_base:#x} is outside the early MMIO window");
    }
}

/// 解析 RISC-V DTB 的 timebase-frequency，并启动周期性 S-mode timer。
fn configure_timer_from_dtb(dtb: &Dtb<'_>) {
    let hz = dtb
        .root()
        .and_then(|root| root.find_child("cpus"))
        .and_then(|cpus| {
            cpus.find_property("timebase-frequency")
                .and_then(|prop| read_be_u32_prop(prop.value()))
                .or_else(|| {
                    cpus.children()
                        .filter(|node| node.base_name_bytes().starts_with(b"cpu"))
                        .find_map(|cpu| {
                            cpu.find_property("timebase-frequency")
                                .and_then(|prop| read_be_u32_prop(prop.value()))
                        })
                })
        })
        .map(|hz| hz as usize)
        .filter(|&hz| hz != 0)
        .unwrap_or_else(|| time::STABLE_TIMER_HZ.load(Ordering::Relaxed));

    time::set_stable_counter_hz(hz);
    time::init_periodic_timer(time::DEFAULT_TIMER_HZ);
    log::info!(
        "[loader] timer configured: stable_hz={} tick_hz={} period_ticks={}",
        time::stable_counter_hz(),
        time::timer_hz(),
        time::timer_period_ticks()
    );
}

// ── 堆映射回调 ────────────────────────────────────────────────────────────────

/// allocator 扩展堆时调用，将虚拟地址映射到物理页。
fn map_kernel_heap(vaddr: usize, paddr: usize, size: usize, policy: allocator::PagePolicy) -> bool {
    match heap_vm::map_kernel_heap_range(vaddr, paddr, size, policy) {
        Ok(()) => true,
        Err(err) => {
            log::error!(
                "[loader][heap_vm] map failed vaddr={:#x} paddr={:#x} size={:#x} policy={:?}: {:?}",
                vaddr,
                paddr,
                size,
                policy,
                err
            );
            false
        }
    }
}

/// allocator 收缩堆时调用，解除虚拟地址映射。
fn unmap_kernel_heap(vaddr: usize, size: usize) -> bool {
    match heap_vm::unmap_kernel_heap_range(vaddr, size) {
        Ok(()) => true,
        Err(err) => {
            log::error!(
                "[loader][heap_vm] unmap failed vaddr={:#x} size={:#x}: {:?}",
                vaddr,
                size,
                err
            );
            false
        }
    }
}

// ── 主入口 ────────────────────────────────────────────────────────────────────

/// 内核架构加载器入口，由 `_start_virtualized` 以 tail-call 方式跳入。
///
/// # Safety
///
/// 仅由引导汇编调用一次，不得重入。
#[unsafe(no_mangle)]
pub extern "C" fn __kernel_arch_loader(hart_id: usize, dtb_addr: usize) -> ! {
    // 安装异常向量，使后续 panic 能输出 backtrace 而非直接挂死
    unsafe { trap::install_exception_entry() };

    // 注册时间戳源，让日志带上时间
    log::register_timestamp_source(kernel_timestamp_ns);

    // 绑定早期日志 sink：格式化后直接输出到 UART
    {
        fn sink_write(record: &log::LogRecord<'_>) {
            let line = format_log_line(record);
            early_console::e_write_bytes(line.as_bytes());
        }
        static SINK: log::LogSink = log::LogSink {
            write_record: sink_write,
        };
        log::bind_log_sink(&SINK);
    }
    log::info!(
        "[loader] RISC-V64 boot: hart={} dtb={:#x}",
        hart_id,
        dtb_addr
    );

    let sbi_info = sbi::init();
    log::info!(
        "[loader] SBI: base={} spec={:?} impl={:?}/{:?} srst={}",
        sbi_info.base_available,
        sbi_info.spec_version,
        sbi_info.implementation_id,
        sbi_info.implementation_version,
        sbi_info.srst_available
    );

    // 初始化引导期分配器
    {
        allocator::KERNEL_ALLOCATOR.bind_address_translation(phys_to_virt, virt_to_phys);

        HART_ID.store(hart_id, Ordering::Relaxed);
        fn get_cpu_id() -> usize {
            HART_ID.load(Ordering::Relaxed)
        }
        allocator::KERNEL_ALLOCATOR.bind_cpu_id(get_cpu_id);

        unsafe extern "C" {
            fn sheap();
            fn eheap();
        }
        let heap_start = sheap as usize;
        let heap_end = eheap as usize;
        allocator::KERNEL_ALLOCATOR.init_boot(heap_start, heap_end - heap_start);
        log::info!(
            "[loader] boot heap: {:#x}..{:#x} ({} MiB)",
            heap_start,
            heap_end,
            (heap_end - heap_start) / (1024 * 1024)
        );
    }

    // 快照 DTB 到内核缓冲区
    let dtb = store_kernel_dtb(dtb_addr).unwrap_or_else(|e| panic!("[loader] DTB: {}", e));
    log::info!("[loader] DTB: {} bytes", dtb.as_bytes().len());
    log::info!(
        "[loader] usable RAM handoff constrained to direct map {:#x}..{:#x}",
        heap_vm::KERNEL_DIRECT_MAP_PHYS_START,
        heap_vm::KERNEL_DIRECT_MAP_PHYS_END
    );

    configure_early_console_from_dtb(&dtb);

    // 检测 ISA 扩展（Zicboz 等）
    detect_isa_extensions(&dtb);

    // 启动 S-mode 周期 timer。sleep/调度 tick 依赖该中断推进。
    configure_timer_from_dtb(&dtb);
    trap::install_riscv_irq_line_ops();

    // 构造 StartContext 并移交控制权，从此不再返回
    unsafe {
        unsafe extern "C" {
            fn skernel();
            fn ekernel();
        }
        let kernel_phys = general::StartPhysRange::new(
            virt_to_phys(skernel as usize),
            virt_to_phys(ekernel as usize),
        );

        let context = StartContext {
            boot: StartBootInfo {
                architecture: StartArchitecture::new("riscv64"),
                protocol: StartBootProtocol::Direct,
                boot_cpu_id: hart_id,
                command_line: None,
            },
            firmware: StartFirmware::Dtb(dtb),
            memory: StartMemory {
                kernel_image: kernel_phys,
                boot_map: StartMemoryMap::Regions(&RISCV_BOOT_MEMORY_MAP),
            },
            address: StartAddressOps {
                phys_to_virt,
                virt_to_phys,
                device_mmio_to_virt: |pa| pa.wrapping_add(heap_vm::MMIO_VIRT_BASE),
            },
            allocator: Some(StartAllocatorOps {
                kernel_heap_region: heap_vm::kernel_heap_region,
                map_kernel_heap_range: map_kernel_heap,
                unmap_kernel_heap_range: unmap_kernel_heap,
                init_kernel_page_table: heap_vm::init_kernel_page_table,
            }),
        };

        unsafe extern "C" {
            fn __kernel_start_init(context: *const StartContext) -> !;
        }
        log::info!("[loader] → __kernel_start_init");
        __kernel_start_init(&context);
    }
}
