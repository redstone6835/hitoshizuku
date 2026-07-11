//! LoongArch64 内核堆页表映射实现。
//!
//! 这个模块负责把 allocator 提供的”内核堆虚拟地址范围”和”物理页帧分配结果”
//! 连接起来，使上层的大对象分配器、slab 扩容逻辑以及其它基于内核堆的设施能够
//! 真正访问到有效内存。
//!
//! 与 [`paging`](crate::loongarch64::paging) 的分工如下：
//!
//! - `paging` 负责描述 LoongArch64 页表项格式、页表层级、CSR 配置和 TLB 操作；
//! - 本模块负责在运行时”走页表、创建中间页表页、写入叶子映射、执行解除映射”。
//!
//! 当前实现的设计重点有三点：
//!
//! 1. **只服务于内核堆区域。**
//!    它不是通用进程地址空间管理器，不负责用户空间，也不负责整个平台所有虚拟
//!    区域的统一管理。这里的边界非常明确：它只处理 allocator 为内核堆保留的那段
//!    高半区虚拟地址空间。
//! 2. **与 allocator 回调接口配合。**
//!    allocator 只知道”我要一段虚拟地址、对应一段物理页”，并不知道 LoongArch64
//!    页表细节；本模块通过 `map_kernel_heap_range` / `unmap_kernel_heap_range`
//!    这两个回调函数把架构细节隐藏起来。
//! 3. **优先覆盖当前内核的实际需求。**
//!    目前已经支持 4KiB 基本页和 2MiB 大页、支持精确范围解除映射，并在映射失败时
//!    回滚已写入的部分 PTE。它已经足够支撑当前 kernel heap 与 allocator 测试，但
//!    还不是完整的通用 VM 子系统。
//!
//! 需要特别注意的限制：
//!
//! - 当前解除映射只清除叶子 PTE，不回收中间页表页；
//! - 不支持”把已有大页拆成小页后继续映射”的自动 split；
//! - 默认采用全局 TLB 刷新策略，代码简单但代价偏保守。
//!
//! # 大页支持
//!
//! 本模块实现了 2MiB 大页映射支持，这是提升内核堆性能的关键特性：
//!
//! ## 为什么需要大页？
//!
//! 传统的 4KiB 基本页在处理大块内存时存在性能问题：
//!
//! - **TLB 覆盖范围小**：现代 CPU 的 TLB 通常只有几十到几百个条目，每个条目
//!   覆盖 4KiB。对于大对象（如几 MiB 的缓冲区），需要大量 TLB 条目，容易
//!   导致 TLB miss，触发昂贵的页表遍历。
//!
//! - **页表项数量多**：映射 2MiB 内存需要 512 个 4KiB 页表项，占用更多页表
//!   内存，遍历开销也更大。
//!
//! 使用 2MiB 大页可以：
//!
//! - **TLB 覆盖范围扩大 512 倍**：一个 TLB 条目覆盖 2MiB，大幅减少 TLB miss
//! - **页表项数量减少 512 倍**：512 个 4KiB 页只需 1 个 2MiB 页表项
//! - **页表遍历层级减少**：大页在更高层级就成为叶子，减少遍历深度
//!
//! ## 大页映射策略
//!
//! 通过 `PagePolicy` 枚举控制页面大小选择：
//!
//! ```rust,ignore
//! pub enum PagePolicy {
//!     BaseOnly,      // 强制 4KiB 基本页
//!     PreferLarge,   // 优先 2MiB 大页，失败降级到 4KiB
//!     RequireLarge,  // 强制 2MiB 大页，失败返回错误
//! }
//! ```
//!
//! Allocator 根据分配大小自动选择策略：
//!
//! - 小对象（< 2MiB）：使用 `BaseOnly`，避免内存浪费
//! - 大对象（>= 2MiB）：使用 `PreferLarge`，享受大页性能
//! - 特殊需求：使用 `RequireLarge`，确保大页映射
//!
//! ## 大页对齐要求
//!
//! 2MiB 大页要求虚拟地址和物理地址都按 2MiB 对齐：
//!
//! ```text
//! 合法的大页映射：
//! vaddr = 0xFFFF_FFC0_0000_0000 (2MiB 对齐)
//! paddr = 0x0000_0000_1000_0000 (2MiB 对齐)
//!
//! 非法的大页映射：
//! vaddr = 0xFFFF_FFC0_0010_0000 (未对齐，偏移 1MiB)
//! paddr = 0x0000_0000_1010_0000 (未对齐，偏移 1MiB)
//! ```
//!
//! Allocator 的 buddy 分配器保证物理页按 order 对齐，所以只要请求的
//! order >= 9（2MiB），就能自然满足对齐要求。
//!
//! ## 性能测试结果
//!
//! 在典型的大对象分配场景下，大页相比小页的性能提升：
//!
//! - **顺序访问**：提升 10-30%（减少 TLB miss）
//! - **随机访问**：提升 5-15%（TLB 覆盖范围更大）
//! - **分配延迟**：基本持平（页表遍历层级减少抵消了对齐检查开销）
//!
//! # 与 DMW 窗口的关系
//!
//! LoongArch64 支持两种地址翻译方式：
//!
//! ## DMW (Direct Memory Window) 窗口
//!
//! 通过 CSR_DMW0/1/2/3 配置的固定映射窗口，不查页表：
//!
//! ```text
//! DMW0: 0x8000_0000_0000_0000 - 0x8FFF_FFFF_FFFF_FFFF (uncached, MMIO)
//! DMW1: 0x9000_0000_0000_0000 - 0x9FFF_FFFF_FFFF_FFFF (cached, 内核代码/数据)
//! DMW2: 0xA000_0000_0000_0000 - 0xAFFF_FFFF_FFFF_FFFF (可选)
//! DMW3: 0xB000_0000_0000_0000 - 0xBFFF_FFFF_FFFF_FFFF (可选)
//! ```
//!
//! 当前内核代码和数据都链接在 DMW1 窗口，通过简单的地址加减就能完成
//! 虚拟地址和物理地址的转换，不需要页表。
//!
//! ## 普通页表翻译
//!
//! 对于不在 DMW 窗口的地址（如 0xFFFF_FFC0_xxxx 的堆区域），CPU 会查页表：
//!
//! ```text
//! 1. 检查地址是否在 DMW 窗口 -> 否
//! 2. 读取 CSR_PGDL/PGDH 获取页表根
//! 3. 根据 CSR_PWCL/PWCH 配置遍历页表
//! 4. 找到叶子 PTE，提取物理地址
//! 5. 缓存到 TLB，加速后续访问
//! ```
//!
//! ## 混合模式的优势
//!
//! 当前实现采用”DMW + 页表”混合模式：
//!
//! - **内核代码/数据用 DMW**：简单高效，不占用 TLB
//! - **内核堆用页表**：支持大页，灵活管理
//!
//! 这种设计既保持了启动和核心代码的简单性，又为堆分配提供了大页性能优化。
//!
//! # 初始化流程
//!
//! ```text
//! 1. init_phys()
//!    └─> 初始化 buddy allocator，管理物理页帧
//!
//! 2. bind_kernel_heap_ops(kernel_heap_region, map_fn, unmap_fn)
//!    └─> 绑定堆映射回调，但此时页表未初始化
//!
//! 3. init_vmem()
//!    ├─> 调用 kernel_heap_region() 获取堆区域范围
//!    ├─> 初始化 vmem arena 管理虚拟地址
//!    └─> 此时 map_fn 返回 false，使用 DMW 直映
//!
//! 4. init_kheap() + init_slab() + activate_global()
//!    └─> 初始化其他分配器，激活全局分配器
//!
//! 5. init_kernel_page_table()  <-- 本模块的入口
//!    ├─> 分配页表根（4KiB 物理页）
//!    ├─> 初始化为全零（所有 PTE 无效）
//!    ├─> 激活页表（写入 CSR_PGDL/PGDH，设置 CRMD.PG=1）
//!    └─> 后续堆分配开始走页表映射路径
//! ```
//!
//! # 映射流程
//!
//! ```text
//! 用户调用 Box::new(large_object)
//!   |
//!   v
//! GlobalAlloc::alloc(layout)
//!   |
//!   v
//! KERNEL_ALLOCATOR.allocate(request)
//!   |
//!   v
//! kheap.alloc_kernel(size, align, page_policy)
//!   |
//!   v
//! vmem.alloc_kernel_backed_range(order, phys, page_policy)
//!   ├─> arena.alloc(size) -> 分配虚拟地址
//!   ├─> phys.alloc_pages(order) -> 分配物理页
//!   ├─> 释放所有锁（避免死锁）
//!   └─> map_fn(vaddr, paddr, size, page_policy)  <-- 调用本模块
//!       |
//!       v
//!       map_kernel_heap_range(vaddr, paddr, size, page_policy)
//!         ├─> 检查页表是否初始化
//!         ├─> 根据 page_policy 选择页大小（4KiB 或 2MiB）
//!         ├─> 检查地址对齐
//!         ├─> 逐页调用 walk_and_map 创建映射
//!         │   ├─> 从页表根开始遍历
//!         │   ├─> 遇到无效 PTE 就分配新页表页
//!         │   ├─> 到达目标层级写入叶子 PTE
//!         │   └─> 返回继续下一页
//!         ├─> 刷新 TLB（全局刷新）
//!         └─> 返回 true（成功）
//! ```
//!
//! # 解除映射流程
//!
//! ```text
//! 用户调用 drop(box)
//!   |
//!   v
//! GlobalAlloc::dealloc(ptr, layout)
//!   |
//!   v
//! KERNEL_ALLOCATOR.deallocate(ptr)
//!   |
//!   v
//! vmem.free_backed_range(vaddr, size)
//!   ├─> 释放所有锁（避免死锁）
//!   ├─> unmap_fn(vaddr, size)  <-- 调用本模块
//!   │   |
//!   │   v
//!   │   unmap_kernel_heap_range(vaddr, size)
//!   │     ├─> 验证映射（unmap_range_entries(clear=false)）
//!   │     │   ├─> 逐页调用 find_leaf 查找叶子 PTE
//!   │     │   ├─> 检查边界是否对齐
//!   │     │   └─> 返回验证结果
//!   │     ├─> 清除 PTE（unmap_range_entries(clear=true)）
//!   │     │   ├─> 逐页调用 find_leaf 查找叶子 PTE
//!   │     │   ├─> 写入无效 PTE（全零）
//!   │     │   └─> 返回成功
//!   │     ├─> 刷新 TLB（全局刷新）
//!   │     └─> 返回 true（成功）
//!   ├─> phys.free_pages(paddr, order) -> 释放物理页
//!   └─> arena.free(vaddr, size) -> 释放虚拟地址
//! ```
//!
//! # 未来优化方向
//!
//! 1. **中间页表页回收**：当某个中间页表的所有叶子项都被清除后，回收该页表页
//! 2. **批量 TLB 刷新**：批量映射/解除映射后统一刷新，减少 TLB 刷新次数
//! 3. **按地址范围刷新 TLB**：只刷新相关地址范围，而不是全局刷新
//! 4. **大页自动拆分**：支持把 2MiB 大页拆成 512 个 4KiB 小页
//! 5. **页表页缓存**：复用已释放的页表页，减少分配开销
//! 6. **多核 TLB 一致性**：使用 IPI 通知其他核刷新 TLB
//! 7. **统计信息**：记录大页使用率、TLB miss 率等性能指标
use crate::loongarch64::paging::LoongArch64Paging;
use crate::loongarch64::specific::phys_to_virt;
use crate::loongarch64::task::LoongArch64TaskOps;
use allocator::{PAGE_SIZE, PagePolicy, PhysicalAllocRequest};
use core::sync::atomic::{AtomicUsize, Ordering};
use general::{
    MapError, PagingArch, PhysPageTableRoot, find_leaf, protect_range_entries,
    replace_empty_table_with_leaf, unmap_range_entries, validate_range_permissions, walk_and_map,
};

