//! RISC-V IOMMU 1.0 platform ELM 驱动。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use allocator::PAGE_SIZE;
use vfs::sync::Spinlock;

use crate::bits::*;
use crate::dev::dma::{
    self, DmaBuffer, DmaConstraints, DmaContext, DmaDirection, DmaMappedRegion, DmaMapper,
    DmaSyncRegion,
};
use crate::dev::iommu::{
    self, IommuAttachRequest, IommuController, IommuDomain, IommuError, IommuRequester,
};
use crate::dev::irq::{self, IrqError, IrqHandler, IrqLine, IrqStatus};
use crate::dev::pci::{
    PciBarType, PciDevice, PciFunctionFirmwareInfo, PciInfo, pci_function_firmware_info,
};
use crate::dev::platform::{
    PlatformDeviceInfo, PlatformIrqRegistrationError, PlatformIrqResolveError,
};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory, unregister_driver,
};
use crate::page_table::{PageTable, PageTableError};

const PLATFORM_COMPATIBLE: &str = "riscv,iommu";
const PCI_COMPATIBLE: &str = "riscv,pci-iommu";

/// 关闭本地中断后持有 controller state，避免 WSI handler 重入同步命令路径。
struct IrqSafeSpinlock<T> {
    inner: Spinlock<T>,
}

impl<T> IrqSafeSpinlock<T> {
    const fn new(value: T) -> Self {
        Self {
            inner: Spinlock::new(value),
        }
    }

    fn lock(&self) -> IrqSafeSpinlockGuard<'_, T> {
        let irq_state = LocalIrqState::acquire();
        let guard = self.inner.lock();
        IrqSafeSpinlockGuard {
            guard,
            _irq_state: irq_state,
        }
    }
}

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

struct IrqSafeSpinlockGuard<'a, T> {
    // 字段按声明顺序析构：必须先解锁，再恢复本地中断。
    guard: vfs::sync::SpinlockGuard<'a, T>,
    _irq_state: LocalIrqState,
}

impl<T> Deref for IrqSafeSpinlockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for IrqSafeSpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HardwareError {
    Invalid,
    OutOfMemory,
    Busy,
    Unsupported,
    Timeout,
    Queue,
    Directory,
}

#[derive(Clone, Copy)]
struct Registers {
    base: usize,
}

impl Registers {
    fn new(base: usize, size: usize) -> Result<Self, HardwareError> {
        if base == 0 || size < REG_SIZE || !base.is_multiple_of(8) {
            return Err(HardwareError::Invalid);
        }
        Ok(Self { base })
    }

    fn read32(self, offset: usize) -> u32 {
        // Safety: probe 已验证整个 4 KiB 标准寄存器窗口完成映射，offset 均为规范常量。
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(self, offset: usize, value: u32) {
        // Safety: 与 `read32` 相同，写入地址是已映射且按 32-bit 对齐的 IOMMU CSR。
        unsafe { write_volatile((self.base + offset) as *mut u32, value) };
        hal::memory::device_io_barrier();
    }

    fn read64(self, offset: usize) -> u64 {
        // Safety: probe 已验证窗口，所有 64-bit CSR offset 均按 8 字节对齐。
        unsafe { read_volatile((self.base + offset) as *const u64) }
    }

    fn write64(self, offset: usize, value: u64) {
        // Safety: 与 `read64` 相同，目标是规范定义的 64-bit IOMMU CSR。
        unsafe { write_volatile((self.base + offset) as *mut u64, value) };
        hal::memory::device_io_barrier();
    }

    fn wait32(self, offset: usize, predicate: impl Fn(u32) -> bool) -> Option<u32> {
        for _ in 0..POLL_LIMIT {
            let value = self.read32(offset);
            if predicate(value) {
                return Some(value);
            }
            core::hint::spin_loop();
        }
        None
    }

    fn wait64(self, offset: usize, predicate: impl Fn(u64) -> bool) -> Option<u64> {
        for _ in 0..POLL_LIMIT {
            let value = self.read64(offset);
            if predicate(value) {
                return Some(value);
            }
            core::hint::spin_loop();
        }
        None
    }
}

struct CommandQueue {
    memory: Option<DmaBuffer>,
    tail: u32,
    mask: u32,
}

impl CommandQueue {
    fn new(regs: Registers) -> Result<Self, HardwareError> {
        let memory = dma_page(DmaDirection::ToDevice)?;
        let base = encode_ppn(memory.paddr()) | QUEUE_LOG2SZ;
        regs.write64(REG_CQB, base);
        let readback = regs.read64(REG_CQB);
        let log2sz = readback & 0x1f;
        if readback & !0x1f != base & !0x1f || log2sz > QUEUE_LOG2SZ {
            return Err(HardwareError::Queue);
        }
        let mask = (2u32 << log2sz) - 1;
        regs.write32(REG_CQT, 0);
        regs.write32(
            REG_CQCSR,
            QUEUE_ENABLE
                | QUEUE_MEM_FAULT
                | CQCSR_TIMEOUT
                | CQCSR_ILLEGAL
                | CQCSR_FENCE_WRITE_PENDING,
        );
        regs.wait32(REG_CQCSR, |value| value & QUEUE_ACTIVE != 0)
            .ok_or(HardwareError::Timeout)?;
        Ok(Self {
            memory: Some(memory),
            tail: 0,
            mask,
        })
    }

    fn push(&mut self, regs: Registers, command: Command) -> Result<(), HardwareError> {
        let next = (self.tail + 1) & self.mask;
        regs.wait32(REG_CQH, |head| head & self.mask != next)
            .ok_or(HardwareError::Timeout)?;
        let memory = self
            .memory
            .as_ref()
            .expect("enabled command queue retains its DMA buffer");
        let address = memory.vaddr() + self.tail as usize * core::mem::size_of::<Command>();
        // Safety: CQ 分配一整页，64 个 16-byte entry 完整落在页内，tail 始终被 mask。
        unsafe { write_volatile(address as *mut Command, command) };
        memory.sync_for_device();
        hal::memory::device_io_barrier();
        self.tail = next;
        regs.write32(REG_CQT, self.tail);
        Ok(())
    }

    fn wait_idle(&self, regs: Registers) -> Result<(), HardwareError> {
        regs.wait32(REG_CQH, |head| head & self.mask == self.tail)
            .ok_or(HardwareError::Timeout)?;
        let status = regs.read32(REG_CQCSR);
        let errors =
            status & (QUEUE_MEM_FAULT | CQCSR_TIMEOUT | CQCSR_ILLEGAL | CQCSR_FENCE_WRITE_PENDING);
        if errors != 0 {
            regs.write32(
                REG_CQCSR,
                QUEUE_ENABLE | (status & QUEUE_INTERRUPT_ENABLE) | errors,
            );
            return Err(HardwareError::Queue);
        }
        Ok(())
    }

