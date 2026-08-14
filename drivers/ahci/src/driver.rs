//! 平台 AHCI/SATA 控制器驱动。
//!
//! 当前实现面向 LS2K1000 固件中的 `snps,spear-ahci`，但寄存器与 ATA 命令均
//! 遵循标准 AHCI 1.x。每个活动端口使用一个 command slot 和可复用 DMA 缓冲，
//! 对上保持块层的异步 BIO 契约，并允许同步调用方通过 `drain` 主动推进完成。

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::num::NonZeroU32;
use core::ops::{Deref, DerefMut};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering, fence};

use spin::mutex::{Mutex, MutexGuard};

use crate::alloc_ahci_dev_name;
use crate::dev::bio::{Bio, BioIoError, BioOp, BioReqError, SubmitError};
use crate::dev::block::{
    BlockAttributes, BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures,
    BlockGeometry, BlockLimits,
};
use crate::dev::dma::{DmaBuffer, DmaContext, DmaDirection};
use crate::dev::function::BlockFunction;
use crate::dev::irq::{self, IrqError, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use crate::protocol::{AhciDmaLayout, AtaCommand, IdentifyInfo, encode_command};
use crate::registers::{AhciPortRegisters, AhciRegisterLayout, effective_port_map};

const COMPAT_SPEAR_AHCI: &str = "snps,spear-ahci";
const PROP_PORTS_IMPLEMENTED: &str = "ports-implemented";

const GHC_HBA_RESET: u32 = 1 << 0;
const GHC_INTERRUPT_ENABLE: u32 = 1 << 1;
const GHC_AHCI_ENABLE: u32 = 1 << 31;
const CAP2_BOH: u32 = 1;
const BOHC_BIOS_OWNED: u32 = 1;
const BOHC_OS_OWNED: u32 = 1 << 1;
const BOHC_BIOS_BUSY: u32 = 1 << 4;

const PORT_CMD_START: u32 = 1;
const PORT_CMD_SPIN_UP: u32 = 1 << 1;
const PORT_CMD_POWER_ON: u32 = 1 << 2;
const PORT_CMD_FIS_RX: u32 = 1 << 4;
const PORT_CMD_FIS_RUNNING: u32 = 1 << 14;
const PORT_CMD_COMMAND_RUNNING: u32 = 1 << 15;
const PORT_TFD_ERROR: u32 = 1;
const PORT_TFD_DRQ: u32 = 1 << 3;
const PORT_TFD_BUSY: u32 = 1 << 7;
const PORT_IRQ_ERROR_MASK: u32 = 0x7d00_0000;
const PORT_IRQ_ENABLE_MASK: u32 = 0x7dc0_00ff;
const SATA_SIGNATURE: u32 = 0x0000_0101;
const SATA_DET_PRESENT: u32 = 3;
const SATA_IPM_ACTIVE: u32 = 1;

const HBA_TIMEOUT_NS: u64 = 2_000_000_000;
const PORT_TIMEOUT_NS: u64 = 1_000_000_000;
const COMMAND_TIMEOUT_NS: u64 = 10_000_000_000;
const COMRESET_ASSERT_NS: u64 = 1_000_000;
const STAGING_BYTES: usize = 1024 * 1024;

struct LocalIrqState {
    state: usize,
}

impl LocalIrqState {
    fn acquire() -> Self {
        Self {
            state: hal::interrupt::save_and_disable_local(),
        }
    }
}

impl Drop for LocalIrqState {
    fn drop(&mut self) {
        hal::interrupt::restore_local(self.state);
    }
}

struct IrqSafeMutex<T> {
    inner: Mutex<T>,
}

impl<T> IrqSafeMutex<T> {
    const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    fn lock(&self) -> IrqSafeMutexGuard<'_, T> {
        let irq_state = LocalIrqState::acquire();
        IrqSafeMutexGuard {
            guard: self.inner.lock(),
            _irq_state: irq_state,
        }
    }
}

