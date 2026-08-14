//! LS2K1000 APB-DMA 选择器和单通道提供方驱动。
//!
//! 通道资源的 `Configure` 参数依次是方向、内存地址低/高 32 位、APB 地址和
//! 字节数。方向 0 表示设备到内存，1 表示内存到设备；`Enable` 提交传输，
//! `Disable` 停止传输，`ReadValue` 返回原始 order 寄存器。

use alloc::sync::Arc;
use core::mem::size_of;
use core::ptr::{read_volatile, write, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering, fence};

use spin::Mutex;

use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection as BufferDirection};
use crate::dev::dt_provider::{
    self, DtbProvider, DtbProviderError, DtbProviderKey, DtbProviderKind, DtbResource,
    DtbResourceReply, DtbResourceRequest,
};
use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use crate::layout::{Ls2xDmaMmioLayout, apply_selector, start_order, stop_order};
use crate::protocol::{DmaDirection, DmaTransfer, Ls2xDmaDescriptor};

const COMPAT_SELECTOR: &str = "loongson,ls-apbdma";
const CHANNEL_COMPATIBLES: [&str; 5] = [
    "loongson,ls-apbdma-0",
    "loongson,ls-apbdma-1",
    "loongson,ls-apbdma-2",
    "loongson,ls-apbdma-3",
    "loongson,ls-apbdma-4",
];
const PROP_DMA_CELLS: &str = "#dma-cells";
const PROP_DMA_CHANNELS: &str = "dma-channels";
const PROP_DMA_REQUESTS: &str = "dma-requests";
const PROP_SELECTOR: &str = "apbdma-sel";
const PROP_CONFIG_COUNT: &str = "#config-nr";
const DMA_RESOURCE_KIND: PnpResourceKind = PnpResourceKind::Dma;

struct SelectorState {
    register: usize,
    claimed: Mutex<u64>,
}

impl SelectorState {
    fn update(&self, bit: u32, value: bool) -> Result<(), DtbProviderError> {
        let _claimed = self.claimed.lock();
        let current = read64(self.register);
        let updated = apply_selector(current, bit, value).ok_or(DtbProviderError::Invalid)?;
        write64(self.register, updated);
        Ok(())
    }
}

struct SelectorResource {
    state: Arc<SelectorState>,
    bit: u32,
    selected: bool,
    original: bool,
    enabled: Mutex<bool>,
}

impl SelectorResource {
    fn restore(&self) {
        let mut enabled = self.enabled.lock();
        if *enabled {
            let _ = self.state.update(self.bit, self.original);
            *enabled = false;
        }
    }
}