    fn set_interrupt_enabled(&self, regs: Registers, enabled: bool) {
        let value = QUEUE_ENABLE | if enabled { QUEUE_INTERRUPT_ENABLE } else { 0 };
        regs.write32(REG_CQCSR, value);
    }

    fn service_interrupt(&self, regs: Registers, interrupt_enabled: bool) {
        let status = regs.read32(REG_CQCSR);
        let errors =
            status & (QUEUE_MEM_FAULT | CQCSR_TIMEOUT | CQCSR_ILLEGAL | CQCSR_FENCE_WRITE_PENDING);
        if errors != 0 {
            log::error!(
                "[riscv-iommu] command queue interrupt: csr={:#x} memory-fault={} timeout={} illegal={} fence-write-pending={}",
                status,
                errors & QUEUE_MEM_FAULT != 0,
                errors & CQCSR_TIMEOUT != 0,
                errors & CQCSR_ILLEGAL != 0,
                errors & CQCSR_FENCE_WRITE_PENDING != 0,
            );
            regs.write32(
                REG_CQCSR,
                QUEUE_ENABLE
                    | if interrupt_enabled {
                        QUEUE_INTERRUPT_ENABLE
                    } else {
                        0
                    }
                    | errors,
            );
        }
        regs.write32(REG_IPSR, IPSR_CQ);
    }

    fn disable(&self, regs: Registers) -> bool {
        regs.write32(REG_CQCSR, 0);
        regs.wait32(REG_CQCSR, |value| value & (QUEUE_ACTIVE | QUEUE_BUSY) == 0)
            .is_some()
    }

    fn leak_memory(&mut self) {
        if let Some(memory) = self.memory.take() {
            core::mem::forget(memory);
        }
    }

    fn stop_or_leak(&mut self, regs: Registers) -> bool {
        if self.disable(regs) {
            true
        } else {
            self.leak_memory();
            false
        }
    }
}

struct FaultQueue {
    memory: Option<DmaBuffer>,
    head: u32,
    mask: u32,
}

impl FaultQueue {
    fn new(regs: Registers) -> Result<Self, HardwareError> {
        let memory = dma_page(DmaDirection::FromDevice)?;
        let base = encode_ppn(memory.paddr()) | QUEUE_LOG2SZ;
        regs.write64(REG_FQB, base);
        let readback = regs.read64(REG_FQB);
        let log2sz = readback & 0x1f;
        if readback & !0x1f != base & !0x1f || log2sz > QUEUE_LOG2SZ {
            return Err(HardwareError::Queue);
        }
        let mask = (2u32 << log2sz) - 1;
        regs.write32(REG_FQH, 0);
        regs.write32(REG_FQCSR, QUEUE_ENABLE | QUEUE_MEM_FAULT | QUEUE_OVERFLOW);
        regs.wait32(REG_FQCSR, |value| value & QUEUE_ACTIVE != 0)
            .ok_or(HardwareError::Timeout)?;
        Ok(Self {
            memory: Some(memory),
            head: 0,
            mask,
        })
    }

    fn drain(&mut self, regs: Registers, interrupt_enabled: bool) {
        // 先清 pending，随后到达的新 record 才能产生新的通知；若 FQCSR 仍有
        // 错误，硬件会保持 FIP，错误清除后允许下一次 handler 完成收尾。
        if regs.read32(REG_IPSR) & IPSR_FQ != 0 {
            regs.write32(REG_IPSR, IPSR_FQ);
        }
        let memory = self
            .memory
            .as_ref()
            .expect("enabled fault queue retains its DMA buffer");
        memory.sync_for_cpu();
        loop {
            let tail = regs.read32(REG_FQT) & self.mask;
            if self.head == tail {
                break;
            }
            while self.head != tail {
                let address =
                    memory.vaddr() + self.head as usize * core::mem::size_of::<FaultRecord>();
                // Safety: FQ 使用同一 64-entry mask，每个 32-byte record 完整落在分配页内。
                let record = unsafe { read_volatile(address as *const FaultRecord) };
                let cause = record.header & 0xfff;
                let did = (record.header >> 40) & 0x00ff_ffff;
                log::error!(
                    "[riscv-iommu] fault cause={} did={:#x} iotval={:#x} iotval2={:#x}",
                    cause,
                    did,
                    record.iotval,
                    record.iotval2
                );
                self.head = (self.head + 1) & self.mask;
            }
            regs.write32(REG_FQH, self.head);
            memory.sync_for_cpu();
        }
        let status = regs.read32(REG_FQCSR);
        let errors = status & (QUEUE_MEM_FAULT | QUEUE_OVERFLOW);
        if errors != 0 {
            log::error!(
                "[riscv-iommu] fault queue interrupt: csr={:#x} memory-fault={} overflow={}",
                status,
                errors & QUEUE_MEM_FAULT != 0,
                errors & QUEUE_OVERFLOW != 0,
            );
            regs.write32(
                REG_FQCSR,
                QUEUE_ENABLE
                    | if interrupt_enabled {
                        QUEUE_INTERRUPT_ENABLE
                    } else {
                        0
                    }
                    | errors,
            );
        }
    }

    fn set_interrupt_enabled(&self, regs: Registers, enabled: bool) {
        let value = QUEUE_ENABLE | if enabled { QUEUE_INTERRUPT_ENABLE } else { 0 };
        regs.write32(REG_FQCSR, value);
    }

    fn disable(&self, regs: Registers) -> bool {
        regs.write32(REG_FQCSR, 0);
        regs.wait32(REG_FQCSR, |value| value & (QUEUE_ACTIVE | QUEUE_BUSY) == 0)
            .is_some()
    }

    fn leak_memory(&mut self) {
        if let Some(memory) = self.memory.take() {
            core::mem::forget(memory);
        }
    }

    fn stop_or_leak(&mut self, regs: Registers) -> bool {
        if self.disable(regs) {
            true
        } else {
            self.leak_memory();
            false
        }
    }
}

struct DirectoryPage {
    buffer: DmaBuffer,
}

impl DirectoryPage {
    fn new() -> Result<Self, HardwareError> {
        Ok(Self {
            buffer: dma_page(DmaDirection::ToDevice)?,
        })
    }

    fn read_word(&self, word: usize) -> u64 {
        let address = self.buffer.vaddr() + word * core::mem::size_of::<u64>();
        // Safety: word 由已验证的 DDI 索引和 context 字段产生，始终位于 4 KiB 页内。
        u64::from_le(unsafe { read_volatile(address as *const u64) })
    }

