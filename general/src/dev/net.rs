//! 网络设备的 PnP function 投影与常驻 queue IRQ 边界。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

use spin::mutex::Mutex;

use net::device::{QueueIrqControl, QueueIrqError, QueueIrqStats, QueueWakeHandle};

use crate::dev::function::{DeviceClassId, DeviceFunction};
use crate::dev::irq::{IrqHandler, IrqLine, IrqStatus};

pub const NET_CLASS: DeviceClassId = DeviceClassId::new("net");

pub struct NetFunction {
    dev_name: Box<str>,
}

impl NetFunction {
    pub fn new(dev_name: &str) -> Self {
        Self {
            dev_name: dev_name.into(),
        }
    }
}

impl DeviceFunction for NetFunction {
    fn class_id(&self) -> DeviceClassId {
        NET_CLASS
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 在常驻内核中构造网络 function trait object，避免动态 ELM 的 vtable 被长期保存。
#[kernel_symbols::export(
    name = "general.dev.net.net_function",
    contract = "kernel.general.device-function@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn net_function(dev_name: &str) -> Arc<dyn DeviceFunction> {
    Arc::new(NetFunction::new(dev_name))
}

const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

#[derive(Clone, Copy)]
enum QueueIrqSource {
    Mmio { status: usize, acknowledge: usize },
    Pci { isr: usize },
}

impl QueueIrqSource {
    fn acknowledge(self) -> u32 {
        match self {
            Self::Mmio {
                status,
                acknowledge,
            } => {
                // Safety: 地址由已完成 probe 的 VirtIO transport 提供，并在 IRQ 资源
                // 注销前保持映射。
                let value = unsafe { read_volatile(status as *const u32) };
                if value != 0 {
                    // Safety: 同上；VirtIO MMIO 使用写回已观察位完成确认。
                    unsafe { write_volatile(acknowledge as *mut u32, value) };
                }
                value
            }
            Self::Pci { isr } => {
                // Safety: 地址来自已校验的 VirtIO PCI ISR capability；读取即确认。
                unsafe { read_volatile(isr as *const u8) as u32 }
            }
        }
    }
}

struct ResidentQueueIrq {
    source: QueueIrqSource,
    rx_avail_flags: usize,
    tx_avail_flags: usize,
    pending: AtomicBool,
    masked: AtomicBool,
    waker: Mutex<Option<Arc<dyn QueueWakeHandle>>>,
    irq_total: AtomicU64,
    irq_mask: AtomicU64,
    irq_unmask: AtomicU64,
}

impl ResidentQueueIrq {
    fn new(source: QueueIrqSource, rx_avail_flags: usize, tx_avail_flags: usize) -> Self {
        Self {
            source,
            rx_avail_flags,
            tx_avail_flags,
            pending: AtomicBool::new(false),
            masked: AtomicBool::new(true),
            waker: Mutex::new(None),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        }
    }

    fn set_ring_interrupts_masked(&self, masked: bool) {
        let flags = if masked {
            VIRTQ_AVAIL_F_NO_INTERRUPT
        } else {
            0
        };
        // Safety: 两个地址指向常驻 DMA allocation 中的 split-ring avail.flags，
        // queue teardown 必须先注销 IRQ 并释放 host control 引用。
        unsafe {
            write_volatile(self.rx_avail_flags as *mut u16, flags);
            write_volatile(self.tx_avail_flags as *mut u16, flags);
        }
        fence(Ordering::SeqCst);
    }

    fn wake(&self) {
        if let Some(waker) = self.waker.lock().as_ref() {
            waker.wake();
        }
    }
}

impl QueueIrqControl for ResidentQueueIrq {
    fn ack_and_mask(&self) -> bool {
        self.set_ring_interrupts_masked(true);
        self.masked.store(true, Ordering::Release);
        self.irq_mask.fetch_add(1, Ordering::Relaxed);
        let observed = self.source.acknowledge() & 1 != 0;
        self.pending.swap(false, Ordering::AcqRel) || observed
    }

