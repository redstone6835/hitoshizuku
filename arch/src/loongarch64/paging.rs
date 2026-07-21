//! LoongArch64 分页实现。
//!
//! 本模块是 `general::PagingArch` 在 LoongArch64 上的具体落地，主要负责：
//! - 约定并编码 LoongArch64 的 PTE 位语义；
//! - 生成硬件页表遍历参数（`PWCL/PWCH/STLBPS`）；
//! - 切换地址空间（`PGDL/PGDH/ASID/CRMD`）与执行 TLB 失效；
//! - 提供启动阶段段权限自检辅助函数。
//!
//! # 约束与假设
//! - 页面大小固定为 4KiB；
//! - 页表层数由 feature 决定：`la64_pt_3level`=3 级，否则 4 级；
//! - 当前按 48 位物理地址构造 PPN 掩码；
//! - 内存属性默认使用 `MAT_CC`（一致性可缓存），并预留 `MAT_SUC` 用于 MMIO 场景。

use general::{PagingArch, PhysPageTableRoot, VirtAddr};

use crate::*;

// ---------------------------
// 地址格式与页表基本参数
// ---------------------------

/// 物理地址位宽（physical address length）。
const PALEN: usize = 48;
/// 页大小位移，4KiB 页对应 12。
const PAGE_SHIFT: usize = 12;
/// 单级页表索引掩码，9 位索引对应 512 项。
const VPN_MASK: usize = 0x1ff;
/// 从物理地址中提取 PPN 部分的掩码（去掉页内偏移位）。
const PPN_MASK: usize = ((1usize << PALEN) - 1) & !((1usize << PAGE_SHIFT) - 1);

/// 页表基地址最低位移（与页大小一致）。
const PAGE_TABLE_BASE_SHIFT: usize = 12;
/// 每级页表索引位宽。
const PAGE_TABLE_INDEX_BITS: usize = 9;
/// LoongArch 64-bit PTE 宽度编码（0 表示 64bit）。
const PAGE_TABLE_PTE_WIDTH_64: usize = 0;

const LA64_PAGE_TABLE_LEVELS: usize = 4;
const SUPPORTED_LEAF_LEVELS: [usize; 3] = [1, 2, 3];

// ---------------------------
// PWCL/PWCH 字段位移定义
// ---------------------------
// 这些常量用于拼装硬件页表遍历控制寄存器。

const PWCL_PTBASE_SHIFT: usize = 0;
const PWCL_PTWIDTH_SHIFT: usize = 5;
const PWCL_DIR1_BASE_SHIFT: usize = 10;
const PWCL_DIR1_WIDTH_SHIFT: usize = 15;
const PWCL_DIR2_BASE_SHIFT: usize = 20;
const PWCL_DIR2_WIDTH_SHIFT: usize = 25;
const PWCL_PTE_WIDTH_SHIFT: usize = 30;

/// 4 级页表时，PWCH 中第 3 级目录字段起始位。
const PWCH_DIR3_BASE_SHIFT: usize = 0;
/// 4 级页表时，PWCH 中第 3 级目录字段宽度位移。
const PWCH_DIR3_WIDTH_SHIFT: usize = 6;

// ---------------------------
// PTE 位定义
// ---------------------------

/// Valid：PTE 有效位。
const PTE_V: usize = 1 << 0;
/// Dirty：硬件写使能相关位。
const PTE_D: usize = 1 << 1;
/// PLV 字段起始位。
const PTE_PLV_SHIFT: usize = 2;
/// MAT 字段起始位（内存属性）。
const PTE_MAT_SHIFT: usize = 4;
/// 普通 PTE 的 Global 位。
const PTE_G: usize = 1 << 6;
/// 目录项中 huge-page PTE 的 Huge 标记位。
///
/// LoongArch64 复用 bit 6：普通末级 PTE 中是 `G`，目录级 huge PTE 中是 `HUGE`。
const PTE_HUGE: usize = 1 << 6;
/// Present/Leaf 标记位（本实现用于叶子判定）。
const PTE_P: usize = 1 << 7;
/// Write：软件写权限位。
const PTE_W: usize = 1 << 8;
/// huge-page PTE 的 Global 位。
///
/// 官方格式中 huge-page PTE 的 `G` 位位于 bit 12，而不是普通 PTE 的 bit 6。
const PTE_HGLOBAL: usize = 1 << 12;
/// Not Readable：不可读位。
const PTE_NR: usize = 1 << 61;
/// Not eXecutable：不可执行位。
const PTE_NX: usize = 1 << 62;

