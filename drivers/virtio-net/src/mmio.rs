//! modern VirtIO-MMIO 网络设备 PnP 适配。

use alloc::sync::Arc;
use core::ptr::read_volatile;

use general::dev::irq::{self, IrqError, IrqHandle};
use general::dev::net::{
    NetQueueIrqBinding, net_function, queue_irq_control, queue_irq_handler,
    virtio_mmio_queue_irq,
};
use general::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use virtio::virtio_mmio::{
    VIRTIO_F_VERSION_1, VIRTIO_STATUS_ACKNOWLEDGE, VIRTIO_STATUS_DRIVER,
    VIRTIO_STATUS_DRIVER_OK, VIRTIO_STATUS_FAILED, VIRTIO_STATUS_FEATURES_OK,
    VirtioMmioTransport, detect as detect_virtio_mmio,
};
use virtio::{SplitVirtQueue, choose_split_queue_size};

use super::common::{VirtioNetQueue, VirtioNetTransport, install_active};

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_NET_DEVICE_ID: u32 = 1;
const MMIO_MAGIC: usize = 0x000;
const MMIO_VERSION: usize = 0x004;
const MMIO_DEVICE_ID: usize = 0x008;
const MMIO_INTERRUPT_STATUS: usize = 0x060;
const MMIO_INTERRUPT_ACK: usize = 0x064;
const NET_CONFIG_BASE: usize = 0x100;

const VIRTIO_NET_F_MTU: u64 = 1 << 3;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
const REQUIRED_FEATURES: u64 = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_MRG_RXBUF;
const OPTIONAL_FEATURES: u64 = VIRTIO_NET_F_MTU | VIRTIO_NET_F_STATUS;

fn read_mac(base: usize) -> [u8; 6] {
    let mut mac = [0u8; 6];
    for (index, byte) in mac.iter_mut().enumerate() {
        // Safety: VirtIO-net config 至少包含协商后的 MAC 字段。
        *byte = unsafe { read_volatile((base + NET_CONFIG_BASE + index) as *const u8) };
    }
    mac
}

fn read_mtu(base: usize, features: u64) -> u32 {
    if features & VIRTIO_NET_F_MTU == 0 {
        return 1500;
    }
    // Safety: VIRTIO_NET_F_MTU 表示 config offset 10 的 u16 字段存在。
    let mtu = unsafe { read_volatile((base + NET_CONFIG_BASE + 10) as *const u16) };
    u32::from(mtu).max(576)
}

fn setup_queue(
    transport: &dyn VirtioMmioTransport,
    context: general::dev::dma::DmaContext,
    index: u16,
) -> Result<SplitVirtQueue, &'static str> {
    transport.select_queue(index);
    let maximum = transport.read_queue_max_size();
    if maximum == 0 || maximum > u32::from(u16::MAX) {
        return Err("VirtIO-net MMIO queue size 无效");
    }
    let size = choose_split_queue_size(maximum as u16, Some(256))
        .map_err(|_| "VirtIO-net MMIO queue size 不受支持")?;
    if size < 16 {
        return Err("VirtIO-net MMIO queue 过小");
    }
    transport.write_queue_size(u32::from(size));
    let queue = SplitVirtQueue::new_in(context, size)
        .map_err(|_| "VirtIO-net MMIO queue DMA 分配失败")?;
    transport.configure_queue_addresses(
        queue.desc_dma_addr() as u64,
        queue.avail_dma_addr() as u64,
        queue.used_dma_addr() as u64,
    );
    transport.enable_queue();
    Ok(queue)
}

fn probe_queue(
    base: usize,
    context: general::dev::dma::DmaContext,
) -> Result<(VirtioNetQueue, NetQueueIrqBinding, [u8; 6], u32), &'static str> {
    let transport = detect_virtio_mmio(base)?;
    if transport.is_legacy() {
        return Err("VirtIO-net 不支持 legacy MMIO transport");
    }
    transport.write_status(0);
    transport.add_status(VIRTIO_STATUS_ACKNOWLEDGE);
    transport.add_status(VIRTIO_STATUS_DRIVER);
    let offered = transport.read_device_features();
    if offered & REQUIRED_FEATURES != REQUIRED_FEATURES {
        transport.add_status(VIRTIO_STATUS_FAILED);
        return Err("VirtIO-net 缺少 VERSION_1、MAC 或 MRG_RXBUF feature");
    }
    let accepted = REQUIRED_FEATURES | (offered & OPTIONAL_FEATURES);
    transport.write_driver_features(accepted);
    transport.add_status(VIRTIO_STATUS_FEATURES_OK);
    if transport.read_status() & VIRTIO_STATUS_FEATURES_OK == 0 {
        transport.add_status(VIRTIO_STATUS_FAILED);
        return Err("VirtIO-net MMIO FEATURES_OK 被设备拒绝");
    }
    let mac = read_mac(base);
    let mtu = read_mtu(base, accepted);
    let rx = setup_queue(transport.as_ref(), context, 0)?;
    let tx = setup_queue(transport.as_ref(), context, 1)?;
    let irq = virtio_mmio_queue_irq(
        rx.avail_flags_addr(),
        tx.avail_flags_addr(),
        base + MMIO_INTERRUPT_STATUS,
        base + MMIO_INTERRUPT_ACK,
    );
    let _ = queue_irq_control(&irq).ack_and_mask();
    transport.add_status(VIRTIO_STATUS_DRIVER_OK);
    Ok((
        VirtioNetQueue::new(VirtioNetTransport::Mmio(transport), rx, tx),
        irq,
        mac,
        mtu,
    ))
}