struct IrqSafeMutexGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    _irq_state: LocalIrqState,
}

impl<T> Deref for IrqSafeMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for IrqSafeMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

struct PortDma {
    command_list: DmaBuffer,
    received_fis: DmaBuffer,
    command_table: DmaBuffer,
    data: DmaBuffer,
    layout: AhciDmaLayout,
}

impl PortDma {
    fn new(context: DmaContext) -> Result<Self, &'static str> {
        let command_list =
            DmaBuffer::new_in(context.clone(), 1024, 1024, DmaDirection::Bidirectional)?;
        let received_fis =
            DmaBuffer::new_in(context.clone(), 256, 256, DmaDirection::Bidirectional)?;
        let command_table =
            DmaBuffer::new_in(context.clone(), 144, 128, DmaDirection::Bidirectional)?;
        let data = DmaBuffer::new_in(context, STAGING_BYTES, 4096, DmaDirection::Bidirectional)?;
        let layout = AhciDmaLayout::new(
            command_list.dma_addr(),
            received_fis.dma_addr(),
            command_table.dma_addr(),
            data.dma_addr(),
            data.len(),
        )
        .map_err(|_| "AHCI DMA layout violates controller constraints")?;
        Ok(Self {
            command_list,
            received_fis,
            command_table,
            data,
            layout,
        })
    }

    fn prepare(
        &mut self,
        command: AtaCommand,
        data_len: usize,
        write_payload: Option<&Bio>,
    ) -> Result<(), SubmitError> {
        self.command_list.sync_for_cpu();
        self.command_table.sync_for_cpu();
        self.data.sync_for_cpu();
        if let Some(bio) = write_payload
            && !bio
                .buffer
                .copy_to_contiguous(&mut self.data.as_mut_slice()[..data_len])
        {
            return Err(SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch));
        }
        encode_command(
            &mut self.command_list.as_mut_slice()[..32],
            self.command_table.as_mut_slice(),
            self.layout.command_table as usize,
            self.layout.data as usize,
            data_len,
            command,
        )
        .map_err(|_| SubmitError::InvalidRequest(BioReqError::TooLarge))?;
        if data_len != 0 {
            self.data.sync_for_device();
        }
        self.command_table.sync_for_device();
        self.command_list.sync_for_device();
        Ok(())
    }
}

struct PendingBio {
    bio: Bio,
    deadline_ns: u64,
}

struct AhciPortState {
    dma: PortDma,
    pending: Option<PendingBio>,
    error_latched: bool,
    stopped: bool,
    failed: bool,
}

struct AhciPort {
    index: u32,
    registers: AhciPortRegisters,
    identify: IdentifyInfo,
    max_blocks: u32,
    state: IrqSafeMutex<AhciPortState>,
    recovering: AtomicBool,
}

