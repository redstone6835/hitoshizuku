//! RISC-V64 内核堆页表映射实现。
//!
//! 本模块负责把 allocator 提供的"内核堆虚拟地址范围"和"物理页帧分配结果"
//! 连接起来，使上层的大对象分配器、slab 扩容以及其它基于内核堆的设施能够
//! 真正访问到有效内存。
//!
//! ## 与 [`paging`](super::paging) 的分工
//!
//! - `paging` 负责描述 RISC-V64 页表项格式、层级、TLB 操作；
//! - 本模块负责在运行时"走页表、创建中间页表页、写入叶子映射、解除映射"。
//!
//! ## 虚拟地址布局（Sv48）
//!
//! ```text
//! PGD[511] (0xFFFF_FF80_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF)
//!   PUD[0..1]: kernel heap（按需映射 4K / 2M 页）
//!   PUD[2]:    kernel code direct map（512×2MiB = 1GiB，PA 0x8000_0000 起）
//!   PUD[3]:    kernel direct map 扩展（1GiB leaf，PA 0xC000_0000 起，boot 阶段建立）
//!
//! PGD[510] (0xFFFF_FF00_0000_0000 .. 0xFFFF_FF80_0000_0000)
//!   PUD[0]:    MMIO 直接映射（1GiB leaf，PA 0x0..0x4000_0000）
//! ```
//!
//! ## 大页策略
//!
//! 与 LoongArch 原型一致，支持三种策略：
//! - `BaseOnly`：强制 4KiB 基本页
//! - `PreferLarge`：优先 2MiB 大页，失败降级到 4KiB
//! - `RequireLarge`：强制 2MiB 大页，失败返回错误

use allocator::{PagePolicy, PAGE_SIZE, PhysicalAllocRequest};
use core::sync::atomic::{AtomicUsize, Ordering};
use general::{MapError, VirtAddr, find_leaf, walk_and_map, unmap_range_entries, PagingArch};

use crate::riscv64::paging::Riscv64Paging;
use crate::riscv64::specific::phys_to_virt;

// ── 常量与静态 ──────────────────────────────────────────────────────────────────

/// 内核堆虚拟地址起始（PGD[511]→PUD[0]）。
pub const KERNEL_HEAP_BASE: usize = 0xFFFF_FF80_0000_0000;

/// 内核堆虚拟地址范围大小（32 GiB）。
pub const KERNEL_HEAP_SIZE: usize = 32 * 1024 * 1024 * 1024;

/// MMIO 直接映射基址（PGD[510]，独立于 kernel heap/code）。
///
/// `device_mmio_to_virt(paddr) = paddr + MMIO_VIRT_BASE`。
pub const MMIO_VIRT_BASE: usize = 0xFFFF_FF00_0000_0000;

/// 内核 direct map 覆盖物理 RAM 的基址和大小（QEMU virt 默认从 0x80000000 开始）。
const KERNEL_PHYS_BASE: usize = 0x8000_0000;
const KERNEL_DIRECT_MAP_SIZE: usize = 0x4000_0000; // 1 GiB

/// 内核 direct map 对应的虚拟基址（高半区，独立于 identity phys_to_virt）。
const KERNEL_VIRT_BASE: usize = KERNEL_PHYS_BASE.wrapping_add(0xFFFF_FF80_0000_0000);

pub(crate) static KERNEL_PAGE_TABLE_ROOT: AtomicUsize = AtomicUsize::new(0);

/// 内核页表的 SATP 值（MODE | ASID=0 | PPN）。
/// trap 入口从用户态来时用于切回内核页表，保证 MMIO 可访问。
#[unsafe(no_mangle)]
pub(crate) static KERNEL_SATP: AtomicUsize = AtomicUsize::new(0);

pub fn kernel_heap_region() -> (usize, usize) {
    (KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE)
}

pub fn kernel_virt_to_phys(vaddr: usize) -> usize {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        // 页表未初始化，使用 direct map 偏移关系：VA = PA + 0xFFFF_FF80_0000_0000
        return vaddr.wrapping_sub(0xFFFF_FF80_0000_0000);
    }
    let root_vaddr = phys_to_virt(root_paddr);
    if let Ok((level, _, pte)) = find_leaf::<Riscv64Paging>(root_vaddr, vaddr, phys_to_virt) {
        let page_size = Riscv64Paging::leaf_page_size(level).unwrap_or(PAGE_SIZE);
        Riscv64Paging::pte_addr(pte) + (vaddr & (page_size - 1))
    } else {
        vaddr.wrapping_sub(0xFFFF_FF80_0000_0000)
    }
}

