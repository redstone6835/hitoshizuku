//! EHCI 主机控制器驱动（2K1000 ehci@40060000）。
//!
//! 实现 EHCI 1.0 的操作寄存器控制与 async 调度表：QH/qTD 池（DMA 缓冲）、
//! 控制/批量/中断传输、根集线器端口（电源/复位/连接/速度），以及
//! FS/LS 设备到伴生 OHCI 的 PORT_OWNER 移交。中断端点也放在 async 表上
//! （轮询完成，带宽语义简化；HS 中断端点实际工作正常）。
//!
//! 传输完成采用轮询 qTD status；修改调度表时停掉 ASE 避免与硬件竞争。
//! 控制端点用 QH DT=1（toggle 取自 qTD bit31，Linux 语义），批量/中断
//! QH 按端点缓存、DT=0 由硬件在 QH overlay 中维护 toggle。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;

use general::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use vfs::sync::Spinlock;

use crate::core::UsbHcd;
use crate::regs::*;

const QH_COUNT: usize = 32;
const QTD_COUNT: usize = 128;
const FRAME_LIST_SIZE: usize = 1024;
const TRANSFER_TIMEOUT_LOOPS: u32 = 400_000;
const RESET_TIMEOUT_LOOPS: u32 = 100_000;

fn delay_ns(duration_ns: u64) {
    let deadline = sched::now_ns_public().saturating_add(duration_ns);
    while sched::now_ns_public() < deadline {
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Control,
    Bulk,
    Interrupt,
}

struct Transfer {
    qh: usize,
    qtds: Vec<usize>,
    /// 数据阶段期望字节数（控制= data.len()；批量/中断= data.len()）。
    expected: usize,
    /// 控制传输的数据 qTD（用于剩余长度换算）。
    data_qtd: Option<usize>,
    transient_qh: bool,
}

/// EHCI 主机控制器实例。
pub struct EhciHcd {
    op: usize,
    ports: usize,
    frame_list: DmaBuffer,
    qh_pool: DmaBuffer,
    qtd_pool: DmaBuffer,
    qh_free: Spinlock<Vec<usize>>,
    qtd_free: Spinlock<Vec<usize>>,
    /// 缓存的批量/中断 QH：key = (dev_addr, ep, dir)。
    cached_qh: Spinlock<BTreeMap<(u8, u8, bool), usize>>,
    head_qh: usize,
    transfers: Spinlock<Vec<Transfer>>,
}

impl EhciHcd {
    pub fn new(op_base: usize, cap_base: usize, context: DmaContext) -> Result<Self, &'static str> {
        // Safety: 能力寄存器由 platform probe 映射的 MMIO 窗口提供。
        let ports = unsafe { core::ptr::read_volatile((cap_base + EHCI_HCSPARAMS) as *const u32) }
            & 0x0f;
        let frame_list = DmaBuffer::new_in(
            context.clone(),
            FRAME_LIST_SIZE * 4,
            4096,
            DmaDirection::Bidirectional,
        )?;
        let qh_pool = DmaBuffer::new_in(
            context.clone(),
            QH_COUNT * core::mem::size_of::<EhciQhHw>(),
            64,
            DmaDirection::Bidirectional,
        )?;
        let qtd_pool = DmaBuffer::new_in(
            context,
            QTD_COUNT * core::mem::size_of::<EhciQtdHw>(),
            64,
            DmaDirection::Bidirectional,
        )?;
        let mut hcd = Self {
            op: op_base,
            ports: ports as usize,
            frame_list,
            qh_pool,
            qtd_pool,
            qh_free: Spinlock::new((0..QH_COUNT).collect()),
            qtd_free: Spinlock::new((0..QTD_COUNT).collect()),
            cached_qh: Spinlock::new(BTreeMap::new()),
            head_qh: 0,
            transfers: Spinlock::new(Vec::new()),
        };
        hcd.hc_reset()?;
        // anchor QH（head of async list）。
        hcd.head_qh = hcd.alloc_qh()?;
        let head = hcd.qh_mut(hcd.head_qh);
        head.info1 = QH_HEAD;
        head.next = QH_NEXT_TERMINATE;
        hcd.write_op(EHCI_ASYNCLISTADDR, hcd.qh_dma(hcd.head_qh) as u32);
        // 帧列表全指向 terminate。
        let frames = hcd.frame_list.as_mut_slice();
        for slot in frames.chunks_exact_mut(4) {
            slot.copy_from_slice(&QH_NEXT_TERMINATE.to_le_bytes());
        }
        hcd.frame_list.sync_for_device();
        hcd.write_op(EHCI_PERIODICLISTBASE, hcd.frame_list.dma_addr() as u32);
        // 运行调度。
        hcd.write_op(
            EHCI_USBCMD,
            EHCI_CMD_RUN | EHCI_CMD_ASE | EHCI_CMD_PSE | (8 << 16),
        );
        hcd.write_op(EHCI_CONFIGFLAG, 1);
        hcd.write_op(
            EHCI_USBINTR,
            EHCI_INTR_PORTCHANGE | EHCI_INTR_ERRINT | EHCI_INTR_IAA | EHCI_INTR_USBINT,
        );
        Ok(hcd)
    }

    fn hc_reset(&self) -> Result<(), &'static str> {
        self.write_op(EHCI_USBCMD, EHCI_CMD_HCRESET);
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBCMD) & EHCI_CMD_HCRESET == 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err("EHCI host controller reset timeout")
    }

    fn read_op(&self, offset: usize) -> u32 {
        // Safety: op 基址由 platform probe 映射，offset 为固定寄存器偏移。
        unsafe { core::ptr::read_volatile((self.op + offset) as *const u32) }
    }

    fn write_op(&self, offset: usize, value: u32) {
        // Safety: 同 read_op，目标寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile((self.op + offset) as *mut u32, value) }
    }

    fn portsc(&self, port: usize) -> usize {
        EHCI_PORTSC + port * 4
    }

    // ── 池管理 ──

    fn qh_ptr(&self, index: usize) -> *mut EhciQhHw {
        // Safety: index < QH_COUNT，qh_pool 常驻 DMA 缓冲。
        (self.qh_pool.vaddr() + index * core::mem::size_of::<EhciQhHw>()) as *mut EhciQhHw
    }

    fn qh_dma(&self, index: usize) -> usize {
        self.qh_pool.dma_addr() + index * core::mem::size_of::<EhciQhHw>()
    }

    fn qh(&self, index: usize) -> &EhciQhHw {
        // Safety: 调度表修改在停表状态下串行执行，读取在轮询路径。
        unsafe { &*self.qh_ptr(index) }
    }

    fn qh_mut(&self, index: usize) -> &mut EhciQhHw {
        // Safety: 同 qh()。
        unsafe { &mut *self.qh_ptr(index) }
    }

    fn qtd_ptr(&self, index: usize) -> *mut EhciQtdHw {
        // Safety: index < QTD_COUNT，qtd_pool 常驻 DMA 缓冲。
        (self.qtd_pool.vaddr() + index * core::mem::size_of::<EhciQtdHw>()) as *mut EhciQtdHw
    }

    fn qtd_dma(&self, index: usize) -> usize {
        self.qtd_pool.dma_addr() + index * core::mem::size_of::<EhciQtdHw>()
    }

    fn qtd(&self, index: usize) -> &EhciQtdHw {
        // Safety: 同 qh()。
        unsafe { &*self.qtd_ptr(index) }
    }

    fn qtd_mut(&self, index: usize) -> &mut EhciQtdHw {
        // Safety: 同 qh_mut()。
        unsafe { &mut *self.qtd_ptr(index) }
    }

    fn alloc_qh(&self) -> Result<usize, &'static str> {
        self.qh_free.lock().pop().ok_or("EHCI QH pool exhausted")
    }

    fn alloc_qtd(&self) -> Result<usize, &'static str> {
        self.qtd_free.lock().pop().ok_or("EHCI qTD pool exhausted")
    }

    fn free_qh(&self, index: usize) {
        self.qh_free.lock().push(index);
    }

    fn free_qtd(&self, index: usize) {
        self.qtd_free.lock().push(index);
    }

    // ── async 调度表 ──

    fn stop_async(&self) {
        self.write_op(EHCI_USBCMD, self.read_op(EHCI_USBCMD) & !EHCI_CMD_ASE);
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBSTS) & (1 << 15) == 0 {
                break;
            }
            delay_ns(100);
        }
    }

    fn start_async(&self) {
        self.write_op(EHCI_USBCMD, self.read_op(EHCI_USBCMD) | EHCI_CMD_ASE);
    }

    fn link_qh(&self, qh: usize) {
        let head = self.qh_mut(self.head_qh);
        let first = head.next;
        let next = if first & QH_NEXT_TERMINATE != 0 {
            self.qh_dma(self.head_qh) as u32
        } else {
            first
        };
        head.next = self.qh_dma(qh) as u32;
        self.qh_mut(qh).next = next;
        compiler_fence(Ordering::SeqCst);
    }

    fn unlink_qh(&self, qh: usize) {
        let target = self.qh_dma(qh) as u32;
        let mut current = self.head_qh;
        for _ in 0..QH_COUNT {
            let next = self.qh(current).next;
            if next & QH_NEXT_TERMINATE != 0 {
                break;
            }
            if next == target {
                self.qh_mut(current).next = self.qh(qh).next;
                break;
            }
            current = ((next & !1) - self.qh_pool.dma_addr() as u32) as usize
                / core::mem::size_of::<EhciQhHw>();
        }
        compiler_fence(Ordering::SeqCst);
    }

    // ── 传输 ──

    fn build_qtd(
        &self,
        buffer: *const u8,
        len: usize,
        pid: u32,
        toggle: bool,
        ioc: bool,
    ) -> Result<usize, &'static str> {
        let index = self.alloc_qtd()?;
        let qtd = self.qtd_mut(index);
        *qtd = EhciQtdHw::default();
        let mut token = QTD_STS_ACTIVE | (EHCI_TUNE_CERR << QTD_CERR_SHIFT);
        if toggle {
            token |= QTD_TOGGLE;
        }
        if ioc {
            token |= QTD_IOC;
        }
        token |= pid << QTD_PID_SHIFT;
        token |= (len as u32) << QTD_LENGTH_SHIFT;
        qtd.token = token;
        qtd.next = QTD_TERMINATE;
        qtd.alt_next = QTD_TERMINATE;
        if len != 0 {
            qtd.buf0 = buffer as u32;
        }
        Ok(index)
    }

    fn remaining(&self, qtd: usize) -> usize {
        ((self.qtd(qtd).token >> QTD_LENGTH_SHIFT) & 0x7fff) as usize
    }

    fn submit_linked(
        &self,
        dev_addr: u8,
        ep: u8,
        data_in: bool,
        kind: TransferKind,
        qtds: Vec<usize>,
        expected: usize,
        data_qtd: Option<usize>,
    ) -> Result<(), &'static str> {
        let transient_qh = kind == TransferKind::Control;
        let qh = if transient_qh {
            self.alloc_qh()?
        } else {
            let mut cache = self.cached_qh.lock();
            let key = (dev_addr, ep, data_in);
            if let Some(&index) = cache.get(&key) {
                index
            } else {
                let index = self.alloc_qh()?;
                cache.insert(key, index);
                index
            }
        };
        let max_packet = if kind == TransferKind::Control { 64 } else { 512 };
        let qh_hw = self.qh_mut(qh);
        *qh_hw = EhciQhHw::default();
        qh_hw.info1 = (u32::from(dev_addr) & 0x7f)
            | (u32::from(ep & 0x0f) << 8)
            | QH_HIGH_SPEED
            | ((max_packet as u32) << 16)
            | (EHCI_TUNE_RL_HS << 28);
        // 控制端点 DT=1（toggle 取自 qTD bit31）；批量/中断 DT=0（硬件
        // 在 QH overlay 维护 toggle，QH 按端点缓存以跨传输保持）。
        if transient_qh {
            qh_hw.info1 |= QH_TOGGLE_CTL;
        }
        qh_hw.info2 = 1 << 30; // MULT = 1
        qh_hw.qtd_next = self.qtd_dma(qtds[0]) as u32;
        qh_hw.token = self.qtd(qtds[0]).token;
        qh_hw.buf0 = self.qtd(qtds[0]).buf0;
        let mut previous = qtds[0];
        for &qtd in &qtds[1..] {
            self.qtd_mut(previous).next = self.qtd_dma(qtd) as u32;
            previous = qtd;
        }
        // 完成中断放在最后一个 qTD。
        let last = *qtds.last().ok_or("empty qtd chain")?;
        self.qtd_mut(last).token |= QTD_IOC;

        // 停表 → 链接 → 同步 → 启表。
        self.stop_async();
        self.link_qh(qh);
        self.qtd_pool.sync_for_device();
        self.qh_pool.sync_for_device();
        self.start_async();

        self.transfers.lock().push(Transfer {
            qh,
            qtds,
            expected,
            data_qtd,
            transient_qh,
        });
        Ok(())
    }

    /// 轮询等待最早的未完成传输，完成后归还 QH/qTD 并返回数据字节数。
    fn poll_completion(&self) -> Result<usize, &'static str> {
        let deadline = sched::now_ns_public().saturating_add(2_000_000_000);
        loop {
            let completed = {
                let transfers = self.transfers.lock();
                if transfers.is_empty() {
                    None
                } else {
                    let transfer = &transfers[0];
                    let last = *transfer.qtds.last().expect("non-empty qtd chain");
                    if self.qtd(last).token & QTD_STS_ACTIVE != 0 {
                        None
                    } else if self.qtd(last).token & QTD_STS_HALT != 0 {
                        Some(Err("EHCI transfer halted"))
                    } else {
                        Some(Ok(()))
                    }
                }
            };
            if let Some(result) = completed {
                let transfer = self.transfers.lock().remove(0);
                self.stop_async();
                self.unlink_qh(transfer.qh);
                self.start_async();
                let transferred = if let Err(error) = result {
                    for qtd in &transfer.qtds {
                        self.free_qtd(*qtd);
                    }
                    if transfer.transient_qh {
                        self.free_qh(transfer.qh);
                    }
                    return Err(error);
                } else {
                    let transferred = match transfer.data_qtd {
                        Some(data_qtd) => transfer.expected.saturating_sub(self.remaining(data_qtd)),
                        None => {
                            let last = *transfer.qtds.last().expect("non-empty qtd chain");
                            transfer.expected.saturating_sub(self.remaining(last))
                        }
                    };
                    for qtd in &transfer.qtds {
                        self.free_qtd(*qtd);
                    }
                    if transfer.transient_qh {
                        self.free_qh(transfer.qh);
                    }
                    transferred
                };
                return Ok(transferred.min(transfer.expected));
            }
            if sched::now_ns_public() >= deadline {
                return Err("EHCI transfer timeout");
            }
            delay_ns(5_000);
        }
    }
}