impl AhciPort {
    fn probe(
        index: u32,
        registers: AhciPortRegisters,
        dma_context: DmaContext,
    ) -> Result<Option<Arc<Self>>, &'static str> {
        write32(registers.interrupt_enable, 0);
        if !stop_port(registers, PORT_TIMEOUT_NS) {
            return Err("AHCI port engine did not stop");
        }
        let mut dma = PortDma::new(dma_context)?;
        dma.command_list.sync_for_device();
        dma.received_fis.sync_for_device();
        program_port_dma(registers, dma.layout);
        clear_port_status(registers);
        if !prepare_link(registers) {
            return Ok(None);
        }
        if !start_port(registers) {
            return Err("AHCI port engine did not start");
        }
        if read32(registers.signature) != SATA_SIGNATURE {
            if !stop_port(registers, PORT_TIMEOUT_NS) {
                core::mem::forget(dma);
                return Err("AHCI non-SATA port did not stop");
            }
            return Ok(None);
        }
        let identify = match identify_device(registers, &mut dma) {
            Ok(identify) => identify,
            Err(error) => {
                if !stop_port(registers, PORT_TIMEOUT_NS) {
                    core::mem::forget(dma);
                }
                return Err(error);
            }
        };
        let max_blocks =
            (STAGING_BYTES / identify.logical_sector_size as usize).min(u16::MAX as usize) as u32;
        if max_blocks == 0 {
            if !stop_port(registers, PORT_TIMEOUT_NS) {
                core::mem::forget(dma);
            }
            return Err("AHCI logical sector exceeds staging buffer");
        }
        Ok(Some(Arc::new(Self {
            index,
            registers,
            identify,
            max_blocks,
            state: IrqSafeMutex::new(AhciPortState {
                dma,
                pending: None,
                error_latched: false,
                stopped: false,
                failed: false,
            }),
            recovering: AtomicBool::new(false),
        })))
    }

    fn submit(&self, bio: Bio) -> Result<(), (SubmitError, Bio)> {
        self.poll_completion();
        if bio.fua {
            return Err((SubmitError::Unsupported, bio));
        }
        let data_len = match (bio.range.blocks as usize)
            .checked_mul(self.identify.logical_sector_size as usize)
        {
            Some(len) => len,
            None => return Err((SubmitError::InvalidRequest(BioReqError::TooLarge), bio)),
        };
        let command = match bio.op {
            BioOp::Read | BioOp::Write => {
                if bio.range.blocks == 0
                    || bio.range.blocks > self.max_blocks
                    || data_len != bio.buffer.len()
                {
                    return Err((
                        SubmitError::InvalidRequest(BioReqError::BufferSizeMismatch),
                        bio,
                    ));
                }
                let Some(end) = bio.range.lba.checked_add(u64::from(bio.range.blocks)) else {
                    return Err((SubmitError::InvalidRequest(BioReqError::OutOfBounds), bio));
                };
                if end > self.identify.sectors {
                    return Err((SubmitError::InvalidRequest(BioReqError::OutOfBounds), bio));
                }
                let sectors = bio.range.blocks as u16;
                if bio.op == BioOp::Read {
                    AtaCommand::ReadDmaExt {
                        lba: bio.range.lba,
                        sectors,
                    }
                } else {
                    AtaCommand::WriteDmaExt {
                        lba: bio.range.lba,
                        sectors,
                    }
                }
            }
            BioOp::Flush if self.identify.supports_flush => AtaCommand::FlushCacheExt,
            BioOp::Flush | BioOp::Discard | BioOp::WriteZeroes => {
                return Err((SubmitError::Unsupported, bio));
            }
        };

        let mut state = self.state.lock();
        if state.failed || state.stopped {
            return Err((SubmitError::DeviceGone, bio));
        }
        if state.pending.is_some() || read32(self.registers.command_issue) & 1 != 0 {
            return Err((SubmitError::QueueFull, bio));
        }
        let write_payload = (bio.op == BioOp::Write).then_some(&bio);
        if let Err(error) = state.dma.prepare(command, data_len, write_payload) {
            return Err((error, bio));
        }
        clear_port_status(self.registers);
        if read32(self.registers.task_file_data) & (PORT_TFD_BUSY | PORT_TFD_DRQ) != 0 {
            return Err((SubmitError::QueueFull, bio));
        }
        state.error_latched = false;
        state.pending = Some(PendingBio {
            bio,
            deadline_ns: hal::time::monotonic_ns().saturating_add(COMMAND_TIMEOUT_NS),
        });
        fence(Ordering::Release);
        write32(self.registers.command_issue, 1);
        Ok(())
    }

    fn poll_completion(&self) -> bool {
        let mut timed_out = false;
        let completion = {
            let mut state = self.state.lock();
            let status = read32(self.registers.interrupt_status);
            if status != 0 {
                state.error_latched |= status & PORT_IRQ_ERROR_MASK != 0;
                write32(self.registers.interrupt_status, status);
            }
            let Some(deadline_ns) = state.pending.as_ref().map(|pending| pending.deadline_ns)
            else {
                return status != 0;
            };
            if read32(self.registers.command_issue) & 1 != 0 {
                if hal::time::monotonic_ns() >= deadline_ns {
                    state.failed = true;
                    timed_out = true;
                }
                None
            } else {
                fence(Ordering::Acquire);
                state.dma.command_list.sync_for_cpu();
                state.dma.command_table.sync_for_cpu();
                let needs_data = state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.bio.op.needs_data());
                if needs_data {
                    state.dma.data.sync_for_cpu();
                }
                let tfd = read32(self.registers.task_file_data);
                let failed = state.error_latched || tfd & PORT_TFD_ERROR != 0;
                let mut pending = state.pending.take().expect("pending BIO disappeared");
                let result = if failed {
                    Err(BioIoError::MediaError)
                } else if pending.bio.op == BioOp::Read
                    && !pending.bio.buffer.copy_from_contiguous(
                        &state.dma.data.as_slice()[..pending.bio.buffer.len()],
                    )
                {
                    Err(BioIoError::Unavailable)
                } else {
                    Ok(())
                };
                state.error_latched = false;
                Some((pending.bio, result))
            }
        };
        if timed_out {
            return self.recover_timed_out_command();
        }
        if let Some((bio, result)) = completion {
            bio.complete(result);
            true
        } else {
            false
        }
    }

    fn recover_timed_out_command(&self) -> bool {
        if self
            .recovering
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return true;
        }
        let stopped = stop_port(self.registers, PORT_TIMEOUT_NS);
        let pending = if stopped {
            let mut state = self.state.lock();
            state.stopped = true;
            state.dma.command_list.sync_for_cpu();
            state.dma.command_table.sync_for_cpu();
            state.dma.received_fis.sync_for_cpu();
            state.dma.data.sync_for_cpu();
            clear_port_status(self.registers);
            state.pending.take().map(|pending| pending.bio)
        } else {
            log::error!(
                "[ahci] port {} timed out and could not stop; retaining DMA buffers",
                self.index
            );
            None
        };
        self.recovering.store(false, Ordering::Release);
        if let Some(bio) = pending {
            bio.complete(Err(BioIoError::Timeout));
        }
        true
    }

    fn enable_interrupts(&self) {
        clear_port_status(self.registers);
        write32(self.registers.interrupt_enable, PORT_IRQ_ENABLE_MASK);
    }

    fn shutdown(&self) -> Result<(), &'static str> {
        write32(self.registers.interrupt_enable, 0);
        let pending = {
            let mut state = self.state.lock();
            if state.stopped {
                return Ok(());
            }
            if !stop_port(self.registers, PORT_TIMEOUT_NS) {
                state.failed = true;
                return Err("AHCI port engine did not stop during removal");
            }
            state.stopped = true;
            state.dma.command_list.sync_for_cpu();
            state.dma.command_table.sync_for_cpu();
            state.dma.received_fis.sync_for_cpu();
            state.dma.data.sync_for_cpu();
            clear_port_status(self.registers);
            state.pending.take().map(|pending| pending.bio)
        };
        if let Some(bio) = pending {
            bio.complete(Err(BioIoError::Unavailable));
        }
        Ok(())
    }
}