    fn write_word(&self, word: usize, value: u64) {
        let address = self.buffer.vaddr() + word * core::mem::size_of::<u64>();
        // Safety: 与 `read_word` 相同，目标是有效且对齐的 DDT/DC u64 槽位。
        unsafe { write_volatile(address as *mut u64, value.to_le()) };
        self.buffer.sync_for_device();
    }
}

struct DeviceDirectory {
    extended: bool,
    mode: u8,
    pas: u8,
    pages: Vec<DirectoryPage>,
}

impl DeviceDirectory {
    fn new(extended: bool, pas: u8) -> Result<Self, HardwareError> {
        let mut pages = Vec::new();
        pages
            .try_reserve(1)
            .map_err(|_| HardwareError::OutOfMemory)?;
        pages.push(DirectoryPage::new()?);
        Ok(Self {
            extended,
            mode: DDTP_MODE_OFF,
            pas,
            pages,
        })
    }

    fn root_paddr(&self) -> usize {
        self.pages[0].buffer.paddr()
    }

    fn physical_valid(&self, paddr: usize) -> bool {
        self.pas as u32 >= usize::BITS || paddr < (1usize << self.pas)
    }

    fn page_by_paddr(&self, paddr: usize) -> Option<usize> {
        self.pages
            .iter()
            .position(|page| page.buffer.paddr() == paddr)
    }

    fn device_limit_bits(&self) -> [u8; 3] {
        if self.extended {
            [6, 15, 24]
        } else {
            [7, 16, 24]
        }
    }

    fn context_location(&mut self, device_id: u32) -> Result<(usize, usize), HardwareError> {
        let levels = self
            .mode
            .checked_sub(DDTP_MODE_1LVL)
            .map(|depth| depth as usize + 1)
            .filter(|levels| (1..=3).contains(levels))
            .ok_or(HardwareError::Directory)?;
        let split = self.device_limit_bits();
        if u64::from(device_id) >= (1u64 << split[levels - 1]) {
            return Err(HardwareError::Invalid);
        }
        let mut page_index = 0usize;
        for level in (1..levels).rev() {
            let entry_index = (device_id as usize >> split[level - 1]) & 0x1ff;
            let entry = self.pages[page_index].read_word(entry_index);
            if entry & DDTE_VALID != 0 {
                if entry & !((((1u64 << 44) - 1) << 10) | DDTE_VALID) != 0 {
                    return Err(HardwareError::Directory);
                }
                let paddr = (((entry >> 10) & ((1u64 << 44) - 1)) << 12) as usize;
                page_index = self.page_by_paddr(paddr).ok_or(HardwareError::Directory)?;
                continue;
            }
            if entry != 0 {
                return Err(HardwareError::Directory);
            }
            self.pages
                .try_reserve(1)
                .map_err(|_| HardwareError::OutOfMemory)?;
            let page = DirectoryPage::new()?;
            let paddr = page.buffer.paddr();
            if !self.physical_valid(paddr) {
                return Err(HardwareError::Invalid);
            }
            let child = self.pages.len();
            self.pages.push(page);
            self.pages[page_index].write_word(entry_index, encode_ppn(paddr) | DDTE_VALID);
            page_index = child;
        }
        let low_bits = split[0] as usize;
        let context_index = device_id as usize & ((1usize << low_bits) - 1);
        let words_per_context = if self.extended { 8 } else { 4 };
        Ok((page_index, context_index * words_per_context))
    }

    fn context_valid(&mut self, device_id: u32) -> Result<bool, HardwareError> {
        let (page, word) = self.context_location(device_id)?;
        Ok(self.pages[page].read_word(word) & DC_TC_VALID != 0)
    }

    fn install_context(&mut self, device_id: u32, fsc: u64) -> Result<(), HardwareError> {
        let (page, word) = self.context_location(device_id)?;
        if self.pages[page].read_word(word) & DC_TC_VALID != 0 {
            return Err(HardwareError::Busy);
        }
        let words = if self.extended { 8 } else { 4 };
        for offset in 0..words {
            self.pages[page].write_word(word + offset, 0);
        }
        // DC word 1 是 IOHGATP（BARE），word 2 是 TA，word 3 是 FSC/IOSATP。
        self.pages[page].write_word(word + 3, fsc);
        hal::memory::device_io_barrier();
        // V 必须最后发布，避免硬件观察到半初始化 context。
        self.pages[page].write_word(word, DC_TC_VALID);
        Ok(())
    }

    fn clear_context(&mut self, device_id: u32) -> Result<(), HardwareError> {
        let (page, word) = self.context_location(device_id)?;
        self.pages[page].write_word(word, 0);
        hal::memory::device_io_barrier();
        Ok(())
    }

    fn leak_memory(&mut self) {
        for page in self.pages.drain(..) {
            core::mem::forget(page);
        }
    }
}

struct ControllerState {
    command: CommandQueue,
    fault: FaultQueue,
    directory: DeviceDirectory,
    attached: Vec<u32>,
    enabled: bool,
    interrupts_enabled: bool,
}

impl ControllerState {
    fn execute(
        &mut self,
        regs: Registers,
        capabilities: u64,
        commands: &[Command],
    ) -> Result<(), HardwareError> {
        if !self.enabled {
            return Err(HardwareError::Busy);
        }
        for command in commands {
            self.command.push(regs, *command)?;
        }
        self.command.push(regs, Command::iofence())?;
        self.command.wait_idle(regs)?;
        self.service_pending(regs, capabilities);
        Ok(())
    }

    fn service_pending(&mut self, regs: Registers, capabilities: u64) -> u32 {
        let pending = regs.read32(REG_IPSR) & IPSR_ALL;
        if pending & IPSR_CQ != 0 {
            self.command
                .service_interrupt(regs, self.interrupts_enabled);
        }
        if pending & IPSR_FQ != 0 {
            self.fault.drain(regs, self.interrupts_enabled);
        }
        if pending & IPSR_PM != 0 {
            service_performance_interrupt(regs, capabilities);
        }
        if pending & IPSR_PQ != 0 {
            service_page_request_interrupt(regs, capabilities);
        }
        pending
    }

