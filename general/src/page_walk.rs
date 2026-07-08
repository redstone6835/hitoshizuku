//! 通用页表遍历函数。
//!
//! 本模块提供与具体 ISA 无关的页表遍历、映射创建与解除映射逻辑。
//! 这些函数以 [`PagingArch`](crate::PagingArch) trait 为抽象边界，
//! 通过调用方注入 `phys_to_virt` 和 `alloc_page` 回调来保持架构无关性。
//!
//! # 设计原则
//!
//! - **零架构依赖**：不引用任何具体架构类型，仅依赖 `PagingArch` trait。
//! - **回调注入**：物理地址转虚拟地址、页表页分配均由调用方以函数指针提供。
//! - **调用方负责策略**：大页选择、TLB 刷新时机、日志记录等策略决策留在架构层。
//!
//! # 使用示例
//!
//! ```rust,ignore
//! use general::page_walk::{find_leaf, walk_and_map, unmap_range_entries, MapError};
//! use general::PagingArch;
//!
//! let result = walk_and_map::<MyPagingArch>(
//!     root_vaddr, vaddr, paddr, target_level,
//!     true, true, false, false, true,
//!     phys_to_virt,
//!     || { /* allocate page table page */ Ok(paddr) },
//! );
//! ```

use crate::PagingArch;

/// 页表遍历过程中的错误类型。
///
/// 此枚举覆盖了页表遍历、映射创建和解除映射过程中可能遇到的所有错误条件。
#[derive(Debug, Clone, Copy)]
pub enum MapError {
    /// 物理内存不足，无法分配中间页表页。
    OutOfMemory,
    /// 虚拟地址或物理地址未按要求对齐。
    Misaligned,
    /// 目标地址已被映射，不支持覆盖已有映射。
    AlreadyMapped,
    /// 指定虚拟地址范围内存在未映射的区域。
    NotMapped,
    /// 不支持的页表层级（如请求的层级未在硬件支持列表中）。
    UnsupportedLevel,
    /// 硬件不支持请求的大页映射。
    UnsupportedHugePage,
    /// 无效的权限组合（如只写不读）。
    InvalidPermission,
}

/// 在页表中查找覆盖 `vaddr` 的现有叶子页表项。
///
/// 此函数从页表根开始遍历，逐层检查 PTE 直到找到叶子项（或遍历完所有层级）。
/// 与 [`walk_and_map`] 不同，它**不分配**新的页表页，仅查找现有映射。
///
/// # 参数
///
/// - `root_vaddr`: 页表根的虚拟地址。
/// - `vaddr`: 要查找的虚拟地址。
/// - `phys_to_virt`: 物理地址到虚拟地址的转换函数（架构提供）。
///
/// # 返回值
///
/// 成功时返回 `(level, pte_ptr, pte)` 三元组：
/// - `level`: 叶子项所在的层级（0 = 最大页）。
/// - `pte_ptr`: 叶子 PTE 的内存地址指针，可用于后续修改或清除。
/// - `pte`: 叶子 PTE 的当前值。
pub fn find_leaf<P: PagingArch>(
    root_vaddr: usize,
    vaddr: usize,
    phys_to_virt: fn(usize) -> usize,
) -> Result<(usize, *mut usize, P::Pte), MapError> {
    let mut table_vaddr = root_vaddr;

    for level in 0..P::LEVELS {
        let index = P::level_index(vaddr, level);
        let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
        let pte_bits = unsafe { core::ptr::read_volatile(pte_ptr) };
        let pte = P::pte_from_usize(pte_bits);

        if !P::pte_is_valid(pte) {
            return Err(MapError::NotMapped);
        }

        if P::pte_is_leaf(pte) {
            return Ok((level, pte_ptr, pte));
        }

        let next_table_paddr = P::pte_addr(pte);
        table_vaddr = phys_to_virt(next_table_paddr);
    }

    Err(MapError::NotMapped)
}

