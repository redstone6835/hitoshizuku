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
use crate::riscv64::paging_geometry::{RiscvPagingMode, common_paging_mode};
use crate::riscv64::sbi;
use crate::riscv64::specific::{current_cpu_id, kernel_timestamp_ns, phys_to_virt, virt_to_phys};
use crate::riscv64::time;
use crate::riscv64::trap;
use fdt::{
    AddressError as FdtAddressError, Fdt, Node, NodeId, PropertyError as FdtPropertyError,
    RiscvCpuBinding, RiscvCpuError, RiscvIsaSource, Tree, TreeError,
};
use general::{
    StartAddressOps, StartAllocatorOps, StartArchitecture, StartBootInfo, StartBootProtocol,
    StartContext, StartFirmware, StartMemory, StartMemoryMap, StartMemoryRegion,
    StartMemoryRegionKind, StartNoMapSupport, StartPhysRange,
};

// ── DTB 访问 ──────────────────────────────────────────────────────────────────

/// MMU 开启前复制 DTB 的固定缓冲区容量（4 MiB）。
pub(super) const DTB_BUF_SIZE: usize = 4096 * 1024;

/// DTB 快照的有效长度，0 表示尚未发布。
static DTB_VALID_LEN: AtomicUsize = AtomicUsize::new(0);

/// DTB 快照缓冲区的 Sync 包装（UnsafeCell 本身非 Sync）。
#[repr(align(4096))]
pub(super) struct DtbBuffer(UnsafeCell<[u8; DTB_BUF_SIZE]>);
// Safety: DTB_BUFFER 只由 boot hart 在 satp=0 时写入一次；Rust 代码发布长度后
// 只读访问。该 NOLOAD 区域位于 sbss 之前，不会被 clear_bss() 擦除。
unsafe impl Sync for DtbBuffer {}
#[unsafe(link_section = ".bss.prepage")]
pub(super) static DTB_BUFFER: DtbBuffer = DtbBuffer(UnsafeCell::new([0u8; DTB_BUF_SIZE]));

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
fn kernel_dtb() -> Option<Fdt<'static>> {
    let len = DTB_VALID_LEN.load(Ordering::Acquire);
    if len == 0 {
        return None;
    }
    // Safety: 有效长度只会在完整 DTB 已复制到固定容量缓冲区后发布，并且发布后
    // 缓冲区保持只读；len 不会超过 DTB_BUF_SIZE。
    let slice = unsafe { core::slice::from_raw_parts(DTB_BUFFER.0.get().cast::<u8>(), len) };
    Fdt::parse(slice).ok()
}