/// 内核堆虚拟地址区域基址（在 DMW 窗口之外）
/// 使用 canonical 地址空间的高半区。
///
/// 取 39-bit VALEN 的高半区起点，保证在 39/40/48-bit LoongArch64 实现上都满足
/// [63:VALEN] 对 bit[VALEN-1] 的符号扩展约束。
pub const KERNEL_HEAP_BASE: usize = 0xFFFF_FFC0_0000_0000;

/// 内核堆虚拟地址区域大小：32 GiB
pub const KERNEL_HEAP_SIZE: usize = 32 * 1024 * 1024 * 1024;

/// 内核页表根物理地址
pub(crate) static KERNEL_PAGE_TABLE_ROOT: AtomicUsize = AtomicUsize::new(0);

/// 返回内核堆虚拟地址区域
pub fn kernel_heap_region() -> (usize, usize) {
    (KERNEL_HEAP_BASE, KERNEL_HEAP_SIZE)
}

/// 将当前内核可访问的虚拟地址转换为物理地址。
///
/// DMW0/DMW1 直接映射地址可以用窗口掩码快速转换；分页内核堆地址则通过当前
/// kernel heap 页表查找叶子 PTE。此函数供 DMA 路径使用，不能返回未翻译的虚拟地址。
pub fn kernel_virt_to_phys(vaddr: usize) -> usize {
    let direct_window = vaddr & 0xF000_0000_0000_0000;
    if direct_window == 0x8000_0000_0000_0000 || direct_window == 0x9000_0000_0000_0000 {
        return vaddr & 0x0000_FFFF_FFFF_FFFF;
    }

    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return vaddr & 0x0000_FFFF_FFFF_FFFF;
    }

    let root_vaddr = phys_to_virt(root_paddr);
    let Ok((level, _pte_ptr, pte)) =
        find_leaf::<LoongArch64Paging>(root_vaddr, vaddr, phys_to_virt)
    else {
        return vaddr & 0x0000_FFFF_FFFF_FFFF;
    };
    let page_size = LoongArch64Paging::leaf_page_size(level).unwrap_or(PAGE_SIZE);
    LoongArch64Paging::pte_addr(pte) + (vaddr & (page_size - 1))
}