/// MAT=CC（一致性可缓存）。
const MAT_CC: usize = 1;
/// MAT=SUC（强顺序非缓存，常用于 MMIO）。
const MAT_SUC: usize = 0;
/// MAT 字段位宽掩码。
const MAT_MASK: usize = 0b11;

// 启动阶段段权限断言所用位掩码。
const FLAG_W: usize = 1 << 8;
const FLAG_D: usize = 1 << 1;
const FLAG_NR: usize = 1 << 61;
const FLAG_NX: usize = 1 << 62;

// 编译期配置约束，避免“参数改了但位编码规则没同步”导致静默错误。
const _: () = {
    // 当前实现假设 4KiB 页与 9bit 逐级索引。
    assert!(PAGE_TABLE_BASE_SHIFT == 12);
    assert!(PAGE_TABLE_INDEX_BITS == 9);
    // 目前 PPN 掩码按 48-bit 物理地址构造。
    assert!(PALEN <= 48);
};

/// 判断当前硬件地址空间是否已经与目标完全一致。
///
/// `CSR_ASID` 还包含硬件实现的 ASID 位宽等只读字段，比较前必须只保留实际 ASID 域。
#[inline]
const fn activation_state_matches(
    current_pgdl: usize,
    current_pgdh: usize,
    current_asid: usize,
    target_pgdl: usize,
    target_pgdh: usize,
    target_asid: usize,
) -> bool {
    current_pgdl == target_pgdl
        && current_pgdh == target_pgdh
        && asid_bits(current_asid) == asid_bits(target_asid)
}

// 编译期覆盖完全相同、每一项单独变化以及 ASID 高位被规范化的判定语义。
const _: () = {
    assert!(activation_state_matches(
        0x1000,
        0x2000,
        0x00ff_0007,
        0x1000,
        0x2000,
        0x0407,
    ));
    assert!(!activation_state_matches(
        0x1000, 0x2000, 7, 0x3000, 0x2000, 7,
    ));
    assert!(!activation_state_matches(
        0x1000, 0x2000, 7, 0x1000, 0x3000, 7,
    ));
    assert!(!activation_state_matches(
        0x1000, 0x2000, 7, 0x1000, 0x2000, 8,
    ));
};

/// 叶子映射权限的内部聚合表示。
///
/// 仅在构造 PTE 时使用，避免接口层直接处理位操作。
#[derive(Clone, Copy)]
struct MapPermBits {
    /// 是否可读。
    read: bool,
    /// 是否可写。
    write: bool,
    /// 是否可执行。
    execute: bool,
    /// 是否允许用户态访问。
    user: bool,
    /// 是否为全局映射（不随 ASID 切换失效）。
    global: bool,
}

/// LoongArch64 的原始页表项封装。
///
/// 采用透明包装以保留底层位级布局，并在 trait 边界上提供类型安全。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct LoongArch64Pte(pub usize);

/// LoongArch64 的原始权限位封装。
///
/// 与 `LoongArch64Pte` 分离是为了在“地址位/权限位”语义上保持清晰。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct LoongArch64Flags(pub usize);

/// LoongArch64 分页实现体。
pub struct LoongArch64Paging;

impl LoongArch64Flags {
    #[inline]
    /// 返回底层位模式。
    pub const fn bits(self) -> usize {
        self.0
    }
}

impl LoongArch64Pte {
    #[inline]
    /// 返回底层位模式。
    pub const fn bits(self) -> usize {
        self.0
    }
}

impl LoongArch64Paging {
    #[inline]
    /// 页表修改后的本地可见性屏障。
    ///
    /// LoongArch64 上，软件写入页表内存、随后执行 `invtlb` 或切换页表根时，必须考虑
    /// 页表写入是否已经按预期对后续页表硬件遍历可见。这里使用 `dbar 0` 作为保守顺序点，
    /// 确保先前页表写入不会被拖到后续 TLB/CSR 操作之后。
    fn page_table_barrier() {
        unsafe {
            core::arch::asm!("dbar 0", options(nostack, preserves_flags));
        }
    }

