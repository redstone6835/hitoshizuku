//! LS2K1000 USB 主机控制器寄存器与数据结构位定义。
//!
//! EHCI 位定义对照 Linux drivers/usb/host/ehci.h（QH/qTD 见 EHCI 1.0
//! 规范 3.5/3.6 与 Linux ehci-q.c 的构建方式）；OHCI 对照
//! drivers/usb/host/ohci.h；dwc2 对照 drivers/usb/dwc2/core.h 的
//! 主机模式寄存器。

// ───────────────────────── EHCI ─────────────────────────

pub const EHCI_HCSPARAMS: usize = 0x04;

/// EHCI 操作寄存器（op base = cap base + CAPLENGTH，通常 0x20）。
pub const EHCI_USBCMD: usize = 0x00;
pub const EHCI_USBSTS: usize = 0x04;
pub const EHCI_USBINTR: usize = 0x08;
pub const EHCI_CTRLDSSEGMENT: usize = 0x10;
pub const EHCI_PERIODICLISTBASE: usize = 0x14;
pub const EHCI_ASYNCLISTADDR: usize = 0x18;
pub const EHCI_CONFIGFLAG: usize = 0x40;
pub const EHCI_PORTSC: usize = 0x44;

pub const EHCI_CMD_RUN: u32 = 1 << 0;
pub const EHCI_CMD_HCRESET: u32 = 1 << 1;
pub const EHCI_CMD_PSE: u32 = 1 << 4;
pub const EHCI_CMD_ASE: u32 = 1 << 5;
pub const EHCI_CMD_IAAD: u32 = 1 << 6;
pub const EHCI_CMD_LRESET: u32 = 1 << 7;

pub const EHCI_STS_USBINT: u32 = 1 << 0;
pub const EHCI_STS_ERRINT: u32 = 1 << 1;
pub const EHCI_STS_PORTCHANGE: u32 = 1 << 2;
pub const EHCI_STS_FLR: u32 = 1 << 3;
pub const EHCI_STS_FATAL: u32 = 1 << 4;
pub const EHCI_STS_IAA: u32 = 1 << 5;
pub const EHCI_STS_HCHALTED: u32 = 1 << 12;
pub const EHCI_STS_ASS: u32 = 1 << 15;
pub const EHCI_STS_W1C_MASK: u32 = EHCI_STS_USBINT
    | EHCI_STS_ERRINT
    | EHCI_STS_PORTCHANGE
    | EHCI_STS_FLR
    | EHCI_STS_FATAL
    | EHCI_STS_IAA;

pub const EHCI_PORTSC_CCS: u32 = 1 << 0;
pub const EHCI_PORTSC_CSC: u32 = 1 << 1;
pub const EHCI_PORTSC_PED: u32 = 1 << 2;
pub const EHCI_PORTSC_PEC: u32 = 1 << 3;
pub const EHCI_PORTSC_OCC: u32 = 1 << 5;
pub const EHCI_PORTSC_PR: u32 = 1 << 8;
pub const EHCI_PORTSC_PP: u32 = 1 << 12;
pub const EHCI_PORTSC_PORT_OWNER: u32 = 1 << 13;
pub const EHCI_PORTSC_WKCNNT_E: u32 = 1 << 20;
/// PORTSC 中写 1 清除的状态变化位。修改其它字段前必须先清掉这些位，
/// 否则“读-改-写”会意外确认尚未处理的端口事件。
pub const EHCI_PORTSC_CHANGE_MASK: u32 = EHCI_PORTSC_CSC | EHCI_PORTSC_PEC | EHCI_PORTSC_OCC;

/// QH 硬件区（48 字节，32 位地址模式）。
///
/// EHCI 要求每个 QH 按 32 字节对齐；`align(32)` 同时把数组槽距补齐到
/// 64 字节，避免连续池中第二个 QH 落在未对齐地址。
#[repr(C, align(32))]
#[derive(Clone, Copy, Default)]
pub struct EhciQhHw {
    pub next: u32,         // dword0: 链接指针（bit0=T，bits2:1=类型）
    pub info1: u32,        // dword1: 端点特性
    pub info2: u32,        // dword2: 端点能力
    pub current: u32,      // dword3: 当前 qTD
    pub qtd_next: u32,     // dword4: 下一个 qTD（overlay）
    pub qtd_alt_next: u32, // dword5: 备选 qTD（overlay）
    pub token: u32,        // dword6: token（overlay）
    pub buf0: u32,         // dword7
    pub buf1: u32,         // dword8
    pub buf2: u32,         // dword9
    pub buf3: u32,         // dword10
    pub buf4: u32,         // dword11
}

