//! modern VirtIO-PCI 网络设备 PnP 适配。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem;
use core::ptr::read_volatile;

use general::dev::dma::{DmaBuffer, DmaDirection};
use general::dev::irq::{self, IrqError, IrqHandle};
use general::dev::net::{
    NetQueueIrqBinding, net_function, queue_irq_control, queue_irq_handler,
    virtio_pci_msix_queue_irq, virtio_pci_msix_queue_irq_event_idx, virtio_pci_queue_irq,
    virtio_pci_queue_irq_event_idx,
};
use general::dev::pci::{
    PciDevice, PciInfo, PciMsiPnpResource, PciMsixSet, attach_msix_pnp_resource,
};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use virtio::{
    SplitVirtQueue, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_VERSION_1, VIRTIO_MSI_NO_VECTOR,
    VIRTIO_PCI_RESET_SPIN_LIMIT, VIRTQ_DESC_F_WRITE, VirtqDescUpdate,
    VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER, VIRTIO_STATUS_DRIVER_OK,
    VIRTIO_STATUS_FAILED, VIRTIO_STATUS_FEATURES_OK, VirtioPciCap, VirtioPciFunction,
    VirtioPciTransport, choose_split_queue_size, parse_virtio_pci_caps,
};

use super::common::{
    VirtioNetQueue, VirtioNetTransport, install_active, install_active_queues,
};

const VIRTIO_PCI_FUNCTION_NETWORK: VirtioPciFunction =
    VirtioPciFunction::new("network", 0x1000, 0x1041);
const VIRTIO_NET_F_MTU: u64 = 1 << 3;
const VIRTIO_NET_F_CSUM: u64 = 1;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
const VIRTIO_NET_F_MQ: u64 = 1 << 22;
const VIRTIO_NET_F_RSS: u64 = 1 << 60;
const REQUIRED_FEATURES: u64 = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_MRG_RXBUF;
const OPTIONAL_FEATURES: u64 =
    VIRTIO_NET_F_CSUM | VIRTIO_NET_F_MTU | VIRTIO_NET_F_STATUS | VIRTIO_F_RING_EVENT_IDX;

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

fn read_u32(capability: VirtioPciCap, offset: usize) -> Option<u32> {
    let address = capability.checked_addr(offset, mem::size_of::<u32>())?;
    // Safety: `checked_addr` 已验证 capability 边界。
    Some(unsafe { read_volatile(address as *const u32) })
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
    msix_vector: Option<u16>,
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
    if let Some(vector) = msix_vector {
        transport
            .set_selected_queue_msix_vector(vector)
            .map_err(|_| "VirtIO-net PCI queue MSI-X vector 被拒绝")?;
    }
    let notify = transport
        .selected_queue_notify_addr()
        .map_err(|_| "VirtIO-net PCI notify capability 无效")?;
    transport.enable_selected_queue();
    Ok((queue, notify))
}

const VIRTIO_NET_CTRL_MQ: u8 = 4;
const VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET: u8 = 0;
const VIRTIO_NET_CTRL_MQ_RSS_CONFIG: u8 = 1;
const VIRTIO_NET_OK: u8 = 0;
const RSS_KEY_SIZE: usize = 40;
const RSS_TABLE_SIZE: usize = 128;
const RSS_HASH_TYPES: u32 = 0x3f;