    #[inline]
    /// 判断某一层是否允许作为叶子映射层。
    ///
    /// 这个判断把“当前是 3 级还是 4 级页表”的编译期差异收束成一个运行时布尔条件，
    /// 避免上层映射代码到处散落层级特判。
    fn is_supported_leaf_level(level: usize) -> bool {
        SUPPORTED_LEAF_LEVELS.contains(&level)
    }

    #[inline]
    /// 计算给定层级叶子映射覆盖的页大小。
    ///
    /// 在 4KiB 基页和每级 9 位索引的配置下，页大小只由“该层离末级还有几级目录”
    /// 决定，因此可以直接通过位移推导。
    fn level_page_size_const(level: usize) -> Option<usize> {
        if !Self::is_supported_leaf_level(level) || level >= LA64_PAGE_TABLE_LEVELS {
            return None;
        }
        let shift = PAGE_SHIFT + PAGE_TABLE_INDEX_BITS * (LA64_PAGE_TABLE_LEVELS - 1 - level);
        Some(1usize << shift)
    }

    #[inline]
    /// 判断某层叶子是否属于目录级大页。
    ///
    /// 对 LoongArch64 来说，最末级叶子和目录级 huge leaf 的编码并不完全一致，特别是
    /// global 位的位置不同。因此这里单独保留一个判断入口。
    const fn is_huge_leaf_level_const(level: usize) -> bool {
        level + 1 < LA64_PAGE_TABLE_LEVELS
    }

    #[inline]
    /// 查询 CPU 是否声明支持 huge page。
    ///
    /// 当前代码只把它作为“是否允许尝试 2MiB 映射”的前置条件。在 `PreferLarge`
    /// 模式下，即使这里返回 false，上层也仍可平滑回落到 base page。
    pub fn supports_huge_pages() -> bool {
        read_cpucfg_word(CPUCFG_WORD1) & CPUCFG1_HP != 0
    }

    #[inline]
    /// 读取硬件报告的 VALEN（返回值为真实位数）。
    fn hardware_valen() -> Option<usize> {
        let word = read_cpucfg_word(CPUCFG_WORD1);
        let valen = ((word >> CPUCFG1_VALEN_SHIFT) & CPUCFG1_VALEN_MASK) + 1;
        if (1..=usize::BITS as usize).contains(&valen) {
            Some(valen)
        } else {
            None
        }
    }

    #[inline]
    /// 计算 canonical 检查使用的 VALEN。
    ///
    /// - 上限来自当前软件页表层级可覆盖位宽；
    /// - 优先参考硬件 `CPUCFG.1.VALEN`，避免与实现不匹配。
    fn canonical_valen() -> usize {
        let software_valen = PAGE_SHIFT + PAGE_TABLE_INDEX_BITS * LA64_PAGE_TABLE_LEVELS;
        match Self::hardware_valen() {
            Some(hardware_valen) => core::cmp::min(hardware_valen, software_valen),
            None => software_valen,
        }
    }

    #[inline]
    /// 返回当前软件实际采用的 canonical 地址位宽。
    ///
    /// 它取“硬件 `VALEN`”与“当前软件页表层级最多能表达的位宽”二者较小值，
    /// 避免软件错误地把硬件或页表根本无法覆盖的虚拟地址视为 canonical。
    pub fn effective_valen() -> usize {
        Self::canonical_valen()
    }

    /// 构造 `CSR_PWCL` 值（4 级页表时，低 3 级信息仍放在 PWCL）。
    const fn page_walk_pwcl() -> usize {
        (PAGE_TABLE_BASE_SHIFT << PWCL_PTBASE_SHIFT)
            | (PAGE_TABLE_INDEX_BITS << PWCL_PTWIDTH_SHIFT)
            | ((PAGE_TABLE_BASE_SHIFT + PAGE_TABLE_INDEX_BITS) << PWCL_DIR1_BASE_SHIFT)
            | (PAGE_TABLE_INDEX_BITS << PWCL_DIR1_WIDTH_SHIFT)
            | ((PAGE_TABLE_BASE_SHIFT + PAGE_TABLE_INDEX_BITS * 2) << PWCL_DIR2_BASE_SHIFT)
            | (PAGE_TABLE_INDEX_BITS << PWCL_DIR2_WIDTH_SHIFT)
            | (PAGE_TABLE_PTE_WIDTH_64 << PWCL_PTE_WIDTH_SHIFT)
    }