    fn set_interrupts_enabled(&mut self, regs: Registers, enabled: bool) {
        self.command.set_interrupt_enabled(regs, enabled);
        self.fault.set_interrupt_enabled(regs, enabled);
        self.interrupts_enabled = enabled;
    }
}

fn service_performance_interrupt(regs: Registers, capabilities: u64) {
    let overflow = regs.read32(REG_IOCOUNTOVF);
    if capabilities & CAP_HPM == 0 {
        log::error!(
            "[riscv-iommu] unexpected performance-monitor interrupt without HPM capability"
        );
    } else if overflow != 0 {
        log::warning!(
            "[riscv-iommu] performance-monitor overflow bitmap={:#010x}",
            overflow
        );
    }

    if overflow & 1 != 0 {
        let cycles = regs.read64(REG_IOHPMCYCLES);
        regs.write64(REG_IOHPMCYCLES, cycles & !HPM_OVERFLOW);
    }
    for counter in 0..HPM_EVENT_COUNTERS {
        if overflow & (1u32 << (counter + 1)) == 0 {
            continue;
        }
        let offset = REG_IOHPMEVT_BASE + counter * core::mem::size_of::<u64>();
        let event = regs.read64(offset);
        regs.write64(offset, event & !HPM_OVERFLOW);
    }
    regs.write32(REG_IPSR, IPSR_PM);
}

fn service_page_request_interrupt(regs: Registers, capabilities: u64) {
    let status = regs.read32(REG_PQCSR);
    let errors = status & (QUEUE_MEM_FAULT | QUEUE_OVERFLOW);
    log::error!(
        "[riscv-iommu] unexpected page-request interrupt while PRI is disabled: pqcsr={:#x} ats-capable={}",
        status,
        capabilities & CAP_ATS != 0,
    );
    if errors != 0 {
        // 保持 PQEN/PIE 的现状，只清规范定义的 W1C 错误位。当前驱动从不打开
        // PQ，若固件留下异常状态也不会误接管未知的 PRI ring。
        regs.write32(
            REG_PQCSR,
            (status & (QUEUE_ENABLE | QUEUE_INTERRUPT_ENABLE)) | errors,
        );
    }
    regs.write32(REG_IPSR, IPSR_PQ);
}

fn stop_controller_state(regs: Registers, state: &mut ControllerState) -> bool {
    state.set_interrupts_enabled(regs, false);
    let directory_stopped = regs_write_ddtp_off(regs);
    state.fault.drain(regs, false);
    let fault_stopped = state.fault.stop_or_leak(regs);
    let command_stopped = state.command.stop_or_leak(regs);
    regs.write32(REG_IPSR, IPSR_ALL);
    state.enabled = false;

    if !directory_stopped {
        // DDTP 仍可能引用任意层级的 directory page；泄漏比 allocator 重用后
        // 形成硬件 use-after-free 更安全。
        state.directory.leak_memory();
        log::error!(
            "[riscv-iommu] hardware shutdown failure: DDTP did not reach OFF; directory memory leaked"
        );
    }
    if !fault_stopped {
        log::error!(
            "[riscv-iommu] hardware shutdown failure: fault queue remained active; DMA buffer leaked"
        );
    }
    if !command_stopped {
        log::error!(
            "[riscv-iommu] hardware shutdown failure: command queue remained active; DMA buffer leaked"
        );
    }
    directory_stopped && fault_stopped && command_stopped
}

struct RiscvIommuCore {
    regs: Registers,
    capabilities: u64,
    translation_mode: u8,
    state: IrqSafeSpinlock<ControllerState>,
    quiesced: AtomicBool,
}

impl RiscvIommuCore {
    fn new(regs: Registers) -> Result<Arc<Self>, HardwareError> {
        let capabilities = regs.read64(REG_CAP);
        if !capability_version_supported(capabilities) {
            return Err(HardwareError::Unsupported);
        }
        let translation_mode = if capabilities & CAP_SV57 != 0 {
            10
        } else if capabilities & CAP_SV48 != 0 {
            9
        } else if capabilities & CAP_SV39 != 0 {
            8
        } else {
            return Err(HardwareError::Unsupported);
        };
        let pas = ((capabilities & CAP_PAS_MASK) >> CAP_PAS_SHIFT) as u8;
        if pas < 32 {
            return Err(HardwareError::Unsupported);
        }

        let mut fctl = regs.read32(REG_FCTL);
        if fctl & FCTL_BE != 0 {
            fctl &= !FCTL_BE;
            regs.write32(REG_FCTL, fctl);
            if regs.read32(REG_FCTL) & FCTL_BE != 0 {
                return Err(HardwareError::Unsupported);
            }
        }
        let igs = ((capabilities & CAP_IGS_MASK) >> CAP_IGS_SHIFT) as u8;
        if igs == 3 {
            return Err(HardwareError::Unsupported);
        }

        let ddtp = regs
            .wait64(REG_DDTP, |value| value & DDTP_BUSY == 0)
            .ok_or(HardwareError::Timeout)?;
        if (ddtp & DDTP_MODE_MASK) > u64::from(DDTP_MODE_BARE) {
            return Err(HardwareError::Busy);
        }

        let directory = DeviceDirectory::new(capabilities & CAP_MSI_FLAT != 0, pas)?;
        if directory.root_paddr() & (PAGE_SIZE - 1) != 0
            || !directory.physical_valid(directory.root_paddr())
        {
            return Err(HardwareError::Invalid);
        }
        let mut command = CommandQueue::new(regs)?;
        let mut fault = match FaultQueue::new(regs) {
            Ok(queue) => queue,
            Err(error) => {
                if !command.stop_or_leak(regs) {
                    log::error!(
                        "[riscv-iommu] probe rollback failure: command queue remained active"
                    );
                }
                return Err(error);
            }
        };
        // PRI 尚未进入通用 IOMMU domain 契约。明确关闭 PQ，不能在没有 page
        // response 路径时消费并丢弃设备请求；PQ cause 仍由共享 handler 诊断。
        regs.write32(REG_PQCSR, 0);
        if regs
            .wait32(REG_PQCSR, |value| value & (QUEUE_ACTIVE | QUEUE_BUSY) == 0)
            .is_none()
        {
            if !fault.stop_or_leak(regs) {
                log::error!("[riscv-iommu] probe rollback failure: fault queue remained active");
            }
            if !command.stop_or_leak(regs) {
                log::error!("[riscv-iommu] probe rollback failure: command queue remained active");
            }
            return Err(HardwareError::Timeout);
        }
        regs.write32(REG_IPSR, IPSR_ALL);
        let mut state = ControllerState {
            command,
            fault,
            directory,
            attached: Vec::new(),
            enabled: true,
            interrupts_enabled: false,
        };
        let mode = match Self::enable_directory(regs, &mut state) {
            Ok(mode) => mode,
            Err(error) => {
                stop_controller_state(regs, &mut state);
                return Err(error);
            }
        };
        state.directory.mode = mode;
        if let Err(error) = state.execute(
            regs,
            capabilities,
            &[Command::iodir_all(), Command::iotinval_all()],
        ) {
            stop_controller_state(regs, &mut state);
            return Err(error);
        }

        Ok(Arc::new(Self {
            regs,
            capabilities,
            translation_mode,
            state: IrqSafeSpinlock::new(state),
            quiesced: AtomicBool::new(false),
        }))
    }

    fn interrupt_generation(&self) -> u8 {
        ((self.capabilities & CAP_IGS_MASK) >> CAP_IGS_SHIFT) as u8
    }

