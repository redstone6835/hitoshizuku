//! LoongArch64 的特定定义和常量。
//!
//! 这个模块包含了 LoongArch64 架构特定的寄存器偏移量、异常代码、相关结构体及其实现
//! 等定义。这些是针对 LoongArch64 架构的特性和约定进行设计的，确保内核能够正确地与
//! 底层进行交互，并且在启动过程中能够正确地采集必要的信息。
//!
//! 它更像整个架构实现共享的一本“硬件语义词典”：
//!
//! - CSR 编号和位域定义集中放在这里；
//! - TrapFrame 的内存布局在这里统一声明，保证汇编和 Rust 使用同一套偏移；
//! - DMW、ASID、CPUCFG 等会被多个模块复用的概念，也统一在这里提供辅助函数。

use core::mem::offset_of;

// CSR 中各个寄存器的定义。
//
// 这些编号本身只是常量，但它们背后代表的子系统完全不同：
// - `CRMD/PRMD/EUEN` 管当前执行模式与扩展能力；
// - `PGDL/PGDH/PGD/PWCL/PWCH/STLBPS` 管页表与硬件页表遍历；
// - `EENTRY/TLBRENTRY/MERRENTRY` 管不同异常入口；
// - `MSGI*` 管核间消息中断；
// - `DMW*` 管直接映射窗口。
pub const CSR_CRMD: usize = 0x0;
pub const CSR_PRMD: usize = 0x1;
pub const CSR_EUEN: usize = 0x2;
pub const CSR_MISC: usize = 0x3;
pub const CSR_ECFG: usize = 0x4;
pub const CSR_ESTAT: usize = 0x5;
pub const CSR_ERA: usize = 0x6;
pub const CSR_BADV: usize = 0x7;
pub const CSR_BADI: usize = 0x8;
pub const CSR_EENTRY: usize = 0xc;
pub const CSR_TLBIDX: usize = 0x10;
pub const CSR_TLBEHI: usize = 0x11;
pub const CSR_TLBELO0: usize = 0x12;
pub const CSR_TLBELO1: usize = 0x13;
pub const CSR_ASID: usize = 0x18;
pub const CSR_PGDL: usize = 0x19;
pub const CSR_PGDH: usize = 0x1a;
pub const CSR_PGD: usize = 0x1b;
pub const CSR_PWCL: usize = 0x1c;
pub const CSR_PWCH: usize = 0x1d;
pub const CSR_STLBPS: usize = 0x1e;
pub const CSR_RVACFG: usize = 0x1f;
pub const CSR_CPUID: usize = 0x20;
pub const CSR_PRCFG1: usize = 0x21;
pub const CSR_PRCFG2: usize = 0x22;
pub const CSR_PRCFG3: usize = 0x23;
pub const CSR_KS0: usize = 0x30;
pub const CSR_KS1: usize = 0x31;
pub const CSR_TID: usize = 0x40;
pub const CSR_TCFG: usize = 0x41;
pub const CSR_TVAL: usize = 0x42;
pub const CSR_CNTC: usize = 0x43;
pub const CSR_TICLR: usize = 0x44;
pub const CSR_LLBCTL: usize = 0x60;
pub const CSR_TLBRENTRY: usize = 0x88;
pub const CSR_TLBRBADV: usize = 0x89;
pub const CSR_TLBRERA: usize = 0x8a;
pub const CSR_TLBRSAVE: usize = 0x8b;
pub const CSR_TLBRELO0: usize = 0x8c;
pub const CSR_TLBRELO1: usize = 0x8d;
pub const CSR_TLBREHI: usize = 0x8e;
pub const CSR_TLBRPRMD: usize = 0x8f;
pub const CSR_MERRCTL: usize = 0x90;
pub const CSR_MERRINFO1: usize = 0x91;
pub const CSR_MERRINFO2: usize = 0x92;
pub const CSR_MERRENTRY: usize = 0x93;
pub const CSR_MERRERA: usize = 0x94;
pub const CSR_MERRSAVE: usize = 0x95;
pub const CSR_CTAG: usize = 0x98;
pub const CSR_MSGIS0: usize = 0xa0;
pub const CSR_MSGIS1: usize = 0xa1;
pub const CSR_MSGIS2: usize = 0xa2;
pub const CSR_MSGIS3: usize = 0xa3;
pub const CSR_MSGIR: usize = 0xa4;
pub const CSR_MSGIE: usize = 0xa5;
pub const CSR_DMW0: usize = 0x180;
pub const CSR_DMW1: usize = 0x181;
pub const CSR_DMW2: usize = 0x182;
pub const CSR_DMW3: usize = 0x183;
pub const CSR_PMCFG0: usize = 0x200;
pub const CSR_PMCNT0: usize = 0x201;
pub const CSR_MWPC: usize = 0x300;
pub const CSR_MWPS: usize = 0x301;
pub const CSR_FWPC: usize = 0x380;
pub const CSR_FWPS: usize = 0x381;
pub const CSR_DBG: usize = 0x500;
pub const CSR_DERA: usize = 0x501;
pub const CSR_DSAVE: usize = 0x502;

