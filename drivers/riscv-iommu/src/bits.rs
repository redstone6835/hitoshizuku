//! RISC-V IOMMU 1.0 寄存器与内存结构位定义。

pub const REG_CAP: usize = 0x0000;
pub const REG_FCTL: usize = 0x0008;
pub const REG_DDTP: usize = 0x0010;
pub const REG_CQB: usize = 0x0018;
pub const REG_CQH: usize = 0x0020;
pub const REG_CQT: usize = 0x0024;
pub const REG_FQB: usize = 0x0028;
pub const REG_FQH: usize = 0x0030;
pub const REG_FQT: usize = 0x0034;
pub const REG_CQCSR: usize = 0x0048;
pub const REG_FQCSR: usize = 0x004c;
pub const REG_PQCSR: usize = 0x0050;
pub const REG_IPSR: usize = 0x0054;
pub const REG_IOCOUNTOVF: usize = 0x0058;
pub const REG_IOHPMCYCLES: usize = 0x0060;
pub const REG_IOHPMEVT_BASE: usize = 0x0160;
pub const REG_ICVEC: usize = 0x02f8;
pub const REG_SIZE: usize = 0x1000;

pub const CAP_VERSION_MASK: u64 = 0xff;
pub const CAP_VERSION_MAJOR_SHIFT: u32 = 4;
pub const CAP_SUPPORTED_MAJOR: u8 = 1;
pub const CAP_SV39: u64 = 1 << 9;
pub const CAP_SV48: u64 = 1 << 10;
pub const CAP_SV57: u64 = 1 << 11;
pub const CAP_MSI_FLAT: u64 = 1 << 22;
pub const CAP_ATS: u64 = 1 << 25;
pub const CAP_IGS_SHIFT: u32 = 28;
pub const CAP_IGS_MASK: u64 = 0x3 << CAP_IGS_SHIFT;
pub const CAP_HPM: u64 = 1 << 30;
pub const CAP_PAS_SHIFT: u32 = 32;
pub const CAP_PAS_MASK: u64 = 0x3f << CAP_PAS_SHIFT;

pub const FCTL_BE: u32 = 1 << 0;
pub const FCTL_WSI: u32 = 1 << 1;

pub const DDTP_MODE_MASK: u64 = 0xf;
pub const DDTP_BUSY: u64 = 1 << 4;
pub const DDTP_MODE_OFF: u8 = 0;
pub const DDTP_MODE_BARE: u8 = 1;
pub const DDTP_MODE_1LVL: u8 = 2;
pub const DDTP_MODE_2LVL: u8 = 3;
pub const DDTP_MODE_3LVL: u8 = 4;

pub const QUEUE_ENABLE: u32 = 1 << 0;
pub const QUEUE_INTERRUPT_ENABLE: u32 = 1 << 1;
pub const QUEUE_MEM_FAULT: u32 = 1 << 8;
pub const QUEUE_OVERFLOW: u32 = 1 << 9;
pub const QUEUE_ACTIVE: u32 = 1 << 16;
pub const QUEUE_BUSY: u32 = 1 << 17;
pub const CQCSR_TIMEOUT: u32 = 1 << 9;
pub const CQCSR_ILLEGAL: u32 = 1 << 10;
pub const CQCSR_FENCE_WRITE_PENDING: u32 = 1 << 11;

pub const IPSR_CQ: u32 = 1 << 0;
pub const IPSR_FQ: u32 = 1 << 1;
pub const IPSR_PM: u32 = 1 << 2;
pub const IPSR_PQ: u32 = 1 << 3;
pub const IPSR_ALL: u32 = IPSR_CQ | IPSR_FQ | IPSR_PM | IPSR_PQ;

pub const INTERRUPT_CAUSE_COUNT: usize = 4;
pub const ICVEC_FIELD_MASK: u64 = 0xf;
pub const HPM_OVERFLOW: u64 = 1 << 63;
pub const HPM_EVENT_COUNTERS: usize = 31;

/// 判断 capabilities.version 是否属于本驱动实现的 1.x 规范族。
///
/// version 高/低半字节分别是 major/minor；1.x 的新增 minor 必须保持向后兼容，
/// 未知 major 则可能改变寄存器和内存结构，不能按 1.0 布局继续驱动。
pub const fn capability_version_supported(capabilities: u64) -> bool {
    let version = (capabilities & CAP_VERSION_MASK) as u8;
    version >> CAP_VERSION_MAJOR_SHIFT == CAP_SUPPORTED_MAJOR
}