/// 校验并发布启动汇编已经复制的 DTB。
///
/// 复制发生在 MMU 开启前，所以无需为固件原址创建任何临时页表映射。
fn store_kernel_dtb(dtb_paddr: usize, snapshot_len: usize) -> Result<Fdt<'static>, &'static str> {
    if dtb_paddr == 0 {
        return Err("missing DTB address");
    }
    if !(32..=DTB_BUF_SIZE).contains(&snapshot_len) {
        return Err("invalid early DTB snapshot length");
    }
    // Safety: 启动汇编在 satp=0 时已按同一上限复制 snapshot_len 字节；
    // 目标区域属于内核镜像且不在 clear_bss() 的范围内。
    let bytes =
        unsafe { core::slice::from_raw_parts(DTB_BUFFER.0.get().cast::<u8>(), snapshot_len) };
    let fdt = Fdt::parse(bytes).map_err(|error| {
        log::error!("[loader] early DTB validation failed: {:?}", error);
        "invalid early DTB snapshot"
    })?;
    if fdt.as_bytes().len() != snapshot_len {
        return Err("early DTB snapshot length mismatch");
    }
    DTB_VALID_LEN.store(snapshot_len, Ordering::Release);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheBlockKind {
    Management,
    Zero,
    Prefetch,
}

impl CacheBlockKind {
    const fn extension(self) -> &'static str {
        match self {
            Self::Management => "zicbom",
            Self::Zero => "zicboz",
            Self::Prefetch => "zicbop",
        }
    }

    const fn property(self) -> &'static str {
        match self {
            Self::Management => "riscv,cbom-block-size",
            Self::Zero => "riscv,cboz-block-size",
            Self::Prefetch => "riscv,cbop-block-size",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RiscvHartFeatures {
    isa_source: RiscvIsaSource,
    mmu_type: &'static str,
    paging_mode: RiscvPagingMode,
    cbom_block_size: Option<usize>,
    cboz_block_size: Option<usize>,
    cbop_block_size: Option<usize>,
    sstc: bool,
    vector: bool,
}

#[derive(Clone, Copy, Debug)]
struct RiscvPlatformFeatures {
    harts: usize,
    split_isa_harts: usize,
    paging_mode: RiscvPagingMode,
    cbom_block_size: Option<usize>,
    cboz_block_size: Option<usize>,
    cbop_block_size: Option<usize>,
    sstc: bool,
    vector: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RiscvCpuConfigError {
    InvalidTree(TreeError),
    MissingCpus,
    NoAvailableCpus,
    MissingBootHart {
        hart_id: usize,
    },
    InvalidDeviceType {
        node: NodeId,
        error: FdtPropertyError,
    },
    InvalidReg {
        node: NodeId,
        error: FdtAddressError,
    },
    InvalidRegCount {
        node: NodeId,
        entries: usize,
    },
    HartIdOverflow {
        node: NodeId,
    },
    InvalidBinding {
        node: NodeId,
        error: RiscvCpuError,
    },
    UnsupportedIsaBase {
        node: NodeId,
    },
    UnsupportedMmuType {
        node: NodeId,
        mmu_type: &'static str,
    },
    MissingCacheBlockSize {
        node: NodeId,
        property: &'static str,
        extension: &'static str,
    },
    UnexpectedCacheBlockSize {
        node: NodeId,
        property: &'static str,
        extension: &'static str,
    },
    InvalidCacheBlockSize {
        node: NodeId,
        property: &'static str,
        size: u32,
    },
    HeterogeneousCacheBlockSize {
        property: &'static str,
        first: usize,
        other: usize,
    },
}

/// 严格解码所有可用 hart，只发布能在全 CPU 调度上安全使用的能力交集。
fn configure_cpu_features_from_dtb(
    dtb: &Fdt<'static>,
    boot_hart_id: usize,
) -> Result<RiscvPagingMode, RiscvCpuConfigError> {
    use crate::riscv64::specific::{
        CBO_BLOCK_SIZE, CBOM_BLOCK_SIZE, CBOP_BLOCK_SIZE, HAS_ZICBOM, HAS_ZICBOP, HAS_ZICBOZ,
    };

    let tree = Tree::from_fdt(*dtb).map_err(RiscvCpuConfigError::InvalidTree)?;
    let cpus = tree
        .find_node("/cpus")
        .ok_or(RiscvCpuConfigError::MissingCpus)?;
    let children = tree
        .children(cpus)
        .ok_or(RiscvCpuConfigError::MissingCpus)?;
    let mut aggregate: Option<RiscvPlatformFeatures> = None;
    let mut boot_mmu_type = None;

    for &node_id in children {
        let node = tree
            .node(node_id)
            .expect("Tree child NodeId must remain valid");
        if !is_riscv_cpu_node(node_id, node)?
            || !tree
                .is_available(node_id)
                .map_err(RiscvCpuConfigError::InvalidTree)?
        {
            continue;
        }
        let hart_id = cpu_hart_id(&tree, node_id)?;
        let features = riscv_hart_features(node_id, node)?;
        if hart_id == boot_hart_id as u64 {
            boot_mmu_type = Some(features.mmu_type);
        }
        merge_platform_features(&mut aggregate, features)?;
    }

    let aggregate = aggregate.ok_or(RiscvCpuConfigError::NoAvailableCpus)?;
    let boot_mmu_type = boot_mmu_type.ok_or(RiscvCpuConfigError::MissingBootHart {
        hart_id: boot_hart_id,
    })?;

    CBOM_BLOCK_SIZE.store(aggregate.cbom_block_size.unwrap_or(0), Ordering::Relaxed);
    CBOP_BLOCK_SIZE.store(aggregate.cbop_block_size.unwrap_or(0), Ordering::Relaxed);
    CBO_BLOCK_SIZE.store(aggregate.cboz_block_size.unwrap_or(0), Ordering::Relaxed);
    HAS_ZICBOM.store(aggregate.cbom_block_size.is_some(), Ordering::Release);
    HAS_ZICBOP.store(aggregate.cbop_block_size.is_some(), Ordering::Release);
    HAS_ZICBOZ.store(aggregate.cboz_block_size.is_some(), Ordering::Release);
    time::set_sstc_available(aggregate.sstc);
    crate::riscv64::vector::detect_vector_support(aggregate.vector);

    log::info!(
        "[loader] DT CPU binding: harts={} split-isa={} legacy-isa={} boot-mmu={} common-mmu={:?} cbom={} cboz={} cbop={} sstc={}",
        aggregate.harts,
        aggregate.split_isa_harts,
        aggregate.harts - aggregate.split_isa_harts,
        boot_mmu_type,
        aggregate.paging_mode,
        aggregate.cbom_block_size.unwrap_or(0),
        aggregate.cboz_block_size.unwrap_or(0),
        aggregate.cbop_block_size.unwrap_or(0),
        aggregate.sstc as usize,
    );
    Ok(aggregate.paging_mode)
}

fn is_riscv_cpu_node(node_id: NodeId, node: Node<'static>) -> Result<bool, RiscvCpuConfigError> {
    let device_type = node
        .property("device_type")
        .map(|property| {
            property
                .as_str()
                .map_err(|error| RiscvCpuConfigError::InvalidDeviceType {
                    node: node_id,
                    error,
                })
        })
        .transpose()?;
    Ok(node.base_name_bytes() == b"cpu" || device_type == Some("cpu"))
}

fn cpu_hart_id(tree: &Tree<'static>, node: NodeId) -> Result<u64, RiscvCpuConfigError> {
    let entries = tree
        .reg(node)
        .map_err(|error| RiscvCpuConfigError::InvalidReg { node, error })?;
    if entries.len() != 1 {
        return Err(RiscvCpuConfigError::InvalidRegCount {
            node,
            entries: entries.len(),
        });
    }
    u64::try_from(entries[0].address).map_err(|_| RiscvCpuConfigError::HartIdOverflow { node })
}

fn riscv_hart_features(
    node_id: NodeId,
    node: Node<'static>,
) -> Result<RiscvHartFeatures, RiscvCpuConfigError> {
    let binding =
        RiscvCpuBinding::parse(node).map_err(|error| RiscvCpuConfigError::InvalidBinding {
            node: node_id,
            error,
        })?;
    if binding.isa_base() != "rv64i" {
        return Err(RiscvCpuConfigError::UnsupportedIsaBase { node: node_id });
    }
    let paging_mode = RiscvPagingMode::from_mmu_type(binding.mmu_type()).ok_or(
        RiscvCpuConfigError::UnsupportedMmuType {
            node: node_id,
            mmu_type: binding.mmu_type(),
        },
    )?;

    Ok(RiscvHartFeatures {
        isa_source: binding.isa_source(),
        mmu_type: binding.mmu_type(),
        paging_mode,
        cbom_block_size: validate_cache_block(
            node_id,
            &binding,
            CacheBlockKind::Management,
            binding.cbom_block_size(),
        )?,
        cboz_block_size: validate_cache_block(
            node_id,
            &binding,
            CacheBlockKind::Zero,
            binding.cboz_block_size(),
        )?,
        cbop_block_size: validate_cache_block(
            node_id,
            &binding,
            CacheBlockKind::Prefetch,
            binding.cbop_block_size(),
        )?,
        sstc: binding.has_isa_extension("sstc"),
        vector: binding.has_isa_extension("v"),
    })
}

fn validate_cache_block(
    node: NodeId,
    binding: &RiscvCpuBinding<'_>,
    kind: CacheBlockKind,
    declared: Option<u32>,
) -> Result<Option<usize>, RiscvCpuConfigError> {
    let extension = kind.extension();
    let supported = binding.has_isa_extension(extension);
    let size = match (supported, declared) {
        (false, None) => return Ok(None),
        (false, Some(_)) => {
            return Err(RiscvCpuConfigError::UnexpectedCacheBlockSize {
                node,
                property: kind.property(),
                extension,
            });
        }
        (true, None) => {
            return Err(RiscvCpuConfigError::MissingCacheBlockSize {
                node,
                property: kind.property(),
                extension,
            });
        }
        (true, Some(size)) => size,
    };
    let native = size as usize;
    let valid = native.is_power_of_two()
        && (!matches!(kind, CacheBlockKind::Zero)
            || native <= allocator::PAGE_SIZE && allocator::PAGE_SIZE.is_multiple_of(native));
    if !valid {
        return Err(RiscvCpuConfigError::InvalidCacheBlockSize {
            node,
            property: kind.property(),
            size,
        });
    }
    Ok(Some(native))
}

fn merge_platform_features(
    aggregate: &mut Option<RiscvPlatformFeatures>,
    hart: RiscvHartFeatures,
) -> Result<(), RiscvCpuConfigError> {
    let Some(current) = aggregate.as_mut() else {
        *aggregate = Some(RiscvPlatformFeatures {
            harts: 1,
            split_isa_harts: usize::from(hart.isa_source == RiscvIsaSource::Split),
            paging_mode: hart.paging_mode,
            cbom_block_size: hart.cbom_block_size,
            cboz_block_size: hart.cboz_block_size,
            cbop_block_size: hart.cbop_block_size,
            sstc: hart.sstc,
            vector: hart.vector,
        });
        return Ok(());
    };

    current.harts += 1;
    current.split_isa_harts += usize::from(hart.isa_source == RiscvIsaSource::Split);
    current.paging_mode = common_paging_mode([current.paging_mode, hart.paging_mode])
        .expect("两个分页模式一定存在交集");
    current.cbom_block_size = intersect_cache_block_sizes(
        CacheBlockKind::Management,
        current.cbom_block_size,
        hart.cbom_block_size,
    )?;
    current.cboz_block_size = intersect_cache_block_sizes(
        CacheBlockKind::Zero,
        current.cboz_block_size,
        hart.cboz_block_size,
    )?;
    current.cbop_block_size = intersect_cache_block_sizes(
        CacheBlockKind::Prefetch,
        current.cbop_block_size,
        hart.cbop_block_size,
    )?;
    current.sstc &= hart.sstc;
    current.vector &= hart.vector;
    Ok(())
}

fn intersect_cache_block_sizes(
    kind: CacheBlockKind,
    first: Option<usize>,
    other: Option<usize>,
) -> Result<Option<usize>, RiscvCpuConfigError> {
    match (first, other) {
        (Some(first), Some(other)) if first != other => {
            Err(RiscvCpuConfigError::HeterogeneousCacheBlockSize {
                property: kind.property(),
                first,
                other,
            })
        }
        (Some(first), Some(_)) => Ok(Some(first)),
        _ => Ok(None),
    }
}

/// 读取 DTB 属性开头的大端 `u32`；不足 4 字节时返回 `None`。
fn read_be_u32_prop(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.get(..4)?.try_into().ok()?))
}

/// 解析 chosen 控制台的完整 16550 binding，并切换最早期输出配置。
///
/// 优先级：`/chosen/bootargs` 中的显式 `earlycon=` → `/chosen/stdout-path`。
/// cmdline 优先是因为直启场景下它表达用户明确意图；DT stdout-path 作为回退。
fn configure_early_console_from_dtb(dtb: &Fdt<'static>) {
    // 先尝试 bootargs 中的显式 earlycon=。
    let cmdline_earlycon =
        dtb.chosen_bootargs().ok().flatten().and_then(|bootargs| {
            general::cmdline::Cmdline::new(bootargs.as_bytes()).find("earlycon")
        });
    if let Some(value) = cmdline_earlycon {
        match early_console::configure_from_cmdline(value) {
            Ok(config) => {
                log::info!(
                    "[loader] early console from cmdline earlycon=: base={:#x} clock={} baud={} width={} endian={:?}",
                    config.phys_base,
                    config.clock_hz,
                    config.baud,
                    config.io_width.bytes(),
                    config.endian,
                );
                return;
            }
            Err(error) => log::warning!(
                "[loader] cmdline earlycon= rejected: {:?}; falling back to DTB stdout-path",
                error
            ),
        }
    }

    match early_console::configure_from_dtb(*dtb) {
        Ok(config) => log::info!(
            "[loader] early console from DTB: base={:#x} clock={} baud={} offset={:#x} shift={} width={} endian={:?}",
            config.phys_base,
            config.clock_hz,
            config.baud,
            config.reg_offset,
            config.reg_shift,
            config.io_width.bytes(),
            config.endian,
        ),
        Err(error) => log::warning!(
            "[loader] DTB early console config rejected: {:?}; keeping QEMU fallback",
            error
        ),
    }
}

/// 解析 RISC-V DTB 的 timebase-frequency，并配置周期性 S-mode timer。
fn configure_timer_from_dtb(dtb: &Fdt<'_>) {
    let hz = dtb
        .root()
        .find_child("cpus")
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

    let timer_hz = dtb
        .chosen_bootargs()
        .ok()
        .flatten()
        .and_then(|bootargs| general::cmdline::Cmdline::new(bootargs.as_bytes()).find("timer_hz"))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(time::DEFAULT_TIMER_HZ);
    time::set_stable_counter_hz(hz);
    if timer_hz == 0 {
        time::disable_periodic_timer();
    } else {
        time::init_periodic_timer(timer_hz);
    }
    log::info!(
        "[loader] timer configured: stable_hz={} tick_hz={} period_ticks={} disabled={}",
        time::stable_counter_hz(),
        time::timer_hz(),
        time::timer_period_ticks(),
        timer_hz == 0
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

fn protect_kernel_heap(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool {
    heap_vm::protect_kernel_heap_range(vaddr, size, read, write, execute).is_ok()
}

fn validate_kernel_heap(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool {
    heap_vm::validate_kernel_heap_range(vaddr, size, read, write, execute).is_ok()
}

fn sync_icache() {
    <crate::riscv64::task::Riscv64TaskOps as general::TaskOps>::sync_icache();
}

// ── 主入口 ────────────────────────────────────────────────────────────────────

/// 内核架构加载器入口，由 `_start_virtualized` 以 tail-call 方式跳入。
///
/// # Safety
///
/// 仅由引导汇编调用一次，不得重入。
#[unsafe(no_mangle)]
pub extern "C" fn __kernel_arch_loader(
    hart_id: usize,
    dtb_addr: usize,
    dtb_snapshot_len: usize,
) -> ! {
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
    // 先快照 DTB 并按板级 DTB 配置早期控制台，保证第一条日志即以正确
    // 波特率输出（实机 UART0 核心时钟 24MHz；QEMU 取 QEMU DTB 的配置）。
    let dtb = store_kernel_dtb(dtb_addr, dtb_snapshot_len)
        .unwrap_or_else(|e| panic!("[loader] DTB: {}", e));
    configure_early_console_from_dtb(&dtb);

    // 串口带宽有限时用 cmdline 过滤日志级别（mygo.loglevel=warn 等）。
    if let Some(level) = dtb
        .chosen_bootargs()
        .ok()
        .flatten()
        .and_then(|bootargs| general::cmdline::Cmdline::new(bootargs.as_bytes()).find("mygo.loglevel"))
    {
        let parsed = match level.trim() {
            "emerg" => log::LogLevel::Emergency,
            "crit" => log::LogLevel::Critical,
            "error" | "err" => log::LogLevel::Error,
            "warn" | "warning" => log::LogLevel::Warning,
            "notice" => log::LogLevel::Notice,
            "debug" => log::LogLevel::Debug,
            _ => log::LogLevel::Info,
        };
        log::set_log_level(parsed);
    }

    log::info!(
        "[loader] RISC-V64 boot: hart={} dtb={:#x}",
        hart_id,
        dtb_addr
    );

    let sbi_info = sbi::init();
    let pmu_counters = if sbi_info.pmu_available {
        sbi::install_pmu_backend().unwrap_or_else(|error| {
            panic!("[loader] failed to install SBI PMU backend: {:?}", error)
        });
        let counters = sbi::pmu_num_counters();
        counters.is_ok().then_some(counters.value)
    } else {
        None
    };
    log::info!(
        "[loader] SBI: base={} spec={:?} impl={:?}/{:?} srst={} hsm={} ipi={} rfence={} pmu={} counters={:?}",
        sbi_info.base_available,
        sbi_info.spec_version,
        sbi_info.implementation_id,
        sbi_info.implementation_version,
        sbi_info.srst_available,
        sbi_info.hsm_available,
        sbi_info.ipi_available,
        sbi_info.rfence_available,
        sbi_info.pmu_available,
        pmu_counters,
    );

    // 初始化引导期分配器
    {
        allocator::KERNEL_ALLOCATOR.bind_address_translation(phys_to_virt, virt_to_phys);

        allocator::KERNEL_ALLOCATOR.bind_cpu_id(current_cpu_id);

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

    log::info!("[loader] DTB: {} bytes", dtb.as_bytes().len());
    log::info!(
        "[loader] usable RAM handoff constrained to direct map {:#x}..{:#x}",
        heap_vm::KERNEL_DIRECT_MAP_PHYS_START,
        heap_vm::KERNEL_DIRECT_MAP_PHYS_END
    );
    let command_line_text = dtb
        .chosen_bootargs()
        .unwrap_or_else(|error| panic!("[loader] invalid /chosen/bootargs: {:?}", error));
    if let Some(command_line) = command_line_text {
        log::info!("[loader] command line from DTB: {}", command_line);
    }
    let command_line = command_line_text.map(str::as_bytes);

    // 页表、定时器和可迁移用户任务都依赖全 hart 能力。先取全部 CPU 的
    // MMU 能力交集，再尝试从早期 Sv39 页表升级到 Sv48。
    let requested_paging_mode = configure_cpu_features_from_dtb(&dtb, hart_id)
        .unwrap_or_else(|error| panic!("[loader] invalid RISC-V CPU DT binding: {:?}", error));
    let final_paging_mode = crate::riscv64::boot::select_final_paging_mode(requested_paging_mode);
    log::info!(
        "[loader] paging mode: requested={:?} final={:?}",
        requested_paging_mode,
        final_paging_mode
    );

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
                command_line,
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
                tracked_heap_region: heap_vm::tracked_heap_region,
                map_kernel_heap_range: map_kernel_heap,
                unmap_kernel_heap_range: unmap_kernel_heap,
                protect_kernel_heap_range: protect_kernel_heap,
                validate_kernel_heap_range: validate_kernel_heap,
                sync_icache,
                init_kernel_page_table: heap_vm::init_kernel_page_table,
                no_map: StartNoMapSupport::Enforced {
                    granule: heap_vm::NO_MAP_GRANULE,
                    prepare: heap_vm::prepare_no_map,
                },
            }),
        };

        unsafe extern "C" {
            fn __kernel_start_init(context: *const StartContext) -> !;
        }
        log::info!("[loader] → __kernel_start_init");
        __kernel_start_init(&context);
    }
}
