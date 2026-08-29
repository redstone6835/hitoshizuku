//! x86 I/O port 访问。
//!
//! 只有设备驱动层应使用这些函数；端口号和访问宽度必须由 ACPI/PCI 资源
//! 描述验证后传入。hosted 构建提供无副作用回退，便于驱动单元测试。

use core::sync::atomic::{AtomicBool, Ordering};

use general::{StartAcpiHostOps, StartAcpiIoOps, StartAcpiPciOps};

const PCI_CONFIG_ADDRESS: u16 = 0x0cf8;
const PCI_CONFIG_DATA: u16 = 0x0cfc;
const PCI_ENABLE: u32 = 1 << 31;
static PCI_CONFIG_LOCK: AtomicBool = AtomicBool::new(false);

/// The legacy configuration mechanism is a single globally shared address/data
/// pair. Linux protects it with `raw_spin_lock_irqsave`; keep the same ordering
/// here so an interrupt cannot observe a half-written address.
struct PciConfigGuard {
    irq_state: usize,
}

impl PciConfigGuard {
    #[inline]
    fn acquire() -> Self {
        let irq_state = super::interrupt::save_and_disable();
        while PCI_CONFIG_LOCK
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            super::specific::cpu_relax();
        }
        Self { irq_state }
    }
}

impl Drop for PciConfigGuard {
    fn drop(&mut self) {
        PCI_CONFIG_LOCK.store(false, Ordering::Release);
        super::interrupt::restore(self.irq_state);
    }
}

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    #[cfg(target_os = "none")]
    {
        let value: u8;
        // Safety: 调用方保证当前 CPL 具有该 I/O bitmap/端口权限。
        // Port I/O is a compiler memory barrier in Linux's inb/outb helpers;
        // `nomem` would allow surrounding MMIO accesses to move across it.
        unsafe {
            core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nostack));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = port;
        0
    }
}

#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    #[cfg(target_os = "none")]
    {
        let value: u16;
        unsafe {
            core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nostack));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = port;
        0
    }
}

#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    #[cfg(target_os = "none")]
    {
        let value: u32;
        unsafe {
            core::arch::asm!("in eax, dx", in("dx") port, out("eax") value, options(nostack));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = port;
        0
    }
}

#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (port, value);
    }
}

#[inline]
pub unsafe fn outw(port: u16, value: u16) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (port, value);
    }
}

#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (port, value);
    }
}

/// 传统 ISA 设备要求的短 I/O 延迟（向端口 0x80 写入零）。
#[inline]
pub unsafe fn io_wait() {
    #[cfg(target_os = "none")]
    unsafe {
        outb(0x80, 0);
    }
}

/// 类型化端口访问器，集中检查端口地址并避免宽度误用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Port<T> {
    number: u16,
    _marker: core::marker::PhantomData<T>,
}

impl Port<u8> {
    pub const fn new(number: u16) -> Self {
        Self {
            number,
            _marker: core::marker::PhantomData,
        }
    }

    /// # Safety
    /// 调用方必须持有该端口的 I/O 权限。
    pub unsafe fn read(self) -> u8 {
        unsafe { inb(self.number) }
    }

    /// # Safety
    /// 调用方必须持有该端口的 I/O 权限。
    pub unsafe fn write(self, value: u8) {
        unsafe { outb(self.number, value) }
    }
}

impl Port<u16> {
    pub const fn new(number: u16) -> Self {
        Self {
            number,
            _marker: core::marker::PhantomData,
        }
    }

    pub unsafe fn read(self) -> u16 {
        unsafe { inw(self.number) }
    }

    pub unsafe fn write(self, value: u16) {
        unsafe { outw(self.number, value) }
    }
}

impl Port<u32> {
    pub const fn new(number: u16) -> Self {
        Self {
            number,
            _marker: core::marker::PhantomData,
        }
    }

    pub unsafe fn read(self) -> u32 {
        unsafe { inl(self.number) }
    }