    fn configure_wsi_vectors(&self, vector_count: usize) -> Result<u64, HardwareError> {
        if !matches!(self.interrupt_generation(), 1 | 2) {
            return Err(HardwareError::Unsupported);
        }
        let requested = interrupt_vector_layout(vector_count).ok_or(HardwareError::Invalid)?;
        let mut fctl = self.regs.read32(REG_FCTL);
        if fctl & FCTL_WSI == 0 {
            fctl |= FCTL_WSI;
            self.regs.write32(REG_FCTL, fctl);
            if self.regs.read32(REG_FCTL) & FCTL_WSI == 0 {
                return Err(HardwareError::Unsupported);
            }
        }
        self.regs.write64(REG_ICVEC, requested);
        let readback = self.regs.read64(REG_ICVEC);
        if !interrupt_vector_layout_valid(readback, vector_count) {
            return Err(HardwareError::Unsupported);
        }
        Ok(readback)
    }

    fn enable_event_interrupts(&self) {
        let mut state = self.state.lock();
        state.service_pending(self.regs, self.capabilities);
        state.set_interrupts_enabled(self.regs, true);
    }

    fn handle_interrupt(&self) -> bool {
        if self.quiesced.load(Ordering::Acquire) || self.regs.read32(REG_IPSR) & IPSR_ALL == 0 {
            return false;
        }
        let mut state = self.state.lock();
        if !state.enabled || self.quiesced.load(Ordering::Acquire) {
            return false;
        }
        state.service_pending(self.regs, self.capabilities) != 0
    }

    fn enable_directory(regs: Registers, state: &mut ControllerState) -> Result<u8, HardwareError> {
        for requested in (DDTP_MODE_1LVL..=DDTP_MODE_3LVL).rev() {
            let value = encode_ppn(state.directory.root_paddr()) | u64::from(requested);
            regs.write64(REG_DDTP, value);
            let readback = regs
                .wait64(REG_DDTP, |value| value & DDTP_BUSY == 0)
                .ok_or(HardwareError::Timeout)?;
            let accepted = (readback & DDTP_MODE_MASK) as u8;
            let ppn_mask = ((1u64 << 44) - 1) << 10;
            if matches!(accepted, DDTP_MODE_1LVL | DDTP_MODE_2LVL | DDTP_MODE_3LVL)
                && readback & ppn_mask == value & ppn_mask
            {
                return Ok(accepted);
            }
            if accepted > DDTP_MODE_BARE {
                return Err(HardwareError::Directory);
            }
        }
        Err(HardwareError::Unsupported)
    }

    fn attach_domain(
        self: &Arc<Self>,
        requester: IommuRequester,
        device_id: u32,
        page_table: PageTable,
    ) -> Result<RiscvIommuDomain, IommuError> {
        if self.quiesced.load(Ordering::Acquire) {
            return Err(IommuError::Busy);
        }
        let mut state = self.state.lock();
        if state.attached.contains(&device_id) {
            return Err(IommuError::Busy);
        }
        state
            .attached
            .try_reserve(1)
            .map_err(|_| IommuError::OutOfMemory)?;
        if state
            .directory
            .context_valid(device_id)
            .map_err(map_hardware_iommu_error)?
        {
            return Err(IommuError::Busy);
        }
        let fsc = (u64::from(page_table.mode()) << 60) | ((page_table.root_paddr() as u64) >> 12);
        state
            .directory
            .install_context(device_id, fsc)
            .map_err(map_hardware_iommu_error)?;
        if let Err(error) = state.execute(
            self.regs,
            self.capabilities,
            &[Command::iodir_device(device_id), Command::iotinval_all()],
        ) {
            let _ = state.directory.clear_context(device_id);
            // 无法确认 IODIR/IOFENCE 生效时，硬件仍可能缓存刚发布的 FSC。泄漏
            // 页表比把页面交还 allocator 后形成 DMA UAF 安全。
            core::mem::forget(page_table);
            return Err(map_hardware_iommu_error(error));
        }
        state.attached.push(device_id);
        drop(state);
        Ok(RiscvIommuDomain {
            core: Arc::clone(self),
            requester,
            device_id,
            state: ManuallyDrop::new(Spinlock::new(DomainState {
                page_table,
                mappings: Vec::new(),
                next_token: 1,
            })),
        })
    }

    fn invalidate_all(&self) -> bool {
        if self.quiesced.load(Ordering::Acquire) {
            return false;
        }
        self.state
            .lock()
            .execute(self.regs, self.capabilities, &[Command::iotinval_all()])
            .is_ok()
    }

    fn poll_faults(&self) {
        if !self.quiesced.load(Ordering::Acquire) {
            let mut state = self.state.lock();
            let interrupts_enabled = state.interrupts_enabled;
            state.fault.drain(self.regs, interrupts_enabled);
            state.service_pending(self.regs, self.capabilities);
        }
    }

    fn detach(&self, device_id: u32) -> bool {
        if self.quiesced.load(Ordering::Acquire) {
            return false;
        }
        let mut state = self.state.lock();
        if !state.attached.contains(&device_id) {
            return true;
        }
        if state.directory.clear_context(device_id).is_err() {
            return false;
        }
        if state
            .execute(
                self.regs,
                self.capabilities,
                &[Command::iodir_device(device_id), Command::iotinval_all()],
            )
            .is_err()
        {
            return false;
        }
        state.attached.retain(|existing| *existing != device_id);
        true
    }

    fn quiesce(&self) {
        if self.quiesced.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.state.lock();
        if !state.attached.is_empty() {
            log::error!(
                "[riscv-iommu] refusing clean shutdown with {} attached device(s)",
                state.attached.len()
            );
        }
        stop_controller_state(self.regs, &mut state);
    }
}

impl Drop for RiscvIommuCore {
    fn drop(&mut self) {
        self.quiesce();
    }
}

fn regs_write_ddtp_off(regs: Registers) -> bool {
    regs.write64(REG_DDTP, u64::from(DDTP_MODE_OFF));
    regs.wait64(REG_DDTP, |value| {
        value & DDTP_BUSY == 0 && value & DDTP_MODE_MASK == u64::from(DDTP_MODE_OFF)
    })
    .is_some()
}

struct RiscvIommuController {
    core: Arc<RiscvIommuCore>,
}

impl IommuController for RiscvIommuController {
    fn attach(&self, request: IommuAttachRequest) -> Result<Arc<dyn IommuDomain>, IommuError> {
        let (requester, specifier) = request.into_parts();
        let [device_id] = specifier.as_ref() else {
            return Err(IommuError::InvalidSpecifier);
        };
        if *device_id >= (1 << 24) {
            return Err(IommuError::InvalidSpecifier);
        }
        let pas = ((self.core.capabilities & CAP_PAS_MASK) >> CAP_PAS_SHIFT) as u8;
        let page_table =
            PageTable::new(self.core.translation_mode, pas).map_err(map_page_table_iommu_error)?;
        let domain = self.core.attach_domain(requester, *device_id, page_table)?;
        Ok(Arc::new(domain))
    }
}

#[derive(Clone, Copy)]
struct MappingRecord {
    token: u64,
    dma_addr: usize,
    iova_base: usize,
    length: usize,
}

struct DomainState {
    page_table: PageTable,
    mappings: Vec<MappingRecord>,
    next_token: u64,
}

impl DomainState {
    fn overlaps(&self, base: usize, length: usize) -> bool {
        let Some(end) = base.checked_add(length) else {
            return true;
        };
        self.mappings.iter().any(|mapping| {
            let mapping_end = mapping.iova_base + mapping.length;
            base < mapping_end && mapping.iova_base < end
        })
    }