fn run_control_command(
    transport: VirtioPciTransport,
    notify: usize,
    queue_index: u16,
    queue: &mut SplitVirtQueue,
    context: general::dev::dma::DmaContext,
    class: u8,
    command: u8,
    payload: &[u8],
) -> Result<(), &'static str> {
    let total = payload
        .len()
        .checked_add(3)
        .ok_or("VirtIO-net control command 过大")?;
    let mut buffer = DmaBuffer::new_in(context, total, 16, DmaDirection::Bidirectional)
        .map_err(|_| "VirtIO-net control DMA 分配失败")?;
    let bytes = buffer.as_mut_slice();
    bytes[0] = class;
    bytes[1] = command;
    bytes[2..2 + payload.len()].copy_from_slice(payload);
    bytes[total - 1] = 0xff;
    buffer.sync_for_device();

    let chain = queue
        .alloc_chain(3)
        .map_err(|_| "VirtIO-net control descriptor 不足")?;
    let descriptors = chain.as_slice();
    let base = buffer.dma_addr() as u64;
    let updates = [
        VirtqDescUpdate::new(descriptors[0], base, 2, 0, Some(descriptors[1])),
        VirtqDescUpdate::new(
            descriptors[1],
            base + 2,
            payload.len() as u32,
            0,
            Some(descriptors[2]),
        ),
        VirtqDescUpdate::new(
            descriptors[2],
            base + (total - 1) as u64,
            1,
            VIRTQ_DESC_F_WRITE,
            None,
        ),
    ];
    if queue.write_descs(&updates).is_err() || queue.push_avail(chain.head()).is_err() {
        let _ = queue.free_chain(chain);
        return Err("VirtIO-net control queue 提交失败");
    }
    transport.notify_queue(notify, queue_index);
    let mut completed = None;
    for _ in 0..VIRTIO_PCI_RESET_SPIN_LIMIT {
        match queue.pop_used() {
            Ok(Some(used)) => {
                completed = Some(used);
                break;
            }
            Ok(None) => core::hint::spin_loop(),
            Err(_) => return Err("VirtIO-net control used ring 损坏"),
        }
    }
    let used = completed.ok_or("VirtIO-net control command 超时")?;
    if used.head != chain.head() || queue.free_chain_from_head(used.head).is_err() {
        return Err("VirtIO-net control completion 不匹配");
    }
    buffer.sync_for_cpu();
    (buffer.as_slice()[total - 1] == VIRTIO_NET_OK)
        .then_some(())
        .ok_or("VirtIO-net control command 被设备拒绝")
}

fn rss_payload(pair_count: u16, key: &[u8; RSS_KEY_SIZE]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + 2 + 2 + RSS_TABLE_SIZE * 2 + 2 + 1 + RSS_KEY_SIZE);
    payload.extend_from_slice(&RSS_HASH_TYPES.to_le_bytes());
    payload.extend_from_slice(&((RSS_TABLE_SIZE - 1) as u16).to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    for index in 0..RSS_TABLE_SIZE {
        payload.extend_from_slice(&(index as u16 % pair_count).to_le_bytes());
    }
    payload.extend_from_slice(&pair_count.to_le_bytes());
    payload.push(RSS_KEY_SIZE as u8);
    payload.extend_from_slice(key);
    payload
}

struct MultiQueueProbe {
    queues: Vec<(VirtioNetQueue, NetQueueIrqBinding)>,
    control: SplitVirtQueue,
    mac: [u8; 6],
    mtu: u32,
    transport: VirtioPciTransport,
}