/// 解除映射：遍历地址范围并清除（或验证）叶子 PTE。
///
/// 此函数负责将指定虚拟地址范围的映射关系从页表中移除。与 [`walk_and_map`]
/// 相对应，它是反向操作的核心实现。
///
/// # 参数
///
/// - `root_vaddr`: 页表根的虚拟地址。
/// - `vaddr`: 要解除映射的虚拟地址起点（必须以最小页大小对齐）。
/// - `size`: 解除映射的区域大小（必须是最小页大小的整数倍）。
/// - `clear`: 是否真正清除 PTE（`true` = 清除，`false` = 仅验证边界和存在性）。
/// - `phys_to_virt`: 物理地址到虚拟地址的转换函数。
///
/// # 实现策略
///
/// 1. 对齐检查：虚拟地址和大小都必须按 [`P::PAGE_SIZE`] 对齐。
/// 2. 逐页调用 [`find_leaf`] 找到覆盖当前地址的叶子项。
/// 3. 边界验证：叶子项的边界必须与请求的范围精确对齐，不支持部分解除映射。
/// 4. 如果 `clear=true`，将叶子 PTE 设为无效值；否则仅验证。
pub fn unmap_range_entries<P: PagingArch>(
    root_vaddr: usize,
    vaddr: usize,
    size: usize,
    clear: bool,
    phys_to_virt: fn(usize) -> usize,
) -> Result<(), MapError> {
    if size == 0 || vaddr % P::PAGE_SIZE != 0 || size % P::PAGE_SIZE != 0 {
        return Err(MapError::Misaligned);
    }

    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::Misaligned)?;
    let mut current_vaddr = vaddr;

    while current_vaddr < end_vaddr {
        let (level, pte_ptr, _pte) = find_leaf::<P>(root_vaddr, current_vaddr, phys_to_virt)?;
        let page_size = P::leaf_page_size(level).ok_or(MapError::UnsupportedLevel)?;
        let leaf_base = current_vaddr & !(page_size - 1);
        let next_vaddr = current_vaddr
            .checked_add(page_size)
            .ok_or(MapError::Misaligned)?;

        if leaf_base != current_vaddr || next_vaddr > end_vaddr {
            return Err(MapError::Misaligned);
        }

        if clear {
            let invalid = P::pte_to_usize(P::invalid_pte());
            unsafe { core::ptr::write_volatile(pte_ptr, invalid) };
        }

        current_vaddr = next_vaddr;
    }

    Ok(())
}

/// 原地修改已存在叶子映射的权限。
///
/// 此函数只更新覆盖范围内的完整叶子页，不拆分大页，也不创建新映射。调用方如果需要
/// 对一个段做精确 W^X，应保证该段按最小页大小独立映射，避免和其它段共享同一叶子页。
pub fn protect_range_entries<P: PagingArch>(
    root_vaddr: usize,
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
    user: bool,
    global: bool,
    phys_to_virt: fn(usize) -> usize,
) -> Result<(), MapError> {
    if size == 0 || vaddr % P::PAGE_SIZE != 0 || size % P::PAGE_SIZE != 0 {
        return Err(MapError::Misaligned);
    }
    if !P::is_valid_leaf_perm(read, write, execute, user, global) {
        return Err(MapError::InvalidPermission);
    }

    let end_vaddr = vaddr.checked_add(size).ok_or(MapError::Misaligned)?;
    let mut current_vaddr = vaddr;

    while current_vaddr < end_vaddr {
        let (level, pte_ptr, old_pte) = find_leaf::<P>(root_vaddr, current_vaddr, phys_to_virt)?;
        let page_size = P::leaf_page_size(level).ok_or(MapError::UnsupportedLevel)?;
        let leaf_base = current_vaddr & !(page_size - 1);
        let next_vaddr = current_vaddr
            .checked_add(page_size)
            .ok_or(MapError::Misaligned)?;

        if leaf_base != current_vaddr || next_vaddr > end_vaddr {
            return Err(MapError::Misaligned);
        }

        let new_pte = P::make_leaf_pte_for_level(
            level,
            P::pte_addr(old_pte),
            read,
            write,
            execute,
            user,
            global,
        )
        .ok_or(MapError::InvalidPermission)?;
        unsafe { core::ptr::write_volatile(pte_ptr, P::pte_to_usize(new_pte)) };
        current_vaddr = next_vaddr;
    }

    Ok(())
}