fn alloc_page_table_page() -> Result<usize, MapError> {
    let request = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map(|alloc| alloc.paddr)
        .map_err(|_| MapError::OutOfMemory)
}

/// 对标 LoongArch：原地修改早期页表 PUD_kernel[2] 从 1GiB leaf → table PTE
/// → PMD（512×2MiB leaves）。用全局 sfence.vma 冲刷一切 TLB/page-walk cache，
/// 保证后续指令取指走新映射。
pub fn init_kernel_page_table() {
    use crate::riscv64::paging::Riscv64Pte;

    // 定位早期页表根
    let satp: usize = read_csr!(satp);
    let root_ppn = satp & 0xFFF_FFFF_FFFF;
    let root_paddr = root_ppn << 12;

    // 走 identity 路径（PGD[0]）读取 PGD[511] → PUD_kernel 物理地址
    let pgd = root_paddr as *const usize;
    let pud_kernel_pte_bits = unsafe { core::ptr::read_volatile(pgd.add(511)) };
    let pud_kernel_paddr = Riscv64Paging::pte_addr(Riscv64Pte(pud_kernel_pte_bits));

    // 分配 PMD 页（4 KiB），按段填入不同权限的 2 MiB leaf（W^X 保护）
    let pmd_req = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    let pmd_alloc = allocator::KERNEL_ALLOCATOR
        .allocate_physical(pmd_req)
        .expect("[arch][heap_vm] failed to allocate PMD page");
    let pmd_paddr = pmd_alloc.paddr;
    let pmd = phys_to_virt(pmd_paddr) as *mut usize;

    // 段边界符号（linker script 已按 2MiB 对齐）
    unsafe extern "C" {
        fn stext();
        fn etext();
        fn srodata();
        fn erodata();
        fn sdata();
    }
    // 将 VA 转为 PMD 索引：VA 减去 PUD[2] 起始后除以 2MiB
    let pud2_va_base = KERNEL_VIRT_BASE; // PUD[2] 对应的 VA 起始
    let two_mib = 2usize * 1024 * 1024;

    let text_start_idx = (stext as usize - pud2_va_base) / two_mib;
    let text_end_idx = (etext as usize - pud2_va_base) / two_mib;
    let rodata_end_idx = (erodata as usize - pud2_va_base) / two_mib;

    for i in 0..512usize {
        let pa = KERNEL_PHYS_BASE + i * two_mib;
        // 根据段归属设置权限（linker 脚本保证 etext/erodata 在 2MiB 边界上）：
        //   [0, text_start)     → RWX（boot trampoline 等启动代码，保守处理）
        //   [text_start, etext) → R+X（代码段，只读可执行）
        //   [etext, erodata)    → R  （只读数据段，不可写）
        //   [erodata, ...)      → R+W（可写数据段和 BSS，不可执行）
        let (r, w, x) = if i < text_start_idx {
            (true, true, true) // 启动头 / 跳板代码
        } else if i < text_end_idx {
            (true, false, true) // .text: R+X
        } else if i < rodata_end_idx {
            (true, false, false) // .rodata: R
        } else {
            (true, true, false) // .data/.bss: R+W
        };
        let leaf = Riscv64Paging::make_leaf_pte(pa, r, w, x, false, true);
        unsafe { core::ptr::write_volatile(pmd.add(i), leaf.bits()) };
    }

    // PUD_kernel[2]：1GiB leaf → table PTE → PMD
    let table_pte = Riscv64Paging::make_table_pte(pmd_paddr);
    let pud_kernel = pud_kernel_paddr as *mut usize;
    unsafe { core::ptr::write_volatile(pud_kernel.add(2), table_pte.bits()) };

    // PGD[510]：MMIO 直接映射区（VA 0xFFFF_FF00_0000_0000 + paddr）
    // 独立于 PGD[511]（kernel code + heap），不与 heap allocator 冲突。
    let mmio_pud_req = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    let mmio_pud_alloc = allocator::KERNEL_ALLOCATOR
        .allocate_physical(mmio_pud_req)
        .expect("[arch][heap_vm] failed to allocate MMIO PUD page");
    let mmio_pud_paddr = mmio_pud_alloc.paddr;
    let mmio_pud = phys_to_virt(mmio_pud_paddr) as *mut usize;
    unsafe { core::ptr::write_bytes(mmio_pud, 0, PAGE_SIZE / core::mem::size_of::<usize>()) };
    // PUD[0]：1GiB leaf 映射 PA 0x0..0x40000000（覆盖 QEMU virt 设备 MMIO）
    let leaf0 = Riscv64Paging::make_leaf_pte(0, true, true, false, false, true);
    unsafe { core::ptr::write_volatile(mmio_pud.add(0), leaf0.bits()) };
    // PUD[1]：1GiB leaf 映射 PA 0x40000000..0x80000000（覆盖 PCI 32-bit BAR 窗口）
    let leaf1 = Riscv64Paging::make_leaf_pte(0x40000000, true, true, false, false, true);
    unsafe { core::ptr::write_volatile(mmio_pud.add(1), leaf1.bits()) };
    // 写入 PGD[510]
    let mmio_pgd_pte = Riscv64Paging::make_table_pte(mmio_pud_paddr);
    let pgd_ptr = phys_to_virt(root_paddr) as *mut usize;
    unsafe { core::ptr::write_volatile(pgd_ptr.add(510), mmio_pgd_pte.bits()) };

    // 确保所有 store 到达内存后再刷 TLB，否则 page-walker 可能读到旧数据
    unsafe { core::arch::asm!("fence rw, rw") };
    // 全局 sfence.vma：冲刷所有 TLB 和 page-walk cache。
    // 之后 CPU 取指重走页表：PGD[511]→PUD[2](table)→PMD[idx]→2MiB leaf，
    // 内核代码 VA 落在 PUD[2] 范围内，映射完整。
    unsafe { core::arch::asm!("sfence.vma") };
    unsafe { core::arch::asm!("fence.i") };

    log::info!(
        "[arch][heap_vm] PUD[2] converted to 2MiB PMD, PUD[0..1] free ({} pages)",
        512usize
    );

    KERNEL_PAGE_TABLE_ROOT.store(root_paddr, Ordering::Release);
    let new_satp: usize = read_csr!(satp);
    KERNEL_SATP.store(new_satp, Ordering::Release);

    // 验证关键内核段的页表权限符合预期
    verify_kernel_segments(root_paddr);

    // PUD[0] 映射就绪，切换 UART 到高半区虚拟地址
    crate::riscv64::early_console::switch_to_virtual();

    // 拆除 identity mapping（PGD[0]）：boot 阶段唯一用途是 UART + DTB 低地址访问，
    // 此时 UART 已切到 PGD[510] MMIO direct map，DTB 已在 loader 中解析完毕。
    // 清除后内核不再能通过低地址直接访问物理内存，消除安全隐患。
    unsafe { core::ptr::write_volatile(pgd_ptr.add(0), 0) };
    unsafe { core::arch::asm!("sfence.vma") };
    log::info!("[arch][heap_vm] identity mapping (PGD[0]) removed");
}