    /// 构造 `CSR_PWCH` 值（4 级页表）。
    ///
    /// 当前仅编码第 3 级目录字段，其余保留位保持为 0。
    const fn page_walk_pwch() -> usize {
        ((PAGE_TABLE_BASE_SHIFT + PAGE_TABLE_INDEX_BITS * 3) << PWCH_DIR3_BASE_SHIFT)
            | (PAGE_TABLE_INDEX_BITS << PWCH_DIR3_WIDTH_SHIFT)
    }

    /// 返回 STLB 页大小编码（4KiB）。
    const fn stlb_page_size() -> usize {
        PAGE_TABLE_BASE_SHIFT
    }

    /// 从物理地址中提取并对齐 PPN 位。
    const fn make_ppn_bits(paddr: usize) -> usize {
        paddr & PPN_MASK
    }

    #[inline]
    /// 以指定 MAT 生成叶子 PTE。
    ///
    /// 该函数生成普通末级 PTE。普通页的 `G` 位位于 bit 6。
    const fn make_leaf_pte_with_mat_const(
        paddr: usize,
        perm: MapPermBits,
        mat: usize,
    ) -> LoongArch64Pte {
        let mut bits = Self::common_leaf_bits(perm, mat);
        if perm.global {
            bits |= PTE_G;
        }
        LoongArch64Pte(Self::make_ppn_bits(paddr) | bits)
    }

    #[inline]
    /// 生成目录级 huge-page PTE。
    ///
    /// LoongArch64 的 huge-page PTE 存在目录项中，bit 6 表示 `HUGE`；
    /// `G` 位移动到 bit 12，避免与 `HUGE` 标志冲突。
    const fn make_huge_leaf_pte_with_mat_const(
        paddr: usize,
        perm: MapPermBits,
        mat: usize,
    ) -> LoongArch64Pte {
        let mut bits = Self::common_leaf_bits(perm, mat) | PTE_HUGE;
        if perm.global {
            bits |= PTE_HGLOBAL;
        }
        LoongArch64Pte((Self::make_ppn_bits(paddr) & !PTE_HGLOBAL) | bits)
    }

    /// 生成叶子 PTE 的公共权限位（不含 PPN）。
    ///
    /// 约定：
    /// - 可写需同时置位 `PTE_W|PTE_D`；
    /// - 不可读使用 `PTE_NR`；
    /// - 不可执行使用 `PTE_NX`；
    /// - `user=true` 时 PLV 设为 3。
    const fn common_leaf_bits(perm: MapPermBits, mat: usize) -> usize {
        // 修复：硬件层面写保护依赖 PTE_D。可写时必须同时置位 W 和 D。
        let mut bits = PTE_P | PTE_V;
        if perm.write {
            bits |= PTE_W | PTE_D;
        }

        bits |= (mat & MAT_MASK) << PTE_MAT_SHIFT;

        if perm.user {
            bits |= 3 << PTE_PLV_SHIFT;
        }
        if !perm.read {
            bits |= PTE_NR;
        }
        if !perm.execute {
            bits |= PTE_NX;
        }

        bits
    }

    #[inline]
    /// 返回无效 PTE（全 0）。
    pub const fn invalid_pte_const() -> LoongArch64Pte {
        LoongArch64Pte(0)
    }

    #[inline]
    /// 构造“指向下一层页表”的非叶子目录项。
    ///
    /// LoongArch 的 `lddir` 按目录项中的页对齐物理地址继续硬件页表遍历。
    /// 非叶子目录项不能混入 `V/G/MAT` 等 leaf/TLB 权限位。
    pub const fn make_table_pte_const(next_table: usize) -> LoongArch64Pte {
        LoongArch64Pte(Self::make_ppn_bits(next_table))
    }

    #[inline]
    /// 使用默认 `MAT_CC` 构造普通叶子 PTE。
    const fn make_leaf_pte_const(paddr: usize, perm: MapPermBits) -> LoongArch64Pte {
        Self::make_leaf_pte_with_mat_const(paddr, perm, MAT_CC)
    }