// CSR_PRMD 寄存器中权限等级的掩码。
pub const CSR_PRMD_PPLV_MASK: usize = 0x3;
pub const CSR_PRMD_PPLV_PLV0: usize = 0x0;
pub const CSR_PRMD_PPLV_PLV1: usize = 0x1;
pub const CSR_PRMD_PPLV_PLV2: usize = 0x2;
pub const CSR_PRMD_PPLV_PLV3: usize = 0x3;

// CSR_ECFG 寄存器中 VS（向量间距）字段定义。
pub const CSR_ECFG_VS_OFFSET: usize = 16;
pub const CSR_ECFG_VS_WIDTH: usize = 3;
pub const CSR_ECFG_VS_MASK: usize = ((1usize << CSR_ECFG_VS_WIDTH) - 1) << CSR_ECFG_VS_OFFSET;

// CSR_EUEN 寄存器中扩展功能使能位字段定义。
pub const EUEN_FPE: usize = 0x1;

/// EUEN bit 4（保留位）：内核内部标志，记录异常入口时是否保存了 FPU 上下文。
/// 写入 CSR_EUEN 前必须清除。
pub const FPU_SAVED: usize = 0x10;
/// EUEN bit 5（保留位）：内核内部标志，记录异常入口时是否保存了 LSX 上下文。
/// 写入 CSR_EUEN 前必须清除。
pub const LSX_SAVED: usize = 0x20;
pub const EUEN_SXE: usize = 0x2;
pub const EUEN_ASXE: usize = 0x4;
pub const EUEN_BTE: usize = 0x8;
pub const EUEN_EXT_CONTEXT_MASK: usize = EUEN_SXE | EUEN_ASXE | EUEN_BTE;

// CSR_CRMD 寄存器中各个功能位的偏移量。
pub const CSR_CRMD_PLV_OFFSET: usize = 0;
pub const CSR_CRMD_IE_OFFSET: usize = 2;
pub const CSR_CRMD_DA_OFFSET: usize = 3;
pub const CSR_CRMD_PG_OFFSET: usize = 4;
pub const CSR_CRMD_DATF_OFFSET: usize = 5;
pub const CSR_CRMD_DATM_OFFSET: usize = 7;
pub const CSR_CRMD_WE_OFFSET: usize = 9;

// CSR_ASID 寄存器字段定义。
pub const CSR_ASID_ASID_OFFSET: usize = 0;
pub const CSR_ASID_ASID_WIDTH: usize = 10;
pub const CSR_ASID_ASID_MASK: usize = (1usize << CSR_ASID_ASID_WIDTH) - 1;
pub const CSR_ASID_BIT_OFFSET: usize = 16;
pub const CSR_ASID_BIT_WIDTH: usize = 8;
pub const CSR_ASID_BIT_MASK: usize = ((1usize << CSR_ASID_BIT_WIDTH) - 1) << CSR_ASID_BIT_OFFSET;