/// 验证内核关键段的页表权限。仅在 debug 构建中生效。
///
/// 确保 W^X（写或执行二选一）策略正确实施：
/// - .text: R+X（只读可执行）
/// - .rodata: R（只读不可执行）
/// - .data: R+W（可读可写不可执行）
fn verify_kernel_segments(root_paddr: usize) {
    use crate::riscv64::paging::Riscv64Paging;

    unsafe extern "C" {
        fn stext();
        fn etext();
        fn srodata();
        fn erodata();
        fn sdata();
        fn edata();
    }

    let root = phys_to_virt(root_paddr);

    let check = |name: &str, va: usize, expect_r: bool, expect_w: bool, expect_x: bool| {
        if let Ok((_level, _ptr, pte)) = find_leaf::<Riscv64Paging>(root, va, phys_to_virt) {
            let flags = <Riscv64Paging as PagingArch>::pte_flags(pte);
            let r = <Riscv64Paging as PagingArch>::flags_readable(flags);
            let w = <Riscv64Paging as PagingArch>::flags_writable(flags);
            let x = <Riscv64Paging as PagingArch>::flags_executable(flags);
            debug_assert!(r == expect_r && w == expect_w && x == expect_x,
                "[heap_vm] segment '{}' at {:#x}: perm R={} W={} X={}, expected R={} W={} X={}",
                name, va, r, w, x, expect_r, expect_w, expect_x);
        }
    };

    // W^X 权限验证
    check(".text", stext as usize, true, false, true);   // R+X
    check(".rodata", srodata as usize, true, false, false); // R
    check(".data", sdata as usize, true, true, false);   // R+W
}