    #[inline]
    /// 使用 `MAT_SUC` 构造叶子 PTE（适用于强顺序、非缓存场景）。
    ///
    /// 提供公开参数版，避免暴露内部 `MapPermBits` 类型。
    pub const fn make_leaf_pte_suc_const(
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> LoongArch64Pte {
        Self::make_leaf_pte_with_mat_const(
            paddr,
            MapPermBits {
                read,
                write,
                execute,
                user,
                global,
            },
            MAT_SUC,
        )
    }

    #[inline]
    /// 返回用于标记“页表项有效”的最小位模式。
    pub const fn table_marker_bits() -> usize {
        // LoongArch 非叶子目录项没有有效位，非零页对齐物理地址即表示存在。
        0
    }

    #[inline]
    /// `CRMD.PG` 位掩码。
    const fn crmd_pg_mask() -> usize {
        1usize << CSR_CRMD_PG_OFFSET
    }

    #[inline]
    /// `CRMD.DA` 位掩码。
    const fn crmd_da_mask() -> usize {
        1usize << CSR_CRMD_DA_OFFSET
    }

    #[inline]
    /// 读取当前 CSR_ASID 的 ASID 字段。
    ///
    /// 这个值会被页表激活和按当前地址空间刷新 TLB 的路径复用，因此单独提供一个轻量
    /// 读取入口。
    pub fn current_asid() -> usize {
        let asid: usize;
        unsafe {
            core::arch::asm!(
                "csrrd {asid}, {csr_asid}",
                asid = out(reg) asid,
                csr_asid = const CSR_ASID,
                options(nostack, preserves_flags)
            )
        }
        asid_bits(asid)
    }

    /// 激活页表并设置 ASID（仅修改 CSR_ASID 的 ASID 域）。
    ///
    /// # Safety
    ///
    /// 与 `PagingArch::activate` 相同，且调用方需保证 `asid` 与地址空间匹配。
    ///
    /// 实现步骤：
    /// 1. 写入 `PGDL/PGDH`；
    /// 2. 更新 `ASID`；
    /// 3. 写入页表遍历控制寄存器；
    /// 4. 切换 `CRMD` 到分页模式并刷新 TLB。
    ///
    /// 这里最关键的硬件语义是 `DA` 与 `PG` 的配合：
    ///
    /// - 早期启动通常运行在 `DA=1, PG=0`，主要依赖 DMW；
    /// - 正式页表准备好后切到 `DA=0, PG=1`，让普通虚拟地址走页表翻译；
    /// - 切换之后立即刷新 TLB，避免旧翻译继续被命中。
    ///
    /// 该兼容入口把低半区和高半区都绑定到同一个根。用户地址空间应调用
    /// [`Self::activate_with_asid_roots`]，让私有用户根与全局内核根保持分离。
    #[inline]
    pub unsafe fn activate_with_asid(root: PhysPageTableRoot, asid: usize) {
        unsafe { Self::activate_with_asid_roots(root, root, asid) };
    }

    /// 使用独立的低半区和高半区页表根激活地址空间。
    ///
    /// LoongArch64 根据虚拟地址最高有效位选择 `PGDL` 或 `PGDH`。用户上下文将
    /// `low_root` 设为进程私有根、`high_root` 设为全局内核根，使运行期新增的内核堆
    /// 映射立即出现在所有地址空间中，无需复制并维护每个用户 PGD 的高半区目录项。
    ///
    /// # Safety
    ///
    /// 两个根都必须指向遵循当前 `PWCL/PWCH` 配置的有效页表，`asid` 必须与
    /// `low_root` 所属地址空间匹配。调用期间必须处于允许切换当前地址空间的边界。
    #[inline]
    pub unsafe fn activate_with_asid_roots(
        low_root: PhysPageTableRoot,
        high_root: PhysPageTableRoot,
        asid: usize,
    ) {
        let pgdl = low_root.as_usize();
        let pgdh = high_root.as_usize();
        let asid_val = asid_bits(asid);
        let current_pgdl: usize;
        let current_pgdh: usize;
        let current_asid: usize;
        unsafe {
            core::arch::asm!(
                "csrrd {current_pgdl}, {csr_pgdl}",
                "csrrd {current_pgdh}, {csr_pgdh}",
                "csrrd {current_asid}, {csr_asid}",
                current_pgdl = out(reg) current_pgdl,
                current_pgdh = out(reg) current_pgdh,
                current_asid = out(reg) current_asid,
                csr_pgdl = const CSR_PGDL,
                csr_pgdh = const CSR_PGDH,
                csr_asid = const CSR_ASID,
                options(nostack, preserves_flags)
            );
        }
        if activation_state_matches(
            current_pgdl,
            current_pgdh,
            current_asid,
            pgdl,
            pgdh,
            asid_val,
        ) {
            // pthread 等同地址空间切换无需重复写 CSR，更不能承担一次全局 TLB 失效。
            return;
        }

        Self::page_table_barrier();
        // 注意：LoongArch 的 csrwr/csrxchg 会把旧 CSR 值写回 rd。
        // 因此每条写 CSR 指令都使用独立 rd 输入并声明为 inout，避免寄存器污染。
        let asid_mask = CSR_ASID_ASID_MASK;
        let pwcl = Self::page_walk_pwcl();
        let pwch = Self::page_walk_pwch();
        let stlbps = Self::stlb_page_size();
        let mut crmd: usize;
        unsafe {
            core::arch::asm!(
                // 分别写入低半区和高半区页全局目录根。
                "csrwr {pgdl}, {csr_pgdl}",
                "csrwr {pgdh}, {csr_pgdh}",
                // 仅交换 CSR_ASID 的 ASID 域。
                "csrxchg {asid_val}, {asid_mask}, {csr_asid}",
                // 配置硬件页表遍历参数。
                "csrwr {pwcl}, {csr_pwcl}",
                "csrwr {pwch}, {csr_pwch}",
                "csrwr {stlbps}, {csr_stlbps}",
                // 读取 CRMD 用于后续修改 PG/DA 位。
                "csrrd {crmd}, {csr_crmd}",
                pgdl = inout(reg) pgdl => _,
                pgdh = inout(reg) pgdh => _,
                asid_val = inout(reg) asid_val => _,
                asid_mask = in(reg) asid_mask,
                pwcl = inout(reg) pwcl => _,
                pwch = inout(reg) pwch => _,
                stlbps = inout(reg) stlbps => _,
                crmd = lateout(reg) crmd,
                csr_pgdl = const CSR_PGDL,
                csr_pgdh = const CSR_PGDH,
                csr_asid = const CSR_ASID,
                csr_pwcl = const CSR_PWCL,
                csr_pwch = const CSR_PWCH,
                csr_stlbps = const CSR_STLBPS,
                csr_crmd = const CSR_CRMD,
                options(nostack, preserves_flags)
            );
            // 打开分页 PG=1，关闭直接映射 DA=0。
            crmd = (crmd | Self::crmd_pg_mask()) & !Self::crmd_da_mask();
            core::arch::asm!(
                "csrwr {crmd}, {csr_crmd}",
                // 保守策略：全局刷新当前核 TLB。
                "invtlb 0x0, $zero, $zero",
                crmd = inout(reg) crmd => _,
                csr_crmd = const CSR_CRMD,
                options(nostack, preserves_flags)
            );
        }
    }

    /// 按指定 ASID 同步刷新所有在线 CPU 的 TLB。
    ///
    /// - `vaddr = Some`：每个在线 CPU 执行 `invtlb 0x5, asid, va`；
    /// - `vaddr = None`：每个在线 CPU 执行 `invtlb 0x0, 0, 0`，包含 global 映射。
    ///
    /// 发布方等待全部目标 CPU 确认后才返回，因此调用者可在返回后安全回收旧映射。
    ///
    /// # Safety
    ///
    /// 调用者必须保证对应页表修改已完成，且该 ASID 与目标地址空间一致。
    ///
    /// `heap_vm` 在撤销映射和收紧权限后采用该同步接口；新映射使用本核发布加缺页
    /// 代次收敛，不在任意 allocator 调用方锁内等待远端 CPU。
    #[inline]
    pub unsafe fn flush_tlb_with_asid(asid: usize, vaddr: Option<VirtAddr>) {
        crate::loongarch64::smp::flush_tlb_all_cpus(asid, vaddr.map(VirtAddr::as_usize));
    }

    /// 仅在指定逻辑 CPU 集合上同步失效该 ASID 的 TLB。
    ///
    /// 当前 CPU 始终执行本地失效；`targets` 只约束远端通知。调用方通常传入地址
    /// 空间生命周期内单调增长的激活 CPU 位图，使从未运行过该地址空间的 CPU 不会
    /// 阻塞页表更新。
    ///
    /// # Safety
    ///
    /// 调用者必须保证 `targets` 包含所有可能缓存过该 ASID translation 的 CPU。
    #[inline]
    pub unsafe fn flush_tlb_with_asid_on_cpus(
        asid: usize,
        vaddr: Option<VirtAddr>,
        targets: usize,
    ) {
        crate::loongarch64::smp::flush_tlb_on_cpus(asid, vaddr.map(VirtAddr::as_usize), targets);
    }

    /// 使用当前 CSR_ASID 同步刷新所有在线 CPU 的 TLB。
    ///
    /// # Safety
    ///
    /// 同 `flush_tlb_with_asid`。
    #[inline]
    pub unsafe fn flush_tlb_current_asid(vaddr: Option<VirtAddr>) {
        unsafe {
            Self::flush_tlb_with_asid(Self::current_asid(), vaddr);
        }
    }
}

impl PagingArch for LoongArch64Paging {
    type Pte = LoongArch64Pte;
    type Flags = LoongArch64Flags;