    fn first_fit(&self, length: usize, limit: usize) -> Option<usize> {
        let mut candidate = PAGE_SIZE;
        loop {
            let end = candidate.checked_add(length)?;
            if end.checked_sub(1)? > limit {
                return None;
            }
            let mut next = None;
            for mapping in &self.mappings {
                let mapping_end = mapping.iova_base.checked_add(mapping.length)?;
                if candidate < mapping_end && mapping.iova_base < end {
                    next = Some(next.map_or(mapping_end, |old: usize| old.max(mapping_end)));
                }
            }
            let Some(next) = next else {
                return Some(candidate);
            };
            candidate = align_up(next, PAGE_SIZE)?;
        }
    }

    fn next_token(&mut self) -> Option<u64> {
        let token = self.next_token;
        self.next_token = self.next_token.checked_add(1)?;
        (token != 0).then_some(token)
    }
}

struct RiscvIommuDomain {
    core: Arc<RiscvIommuCore>,
    requester: IommuRequester,
    device_id: u32,
    state: ManuallyDrop<Spinlock<DomainState>>,
}

impl RiscvIommuDomain {
    fn geometry(region: DmaSyncRegion) -> Option<(usize, usize, usize)> {
        if region.len == 0 {
            return None;
        }
        let paddr_base = region.paddr & !(PAGE_SIZE - 1);
        let offset = region.paddr - paddr_base;
        let covered = offset.checked_add(region.len)?;
        let length = align_up(covered, PAGE_SIZE)?;
        paddr_base.checked_add(length)?;
        Some((paddr_base, offset, length))
    }

    fn map(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        requested: Option<usize>,
    ) -> Option<DmaMappedRegion> {
        let (paddr_base, offset, length) = Self::geometry(region)?;
        let mut state = self.state.lock();
        state.mappings.try_reserve(1).ok()?;
        let token = state.next_token()?;
        let limit = constraints.address_mask.min(state.page_table.max_iova());
        let iova_base = match requested {
            Some(dma_addr) => {
                if dma_addr & (PAGE_SIZE - 1) != offset {
                    return None;
                }
                dma_addr.checked_sub(offset)?
            }
            None => state.first_fit(length, limit)?,
        };
        let dma_addr = iova_base.checked_add(offset)?;
        if iova_base.is_multiple_of(PAGE_SIZE)
            && !state.overlaps(iova_base, length)
            && constraints_accepts(constraints, dma_addr, region.len)
        {
            let writable = !matches!(region.direction, DmaDirection::ToDevice);
            let mut mapped = 0usize;
            while mapped < length {
                if state
                    .page_table
                    .map_page(iova_base + mapped, paddr_base + mapped, writable)
                    .is_err()
                {
                    while mapped != 0 {
                        mapped -= PAGE_SIZE;
                        let _ = state.page_table.unmap_page(iova_base + mapped);
                    }
                    return None;
                }
                mapped += PAGE_SIZE;
            }
            if !self.core.invalidate_all() {
                while mapped != 0 {
                    mapped -= PAGE_SIZE;
                    let _ = state.page_table.unmap_page(iova_base + mapped);
                }
                let _ = self.core.invalidate_all();
                return None;
            }
            state.mappings.push(MappingRecord {
                token,
                dma_addr,
                iova_base,
                length,
            });
            Some(DmaMappedRegion { dma_addr, token })
        } else {
            None
        }
    }
}

impl DmaMapper for RiscvIommuDomain {
    fn sync_for_device(&self, region: DmaSyncRegion) {
        dma::sync_for_device(region);
        self.core.poll_faults();
    }

    fn sync_for_cpu(&self, region: DmaSyncRegion) {
        dma::sync_for_cpu(region);
        self.core.poll_faults();
    }

    fn phys_to_dma(&self, _region: DmaSyncRegion, _constraints: DmaConstraints) -> Option<usize> {
        None
    }

    fn map_region(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
    ) -> Option<DmaMappedRegion> {
        self.map(region, constraints, None)
    }

    fn map_region_at(
        &self,
        region: DmaSyncRegion,
        constraints: DmaConstraints,
        dma_addr: usize,
    ) -> Option<DmaMappedRegion> {
        self.map(region, constraints, Some(dma_addr))
    }

    fn unmap_region(&self, _region: DmaSyncRegion, mapping: DmaMappedRegion) -> bool {
        let mut state = self.state.lock();
        let Some(index) = state.mappings.iter().position(|record| {
            record.token == mapping.token && record.dma_addr == mapping.dma_addr
        }) else {
            return false;
        };
        let record = state.mappings[index];
        let mut offset = 0usize;
        while offset < record.length {
            if state
                .page_table
                .unmap_page(record.iova_base + offset)
                .is_err()
            {
                return false;
            }
            offset += PAGE_SIZE;
        }
        if !self.core.invalidate_all() {
            return false;
        }
        state.mappings.swap_remove(index);
        true
    }
}

impl IommuDomain for RiscvIommuDomain {}

impl Drop for RiscvIommuDomain {
    fn drop(&mut self) {
        let outstanding = self.state.lock().mappings.len();
        if outstanding != 0 {
            log::error!(
                "[riscv-iommu] dropping DID {:#x} with {} mapping(s); context is revoked before pages are freed",
                self.device_id,
                outstanding
            );
        }
        if self.core.detach(self.device_id) {
            // Safety: state 包在 ManuallyDrop 中且本对象只执行一次 Drop；成功 revoke
            // device context 后，硬件已不再引用其中页表，现可正常析构。
            unsafe { ManuallyDrop::drop(&mut self.state) };
        } else {
            log::error!(
                "[riscv-iommu] failed to detach DID {:#x} requester={:?}; leaking page tables to prevent DMA UAF",
                self.device_id,
                self.requester
            );
        }
    }
}

struct Binding {
    core: Arc<RiscvIommuCore>,
}

struct RiscvIommuIrqHandler {
    core: Arc<RiscvIommuCore>,
}

impl IrqHandler for RiscvIommuIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.core.handle_interrupt() {
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

fn map_irq_error(error: IrqError) -> PnpError {
    match error {
        IrqError::OutOfMemory => PnpError::OutOfMemory,
        IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "riscv iommu IRQ line not found")
        }
        IrqError::AlreadyRegistered => PnpError::registration_failed(
            PnpResourceKind::Irq,
            "riscv iommu IRQ line conflicts with an exclusive handler",
        ),
    }
}

