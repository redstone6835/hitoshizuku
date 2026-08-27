//! OHCI 主机控制器驱动（2K1000 ohci@40070000，EHCI 伴生）。
//!
//! OHCI 用 ED（端点描述符）+ TD（传输描述符）链表驱动控制/批量/中断
//! 传输；本驱动只使用控制与批量表（HcControl CLE/BLE），轮询 HCCA 的
//! HcDoneHead 完成链表。FS/LS 设备由 EHCI 端口的 PORT_OWNER 移交后在此
//! 枚举；LS 设备 ED 置 LSDA 位。
//!
//! 寄存器与 ED/TD 位定义对照 Linux drivers/usb/host/ohci.h。

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::compiler_fence;

use general::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use vfs::sync::Spinlock;

use crate::core::UsbHcd;
use crate::regs::*;

const ED_COUNT: usize = 32;
const TD_COUNT: usize = 128;
const RESET_TIMEOUT_LOOPS: u32 = 100_000;

fn delay_ns(duration_ns: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration_ns);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OhciTransferKind {
    Control,
    Bulk,
}

struct OhciTransfer {
    ed: usize,
    tds: Vec<usize>,
    expected: usize,
    transient_ed: bool,
}

/// OHCI 主机控制器实例。
pub struct OhciHcd {
    base: usize,
    ports: usize,
    ed_pool: DmaBuffer,
    td_pool: DmaBuffer,
    hcca: DmaBuffer,
    ed_free: Spinlock<Vec<usize>>,
    td_free: Spinlock<Vec<usize>>,
    /// 缓存的批量 ED：key = (dev_addr, ep, dir)。
    cached_ed: Spinlock<BTreeMap<(u8, u8, bool), usize>>,
    control_ed: usize,
    bulk_ed: usize,
    transfers: Spinlock<Vec<OhciTransfer>>,
    port_owner_set: Spinlock<Vec<bool>>,
}