impl UsbHcd for EhciHcd {
    fn name(&self) -> &'static str {
        "ehci"
    }

    fn port_count(&self) -> usize {
        self.ports
    }

    fn port_power_on(&self, port: usize) -> Result<(), &'static str> {
        let reg = self.portsc(port);
        self.write_op(reg, self.read_op(reg) | EHCI_PORTSC_PP | EHCI_PORTSC_WKCNNT_E);
        Ok(())
    }

    fn port_connected(&self, port: usize) -> bool {
        self.read_op(self.portsc(port)) & EHCI_PORTSC_CCS != 0
    }

    fn port_reset(&self, port: usize) -> Result<u8, &'static str> {
        let reg = self.portsc(port);
        self.write_op(reg, (self.read_op(reg) | EHCI_PORTSC_PR) & !EHCI_PORTSC_CSC);
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read_op(reg) & EHCI_PORTSC_PR == 0 {
                break;
            }
            delay_ns(1_000);
        }
        if self.read_op(reg) & EHCI_PORTSC_PR != 0 {
            return Err("EHCI port reset timeout");
        }
        // 等待端口使能（HS 枚举完成）。
        for _ in 0..RESET_TIMEOUT_LOOPS {
            if self.read_op(reg) & EHCI_PORTSC_PED != 0 {
                break;
            }
            if self.read_op(reg) & EHCI_PORTSC_CCS == 0 {
                return Err("EHCI device disconnected during reset");
            }
            delay_ns(1_000);
        }
        let portsc = self.read_op(reg);
        let speed = ((portsc & EHCI_PORTSC_DEVSPD_MASK) >> EHCI_PORTSC_DEVSPD_SHIFT) as u8;
        // FS/LS 设备移交伴生 OHCI（PORT_OWNER 后 EHCI 端口不再工作）。
        if speed != USB_SPEED_HIGH {
            self.write_op(reg, self.read_op(reg) | EHCI_PORTSC_PORT_OWNER);
        }
        Ok(if speed == USB_SPEED_LOW {
            USB_SPEED_LOW
        } else if speed == USB_SPEED_FULL {
            USB_SPEED_FULL
        } else {
            USB_SPEED_HIGH
        })
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
        let mut qtds = Vec::new();
        // SETUP（toggle DATA0）。
        qtds.push(self.build_qtd(setup_bytes.as_ptr(), 8, QTD_PID_SETUP, false, false)?);
        // DATA（toggle DATA1；零长度时 status 直接为 IN）。
        let mut data_pid = if data_in || data.is_empty() {
            QTD_PID_IN
        } else {
            QTD_PID_OUT
        };
        let data_qtd = if data.is_empty() {
            None
        } else {
            qtds.push(self.build_qtd(data.as_ptr(), data.len(), data_pid, true, false)?);
            Some(qtds.len() - 1)
        };
        // STATUS（pid 与 data 相反，强制 DATA1）。
        data_pid = if data_pid == QTD_PID_IN {
            QTD_PID_OUT
        } else {
            QTD_PID_IN
        };
        qtds.push(self.build_qtd(core::ptr::null(), 0, data_pid, true, true)?);
        let expected = data.len();
        self.submit_linked(dev_addr, 0, data_in, TransferKind::Control, qtds, expected, data_qtd)?;
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
        let pid = if data_in { QTD_PID_IN } else { QTD_PID_OUT };
        let mut qtds = Vec::new();
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(16384);
            qtds.push(self.build_qtd(
                data.as_ptr().wrapping_add(offset),
                chunk,
                pid,
                false,
                false,
            )?);
            offset += chunk;
        }
        if qtds.is_empty() {
            qtds.push(self.build_qtd(core::ptr::null(), 0, pid, false, true)?);
        }
        let expected = data.len();
        self.submit_linked(dev_addr, ep & 0x0f, data_in, TransferKind::Bulk, qtds, expected, None)?;
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
        let pid = if data_in { QTD_PID_IN } else { QTD_PID_OUT };
        let mut qtds = Vec::new();
        qtds.push(self.build_qtd(data.as_ptr(), data.len(), pid, false, true)?);
        let expected = data.len();
        self.submit_linked(
            dev_addr,
            ep & 0x0f,
            data_in,
            TransferKind::Interrupt,
            qtds,
            expected,
            None,
        )?;
        let transferred = self.poll_completion()?;
        Ok(transferred.min(data.len()))
    }
}
