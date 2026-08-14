//! LS2K1000 GMAC（snps,dwmac-3.70a）寄存器与描述符布局。
//!
//! 位定义对照 Linux stmmac 的 dwmac1000.h / dwmac_dma.h / descs.h，MII
//! 地址寄存器采用 dwmac-loongson.c 的 Loongson 布局（PA[15:11]、
//! RDA[10:6]、CR[5:2]）。2K1000 的 GMAC 是旧式单 DMA 通道核
//! （DMA 寄存器块位于 MAC 基址 + 0x1000，normal 描述符，32 位地址）。

// ───────────────────────── MAC 寄存器（基址 = 节点 reg） ─────────────────────

pub const GMAC_CONTROL: usize = 0x000;
pub const GMAC_FRAME_FILTER: usize = 0x004;
pub const GMAC_MII_ADDR: usize = 0x010;
pub const GMAC_MII_DATA: usize = 0x014;
pub const GMAC_DEBUG: usize = 0x024;
pub const GMAC_INT_MASK: usize = 0x03c;
pub const GMAC_ADDR0_HIGH: usize = 0x300;
pub const GMAC_ADDR0_LOW: usize = 0x304;

pub const GMAC_CONTROL_2K: u32 = 0x0800_0000;
pub const GMAC_CONTROL_JD: u32 = 0x0040_0000;
pub const GMAC_CONTROL_BE: u32 = 0x0020_0000;
pub const GMAC_CONTROL_JE: u32 = 0x0010_0000;
pub const GMAC_CONTROL_PS: u32 = 0x0000_8000;
pub const GMAC_CONTROL_FES: u32 = 0x0000_4000;
pub const GMAC_CONTROL_LM: u32 = 0x0000_1000;
pub const GMAC_CONTROL_DM: u32 = 0x0000_0800;
pub const GMAC_CONTROL_IPC: u32 = 0x0000_0400;
pub const GMAC_CONTROL_TE: u32 = 0x0000_0008;
pub const GMAC_CONTROL_RE: u32 = 0x0000_0004;
/// 内核初始化位（Linux dwmac1000_core_init 的 GMAC_CORE_INIT）。
pub const GMAC_CORE_INIT: u32 = GMAC_CONTROL_JD | GMAC_CONTROL_PS | GMAC_CONTROL_BE;

pub const GMAC_FRAME_FILTER_PR: u32 = 0x0000_0001;
pub const GMAC_FRAME_FILTER_PM: u32 = 0x0000_0010;

// ─────────────── MII 地址寄存器（Loongson 布局） ───────────────
// [0] GBUSY、[1] GWRITE、[5:2] CR、[10:6] RDA、[15:11] PA。

pub const MII_ADDR_GBUSY: u32 = 1 << 0;
pub const MII_ADDR_GWRITE: u32 = 1 << 1;
pub const MII_CR_SHIFT: u32 = 2;
/// 100-150 MHz 输入时钟对应的 CSR 分频（MDC = clk/62）。
pub const MII_CR_100_150M: u32 = 0x1;
pub const MII_RDA_SHIFT: u32 = 6;
pub const MII_PA_SHIFT: u32 = 11;

// ───────────────────────── DMA 寄存器（基址 + 0x1000） ─────────────────────

pub const DMA_BUS_MODE: usize = 0x1000;
pub const DMA_XMT_POLL_DEMAND: usize = 0x1004;
pub const DMA_RCV_POLL_DEMAND: usize = 0x1008;
pub const DMA_RCV_BASE_ADDR: usize = 0x100c;
pub const DMA_TX_BASE_ADDR: usize = 0x1010;
pub const DMA_STATUS: usize = 0x1014;
pub const DMA_CONTROL: usize = 0x1018;
pub const DMA_INTR_ENA: usize = 0x101c;

pub const DMA_BUS_MODE_SFT_RESET: u32 = 0x0000_0001;
pub const DMA_BUS_MODE_PBL_SHIFT: u32 = 8;
pub const DMA_BUS_MODE_RPBL_SHIFT: u32 = 17;
pub const DMA_BUS_MODE_FB: u32 = 0x0001_0000;
pub const DMA_BUS_MODE_USP: u32 = 0x0080_0000;
pub const DMA_BUS_MODE_MAXPBL: u32 = 0x0100_0000;