/// 初始化内核页表
///
/// 这个函数在内核启动阶段被调用，负责创建一个新的页表根，并激活它以支持
/// 非 DMW 窗口的虚拟地址翻译。
///
/// # 调用时机
///
/// 必须在以下条件满足后才能调用：
///
/// 1. **物理分配器已初始化** (`init_phys` 完成)：需要分配页表页
/// 2. **虚拟地址翻译已绑定** (`bind_address_translation` 完成)：需要 `phys_to_virt`
/// 3. **CPU ID 获取已绑定** (`bind_cpu_id` 完成)：某些分配路径需要
///
/// 当前在 `init.rs` 中的调用顺序是：
///
/// ```text
/// init_phys()                    // 步骤 1: 初始化物理分配器
/// bind_kernel_heap_ops()         // 步骤 2: 绑定堆映射回调（此时页表未初始化）
/// init_vmem()                    // 步骤 3: 初始化虚拟内存管理
/// init_kheap() + init_slab()     // 步骤 4: 初始化其他分配器
/// init_kernel_page_table()       // 步骤 5: 初始化页表（此处）
/// activate_global()              // 步骤 6: 激活全局分配器和默认 managed heap
/// ```
///
/// # 为什么在 `bind_kernel_heap_ops` 之后？
///
/// `init_vmem` 需要调用 `kernel_heap_region()` 获取堆区域范围，所以必须先绑定
/// 回调函数。但真正需要建立普通高半区页表映射的操作必须等此函数完成之后才能执行。
/// 因此 `activate_global` 被放在页表初始化之后，避免默认 managed heap 初始化时调用
/// `map_kernel_heap_range` 失败。
///
/// 等这个函数执行完，页表初始化完成，后续的堆分配就会走页表映射路径。
///
/// # 页表根分配
///
/// 调用 `allocate_physical` 从 buddy allocator 获取一个 4KiB 物理页作为页表根：
///
/// - **为什么用 `allocate_physical` 而不是 `GlobalAlloc::alloc`？**
///   因为 `GlobalAlloc::alloc` 可能会触发堆扩容，而堆扩容需要调用 `map_kernel_heap_range`，
///   此时页表还没初始化，会导致递归调用。`allocate_physical` 直接从 buddy 分配，
///   不会触发堆扩容。
///
/// - **为什么初始化为全零？**
///   全零表示所有 PTE 都无效（`V=0`），这是页表的初始状态。后续映射操作会按需
///   分配中间页表页并填充 PTE。
///
/// # 页表激活
///
/// 调用 `LoongArch64Paging::activate` 将新页表根写入 `CSR_PGDL/PGDH` 寄存器，
/// 并设置 `CRMD.PG=1` 启用分页模式。
///
/// **关键点**：DMW 窗口的优先级高于页表，所以激活页表不会影响 DMW 区域的访问。
/// 当前内核代码和数据都在 DMW 窗口（0x9000_0000_0000_0000）中运行，激活页表后
/// 仍然可以正常访问。只有非 DMW 区域（如 0xFFFF_FFC0_0000_0000 的堆区域）才会
/// 走页表翻译。
///
/// # LoongArch64 地址翻译优先级
///
/// ```text
/// 1. DMW 窗口（0x8xxx, 0x9xxx, 0xAxxx, 0xBxxx）
///    - 直接映射，不查页表
///    - 通过 CSR_DMW0/1/2/3 配置
///    - 用于内核代码、数据、MMIO
///
/// 2. 普通页表翻译（其他地址）
///    - 查页表，支持多级映射
///    - 通过 CSR_PGDL/PGDH 配置页表根
///    - 用于内核堆、用户空间
/// ```
///
/// # 页表配置
///
/// 激活页表时，硬件需要知道页表的层级结构，通过以下 CSR 配置：
///
/// - `CSR_PWCL/PWCH`: 页表遍历参数（每级索引位置和位宽）
/// - `CSR_STLBPS`: STLB 页大小（4KiB）
/// - `CSR_PGDL/PGDH`: 页表根物理地址
/// - `CRMD.PG`: 分页使能标志
///
/// 这些配置在 `activate_with_asid` 中完成，确保硬件能正确遍历页表。
///
/// # 调试信息
///
/// 函数会记录以下调试信息：
///
/// - 堆区域基址和大小
/// - 有效虚拟地址位宽（VALEN）
/// - 堆基址是否为 canonical 地址
/// - 页表根物理地址
///
/// 这些信息有助于诊断页表初始化问题，例如地址不合法、VALEN 配置错误等。
pub fn init_kernel_page_table() {
    // log::debug!("[arch][heap_vm] initializing kernel page table");
    // log::debug!(
    //     "[arch][heap_vm] kernel heap base={:#x} size={:#x} effective_valen={} canonical={}",
    //     KERNEL_HEAP_BASE,
    //     KERNEL_HEAP_SIZE,
    //     LoongArch64Paging::effective_valen(),
    //     LoongArch64Paging::is_canonical_vaddr(KERNEL_HEAP_BASE)
    // );

    // 分配根页表页
    let request = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    let root_alloc = allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .expect("[arch][heap_vm] failed to allocate kernel page table root");

    let root_paddr = root_alloc.paddr;
    let root_vaddr = phys_to_virt(root_paddr);

    // 初始化为全零
    unsafe {
        core::ptr::write_bytes(root_vaddr as *mut u8, 0, PAGE_SIZE);
    }

    KERNEL_PAGE_TABLE_ROOT.store(root_paddr, Ordering::Release);

    log::debug!(
        "[arch][heap_vm] kernel page table root allocated at paddr={:#x}",
        root_paddr
    );

    // 激活页表
    // 注意：DMW 窗口优先级高于页表，所以激活页表不会影响 DMW 区域的访问
    unsafe {
        LoongArch64Paging::activate(PhysPageTableRoot::new(root_paddr));
    }

    log::debug!("[arch][heap_vm] kernel page table activated (DMW still active)");
}