    const PAGE_SIZE: usize = 4096;
    const LEVELS: usize = LA64_PAGE_TABLE_LEVELS;
    /// 单级页表项数：2^9 = 512。
    const ENTRIES_PER_TABLE: usize = 512;

    /// 判断虚拟地址是否为“规范地址”。
    ///
    /// 这里把 LoongArch DMW 高位窗口也视作合法虚拟地址。
    /// 对于非 DMW 地址，要求其高位满足严格符号扩展。
    /// 注意：DMW 区域访问可绕过常规页表翻译。
    fn is_canonical_vaddr(vaddr: usize) -> bool {
        // LoongArch DMW 窗口 (0x8..=0xb) 是合法的内核虚拟区域映射
        let top_nibble = vaddr >> 60;
        if (0x8..=0xb).contains(&top_nibble) {
            return true;
        }

        let valen = Self::canonical_valen();
        if valen >= usize::BITS as usize {
            return true;
        }

        // 非 DMW 地址需满足 [63:VALEN] 对 bit[VALEN-1] 的符号扩展约束。
        let sign = (vaddr >> (valen - 1)) & 1;
        let upper = vaddr >> valen;
        (upper == 0 && sign == 0) || (upper == (usize::MAX >> valen) && sign == 1)
    }

    #[inline]
    /// 计算给定层级在 `vaddr` 中对应的页表索引。
    fn level_index(vaddr: usize, level: usize) -> usize {
        let shift = PAGE_SHIFT + 9 * (Self::LEVELS - 1 - level);
        (vaddr >> shift) & VPN_MASK
    }