fn build_multi_queue_candidate(
    pci: &PciDevice,
    msix: &PciMsixSet,
    pair_count: u16,
) -> Result<MultiQueueProbe, &'static str> {
    let capabilities = parse_virtio_pci_caps(pci).ok_or("VirtIO-net PCI capability 缺失")?;
    let transport = VirtioPciTransport::new(capabilities)
        .map_err(|_| "VirtIO-net PCI capability 无效")?;
    transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
    transport.add_status(VIRTIO_STATUS_DRIVER);
    let offered = transport.device_features();
    let multi_features = VIRTIO_NET_F_CTRL_VQ | VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS;
    let accepted = REQUIRED_FEATURES | multi_features | (offered & OPTIONAL_FEATURES);
    transport.set_driver_features(accepted);
    transport.add_status(VIRTIO_STATUS_FEATURES_OK);
    if transport.status() & VIRTIO_STATUS_FEATURES_OK == 0 {
        return Err("VirtIO-net PCI MQ FEATURES_OK 被设备拒绝");
    }
    transport
        .set_config_msix_vector(pair_count)
        .map_err(|_| "VirtIO-net PCI config MSI-X vector 被拒绝")?;
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
    let event_idx = accepted & VIRTIO_F_RING_EVENT_IDX != 0;
    let tx_checksum = accepted & VIRTIO_NET_F_CSUM != 0;
    let mut queues = Vec::with_capacity(pair_count as usize);
    for id in 0..pair_count {
        let rx_index = id * 2;
        let tx_index = rx_index + 1;
        let (rx, rx_notify) = setup_queue(transport, context, rx_index, Some(id))?;
        let (tx, tx_notify) = setup_queue(transport, context, tx_index, Some(id))?;
        let irq = if event_idx {
            virtio_pci_msix_queue_irq_event_idx(
                rx.used_event_addr()
                    .map_err(|_| "VirtIO-net PCI MQ RX EVENT_IDX 布局无效")?,
                rx.used_idx_addr(),
                tx.used_event_addr()
                    .map_err(|_| "VirtIO-net PCI MQ TX EVENT_IDX 布局无效")?,
                tx.used_idx_addr(),
            )
        } else {
            virtio_pci_msix_queue_irq(rx.avail_flags_addr(), tx.avail_flags_addr())
        };
        queues.push((
            VirtioNetQueue::new(
                net::QueuePairId(id),
                VirtioNetTransport::Pci {
                    transport,
                    rx_queue: rx_index,
                    tx_queue: tx_index,
                    rx_notify,
                    tx_notify,
                },
                rx,
                tx,
                event_idx,
                tx_checksum,
            ),
            irq,
        ));
    }
    let control_index = pair_count * 2;
    let (mut control, control_notify) =
        setup_queue(transport, context, control_index, Some(pair_count))?;
    transport.add_status(VIRTIO_STATUS_DRIVER_OK);
    run_control_command(
        transport,
        control_notify,
        control_index,
        &mut control,
        context,
        VIRTIO_NET_CTRL_MQ,
        VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET,
        &pair_count.to_le_bytes(),
    )?;
    let boot = net::device::boot_config().ok_or("VirtIO-net boot config 未安装")?;
    let rss = rss_payload(pair_count, boot.rss_key());
    run_control_command(
        transport,
        control_notify,
        control_index,
        &mut control,
        context,
        VIRTIO_NET_CTRL_MQ,
        VIRTIO_NET_CTRL_MQ_RSS_CONFIG,
        &rss,
    )?;
    debug_assert_eq!(msix.len(), pair_count as usize + 1);
    Ok(MultiQueueProbe {
        queues,
        control,
        mac,
        mtu,
        transport,
    })
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
    let (rx, rx_notify) = setup_queue(transport, context, 0, None)?;
    let (tx, tx_notify) = setup_queue(transport, context, 1, None)?;
    let event_idx = accepted & VIRTIO_F_RING_EVENT_IDX != 0;
    let tx_checksum = accepted & VIRTIO_NET_F_CSUM != 0;
    let irq = if event_idx {
        virtio_pci_queue_irq_event_idx(
            rx.used_event_addr()
                .map_err(|_| "VirtIO-net PCI RX EVENT_IDX 布局无效")?,
            rx.used_idx_addr(),
            tx.used_event_addr()
                .map_err(|_| "VirtIO-net PCI TX EVENT_IDX 布局无效")?,
            tx.used_idx_addr(),
            capabilities.isr.vaddr,
        )
    } else {
        virtio_pci_queue_irq(
            rx.avail_flags_addr(),
            tx.avail_flags_addr(),
            capabilities.isr.vaddr,
        )
    };
    let _ = queue_irq_control(&irq).ack_and_mask();
    Ok((
        VirtioNetQueue::new(
            net::QueuePairId(0),
            VirtioNetTransport::Pci {
                transport,
                rx_queue: 0,
                tx_queue: 1,
                rx_notify,
                tx_notify,
            },
            rx,
            tx,
            event_idx,
            tx_checksum,
        ),
        irq,
        mac,
        mtu,
        transport,
    ))
}

struct MultiQueueReady {
    queues: Vec<(VirtioNetQueue, Arc<dyn net::device::QueueIrqControl>)>,
    control: SplitVirtQueue,
    mac: [u8; 6],
    mtu: u32,
}

fn unregister_irq_handles(handles: &mut Vec<IrqHandle>) {
    for handle in handles.drain(..) {
        let _ = irq::unregister_irq_handler(handle);
    }
}

