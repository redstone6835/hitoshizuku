//! modern VirtIO-PCI 网络设备 PnP 适配。

use alloc::sync::Arc;
use core::mem;
use core::ptr::read_volatile;

use general::dev::irq::{self, IrqError};
use general::dev::net::{
    NetQueueIrqBinding, net_function, queue_irq_control, queue_irq_handler,
    virtio_pci_queue_irq,
};
use general::dev::pci::{
    PciDevice, PciInfo, PciMsiPnpResource, attach_msix_pnp_resource,
};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use virtio::{
    SplitVirtQueue, VIRTIO_F_VERSION_1, VIRTIO_MSI_NO_VECTOR, VIRTIO_PCI_RESET_SPIN_LIMIT,
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK,
    VIRTIO_STATUS_FAILED, VIRTIO_STATUS_FEATURES_OK, VirtioPciCap, VirtioPciFunction,
    VirtioPciTransport, choose_split_queue_size, parse_virtio_pci_caps,
};

use super::common::{VirtioNetQueue, VirtioNetTransport, install_active};

const VIRTIO_PCI_FUNCTION_NETWORK: VirtioPciFunction =
    VirtioPciFunction::new("network", 0x1000, 0x1041);
const VIRTIO_NET_F_MTU: u64 = 1 << 3;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const REQUIRED_FEATURES: u64 = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_MRG_RXBUF;
const OPTIONAL_FEATURES: u64 = VIRTIO_NET_F_MTU | VIRTIO_NET_F_STATUS;

fn read_u8(capability: VirtioPciCap, offset: usize) -> Option<u8> {
    let address = capability.checked_addr(offset, mem::size_of::<u8>())?;
    // Safety: `checked_addr` 已验证 capability 边界。
    Some(unsafe { read_volatile(address as *const u8) })
}

fn read_u16(capability: VirtioPciCap, offset: usize) -> Option<u16> {
    let address = capability.checked_addr(offset, mem::size_of::<u16>())?;
    // Safety: `checked_addr` 已验证 capability 边界。
    Some(unsafe { read_volatile(address as *const u16) })
}

fn read_mac(capability: VirtioPciCap) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        *byte = read_u8(capability, index)?;
    }
    Some(mac)
}

fn setup_queue(
    transport: VirtioPciTransport,
    context: general::dev::dma::DmaContext,
    index: u16,
) -> Result<(SplitVirtQueue, usize), &'static str> {
    transport.select_queue(index);
    let maximum = transport.selected_queue_size();
    if maximum == 0 {
        return Err("VirtIO-net PCI queue 不存在");
    }
    let size = choose_split_queue_size(maximum, Some(256))
        .map_err(|_| "VirtIO-net PCI queue size 不受支持")?;
    if size < 16 {
        return Err("VirtIO-net PCI queue 过小");
    }
    transport.set_selected_queue_size(size);
    let queue = SplitVirtQueue::new_in(context, size)
        .map_err(|_| "VirtIO-net PCI queue DMA 分配失败")?;
    transport.set_selected_queue_addresses(
        queue.desc_dma_addr() as u64,
        queue.avail_dma_addr() as u64,
        queue.used_dma_addr() as u64,
    );
    let notify = transport
        .selected_queue_notify_addr()
        .map_err(|_| "VirtIO-net PCI notify capability 无效")?;
    transport.enable_selected_queue();
    Ok((queue, notify))
}

fn probe_queue(
    pci: &PciDevice,
) -> Result<
    (
        VirtioNetQueue,
        NetQueueIrqBinding,
        [u8; 6],
        u32,
        VirtioPciTransport,
    ),
    &'static str,