pub const QH_HEAD: u32 = 1 << 15;
pub const QH_TOGGLE_CTL: u32 = 1 << 14;
pub const QH_HIGH_SPEED: u32 = 2 << 12;
pub const QH_NEXT_TERMINATE: u32 = 1 << 0;
pub const QH_NEXT_TYPE_QH: u32 = 1 << 1;
pub const QH_NEXT_TYPE_MASK: u32 = 3 << 1;
pub const QH_NEXT_POINTER_MASK: u32 = !0x1f;

/// 编码 QH horizontal link pointer（EHCI 1.0 3.6.1）。
pub const fn qh_next(dma: u32) -> u32 {
    (dma & QH_NEXT_POINTER_MASK) | QH_NEXT_TYPE_QH
}

/// 从 horizontal link pointer 中提取 32 字节对齐的 DMA 地址。
pub const fn qh_next_pointer(link: u32) -> u32 {
    link & QH_NEXT_POINTER_MASK
}

const _: () = assert!(core::mem::size_of::<EhciQhHw>() == 64);
const _: () = assert!(core::mem::align_of::<EhciQhHw>() == 32);

/// qTD（32 字节）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EhciQtdHw {
    pub next: u32,     // dword0
    pub alt_next: u32, // dword1
    pub token: u32,    // dword2
    pub buf0: u32,     // dword3
    pub buf1: u32,     // dword4
    pub buf2: u32,     // dword5
    pub buf3: u32,     // dword6
    pub buf4: u32,     // dword7
}

pub const QTD_TOGGLE: u32 = 1 << 31;
pub const QTD_IOC: u32 = 1 << 15;
pub const QTD_CERR_SHIFT: u32 = 10;
pub const QTD_PID_SHIFT: u32 = 8;
pub const QTD_PID_OUT: u32 = 0;
pub const QTD_PID_IN: u32 = 1;
pub const QTD_PID_SETUP: u32 = 2;
pub const QTD_STS_ACTIVE: u32 = 1 << 7;
pub const QTD_STS_HALT: u32 = 1 << 6;
pub const QTD_LENGTH_SHIFT: u32 = 16;
pub const QTD_TERMINATE: u32 = 1 << 0;

pub const EHCI_TUNE_CERR: u32 = 3;
pub const EHCI_TUNE_RL_HS: u32 = 8;

// ───────────────────────── OHCI ─────────────────────────

pub const OHCI_HC_CONTROL: usize = 0x04;
pub const OHCI_HC_COMMAND_STATUS: usize = 0x08;
pub const OHCI_HC_INTERRUPT_STATUS: usize = 0x0c;
pub const OHCI_HC_HCCA: usize = 0x18;
pub const OHCI_HC_FM_INTERVAL: usize = 0x34;
pub const OHCI_HC_RH_STATUS: usize = 0x50;
pub const OHCI_HC_RH_PORT_STATUS: usize = 0x54;

pub const OHCI_CTRL_CBSR: u32 = 0 << 0;
pub const OHCI_CTRL_HCFS_OPERATIONAL: u32 = 2 << 6;
pub const OHCI_CTRL_RWE: u32 = 1 << 10;
pub const OHCI_CTRL_CLE: u32 = 1 << 4;
pub const OHCI_CTRL_BLE: u32 = 1 << 5;
pub const OHCI_CMDSTAT_HCR: u32 = 1 << 0;
pub const OHCI_CMDSTAT_CLF: u32 = 1 << 1;
pub const OHCI_CMDSTAT_BLF: u32 = 1 << 2;

pub const OHCI_INTR_WDH: u32 = 1 << 0;
pub const OHCI_RHSTATUS_LPSC: u32 = 1 << 0;

pub const OHCI_PORT_CCS: u32 = 1 << 0;
pub const OHCI_PORT_LSDA: u32 = 1 << 9;
pub const OHCI_PORT_PRS: u32 = 1 << 4;
pub const OHCI_PORT_PRSC: u32 = 1 << 20;

/// ED（16 字节）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct OhciEd {
    pub word0: u32,
    pub word1: u32,
    pub tail: u32,
    pub head: u32,
}

pub const ED_FUNC_ADDR_SHIFT: u32 = 0;
pub const ED_EN_SHIFT: u32 = 7;
pub const ED_DIR_SHIFT: u32 = 11;
pub const ED_SPEED_SHIFT: u32 = 13;
pub const ED_MAXPACKET_SHIFT: u32 = 16;
pub const ED_H: u32 = 1 << 30;

/// TD（16 字节）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct OhciTd {
    pub word0: u32,
    pub word1: u32,
    pub cbp: u32,
    pub be: u32,
}

pub const TD_ROUNDING: u32 = 1 << 18;
pub const TD_TOGGLE: u32 = 1 << 24;
pub const TD_DP_SHIFT: u32 = 25;
pub const TD_DP_SETUP: u32 = 0;
pub const TD_DP_IN: u32 = 2;
pub const TD_DP_OUT: u32 = 3;
pub const TD_CC_NOERROR: u32 = 0;

// ───────────────────────── dwc2（主机模式） ─────────────────────────

