//! StarFive JH7110 MMC (DesignWare Mobile Storage Host) 块设备驱动。
//!
//! 实现范围（与 Linux dw_mmc-starfive / dw_mmc 对齐的最小可用子集）：
//! - 控制器复位、FIFO 阈值、时钟分频与供电使能；
//! - SD / eMMC 卡初始化：CMD0/8/55/41（SD）或 CMD1（eMMC）、CMD2/3/9/7；
//! - IDMAC（内部 DMA）单块读写（CMD17/24），1-bit 总线（实机验证 PIO 数据
//!   阶段在 JH7110 上不可用，Linux/U-Boot 均只用 IDMAC）；
//! - 注册为 BlockDevice 并以 mmc0 暴露给 VFS；
//! - ciu 时钟速率经 dt_provider 从 JH7110 CRG 查询（biu/ciu 引用）。
//!
//! 寄存器定义来源：Linux drivers/mmc/host/dw_mmc.h；VERID 决定 FIFO 数据口
//! 偏移（< 2.40a 用 0x100，否则 0x200）。

use alloc::sync::Arc;
use allocator::{KERNEL_ALLOCATOR, MemoryPlacement, PhysicalAllocRequest};
use core::any::Any;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::dev::bio::{Bio, BioIoError, BioOp, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures,
    BlockGeometry, BlockLimits,
};
use crate::dev::dma::DmaDirection;
use crate::dev::dt_provider::{
    self, DtbProviderError, DtbResourceLease, DtbResourceReply, DtbResourceRequest,
};
use crate::dev::function::BlockFunction;
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

// ── dw_mmc 寄存器 ──
const REG_CTRL: usize = 0x000;
const REG_PWREN: usize = 0x004;
const REG_CLKDIV: usize = 0x008;
const REG_CLKENA: usize = 0x010;
const REG_TMOUT: usize = 0x014;
const REG_CTYPE: usize = 0x018;
const REG_BLKSIZ: usize = 0x01c;
const REG_BYTCNT: usize = 0x020;
const REG_CMDARG: usize = 0x028;
const REG_CMD: usize = 0x02c;
const REG_RESP0: usize = 0x030;
const REG_RINTSTS: usize = 0x044;
const REG_STATUS: usize = 0x048;
const REG_FIFOTH: usize = 0x04c;
const REG_VERID: usize = 0x06c;
const DATA_OFFSET: usize = 0x100;
const DATA_240A_OFFSET: usize = 0x200;
const VERID_240A: u32 = 0x240a;

// CTRL 位
const CTRL_RESET: u32 = 1 << 0;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_ALL_RESET: u32 = CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET;

// CMD 位
const CMD_START: u32 = 1 << 31;
const CMD_UPD_CLK: u32 = 1 << 21;
// JH7110 的 DW MSHC 要求所有命令携带 USE_HOLD_REG（U-Boot/Linux 均无条件
// 设置）；缺失会导致数据阶段异常（PIO 无 RXDR / IDMAC 总线错误）。
const CMD_USE_HOLD_REG: u32 = 1 << 29;
// 等待前一数据阶段完成（U-Boot 对除 STOP 外的所有命令设置）。
const CMD_PRV_DAT_WAIT: u32 = 1 << 13;
const CMD_INIT: u32 = 1 << 15;
const CMD_DAT_WR: u32 = 1 << 10;
const CMD_DAT_EXP: u32 = 1 << 9;
const CMD_RESP_CRC: u32 = 1 << 8;
const CMD_RESP_LONG: u32 = 1 << 7;
const CMD_RESP_EXP: u32 = 1 << 6;

// RINTSTS 位
const INT_RTO: u32 = 1 << 8;
const INT_DRTO: u32 = 1 << 9;
const INT_DCRC: u32 = 1 << 7;
const INT_RCRC: u32 = 1 << 6;
const INT_TXDR: u32 = 1 << 4;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_RESP_ERR: u32 = 1 << 1;
const INT_ERR_MASK: u32 = INT_RTO | INT_DRTO | INT_DCRC | INT_RCRC | INT_RESP_ERR;

const STATUS_BUSY: u32 = 1 << 9;

// CTYPE 位

const BLOCK_SIZE: u32 = 512;
const INIT_CLOCK_HZ: u32 = 400_000;

// IDMAC（内部 DMA）寄存器与描述符位。
const REG_BMOD: usize = 0x80;
const REG_PLDMND: usize = 0x84;
const REG_DBADDR: usize = 0x88;
const REG_IDSTS: usize = 0x8c;
const REG_IDINTEN: usize = 0x90;

/// JH7110 变体的 USE_IDMAC 位于 CTRL bit 29（Debian dw_mci_idmac_start_dma
/// 反汇编实证：CTRL |= 0x20000000；标准 dw_mmc 的 BIT(25) 在本 SoC 无效）。
const CTRL_USE_IDMAC_JH7110: u32 = 1 << 29;

const CTRL_DMA_EN: u32 = 1 << 5;
const CTRL_IDMAC_EN: u32 = 1 << 25;
const BMOD_IDMAC_FB: u32 = 1 << 1;
const BMOD_IDMAC_EN: u32 = 1 << 7;