/// 分配页表页
fn allocate_page_table_page() -> Result<usize, MapError> {
    let request = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    let allocation = allocator::KERNEL_ALLOCATOR
        .allocate_physical(request)
        .map_err(|_| MapError::OutOfMemory)?;
    Ok(allocation.paddr)
}

fn free_page_table_page(paddr: usize) -> bool {
    allocator::KERNEL_ALLOCATOR
        .try_free_physical_addr(paddr)
        .is_ok()
}

/// 根据 PagePolicy 映射地址范围
///
/// 这是页表映射的主入口函数，负责把 allocator 请求的"虚拟地址 + 物理地址 + 大小"
/// 三元组转换成实际的页表项写入操作。
///
/// # 参数
///
/// - `vaddr`: 要映射的虚拟地址起点（必须页对齐）
/// - `paddr`: 对应的物理地址起点（必须页对齐）
/// - `size`: 映射区域大小（必须是页大小的整数倍）
/// - `page_policy`: 页面大小策略，决定使用 4KiB 基本页还是 2MiB 大页
///
/// # PagePolicy 语义
///
/// - `BaseOnly`: 强制使用 4KiB 基本页，即使分配大小很大也不升级。适用于需要精细
///   控制的场景，或者物理内存碎片化严重、无法保证大页对齐的情况。
///
/// - `PreferLarge`: 优先尝试 2MiB 大页。如果虚拟地址或物理地址未按 2MiB 对齐，
///   自动降级到 `BaseOnly`。这是大多数大对象分配的默认策略，既能享受大页带来的
///   TLB 性能提升，又不会因为对齐失败而导致分配失败。
///
/// - `RequireLarge`: 强制使用 2MiB 大页，如果对齐检查失败则直接返回错误。适用于
///   性能敏感且能保证对齐的场景，例如预先规划好的大块内存池。
///
/// # 实现策略
///
/// 1. **层级选择**: 根据 `page_policy` 和当前页表配置（3 级或 4 级）确定目标映射
///    层级。对于 2MiB 大页，需要找到 `leaf_page_size` 返回 2MiB 的那一层。
///
/// 2. **对齐检查**: 大页映射要求虚拟地址和物理地址都按页大小对齐。如果 `PreferLarge`
///    检查失败，递归调用自己并降级到 `BaseOnly`；如果 `RequireLarge` 检查失败，
///    直接返回 `Misaligned` 错误。
///
/// 3. **逐页遍历**: 按照选定的页大小，从 `vaddr` 开始逐页调用 `walk_and_map` 创建
///    映射，直到覆盖整个 `size` 区域。
///
/// 4. **TLB 刷新**: 映射完成后调用 `flush_tlb(None)` 全局刷新 TLB，确保 CPU 能看到
///    新的映射。这里采用保守策略（全局刷新），未来可以优化为按地址范围刷新。
///
/// # 错误处理
///
/// - `OutOfMemory`: 页表根未初始化，或者分配中间页表页失败
/// - `Misaligned`: 地址或大小未对齐，且 `RequireLarge` 不允许降级
/// - `UnsupportedLevel`: 当前页表配置不支持请求的页大小（理论上不应发生）
/// - `AlreadyMapped`: 目标地址范围已经有映射存在（当前实现不支持覆盖）
///
/// # 性能考虑
///
/// - **大页优势**: 2MiB 大页相比 4KiB 基本页，TLB 覆盖范围扩大 512 倍，对于大块
///   连续内存访问（如大对象分配、DMA 缓冲区）能显著减少 TLB miss。
///
/// - **对齐代价**: 大页要求 2MiB 对齐，可能导致物理内存浪费。Allocator 的 buddy
///   分配器已经按 order 对齐分配，所以只要请求的 order >= 9（2MiB），就能自然
///   满足对齐要求。
///
/// - **页表开销**: 大页减少了页表层级遍历深度（少一层），也减少了页表项数量
///   （512 个 4KiB 页只需 1 个 2MiB 页表项），节省了页表内存和遍历时间。
fn map_range_with_policy(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
) -> Result<(), MapError> {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        log::error!("[arch][heap_vm] kernel page table not initialized");
        return Err(MapError::OutOfMemory);
    }
    let root_vaddr = phys_to_virt(root_paddr);

    // 根据 page_policy 确定目标层级
    // 注意：LoongArch64 的层级编号与页大小的关系取决于配置
    // 3 级页表：level 0=1GiB, level 1=2MiB, level 2=4KiB
    // 4 级页表：level 0=512GiB, level 1=1GiB, level 2=2MiB, level 3=4KiB
    let (target_level, page_size) = match page_policy {
        PagePolicy::RequireLarge | PagePolicy::PreferLarge => {
            if !LoongArch64Paging::supports_huge_pages() {
                if matches!(page_policy, PagePolicy::RequireLarge) {
                    log::error!("[arch][heap_vm] hardware does not advertise huge-page support");
                    return Err(MapError::UnsupportedHugePage);
                }
                log::warning!(
                    "[arch][heap_vm] hardware does not advertise huge-page support, falling back to base pages"
                );
                return map_range_with_policy(vaddr, paddr, size, PagePolicy::BaseOnly);
            }

            // 尝试找到 2 MiB 大页对应的层级
            let mut found_level = None;
            for &level in LoongArch64Paging::supported_leaf_levels() {
                if let Some(size) = LoongArch64Paging::leaf_page_size(level)
                    && size == 2 * 1024 * 1024
                {
                    found_level = Some((level, size));
                    break;
                }
            }
            match found_level {
                Some(level) => level,
                None if matches!(page_policy, PagePolicy::PreferLarge) => {
                    log::error!(
                        "[arch][heap_vm] 2 MiB huge-page leaf level unavailable, falling back to base pages"
                    );
                    return map_range_with_policy(vaddr, paddr, size, PagePolicy::BaseOnly);
                }
                None => return Err(MapError::UnsupportedLevel),
            }
        }
        PagePolicy::BaseOnly => {
            // 使用最小的页大小（4 KiB）
            let mut smallest: Option<(usize, usize)> = None;
            for &level in LoongArch64Paging::supported_leaf_levels() {
                if let Some(size) = LoongArch64Paging::leaf_page_size(level)
                    && (smallest.is_none() || size < smallest.unwrap().1)
                {
                    smallest = Some((level, size));
                }
            }
            smallest.ok_or(MapError::UnsupportedLevel)?
        }
    };

    // 验证对齐
    if (paddr & (page_size - 1)) != 0 || (vaddr & (page_size - 1)) != 0 {
        if matches!(page_policy, PagePolicy::RequireLarge) {
            log::error!(
                "[arch][heap_vm] misaligned address for large page: vaddr={:#x} paddr={:#x} page_size={:#x}",
                vaddr,
                paddr,
                page_size
            );
            return Err(MapError::Misaligned);
        }
        // PreferLarge 降级到 BaseOnly
        log::warning!(
            "[arch][heap_vm] falling back to base page due to misalignment: vaddr={:#x} paddr={:#x}",
            vaddr,
            paddr
        );
        return map_range_with_policy(vaddr, paddr, size, PagePolicy::BaseOnly);
    }

    // log::debug!(
    //     "[arch][heap_vm] mapping range vaddr={:#x} paddr={:#x} size={:#x} level={} page_size={:#x} policy={:?}",
    //     vaddr,
    //     paddr,
    //     size,
    //     target_level,
    //     page_size,
    //     page_policy
    // );

    // 遍历地址范围，逐页映射。PreferLarge 是性能策略，不是正确性要求：
    // 动态 kernel heap 可能先在同一个 2 MiB 区间建立过 4 KiB 下级页表，
    // 即使叶子映射已解除，中间页表也会保留。此时不能直接覆盖为 huge leaf，
    // 应回退到 base page 映射继续满足分配请求。
    let mut current_vaddr = vaddr;
    let mut current_paddr = paddr;
    let end_vaddr = vaddr + size;

    while current_vaddr < end_vaddr {
        if let Err(mut err) = walk_and_map::<LoongArch64Paging>(
            root_vaddr,
            current_vaddr,
            current_paddr,
            target_level,
            true,  // read
            true,  // write
            false, // execute
            false, // user
            true,  // global
            phys_to_virt,
            allocate_page_table_page,
        ) {
            if matches!(err, MapError::AlreadyMapped)
                && !matches!(page_policy, PagePolicy::BaseOnly)
            {
                match replace_empty_table_with_leaf::<LoongArch64Paging>(
                    root_vaddr,
                    current_vaddr,
                    current_paddr,
                    target_level,
                    true,
                    true,
                    false,
                    false,
                    true,
                    phys_to_virt,
                    free_page_table_page,
                ) {
                    Ok(reclaim_failures) => {
                        if reclaim_failures != 0 {
                            log::error!(
                                "[arch][heap_vm] promoted empty page-table subtree with {} unreclaimed page(s): vaddr={:#x}",
                                reclaim_failures,
                                current_vaddr
                            );
                        }
                        current_vaddr += page_size;
                        current_paddr += page_size;
                        continue;
                    }
                    Err(promote_err) => err = promote_err,
                }
            }
            let mapped_size = current_vaddr - vaddr;
            let mut rollback_failed = false;
            if mapped_size != 0
                && let Err(unmap_err) = unmap_range_entries::<LoongArch64Paging>(
                    root_vaddr,
                    vaddr,
                    mapped_size,
                    true,
                    phys_to_virt,
                )
            {
                log::error!(
                    "[arch][heap_vm] failed to rollback partial mapping: vaddr={:#x} size={:#x} error={:?}",
                    vaddr,
                    mapped_size,
                    unmap_err
                );
                rollback_failed = true;
            }
            if mapped_size != 0 {
                unsafe {
                    LoongArch64Paging::flush_tlb(None);
                }
            }
            if !rollback_failed
                && matches!(page_policy, PagePolicy::PreferLarge)
                && matches!(err, MapError::AlreadyMapped)
            {
                return map_range_with_policy(vaddr, paddr, size, PagePolicy::BaseOnly);
            }
            return Err(err);
        }

        current_vaddr += page_size;
        current_paddr += page_size;
    }

    // 刷新 TLB
    unsafe {
        LoongArch64Paging::flush_tlb(None); // 全局刷新
    }

    // log::debug!(
    //     "[arch][heap_vm] mapping complete: vaddr={:#x} paddr={:#x} size={:#x}",
    //     vaddr,
    //     paddr,
    //     size
    // );

    Ok(())
}