impl DtbResource for SelectorResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        match request {
            DtbResourceRequest::Enable => {
                let mut enabled = self.enabled.lock();
                if !*enabled {
                    self.state.update(self.bit, self.selected)?;
                    *enabled = true;
                }
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Disable => {
                self.restore();
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::ReadValue => Ok(DtbResourceReply::Value(u64::from(
                read64(self.state.register) & (1u64 << self.bit) != 0,
            ))),
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

impl Drop for SelectorResource {
    fn drop(&mut self) {
        self.restore();
        let mut claimed = self.state.claimed.lock();
        *claimed &= !(1u64 << self.bit);
    }
}

struct SelectorProvider {
    state: Arc<SelectorState>,
}

impl DtbProvider for SelectorProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        let &[bit, selected] = specifier else {
            return Err(DtbProviderError::AcquireFailed);
        };
        if bit >= 64 || selected > 1 {
            return Err(DtbProviderError::AcquireFailed);
        }
        let mask = 1u64 << bit;
        let original = {
            let mut claimed = self.state.claimed.lock();
            if *claimed & mask != 0 {
                return Err(DtbProviderError::Busy);
            }
            *claimed |= mask;
            read64(self.state.register) & mask != 0
        };
        Ok(Arc::new(SelectorResource {
            state: Arc::clone(&self.state),
            bit,
            selected: selected != 0,
            original,
            enabled: Mutex::new(false),
        }))
    }
}

struct ChannelHardware {
    order: usize,
    dma: DmaContext,
    claimed: AtomicBool,
}

struct ChannelTransferState {
    descriptor: DmaBuffer,
    configured: bool,
    started: bool,
    address_64_bit: bool,
}

struct ChannelResource {
    hardware: Arc<ChannelHardware>,
    state: Mutex<ChannelTransferState>,
}

impl ChannelResource {
    fn stop_locked(order: usize, state: &mut ChannelTransferState) {
        if state.started {
            write64(order, stop_order(read64(order), state.address_64_bit));
            fence(Ordering::SeqCst);
            state.started = false;
        }
    }
}

impl DtbResource for ChannelResource {
    fn control(
        &self,
        request: DtbResourceRequest<'_>,
    ) -> Result<DtbResourceReply, DtbProviderError> {
        let mut state = self.state.lock();
        match request {
            DtbResourceRequest::Configure(words) => {
                let &[direction, memory_low, memory_high, peripheral, bytes] = words else {
                    return Err(DtbProviderError::Invalid);
                };
                if state.started {
                    return Err(DtbProviderError::Busy);
                }
                let direction = match direction {
                    0 => DmaDirection::DeviceToMemory,
                    1 => DmaDirection::MemoryToDevice,
                    _ => return Err(DtbProviderError::Invalid),
                };
                let memory = u64::from(memory_low) | (u64::from(memory_high) << 32);
                let descriptor = Ls2xDmaDescriptor::single(DmaTransfer {
                    direction,
                    memory,
                    peripheral,
                    bytes,
                })
                .map_err(|_| DtbProviderError::Invalid)?;
                // Safety: DmaBuffer 保证至少 32 字节对齐且容量覆盖完整描述符；当前
                // 持有通道锁，没有设备在 configured 状态前读取这块内存。
                unsafe {
                    write(
                        state.descriptor.vaddr() as *mut Ls2xDmaDescriptor,
                        descriptor,
                    );
                }
                state.descriptor.sync_for_device();
                state.address_64_bit =
                    memory_high != 0 || state.descriptor.dma_addr() > u32::MAX as usize;
                state.configured = true;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Enable => {
                if !state.configured {
                    return Err(DtbProviderError::Invalid);
                }
                if state.started {
                    return Ok(DtbResourceReply::Done);
                }
                let order = start_order(state.descriptor.dma_addr() as u64, state.address_64_bit)
                    .map_err(|_| DtbProviderError::HardwareFailure)?;
                write64(self.hardware.order, 0);
                fence(Ordering::Release);
                write64(self.hardware.order, order);
                state.started = true;
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::Disable => {
                Self::stop_locked(self.hardware.order, &mut state);
                Ok(DtbResourceReply::Done)
            }
            DtbResourceRequest::ReadValue => {
                Ok(DtbResourceReply::Value(read64(self.hardware.order)))
            }
            _ => Err(DtbProviderError::UnsupportedOperation),
        }
    }
}

impl Drop for ChannelResource {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        Self::stop_locked(self.hardware.order, state);
        self.hardware.claimed.store(false, Ordering::Release);
    }
}

struct ChannelProvider {
    hardware: Arc<ChannelHardware>,
    max_request: u32,
}

impl DtbProvider for ChannelProvider {
    fn acquire(&self, specifier: &[u32]) -> Result<Arc<dyn DtbResource>, DtbProviderError> {
        let &[request] = specifier else {
            return Err(DtbProviderError::AcquireFailed);
        };
        if request > self.max_request {
            return Err(DtbProviderError::AcquireFailed);
        }
        self.hardware
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| DtbProviderError::Busy)?;
        let descriptor = match DmaBuffer::new_in(
            self.hardware.dma.clone(),
            size_of::<Ls2xDmaDescriptor>(),
            32,
            BufferDirection::Bidirectional,
        ) {
            Ok(descriptor) => descriptor,
            Err(_) => {
                self.hardware.claimed.store(false, Ordering::Release);
                return Err(DtbProviderError::OutOfMemory);
            }
        };
        Ok(Arc::new(ChannelResource {
            hardware: Arc::clone(&self.hardware),
            state: Mutex::new(ChannelTransferState {
                descriptor,
                configured: false,
                started: false,
                address_64_bit: false,
            }),
        }))
    }
}

struct LoongsonApbDmaDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl LoongsonApbDmaDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_SELECTOR)
            || CHANNEL_COMPATIBLES
                .iter()
                .any(|compatible| info.has_id(compatible))
    }

    fn probe_selector(
        &self,
        dev: &Arc<PnpDevice>,
        info: &PlatformDeviceInfo,
    ) -> Result<(), PnpError> {
        if info.u32_property(PROP_CONFIG_COUNT) != Some(2) {
            return Err(PnpError::malformed(
                DMA_RESOURCE_KIND,
                "APB-DMA selector #config-nr must be two",
            ));
        }
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            DMA_RESOURCE_KIND,
            "APB-DMA selector is missing a phandle",
        ))?;
        let (phys, size) = exact_mmio(info, "APB-DMA selector register missing")?;
        let register = Ls2xDmaMmioLayout::new((self.device_mmio_to_virt)(phys), size)
            .map_err(|_| {
                PnpError::malformed(PnpResourceKind::Mmio, "invalid APB-DMA selector window")
            })?
            .order();
        dev.reserve_owned_resources(1)?;
        let handle = dt_provider::register(
            DtbProviderKey::new(DtbProviderKind::Dma, phandle),
            Arc::new(SelectorProvider {
                state: Arc::new(SelectorState {
                    register,
                    claimed: Mutex::new(0),
                }),
            }),
        )
        .map_err(DtbProviderError::into_pnp_error)?;
        if let Err(error) = dev.own_resource(dt_provider::provider_pnp_resource(
            handle,
            "loongson-apbdma-selector-provider",
        )) {
            let _ = dt_provider::unregister(handle);
            return Err(error);
        }
        log::printk!("[loongson-apbdma] selector {} phys={:#x}", dev.name, phys);
        Ok(())
    }

    fn probe_channel(
        &self,
        dev: &Arc<PnpDevice>,
        info: &PlatformDeviceInfo,
    ) -> Result<(), PnpError> {
        validate_channel_properties(info)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            DMA_RESOURCE_KIND,
            "APB-DMA channel is missing a phandle",
        ))?;
        let selector = selector_specifier(info)?;
        let selector_lease = dt_provider::acquire(
            DtbProviderKey::new(DtbProviderKind::Dma, selector.0),
            &[selector.1, selector.2],
        )
        .map_err(DtbProviderError::into_pnp_error)?;
        selector_lease
            .control(DtbResourceRequest::Enable)
            .map_err(DtbProviderError::into_pnp_error)?;
        let (phys, size) = exact_mmio(info, "APB-DMA channel order register missing")?;
        let order = Ls2xDmaMmioLayout::new((self.device_mmio_to_virt)(phys), size)
            .map_err(|_| {
                PnpError::malformed(PnpResourceKind::Mmio, "invalid APB-DMA channel window")
            })?
            .order();
        dev.reserve_owned_resources(2)?;
        dev.own_resource(dt_provider::lease_pnp_resource(
            selector_lease,
            "loongson-apbdma-selector-lease",
        ))?;
        let hardware = Arc::new(ChannelHardware {
            order,
            dma: info.dma_context(),
            claimed: AtomicBool::new(false),
        });
        write64(order, stop_order(read64(order), true));
        let handle = dt_provider::register(
            DtbProviderKey::new(DtbProviderKind::Dma, phandle),
            Arc::new(ChannelProvider {
                hardware,
                max_request: info.u32_property(PROP_DMA_REQUESTS).unwrap_or(0),
            }),
        )
        .map_err(DtbProviderError::into_pnp_error)?;
        if let Err(error) = dev.own_resource(dt_provider::provider_pnp_resource(
            handle,
            "loongson-apbdma-channel-provider",
        )) {
            let _ = dt_provider::unregister(handle);
            return Err(error);
        }
        log::printk!(
            "[loongson-apbdma] channel {} phandle={:#x} phys={:#x}",
            dev.name,
            phandle,
            phys
        );
        Ok(())
    }
}

