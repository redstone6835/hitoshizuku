//! RISC-V64 S-mode CSR 编号与位域常量。
//!
//! 来源：RISC-V Privileged Specification v1.12, Chapter 4 (Supervisor-Level ISA)
//!
//! ```text
//! sstatus (CSR 0x100) 位域布局：
//!
//!   63    33 32  20 19 18  15 14 13 12  9  8  7  6  5  4  2  1  0
//!  ┌──┐  ┌──┐  ┌──┐──┬──┐──┬──┬──┐──┬──┐──┐──┐──┬──┐──┐──┬──┐──┐
//!  │SD│..│UXL│..│MXR│SUM│ 0│ FS │ VS │SPP│ 0│ 0│SPIE│ 0│ 0│SIE│ 0│
//!  └──┘  └──┘  └──┘──┴──┘──┴──┴──┘──┴──┘──┘──┘──┴──┘──┘──┴──┘──┘
//! ```

// ── CSR 编号 ──────────────────────────────────────────────────────────────────

pub const CSR_SSTATUS: usize = 0x100; // 监管者状态
pub const CSR_SIE: usize = 0x104; // 中断使能（各源独立）
pub const CSR_STVEC: usize = 0x105; // 异常入口基地址
pub const CSR_SSCRATCH: usize = 0x140; // 暂存（trap entry 栈交换）
pub const CSR_SEPC: usize = 0x141; // 异常返回地址
pub const CSR_SCAUSE: usize = 0x142; // 异常/中断原因
pub const CSR_STVAL: usize = 0x143; // 异常附加值（fault 地址等）
pub const CSR_SIP: usize = 0x144; // 中断待决
pub const CSR_SATP: usize = 0x180; // 页表根 + ASID + MODE
pub const CSR_FCSR: usize = 0x003; // 浮点控制状态

// ── sstatus 位域 ──────────────────────────────────────────────────────────────

pub const SSTATUS_SIE: usize = 1 << 1; // [1]     全局中断使能
pub const SSTATUS_SPIE: usize = 1 << 5; // [5]     trap 前 SIE 备份
pub const SSTATUS_SPP: usize = 1 << 8; // [8]     trap 前特权级 (0=U,1=S)
pub const SSTATUS_VS_MASK: usize = 3 << 9; // [10:9]  向量状态
pub const SSTATUS_VS_INITIAL: usize = 0b01 << 9; // [10:9]  VS = Initial；首次允许用户执行向量指令
pub const SSTATUS_VS_CLEAN: usize = 0b10 << 9; // [10:9]  VS = Clean；向量上下文已同步到内存
pub const SSTATUS_VS_DIRTY: usize = 0b11 << 9; // [10:9]  VS = Dirty；向量上下文需要保存
pub const SSTATUS_FS_MASK: usize = 0b11 << 13; // [14:13] 浮点状态（2-bit 提取掩码）
pub const SSTATUS_FS_INITIAL: usize = 0b01 << 13; // [14:13] FS = Initial；首次允许用户执行浮点指令
pub const SSTATUS_FS_CLEAN: usize = 0b10 << 13; // [14:13] FS = Clean；内存副本与硬件一致
pub const SSTATUS_FS_DIRTY: usize = 0b11 << 13; // [14:13] FS = Dirty；数值恰等于 MASK（0b11 是最大编码值）
pub const SSTATUS_SUM: usize = 1 << 18; // [18]    S-mode 访问 U 页
pub const SSTATUS_MXR: usize = 1 << 19; // [19]    execute-only 页可读
pub const SSTATUS_UXL_MASK: usize = 0b11 << 32; // [33:32] U-mode XLEN
pub const SSTATUS_UXL_64: usize = 0b10 << 32; // [33:32] U-mode 使用 64-bit XLEN
pub const SSTATUS_SD: usize = 1 << 63; // [63]    FS|VS 脏位汇总

/// 从用户上下文中允许恢复的 `sstatus` 位。
///
/// 其它位均由内核拥有：返回 U-mode 时固定清除 SPP/SIE/SUM/MXR，固定设置
/// SPIE 和 UXL=64，仅允许用户的 FPU/Vector 状态机随上下文恢复。
pub const SSTATUS_USER_RESTORE_MASK: usize = SSTATUS_FS_MASK | SSTATUS_VS_MASK;
pub const SSTATUS_USER_RETURN_BASE: usize = SSTATUS_SPIE | SSTATUS_UXL_64;

// ── Vector CSR 编号 ─────────────────────────────────────────────────────────

pub const CSR_VSTART: usize = 0x008;
pub const CSR_VCSR: usize = 0x00f;
pub const CSR_VL: usize = 0xc20;
pub const CSR_VTYPE: usize = 0xc21;
pub const CSR_VLENB: usize = 0xc22;

// ── sie/sip 位域 ─────────────────────────────────────────────────────────────

pub const SIE_SSIE: usize = 1 << IRQ_S_SOFT; // S-mode 软件中断使能
pub const SIE_STIE: usize = 1 << IRQ_S_TIMER; // S-mode 定时器中断使能
pub const SIE_SEIE: usize = 1 << IRQ_S_EXT; // S-mode 外部中断使能
pub const SIP_SSIP: usize = SIE_SSIE; // sip.SSIP 与 sie.SSIE 使用相同 bit 位置