> {
    pci.try_enable_mmio()
        .map_err(|_| "VirtIO-net PCI 无法启用 MMIO decode")?;
    pci.try_enable_bus_master()
        .map_err(|_| "VirtIO-net PCI 无法启用 bus master")?;
    let capabilities = parse_virtio_pci_caps(pci).ok_or("VirtIO-net PCI capability 缺失")?;
    let transport = VirtioPciTransport::new(capabilities)
        .map_err(|_| "VirtIO-net PCI capability 无效")?;
    if !transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT) {
        return Err("VirtIO-net PCI reset 超时");
    }
    transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
    transport.add_status(VIRTIO_STATUS_DRIVER);
    let offered = transport.device_features();
    if offered & REQUIRED_FEATURES != REQUIRED_FEATURES {
        transport.set_status(transport.status() | VIRTIO_STATUS_FAILED);
        return Err("VirtIO-net 缺少 VERSION_1、MAC 或 MRG_RXBUF feature");
    }
    let accepted = REQUIRED_FEATURES | (offered & OPTIONAL_FEATURES);
    transport.set_driver_features(accepted);
    transport.add_status(VIRTIO_STATUS_FEATURES_OK);
    if transport.status() & VIRTIO_STATUS_FEATURES_OK == 0 {
        transport.set_status(transport.status() | VIRTIO_STATUS_FAILED);
        return Err("VirtIO-net PCI FEATURES_OK 被设备拒绝");
    }
    let device = capabilities
        .device
        .ok_or("VirtIO-net PCI device config 缺失")?;
    let mac = read_mac(device).ok_or("VirtIO-net PCI MAC config 截断")?;
    let mtu = if accepted & VIRTIO_NET_F_MTU != 0 {
        u32::from(read_u16(device, 10).ok_or("VirtIO-net PCI MTU config 截断")?).max(576)
    } else {
        1500
    };
    let context = pci.dma_context();
    let (rx, rx_notify) = setup_queue(transport, context, 0)?;
    let (tx, tx_notify) = setup_queue(transport, context, 1)?;
    let irq = virtio_pci_queue_irq(
        rx.avail_flags_addr(),
        tx.avail_flags_addr(),
        capabilities.isr.vaddr,
    );
    let _ = queue_irq_control(&irq).ack_and_mask();
    Ok((
        VirtioNetQueue::new(
            VirtioNetTransport::Pci {
                transport,
                rx_notify,
                tx_notify,
            },
            rx,
            tx,
        ),
        irq,
        mac,
        mtu,
        transport,
    ))
}

fn map_irq_error(error: IrqError) -> &'static str {
    match error {
        IrqError::OutOfMemory => "out of memory",
        IrqError::NotFound => "not found",
        IrqError::AlreadyRegistered => "already registered",
    }
}

fn clear_queue_msix_vectors(transport: VirtioPciTransport) {
    transport.select_queue(0);
    let _ = transport.set_selected_queue_msix_vector(VIRTIO_MSI_NO_VECTOR);
    transport.select_queue(1);
    let _ = transport.set_selected_queue_msix_vector(VIRTIO_MSI_NO_VECTOR);
}