impl PnpDriver for LoongsonApbDmaDriver {
    fn name(&self) -> &'static str {
        "platform-loongson-apbdma"
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

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        if info.has_id(COMPAT_SELECTOR) {
            self.probe_selector(dev, info)
        } else {
            self.probe_channel(dev, info)
        }
    }

    fn remove(&self, _dev: &Arc<PnpDevice>) {}
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn exact_mmio(
    info: &PlatformDeviceInfo,
    missing: &'static str,
) -> Result<(usize, usize), PnpError> {
    let mut windows = info.mmio_resources();
    let window = windows
        .next()
        .ok_or(PnpError::missing(PnpResourceKind::Mmio, missing))?;
    if windows.next().is_some() {
        return Err(PnpError::malformed(
            PnpResourceKind::Mmio,
            "APB-DMA node requires exactly one register window",
        ));
    }
    Ls2xDmaMmioLayout::new(window.0, window.1).map_err(|_| {
        PnpError::malformed(PnpResourceKind::Mmio, "invalid APB-DMA register window")
    })?;
    Ok(window)
}

fn validate_channel_properties(info: &PlatformDeviceInfo) -> Result<(), PnpError> {
    if info.u32_property(PROP_DMA_CELLS) != Some(1)
        || info.u32_property(PROP_DMA_CHANNELS) != Some(1)
        || info
            .u32_property(PROP_DMA_REQUESTS)
            .is_none_or(|requests| requests == 0)
    {
        return Err(PnpError::malformed(
            DMA_RESOURCE_KIND,
            "APB-DMA channel properties are invalid",
        ));
    }
    Ok(())
}