struct AhciBlockIo {
    port: Arc<AhciPort>,
}

impl BlockDriver for AhciBlockIo {
    fn queue_bio(&self, bio: Bio) -> Result<(), (SubmitError, Bio)> {
        self.port.submit(bio)
    }

    fn drain(&self) {
        self.port.poll_completion();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct AhciController {
    registers: AhciRegisterLayout,
    ports: Vec<Arc<AhciPort>>,
}

impl AhciController {
    fn initialize(
        registers: AhciRegisterLayout,
        firmware_port_map: Option<u32>,
        dma_context: DmaContext,
    ) -> Result<Arc<Self>, &'static str> {
        bios_handoff(registers)?;
        let saved_cap = read32(registers.cap());
        let saved_map = effective_port_map(
            saved_cap,
            read32(registers.ports_implemented()),
            firmware_port_map,
        )
        .ok_or("AHCI has no valid implemented ports")?;
        reset_hba(registers)?;
        write32(registers.ports_implemented(), saved_map);
        log::printk!(
            "[ahci] version={:#x} cap={:#x} ports={:#x}",
            read32(registers.version()),
            saved_cap,
            saved_map
        );

        for index in 0..32 {
            if saved_map & (1 << index) != 0 && registers.port(index).is_none() {
                return Err("AHCI port register window is truncated");
            }
        }
        let mut ports = Vec::new();
        ports
            .try_reserve(saved_map.count_ones() as usize)
            .map_err(|_| "AHCI port allocation failed")?;
        for index in 0..32 {
            if saved_map & (1 << index) == 0 {
                continue;
            }
            let port_registers = registers.port(index).expect("port layout was prevalidated");
            match AhciPort::probe(index, port_registers, dma_context.clone()) {
                Ok(Some(port)) => {
                    ports.push(port);
                }
                Ok(None) => {}
                Err(error) => {
                    if ports.iter().any(|port| port.shutdown().is_err()) {
                        core::mem::forget(ports);
                    }
                    return Err(error);
                }
            }
        }
        Ok(Arc::new(Self { registers, ports }))
    }

    fn enable_interrupts(&self) {
        for port in &self.ports {
            port.enable_interrupts();
        }
        write32(self.registers.interrupt_status(), u32::MAX);
        let ghc = read32(self.registers.ghc());
        write32(
            self.registers.ghc(),
            ghc | GHC_AHCI_ENABLE | GHC_INTERRUPT_ENABLE,
        );
    }

    fn handle_interrupt(&self) -> bool {
        let pending = read32(self.registers.interrupt_status());
        if pending == 0 {
            return false;
        }
        let mut handled = false;
        for port in &self.ports {
            if pending & (1 << port.index) != 0 {
                handled |= port.poll_completion();
            }
        }
        write32(self.registers.interrupt_status(), pending);
        handled || pending != 0
    }

    fn shutdown(&self) -> Result<(), &'static str> {
        let ghc = read32(self.registers.ghc());
        write32(self.registers.ghc(), ghc & !GHC_INTERRUPT_ENABLE);
        let mut failed = false;
        for port in &self.ports {
            failed |= port.shutdown().is_err();
        }
        if failed {
            Err("one or more AHCI ports failed to stop")
        } else {
            Ok(())
        }
    }
}