fn register_irq(
    dev: &Arc<PnpDevice>,
    pci: &PciDevice,
    binding: &NetQueueIrqBinding,
    transport: VirtioPciTransport,
) -> Result<(), PnpError> {
    let handler = queue_irq_handler(binding);
    if let Ok(msix) = pci.try_configure_msix(1) {
        let Some(line) = msix.line(0) else {
            pci.release_configured_msix(msix);
            return Err(PnpError::InvalidState);
        };
        transport.select_queue(0);
        let rx_vector = transport.set_selected_queue_msix_vector(0);
        transport.select_queue(1);
        let tx_vector = transport.set_selected_queue_msix_vector(0);
        if rx_vector.is_ok() && tx_vector.is_ok() {
            match irq::register_irq_handler(line, Arc::clone(&handler)) {
                Ok(irq_handle) if pci.try_enable_configured_msix(&msix).is_ok() => {
                    pci.disable_interrupts();
                    if let Err(error) = attach_msix_pnp_resource(
                        dev,
                        pci.clone(),
                        msix,
                        "virtio-net-pci-msix",
                    ) {
                        let _ = irq::unregister_irq_handler(irq_handle);
                        return Err(error);
                    }
                    if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
                        irq_handle,
                        "virtio-net-pci-msix-irq",
                    )) {
                        let _ = irq::unregister_irq_handler(irq_handle);
                        return Err(error);
                    }
                    return Ok(());
                }
                Ok(irq_handle) => {
                    let _ = irq::unregister_irq_handler(irq_handle);
                }
                Err(error) => {
                    log::warning!(
                        "[virtio-net] PCI MSI-X IRQ {:?} 注册失败: {}",
                        line,
                        map_irq_error(error)
                    );
                }
            }
        } else {
            log::warning!("[virtio-net] PCI MSI-X queue vector 被设备拒绝");
        }
        clear_queue_msix_vectors(transport);
        pci.release_configured_msix(msix);
    }
    if let Ok(msi) = pci.try_configure_single_msi() {
        let line = msi.line();
        match irq::register_irq_handler(line, Arc::clone(&handler)) {
            Ok(irq_handle) if pci.try_enable_configured_msi(msi).is_ok() => {
                pci.disable_interrupts();
                if let Err(error) = dev.own_boxed_resource(PciMsiPnpResource::boxed(
                    pci.clone(),
                    msi,
                    "virtio-net-pci-msi",
                )) {
                    let _ = irq::unregister_irq_handler(irq_handle);
                    pci.release_configured_msi(msi);
                    return Err(error);
                }
                if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
                    irq_handle,
                    "virtio-net-pci-msi-irq",
                )) {
                    let _ = irq::unregister_irq_handler(irq_handle);
                    return Err(error);
                }
                return Ok(());
            }
            Ok(irq_handle) => {
                let _ = irq::unregister_irq_handler(irq_handle);
                pci.release_configured_msi(msi);
            }
            Err(error) => {
                log::warning!(
                    "[virtio-net] PCI MSI IRQ {:?} 注册失败: {}",
                    line,
                    map_irq_error(error)
                );
                pci.release_configured_msi(msi);
            }
        }
    }
    let line = pci.routed_irq_line().ok_or_else(|| {
        PnpError::missing(PnpResourceKind::Irq, "virtio-net PCI routed irq missing")
    })?;
    let handle = irq::register_irq_handler(line, handler).map_err(|error| {
        log::error!("[virtio-net] PCI INTx 注册失败: {}", map_irq_error(error));
        PnpError::registration_failed(PnpResourceKind::Irq, "virtio-net PCI INTx")
    })?;
    pci.enable_interrupts();
    if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
        handle,
        "virtio-net-pci-intx",
    )) {
        let _ = irq::unregister_irq_handler(handle);
        pci.disable_interrupts();
        return Err(error);
    }
    Ok(())
}

pub(crate) struct VirtioPciNetDriver;

impl PnpDriver for VirtioPciNetDriver {
    fn name(&self) -> &'static str {
        "virtio-pci-net"
    }

    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        let PnpId::Pci { .. } = id else {
            return false;
        };
        info.as_any()
            .downcast_ref::<PciInfo>()
            .is_some_and(|info| {
                VIRTIO_PCI_FUNCTION_NETWORK.matches_pci_ids(info.vendor, info.device_id)
            })
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;
        let (queue, irq_binding, mac, mtu, transport) = probe_queue(&pci).map_err(|error| {
            log::error!("[virtio-net] PCI probe 失败: {}", error);
            PnpError::hardware_failure("virtio-net PCI init failed")
        })?;
        register_irq(dev, &pci, &irq_binding, transport).map_err(|error| {
            log::error!("[virtio-net] PCI IRQ 初始化失败: {:?}", error);
            error
        })?;
        transport.add_status(VIRTIO_STATUS_DRIVER_OK);
        if let Err(kind) = install_active(
            queue,
            pci.dma_context(),
            queue_irq_control(&irq_binding),
            mac,
            mtu,
        )
        {
            log::error!("[virtio-net] 注册网络设备失败: {:?}", kind);
            return Err(PnpError::registration_failed(
                PnpResourceKind::Function,
                "virtio-net host registration",
            ));
        }
        if let Err(error) = dev.register_function(net_function("eth0")) {
            super::common::remove_active_from_pnp();
            super::common::destroy_active();
            return Err(error);
        }
        log::printk!(
            "[virtio-net] PCI attached eth0 mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} mtu={}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], mtu
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        super::common::remove_active_from_pnp();
        if let Some(pci) = PciDevice::from_pnp(dev) {
            pci.disable_interrupts();
        }
        log::printk!("[virtio-net] PCI removed {}", dev.name);
    }
}

struct VirtioPciNetFactory;

impl DriverFactory for VirtioPciNetFactory {
    fn name(&self) -> &'static str {
        "virtio-pci-net"
    }

    fn create(&self, _context: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioPciNetDriver))
    }
}

pub(crate) fn register_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(VirtioPciNetFactory))
}