fn try_probe_multi_queue(
    dev: &Arc<PnpDevice>,
    pci: &PciDevice,
) -> Result<Option<MultiQueueReady>, PnpError> {
    pci.try_enable_mmio()
        .map_err(|_| PnpError::hardware_failure("virtio-net PCI MMIO enable"))?;
    pci.try_enable_bus_master()
        .map_err(|_| PnpError::hardware_failure("virtio-net PCI bus master enable"))?;
    let capabilities = parse_virtio_pci_caps(pci).ok_or(PnpError::InvalidState)?;
    let transport = VirtioPciTransport::new(capabilities).map_err(|_| PnpError::InvalidState)?;
    if !transport.reset_wait(VIRTIO_PCI_RESET_SPIN_LIMIT) {
        return Err(PnpError::hardware_failure("virtio-net PCI reset timeout"));
    }
    let offered = transport.device_features();
    let required = REQUIRED_FEATURES | VIRTIO_NET_F_CTRL_VQ | VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS;
    if offered & required != required {
        return Ok(None);
    }
    let device = capabilities.device.ok_or(PnpError::InvalidState)?;
    let max_pairs = read_u16(device, 8).unwrap_or(1);
    let max_key = read_u8(device, 17).unwrap_or(0) as usize;
    let max_table = read_u16(device, 18).unwrap_or(0) as usize;
    let supported_hash = read_u32(device, 20).unwrap_or(0);
    let desired = u16::from(
        net::device::boot_config()
            .map(|boot| boot.active_cpu_count())
            .unwrap_or(1),
    )
    .min(max_pairs);
    if desired < 2
        || max_key < RSS_KEY_SIZE
        || max_table < RSS_TABLE_SIZE
        || supported_hash & RSS_HASH_TYPES != RSS_HASH_TYPES
    {
        return Ok(None);
    }
    let msix = match pci.try_configure_msix(desired + 1) {
        Ok(msix) => msix,
        Err(_) => return Ok(None),
    };
    let probe = match build_multi_queue_candidate(pci, &msix, desired) {
        Ok(probe) => probe,
        Err(error) => {
            log::warning!("[virtio-net] PCI MQ/RSS 回退单队列: {}", error);
            pci.release_configured_msix(msix);
            return Ok(None);
        }
    };
    let mut irq_handles = Vec::with_capacity(desired as usize + 1);
    for (index, (_, binding)) in probe.queues.iter().enumerate() {
        let Some(line) = msix.line(index) else {
            unregister_irq_handles(&mut irq_handles);
            probe.transport.set_status(0);
            pci.release_configured_msix(msix);
            return Ok(None);
        };
        match irq::register_irq_handler(line, queue_irq_handler(binding)) {
            Ok(handle) => irq_handles.push(handle),
            Err(_) => {
                unregister_irq_handles(&mut irq_handles);
                probe.transport.set_status(0);
                pci.release_configured_msix(msix);
                return Ok(None);
            }
        }
    }
    let Some(config_line) = msix.line(desired as usize) else {
        unregister_irq_handles(&mut irq_handles);
        probe.transport.set_status(0);
        pci.release_configured_msix(msix);
        return Ok(None);
    };
    match irq::register_irq_handler(config_line, queue_irq_handler(&probe.queues[0].1)) {
        Ok(handle) => irq_handles.push(handle),
        Err(_) => {
            unregister_irq_handles(&mut irq_handles);
            probe.transport.set_status(0);
            pci.release_configured_msix(msix);
            return Ok(None);
        }
    }
    if pci.try_enable_configured_msix(&msix).is_err() {
        unregister_irq_handles(&mut irq_handles);
        probe.transport.set_status(0);
        pci.release_configured_msix(msix);
        return Ok(None);
    }
    pci.disable_interrupts();
    if let Err(error) = attach_msix_pnp_resource(
        dev,
        pci.clone(),
        msix,
        "virtio-net-pci-mq-msix",
    ) {
        unregister_irq_handles(&mut irq_handles);
        probe.transport.set_status(0);
        return Err(error);
    }
    for (index, handle) in irq_handles.drain(..).enumerate() {
        if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
            handle,
            if index == desired as usize {
                "virtio-net-pci-config-irq"
            } else {
                "virtio-net-pci-queue-irq"
            },
        )) {
            let _ = irq::unregister_irq_handler(handle);
            probe.transport.set_status(0);
            return Err(error);
        }
    }
    let queues = probe
        .queues
        .into_iter()
        .map(|(queue, binding)| (queue, queue_irq_control(&binding)))
        .collect();
    Ok(Some(MultiQueueReady {
        queues,
        control: probe.control,
        mac: probe.mac,
        mtu: probe.mtu,
    }))
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
        if let Some(multi) = try_probe_multi_queue(dev, &pci)? {
            let pair_count = multi.queues.len();
            if let Err(kind) = install_active_queues(
                multi.queues,
                Some(multi.control),
                pci.dma_context(),
                multi.mac,
                multi.mtu,
            ) {
                log::error!("[virtio-net] 注册 PCI MQ/RSS 网络设备失败: {:?}", kind);
                return Err(PnpError::registration_failed(
                    PnpResourceKind::Function,
                    "virtio-net MQ/RSS host registration",
                ));
            }
            if let Err(error) = dev.register_function(net_function("eth0", pci.dma_context())) {
                super::common::remove_active_from_pnp();
                super::common::destroy_active();
                return Err(error);
            }
            log::printk!(
                "[virtio-net] PCI attached eth0 MQ/RSS pairs={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} mtu={}",
                pair_count,
                multi.mac[0],
                multi.mac[1],
                multi.mac[2],
                multi.mac[3],
                multi.mac[4],
                multi.mac[5],
                multi.mtu
            );
            return Ok(());
        }
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
        if let Err(error) = dev.register_function(net_function("eth0", pci.dma_context())) {
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
