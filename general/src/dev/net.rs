//! 网络设备的 PnP function 投影与常驻 queue IRQ 边界。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

use spin::mutex::Mutex;

use net::device::{QueueIrqControl, QueueIrqError, QueueIrqStats, QueueWakeHandle};

use crate::dev::dma::DmaContext;
use crate::dev::function::{DeviceClassId, DeviceFunction};
use crate::dev::irq::{IrqHandler, IrqLine, IrqStatus};

pub const NET_CLASS: DeviceClassId = DeviceClassId::new("net");

pub struct NetFunction {
    dev_name: Box<str>,
    dma_context: DmaContext,
    gone: AtomicBool,
}

impl NetFunction {
    pub fn new(dev_name: &str, dma_context: DmaContext) -> Self {
        Self {
            dev_name: dev_name.into(),
            dma_context,
            gone: AtomicBool::new(false),
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

    fn operation_contract(&self) -> Option<&str> {
        Some("mygo.device.net@1;1=dma_constraints:32")
    }

    fn invoke(
        &self,
        opcode: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, crate::dev::function::DeviceFunctionInvokeError> {
        use crate::dev::function::DeviceFunctionInvokeError as InvokeError;

        if self.is_gone() {
            return Err(InvokeError::Gone);
        }
        if opcode != 1 {
            return Err(InvokeError::Unsupported);
        }
        if !input.is_empty() || output.len() < 32 {
            return Err(InvokeError::Invalid);
        }
        let constraints = self.dma_context.constraints();
        let flags =
            u64::from(constraints.coherent) | (u64::from(constraints.supports_scatter_gather) << 1);
        output[0..8].copy_from_slice(&(constraints.address_mask as u64).to_le_bytes());
        output[8..16].copy_from_slice(&(constraints.max_segment_size as u64).to_le_bytes());
        output[16..24].copy_from_slice(&(constraints.max_segments as u64).to_le_bytes());
        output[24..32].copy_from_slice(&flags.to_le_bytes());
        Ok(32)
    }

    fn dma_context(&self) -> Option<DmaContext> {
        (!self.is_gone()).then_some(self.dma_context)
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    fn mark_gone(&self) {
        self.gone.store(true, Ordering::Release);
    }

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
pub fn net_function(dev_name: &str, dma_context: DmaContext) -> Arc<dyn DeviceFunction> {
    Arc::new(NetFunction::new(dev_name, dma_context))
}

const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

#[derive(Clone, Copy)]
enum RingInterruptControl {
    Flags {
        rx: usize,
        tx: usize,
    },
    EventIdx {
        rx_event: usize,
        rx_used_idx: usize,
        tx_event: usize,
        tx_used_idx: usize,
    },
}

#[derive(Clone, Copy)]
enum QueueIrqSource {
    Mmio { status: usize, acknowledge: usize },
    Pci { isr: usize },
    PciMsix,
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
            Self::PciMsix => 1,
        }
    }
}

struct ResidentQueueIrq {
    source: QueueIrqSource,
    rings: RingInterruptControl,
    pending: AtomicBool,
    masked: AtomicBool,
    waker: Mutex<Option<Arc<dyn QueueWakeHandle>>>,
    irq_total: AtomicU64,
    irq_mask: AtomicU64,
    irq_unmask: AtomicU64,
}

impl ResidentQueueIrq {
    fn new(source: QueueIrqSource, rings: RingInterruptControl) -> Self {
        Self {
            source,
            rings,
            pending: AtomicBool::new(false),
            masked: AtomicBool::new(true),
            waker: Mutex::new(None),
            irq_total: AtomicU64::new(0),
            irq_mask: AtomicU64::new(0),
            irq_unmask: AtomicU64::new(0),
        }
    }

    fn set_ring_interrupts_masked(&self, masked: bool) {
        match self.rings {
            RingInterruptControl::Flags { rx, tx } => {
                let flags = if masked {
                    VIRTQ_AVAIL_F_NO_INTERRUPT
                } else {
                    0
                };
                // Safety: 地址指向常驻 split-ring avail.flags，teardown 先注销 IRQ。
                unsafe {
                    write_volatile(rx as *mut u16, flags);
                    write_volatile(tx as *mut u16, flags);
                }
            }
            RingInterruptControl::EventIdx {
                rx_event,
                rx_used_idx,
                tx_event,
                tx_used_idx,
            } => {
                // Safety: 地址来自经过布局校验的 split ring。arm 发生在 worker 已确认
                // queue 为空之后，因此当前 used.idx 就是下一个期望事件的基线。
                unsafe {
                    let rx = read_volatile(rx_used_idx as *const u16);
                    let tx = read_volatile(tx_used_idx as *const u16);
                    write_volatile(
                        rx_event as *mut u16,
                        if masked { rx.wrapping_sub(1) } else { rx },
                    );
                    write_volatile(
                        tx_event as *mut u16,
                        if masked { tx.wrapping_sub(1) } else { tx },
                    );
                }
            }
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
                RingInterruptControl::Flags {
                    rx: rx_avail_flags,
                    tx: tx_avail_flags,
                },
            )),
        }
    }

    fn virtio_pci(rx_avail_flags: usize, tx_avail_flags: usize, isr_status: usize) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::Pci { isr: isr_status },
                RingInterruptControl::Flags {
                    rx: rx_avail_flags,
                    tx: tx_avail_flags,
                },
            )),
        }
    }

    fn virtio_mmio_event_idx(
        rx_event: usize,
        rx_used_idx: usize,
        tx_event: usize,
        tx_used_idx: usize,
        interrupt_status: usize,
        interrupt_acknowledge: usize,
    ) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::Mmio {
                    status: interrupt_status,
                    acknowledge: interrupt_acknowledge,
                },
                RingInterruptControl::EventIdx {
                    rx_event,
                    rx_used_idx,
                    tx_event,
                    tx_used_idx,
                },
            )),
        }
    }

    fn virtio_pci_event_idx(
        rx_event: usize,
        rx_used_idx: usize,
        tx_event: usize,
        tx_used_idx: usize,
        isr_status: usize,
    ) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::Pci { isr: isr_status },
                RingInterruptControl::EventIdx {
                    rx_event,
                    rx_used_idx,
                    tx_event,
                    tx_used_idx,
                },
            )),
        }
    }

    fn virtio_pci_msix(rx_avail_flags: usize, tx_avail_flags: usize) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::PciMsix,
                RingInterruptControl::Flags {
                    rx: rx_avail_flags,
                    tx: tx_avail_flags,
                },
            )),
        }
    }

    fn virtio_pci_msix_event_idx(
        rx_event: usize,
        rx_used_idx: usize,
        tx_event: usize,
        tx_used_idx: usize,
    ) -> Self {
        Self {
            inner: Arc::new(ResidentQueueIrq::new(
                QueueIrqSource::PciMsix,
                RingInterruptControl::EventIdx {
                    rx_event,
                    rx_used_idx,
                    tx_event,
                    tx_used_idx,
                },
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
    name = "general.dev.net.virtio_mmio_queue_irq_event_idx",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
        | kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn virtio_mmio_queue_irq_event_idx(
    rx_event: usize,
    rx_used_idx: usize,
    tx_event: usize,
    tx_used_idx: usize,
    interrupt_status: usize,
    interrupt_acknowledge: usize,
) -> NetQueueIrqBinding {
    NetQueueIrqBinding::virtio_mmio_event_idx(
        rx_event,
        rx_used_idx,
        tx_event,
        tx_used_idx,
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
    name = "general.dev.net.virtio_pci_queue_irq_event_idx",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
        | kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn virtio_pci_queue_irq_event_idx(
    rx_event: usize,
    rx_used_idx: usize,
    tx_event: usize,
    tx_used_idx: usize,
    isr_status: usize,
) -> NetQueueIrqBinding {
    NetQueueIrqBinding::virtio_pci_event_idx(
        rx_event,
        rx_used_idx,
        tx_event,
        tx_used_idx,
        isr_status,
    )
}

#[kernel_symbols::export(
    name = "general.dev.net.virtio_pci_msix_queue_irq",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
        | kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn virtio_pci_msix_queue_irq(
    rx_avail_flags: usize,
    tx_avail_flags: usize,
) -> NetQueueIrqBinding {
    NetQueueIrqBinding::virtio_pci_msix(rx_avail_flags, tx_avail_flags)
}

#[kernel_symbols::export(
    name = "general.dev.net.virtio_pci_msix_queue_irq_event_idx",
    contract = "kernel.general.net-queue-irq@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_INTERRUPT
        | kernel_symbols::capability::DEVICE_DMA,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn virtio_pci_msix_queue_irq_event_idx(
    rx_event: usize,
    rx_used_idx: usize,
    tx_event: usize,
    tx_used_idx: usize,
) -> NetQueueIrqBinding {
    NetQueueIrqBinding::virtio_pci_msix_event_idx(rx_event, rx_used_idx, tx_event, tx_used_idx)
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