    #[inline]
    /// 返回无效页表项。
    fn invalid_pte() -> Self::Pte {
        Self::invalid_pte_const()
    }

    #[inline]
    /// 判断页表项是否有效。
    ///
    /// LoongArch64 的非叶子目录项没有单独的 Present 位，只要目录项中携带了非零、
    /// 页对齐的下一层物理地址，硬件就可以继续沿 `lddir` 遍历。因此这里把“非零”
    /// 作为统一有效性判断。
    fn pte_is_valid(pte: Self::Pte) -> bool {
        // 非叶子目录项是纯物理地址，不带 `PTE_V`；leaf PTE 才使用 `PTE_V`。
        pte.0 != 0
    }

    #[inline]
    /// 判断页表项是否为叶子映射。
    ///
    /// 本实现使用 `(V|P)` 共同存在来表示叶子项。
    /// 这相当于在软件层为“目录项”和“叶子项”补了一个稳定判别条件。
    fn pte_is_leaf(pte: Self::Pte) -> bool {
        (pte.0 & (PTE_V | PTE_P)) == (PTE_V | PTE_P)
    }

    #[inline]
    /// 从页表项中提取物理页基地址。
    ///
    /// 对目录项来说，这是下一层页表页物理地址；对叶子项来说，这是映射页框基址。
    /// 两者在硬件上复用同一组 PPN 位，因此统一走同一个提取逻辑。
    fn pte_addr(pte: Self::Pte) -> usize {
        pte.0 & PPN_MASK
    }

    #[inline]
    /// 提取权限位视图。
    fn pte_flags(pte: Self::Pte) -> Self::Flags {
        LoongArch64Flags(pte.0)
    }

    #[inline]
    /// 检查可读权限。
    fn flags_readable(flags: Self::Flags) -> bool {
        flags.0 & PTE_NR == 0
    }