fn map_irq_error(error: IrqError) -> PnpError {
    match error {
        IrqError::OutOfMemory => PnpError::OutOfMemory,
        IrqError::AlreadyRegistered => PnpError::registration_failed(
            PnpResourceKind::Irq,
            "virtio-net MMIO irq already registered",
        ),
        IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "virtio-net MMIO irq not found")
        }
    }
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn register_irq(
    info: &PlatformDeviceInfo,
    binding: &NetQueueIrqBinding,
) -> Result<IrqHandle, PnpError> {
    match info.register_first_irq_handler(queue_irq_handler(binding)) {
        Ok(handle) => Ok(handle),
        Err(PlatformIrqRegistrationError::NoResource) => Err(PnpError::missing(
            PnpResourceKind::Irq,
            "virtio-net MMIO irq missing",
        )),
        Err(PlatformIrqRegistrationError::Unresolved) => {
            Err(PnpError::dependency(first_irq_dependency(info)))
        }
        Err(PlatformIrqRegistrationError::RegistrationFailed { err, .. }) => {
            Err(map_irq_error(err))
        }
    }
}

pub(crate) struct VirtioMmioNetDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl VirtioMmioNetDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_device(&self, info: &PlatformDeviceInfo) -> bool {
        if !(info.has_id("virtio,mmio") || info.has_id("LNRO0005")) {
            return false;
        }
        let Some((physical, _)) = info.first_mmio() else {
            return false;
        };
        let base = (self.device_mmio_to_virt)(physical);
        // Safety: 固件已经把该范围声明为 MMIO resource，probe 只读取标准头。
        let magic = unsafe { read_volatile((base + MMIO_MAGIC) as *const u32) };
        let version = unsafe { read_volatile((base + MMIO_VERSION) as *const u32) };
        let device = unsafe { read_volatile((base + MMIO_DEVICE_ID) as *const u32) };
        magic == VIRTIO_MMIO_MAGIC && version == 2 && device == VIRTIO_NET_DEVICE_ID
    }
}

impl PnpDriver for VirtioMmioNetDriver {
    fn name(&self) -> &'static str {
        "virtio-mmio-net"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(|info| self.matches_device(info))
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let (physical, _) = info
            .first_mmio()
            .ok_or_else(|| PnpError::missing(PnpResourceKind::Mmio, "virtio-net MMIO missing"))?;
        let base = (self.device_mmio_to_virt)(physical);
        let (queue, irq_binding, mac, mtu) = probe_queue(base, info.dma_context()).map_err(|error| {
            log::error!("[virtio-net] MMIO probe 失败: {}", error);
            PnpError::hardware_failure("virtio-net MMIO init failed")
        })?;
        let irq_handle = register_irq(info, &irq_binding)?;
        if let Err(kind) = install_active(
            queue,
            info.dma_context(),
            queue_irq_control(&irq_binding),
            mac,
            mtu,
        )
        {
            let _ = irq::unregister_irq_handler(irq_handle);
            log::error!("[virtio-net] 注册网络设备失败: {:?}", kind);
            return Err(PnpError::registration_failed(
                PnpResourceKind::Function,
                "virtio-net host registration",
            ));
        }
        if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
            irq_handle,
            "virtio-net-mmio-irq",
        )) {
            super::common::remove_active_from_pnp();
            super::common::destroy_active();
            let _ = irq::unregister_irq_handler(irq_handle);
            return Err(error);
        }
        if let Err(error) = dev.register_function(net_function("eth0")) {
            super::common::remove_active_from_pnp();
            super::common::destroy_active();
            return Err(error);
        }
        log::printk!(
            "[virtio-net] MMIO attached eth0 mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} mtu={}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], mtu
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        super::common::remove_active_from_pnp();
        log::printk!("[virtio-net] MMIO removed {}", dev.name);
    }
}

struct VirtioMmioNetFactory;

impl DriverFactory for VirtioMmioNetFactory {
    fn name(&self) -> &'static str {
        "virtio-mmio-net"
    }

    fn create(&self, context: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(VirtioMmioNetDriver::new(
            context.device_mmio_to_virt,
        )))
    }
}

pub(crate) fn register_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(VirtioMmioNetFactory))
}