/// 按规范的 cause 顺序把 CQ/FQ/PM/PQ 均匀映射到可用中断向量。
///
/// CQ 固定使用 vector 0；其它 cause 依次使用 `cause % vector_count`，与 Linux
/// 的退化策略一致，因而一条 WSI 时四种 cause 会自然共享同一 handler。
pub const fn interrupt_vector_layout(vector_count: usize) -> Option<u64> {
    if vector_count == 0 || vector_count > INTERRUPT_CAUSE_COUNT {
        return None;
    }
    let fq = 1 % vector_count;
    let pm = 2 % vector_count;
    let pq = 3 % vector_count;
    Some(((fq as u64) << 4) | ((pm as u64) << 8) | ((pq as u64) << 12))
}

/// 验证硬件 WARL readback 没有把任何 cause 路由到不存在的 vector。
pub const fn interrupt_vector_layout_valid(layout: u64, vector_count: usize) -> bool {
    if vector_count == 0 || vector_count > INTERRUPT_CAUSE_COUNT {
        return false;
    }
    let mut cause = 0usize;
    while cause < INTERRUPT_CAUSE_COUNT {
        let vector = ((layout >> (cause * 4)) & ICVEC_FIELD_MASK) as usize;
        if vector >= vector_count {
            return false;
        }
        cause += 1;
    }
    true
}

pub const DC_TC_VALID: u64 = 1 << 0;
pub const DDTE_VALID: u64 = 1 << 0;

pub const CMD_IOTINVAL_VMA: u64 = 1;
pub const CMD_IOFENCE_C: u64 = 2 | (1 << 12) | (1 << 13);
pub const CMD_IODIR_DDT: u64 = 3;
pub const CMD_IODIR_DV: u64 = 1 << 33;

pub const PTE_VALID: u64 = 1 << 0;
pub const PTE_READ: u64 = 1 << 1;
pub const PTE_WRITE: u64 = 1 << 2;
pub const PTE_EXECUTE: u64 = 1 << 3;
pub const PTE_USER: u64 = 1 << 4;
pub const PTE_ACCESSED: u64 = 1 << 6;
pub const PTE_DIRTY: u64 = 1 << 7;
pub const PTE_PPN_MASK: u64 = ((1u64 << 44) - 1) << 10;

pub const POLL_LIMIT: usize = 1_000_000;
pub const QUEUE_LOG2SZ: u64 = 5;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Command {
    pub dword0: u64,
    pub dword1: u64,
}

impl Command {
    pub const fn iotinval_all() -> Self {
        Self {
            dword0: CMD_IOTINVAL_VMA,
            dword1: 0,
        }
    }

    pub const fn iodir_device(device_id: u32) -> Self {
        Self {
            dword0: CMD_IODIR_DDT | CMD_IODIR_DV | ((device_id as u64) << 40),
            dword1: 0,
        }
    }

    pub const fn iodir_all() -> Self {
        Self {
            dword0: CMD_IODIR_DDT,
            dword1: 0,
        }
    }

    pub const fn iofence() -> Self {
        Self {
            dword0: CMD_IOFENCE_C,
            dword1: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FaultRecord {
    pub header: u64,
    pub reserved: u64,
    pub iotval: u64,
    pub iotval2: u64,
}

pub const fn encode_ppn(paddr: usize) -> u64 {
    ((paddr as u64) >> 12) << 10
}

pub const fn decode_pte_paddr(pte: u64) -> usize {
    (((pte & PTE_PPN_MASK) >> 10) << 12) as usize
}

pub const fn pte_is_leaf(pte: u64) -> bool {
    pte & (PTE_READ | PTE_WRITE | PTE_EXECUTE) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_layout_folds_four_causes_over_available_vectors() {
        assert_eq!(interrupt_vector_layout(1), Some(0x0000));
        assert_eq!(interrupt_vector_layout(2), Some(0x1010));
        assert_eq!(interrupt_vector_layout(3), Some(0x0210));
        assert_eq!(interrupt_vector_layout(4), Some(0x3210));
        assert_eq!(interrupt_vector_layout(0), None);
        assert_eq!(interrupt_vector_layout(5), None);
    }

    #[test]
    fn interrupt_layout_readback_rejects_out_of_range_vectors() {
        assert!(interrupt_vector_layout_valid(0x3210, 4));
        assert!(interrupt_vector_layout_valid(0x0210, 3));
        assert!(!interrupt_vector_layout_valid(0x3210, 3));
        assert!(!interrupt_vector_layout_valid(0x0010, 1));
    }

    #[test]
    fn capability_version_accepts_compatible_minor_only() {
        assert!(capability_version_supported(0x10));
        assert!(capability_version_supported(0x1f));
        assert!(!capability_version_supported(0x00));
        assert!(!capability_version_supported(0x20));
        assert!(!capability_version_supported(0xf0));
    }
}