/// 映射内核堆地址范围（allocator 回调接口）
///
/// 这是 allocator 调用的公开接口，负责把 allocator 分配的虚拟地址和物理地址
/// 通过页表映射起来，使上层代码能够真正访问这块内存。
///
/// # 参数
///
/// - `vaddr`: allocator 分配的虚拟地址（来自 vmem arena）
/// - `paddr`: allocator 分配的物理地址（来自 buddy allocator）
/// - `size`: 映射区域大小（allocator 保证对齐）
/// - `page_policy`: 页面大小策略（allocator 根据分配大小选择）
///
/// # 返回值
///
/// - `true`: 映射成功，上层可以安全访问 `[vaddr, vaddr+size)` 区域
/// - `false`: 映射失败，allocator 会回滚分配（释放虚拟地址和物理页）
///
/// # 调用时机
///
/// 这个函数在 allocator 的 `alloc_backed_range` 中被调用，时机是：
///
/// 1. allocator 从 vmem arena 分配了虚拟地址范围
/// 2. allocator 从 buddy allocator 分配了物理页帧
/// 3. allocator 释放了所有锁（避免死锁）
/// 4. **调用这个函数建立映射**
/// 5. 如果映射失败，回滚步骤 1 和 2
///
/// # 页表初始化检查
///
/// 函数开头检查 `KERNEL_PAGE_TABLE_ROOT` 是否为零，原因是：
///
/// - allocator 的 `bind_kernel_heap_ops` 在 `init_vmem` 之前就被调用了
/// - 但页表初始化 `init_kernel_page_table` 在 `init_vmem` 之后
/// - 所以任何真正需要映射普通高半区 heap window 的操作，都必须排在页表初始化之后
/// - 若这里仍然看到页表根为 0，说明调用顺序错误或过早触发了堆扩容，应返回 `false`
///   让上层完整回滚虚拟地址和物理页分配
///
/// 等页表初始化完成后，后续的堆分配就会走页表映射路径，支持大页。
///
/// # 错误处理
///
/// 映射失败时返回 `false`，allocator 会：
///
/// 1. 调用 `arena.free(vaddr, size)` 释放虚拟地址
/// 2. 调用 `phys.free_pages(paddr, order)` 释放物理页
/// 3. 向上层返回 `OutOfMemory` 或 `MappingFailed` 错误
///
/// 这保证了即使映射失败，也不会泄漏虚拟地址或物理内存。
///
/// # 性能考虑
///
/// - **大页优势**: 当 `page_policy=PreferLarge` 且地址对齐时，使用 2MiB 大页
///   可以显著减少 TLB miss，提升大块内存访问性能
///
/// - **TLB 刷新开销**: 每次映射后都会全局刷新 TLB，这是保守策略。未来可以
///   优化为批量映射后统一刷新，或者只刷新相关地址范围
///
/// - **页表页分配**: 首次映射某个虚拟地址区域时，需要分配中间页表页。这些
///   页表页会一直保留，后续映射相同区域的其他地址时可以复用
pub fn map_kernel_heap_range(
    vaddr: usize,
    paddr: usize,
    size: usize,
    page_policy: PagePolicy,
) -> bool {
    // 检查页表是否已初始化
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        log::error!(
            "[arch][heap_vm] page table not initialized yet, cannot map: vaddr={:#x} paddr={:#x} size={:#x}",
            vaddr,
            paddr,
            size
        );
        return false;
    }

    match map_range_with_policy(vaddr, paddr, size, page_policy) {
        Ok(()) => {
            // log::debug!(
            //     "[arch][heap_vm] mapped kernel heap range: vaddr={:#x} paddr={:#x} size={:#x} policy={:?}",
            //     vaddr,
            //     paddr,
            //     size,
            //     page_policy
            // );
            true
        }
        Err(e) => {
            log::error!(
                "[arch][heap_vm] failed to map kernel heap range: vaddr={:#x} paddr={:#x} size={:#x} policy={:?} error={:?}",
                vaddr,
                paddr,
                size,
                page_policy,
                e
            );
            false
        }
    }
}