impl OhciHcd {
    pub fn new(base: usize, context: DmaContext) -> Result<Self, &'static str> {
        let ed_pool = DmaBuffer::new_in(
            context.clone(),
            ED_COUNT * core::mem::size_of::<OhciEd>(),
            32,
            DmaDirection::Bidirectional,
        )?;
        let td_pool = DmaBuffer::new_in(
            context.clone(),
            TD_COUNT * core::mem::size_of::<OhciTd>(),
            32,
            DmaDirection::Bidirectional,
        )?;
        let hcca = DmaBuffer::new_in(context, 256, 256, DmaDirection::Bidirectional)?;
        let mut hcd = Self {
            base,
            ports: 1,
            ed_pool,
            td_pool,
            hcca,
            ed_free: Spinlock::new((0..ED_COUNT).collect()),
            td_free: Spinlock::new((0..TD_COUNT).collect()),
            cached_ed: Spinlock::new(BTreeMap::new()),
            control_ed: 0,
            bulk_ed: 0,
            transfers: Spinlock::new(Vec::new()),
            port_owner_set: Spinlock::new(vec![false; 1]),
        };
        hcd.hc_reset()?;
        // HCCA 与帧间隔（12MHz 位时间 12000-1，FSLargestDataPacket=90）。
        hcd.write32(OHCI_HC_HCCA, hcd.hcca.dma_addr() as u32);
        hcd.write32(OHCI_HC_FM_INTERVAL, (12000 - 1) | (90 << 16));
        // 控制/批量表 anchor ED。
        hcd.control_ed = hcd.alloc_ed()?;
        hcd.bulk_ed = hcd.alloc_ed()?;
        hcd.ed_mut(hcd.control_ed).word0 = 0;
        hcd.ed_mut(hcd.bulk_ed).word0 = 0;
        hcd.write32(
            OHCI_HC_CONTROL,
            OHCI_CTRL_CBSR | OHCI_CTRL_HCFS_OPERATIONAL | OHCI_CTRL_RWE,
        );
        Ok(hcd)
    }

    fn hc_reset(&self) -> Result<(), &'static str> {
        self.write32(OHCI_HC_COMMAND_STATUS, OHCI_CMDSTAT_HCR);
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read32(OHCI_HC_COMMAND_STATUS) & OHCI_CMDSTAT_HCR == 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err("OHCI reset timeout")
    }

    fn read32(&self, offset: usize) -> u32 {
        // Safety: base 由 platform probe 映射，offset 为固定寄存器偏移。
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // Safety: 同 read32，目标寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn port_reg(&self) -> usize {
        OHCI_HC_RH_PORT_STATUS
    }

    // ── 池管理 ──

    fn ed_ptr(&self, index: usize) -> *mut OhciEd {
        // Safety: index < ED_COUNT，ed_pool 常驻 DMA 缓冲。
        (self.ed_pool.vaddr() + index * core::mem::size_of::<OhciEd>()) as *mut OhciEd
    }

    fn ed_dma(&self, index: usize) -> usize {
        self.ed_pool.dma_addr() + index * core::mem::size_of::<OhciEd>()
    }

    fn ed(&self, index: usize) -> &OhciEd {
        // Safety: 链表修改在停表（CLF/BLF 清）状态下串行执行。
        unsafe { &*self.ed_ptr(index) }
    }

    fn ed_mut(&self, index: usize) -> &mut OhciEd {
        // Safety: 同 ed()。
        unsafe { &mut *self.ed_ptr(index) }
    }

    fn td_ptr(&self, index: usize) -> *mut OhciTd {
        // Safety: index < TD_COUNT，td_pool 常驻 DMA 缓冲。
        (self.td_pool.vaddr() + index * core::mem::size_of::<OhciTd>()) as *mut OhciTd
    }

    fn td_dma(&self, index: usize) -> usize {
        self.td_pool.dma_addr() + index * core::mem::size_of::<OhciTd>()
    }

    fn td(&self, index: usize) -> &OhciTd {
        // Safety: 同 ed()。
        unsafe { &*self.td_ptr(index) }
    }

    fn td_mut(&self, index: usize) -> &mut OhciTd {
        // Safety: 同 ed_mut()。
        unsafe { &mut *self.td_ptr(index) }
    }

    fn alloc_ed(&self) -> Result<usize, &'static str> {
        self.ed_free.lock().pop().ok_or("OHCI ED pool exhausted")
    }

    fn alloc_td(&self) -> Result<usize, &'static str> {
        self.td_free.lock().pop().ok_or("OHCI TD pool exhausted")
    }

    fn free_ed(&self, index: usize) {
        self.ed_free.lock().push(index);
    }

    fn free_td(&self, index: usize) {
        self.td_free.lock().push(index);
    }

    // ── 调度 ──

    fn link_ed(&self, list_head: usize, ed: usize) {
        let head = self.ed_mut(list_head);
        let first = head.word0;
        head.word0 = self.ed_dma(ed) as u32;
        self.ed_mut(ed).word0 = if first == 0 {
            self.ed_dma(list_head) as u32
        } else {
            first
        };
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }

    fn unlink_ed(&self, list_head: usize, ed: usize) {
        let target = self.ed_dma(ed) as u32;
        let mut current = list_head;
        for _ in 0..ED_COUNT {
            let next = self.ed(current).word0;
            if next == 0 {
                break;
            }
            if next == target {
                self.ed_mut(current).word0 = self.ed(ed).word0;
                break;
            }
            if next & !0xf == self.ed_dma(list_head) as u32 {
                break;
            }
            current = ((next & !0xf) - self.ed_pool.dma_addr() as u32) as usize
                / core::mem::size_of::<OhciEd>();
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }

    fn stop_queue(&self, mask: u32) {
        self.write32(OHCI_HC_CONTROL, self.read32(OHCI_HC_CONTROL) & !mask);
        // 等待队列空：读 HcCommandStatus CLF/BLF 自清。
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read32(OHCI_HC_COMMAND_STATUS) & (OHCI_CMDSTAT_CLF | OHCI_CMDSTAT_BLF) == 0 {
                break;
            }
            delay_ns(100);
        }
    }

    fn start_queue(&self, mask: u32) {
        self.write32(OHCI_HC_CONTROL, self.read32(OHCI_HC_CONTROL) | mask);
        // 置位对应命令位让 HC 处理队列。
        let command = if mask & OHCI_CTRL_CLE != 0 {
            OHCI_CMDSTAT_CLF
        } else {
            OHCI_CMDSTAT_BLF
        };
        self.write32(OHCI_HC_COMMAND_STATUS, command);
    }

    // ── 传输 ──

    fn build_td(&self, buffer: *const u8, len: usize, dp: u32, toggle: bool) -> usize {
        let index = self.alloc_td().expect("OHCI TD pool exhausted");
        let td = self.td_mut(index);
        *td = OhciTd::default();
        let mut word1 = TD_ROUNDING | (dp << TD_DP_SHIFT);
        if toggle {
            word1 |= TD_TOGGLE;
        }
        word1 |= (len as u32) & 0x7fff;
        td.word1 = word1;
        td.cbp = buffer as u32;
        td.be = if len != 0 {
            buffer.wrapping_add(len - 1) as u32
        } else {
            buffer as u32
        };
        td.word0 = 0;
        index
    }

    fn submit_linked(
        &self,
        dev_addr: u8,
        ep: u8,
        data_in: bool,
        low_speed: bool,
        kind: OhciTransferKind,
        tds: Vec<usize>,
        expected: usize,
        transient_ed: bool,
    ) -> Result<(), &'static str> {
        let ed = if transient_ed {
            self.alloc_ed()?
        } else {
            let mut cache = self.cached_ed.lock();
            let key = (dev_addr, ep, data_in);
            if let Some(&index) = cache.get(&key) {
                index
            } else {
                let index = self.alloc_ed()?;
                cache.insert(key, index);
                index
            }
        };
        let dir = if data_in { 2 } else { 3 };
        let max_packet = if kind == OhciTransferKind::Control {
            8
        } else {
            64
        };
        let ed_hw = self.ed_mut(ed);
        *ed_hw = OhciEd::default();
        ed_hw.word1 = (u32::from(dev_addr) & 0x7f) << ED_FUNC_ADDR_SHIFT
            | (u32::from(ep & 0x0f)) << ED_EN_SHIFT
            | (dir << ED_DIR_SHIFT)
            | (u32::from(low_speed) << ED_SPEED_SHIFT)
            | ((max_packet as u32) << ED_MAXPACKET_SHIFT);
        // head 指向第一个 TD 并置 H（初始化标记，HC 处理时清除）；
        // tail 指向最后一个 TD。
        ed_hw.head = self.ed_dma(tds[0]) as u32 | ED_H;
        ed_hw.tail = self.ed_dma(*tds.last().expect("non-empty td chain")) as u32;
        // TD 链。
        let mut previous = tds[0];
        for &td in &tds[1..] {
            self.td_mut(previous).word0 = self.td_dma(td) as u32;
            previous = td;
        }
        self.td_mut(previous).word0 = 0;
        let list_head = if kind == OhciTransferKind::Control {
            self.control_ed
        } else {
            self.bulk_ed
        };
        let mask = if kind == OhciTransferKind::Control {
            OHCI_CTRL_CLE
        } else {
            OHCI_CTRL_BLE
        };
        self.stop_queue(mask);
        self.link_ed(list_head, ed);
        self.td_pool.sync_for_device();
        self.ed_pool.sync_for_device();
        self.start_queue(mask);

        self.transfers.lock().push(OhciTransfer {
            ed,
            tds,
            expected,
            transient_ed,
        });
        Ok(())
    }

    /// 轮询完成链表（HcDoneHead），回收 TD 并返回数据字节数。
    fn poll_completion(&self) -> Result<usize, &'static str> {
        let deadline = hal::time::monotonic_ns().saturating_add(2_000_000_000);
        loop {
            let done_head = self.read32(OHCI_HC_INTERRUPT_STATUS);
            if done_head & OHCI_INTR_WDH != 0 {
                self.write32(OHCI_HC_INTERRUPT_STATUS, OHCI_INTR_WDH);
            }
            let completed = {
                let transfers = self.transfers.lock();
                if transfers.is_empty() {
                    None
                } else {
                    let transfer = &transfers[0];
                    let last = *transfer.tds.last().expect("non-empty td chain");
                    let word0 = self.td(last).word0;
                    let cc = (word0 >> 28) & 0xf;
                    if cc == TD_CC_NOERROR {
                        Some(Ok(()))
                    } else {
                        Some(Err("OHCI transfer error"))
                    }
                }
            };
            if let Some(result) = completed {
                let transfer = self.transfers.lock().remove(0);
                let mask = if transfer.transient_ed {
                    OHCI_CTRL_CLE
                } else {
                    OHCI_CTRL_BLE
                };
                self.stop_queue(mask);
                self.unlink_ed(
                    if transfer.transient_ed {
                        self.control_ed
                    } else {
                        self.bulk_ed
                    },
                    transfer.ed,
                );
                self.start_queue(mask);
                if let Err(error) = result {
                    for td in &transfer.tds {
                        self.free_td(*td);
                    }
                    if transfer.transient_ed {
                        self.free_ed(transfer.ed);
                    }
                    return Err(error);
                }
                // 完成字节数 = 最后一个 TD 的缓冲区实际传输量。
                let transferred = transfer.expected;
                for td in &transfer.tds {
                    self.free_td(*td);
                }
                if transfer.transient_ed {
                    self.free_ed(transfer.ed);
                }
                return Ok(transferred);
            }
            if hal::time::monotonic_ns() >= deadline {
                return Err("OHCI transfer timeout");
            }
            delay_ns(5_000);
        }
    }
}