/// 页表遍历并创建映射。
///
/// 这是页表操作的核心函数，负责从页表根开始逐层遍历，按需分配中间页表页，
/// 最终在目标层级写入叶子页表项。
///
/// # 参数
///
/// - `root_vaddr`: 页表根的虚拟地址。
/// - `vaddr`: 要映射的虚拟地址（单个页，需按目标页大小对齐）。
/// - `paddr`: 对应的物理地址（需按目标页大小对齐）。
/// - `target_level`: 目标映射层级（0 = 最大页）。
/// - `read/write/execute/user/global`: 页面权限标志。
/// - `phys_to_virt`: 物理地址到虚拟地址的转换函数。
/// - `alloc_page`: 分配中间页表页的回调函数，返回物理地址或在内存不足时返回错误。
///
/// # 错误情况
///
/// - `OutOfMemory`: 分配中间页表页失败。
/// - `AlreadyMapped`: 遍历路径上存在叶子项（不支持覆盖映射）。
/// - `InvalidPermission`: `target_level` 上无法构造有效 PTE。
pub fn walk_and_map<P: PagingArch>(
    root_vaddr: usize,
    vaddr: usize,
    paddr: usize,
    target_level: usize,
    read: bool,
    write: bool,
    execute: bool,
    user: bool,
    global: bool,
    phys_to_virt: fn(usize) -> usize,
    alloc_page: fn() -> Result<usize, MapError>,
) -> Result<(), MapError> {
    let mut table_vaddr = root_vaddr;

    // 遍历到目标层级
    for level in 0..target_level {
        let index = P::level_index(vaddr, level);
        let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
        let pte_bits = unsafe { core::ptr::read_volatile(pte_ptr) };
        let pte = P::pte_from_usize(pte_bits);

        if !P::pte_is_valid(pte) {
            // 分配新页表页
            let new_table_paddr = alloc_page()?;
            let new_table_vaddr = phys_to_virt(new_table_paddr);

            // 初始化为全零
            unsafe {
                core::ptr::write_bytes(new_table_vaddr as *mut u8, 0, P::PAGE_SIZE);
            }

            // 创建指向下一层的 PTE
            let new_pte = P::make_table_pte(new_table_paddr);
            unsafe { core::ptr::write_volatile(pte_ptr, P::pte_to_usize(new_pte)) };

            table_vaddr = new_table_vaddr;
        } else if P::pte_is_leaf(pte) {
            return Err(MapError::AlreadyMapped);
        } else {
            let next_table_paddr = P::pte_addr(pte);
            table_vaddr = phys_to_virt(next_table_paddr);
        }
    }

    // 在目标层级创建叶子映射
    let index = P::level_index(vaddr, target_level);
    let pte_ptr = (table_vaddr + index * core::mem::size_of::<usize>()) as *mut usize;
    let old_pte = P::pte_from_usize(unsafe { core::ptr::read_volatile(pte_ptr) });
    if P::pte_is_valid(old_pte) {
        return Err(MapError::AlreadyMapped);
    }

    let leaf_pte =
        P::make_leaf_pte_for_level(target_level, paddr, read, write, execute, user, global)
            .ok_or(MapError::InvalidPermission)?;

    unsafe { core::ptr::write_volatile(pte_ptr, P::pte_to_usize(leaf_pte)) };

    Ok(())
}