// CSR_CPUID 寄存器字段定义。
pub const CSR_CPUID_COREID_WIDTH: usize = 9;
pub const CSR_CPUID_COREID_MASK: usize = (1 << CSR_CPUID_COREID_WIDTH) - 1;

// CSR_MSGIR 编码字段（预留给多核消息中断 API）。
pub const CSR_MSGIR_DATA_OFFSET: usize = 0;
pub const CSR_MSGIR_DATA_WIDTH: usize = 16;
pub const CSR_MSGIR_DATA_MASK: usize = (1usize << CSR_MSGIR_DATA_WIDTH) - 1;
pub const CSR_MSGIR_CPU_OFFSET: usize = 16;

/// 将 ASID 规范化为 CSR_ASID 可写入位宽。
///
/// 硬件可实现的 ASID 位宽是有限的，这里统一做掩码，避免上层把任意 `usize`
/// 直接写入 CSR 时污染保留位。
#[inline]
pub const fn asid_bits(asid: usize) -> usize {
    asid & CSR_ASID_ASID_MASK
}

/// 编码写入 `CSR_MSGIR` 的值。
///
/// `CSR_MSGIR` 把“目标 CPU”和“消息数据”复合在同一个寄存器值中。把编码逻辑集中在
/// 这里，可以避免消息中断路径里到处出现位运算常量。
#[inline]
pub const fn msgir_encode(cpu_id: usize, data: usize) -> usize {
    ((cpu_id & CSR_CPUID_COREID_MASK) << CSR_MSGIR_CPU_OFFSET)
        | ((data & CSR_MSGIR_DATA_MASK) << CSR_MSGIR_DATA_OFFSET)
}

/// DMW0 线性映射窗口基址（uncached）。
pub const DMW0_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;

/// DMW1 线性映射窗口基址（cached）。
///
/// 当前内核链接在 `0x9000_0000_0000_0000` 高半区窗口内，
/// 其对应物理地址可通过减去该窗口基址得到。
pub const DMW1_CACHED_BASE: usize = 0x9000_0000_0000_0000;
pub const PHYS_ADDR_MASK: usize = 0x0000_FFFF_FFFF_FFFF;

/// CPUCFG 配置字 1（含 PALEN/VALEN 字段）。
pub const CPUCFG_WORD1: usize = 0x1;
/// CPUCFG.1 中 VALEN 字段起始位。
pub const CPUCFG1_VALEN_SHIFT: usize = 12;
/// CPUCFG.1 中 VALEN 字段位宽。
pub const CPUCFG1_VALEN_BITS: usize = 8;
/// CPUCFG.1 中 VALEN 字段掩码（其值为“位数减 1”）。
pub const CPUCFG1_VALEN_MASK: usize = (1usize << CPUCFG1_VALEN_BITS) - 1;
/// CPUCFG.1 中 HP（huge page）能力位。
pub const CPUCFG1_HP: usize = 1 << 24;
/// CPUCFG.1 中 UAL（非对齐访存）能力位。
const CPUCFG1_UAL: usize = 1 << 20;
/// LoongArch 用户 ABI 中的非对齐访存能力位。
const HWCAP_LOONGARCH_UAL: usize = 1 << 2;

/// 读取指定 CPUCFG 配置字。
///
/// `cpucfg` 返回的是实现相关能力信息。这里统一封装成函数，供分页、定时器等平台代码
/// 查询能力位。返回值被裁剪到 32 位，是因为 LA64 的 CPUCFG 语义按 32 位配置字定义，
/// 而硬件会把结果符号扩展到 GRLEN。
#[inline]
pub fn read_cpucfg_word(index: usize) -> usize {
    let mut value: usize;
    unsafe {
        core::arch::asm!(
            "cpucfg {value}, {index}",
            value = out(reg) value,
            index = in(reg) index,
            options(nostack, preserves_flags)
        );
    }
    // LA64 下 CPUCFG 配置字宽度为 32 位，指令结果会符号扩展到 GRLEN。
    // 这里统一裁剪为低 32 位，避免上半区符号位干扰字段解析。
    value as u32 as usize
}