fn selector_specifier(info: &PlatformDeviceInfo) -> Result<(u32, u32, u32), PnpError> {
    let mut cells = info
        .u32_list_property(PROP_SELECTOR)
        .ok_or(PnpError::missing(
            DMA_RESOURCE_KIND,
            "APB-DMA channel selector reference missing",
        ))?;
    let phandle = cells.next().ok_or(PnpError::malformed(
        DMA_RESOURCE_KIND,
        "APB-DMA selector phandle missing",
    ))?;
    let bit = cells.next().ok_or(PnpError::malformed(
        DMA_RESOURCE_KIND,
        "APB-DMA selector bit missing",
    ))?;
    let value = cells.next().ok_or(PnpError::malformed(
        DMA_RESOURCE_KIND,
        "APB-DMA selector value missing",
    ))?;
    if cells.next().is_some() || phandle == 0 || bit >= 64 || value > 1 {
        return Err(PnpError::malformed(
            DMA_RESOURCE_KIND,
            "APB-DMA selector reference is invalid",
        ));
    }
    Ok((phandle, bit, value))
}

fn read64(address: usize) -> u64 {
    // Safety: 调用方已通过 Ls2xDmaMmioLayout 校验 8 字节对齐和窗口长度，
    // 平台总线映射在驱动资源释放前保持有效。
    unsafe { read_volatile(address as *const u64) }
}

fn write64(address: usize, value: u64) {
    // Safety: 与 read64 相同，地址指向当前设备拥有的 64 位 MMIO 寄存器。
    unsafe { write_volatile(address as *mut u64, value) }
}

struct LoongsonApbDmaFactory;

impl DriverFactory for LoongsonApbDmaFactory {
    fn name(&self) -> &'static str {
        "platform-loongson-apbdma"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(LoongsonApbDmaDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(LoongsonApbDmaFactory))
}