// ── scause ────────────────────────────────────────────────────────────────────

/// 最高位为 1 表示中断，为 0 表示异常。
pub const SCAUSE_INTERRUPT: usize = 1 << 63;

// 中断号（scause[62:0]，需 OR SCAUSE_INTERRUPT）
pub const IRQ_S_SOFT: usize = 1; // S-mode 软件中断 (SSIP)
pub const IRQ_S_TIMER: usize = 5; // S-mode 定时器中断 (STIP)
pub const IRQ_S_EXT: usize = 9; // S-mode 外部中断 (SEIP, PLIC)

// 异常号（scause[62:0]，最高位为 0）
//
// Code | 名称                    | 触发条件
// ─────┼─────────────────────────┼──────────────────────────────────────
//   0  | 指令地址未对齐          | PC 非自然对齐（C 扩展关闭时）
//   1  | 指令访问错误            | PMP/PMA 拒绝取指
//   2  | 非法指令                | 未知操作码 / 权限不足
//   3  | 断点                    | ebreak 指令触发
//   4  | 加载地址未对齐          | 非原子 load 操作未对齐
//   5  | 加载访问错误            | PMP/PMA 拒绝 load 操作
//   6  | 存储地址未对齐          | 非原子 store 操作未对齐
//   7  | 存储访问错误            | PMP/PMA 拒绝 store 操作
//   8  | 用户态系统调用          | U-mode 下执行 ecall
//   9  | 监管态系统调用          | S-mode 下执行 ecall
//  12  | 指令页故障              | 页表翻译失败（取指阶段）
//  13  | 加载页故障              | 页表翻译失败（load 操作）
//  15  | 存储页故障              | 页表翻译失败（store 操作）

pub const EXC_INST_MISALIGNED: usize = 0;
pub const EXC_INST_ACCESS: usize = 1;
pub const EXC_ILLEGAL_INST: usize = 2;
pub const EXC_BREAKPOINT: usize = 3;
pub const EXC_LOAD_MISALIGNED: usize = 4;
pub const EXC_LOAD_ACCESS: usize = 5;
pub const EXC_STORE_MISALIGNED: usize = 6;
pub const EXC_STORE_ACCESS: usize = 7;
pub const EXC_ECALL_U: usize = 8;
pub const EXC_ECALL_S: usize = 9;
// 10, 11: reserved
pub const EXC_INST_PAGE_FAULT: usize = 12;
pub const EXC_LOAD_PAGE_FAULT: usize = 13;
// 14: reserved
pub const EXC_STORE_PAGE_FAULT: usize = 15;

// ── satp ──────────────────────────────────────────────────────────────────────

/// Sv48 模式：satp.MODE = 9（四级页表，48 位虚拟地址）。
pub const SATP_MODE_SV48: usize = 9 << 60;

// ── stvec ─────────────────────────────────────────────────────────────────────

/// 直接模式：所有 trap 跳转到 BASE 地址（不区分中断号）。
pub const STVEC_MODE_DIRECT: usize = 0;

// ── CSR 访问宏 ────────────────────────────────────────────────────────────────
//
// RISC-V CSR 指令要求寄存器名为编译期立即数，无法用运行时变量。
// 以下宏封装了 unsafe + asm，调用处只需写 CSR 名（与 asm 里一致的字符串）。

/// 读取 CSR 值。
///
/// ```ignore
/// let val = read_csr!(sstatus);  // 示例：读取 sstatus 寄存器
/// ```
#[macro_export]
macro_rules! read_csr {
    ($csr:ident) => {{
        let _val: usize;
        unsafe { core::arch::asm!(concat!("csrr {}, ", stringify!($csr)), out(reg) _val, options(nomem, nostack)) };
        _val
    }};
}

/// 写入 CSR。
///
/// ```ignore
/// write_csr!(satp, new_satp);
/// ```
#[macro_export]
macro_rules! write_csr {
    ($csr:ident, $val:expr) => {{
        let v: usize = $val;
        unsafe { core::arch::asm!(concat!("csrw ", stringify!($csr), ", {}"), in(reg) v, options(nomem, nostack)) };
    }};
}

/// 置位 CSR 比特（csrs）。
///
/// ```ignore
/// set_csr!(sstatus, SSTATUS_SUM);
/// ```
#[macro_export]
macro_rules! set_csr {
    ($csr:ident, $bits:expr) => {{
        let mask: usize = $bits;
        unsafe { core::arch::asm!(concat!("csrs ", stringify!($csr), ", {}"), in(reg) mask, options(nomem, nostack)) };
    }};
}

/// 清除 CSR 比特（csrc）。
///
/// ```ignore
/// clear_csr!(sstatus, SSTATUS_SIE);
/// ```
#[macro_export]
macro_rules! clear_csr {
    ($csr:ident, $bits:expr) => {{
        let mask: usize = $bits;
        unsafe { core::arch::asm!(concat!("csrc ", stringify!($csr), ", {}"), in(reg) mask, options(nomem, nostack)) };
    }};
}