/// 返回当前处理器可安全暴露给用户态的 LoongArch HWCAP。
pub fn user_hwcap() -> usize {
    let cpucfg1 = read_cpucfg_word(CPUCFG_WORD1);
    if cpucfg1 & CPUCFG1_UAL != 0 {
        HWCAP_LOONGARCH_UAL
    } else {
        0
    }
}

// LoongArch64 定义的异常代码（ECODE）枚举。
pub const ECODE_INT: usize = 0; // 中断
pub const ECODE_PIL: usize = 1; // 页无效异常（装入）
pub const ECODE_PIS: usize = 2; // 页无效异常（存储）
pub const ECODE_PIF: usize = 3; // 页无效异常（取指）
pub const ECODE_PME: usize = 4; // 页修改异常（写保护）
pub const ECODE_PNR: usize = 5; // 页不可读异常
pub const ECODE_PNX: usize = 6; // 页不可执行异常
pub const ECODE_PPI: usize = 7; // 页特权等级不合规
pub const ECODE_ADE: usize = 8; // 地址错误异常
pub const ECODE_ALE: usize = 9; // 地址非对齐异常
pub const ECODE_BCE: usize = 10; // 边界检查异常
pub const ECODE_SYS: usize = 11; // 系统调用异常
pub const ECODE_BRK: usize = 12; // 断点异常
pub const ECODE_INE: usize = 13; // 指令不存在异常
pub const ECODE_IPE: usize = 14; // 指令特权错误异常
pub const ECODE_FPD: usize = 15; // 浮点指令禁用异常
pub const ECODE_SXD: usize = 16; // 向量扩展禁用异常
pub const ECODE_ASXD: usize = 17; // 高级向量扩展禁用异常
pub const ECODE_FPE: usize = 18; // 浮点异常

/// 异常上下文（陷阱帧）结构。
///
/// LoongArch64 的异常上下文包含了所有通用寄存器、核心控制寄存器以及浮点寄存器的
/// 状态，以便在异常处理过程中能够完整地保存和恢复用户态程序的执行状态。
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct TrapFrame {
    // 1. 除 $r0 之外的 31 个通用寄存器
    pub ra: usize, // $r1
    pub tp: usize, // $r2
    pub sp: usize, // $r3
    pub a0: usize, // $r4
    pub a1: usize, // $r5
    pub a2: usize, // $r6
    pub a3: usize, // $r7
    pub a4: usize, // $r8
    pub a5: usize, // $r9
    pub a6: usize, // $r10
    pub a7: usize, // $r11
    pub t0: usize, // $r12
    pub t1: usize, // $r13
    pub t2: usize, // $r14
    pub t3: usize, // $r15
    pub t4: usize, // $r16
    pub t5: usize, // $r17
    pub t6: usize, // $r18
    pub t7: usize, // $r19
    pub t8: usize, // $r20
    pub rx: usize, // $r21
    pub s0: usize, // $r22
    pub s1: usize, // $r23
    pub s2: usize, // $r24
    pub s3: usize, // $r25
    pub s4: usize, // $r26
    pub s5: usize, // $r27
    pub s6: usize, // $r28
    pub s7: usize, // $r29
    pub s8: usize, // $r30
    pub s9: usize, // $r31

    // 2. 控制寄存器
    pub pc: usize,     // CSR_ERA
    pub status: usize, // CSR_PRMD
    pub euen: usize,   // CSR_EUEN
    pub llbctl: usize, // CSR_LLBCTL

    // 3. LSX 向量寄存器。显式填充使寄存器区保持 16 字节对齐。
    pub _lsx_padding: u64,
    pub lsx: [[u64; 2]; 32], // VR0..VR31

    // 4. FPU 浮点寄存器
    pub f: [u64; 32], // F0..F31
    pub fcsr: u64,    // FCSR 控制位
    pub fcc: u64,     // FPU 浮点使能标志位
}