// IDMAC 描述符控制位（DES0）。
const IDMAC_OWN: u32 = 1 << 31;
const IDMAC_FS: u32 = 1 << 3;
const IDMAC_LD: u32 = 1 << 2;
// IDSTS 位布局（与 U-Boot dwmmc.h 及 Debian 清除掩码 0x337 一致）：
// TI=0, RI=1, FBE=2, DU=3, CES=4, NI=8, AI=9；bit 13/15 本 SoC 未定义，
// 实测常驻置位（0xa000），不得当作错误。
const IDSTS_RI: u32 = 1 << 1;
const IDSTS_FBE: u32 = 1 << 2;
const IDSTS_DU: u32 = 1 << 3;
const IDSTS_CES: u32 = 1 << 4;
// 真正的错误只有 FBE/DU/CES；NI/AI 是汇总位（完成时正常置位），不得判错。
const IDSTS_ERR_MASK: u32 = IDSTS_FBE | IDSTS_DU | IDSTS_CES;

/// IDMAC 描述符与 bounce 缓冲共用的单页布局：
/// [0..16) = 描述符，[512..1024) = 数据 bounce。
const DMA_PAGE_LAYOUT_DESC: usize = 0;
const DMA_PAGE_LAYOUT_DATA: usize = BLOCK_SIZE as usize;

/// IDMAC 传输页候选物理地址：JH7110 的 SD 控制器 AXI 窗口不覆盖高地址
/// （U-Boot 的 DMA 缓冲在 0x4xxxxxxx 工作正常；0x80d6e000 实测 Fatal Bus
/// Error），因此从 1GB 附近的低 RAM 精确取一页。
const IDMAC_PHYS_CANDIDATES: [usize; 5] = [
    0x4010_0000,
    0x4020_0000,
    0x4030_0000,
    0x4040_0000,
    0x4050_0000,
];

static IDMAC_BUSY: AtomicBool = AtomicBool::new(false);

/// 从低物理地址区精确分配 IDMAC 传输页（JH7110 SD 控制器 AXI 窗口限制）。
/// 成功后该页归驱动永久持有（不释放）。
fn alloc_idmac_page() -> Result<(usize, usize), MmcError> {
    let phys_to_virt = KERNEL_ALLOCATOR
        .load_phys_to_virt()
        .ok_or(MmcError::IoFailed)?;
    for candidate in IDMAC_PHYS_CANDIDATES {
        let request = PhysicalAllocRequest::new(4096, 4096)
            .with_placement(MemoryPlacement::ExactPhys(candidate));
        if let Ok(allocation) = KERNEL_ALLOCATOR.allocate_physical(request) {
            let paddr = allocation.paddr;
            // 物理分配由 allocator registry 持有，未显式 free 即永久保留。
            let vaddr = phys_to_virt(paddr);
            log::printk!(
                "[jh7110-mmc] idmac page phys={:#x} virt={:#x}",
                paddr,
                vaddr
            );
            return Ok((paddr, vaddr));
        }
    }
    Err(MmcError::IoFailed)
}

/// 静态 IDMAC 页的互斥保护（单缓冲，并发传输需串行化）。
struct IdmacGuard;

impl IdmacGuard {
    fn acquire() -> Self {
        while IDMAC_BUSY.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Self
    }
}