struct AhciIrqHandler {
    controller: Arc<AhciController>,
}

impl IrqHandler for AhciIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.controller.handle_interrupt() {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

struct AhciBinding {
    controller: Arc<AhciController>,
    blocks: Vec<(String, Arc<BlockDevice>)>,
}

struct AhciPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl AhciPlatformDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_SPEAR_AHCI)
    }
}

impl PnpDriver for AhciPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-ahci"
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
        let mut windows = info.mmio_resources();
        let (phys, size) = windows.next().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "AHCI register window missing",
        ))?;
        if windows.next().is_some() {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "AHCI requires exactly one register window",
            ));
        }
        AhciRegisterLayout::new(phys, size).map_err(|_| {
            PnpError::malformed(PnpResourceKind::Mmio, "invalid AHCI register window")
        })?;
        let registers = AhciRegisterLayout::new((self.device_mmio_to_virt)(phys), size)
            .map_err(|_| PnpError::hardware_failure("invalid AHCI MMIO mapping"))?;
        dev.reserve_owned_resources(1)?;
        let controller = AhciController::initialize(
            registers,
            info.u32_property(PROP_PORTS_IMPLEMENTED),
            info.dma_context(),
        )
        .map_err(|error| {
            log::error!("[ahci] probe failed for {}: {}", dev.name, error);
            PnpError::hardware_failure("AHCI controller initialization failed")
        })?;

        let mut blocks = Vec::new();
        if blocks.try_reserve(controller.ports.len()).is_err() {
            if controller.shutdown().is_err() {
                core::mem::forget(controller);
            }
            return Err(PnpError::OutOfMemory);
        }
        for port in &controller.ports {
            let stable_key = format!("{}-port{}", dev.name, port.index);
            let dev_name = match alloc_ahci_dev_name(&stable_key) {
                Ok(name) => name,
                Err(error) => {
                    if controller.shutdown().is_err() {
                        core::mem::forget(controller);
                    }
                    return Err(error.into());
                }
            };
            let block = match create_block_device(&dev_name, Arc::clone(port)) {
                Ok(block) => block,
                Err(_) => {
                    if controller.shutdown().is_err() {
                        core::mem::forget(controller);
                    }
                    return Err(PnpError::registration_failed(
                        PnpResourceKind::Function,
                        "AHCI block geometry",
                    ));
                }
            };
            blocks.push((dev_name, block));
        }

        let irq_handler: Arc<dyn IrqHandler> = Arc::new(AhciIrqHandler {
            controller: Arc::clone(&controller),
        });
        let irq_handle = match info.register_first_irq_handler(irq_handler) {
            Ok(handle) => handle,
            Err(error) => {
                if controller.shutdown().is_err() {
                    core::mem::forget(controller);
                }
                return Err(map_platform_irq_error(info, error));
            }
        };
        if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
            irq_handle,
            "platform-ahci-irq",
        )) {
            let _ = irq::unregister_irq_handler(irq_handle);
            if controller.shutdown().is_err() {
                core::mem::forget(controller);
            }
            return Err(error);
        }
        controller.enable_interrupts();

        for (index, (dev_name, block)) in blocks.iter().enumerate() {
            let function =
                BlockFunction::with_projection_name_arc(&dev.name, &dev_name, Arc::clone(&block));
            if let Err(error) = dev.register_function(function) {
                if controller.shutdown().is_err() {
                    core::mem::forget(controller);
                }
                return Err(error);
            }
            let port = &controller.ports[index];
            log::printk!(
                "[ahci] port={} sectors={} logical={} physical={} -> /dev/{}",
                port.index,
                port.identify.sectors,
                port.identify.logical_sector_size,
                port.identify.physical_sector_size,
                dev_name
            );
        }
        dev.set_driver_data(Arc::new(AhciBinding { controller, blocks }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Err(error) = self.try_remove(dev) {
            log::error!("[ahci] remove failed for {}: {:?}", dev.name, error);
        }
    }

    fn try_remove(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let data = dev.take_driver_data().ok_or(PnpError::InvalidState)?;
        let binding = Arc::downcast::<AhciBinding>(data).map_err(|_| PnpError::InvalidState)?;
        for (_, block) in &binding.blocks {
            block.mark_gone();
        }
        if binding.controller.shutdown().is_err() {
            core::mem::forget(binding);
            return Err(PnpError::hardware_failure("AHCI controller did not stop"));
        }
        Ok(())
    }
}

