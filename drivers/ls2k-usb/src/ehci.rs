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
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use general::dev::dma::{DmaBorrowedMapping, DmaBuffer, DmaContext, DmaDirection};
use vfs::sync::Spinlock;

use crate::core::UsbHcd;
use crate::regs::*;

const QH_COUNT: usize = 32;
const QTD_COUNT: usize = 128;
const FRAME_LIST_SIZE: usize = 1024;
const EHCI_PAGE_SIZE: usize = 4096;
const QTD_BUFFER_COUNT: usize = 5;
const HALT_TIMEOUT_LOOPS: u32 = 2_000;
const CONTROLLER_RESET_TIMEOUT_LOOPS: u32 = 250_000;
const PORT_RESET_ASSERT_NS: u64 = 50_000_000;
const PORT_RESET_CLEAR_TIMEOUT_LOOPS: u32 = 2_000;
const SCHEDULE_TIMEOUT_LOOPS: u32 = 100_000;

fn delay_ns(duration_ns: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration_ns);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Control,
    Bulk,
    Interrupt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AsyncStopState {
    ScheduleStopped,
    ControllerHalted,
}

struct Transfer {
    qh: usize,
    qtds: Vec<usize>,
    /// 描述符引用的映射必须一直存活到控制器停止访问本次传输。
    payload_dma: Vec<TransferDma>,
    /// 数据阶段期望字节数（控制= data.len()；批量/中断= data.len()）。
    expected: usize,
    /// 控制传输的数据 qTD（用于剩余长度换算）。
    data_qtd: Option<usize>,
    transient_qh: bool,
}

/// 一段 qTD payload 的 DMA 生命周期。
///
/// 优先直接映射调用方缓冲；地址不可达、物理页不连续或超出 EHCI 32 位地址域时，
/// 使用连续 DMA 缓冲中转。bounce 缓冲保存原地址，只在成功的 IN 传输后复制回去。
enum TransferDma {
    Borrowed(DmaBorrowedMapping),
    Bounce {
        buffer: DmaBuffer,
        original_vaddr: usize,
        len: usize,
        copy_back: bool,
    },
}

impl TransferDma {
    fn dma_addr(&self) -> usize {
        match self {
            Self::Borrowed(mapping) => mapping.dma_addr(),
            Self::Bounce { buffer, .. } => buffer.dma_addr(),
        }
    }

    fn sync_for_device(&self) {
        match self {
            Self::Borrowed(mapping) => mapping.sync_for_device(),
            Self::Bounce { buffer, .. } => buffer.sync_for_device(),
        }
    }

    fn reclaim_for_cpu(&self, successful: bool) {
        match self {
            Self::Borrowed(mapping) => mapping.sync_for_cpu(),
            Self::Bounce {
                buffer,
                original_vaddr,
                len,
                copy_back,
            } => {
                buffer.sync_for_cpu();
                if successful && *copy_back {
                    // Safety: original_vaddr/len 来自仍被同步 control/bulk/interrupt
                    // 调用持有的 `&mut [u8]`；本对象在该调用返回前完成复制。
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            buffer.vaddr() as *const u8,
                            *original_vaddr as *mut u8,
                            *len,
                        )
                    };
                }
            }
        }
    }
}

/// EHCI 主机控制器实例。
pub struct EhciHcd {
    op: usize,
    ports: usize,
    dma_context: DmaContext,
    frame_list: DmaBuffer,
    qh_pool: DmaBuffer,
    qtd_pool: DmaBuffer,
    qh_free: Spinlock<Vec<usize>>,
    qtd_free: Spinlock<Vec<usize>>,
    /// 缓存的批量/中断 QH：key = (dev_addr, ep, dir)。
    cached_qh: Spinlock<BTreeMap<(u8, u8, bool), usize>>,
    head_qh: usize,
    transfers: Spinlock<Vec<Transfer>>,
    /// 当前实现是同步 submit + poll，完整事务必须保持一一对应。
    io_lock: sched::mutex::Mutex<()>,
    faulted: AtomicBool,
}