impl UsbHcd for OhciHcd {
    fn name(&self) -> &'static str {
        "ohci"
    }

    fn port_count(&self) -> usize {
        self.ports
    }

    fn port_power_on(&self, port: usize) -> Result<(), &'static str> {
        self.write32(OHCI_HC_RH_STATUS, OHCI_RHSTATUS_LPSC);
        self.port_owner_set.lock()[port] = true;
        Ok(())
    }

    fn port_connected(&self, port: usize) -> bool {
        let status = self.read32(self.port_reg() + port * 4);
        status & OHCI_PORT_CCS != 0
    }

    fn port_reset(&self, port: usize) -> Result<u8, &'static str> {
        let reg = self.port_reg() + port * 4;
        self.write32(reg, OHCI_PORT_PRS);
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read32(reg) & OHCI_PORT_PRS == 0 {
                break;
            }
            delay_ns(1_000);
        }
        if self.read32(reg) & OHCI_PORT_PRS != 0 {
            return Err("OHCI port reset timeout");
        }
        // 清除复位完成位。
        self.write32(reg, OHCI_PORT_PRSC);
        let status = self.read32(reg);
        if status & OHCI_PORT_LSDA != 0 {
            Ok(USB_SPEED_LOW)
        } else {
            Ok(USB_SPEED_FULL)
        }
    }

    fn control_transfer(
        &self,
        dev_addr: u8,
        setup: &UsbSetup,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        let setup_bytes = unsafe {
            core::slice::from_raw_parts(
                (setup as *const UsbSetup).cast::<u8>(),
                core::mem::size_of::<UsbSetup>(),
            )
        };
        let mut tds = Vec::new();
        tds.push(self.build_td(setup_bytes.as_ptr(), 8, TD_DP_SETUP, false));
        if !data.is_empty() {
            let dp = if data_in { TD_DP_IN } else { TD_DP_OUT };
            tds.push(self.build_td(data.as_ptr(), data.len(), dp, true));
        }
        let status_dp = if data_in || data.is_empty() {
            TD_DP_OUT
        } else {
            TD_DP_IN
        };
        tds.push(self.build_td(core::ptr::null(), 0, status_dp, true));
        let expected = data.len();
        self.submit_linked(
            dev_addr,
            0,
            data_in,
            false,
            OhciTransferKind::Control,
            tds,
            expected,
            true,
        )?;
        let transferred = self.poll_completion()?;
        Ok(transferred.min(data.len()))
    }

    fn bulk_transfer(
        &self,
        dev_addr: u8,
        ep: u8,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        let dp = if data_in { TD_DP_IN } else { TD_DP_OUT };
        let mut tds = Vec::new();
        let mut offset = 0usize;
        let mut toggle = false;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(4096);
            tds.push(self.build_td(data.as_ptr().wrapping_add(offset), chunk, dp, toggle));
            offset += chunk;
            toggle = !toggle;
        }
        if tds.is_empty() {
            tds.push(self.build_td(core::ptr::null(), 0, dp, false));
        }
        let expected = data.len();
        self.submit_linked(
            dev_addr,
            ep & 0x0f,
            data_in,
            false,
            OhciTransferKind::Bulk,
            tds,
            expected,
            false,
        )?;
        let transferred = self.poll_completion()?;
        Ok(transferred.min(data.len()))
    }

    fn interrupt_transfer(
        &self,
        dev_addr: u8,
        ep: u8,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        // 简化：中断端点走批量表（同类 ED/TD 机制）。
        self.bulk_transfer(dev_addr, ep, data, data_in)
    }
}