    pub unsafe fn write(self, value: u32) {
        unsafe { outl(self.number, value) }
    }
}

#[inline]
const fn valid_port_range(port: u16, width: u16) -> bool {
    width != 0 && (port as u32).saturating_add(width as u32) <= 0x1_0000
}

#[inline]
const fn pci_config_address(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
    width: u16,
) -> Option<u32> {
    // Mechanism #1 addresses segment zero and the conventional 256-byte PCI
    // configuration space. Extended (ECAM) access is installed separately
    // from MCFG and never silently routed through CF8/CFC.
    if segment != 0
        || device >= 32
        || function >= 8
        || width == 0
        || !width.is_power_of_two()
        || offset & (width - 1) != 0
        // Mechanism #1 selects a 32-bit dword, but byte/word accesses may use
        // the final byte/word within the 256-byte legacy configuration space.
        // Checking the complete access width avoids rejecting offsets 0xfd..
        // 0xff for byte reads (and 0xfe for a word read).
        || (offset as u32).saturating_add(width as u32) > 0x100
    {
        return None;
    }
    Some(
        PCI_ENABLE
            | (bus as u32) << 16
            | (device as u32) << 11
            | (function as u32) << 8
            | (offset as u32 & 0xfc),
    )
}

#[inline]
fn legacy_pci_read32(address: u32) -> u32 {
    #[cfg(target_os = "none")]
    {
        let _guard = PciConfigGuard::acquire();
        // Safety: the address was constructed by `pci_config_address`.
        unsafe {
            outl(PCI_CONFIG_ADDRESS, address);
            inl(PCI_CONFIG_DATA)
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = address;
        u32::MAX
    }
}

#[inline]
fn legacy_pci_write32(address: u32, value: u32) {
    #[cfg(target_os = "none")]
    {
        let _guard = PciConfigGuard::acquire();
        // Safety: the address was constructed by `pci_config_address`.
        unsafe {
            outl(PCI_CONFIG_ADDRESS, address);
            outl(PCI_CONFIG_DATA, value);
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (address, value);
    }
}

#[inline]
fn legacy_pci_update32(address: u32, mask: u32, value: u32) {
    #[cfg(target_os = "none")]
    {
        let _guard = PciConfigGuard::acquire();
        // Keep the read/modify/write pair under one CF8/CFC transaction lock;
        // splitting it would let an IRQ handler overwrite the address latch.
        unsafe {
            outl(PCI_CONFIG_ADDRESS, address);
            let old = inl(PCI_CONFIG_DATA);
            outl(PCI_CONFIG_ADDRESS, address);
            outl(PCI_CONFIG_DATA, (old & !mask) | (value & mask));
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (address, mask, value);
    }
}

fn acpi_read_u8(port: u16) -> u8 {
    if !valid_port_range(port, 1) {
        return 0xff;
    }
    unsafe { inb(port) }
}

fn acpi_read_u16(port: u16) -> u16 {
    if !valid_port_range(port, 2) || port & 1 != 0 {
        return u16::MAX;
    }
    unsafe { inw(port) }
}

fn acpi_read_u32(port: u16) -> u32 {
    if !valid_port_range(port, 4) || port & 3 != 0 {
        return u32::MAX;
    }
    unsafe { inl(port) }
}

fn acpi_write_u8(port: u16, value: u8) {
    if valid_port_range(port, 1) {
        unsafe { outb(port, value) }
    }
}

fn acpi_write_u16(port: u16, value: u16) {
    if valid_port_range(port, 2) && port & 1 == 0 {
        unsafe { outw(port, value) }
    }
}

fn acpi_write_u32(port: u16, value: u32) {
    if valid_port_range(port, 4) && port & 3 == 0 {
        unsafe { outl(port, value) }
    }
}

fn acpi_pci_read_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    let Some(address) = pci_config_address(segment, bus, device, function, offset, 1) else {
        return 0xff;
    };
    (legacy_pci_read32(address) >> ((offset & 3) * 8)) as u8
}

fn acpi_pci_read_u16(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
    let Some(address) = pci_config_address(segment, bus, device, function, offset, 2) else {
        return u16::MAX;
    };
    (legacy_pci_read32(address) >> ((offset & 2) * 8)) as u16
}

fn acpi_pci_read_u32(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    let Some(address) = pci_config_address(segment, bus, device, function, offset, 4) else {
        return u32::MAX;
    };
    legacy_pci_read32(address)
}

fn acpi_pci_write_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u8) {
    let Some(address) = pci_config_address(segment, bus, device, function, offset, 1) else {
        return;
    };
    let shift = (offset & 3) * 8;
    legacy_pci_update32(address, 0xff << shift, u32::from(value) << shift);
}

fn acpi_pci_write_u16(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u16) {
    let Some(address) = pci_config_address(segment, bus, device, function, offset, 2) else {
        return;
    };
    let shift = (offset & 2) * 8;
    legacy_pci_update32(address, 0xffff << shift, u32::from(value) << shift);
}

fn acpi_pci_write_u32(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u32) {
    let Some(address) = pci_config_address(segment, bus, device, function, offset, 4) else {
        return;
    };
    legacy_pci_write32(address, value);
}

/// x86 ACPI host capabilities.  Both callback tables are static POD values,
/// so loaders can put them directly into `StartAcpiHostOps` without borrowing
/// a temporary runtime object.
pub fn acpi_host_ops() -> StartAcpiHostOps {
    StartAcpiHostOps {
        io: Some(StartAcpiIoOps {
            read_u8: acpi_read_u8,
            read_u16: acpi_read_u16,
            read_u32: acpi_read_u32,
            write_u8: acpi_write_u8,
            write_u16: acpi_write_u16,
            write_u32: acpi_write_u32,
        }),
        pci: Some(StartAcpiPciOps {
            read_u8: acpi_pci_read_u8,
            read_u16: acpi_pci_read_u16,
            read_u32: acpi_pci_read_u32,
            write_u8: acpi_pci_write_u8,
            write_u16: acpi_pci_write_u16,
            write_u32: acpi_pci_write_u32,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ports_keep_number_and_width() {
        let byte = Port::<u8>::new(0x3f8);
        let word = Port::<u16>::new(0x3f8);
        assert_eq!(byte.number, word.number);
        assert_eq!(unsafe { byte.read() }, 0);
    }

    #[test]
    fn pci_mechanism_one_address_is_strictly_encoded() {
        assert_eq!(
            pci_config_address(0, 2, 3, 4, 0x10, 4),
            Some(PCI_ENABLE | (2 << 16) | (3 << 11) | (4 << 8) | 0x10)
        );
        assert_eq!(pci_config_address(1, 0, 0, 0, 0, 4), None);
        assert_eq!(pci_config_address(0, 0, 32, 0, 0, 4), None);
        assert_eq!(pci_config_address(0, 0, 0, 0, 0x100, 4), None);
        assert_eq!(pci_config_address(0, 0, 0, 0, 3, 2), None);
        assert!(pci_config_address(0, 0, 0, 0, 0xff, 1).is_some());
        assert!(pci_config_address(0, 0, 0, 0, 0xfe, 2).is_some());
        assert!(pci_config_address(0, 0, 0, 0, 0xfd, 2).is_none());
        assert!(pci_config_address(0, 0, 0, 0, 0xfc, 4).is_some());
        assert!(pci_config_address(0, 0, 0, 0, 0xfd, 4).is_none());
    }

    #[test]
    fn acpi_host_ops_always_has_checked_tables() {
        let ops = acpi_host_ops();
        assert!(ops.io.is_some());
        assert!(ops.pci.is_some());
        let pci = ops.pci.expect("PCI callback table");
        assert_eq!((pci.read_u32)(1, 0, 0, 0, 0), u32::MAX);
        assert_eq!((pci.read_u32)(0, 0, 32, 0, 0), u32::MAX);
    }
}