pub const DMA_STATUS_NIS: u32 = 0x0001_0000;
pub const DMA_STATUS_AIS: u32 = 0x0000_8000;
pub const DMA_STATUS_ERI: u32 = 0x0000_4000;
pub const DMA_STATUS_FBI: u32 = 0x0000_2000;
pub const DMA_STATUS_RPS: u32 = 0x0000_0100;
pub const DMA_STATUS_RU: u32 = 0x0000_0080;
pub const DMA_STATUS_RI: u32 = 0x0000_0040;
pub const DMA_STATUS_UNF: u32 = 0x0000_0020;
pub const DMA_STATUS_OVF: u32 = 0x0000_0010;
pub const DMA_STATUS_TU: u32 = 0x0000_0004;
pub const DMA_STATUS_TPS: u32 = 0x0000_0002;
pub const DMA_STATUS_TI: u32 = 0x0000_0001;
/// 需要写回确认的中断位（RW1C）。
pub const DMA_STATUS_INTR_BITS: u32 = DMA_STATUS_NIS
    | DMA_STATUS_AIS
    | DMA_STATUS_ERI
    | DMA_STATUS_FBI
    | DMA_STATUS_RPS
    | DMA_STATUS_RU
    | DMA_STATUS_RI
    | DMA_STATUS_UNF
    | DMA_STATUS_OVF
    | DMA_STATUS_TU
    | DMA_STATUS_TPS
    | DMA_STATUS_TI;

pub const DMA_CONTROL_TSF: u32 = 0x0020_0000;
pub const DMA_CONTROL_RSF: u32 = 0x0200_0000;
pub const DMA_CONTROL_ST: u32 = 0x0000_2000;
pub const DMA_CONTROL_SR: u32 = 0x0000_0002;

pub const DMA_INTR_ENA_NIE: u32 = 0x0001_0000;
pub const DMA_INTR_ENA_AIE: u32 = 0x0000_8000;
pub const DMA_INTR_ENA_FBE: u32 = 0x0000_2000;
pub const DMA_INTR_ENA_RIE: u32 = 0x0000_0040;
pub const DMA_INTR_ENA_UNE: u32 = 0x0000_0020;
pub const DMA_INTR_ENA_TIE: u32 = 0x0000_0001;
pub const DMA_INTR_NORMAL: u32 = DMA_INTR_ENA_NIE | DMA_INTR_ENA_RIE | DMA_INTR_ENA_TIE;
pub const DMA_INTR_ABNORMAL: u32 = DMA_INTR_ENA_AIE | DMA_INTR_ENA_FBE | DMA_INTR_ENA_UNE;
pub const DMA_INTR_DEFAULT_MASK: u32 = DMA_INTR_NORMAL | DMA_INTR_ABNORMAL;

// ───────────────────────── 描述符位（normal 模式） ─────────────────────────

/// 16 字节 normal 描述符（32 位地址）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DmaDesc {
    pub des0: u32,
    pub des1: u32,
    pub des2: u32,
    pub des3: u32,
}

pub const TDES0_OWN: u32 = 1 << 31;
pub const TDES0_ERROR_SUMMARY: u32 = 1 << 15;
pub const TDES1_BUFFER1_SIZE_MASK: u32 = 0x7ff;
pub const TDES1_END_RING: u32 = 1 << 25;
pub const TDES1_FIRST_SEGMENT: u32 = 1 << 29;
pub const TDES1_LAST_SEGMENT: u32 = 1 << 30;
pub const TDES1_INTERRUPT: u32 = 1 << 31;

pub const RDES0_OWN: u32 = 1 << 31;
pub const RDES0_FRAME_LEN_SHIFT: u32 = 16;
pub const RDES0_FRAME_LEN_MASK: u32 = 0x3fff << 16;
pub const RDES0_ERROR_SUMMARY: u32 = 1 << 15;
pub const RDES0_LAST_DESCRIPTOR: u32 = 1 << 8;
pub const RDES1_BUFFER1_SIZE_MASK: u32 = 0x7ff;
pub const RDES1_END_RING: u32 = 1 << 25;
pub const RDES1_DISABLE_IC: u32 = 1 << 31;

// ───────────────────────── PHY（C22） ─────────────────────────

pub const MII_BMCR: u16 = 0x00;
pub const MII_BMSR: u16 = 0x01;
pub const MII_PHYIDR1: u16 = 0x02;
pub const MII_PHYIDR2: u16 = 0x03;
pub const MII_ADVERTISE: u16 = 0x04;
pub const MII_LPA: u16 = 0x05;
pub const MII_CTRL1000: u16 = 0x09;
pub const MII_STAT1000: u16 = 0x0a;

pub const BMCR_RESET: u16 = 0x8000;
pub const BMCR_ANENABLE: u16 = 0x1000;
pub const BMCR_ANRESTART: u16 = 0x0200;
pub const BMSR_ANEGCOMPLETE: u16 = 0x0020;
pub const BMSR_LINKSTATUS: u16 = 0x0004;
/// ANAR：802.3 selector + 10HD/10FD/100HD/100FD。
pub const ADVERTISE_ALL: u16 = 0x01e1;
pub const CTRL1000_1000FULL: u16 = 0x0200;
/// 1000BTSR bit11 = 1000BASE-T 全双工协商结果。
pub const STAT1000_1000FULL: u16 = 0x0800;
/// LPA：bit8=100FD、bit7=100HD、bit6=10FD、bit5=10HD。
pub const LPA_100FD: u16 = 0x0100;
pub const LPA_100HD: u16 = 0x0080;
pub const LPA_10FD: u16 = 0x0040;
pub const LPA_10HD: u16 = 0x0020;