impl Drop for IdmacGuard {
    fn drop(&mut self) {
        IDMAC_BUSY.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
enum MmcError {
    Timeout,
    ResponseError,
    DataError,
    NoCard,
    IoFailed,
}

/// 读取 CLINT mtime（rdtime）。VF2/QEMU 均为 4-10 MHz 量级，
/// 以 4 MHz 保守换算：1 ms = 4000 tick，确保超时只长不短。
fn rdtime() -> u64 {
    let value: u64;
    // Safety: S-mode 下 rdtime 由固件使能（OpenSBI 设置 mcounteren.TM）。
    unsafe { core::arch::asm!("rdtime {}", out(reg) value, options(nomem, nostack)) };
    value
}

fn ticks_per_ms() -> u64 {
    4_000
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = rdtime().saturating_add(timeout_ms.saturating_mul(ticks_per_ms()));
    loop {
        if condition() {
            return true;
        }
        if rdtime() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// 启动早期无调度器可用时的忙等延时（rdtime 为基，4MHz 保守换算）。
fn busy_delay_ms(ms: u64) {
    let deadline = rdtime().saturating_add(ms.saturating_mul(ticks_per_ms()));
    while rdtime() < deadline {
        core::hint::spin_loop();
    }
}

struct JhMmcHost {
    base: usize,
    fifo_data: usize,
    ciu_hz: u32,
    /// IDMAC 传输页：(物理地址, 虚拟地址)。
    idmac_page: (usize, usize),
    dma_coherent: bool,
}

impl JhMmcHost {
    fn read32(&self, reg: usize) -> u32 {
        // Safety: probe 已按 DT reg 窗口校验 MMIO 范围，寄存器访问在窗口内。
        unsafe { core::ptr::read_volatile(self.base.wrapping_add(reg) as *const u32) }
    }

    fn write32(&self, reg: usize, value: u32) {
        // Safety: 同 read32。
        unsafe { core::ptr::write_volatile(self.base.wrapping_add(reg) as *mut u32, value) }
    }

    fn reset(&self) {
        self.write32(REG_CTRL, 0);
        self.write32(REG_CTRL, CTRL_ALL_RESET);
        let _ = wait_until(|| self.read32(REG_CTRL) & CTRL_ALL_RESET == 0, 500);
        self.write32(REG_RINTSTS, 0xffff_ffff);
        self.write32(REG_TMOUT, 0xffff_ffff);
        self.write32(REG_PWREN, 1);
    }

    /// 发送原始命令（写入 CMDARG/CMD 并等待控制器接受）。
    fn send_raw(&self, cmd: u32, arg: u32) -> bool {
        // 等待上一命令被接受（START 位清除）且数据线空闲。
        let accepted = wait_until(|| self.read32(REG_CMD) & CMD_START == 0, 2000);
        if !accepted {
            return false;
        }
        // 数据阶段忙则等待（与 U-Boot dwmci_send_cmd 的 STATUS BUSY 等待一致）。
        if !wait_until(|| self.read32(REG_STATUS) & STATUS_BUSY == 0, 100) {
            return false;
        }
        self.write32(REG_RINTSTS, 0xffff_ffff);
        self.write32(REG_CMDARG, arg);
        self.write32(
            REG_CMD,
            CMD_START | cmd | CMD_USE_HOLD_REG | CMD_PRV_DAT_WAIT,
        );
        wait_until(|| self.read32(REG_CMD) & CMD_START == 0, 2000)
    }

    /// 发送命令并等待 CMD_DONE 或错误。
    fn send_cmd(&self, index: u32, arg: u32, flags: u32) -> Result<[u32; 4], MmcError> {
        if !self.send_raw(flags | index, arg) {
            return Err(MmcError::Timeout);
        }
        // 卡忙时响应可能超过 100ms（TMOUT 由控制器兜底），放宽到 500ms。
        let done = wait_until(
            || {
                let status = self.read32(REG_RINTSTS);
                status & (INT_CMD_DONE | INT_ERR_MASK) != 0
            },
            500,
        );
        if !done {
            return Err(MmcError::Timeout);
        }
        let status = self.read32(REG_RINTSTS);
        if status & INT_ERR_MASK != 0 {
            return Err(MmcError::ResponseError);
        }
        let mut response = [0u32; 4];
        for (index, slot) in response.iter_mut().enumerate() {
            *slot = self.read32(REG_RESP0 + index * 4);
        }
        Ok(response)
    }

    /// 更新卡时钟（divisor 为 CLKDIV 寄存器值）。
    fn update_clock(&self, divider: u32) -> bool {
        self.write32(REG_CLKENA, 0);
        if !self.send_raw(CMD_UPD_CLK, 0) {
            return false;
        }
        self.write32(REG_CLKDIV, divider);
        if !self.send_raw(CMD_UPD_CLK, 0) {
            return false;
        }
        self.write32(REG_CLKENA, 1);
        self.send_raw(CMD_UPD_CLK, 0)
    }

    /// 设置卡时钟（Hz）。
    fn set_clock(&self, hz: u32) -> Result<(), MmcError> {
        let mut divider = self.ciu_hz / hz;
        if self.ciu_hz % hz != 0 && self.ciu_hz > hz {
            divider += 1;
        }
        divider = if self.ciu_hz != hz {
            divider.div_ceil(2)
        } else {
            0
        };
        if self.update_clock(divider) {
            Ok(())
        } else {
            Err(MmcError::Timeout)
        }
    }
}

struct CardInfo {
    /// 总块数（512 字节块）。
    block_count: u64,
    /// 是否块寻址（SDHC/XC 与 eMMC 为真，SDSC 为假）。
    block_addressed: bool,
}

impl JhMmcHost {
    fn card_init(&self) -> Result<CardInfo, MmcError> {
        // CMD0：进入 idle（重试 3 次）。
        let mut ok_cmd0 = false;
        for _ in 0..3 {
            if self.send_raw(CMD_INIT | 0, 0) {
                ok_cmd0 = true;
                break;
            }
            busy_delay_ms(2);
        }
        if !ok_cmd0 {
            return Err(MmcError::Timeout);
        }
        // CMD0 后卡需要恢复时间（SD 规范建议 1ms+）。
        busy_delay_ms(2);
        // 区分 SD 与 eMMC：CMD8 有响应为 SD v2+（重试 3 次）。
        let mut sd_v2 = false;
        for _ in 0..3 {
            match self.send_cmd(8, 0x0000_01aa, CMD_RESP_EXP | CMD_RESP_CRC) {
                Ok(_) => {
                    sd_v2 = true;
                    break;
                }
                Err(_) => busy_delay_ms(2),
            }
        }
        if sd_v2 {
            // SD：CMD55 + ACMD41 循环等待上电完成。每次尝试间隔 5ms，
            // 避免在卡上电期间连续快速重试导致总线状态恶化。
            let mut ocr = 0u32;
            let mut ok = false;
            for attempt in 0..100 {
                let _ = self.send_cmd(55, 0, CMD_RESP_EXP | CMD_RESP_CRC);
                if let Ok(response) = self.send_cmd(41, 0x40ff_8000, CMD_RESP_EXP) {
                    ocr = response[0];
                    if ocr & (1 << 31) != 0 {
                        ok = true;
                        break;
                    }
                } else if attempt % 10 == 9 {
                    log::info!("[jh7110-mmc] ACMD41 attempt {} no response", attempt + 1);
                }
                busy_delay_ms(5);
            }
            if !ok {
                log::warning!(
                    "[jh7110-mmc] ACMD41 no power-up: ocr={:#x} rintsts={:#x} clkdiv={:#x}",
                    ocr,
                    self.read32(REG_RINTSTS),
                    self.read32(REG_CLKDIV)
                );
                return Err(MmcError::NoCard);
            }
        } else {
            // eMMC：先回到 idle，再 CMD1 循环。
            if !self.send_raw(CMD_INIT | 0, 0) {
                return Err(MmcError::Timeout);
            }
            busy_delay_ms(2);
            let mut ocr = 0u32;
            let mut ok = false;
            for _ in 0..100 {
                if let Ok(response) = self.send_cmd(1, 0x40ff_8000, CMD_RESP_EXP) {
                    ocr = response[0];
                    if ocr & (1 << 31) != 0 {
                        ok = true;
                        break;
                    }
                }
                busy_delay_ms(5);
            }
            if !ok {
                log::warning!(
                    "[jh7110-mmc] CMD1 no response: ocr={:#x} rintsts={:#x}",
                    ocr,
                    self.read32(REG_RINTSTS)
                );
                return Err(MmcError::NoCard);
            }
        }

        // CMD2 取 CID。
        let _cid = self.send_cmd(2, 0, CMD_RESP_EXP | CMD_RESP_LONG | CMD_RESP_CRC)?;
        // CMD3 取 RCA。
        let rca_response = self.send_cmd(3, 0, CMD_RESP_EXP | CMD_RESP_CRC)?;
        let rca = rca_response[0] >> 16;
        // CMD9 取 CSD。
        let csd = self.send_cmd(9, rca << 16, CMD_RESP_EXP | CMD_RESP_LONG | CMD_RESP_CRC)?;
        // CMD7 选中卡。
        let _ = self.send_cmd(7, rca << 16, CMD_RESP_EXP | CMD_RESP_CRC)?;

        let csd_value = (u128::from(csd[3]) << 96)
            | (u128::from(csd[2]) << 64)
            | (u128::from(csd[1]) << 32)
            | u128::from(csd[0]);
        let (block_count, block_addressed) = parse_csd(csd_value);
        if block_count == 0 {
            return Err(MmcError::IoFailed);
        }

        // 保守保持 1-bit 总线：ACMD6 4-bit 切换在实机上导致数据阶段超时
        // （CTYPE_4BIT 已写但卡侧切换未确认，RXDR 永不置位）。1-bit 25MHz
        // 足够启动期块读取（分区表 + ext4 元数据）。
        self.write32(REG_CTYPE, 0);

        // 切到默认速度满时钟（不超过 25 MHz 保守值）。
        let full = self.ciu_hz.min(25_000_000);
        self.set_clock(full)?;
        busy_delay_ms(2);

        Ok(CardInfo {
            block_count,
            block_addressed,
        })
    }

    /// IDMAC 描述符 + bounce 共用的静态页（内核 .bss，物理地址 < 4GB，满足
    /// IDMAC 32 位 DBADDR/DES 寻址）。[0..16) = 描述符，[512..1024) = 数据区。
    fn idmac_prepare(&self, direction: DmaDirection) -> Result<(usize, usize), MmcError> {
        let (paddr, vaddr) = self.idmac_page;
        if paddr == 0 || vaddr == 0 {
            return Err(MmcError::IoFailed);
        }
        let dma_base = paddr
            .checked_add(DMA_PAGE_LAYOUT_DESC)
            .ok_or(MmcError::IoFailed)?;
        // Safety: 描述符位于页对齐静态缓冲区，8 字节对齐。
        // 布局（DW MSHC IDMAC，与 Debian dw_mci_idmac_start_dma / U-Boot
        // dwmci_set_idma_desc 一致）：
        //   des0 = 控制位（OWN|CH|FS|LD），des1 = 传输字节数，
        //   des2 = 缓冲区物理地址，des3 = 下一描述符地址（LD 置位后不用）。
        unsafe {
            let desc = vaddr as *mut u32;
            // 与 Debian dw_mci_idmac_start_dma 逐项一致：
            // des0 = OWN|CH|DIC（0x80000012），des1 = 字节数，des2 = 缓冲地址，
            // des3 = 下一描述符；其后的哨兵描述符 des0 = ER(0x20) 标记 ring 结束。
            // 单描述符传输：des0 = OWN|FD|LD（0x8000000C，与 Debian 的
            // 首描述符 OR FD、末描述符清 CH|DIC 再 OR LD 的最终值一致）。
            desc.add(0).write_volatile(IDMAC_OWN | IDMAC_FS | IDMAC_LD);
            desc.add(1).write_volatile(BLOCK_SIZE);
            desc.add(2)
                .write_volatile((dma_base + DMA_PAGE_LAYOUT_DATA) as u32);
            desc.add(3).write_volatile((dma_base + 16) as u32);
            desc.add(4).write_volatile(0x20); // ER 哨兵
            desc.add(5).write_volatile(0);
            desc.add(6).write_volatile(0);
            desc.add(7).write_volatile(0);
        }
        if !self.dma_coherent && !hal::memory::dma_clean_range(vaddr, 64) {
            log::warning!("[jh7110-mmc] Zicbom unavailable; refusing non-coherent IDMAC");
            return Err(MmcError::IoFailed);
        }
        let data_vaddr = vaddr + DMA_PAGE_LAYOUT_DATA;
        if !self.dma_coherent && matches!(direction, DmaDirection::FromDevice) {
            // 设备将覆盖数据区：先 invalidate，避免脏行被后续写回污染。
            if !hal::memory::dma_invalidate_range(data_vaddr, BLOCK_SIZE as usize) {
                return Err(MmcError::IoFailed);
            }
        }
        Ok((paddr, data_vaddr))
    }

    /// 传输前配置控制器 DMA 引擎（与 Debian dw_mci_idmac_start_dma 逐步对齐：
    /// DMA_RESET → IDSTS/IDINTEN/DBADDR → CTRL |= USE_IDMAC(bit29) →
    /// BMOD SWR → BMOD DE|FB → PLDMND=1）。
    fn idmac_start(&self, dma_base: usize) {
        self.write32(REG_CTRL, self.read32(REG_CTRL) | (1 << 2));
        let _ = wait_until(|| self.read32(REG_CTRL) & (1 << 2) == 0, 500);
        self.write32(REG_IDSTS, 0xffff_ffff);
        self.write32(REG_IDINTEN, 0x103);
        self.write32(REG_DBADDR, dma_base as u32);
        self.write32(
            REG_CTRL,
            self.read32(REG_CTRL) | CTRL_USE_IDMAC_JH7110 | CTRL_DMA_EN | CTRL_IDMAC_EN,
        );
        self.write32(REG_BMOD, self.read32(REG_BMOD) | 0x1);
        self.write32(REG_BMOD, BMOD_IDMAC_FB | BMOD_IDMAC_EN);
        self.write32(REG_PLDMND, 1);
        self.write32(REG_BLKSIZ, BLOCK_SIZE);
        self.write32(REG_BYTCNT, BLOCK_SIZE);
    }

    /// 等待 IDMAC 完成位（RI=读 / TI=写）。
    fn idmac_wait(&self, done_bit: u32) -> Result<(), MmcError> {
        let ok = wait_until(
            || {
                let status = self.read32(REG_IDSTS);
                status & (done_bit | IDSTS_ERR_MASK) != 0
            },
            5000,
        );
        let status = self.read32(REG_IDSTS);
        self.write32(REG_IDSTS, done_bit | IDSTS_ERR_MASK);
        if !ok || status & IDSTS_ERR_MASK != 0 {
            log::warning!(
                "[jh7110-mmc] idmac done={} idsts={:#x} err={:#x} rintsts={:#x} ctrl={:#x} bmod={:#x}",
                ok,
                status,
                status & IDSTS_ERR_MASK,
                self.read32(REG_RINTSTS),
                self.read32(REG_CTRL),
                self.read32(REG_BMOD)
            );
            return Err(MmcError::DataError);
        }
        Ok(())
    }

    fn read_block(&self, card: &CardInfo, block: u64, buf: &mut [u8]) -> Result<(), MmcError> {
        if buf.len() != BLOCK_SIZE as usize {
            return Err(MmcError::IoFailed);
        }
        let address = if card.block_addressed {
            u32::try_from(block).map_err(|_| MmcError::IoFailed)?
        } else {
            u32::try_from(block * u64::from(BLOCK_SIZE)).map_err(|_| MmcError::IoFailed)?
        };
        let _guard = IdmacGuard::acquire();
        let (paddr, data_vaddr) = self.idmac_prepare(DmaDirection::FromDevice)?;
        self.idmac_start(paddr);
        // CMD17 响应超时重试 3 次（卡忙/总线瞬时状态常见于刚初始化后）。
        let mut last_error = MmcError::Timeout;
        let mut sent = false;
        for _ in 0..3 {
            match self.send_cmd(17, address, CMD_RESP_EXP | CMD_RESP_CRC | CMD_DAT_EXP) {
                Ok(_) => {
                    sent = true;
                    break;
                }
                Err(error) => {
                    last_error = error;
                    self.write32(REG_RINTSTS, 0xffff_ffff);
                    busy_delay_ms(5);
                }
            }
        }
        if !sent {
            log::warning!(
                "[jh7110-mmc] read_block({}) CMD17 failed: {:?} rintsts={:#x}",
                block,
                last_error,
                self.read32(REG_RINTSTS)
            );
            return Err(MmcError::ResponseError);
        }
        self.idmac_wait(IDSTS_RI)?;
        if !self.dma_coherent && !hal::memory::dma_invalidate_range(data_vaddr, BLOCK_SIZE as usize)
        {
            return Err(MmcError::IoFailed);
        }
        // Safety: data_vaddr 指向静态 IDMAC 数据区（512B），设备已写完。
        buf.copy_from_slice(unsafe {
            core::slice::from_raw_parts(data_vaddr as *const u8, BLOCK_SIZE as usize)
        });
        Ok(())
    }

    fn write_block(&self, card: &CardInfo, block: u64, buf: &[u8]) -> Result<(), MmcError> {
        if buf.len() != BLOCK_SIZE as usize {
            return Err(MmcError::IoFailed);
        }
        let address = if card.block_addressed {
            u32::try_from(block).map_err(|_| MmcError::IoFailed)?
        } else {
            u32::try_from(block * u64::from(BLOCK_SIZE)).map_err(|_| MmcError::IoFailed)?
        };
        // 写路径用 PIO：IDMAC 写方向在本 SoC 上不喂数（TXDR 常置 + HTO 超时，
        // 实测）。控制器主动置 TXDR 时 CPU 直接写 FIFO 数据口即可。
        let _guard = IdmacGuard::acquire();
        // 关掉 IDMAC 引擎，避免上一读的残留描述符在 TXDR 时双喂。
        self.write32(
            REG_CTRL,
            self.read32(REG_CTRL) & !(CTRL_USE_IDMAC_JH7110 | CTRL_DMA_EN | CTRL_IDMAC_EN),
        );
        self.write32(REG_BMOD, self.read32(REG_BMOD) & !BMOD_IDMAC_EN);
        self.write32(REG_BLKSIZ, BLOCK_SIZE);
        self.write32(REG_BYTCNT, BLOCK_SIZE);
        if let Err(error) = self.send_cmd(
            24,
            address,
            CMD_RESP_EXP | CMD_RESP_CRC | CMD_DAT_EXP | CMD_DAT_WR,
        ) {
            log::warning!(
                "[jh7110-mmc] write_block({}) CMD24 failed: {:?} rintsts={:#x}",
                block,
                error,
                self.read32(REG_RINTSTS)
            );
            return Err(MmcError::ResponseError);
        }
        // PIO 数据阶段：TXDR 置位时逐字写入 FIFO 数据口（绝对地址，不能再加 base）。
        let mut words = buf.len() / 4;
        let mut offset = 0usize;
        let deadline = rdtime().saturating_add(5000u64.saturating_mul(ticks_per_ms()));
        while words != 0 {
            let status = self.read32(REG_RINTSTS);
            if status & (INT_DRTO | INT_DCRC) != 0 {
                log::warning!(
                    "[jh7110-mmc] write_block({}) PIO data error rintsts={:#x}",
                    block,
                    status
                );
                return Err(MmcError::DataError);
            }
            if status & INT_TXDR != 0 {
                let value = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());
                // Safety: fifo_data 是 probe 解析的绝对 MMIO 数据口地址。
                unsafe { core::ptr::write_volatile(self.fifo_data as *mut u32, value) };
                self.write32(REG_RINTSTS, INT_TXDR);
                offset += 4;
                words -= 1;
            }
            if words != 0 && rdtime() >= deadline {
                log::warning!(
                    "[jh7110-mmc] write_block({}) PIO TXDR timeout rintsts={:#x} words_left={}",
                    block,
                    self.read32(REG_RINTSTS),
                    words
                );
                return Err(MmcError::Timeout);
            }
        }
        if !wait_until(|| self.read32(REG_RINTSTS) & INT_DATA_OVER != 0, 2000) {
            log::warning!(
                "[jh7110-mmc] write_block({}) DTO timeout rintsts={:#x}",
                block,
                self.read32(REG_RINTSTS)
            );
            return Err(MmcError::Timeout);
        }
        Ok(())
    }
}
/// 解析 CSD 得到总容量（512 字节块数）与寻址方式。
fn parse_csd(csd: u128) -> (u64, bool) {
    let structure = ((csd >> 126) & 0x3) as u8;
    if structure >= 1 {
        // CSD v2.0：capacity = (c_size + 1) * 512 KiB。
        let c_size = ((csd >> 48) & 0x3f_ffff) as u64;
        let bytes = (c_size + 1) * 512 * 1024;
        (bytes / u64::from(BLOCK_SIZE), true)
    } else {
        // CSD v1.0：SDSC，字节寻址。
        let read_bl_len = ((csd >> 80) & 0xf) as u32;
        let c_size = ((csd >> 62) & 0xfff) as u64;
        let c_size_mult = ((csd >> 47) & 0x7) as u32;
        let block_len = 1u64 << read_bl_len;
        let mult = 1u64 << (c_size_mult + 2);
        let bytes = (c_size + 1) * mult * block_len;
        (bytes / u64::from(BLOCK_SIZE), false)
    }
}

fn ciu_clock(info: &PlatformDeviceInfo) -> Result<Option<(u32, DtbResourceLease)>, PnpError> {
    let reference = info
        .dtb_reference_by_name("clocks", "ciu")
        .or_else(|| info.dtb_references("clocks").nth(1))
        .or_else(|| info.dtb_references("clocks").next());
    let Some(reference) = reference else {
        return Ok(None);
    };
    let lease =
        dt_provider::acquire_reference(reference).map_err(DtbProviderError::into_pnp_error)?;
    let rate = match lease
        .control(DtbResourceRequest::GetRate)
        .map_err(DtbProviderError::into_pnp_error)?
    {
        DtbResourceReply::Value(rate) => rate,
        _ => return Ok(None),
    };
    Ok(Some((
        u32::try_from(rate).map_err(|_| {
            PnpError::malformed(PnpResourceKind::Other("clock"), "ciu rate too large")
        })?,
        lease,
    )))
}

fn fifo_depth(info: &PlatformDeviceInfo) -> u32 {
    info.bytes_property("fifo-depth")
        .and_then(|raw| {
            let bytes: [u8; 4] = raw.try_into().ok()?;
            Some(u32::from_be_bytes(bytes))
        })
        .filter(|depth| *depth != 0 && *depth <= 1024)
        .unwrap_or(32)
}

static NEXT_DISK: AtomicUsize = AtomicUsize::new(0);

struct JhMmcIo {
    host: JhMmcHost,
    card: CardInfo,
}

impl BlockDriver for JhMmcIo {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn queue_bio(&self, mut bio: Bio) -> Result<(), (SubmitError, Bio)> {
        if bio.op == BioOp::Flush {
            bio.complete(Ok(()));
            return Ok(());
        }
        if !matches!(bio.op, BioOp::Read | BioOp::Write) {
            return Err((SubmitError::Unsupported, bio));
        }
        let block_size = bio.block_size.get() as usize;
        if block_size != BLOCK_SIZE as usize {
            return Err((
                SubmitError::InvalidRequest(crate::dev::bio::BioReqError::BufferSizeMismatch),
                bio,
            ));
        }
        let total = (bio.range.blocks as usize).checked_mul(block_size);
        let Some(total) = total else {
            return Err((
                SubmitError::InvalidRequest(crate::dev::bio::BioReqError::TooLarge),
                bio,
            ));
        };
        if bio.buffer.len() != total {
            return Err((
                SubmitError::InvalidRequest(crate::dev::bio::BioReqError::BufferSizeMismatch),
                bio,
            ));
        }
        let lba = bio.range.lba;
        // 逐块传输，块数据经临时缓冲与（可能跨段、段长未必 512 对齐的）
        // 拼接缓冲视图散收（scatter/gather）。文件系统后端（如 extfs 的块组
        // 描述符读取）会传 64 字节级的小段，不能按整块切片。
        let mut scratch = [0u8; BLOCK_SIZE as usize];
        let mut failed = false;
        let mut seg = 0usize;
        let mut seg_off = 0usize;
        for block in 0..bio.range.blocks as u64 {
            let is_read = matches!(bio.op, BioOp::Read);
            if is_read {
                if self
                    .host
                    .read_block(&self.card, lba + block, &mut scratch)
                    .is_err()
                {
                    failed = true;
                    break;
                }
            }
            // 散收 scratch 与段视图之间的一块数据。
            let mut off = 0usize;
            while off < scratch.len() {
                let Some(seg_len) = bio.buffer.segment(seg).map(|s| s.len()) else {
                    failed = true;
                    break;
                };
                if seg_len == 0 || seg_off >= seg_len {
                    seg += 1;
                    seg_off = 0;
                    continue;
                }
                let take = (scratch.len() - off).min(seg_len - seg_off);
                let ok = if is_read {
                    bio.buffer.with_segment_mut(seg, |s| {
                        s[seg_off..seg_off + take].copy_from_slice(&scratch[off..off + take]);
                    })
                } else {
                    bio.buffer.with_segment(seg, |s| {
                        scratch[off..off + take].copy_from_slice(&s[seg_off..seg_off + take]);
                    })
                };
                if ok.is_none() {
                    failed = true;
                    break;
                }
                seg_off += take;
                off += take;
            }
            if failed {
                break;
            }
            if !is_read
                && self
                    .host
                    .write_block(&self.card, lba + block, &scratch)
                    .is_err()
            {
                failed = true;
                break;
            }
        }
        if failed {
            bio.complete(Err(BioIoError::MediaError));
        } else {
            bio.complete(Ok(()));
        }
        Ok(())
    }
}

struct JhMmcBinding {
    // Hold the registered block controller until the PnP device is removed.
    _io: Arc<JhMmcIo>,
}

struct JhMmcDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl JhMmcDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id("starfive,jh7110-mmc")
    }
}

impl PnpDriver for JhMmcDriver {
    fn name(&self) -> &'static str {
        "platform-jh7110-mmc"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &alloc::sync::Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let (phys, size) = info
            .first_mmio()
            .ok_or(PnpError::missing(PnpResourceKind::Mmio, "mmc reg missing"))?;
        if size < 0x300 {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "mmc reg window too small",
            ));
        }
        let base = (self.device_mmio_to_virt)(phys);

        // 时钟（ciu）来自 CRG provider。
        let ciu_reference = info
            .dtb_reference_by_name("clocks", "ciu")
            .or_else(|| info.dtb_references("clocks").nth(1))
            .or_else(|| info.dtb_references("clocks").next());
        let (ciu_hz, lease) = match ciu_clock(info)? {
            Some(value) => value,
            None => {
                let dependency = ciu_reference
                    .map(|reference| PnpDependency::DtbProvider {
                        kind: crate::dev::dt_provider::DtbProviderKind::Clock as u16,
                        phandle: reference.phandle,
                    })
                    .unwrap_or(PnpDependency::Other("mmc-ciu-clock"));
                return Err(PnpError::dependency(dependency));
            }
        };
        let fifo_depth = fifo_depth(info);
        let verid = unsafe { core::ptr::read_volatile(base.wrapping_add(REG_VERID) as *const u32) };
        let fifo_data = base
            + if verid < VERID_240A {
                DATA_OFFSET
            } else {
                DATA_240A_OFFSET
            };
        let idmac_page = alloc_idmac_page()
            .map_err(|_| PnpError::hardware_failure("idmac low-memory page allocation failed"))?;
        let host = JhMmcHost {
            base,
            fifo_data,
            ciu_hz,
            idmac_page,
            dma_coherent: info.dma.constraints().coherent,
        };
        dev.own_boxed_resource(dt_provider::lease_pnp_resource_boxed(
            lease,
            "jh7110-mmc-ciu",
        ))?;

        host.reset();
        // FIFOTH：MSIZE=2（IDMAC 突发，与 Linux/U-Boot 一致）+ rx/tx 水位。
        let rx_wm = fifo_depth / 2 - 1;
        let tx_wm = fifo_depth / 2;
        host.write32(REG_FIFOTH, (2 << 28) | (rx_wm << 16) | tx_wm);
        host.set_clock(INIT_CLOCK_HZ)
            .map_err(|_| PnpError::hardware_failure("mmc clock init failed"))?;

        // 卡初始化对总线瞬时状态敏感（插卡后首次上电尤其如此）：失败后
        // 完整复位控制器重试一次，与 Linux mmc_rescan 的重试语义一致。
        let card = match host.card_init() {
            Ok(card) => card,
            Err(first) => {
                log::warning!(
                    "[jh7110-mmc] card init attempt 1 failed: {:?}; retrying",
                    first
                );
                host.reset();
                host.write32(REG_FIFOTH, (2 << 28) | (rx_wm << 16) | tx_wm);
                if host.set_clock(INIT_CLOCK_HZ).is_err() {
                    return Err(PnpError::hardware_failure("mmc clock init failed"));
                }
                host.card_init().map_err(|error| {
                    log::warning!("[jh7110-mmc] card init failed: {:?}", error);
                    PnpError::hardware_failure("mmc card init failed")
                })?
            }
        };

        let io = Arc::new(JhMmcIo { host, card });
        let disk_index = NEXT_DISK.fetch_add(1, Ordering::Relaxed);
        let name = alloc::format!("mmc{}", disk_index);

        let block = BlockDevice::new(
            BlockDeviceInit {
                name: &name,
                subsystem: "mmc",
                class: BlockClass::Whole,
                geometry: BlockGeometry::new(
                    NonZeroU32::new(BLOCK_SIZE).expect("512"),
                    NonZeroU32::new(BLOCK_SIZE).expect("512"),
                    Some(io.card.block_count),
                )
                .ok_or(PnpError::OutOfMemory)?,
                limits: BlockLimits::new(
                    Some(NonZeroU32::new(256).expect("256")),
                    Some(NonZeroU32::new(1).expect("1")),
                    Some(NonZeroU32::new(4).expect("4")),
                )
                .unwrap_or_else(BlockLimits::unrestricted),
                attributes: BlockAttributes::new(
                    false,
                    false,
                    Some(NonZeroU32::new(1).expect("1")),
                    None,
                ),
                features: BlockFeatures(0),
            },
            io.clone(),
            None,
        );
        dev.register_function(BlockFunction::with_projection_name_arc(
            &dev.name,
            &name,
            Arc::new(block),
        ))?;
        log::printk!(
            "[jh7110-mmc] bound {} phys={:#x} verid={:#x} ciu={} blocks={} -> /dev/{}",
            dev.id,
            phys,
            verid,
            ciu_hz,
            io.card.block_count,
            name
        );
        dev.set_driver_data(Arc::new(JhMmcBinding { _io: io }));
        Ok(())
    }

    fn remove(&self, dev: &alloc::sync::Arc<PnpDevice>) {
        if dev.take_driver_data().is_some() {
            log::printk!("[jh7110-mmc] removed {}", dev.id);
        }
    }
}

struct JhMmcFactory;

impl DriverFactory for JhMmcFactory {
    fn name(&self) -> &'static str {
        "platform-jh7110-mmc"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(JhMmcDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(JhMmcFactory))
}