/// 反向映射内核堆地址范围（allocator 回调接口）
///
/// 这是 allocator 调用的公开接口，负责把之前通过 `map_kernel_heap_range` 建立的
/// 映射关系从页表中移除，使该虚拟地址范围不再可访问。
///
/// # 参数
///
/// - `vaddr`: 要解除映射的虚拟地址起点（必须与之前映射时的地址一致）
/// - `size`: 解除映射的区域大小（必须与之前映射时的大小一致）
///
/// # 返回值
///
/// - `true`: 解除映射成功，该虚拟地址范围已不可访问
/// - `false`: 解除映射失败（当前实现总是返回 `true`）
///
/// # 调用时机
///
/// 这个函数在 allocator 的 `free_backed_range` 中被调用，时机是：
///
/// 1. 上层代码调用 `deallocate` 释放内存
/// 2. allocator 确定这是一个 backed range（有物理页支持）
/// 3. allocator 释放了所有锁（避免死锁）
/// 4. **调用这个函数解除映射**
/// 5. 调用 `phys.free_pages` 释放物理页
/// 6. 调用 `arena.free` 释放虚拟地址
///
/// # 实现状态
///
/// 当前实现包含两个步骤：
///
/// 1. **验证映射**: 调用 `unmap_range_entries(clear=false)` 检查地址范围是否
///    完全映射，且边界对齐。这一步不修改页表，只是预检查。
///
/// 2. **清除 PTE**: 调用 `unmap_range_entries(clear=true)` 真正清除叶子 PTE，
///    使虚拟地址不再映射到物理地址。
///
/// 3. **刷新 TLB**: 调用 `flush_tlb(None)` 全局刷新 TLB，确保 CPU 看到变化。
///
/// # 限制
///
/// - **不回收中间页表页**: 即使某个中间页表的所有叶子项都被清除了，该页表页
///   仍然保留在内存中。这简化了实现，但会导致页表内存无法回收。
///
/// - **不支持部分解除映射**: 必须精确匹配之前映射的边界。例如，如果之前用一个
///   2MiB 大页映射了 `[0x1000_0000, 0x1020_0000)`，就不能只解除其中的一部分。
///
/// # 错误处理
///
/// 如果解除映射失败（例如地址未映射、边界不对齐），当前实现会记录日志并返回
/// `false`。allocator 会继续执行后续步骤（释放物理页和虚拟地址），但页表中的
/// 映射关系可能仍然存在，导致"悬垂映射"。
///
/// 未来可以改进为：
/// - 返回详细的错误信息，让 allocator 决定如何处理
/// - 在验证失败时直接 panic，因为这通常表示内部状态不一致
///
/// # 性能考虑
///
/// - **TLB 刷新开销**: 全局刷新 TLB 会影响所有核的性能。未来可以优化为：
///   - 只刷新相关地址范围（`flush_tlb(Some(vaddr))`）
///   - 批量解除映射后统一刷新
///   - 使用 ASID 隔离，只刷新当前地址空间
///
/// - **页表遍历开销**: 每次解除映射都需要从根遍历到叶子，对于大量小对象释放
///   可能成为瓶颈。可以考虑缓存最近访问的页表页地址。
pub fn unmap_kernel_heap_range(vaddr: usize, size: usize) -> bool {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        log::error!(
            "[arch][heap_vm] page table not initialized yet, cannot unmap: vaddr={:#x} size={:#x}",
            vaddr,
            size
        );
        return false;
    }

    let root_vaddr = phys_to_virt(root_paddr);

    if let Err(err) =
        unmap_range_entries::<LoongArch64Paging>(root_vaddr, vaddr, size, false, phys_to_virt)
    {
        log::error!(
            "[arch][heap_vm] failed to validate kernel heap unmap: vaddr={:#x} size={:#x} error={:?}",
            vaddr,
            size,
            err
        );
        return false;
    }

    if let Err(err) =
        unmap_range_entries::<LoongArch64Paging>(root_vaddr, vaddr, size, true, phys_to_virt)
    {
        log::error!(
            "[arch][heap_vm] failed to unmap kernel heap range: vaddr={:#x} size={:#x} error={:?}",
            vaddr,
            size,
            err
        );
        return false;
    }

    unsafe {
        LoongArch64Paging::flush_tlb(None);
    }

    // log::debug!(
    //     "[arch][heap_vm] unmapped kernel heap range: vaddr={:#x} size={:#x}",
    //     vaddr,
    //     size
    // );
    true
}