impl TrapFrame {
    /// 获取系统调用号。
    pub fn syscall_id(&self) -> usize {
        self.a7
    }

    /// 获取系统调用传入的六个参数。
    pub fn syscall_args(&self) -> [usize; 6] {
        [self.a0, self.a1, self.a2, self.a3, self.a4, self.a5]
    }

    /// 设置系统调用的返回值。
    pub fn set_syscall_return(&mut self, value: usize) {
        self.a0 = value;
    }

    /// 跳过某一系统调用指令，继续执行后续指令。
    pub fn skip_syscall_insn(&mut self) {
        self.pc = self.pc.wrapping_add(4);
    }
}

// 陷阱帧的大小（对齐到 16 字节）。
pub const FRAME_SIZE: usize = (size_of::<TrapFrame>() + 15) & !15;

// 陷阱帧中各个寄存器的偏移量。
pub const RA_OFFSET: usize = offset_of!(TrapFrame, ra);
pub const TP_OFFSET: usize = offset_of!(TrapFrame, tp);
pub const SP_OFFSET: usize = offset_of!(TrapFrame, sp);
pub const A0_OFFSET: usize = offset_of!(TrapFrame, a0);
pub const A1_OFFSET: usize = offset_of!(TrapFrame, a1);
pub const A2_OFFSET: usize = offset_of!(TrapFrame, a2);
pub const A3_OFFSET: usize = offset_of!(TrapFrame, a3);
pub const A4_OFFSET: usize = offset_of!(TrapFrame, a4);
pub const A5_OFFSET: usize = offset_of!(TrapFrame, a5);
pub const A6_OFFSET: usize = offset_of!(TrapFrame, a6);
pub const A7_OFFSET: usize = offset_of!(TrapFrame, a7);
pub const T0_OFFSET: usize = offset_of!(TrapFrame, t0);
pub const T1_OFFSET: usize = offset_of!(TrapFrame, t1);
pub const T2_OFFSET: usize = offset_of!(TrapFrame, t2);
pub const T3_OFFSET: usize = offset_of!(TrapFrame, t3);
pub const T4_OFFSET: usize = offset_of!(TrapFrame, t4);
pub const T5_OFFSET: usize = offset_of!(TrapFrame, t5);
pub const T6_OFFSET: usize = offset_of!(TrapFrame, t6);
pub const T7_OFFSET: usize = offset_of!(TrapFrame, t7);
pub const T8_OFFSET: usize = offset_of!(TrapFrame, t8);
pub const RX_OFFSET: usize = offset_of!(TrapFrame, rx);
pub const S0_OFFSET: usize = offset_of!(TrapFrame, s0);
pub const S1_OFFSET: usize = offset_of!(TrapFrame, s1);
pub const S2_OFFSET: usize = offset_of!(TrapFrame, s2);
pub const S3_OFFSET: usize = offset_of!(TrapFrame, s3);
pub const S4_OFFSET: usize = offset_of!(TrapFrame, s4);
pub const S5_OFFSET: usize = offset_of!(TrapFrame, s5);
pub const S6_OFFSET: usize = offset_of!(TrapFrame, s6);
pub const S7_OFFSET: usize = offset_of!(TrapFrame, s7);
pub const S8_OFFSET: usize = offset_of!(TrapFrame, s8);
pub const S9_OFFSET: usize = offset_of!(TrapFrame, s9);
pub const PC_OFFSET: usize = offset_of!(TrapFrame, pc);
pub const STATUS_OFFSET: usize = offset_of!(TrapFrame, status);
pub const EUEN_OFFSET: usize = offset_of!(TrapFrame, euen);
pub const LLBCTL_OFFSET: usize = offset_of!(TrapFrame, llbctl);
pub const LSX_PADDING_OFFSET: usize = offset_of!(TrapFrame, _lsx_padding);
pub const LSX_OFFSET: usize = offset_of!(TrapFrame, lsx);
pub const F_OFFSET: usize = offset_of!(TrapFrame, f);
pub const FCSR_OFFSET: usize = offset_of!(TrapFrame, fcsr);
pub const FCC_OFFSET: usize = offset_of!(TrapFrame, fcc);