fn create_block_device(name: &str, port: Arc<AhciPort>) -> Result<Arc<BlockDevice>, &'static str> {
    let logical = NonZeroU32::new(port.identify.logical_sector_size)
        .ok_or("AHCI logical sector size is zero")?;
    let physical = NonZeroU32::new(port.identify.physical_sector_size)
        .ok_or("AHCI physical sector size is zero")?;
    let geometry = BlockGeometry::new(logical, physical, Some(port.identify.sectors))
        .ok_or("AHCI block geometry is invalid")?;
    let max_blocks = NonZeroU32::new(port.max_blocks).ok_or("AHCI maximum BIO is zero")?;
    let limits = BlockLimits::new(Some(max_blocks), None, None)
        .ok_or("AHCI block limits are invalid")?
        .with_data_segment_limits(NonZeroU32::new(1), NonZeroU32::new(STAGING_BYTES as u32));
    let attributes =
        BlockAttributes::new(false, port.identify.rotational, NonZeroU32::new(1), None);
    let features = if port.identify.supports_flush {
        BlockFeatures::FLUSH
    } else {
        BlockFeatures::default()
    };
    let io: Arc<dyn BlockDriver> = Arc::new(AhciBlockIo {
        port: Arc::clone(&port),
    });
    Ok(Arc::new(BlockDevice::new(
        BlockDeviceInit {
            name,
            subsystem: "ahci",
            class: BlockClass::Whole,
            geometry,
            limits,
            attributes,
            features,
        },
        io,
        None,
    )))
}