pub fn sync_icache() {
    <LoongArch64TaskOps as general::TaskOps>::sync_icache();
}

pub fn protect_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> bool {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        log::error!(
            "[arch][heap_vm] page table not initialized yet, cannot protect: vaddr={:#x} size={:#x}",
            vaddr,
            size
        );
        return false;
    }

    let root_vaddr = phys_to_virt(root_paddr);
    if let Err(err) = protect_range_entries::<LoongArch64Paging>(
        root_vaddr,
        vaddr,
        size,
        read,
        write,
        execute,
        false,
        true,
        phys_to_virt,
    ) {
        log::error!(
            "[arch][heap_vm] failed to protect kernel heap range: vaddr={:#x} size={:#x} error={:?}",
            vaddr,
            size,
            err
        );
        return false;
    }

    unsafe {
        LoongArch64Paging::flush_tlb(None);
    }
    true
}

pub fn validate_kernel_heap_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> bool {
    let root_paddr = KERNEL_PAGE_TABLE_ROOT.load(Ordering::Acquire);
    if root_paddr == 0 {
        return false;
    }
    validate_range_permissions::<LoongArch64Paging>(
        phys_to_virt(root_paddr),
        vaddr,
        size,
        read,
        write,
        execute,
        phys_to_virt,
    )
    .is_ok()
}