impl EhciHcd {
    pub fn new(op_base: usize, cap_base: usize, context: DmaContext) -> Result<Self, &'static str> {
        // Safety: 能力寄存器由 platform probe 映射的 MMIO 窗口提供。
        let ports =
            unsafe { core::ptr::read_volatile((cap_base + EHCI_HCSPARAMS) as *const u32) } & 0x0f;
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
            context.clone(),
            QTD_COUNT * core::mem::size_of::<EhciQtdHw>(),
            64,
            DmaDirection::Bidirectional,
        )?;
        Self::validate_dma_region(&frame_list, 4096)?;
        Self::validate_dma_region(&qh_pool, 32)?;
        Self::validate_dma_region(&qtd_pool, 32)?;
        let mut hcd = Self {
            op: op_base,
            ports: ports as usize,
            dma_context: context,
            frame_list,
            qh_pool,
            qtd_pool,
            qh_free: Spinlock::new((0..QH_COUNT).collect()),
            qtd_free: Spinlock::new((0..QTD_COUNT).collect()),
            cached_qh: Spinlock::new(BTreeMap::new()),
            head_qh: 0,
            transfers: Spinlock::new(Vec::new()),
            io_lock: sched::mutex::Mutex::new(()),
            faulted: AtomicBool::new(false),
        };
        hcd.hc_reset()?;
        // anchor QH（head of async list）。
        hcd.head_qh = hcd.alloc_qh()?;
        let head = hcd.qh_mut(hcd.head_qh);
        head.info1 = QH_HEAD;
        head.next = qh_next(hcd.qh_dma_u32(hcd.head_qh));
        head.qtd_next = QTD_TERMINATE;
        head.qtd_alt_next = QTD_TERMINATE;
        head.token = QTD_STS_HALT;
        hcd.qh_pool.sync_for_device();
        hcd.write_op(EHCI_ASYNCLISTADDR, hcd.qh_dma(hcd.head_qh) as u32);
        // 帧列表全指向 terminate。
        let frames = hcd.frame_list.as_mut_slice();
        for slot in frames.chunks_exact_mut(4) {
            slot.copy_from_slice(&QH_NEXT_TERMINATE.to_le_bytes());
        }
        hcd.frame_list.sync_for_device();
        hcd.write_op(EHCI_PERIODICLISTBASE, hcd.frame_list.dma_addr() as u32);
        hcd.write_op(EHCI_CTRLDSSEGMENT, 0);
        hcd.write_op(EHCI_USBSTS, EHCI_STS_W1C_MASK);
        // qTD 完成由同步路径轮询；在真正接入可睡眠 hotplug worker 前保持
        // USBINTR 全部关闭，避免 IRQ 中执行枚举或遗留状态形成中断风暴。
        hcd.write_op(EHCI_USBINTR, 0);
        hcd.write_op(EHCI_USBCMD, EHCI_CMD_RUN | (8 << 16));
        hcd.write_op(EHCI_CONFIGFLAG, 1);
        hcd.start_async()?;
        let command = hcd.read_op(EHCI_USBCMD);
        let status = hcd.read_op(EHCI_USBSTS);
        log::printk!(
            "[ls2k-usb] EHCI schedule started USBCMD={:#010x} USBSTS={:#010x} run={} ase={} iaad={} lreset={} ass={} halted={}",
            command,
            status,
            command & EHCI_CMD_RUN != 0,
            command & EHCI_CMD_ASE != 0,
            command & EHCI_CMD_IAAD != 0,
            command & EHCI_CMD_LRESET != 0,
            status & EHCI_STS_ASS != 0,
            status & EHCI_STS_HCHALTED != 0,
        );
        Ok(hcd)
    }

    fn hc_reset(&self) -> Result<(), &'static str> {
        // 对照 Linux ehci_halt()：先屏蔽固件遗留中断，再停止控制器并等待
        // HCHalted。EHCI 只允许在 halted 状态置 HCRESET。
        self.write_op(EHCI_USBINTR, 0);
        let command = self.read_op(EHCI_USBCMD) & !(EHCI_CMD_RUN | EHCI_CMD_IAAD);
        self.write_op(EHCI_USBCMD, command);
        for _ in 0..HALT_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBSTS) & EHCI_STS_HCHALTED != 0 {
                break;
            }
            delay_ns(1_000);
        }
        if self.read_op(EHCI_USBSTS) & EHCI_STS_HCHALTED == 0 {
            return Err("EHCI host controller halt timeout");
        }

        self.write_op(EHCI_USBCMD, self.read_op(EHCI_USBCMD) | EHCI_CMD_HCRESET);
        for _ in 0..CONTROLLER_RESET_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBCMD) & EHCI_CMD_HCRESET == 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err("EHCI host controller reset timeout")
    }

    fn validate_dma_region(buffer: &DmaBuffer, align: usize) -> Result<(), &'static str> {
        let start = buffer.dma_addr();
        let end = start
            .checked_add(buffer.len().saturating_sub(1))
            .ok_or("EHCI DMA region address overflow")?;
        if start & (align - 1) != 0 {
            return Err("EHCI DMA region is misaligned");
        }
        if end > u32::MAX as usize {
            return Err("EHCI DMA region exceeds 32-bit address space");
        }
        Ok(())
    }

    fn validate_qtd_dma_region(dma_addr: usize, len: usize) -> Result<(), &'static str> {
        if len > 0x7fff {
            return Err("EHCI qTD payload exceeds token length field");
        }
        let page_offset = dma_addr & (EHCI_PAGE_SIZE - 1);
        let capacity = QTD_BUFFER_COUNT * EHCI_PAGE_SIZE - page_offset;
        if len > capacity {
            return Err("EHCI qTD payload exceeds five buffer pages");
        }
        let end = dma_addr
            .checked_add(len.saturating_sub(1))
            .ok_or("EHCI qTD DMA address overflow")?;
        if end > u32::MAX as usize {
            return Err("EHCI qTD payload exceeds 32-bit address space");
        }
        Ok(())
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

    fn qh_dma_u32(&self, index: usize) -> u32 {
        self.qh_dma(index) as u32
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

    fn qtd_dma_u32(&self, index: usize) -> u32 {
        self.qtd_dma(index) as u32
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

    fn halt_controller(&self) -> Result<(), &'static str> {
        self.write_op(EHCI_USBINTR, 0);
        let command = self.read_op(EHCI_USBCMD)
            & !(EHCI_CMD_RUN | EHCI_CMD_ASE | EHCI_CMD_PSE | EHCI_CMD_IAAD);
        self.write_op(EHCI_USBCMD, command);
        for _ in 0..HALT_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBSTS) & EHCI_STS_HCHALTED != 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err("EHCI host controller halt timeout")
    }

    /// 返回 `ControllerHalted` 时控制器已停止 DMA，调用者可以安全回收描述符，
    /// 但不能再次启动调度。`Err` 表示连全局停机都无法确认，任何回收都不安全。
    fn stop_async(&self) -> Result<AsyncStopState, &'static str> {
        if self.faulted.load(Ordering::Acquire) {
            return if self.read_op(EHCI_USBSTS) & EHCI_STS_HCHALTED != 0 {
                Ok(AsyncStopState::ControllerHalted)
            } else {
                Err("EHCI faulted controller is not halted")
            };
        }
        self.write_op(EHCI_USBCMD, self.read_op(EHCI_USBCMD) & !EHCI_CMD_ASE);
        for _ in 0..SCHEDULE_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBSTS) & EHCI_STS_ASS == 0 {
                return Ok(AsyncStopState::ScheduleStopped);
            }
            delay_ns(100);
        }
        self.halt_controller()?;
        self.faulted.store(true, Ordering::Release);
        Ok(AsyncStopState::ControllerHalted)
    }

    fn start_async(&self) -> Result<(), &'static str> {
        if self.faulted.load(Ordering::Acquire) {
            return Err("EHCI controller is faulted");
        }
        self.write_op(EHCI_USBCMD, self.read_op(EHCI_USBCMD) | EHCI_CMD_ASE);
        for _ in 0..SCHEDULE_TIMEOUT_LOOPS {
            if self.read_op(EHCI_USBSTS) & EHCI_STS_ASS != 0 {
                return Ok(());
            }
            delay_ns(100);
        }
        self.halt_controller()?;
        self.faulted.store(true, Ordering::Release);
        Err("EHCI async schedule start timeout")
    }

    fn link_qh(&self, qh: usize) {
        let head = self.qh_mut(self.head_qh);
        let first = head.next;
        head.next = qh_next(self.qh_dma_u32(qh));
        self.qh_mut(qh).next = first;
        compiler_fence(Ordering::SeqCst);
    }

    fn unlink_qh(&self, qh: usize) -> bool {
        let target = qh_next(self.qh_dma_u32(qh));
        let mut current = self.head_qh;
        let mut unlinked = false;
        for _ in 0..QH_COUNT {
            let next = self.qh(current).next;
            if next & QH_NEXT_TERMINATE != 0 {
                break;
            }
            if next == target {
                self.qh_mut(current).next = self.qh(qh).next;
                unlinked = true;
                break;
            }
            if next & QH_NEXT_TYPE_MASK != QH_NEXT_TYPE_QH {
                break;
            }
            let Some(offset) =
                (qh_next_pointer(next) as usize).checked_sub(self.qh_pool.dma_addr())
            else {
                break;
            };
            if offset % core::mem::size_of::<EhciQhHw>() != 0 {
                break;
            }
            let index = offset / core::mem::size_of::<EhciQhHw>();
            if index >= QH_COUNT {
                break;
            }
            current = index;
        }
        compiler_fence(Ordering::SeqCst);
        unlinked
    }

    // ── 传输 ──

    fn build_qtd(
        &self,
        dma_addr: Option<usize>,
        len: usize,
        pid: u32,
        toggle: bool,
        ioc: bool,
    ) -> Result<usize, &'static str> {
        if len != 0 {
            Self::validate_qtd_dma_region(
                dma_addr.ok_or("EHCI qTD payload has no DMA mapping")?,
                len,
            )?;
        }
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
        if let Some(dma_addr) = dma_addr.filter(|_| len != 0) {
            let page_base = dma_addr & !(EHCI_PAGE_SIZE - 1);
            let page_count = ((dma_addr & (EHCI_PAGE_SIZE - 1)) + len).div_ceil(EHCI_PAGE_SIZE);
            let mut buffers = [0u32; QTD_BUFFER_COUNT];
            buffers[0] = dma_addr as u32;
            for (slot, entry) in buffers.iter_mut().enumerate().take(page_count).skip(1) {
                *entry = (page_base + slot * EHCI_PAGE_SIZE) as u32;
            }
            qtd.buf0 = buffers[0];
            qtd.buf1 = buffers[1];
            qtd.buf2 = buffers[2];
            qtd.buf3 = buffers[3];
            qtd.buf4 = buffers[4];
        }
        Ok(index)
    }

    fn map_transfer_buffer(
        &self,
        vaddr: usize,
        len: usize,
        direction: DmaDirection,
    ) -> Result<TransferDma, &'static str> {
        if len == 0 {
            return Err("EHCI cannot map an empty payload");
        }
        if let Some(mapping) = self.dma_context.map_borrowed(vaddr, len, direction) {
            if Self::validate_qtd_dma_region(mapping.dma_addr(), len).is_ok() {
                return Ok(TransferDma::Borrowed(mapping));
            }
        }

        let mut buffer =
            DmaBuffer::new_in(self.dma_context.clone(), len, EHCI_PAGE_SIZE, direction)?;
        Self::validate_qtd_dma_region(buffer.dma_addr(), len)?;
        if direction != DmaDirection::FromDevice {
            // Safety: vaddr/len 来自本次同步传输持有的有效输入切片；buffer 独占且
            // 至少有 len 字节容量，两段内存不重叠。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    vaddr as *const u8,
                    buffer.as_mut_slice().as_mut_ptr(),
                    len,
                )
            };
        }
        Ok(TransferDma::Bounce {
            buffer,
            original_vaddr: vaddr,
            len,
            copy_back: direction != DmaDirection::ToDevice,
        })
    }

    fn build_mapped_qtd(
        &self,
        vaddr: usize,
        len: usize,
        direction: DmaDirection,
        pid: u32,
        toggle: bool,
        ioc: bool,
        payload_dma: &mut Vec<TransferDma>,
    ) -> Result<usize, &'static str> {
        let mapping = self.map_transfer_buffer(vaddr, len, direction)?;
        let index = self.build_qtd(Some(mapping.dma_addr()), len, pid, toggle, ioc)?;
        payload_dma.push(mapping);
        Ok(index)
    }

    fn remaining(&self, qtd: usize) -> usize {
        ((self.qtd(qtd).token >> QTD_LENGTH_SHIFT) & 0x7fff) as usize
    }

    fn reclaim_transfer(&self, transfer: &Transfer, successful: bool) {
        for mapping in &transfer.payload_dma {
            mapping.reclaim_for_cpu(successful);
        }
        for qtd in &transfer.qtds {
            self.free_qtd(*qtd);
        }
        if transfer.transient_qh {
            self.free_qh(transfer.qh);
        }
    }

    fn submit_linked(
        &self,
        dev_addr: u8,
        ep: u8,
        data_in: bool,
        kind: TransferKind,
        qtds: Vec<usize>,
        payload_dma: Vec<TransferDma>,
        expected: usize,
        data_qtd: Option<usize>,
    ) -> Result<(), &'static str> {
        if qtds.is_empty() {
            return Err("empty qtd chain");
        }
        let transient_qh = kind == TransferKind::Control;
        let qh = if transient_qh {
            self.alloc_qh()
        } else {
            let mut cache = self.cached_qh.lock();
            let key = (dev_addr, ep, data_in);
            if let Some(&index) = cache.get(&key) {
                Ok(index)
            } else {
                self.alloc_qh().map(|index| {
                    cache.insert(key, index);
                    index
                })
            }
        };
        let qh = match qh {
            Ok(qh) => qh,
            Err(error) => {
                for qtd in qtds {
                    self.free_qtd(qtd);
                }
                return Err(error);
            }
        };
        let max_packet = if kind == TransferKind::Control {
            64
        } else {
            512
        };
        let qh_hw = self.qh_mut(qh);
        let saved_toggle = if transient_qh {
            0
        } else {
            qh_hw.token & QTD_TOGGLE
        };
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
        qh_hw.qtd_next = self.qtd_dma_u32(qtds[0]);
        qh_hw.qtd_alt_next = QTD_TERMINATE;
        // Linux qh_update() 只发布 qTD next/alt-next，并保留非控制端点的
        // data toggle。active token 和 buffer 属于硬件 overlay，不能从首个
        // qTD 复制，否则控制器会把 QH 自身误判为正在执行的 qTD。
        qh_hw.token = saved_toggle;
        let mut previous = qtds[0];
        for &qtd in &qtds[1..] {
            self.qtd_mut(previous).next = self.qtd_dma_u32(qtd);
            previous = qtd;
        }
        // 完成中断放在最后一个 qTD。
        let last = *qtds.last().ok_or("empty qtd chain")?;
        self.qtd_mut(last).token |= QTD_IOC;

        // 停表 → 链接 → 同步 → 发布事务 → 启表。停表失败时控制器已被
        // 隔离或函数直接返回，不能让硬件继续看到即将释放的描述符。
        let stop_state = match self.stop_async() {
            Ok(state) => state,
            Err(error) => {
                for qtd in qtds {
                    self.free_qtd(qtd);
                }
                return Err(error);
            }
        };
        if stop_state == AsyncStopState::ControllerHalted {
            for qtd in qtds {
                self.free_qtd(qtd);
            }
            return Err("EHCI controller is halted");
        }
        self.link_qh(qh);
        for mapping in &payload_dma {
            mapping.sync_for_device();
        }
        self.qtd_pool.sync_for_device();
        self.qh_pool.sync_for_device();
        self.transfers.lock().push(Transfer {
            qh,
            qtds,
            payload_dma,
            expected,
            data_qtd,
            transient_qh,
        });
        if let Err(error) = self.start_async() {
            // start_async() 失败时会先把控制器置为 faulted 并确认 halted；此时
            // 可以安全地撤销刚发布的事务，但不能重新打开调度。
            let transfer = self.transfers.lock().remove(0);
            self.qh_pool.sync_for_cpu();
            if !self.unlink_qh(transfer.qh) {
                panic!("EHCI transaction QH disappeared after schedule start failure");
            }
            self.qh_pool.sync_for_device();
            self.reclaim_transfer(&transfer, false);
            return Err(error);
        }
        Ok(())
    }

    /// 轮询等待最早的未完成传输，完成后归还 QH/qTD 并返回数据字节数。
    fn poll_completion(&self) -> Result<usize, &'static str> {
        let deadline = hal::time::monotonic_ns().saturating_add(2_000_000_000);
        loop {
            // 非 coherent 平台上，控制器写回的 active/halt/remaining 字段必须
            // 先归还给 CPU，再据此判断完成状态。
            self.qtd_pool.sync_for_cpu();
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
                let stop_state = match self.stop_async() {
                    Ok(state) => state,
                    Err(error) => panic!("EHCI cannot quiesce before transfer reclaim: {error}"),
                };
                let transfer = self.transfers.lock().remove(0);
                self.qh_pool.sync_for_cpu();
                if !self.unlink_qh(transfer.qh) {
                    panic!("EHCI completed transfer QH is not linked");
                }
                self.qh_pool.sync_for_device();
                let restart_error = if stop_state == AsyncStopState::ScheduleStopped {
                    self.start_async().err()
                } else {
                    Some("EHCI controller is halted")
                };
                let successful = result.is_ok();
                let transferred = if successful {
                    match transfer.data_qtd {
                        Some(data_qtd) => {
                            transfer.expected.saturating_sub(self.remaining(data_qtd))
                        }
                        None => {
                            let last = *transfer.qtds.last().expect("non-empty qtd chain");
                            transfer.expected.saturating_sub(self.remaining(last))
                        }
                    }
                } else {
                    0
                };
                self.reclaim_transfer(&transfer, successful);
                if let Some(error) = restart_error {
                    return Err(error);
                }
                if let Err(error) = result {
                    return Err(error);
                }
                return Ok(transferred.min(transfer.expected));
            }
            if hal::time::monotonic_ns() >= deadline {
                // 超时后必须先让 async schedule 停止引用本次 QH，才能撤销
                // borrowed mapping；否则调用方返回会把硬件留在悬空地址上。
                let stop_state = match self.stop_async() {
                    Ok(state) => state,
                    Err(error) => panic!("EHCI cannot quiesce after transfer timeout: {error}"),
                };
                let transfer = self.transfers.lock().remove(0);
                self.qh_pool.sync_for_cpu();
                if !self.unlink_qh(transfer.qh) {
                    panic!("EHCI timed-out transfer QH is not linked");
                }
                self.qh_pool.sync_for_device();
                if stop_state == AsyncStopState::ScheduleStopped {
                    if let Err(error) = self.start_async() {
                        self.reclaim_transfer(&transfer, false);
                        return Err(error);
                    }
                }
                self.reclaim_transfer(&transfer, false);
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

    fn shutdown(&self) -> Result<(), &'static str> {
        let _guard = self.io_lock.lock();
        self.halt_controller()?;
        self.faulted.store(true, Ordering::Release);
        self.write_op(EHCI_USBSTS, EHCI_STS_W1C_MASK);
        Ok(())
    }

    fn port_power_on(&self, port: usize) -> Result<(), &'static str> {
        let reg = self.portsc(port);
        self.write_op(
            reg,
            (self.read_op(reg) & !EHCI_PORTSC_CHANGE_MASK) | EHCI_PORTSC_PP | EHCI_PORTSC_WKCNNT_E,
        );
        Ok(())
    }

    fn port_connected(&self, port: usize) -> bool {
        let status = self.read_op(self.portsc(port));
        status & EHCI_PORTSC_CCS != 0 && status & EHCI_PORTSC_PORT_OWNER == 0
    }

    fn port_reset(&self, port: usize) -> Result<u8, &'static str> {
        let reg = self.portsc(port);
        let mut status = self.read_op(reg);
        if status & EHCI_PORTSC_CCS == 0 {
            return Err("EHCI port has no connected device");
        }

        // EHCI 1.0/USB 2.0：主机置 PR 并清 PED，保持根端口复位至少 50 ms；
        // PR 不会由控制器自动按时清除，必须由软件终止复位。
        status &= !(EHCI_PORTSC_CHANGE_MASK | EHCI_PORTSC_PED);
        self.write_op(reg, status | EHCI_PORTSC_PR);
        delay_ns(PORT_RESET_ASSERT_NS);
        status = self.read_op(reg) & !(EHCI_PORTSC_CHANGE_MASK | EHCI_PORTSC_PR);
        self.write_op(reg, status);

        for _ in 0..PORT_RESET_CLEAR_TIMEOUT_LOOPS {
            if self.read_op(reg) & EHCI_PORTSC_PR == 0 {
                break;
            }
            delay_ns(1_000);
        }
        status = self.read_op(reg);
        if status & EHCI_PORTSC_PR != 0 {
            return Err("EHCI port reset timeout");
        }
        if status & EHCI_PORTSC_CCS == 0 {
            return Err("EHCI device disconnected during reset");
        }

        // 非 TDI EHCI 的 bits 27:26 不是可依赖的速度编码。复位后 PED 置位
        // 才表示高速握手成功；否则把端口交给 companion OHCI。
        if status & EHCI_PORTSC_PED != 0 {
            return Ok(USB_SPEED_HIGH);
        }
        self.write_op(
            reg,
            (status & !EHCI_PORTSC_CHANGE_MASK) | EHCI_PORTSC_PORT_OWNER,
        );
        Ok(USB_SPEED_FULL)
    }

    fn control_transfer(
        &self,
        dev_addr: u8,
        setup: &UsbSetup,
        data: &mut [u8],
        data_in: bool,
    ) -> Result<usize, &'static str> {
        let _guard = self.io_lock.lock();
        if self.faulted.load(Ordering::Acquire) {
            return Err("EHCI controller is faulted");
        }
        let setup_bytes = unsafe {
            core::slice::from_raw_parts(
                (setup as *const UsbSetup).cast::<u8>(),
                core::mem::size_of::<UsbSetup>(),
            )
        };
        let mut qtds = Vec::new();
        let mut payload_dma = Vec::new();
        // SETUP（toggle DATA0）。
        qtds.push(self.build_mapped_qtd(
            setup_bytes.as_ptr() as usize,
            8,
            DmaDirection::ToDevice,
            QTD_PID_SETUP,
            false,
            false,
            &mut payload_dma,
        )?);
        // DATA（toggle DATA1）。
        let data_pid = if data_in { QTD_PID_IN } else { QTD_PID_OUT };
        let data_qtd = if data.is_empty() {
            None
        } else {
            let qtd = self.build_mapped_qtd(
                data.as_mut_ptr() as usize,
                data.len(),
                if data_in {
                    DmaDirection::FromDevice
                } else {
                    DmaDirection::ToDevice
                },
                data_pid,
                true,
                false,
                &mut payload_dma,
            )?;
            qtds.push(qtd);
            Some(qtd)
        };
        // STATUS（无数据请求固定为 IN；有数据时与 DATA 相反，强制 DATA1）。
        let status_pid = if data.is_empty() {
            QTD_PID_IN
        } else if data_pid == QTD_PID_IN {
            QTD_PID_OUT
        } else {
            QTD_PID_IN
        };
        qtds.push(self.build_qtd(None, 0, status_pid, true, true)?);
        let expected = data.len();
        self.submit_linked(
            dev_addr,
            0,
            data_in,
            TransferKind::Control,
            qtds,
            payload_dma,
            expected,
            data_qtd,
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
        let _guard = self.io_lock.lock();
        if self.faulted.load(Ordering::Acquire) {
            return Err("EHCI controller is faulted");
        }
        let pid = if data_in { QTD_PID_IN } else { QTD_PID_OUT };
        let mut qtds = Vec::new();
        let mut payload_dma = Vec::new();
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(16384);
            qtds.push(self.build_mapped_qtd(
                data.as_mut_ptr().wrapping_add(offset) as usize,
                chunk,
                if data_in {
                    DmaDirection::FromDevice
                } else {
                    DmaDirection::ToDevice
                },
                pid,
                false,
                false,
                &mut payload_dma,
            )?);
            offset += chunk;
        }
        if qtds.is_empty() {
            qtds.push(self.build_qtd(None, 0, pid, false, true)?);
        }
        let expected = data.len();
        self.submit_linked(
            dev_addr,
            ep & 0x0f,
            data_in,
            TransferKind::Bulk,
            qtds,
            payload_dma,
            expected,
            None,
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
        let _guard = self.io_lock.lock();
        if self.faulted.load(Ordering::Acquire) {
            return Err("EHCI controller is faulted");
        }
        let pid = if data_in { QTD_PID_IN } else { QTD_PID_OUT };
        let mut qtds = Vec::new();
        let mut payload_dma = Vec::new();
        if data.is_empty() {
            qtds.push(self.build_qtd(None, 0, pid, false, true)?);
        } else {
            qtds.push(self.build_mapped_qtd(
                data.as_mut_ptr() as usize,
                data.len(),
                if data_in {
                    DmaDirection::FromDevice
                } else {
                    DmaDirection::ToDevice
                },
                pid,
                false,
                true,
                &mut payload_dma,
            )?);
        }
        let expected = data.len();
        self.submit_linked(
            dev_addr,
            ep & 0x0f,
            data_in,
            TransferKind::Interrupt,
            qtds,
            payload_dma,
            expected,
            None,
        )?;
        let transferred = self.poll_completion()?;
        Ok(transferred.min(data.len()))
    }
}