fn platform_irq_dependency(info: &PlatformDeviceInfo, index: usize) -> PnpDependency {
    info.irq_at(index)
        .and_then(|resource| resource.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn platform_wsi_count(info: &PlatformDeviceInfo) -> Result<usize, PnpError> {
    let mut count = 0usize;
    while count < INTERRUPT_CAUSE_COUNT {
        match info.resolve_irq_line_at(count) {
            Ok(_) => count += 1,
            Err(PlatformIrqResolveError::NoResource) => break,
            Err(PlatformIrqResolveError::Unresolved) => {
                return Err(PnpError::dependency(platform_irq_dependency(info, count)));
            }
        }
    }
    if count == INTERRUPT_CAUSE_COUNT && info.irq_at(INTERRUPT_CAUSE_COUNT).is_some() {
        return Err(PnpError::malformed(
            PnpResourceKind::Irq,
            "riscv iommu exposes more than four WSI vectors",
        ));
    }
    Ok(count)
}

fn install_platform_wsi(
    dev: &Arc<PnpDevice>,
    info: &PlatformDeviceInfo,
    core: &Arc<RiscvIommuCore>,
) -> Result<usize, PnpError> {
    if !matches!(core.interrupt_generation(), 1 | 2) {
        log::warning!(
            "[riscv-iommu] {} supports MSI signaling only; platform MSI programming is unavailable, using polling fallback",
            dev.id
        );
        return Ok(0);
    }

    let vector_count = platform_wsi_count(info)?;
    if vector_count == 0 {
        log::warning!(
            "[riscv-iommu] {} has no wired interrupt resources; using polling fallback",
            dev.id
        );
        return Ok(0);
    }
    dev.reserve_owned_resources(vector_count + 1)?;
    let layout = core
        .configure_wsi_vectors(vector_count)
        .map_err(map_probe_error)?;
    let handler: Arc<dyn IrqHandler> = Arc::new(RiscvIommuIrqHandler {
        core: Arc::clone(core),
    });
    for index in 0..vector_count {
        let handle = match info.register_irq_handler_at(index, Arc::clone(&handler)) {
            Ok(handle) => handle,
            Err(PlatformIrqRegistrationError::NoResource) => {
                return Err(PnpError::malformed(
                    PnpResourceKind::Irq,
                    "riscv iommu WSI resources changed during probe",
                ));
            }
            Err(PlatformIrqRegistrationError::Unresolved) => {
                return Err(PnpError::dependency(platform_irq_dependency(info, index)));
            }
            Err(PlatformIrqRegistrationError::RegistrationFailed { err, .. }) => {
                return Err(map_irq_error(err));
            }
        };
        if let Err(error) = dev.own_resource(irq::irq_handler_pnp_resource(
            handle,
            "riscv-iommu-platform-wsi",
        )) {
            let _ = irq::unregister_irq_handler(handle);
            return Err(error);
        }
    }
    core.enable_event_interrupts();
    log::printk!(
        "[riscv-iommu] {} enabled {} WSI vector(s), icvec={:#x}",
        dev.id,
        vector_count,
        layout
    );
    Ok(vector_count)
}

fn bind_controller(
    dev: &Arc<PnpDevice>,
    phandle: u32,
    core: Arc<RiscvIommuCore>,
    resource_label: &'static str,
    transport: &'static str,
    phys: u64,
) -> Result<(), PnpError> {
    let controller: Arc<dyn IommuController> = Arc::new(RiscvIommuController {
        core: Arc::clone(&core),
    });
    // controller 注册会同步唤醒 deferred consumer；必须先保证 provider resource
    // 槽位存在，避免发布依赖后因 PnP Vec 扩容 OOM 留下无主 controller。
    dev.reserve_owned_resources(1)?;
    let handle = match iommu::register_iommu_controller(phandle, controller) {
        Ok(handle) => handle,
        Err(error) => {
            core.quiesce();
            return Err(map_controller_registration_error(error));
        }
    };
    if let Err(error) =
        dev.own_boxed_resource(iommu::controller_pnp_resource_boxed(handle, resource_label))
    {
        let _ = iommu::unregister_iommu_controller(handle);
        core.quiesce();
        return Err(error);
    }
    dev.set_driver_data(Arc::new(Binding {
        core: Arc::clone(&core),
    }));
    log::printk!(
        "[riscv-iommu] bound {} transport={} phandle={:#x} phys={:#x} cap={:#x} ddt-mode={} s-stage=Sv{}",
        dev.id,
        transport,
        phandle,
        phys,
        core.capabilities,
        core.state.lock().directory.mode,
        match core.translation_mode {
            8 => 39,
            9 => 48,
            _ => 57,
        }
    );
    Ok(())
}

fn quiesce_binding(dev: &Arc<PnpDevice>) {
    if let Some(data) = dev.take_driver_data()
        && let Ok(binding) = data.downcast::<Binding>()
    {
        binding.core.quiesce();
    }
}

struct PlatformRiscvIommuDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl PnpDriver for PlatformRiscvIommuDriver {
    fn name(&self) -> &'static str {
        "platform-riscv-iommu"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(|info| info.has_id(PLATFORM_COMPATIBLE))
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::missing(
            PnpResourceKind::FirmwareBus,
            "riscv iommu phandle missing",
        ))?;
        if info.u32_property("#iommu-cells") != Some(1) {
            return Err(PnpError::malformed(
                PnpResourceKind::FirmwareBus,
                "riscv iommu requires #iommu-cells = <1>",
            ));
        }
        let (phys, size) = info.first_mmio().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "riscv iommu reg missing",
        ))?;
        let regs =
            Registers::new((self.device_mmio_to_virt)(phys), size).map_err(map_probe_error)?;
        let core = RiscvIommuCore::new(regs).map_err(map_probe_error)?;
        if let Err(error) = install_platform_wsi(dev, info, &core) {
            core.quiesce();
            return Err(error);
        }
        let result = bind_controller(
            dev,
            phandle,
            Arc::clone(&core),
            "platform-riscv-iommu-controller",
            "platform",
            phys as u64,
        );
        if result.is_err() {
            core.quiesce();
        }
        result
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        quiesce_binding(dev);
        log::printk!("[riscv-iommu] removed {} transport=platform", dev.id);
    }
}

fn is_riscv_pci_iommu(firmware: &PciFunctionFirmwareInfo) -> bool {
    firmware.has_compatible(PCI_COMPATIBLE)
}

fn pci_firmware_info(id: &PnpId) -> Option<PciFunctionFirmwareInfo> {
    let PnpId::Pci {
        segment,
        bus,
        device,
        function,
    } = id
    else {
        return None;
    };
    pci_function_firmware_info(*segment, *bus, *device, *function)
}