fn identify_device(
    registers: AhciPortRegisters,
    dma: &mut PortDma,
) -> Result<IdentifyInfo, &'static str> {
    dma.prepare(AtaCommand::IdentifyDevice, 512, None)
        .map_err(|_| "failed to encode ATA IDENTIFY")?;
    clear_port_status(registers);
    if read32(registers.task_file_data) & (PORT_TFD_BUSY | PORT_TFD_DRQ) != 0 {
        return Err("ATA device remained busy before IDENTIFY");
    }
    fence(Ordering::Release);
    write32(registers.command_issue, 1);
    if !wait_until(COMMAND_TIMEOUT_NS, || {
        read32(registers.command_issue) & 1 == 0
    }) {
        return Err("ATA IDENTIFY timed out");
    }
    fence(Ordering::Acquire);
    dma.command_list.sync_for_cpu();
    dma.command_table.sync_for_cpu();
    dma.data.sync_for_cpu();
    let status = read32(registers.interrupt_status);
    let tfd = read32(registers.task_file_data);
    write32(registers.interrupt_status, status);
    if status & PORT_IRQ_ERROR_MASK != 0 || tfd & PORT_TFD_ERROR != 0 {
        return Err("ATA IDENTIFY reported a device error");
    }
    IdentifyInfo::parse(&dma.data.as_slice()[..512]).map_err(|_| "ATA IDENTIFY data is unsupported")
}

fn program_port_dma(registers: AhciPortRegisters, layout: AhciDmaLayout) {
    write32(registers.command_list_base, layout.command_list);
    write32(registers.command_list_base_upper, 0);
    write32(registers.received_fis_base, layout.received_fis);
    write32(registers.received_fis_base_upper, 0);
}

fn clear_port_status(registers: AhciPortRegisters) {
    write32(registers.interrupt_status, u32::MAX);
    write32(registers.sata_error, u32::MAX);
}

fn prepare_link(registers: AhciPortRegisters) -> bool {
    let command = read32(registers.command) | PORT_CMD_SPIN_UP | PORT_CMD_POWER_ON;
    write32(registers.command, command);
    if link_is_active(read32(registers.sata_status)) {
        return true;
    }
    let control = read32(registers.sata_control);
    write32(registers.sata_control, (control & !0x0f) | 1);
    delay_ns(COMRESET_ASSERT_NS);
    write32(registers.sata_control, control & !0x0f);
    wait_until(PORT_TIMEOUT_NS, || {
        link_is_active(read32(registers.sata_status))
    })
}

const fn link_is_active(status: u32) -> bool {
    status & 0x0f == SATA_DET_PRESENT && (status >> 8) & 0x0f == SATA_IPM_ACTIVE
}

fn start_port(registers: AhciPortRegisters) -> bool {
    if !wait_until(PORT_TIMEOUT_NS, || {
        read32(registers.command) & (PORT_CMD_FIS_RUNNING | PORT_CMD_COMMAND_RUNNING) == 0
    }) {
        return false;
    }
    let command = read32(registers.command)
        | PORT_CMD_SPIN_UP
        | PORT_CMD_POWER_ON
        | PORT_CMD_FIS_RX
        | PORT_CMD_START;
    write32(registers.command, command);
    true
}