pub const DWC2_OTGCTL: usize = 0x00;
pub const DWC2_GAHBCFG: usize = 0x08;
pub const DWC2_GUSBCFG: usize = 0x0c;
pub const DWC2_GRSTCTL: usize = 0x10;
pub const DWC2_GINTMSK: usize = 0x18;
pub const DWC2_GRXFSIZ: usize = 0x24;
pub const DWC2_GNPTXFSIZ: usize = 0x28;
pub const DWC2_HPTXFSIZ: usize = 0x100;
pub const DWC2_HPRT: usize = 0x440;
pub const DWC2_HCCHAR0: usize = 0x500;
pub const DWC2_HC_CHAN_STRIDE: usize = 0x20;

pub const DWC2_OTGCTL_HSTEN: u32 = 1 << 1;

pub const DWC2_GAHBCFG_GLBL_INTR_EN: u32 = 1 << 0;
pub const DWC2_GAHBCFG_DMA_EN: u32 = 1 << 5;

pub const DWC2_GUSBCFG_FORCEHSTMODE: u32 = 1 << 29;
pub const DWC2_GUSBCFG_HNPCAP: u32 = 1 << 9;
pub const DWC2_GUSBCFG_SRPCAP: u32 = 1 << 8;
pub const DWC2_GUSBCFG_ULPI_UTMI_SEL: u32 = 1 << 4;

pub const DWC2_GRSTCTL_CSFTRST: u32 = 1 << 0;

pub const DWC2_HPRT_PRTENA: u32 = 1 << 1;
pub const DWC2_HPRT_PRTCONNDET: u32 = 1 << 3;
pub const DWC2_HPRT_PRTENCHNG: u32 = 1 << 4;
pub const DWC2_HPRT_PRTOVRCURRCHNG: u32 = 1 << 6;
pub const DWC2_HPRT_PRTRST: u32 = 1 << 8;
pub const DWC2_HPRT_PRTPWR: u32 = 1 << 12;
pub const DWC2_HPRT_PRTCONNSTS: u32 = 1 << 16;
pub const DWC2_HPRT_PRTSPD_MASK: u32 = 3 << 17;

pub const DWC2_HCCHAR_EPDIR: u32 = 1 << 11;
pub const DWC2_HCCHAR_EPTYPE_CONTROL: u32 = 0 << 18;
pub const DWC2_HCCHAR_EPTYPE_BULK: u32 = 2 << 18;
pub const DWC2_HCCHAR_EPTYPE_INTERRUPT: u32 = 3 << 18;
pub const DWC2_HCCHAR_MPS_MASK: u32 = 0x7ff << 0;
pub const DWC2_HCCHAR_EPNUM_SHIFT: u32 = 11;
pub const DWC2_HCCHAR_DEVADDR_SHIFT: u32 = 22;
pub const DWC2_HCCHAR_CHENA: u32 = 1 << 31;

pub const DWC2_HCINT_XFERCOMP: u32 = 1 << 0;
pub const DWC2_HCINT_CHHLTD: u32 = 1 << 1;
pub const DWC2_HCINT_STALL: u32 = 1 << 2;
pub const DWC2_HCINT_XACTERR: u32 = 1 << 6;
pub const DWC2_HCINT_BBLERR: u32 = 1 << 7;
pub const DWC2_HCINT_ALL: u32 = 0x3ff;

pub const DWC2_HCTSIZ_XFERSIZE_SHIFT: u32 = 0;
pub const DWC2_HCTSIZ_PKTCNT_SHIFT: u32 = 19;
pub const DWC2_HCTSIZ_PID_DATA1: u32 = 1 << 29;
pub const DWC2_HCTSIZ_PID_SETUP: u32 = 2 << 29;

/// USB 标准描述符常量。
pub const USB_DT_DEVICE: u8 = 1;
pub const USB_DT_CONFIG: u8 = 2;
pub const USB_DT_INTERFACE: u8 = 4;
pub const USB_DT_ENDPOINT: u8 = 5;

pub const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
pub const USB_REQ_SET_ADDRESS: u8 = 0x05;
pub const USB_REQ_SET_CONFIGURATION: u8 = 0x09;
pub const USB_DIR_IN: u8 = 0x80;
pub const USB_DIR_OUT: u8 = 0x00;
pub const USB_TYPE_STANDARD: u8 = 0x00;
pub const USB_RECIP_DEVICE: u8 = 0x00;

pub const USB_SPEED_HIGH: u8 = 0;
pub const USB_SPEED_FULL: u8 = 1;
pub const USB_SPEED_LOW: u8 = 2;

/// 8 字节 USB 控制请求（setup packet）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UsbSetup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

/// 接口描述符（9 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UsbInterfaceDesc {
    pub length: u8,
    pub descriptor_type: u8,
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub num_endpoints: u8,
    pub interface_class: u8,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    pub interface_string: u8,
}