    #[inline]
    /// 检查可写权限。
    ///
    /// 可写必须同时满足 `W=1` 且 `D=1`。
    fn flags_writable(flags: Self::Flags) -> bool {
        (flags.0 & (PTE_W | PTE_D)) == (PTE_W | PTE_D)
    }

    #[inline]
    /// 检查可执行权限。
    fn flags_executable(flags: Self::Flags) -> bool {
        flags.0 & PTE_NX == 0
    }

    #[inline]
    /// 检查是否允许用户态访问（PLV=3）。
    fn flags_user_accessible(flags: Self::Flags) -> bool {
        ((flags.0 >> PTE_PLV_SHIFT) & 0b11) == 0b11
    }

    #[inline]
    /// 检查是否为全局映射。
    ///
    /// 当前这里只检查普通 leaf 的 `PTE_G`。目录级 huge leaf 使用的是 `PTE_HGLOBAL`，
    /// 这一点说明当前权限抽象已经可用，但还没有把 huge leaf 的 global 语义完全统一。
    fn flags_global(flags: Self::Flags) -> bool {
        flags.0 & PTE_G != 0
    }

    #[inline]
    /// 构造页表指针项（非叶子项）。
    fn make_table_pte(next_table: usize) -> Self::Pte {
        Self::make_table_pte_const(next_table)
    }

    #[inline]
    fn is_valid_leaf_perm(
        read: bool,
        write: bool,
        execute: bool,
        _user: bool,
        _global: bool,
    ) -> bool {
        // PROT_NONE 仍需要一个有效 leaf 来保留物理页身份；硬件权限位会把
        // 读/写/执行都挡掉。
        let _ = (read, write, execute);
        true
    }

    #[inline]
    /// 构造普通内存叶子映射（默认 `MAT_CC`）。
    fn make_leaf_pte(
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> Self::Pte {
        Self::make_leaf_pte_const(
            paddr,
            MapPermBits {
                read,
                write,
                execute,
                user,
                global,
            },
        )
    }

    #[inline]
    fn supported_leaf_levels() -> &'static [usize] {
        &SUPPORTED_LEAF_LEVELS
    }

    #[inline]
    fn leaf_page_size(level: usize) -> Option<usize> {
        Self::level_page_size_const(level)
    }

    #[inline]
    fn make_leaf_pte_for_level(
        level: usize,
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> Option<Self::Pte> {
        let page_size = Self::level_page_size_const(level)?;
        if paddr & (page_size - 1) != 0 {
            return None;
        }
        let perm = MapPermBits {
            read,
            write,
            execute,
            user,
            global,
        };
        if Self::is_huge_leaf_level_const(level) {
            Some(Self::make_huge_leaf_pte_with_mat_const(paddr, perm, MAT_CC))
        } else {
            Some(Self::make_leaf_pte_const(paddr, perm))
        }
    }

    /// 激活页表（保持当前 ASID）。
    unsafe fn activate(root: PhysPageTableRoot) {
        unsafe {
            Self::activate_with_asid(root, Self::current_asid());
        }
    }

    /// 按当前 ASID 刷新 TLB。
    #[inline]
    fn pte_to_usize(pte: Self::Pte) -> usize {
        pte.0
    }

    #[inline]
    fn pte_from_usize(bits: usize) -> Self::Pte {
        LoongArch64Pte(bits)
    }

    unsafe fn flush_tlb(vaddr: Option<VirtAddr>) {
        unsafe {
            Self::flush_tlb_current_asid(vaddr);
        }
    }
}

/// 启动阶段段权限自检。
///
/// 用于验证链接脚本或映射逻辑产出的段权限是否符合预期：
/// - `read` 由 `NR` 反向表示；
/// - `write` 需要同时满足 `W` 与 `D`；
/// - `exec` 由 `NX` 反向表示。
pub fn assert_segment_perm(
    name: &str,
    bits: usize,
    expect_read: bool,
    expect_write: bool,
    expect_exec: bool,
) {
    let read = bits & FLAG_NR == 0;
    let write = (bits & (FLAG_W | FLAG_D)) == (FLAG_W | FLAG_D);
    let exec = bits & FLAG_NX == 0;
    assert_eq!(
        (read, write, exec),
        (expect_read, expect_write, expect_exec),
        "{name} perm mismatch: flags={:#x}",
        bits
    );
}