    fn unmask(&self) {
        self.pending.store(false, Ordering::Release);
        self.set_ring_interrupts_masked(false);
        self.masked.store(false, Ordering::Release);
        self.irq_unmask.fetch_add(1, Ordering::Relaxed);
        if self.pending.load(Ordering::Acquire) {
            self.wake();
        }
    }

    fn set_waker(&self, waker: Arc<dyn QueueWakeHandle>) -> Result<(), QueueIrqError> {
        let mut slot = self.waker.lock();
        if slot.is_some() {
            return Err(QueueIrqError::WakerAlreadyInstalled);
        }
        *slot = Some(waker);
        Ok(())
    }

    fn clear_waker(&self) {
        *self.waker.lock() = None;
    }

    fn stats(&self) -> QueueIrqStats {
        QueueIrqStats {
            irq_total: self.irq_total.load(Ordering::Relaxed),
            irq_mask: self.irq_mask.load(Ordering::Relaxed),
            irq_unmask: self.irq_unmask.load(Ordering::Relaxed),
        }
    }
}

impl IrqHandler for ResidentQueueIrq {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        let status = self.source.acknowledge();
        if status == 0 {
            return IrqStatus::Unhandled;
        }
        self.irq_total.fetch_add(1, Ordering::Relaxed);
        if status & 1 != 0 {
            self.set_ring_interrupts_masked(true);
            self.masked.store(true, Ordering::Release);
            self.irq_mask.fetch_add(1, Ordering::Relaxed);
            if !self.pending.swap(true, Ordering::AcqRel) {
                self.wake();
            }
        }
        IrqStatus::Handled
    }
}

/// 常驻内核拥有的 VirtIO queue IRQ 对象。control 与 handler 共享同一状态，
/// 因而硬 IRQ 不需要进入可卸载的 driver ELM。
pub struct NetQueueIrqBinding {
    inner: Arc<ResidentQueueIrq>,
}

impl NetQueueIrqBinding {
    fn virtio_mmio(
        rx_avail_flags: usize,
        tx_avail_flags: usize,
        interrupt_status: usize,
        interrupt_acknowledge: usize,
    ) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::Mmio {
                    status: interrupt_status,
                    acknowledge: interrupt_acknowledge,
                },
                rx_avail_flags,
                tx_avail_flags,
            )),
        }
    }

    fn virtio_pci(rx_avail_flags: usize, tx_avail_flags: usize, isr_status: usize) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::Pci { isr: isr_status },
                rx_avail_flags,
                tx_avail_flags,
            )),
        }
    }

    fn control(&self) -> Arc<dyn QueueIrqControl> {
        self.inner.clone()
    }

    fn handler(&self) -> Arc<dyn IrqHandler> {
        self.inner.clone()
    }
}

#[kernel_symbols::export(
    name = "general.dev.net.virtio_mmio_queue_irq",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
        | kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn virtio_mmio_queue_irq(
    rx_avail_flags: usize,
    tx_avail_flags: usize,
    interrupt_status: usize,
    interrupt_acknowledge: usize,
) -> NetQueueIrqBinding {
    NetQueueIrqBinding::virtio_mmio(
        rx_avail_flags,
        tx_avail_flags,
        interrupt_status,
        interrupt_acknowledge,
    )
}

#[kernel_symbols::export(
    name = "general.dev.net.virtio_pci_queue_irq",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
        | kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn virtio_pci_queue_irq(
    rx_avail_flags: usize,
    tx_avail_flags: usize,
    isr_status: usize,
) -> NetQueueIrqBinding {
    NetQueueIrqBinding::virtio_pci(rx_avail_flags, tx_avail_flags, isr_status)
}

#[kernel_symbols::export(
    name = "general.dev.net.queue_irq_control",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn queue_irq_control(binding: &NetQueueIrqBinding) -> Arc<dyn QueueIrqControl> {
    binding.control()
}

#[kernel_symbols::export(
    name = "general.dev.net.queue_irq_handler",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn queue_irq_handler(binding: &NetQueueIrqBinding) -> Arc<dyn IrqHandler> {
    binding.handler()
}