/// 对标 LoongArch：动态搜索 2 MiB 叶子层级，不硬编码 level 1（1 GiB）。
fn map_range_with_policy(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
) -> Result<(), MapError> {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return Err(MapError::OutOfMemory);
    }
    let root_vaddr = phys_to_virt(root_paddr);

    // 动态搜索层级，对标 LoongArch 的做法
    let (target_level, page_size) = find_leaf_level(page_policy, vaddr, paddr, size)?;

    let mut current_vaddr = vaddr;
    let mut current_paddr = paddr;
    let end_vaddr = vaddr + size;

    while current_vaddr < end_vaddr {
        // 如果大页不对齐，降级到 BaseOnly（4K 页）
        if (current_vaddr & (page_size - 1)) != 0 || (current_paddr & (page_size - 1)) != 0 {
            walk_and_map::<Riscv64Paging>(
                root_vaddr,
                current_vaddr,
                current_paddr,
                find_smallest_leaf_level(),
                true, true, true, false, true,
                phys_to_virt,
                alloc_page_table_page,
            )?;
            current_vaddr += PAGE_SIZE;
            current_paddr += PAGE_SIZE;
        } else {
            let result = walk_and_map::<Riscv64Paging>(
                root_vaddr,
                current_vaddr,
                current_paddr,
                target_level,
                true, true, true, false, true,
                phys_to_virt,
                alloc_page_table_page,
            );
            if result.is_err() && page_policy == PagePolicy::RequireLarge {
                return result;
            }
            current_vaddr += page_size;
            current_paddr += page_size;
        }
    }

    unsafe {
        Riscv64Paging::flush_tlb(None);
    }
    Ok(())
}

/// 搜索 2 MiB 叶子层级（对标 LoongArch 的动态搜索）。
fn find_2mib_leaf_level() -> Option<(usize, usize)> {
    for &level in Riscv64Paging::supported_leaf_levels() {
        if let Some(size) = Riscv64Paging::leaf_page_size(level) {
            if size == 2 * 1024 * 1024 {
                return Some((level, size));
            }
        }
    }
    None
}

/// 搜索最小叶子层级（4 KiB）。
fn find_smallest_leaf_level() -> usize {
    let mut smallest: Option<(usize, usize)> = None;
    for &level in Riscv64Paging::supported_leaf_levels() {
        if let Some(size) = Riscv64Paging::leaf_page_size(level) {
            if smallest.is_none() || size < smallest.unwrap().1 {
                smallest = Some((level, size));
            }
        }
    }
    smallest.map(|(l, _)| l).unwrap_or(3)
}

/// 根据 page_policy 确定目标映射层级。
fn find_leaf_level(
    page_policy: PagePolicy,
    _vaddr: usize,
    _paddr: usize,
    size: usize,
) -> Result<(usize, usize), MapError> {
    match page_policy {
        PagePolicy::BaseOnly => {
            let level = find_smallest_leaf_level();
            let page_size = Riscv64Paging::leaf_page_size(level).unwrap_or(PAGE_SIZE);
            Ok((level, page_size))
        }
        PagePolicy::PreferLarge | PagePolicy::RequireLarge => {
            // 只有 >= 2MiB 的分配才尝试大页
            if size >= 2 * 1024 * 1024 {
                if let Some((level, ps)) = find_2mib_leaf_level() {
                    return Ok((level, ps));
                }
            }
            if page_policy == PagePolicy::RequireLarge {
                return Err(MapError::UnsupportedLevel);
            }
            // 降级到 BaseOnly
            let level = find_smallest_leaf_level();
            let page_size = Riscv64Paging::leaf_page_size(level).unwrap_or(PAGE_SIZE);
            Ok((level, page_size))
        }
    }
}

pub(crate) fn flush_kernel_tlb_addr(vaddr: usize) {
    unsafe {
        Riscv64Paging::flush_tlb(Some(general::VirtAddr::new(vaddr)));
    }
}

pub fn map_kernel_heap_range(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
) -> Result<(), MapError> {
    map_range_with_policy(vaddr, paddr, size, page_policy)
}

pub fn unmap_kernel_heap_range(vaddr: usize, size: usize) -> Result<(), MapError> {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return Err(MapError::OutOfMemory);
    }
    let root_vaddr = phys_to_virt(root_paddr);

    unmap_range_entries::<Riscv64Paging>(root_vaddr, vaddr, size, true, phys_to_virt)?;

    unsafe {
        Riscv64Paging::flush_tlb(None);
    }
    Ok(())
}