fn disable_pci_function(pci: &PciDevice, id: &PnpId) {
    if pci.try_disable_bus_master().is_err() {
        log::error!("[riscv-iommu] failed to disable bus master for {}", id);
    }
    if pci.try_disable_mmio().is_err() {
        log::error!("[riscv-iommu] failed to disable MMIO decode for {}", id);
    }
}

struct PciRiscvIommuDriver;

impl PnpDriver for PciRiscvIommuDriver {
    fn name(&self) -> &'static str {
        "pci-riscv-iommu"
    }

    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        info.as_any().downcast_ref::<PciInfo>().is_some()
            && pci_firmware_info(id)
                .as_ref()
                .is_some_and(is_riscv_pci_iommu)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let pci = PciDevice::from_pnp(dev).ok_or(PnpError::InvalidState)?;
        let firmware = pci.firmware_info().ok_or(PnpError::missing(
            PnpResourceKind::FirmwareBus,
            "riscv PCI iommu firmware metadata missing",
        ))?;
        if !is_riscv_pci_iommu(&firmware) {
            return Err(PnpError::InvalidState);
        }
        let phandle = firmware.phandle.ok_or(PnpError::missing(
            PnpResourceKind::FirmwareBus,
            "riscv PCI iommu phandle missing",
        ))?;
        if firmware.u32_property("#iommu-cells") != Some(1) {
            return Err(PnpError::malformed(
                PnpResourceKind::FirmwareBus,
                "riscv PCI iommu requires #iommu-cells = <1>",
            ));
        }

        let (bar, vaddr) = pci.map_bar_virt(0).ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "riscv PCI iommu BAR0 missing",
        ))?;
        if bar.bar_type != PciBarType::Memory {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "riscv PCI iommu BAR0 is not MMIO",
            ));
        }
        let size = usize::try_from(bar.size).map_err(|_| {
            PnpError::malformed(
                PnpResourceKind::Mmio,
                "riscv PCI iommu BAR0 is not representable",
            )
        })?;
        pci.try_enable_mmio().map_err(|_| {
            PnpError::hardware_failure("riscv PCI iommu failed to enable MMIO decode")
        })?;
        if pci.try_enable_bus_master().is_err() {
            disable_pci_function(&pci, &dev.id);
            return Err(PnpError::hardware_failure(
                "riscv PCI iommu failed to enable bus master",
            ));
        }

        let result = Registers::new(vaddr, size)
            .map_err(map_probe_error)
            .and_then(|regs| {
                RiscvIommuCore::new(regs)
                    .map_err(map_probe_error)
                    .and_then(|core| {
                        bind_controller(
                            dev,
                            phandle,
                            core,
                            "pci-riscv-iommu-controller",
                            "pci",
                            bar.phys_addr,
                        )
                    })
            });
        if result.is_err() {
            disable_pci_function(&pci, &dev.id);
        }
        result
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        quiesce_binding(dev);
        if let Some(pci) = PciDevice::from_pnp(dev) {
            disable_pci_function(&pci, &dev.id);
        }
        log::printk!("[riscv-iommu] removed {} transport=pci", dev.id);
    }
}

struct PlatformRiscvIommuFactory;

impl DriverFactory for PlatformRiscvIommuFactory {
    fn name(&self) -> &'static str {
        "platform-riscv-iommu"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(PlatformRiscvIommuDriver {
            device_mmio_to_virt: ctx.device_mmio_to_virt,
        }))
    }
}

struct PciRiscvIommuFactory;

impl DriverFactory for PciRiscvIommuFactory {
    fn name(&self) -> &'static str {
        "pci-riscv-iommu"
    }

    fn create(&self, _ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(PciRiscvIommuDriver))
    }
}

pub(super) fn register_builtin_drivers() -> Result<[DriverHandle; 2], PnpError> {
    let platform = register_driver_factory(Arc::new(PlatformRiscvIommuFactory))?;
    match register_driver_factory(Arc::new(PciRiscvIommuFactory)) {
        Ok(pci) => Ok([platform, pci]),
        Err(error) => {
            let _ = unregister_driver(platform);
            Err(error)
        }
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

fn dma_page(direction: DmaDirection) -> Result<DmaBuffer, HardwareError> {
    DmaBuffer::new_in(
        DmaContext::default_coherent(),
        PAGE_SIZE,
        PAGE_SIZE,
        direction,
    )
    .map_err(|_| HardwareError::OutOfMemory)
}

fn constraints_accepts(constraints: DmaConstraints, dma_addr: usize, len: usize) -> bool {
    dma_addr
        .checked_add(len.saturating_sub(1))
        .is_some_and(|end| end <= constraints.address_mask)
        && len <= constraints.max_segment_size
}

fn map_page_table_iommu_error(error: PageTableError) -> IommuError {
    match error {
        PageTableError::OutOfMemory => IommuError::OutOfMemory,
        PageTableError::Invalid => IommuError::Unsupported,
        PageTableError::Conflict | PageTableError::Corrupt => IommuError::HardwareFailure,
    }
}

fn map_hardware_iommu_error(error: HardwareError) -> IommuError {
    match error {
        HardwareError::OutOfMemory => IommuError::OutOfMemory,
        HardwareError::Busy => IommuError::Busy,
        HardwareError::Invalid => IommuError::InvalidSpecifier,
        HardwareError::Unsupported => IommuError::Unsupported,
        HardwareError::Timeout | HardwareError::Queue | HardwareError::Directory => {
            IommuError::HardwareFailure
        }
    }
}

fn map_probe_error(error: HardwareError) -> PnpError {
    log::error!("[riscv-iommu] hardware initialization failed: {:?}", error);
    match error {
        HardwareError::OutOfMemory => PnpError::OutOfMemory,
        HardwareError::Busy => PnpError::ResourceBusy {
            kind: PnpResourceKind::Dma,
            detail: "riscv iommu is already enabled",
        },
        HardwareError::Invalid | HardwareError::Unsupported => PnpError::malformed(
            PnpResourceKind::Mmio,
            "unsupported riscv iommu register capabilities",
        ),
        HardwareError::Timeout | HardwareError::Queue | HardwareError::Directory => {
            PnpError::HardwareFailure {
                detail: "riscv iommu initialization failed",
            }
        }
    }
}

fn map_controller_registration_error(error: IommuError) -> PnpError {
    match error {
        IommuError::OutOfMemory => PnpError::OutOfMemory,
        IommuError::AlreadyRegistered | IommuError::Busy => PnpError::ResourceBusy {
            kind: PnpResourceKind::Dma,
            detail: "riscv iommu controller registry is busy",
        },
        _ => PnpError::registration_failed(
            PnpResourceKind::Dma,
            "riscv iommu controller registration failed",
        ),
    }
}