fn stop_port(registers: AhciPortRegisters, timeout_ns: u64) -> bool {
    let command = read32(registers.command) & !PORT_CMD_START;
    write32(registers.command, command);
    if !wait_until(timeout_ns, || {
        read32(registers.command) & PORT_CMD_COMMAND_RUNNING == 0
    }) {
        return false;
    }
    let command = read32(registers.command) & !PORT_CMD_FIS_RX;
    write32(registers.command, command);
    wait_until(timeout_ns, || {
        read32(registers.command) & PORT_CMD_FIS_RUNNING == 0
    })
}

fn bios_handoff(registers: AhciRegisterLayout) -> Result<(), &'static str> {
    if read32(registers.cap2()) & CAP2_BOH == 0 {
        return Ok(());
    }
    let handoff = read32(registers.bios_handoff());
    write32(registers.bios_handoff(), handoff | BOHC_OS_OWNED);
    if wait_until(HBA_TIMEOUT_NS, || {
        read32(registers.bios_handoff()) & (BOHC_BIOS_OWNED | BOHC_BIOS_BUSY) == 0
    }) {
        Ok(())
    } else {
        Err("AHCI BIOS ownership handoff timed out")
    }
}

fn reset_hba(registers: AhciRegisterLayout) -> Result<(), &'static str> {
    write32(registers.ghc(), read32(registers.ghc()) | GHC_AHCI_ENABLE);
    write32(
        registers.ghc(),
        read32(registers.ghc()) | GHC_AHCI_ENABLE | GHC_HBA_RESET,
    );
    if !wait_until(HBA_TIMEOUT_NS, || {
        read32(registers.ghc()) & GHC_HBA_RESET == 0
    }) {
        return Err("AHCI HBA reset timed out");
    }
    write32(registers.ghc(), read32(registers.ghc()) | GHC_AHCI_ENABLE);
    write32(registers.interrupt_status(), u32::MAX);
    Ok(())
}

fn wait_until(timeout_ns: u64, mut ready: impl FnMut() -> bool) -> bool {
    let start = hal::time::monotonic_ns();
    loop {
        if ready() {
            return true;
        }
        if hal::time::monotonic_ns().saturating_sub(start) >= timeout_ns {
            return false;
        }
        core::hint::spin_loop();
    }
}

fn delay_ns(duration_ns: u64) {
    let start = hal::time::monotonic_ns();
    while hal::time::monotonic_ns().saturating_sub(start) < duration_ns {
        core::hint::spin_loop();
    }
}

fn read32(address: usize) -> u32 {
    // Safety: 所有地址均由已验证的 AHCI MMIO 布局产生，并满足 32 位对齐。
    unsafe { read_volatile(address as *const u32) }
}

fn write32(address: usize, value: u32) {
    // Safety: 安全条件与 `read32` 相同，目标寄存器支持对齐的 32 位易失写入。
    unsafe { write_volatile(address as *mut u32, value) }
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn map_platform_irq_error(
    info: &PlatformDeviceInfo,
    error: PlatformIrqRegistrationError,
) -> PnpError {
    match error {
        PlatformIrqRegistrationError::NoResource => {
            PnpError::missing(PnpResourceKind::Irq, "AHCI interrupt missing")
        }
        PlatformIrqRegistrationError::Unresolved => {
            PnpError::dependency(first_irq_dependency(info))
        }
        PlatformIrqRegistrationError::RegistrationFailed { err, .. } => match err {
            IrqError::OutOfMemory => PnpError::OutOfMemory,
            IrqError::AlreadyRegistered => {
                PnpError::registration_failed(PnpResourceKind::Irq, "AHCI interrupt busy")
            }
            IrqError::NotFound => {
                PnpError::registration_failed(PnpResourceKind::Irq, "AHCI interrupt unavailable")
            }
        },
    }
}

struct AhciFactory;

impl DriverFactory for AhciFactory {
    fn name(&self) -> &'static str {
        "platform-ahci"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(AhciPlatformDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(AhciFactory))
}