const _: () = {
    assert!(LSX_OFFSET % 16 == 0);
    assert!(align_of::<TrapFrame>() >= 16);
    assert!(FRAME_SIZE % 16 == 0);
};

/// LoongArch64 架构的 ID 标识 (用于异常分发)。
pub const ARCH_ID_LOONGARCH64: usize = 2;

/// Per-CPU 数据结构中内核异常栈的偏移量。
///
/// 假设 $tp ($r2) 指向当前 CPU 的 Per-CPU 结构体，该结构体的第 0 个字节存放该
/// 核心的内核异常栈栈顶。
pub const PER_CPU_KSTACK_OFFSET: usize = 0;

/// 默认定时器中断频率（Hz）。可通过内核命令行 `timer_hz=N` 覆盖。
pub const DEFAULT_TIMER_HZ: usize = 100;

#[inline]
/// 读取稳定计数器原始周期值。
pub fn stable_counter_raw() -> u64 {
    let cnt: u64;
    unsafe {
        core::arch::asm!(
            "rdtime.d {cnt}, $zero",
            cnt = out(reg) cnt,
        );
    }
    cnt
}

#[inline]
/// 返回稳定计数器频率（Hz）。
pub fn stable_counter_hz() -> u64 {
    super::STABLE_TIMER_HZ.load(core::sync::atomic::Ordering::Relaxed) as u64
}

#[inline]
/// 将稳定计数器周期值换算为纳秒。
pub fn stable_counter_to_ns(cnt: u64) -> u64 {
    let hz = stable_counter_hz();
    if hz == 0 {
        return 0;
    }
    // 分段计算避免 cnt * 1e9 溢出
    let secs = cnt / hz;
    let frac_ns = (cnt % hz) * 1_000_000_000 / hz;
    secs * 1_000_000_000 + frac_ns
}

#[inline]
/// 读取稳定计数器并换算为纳秒。
///
/// `rdtime.d` 返回的是硬件计数器值，不是直接的时间单位。这里结合平台初始化阶段探测
/// 得到的稳定频率，把计数值换算成纳秒时间戳。
pub fn kernel_timestamp_ns() -> u64 {
    let cnt = stable_counter_raw();
    stable_counter_to_ns(cnt)
}

#[inline]
/// 将物理地址投影到 DMW1 cached 窗口。
///
/// 当前内核在很多早期路径中靠 DMW1 直接访问普通 RAM：虚拟地址高位固定在 `0x9...`，
/// 低位保持物理地址不变，因此这里只需把窗口基址前缀并上去。
pub fn phys_to_virt(paddr: usize) -> usize {
    paddr | DMW1_CACHED_BASE
}

#[inline]
/// 从内核直映地址中提取物理地址部分。
///
/// 这条辅助函数支撑了 boot、trap、allocator 等多个子系统在“高半区直映地址”和
/// “物理地址”之间来回切换。
pub fn virt_to_phys(vaddr: usize) -> usize {
    vaddr & PHYS_ADDR_MASK
}

#[inline]
/// 将固件传来的指针规范化为当前早期内核可访问的 DMW1 虚拟地址。
///
/// 某些启动路径会把物理地址或不同窗口下的直映地址交给内核。这里先提取物理部分，
/// 再统一投影回 DMW1 cached 窗口，供 loader 在早期稳定访问固件数据。
pub fn reset_to_virt(ptr: usize) -> usize {
    phys_to_virt(virt_to_phys(ptr))
}
