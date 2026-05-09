//! `acpi` 是一个与 ACPI（高级配置与电源接口）交互的 Rust 库。ACPI 是用于电源管理、
//! 设备发现与配置的复杂框架，广泛用于现代 x64、ARM 和 RISC-V 平台。操作系统需要通过
//! ACPI 正确设置平台的中断控制器、执行电源管理并支持其他平台能力。
//!
//! 本 crate 提供无需分配器的受限 API，可在引导加载程序中使用。这部分 API 支持搜索
//! RSDP、枚举可用表以及使用表的原始结构。其他功能位于 `alloc` feature 之后（默认启用），
//! 需要分配器支持。
//!
//! 在有分配器时，本 crate 还提供静态表的高层接口以及 AML（编码在 DSDT 和 SSDT 中的
//! 字节码格式）的动态解释器。
//!
//! ### 使用方式
//! 使用本库需要提供 [`Handler`] trait 的实现，该 trait 允许库请求将物理内存区域映射
//! 到虚拟地址空间等操作。
//!
//! 接下来需要获取 RSDP 或 RSDT/XSDT 的物理地址。获取方法取决于运行平台和启动方式。
//! 如果系统通过 BIOS 启动，可以使用 [`rsdp::Rsdp::search_for_on_bios`]。UEFI 提供了
//! 获取 RSDP 地址的单独机制。
//!
//! 然后需要构造 [`AcpiTables`] 实例，根据已有信息可选择：
//! * 如果有 RSDP 物理地址，使用 [`AcpiTables::from_rsdp`]
//! * 如果有 RSDT/XSDT 物理地址，使用 [`AcpiTables::from_rsdt`]
//!
//! 获取 [`AcpiTables`] 后即可搜索相关表，或使用高层接口如 [`PlatformInfo`]、
//! [`PciConfigRegions`] 或 [`HpetInfo`]。

#![no_std]

extern crate alloc;

pub mod address;
pub mod aml;
pub mod platform;
pub mod registers;
pub mod rsdp;
pub mod sdt;

pub use pci_types::PciAddress;
pub use sdt::{fadt::PowerProfile, hpet::HpetInfo, madt::MadtError};

use crate::sdt::{SdtHeader, Signature};
use core::{
    fmt, mem,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::NonNull,
};
use log::warning as warn;
use rsdp::Rsdp;

/// 在找到 RSDP 或 RSDT/XSDT 后构造，用于枚举系统的 ACPI 表。
pub struct AcpiTables<H: Handler> {
    rsdt_mapping: PhysicalMapping<H, SdtHeader>,
    pub rsdp_revision: u8,
    handler: H,
}

unsafe impl<H> Send for AcpiTables<H> where H: Handler + Send {}
unsafe impl<H> Sync for AcpiTables<H> where H: Handler + Send {}

impl<H> AcpiTables<H>
where
    H: Handler,
{
    /// 从 RSDP 的**物理**地址构造 `AcpiTables`。
    ///
    /// # Safety
    /// RSDP 的地址必须有效。
    pub unsafe fn from_rsdp(handler: H, rsdp_address: usize) -> Result<AcpiTables<H>, AcpiError> {
        let rsdp_mapping = unsafe { handler.map_physical_region::<Rsdp>(rsdp_address, mem::size_of::<Rsdp>()) };

        /*
         * If the address given does not have a correct RSDP signature, the user has probably given
         * us an invalid address, and we should not continue. We're more lenient with other errors
         * as it's probably a real RSDP and the firmware developers are just lazy.
         */
        match rsdp_mapping.validate() {
            Ok(()) => (),
            Err(AcpiError::RsdpIncorrectSignature) => return Err(AcpiError::RsdpIncorrectSignature),
            Err(AcpiError::RsdpInvalidOemId) | Err(AcpiError::RsdpInvalidChecksum) => {
                warn!("RSDP has invalid checksum or OEM ID. Continuing.");
            }
            Err(_) => (),
        }

        let rsdp_revision = rsdp_mapping.revision();
        let rsdt_address = if rsdp_revision == 0 {
            // We're running on ACPI Version 1.0. We should use the 32-bit RSDT address.
            rsdp_mapping.rsdt_address() as usize
        } else {
            /*
             * We're running on ACPI Version 2.0+. We should use the 64-bit XSDT address, truncated
             * to 32 bits on x86.
             */
            rsdp_mapping.xsdt_address() as usize
        };

        unsafe { Self::from_rsdt(handler, rsdp_revision, rsdt_address) }
    }

    /// 从 RSDT/XSDT 的**物理**地址和 RSDP 中的版本号构造 `AcpiTables`。
    ///
    /// # Safety
    /// RSDT 的地址必须有效。
    pub unsafe fn from_rsdt(
        handler: H,
        rsdp_revision: u8,
        rsdt_address: usize,
    ) -> Result<AcpiTables<H>, AcpiError> {
        let rsdt_mapping =
            unsafe { handler.map_physical_region::<SdtHeader>(rsdt_address, mem::size_of::<SdtHeader>()) };
        let rsdt_length = rsdt_mapping.length;
        let rsdt_mapping = unsafe { handler.map_physical_region::<SdtHeader>(rsdt_address, rsdt_length as usize) };
        Ok(Self { rsdt_mapping, rsdp_revision, handler })
    }

    /// 遍历 SDT 的**物理**地址列表。
    pub fn table_entries(&self) -> impl Iterator<Item = usize> {
        let entry_size = if self.rsdp_revision == 0 { 4 } else { 8 };
        let mut table_entries_ptr =
            unsafe { self.rsdt_mapping.virtual_start.as_ptr().byte_add(mem::size_of::<SdtHeader>()) }.cast::<u8>();
        let mut num_entries = (self.rsdt_mapping.region_length - mem::size_of::<SdtHeader>()) / entry_size;

        core::iter::from_fn(move || {
            if num_entries > 0 {
                unsafe {
                    let entry = if entry_size == 4 {
                        *table_entries_ptr.cast::<u32>() as usize
                    } else {
                        *table_entries_ptr.cast::<u64>() as usize
                    };
                    table_entries_ptr = table_entries_ptr.byte_add(entry_size);
                    num_entries -= 1;

                    Some(entry)
                }
            } else {
                None
            }
        })
    }

    /// 遍历每个 SDT 的头部及其**物理**地址。
    pub fn table_headers(&self) -> impl Iterator<Item = (usize, SdtHeader)> {
        self.table_entries().map(|table_phys_address| {
            let mapping = unsafe {
                self.handler.map_physical_region::<SdtHeader>(table_phys_address, mem::size_of::<SdtHeader>())
            };
            (table_phys_address, *mapping)
        })
    }

    /// 查找所有签名为 `T::SIGNATURE` 的表。
    pub fn find_tables<T>(&self) -> impl Iterator<Item = PhysicalMapping<H, T>>
    where
        T: AcpiTable,
    {
        self.table_entries().filter_map(|table_phys_address| {
            let header_mapping = unsafe {
                self.handler.map_physical_region::<SdtHeader>(table_phys_address, mem::size_of::<SdtHeader>())
            };
            if header_mapping.signature == T::SIGNATURE {
                // Extend the mapping to the entire table
                let length = header_mapping.length;
                drop(header_mapping);
                Some(unsafe { self.handler.map_physical_region::<T>(table_phys_address, length as usize) })
            } else {
                None
            }
        })
    }

    /// 查找第一个签名为 `T::SIGNATURE` 的表。
    pub fn find_table<T>(&self) -> Option<PhysicalMapping<H, T>>
    where
        T: AcpiTable,
    {
        self.find_tables().next()
    }

    pub fn dsdt(&self) -> Result<AmlTable, AcpiError> {
        let Some(fadt) = self.find_table::<sdt::fadt::Fadt>() else {
            Err(AcpiError::TableNotFound(Signature::FADT))?
        };
        let phys_address = fadt.dsdt_address()?;
        let header =
            unsafe { self.handler.map_physical_region::<SdtHeader>(phys_address, mem::size_of::<SdtHeader>()) };
        Ok(AmlTable { phys_address, length: header.length, revision: header.revision })
    }

    pub fn ssdts(&self) -> impl Iterator<Item = AmlTable> {
        self.table_headers().filter_map(|(phys_address, header)| {
            if header.signature == Signature::SSDT {
                Some(AmlTable { phys_address, length: header.length, revision: header.revision })
            } else {
                None
            }
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AmlTable {
    /// 表的起始物理地址。加上 `mem::size_of::<SdtHeader>()` 即为 AML 流的起始物理地址。
    pub phys_address: usize,
    /// 表的长度，包含头部。
    pub length: u32,
    pub revision: u8,
}

/// 所有表示 ACPI 表的类型都应实现此 trait。
///
/// ### Safety
/// 表的内存被直接解释，因此必须提供能正确表示表结构的类型。无论提供的类型大小如何，
/// 映射的区域大小始终按 SDT 头中的指定值。如果表定义可能大于有效 SDT 的大小，
/// 应使用 [`ExtendedField`](sdt::ExtendedField) 来定义可能存在也可能不存在的字段。
pub unsafe trait AcpiTable {
    const SIGNATURE: Signature;

    fn header(&self) -> &SdtHeader;

    fn validate(&self) -> Result<(), AcpiError> {
        unsafe { self.header().validate(Self::SIGNATURE) }
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AcpiError {
    NoValidRsdp,
    RsdpIncorrectSignature,
    RsdpInvalidOemId,
    RsdpInvalidChecksum,

    SdtInvalidSignature(Signature),
    SdtInvalidOemId(Signature),
    SdtInvalidTableId(Signature),
    SdtInvalidChecksum(Signature),
    SdtInvalidCreatorId(Signature),

    TableNotFound(Signature),
    InvalidFacsAddress,
    InvalidDsdtAddress,
    InvalidMadt(MadtError),
    InvalidGenericAddress,

    Timeout,

    #[cfg(feature = "aml")]
    Aml(aml::AmlError),

    /// This is emitted to signal that the library does not support the requested behaviour. This
    /// should eventually never be emitted.
    LibUnimplemented,

    /// This can be returned by the host (user of the library) to signal that required behaviour
    /// has not been implemented. This will cause the error to be propagated back to the host if an
    /// operation that requires that behaviour is performed.
    HostUnimplemented,
}

/// Describes a physical mapping created by [`Handler::map_physical_region`] and unmapped by
/// [`Handler::unmap_physical_region`]. The region mapped must be at least `size_of::<T>()`
/// bytes, but may be bigger.
pub struct PhysicalMapping<H, T>
where
    H: Handler,
{
    /// The physical address of the mapped structure. The actual mapping may start at a lower address
    /// if the requested physical address is not well-aligned.
    pub physical_start: usize,
    /// The virtual address of the mapped structure. It must be a valid, non-null pointer to the
    /// start of the requested structure. The actual virtual mapping may start at a lower address
    /// if the requested address is not well-aligned.
    pub virtual_start: NonNull<T>,
    /// The size of the requested region, in bytes. Can be equal or larger to `size_of::<T>()`. If a
    /// larger region has been mapped, this should still be the requested size.
    pub region_length: usize,
    /// The total size of the produced mapping. This may be the same as `region_length`, or larger to
    /// meet requirements of the mapping implementation.
    pub mapped_length: usize,
    /// The [`Handler`] that was used to produce the mapping. When this mapping is dropped, this
    /// handler will be used to unmap the region.
    pub handler: H,
}

impl<H, T> PhysicalMapping<H, T>
where
    H: Handler,
{
    /// Get a pinned reference to the inner `T`. This is generally only useful if `T` is `!Unpin`,
    /// otherwise the mapping can simply be dereferenced to access the inner type.
    pub fn get(&self) -> Pin<&T> {
        unsafe { Pin::new_unchecked(self.virtual_start.as_ref()) }
    }
}

impl<H, T> fmt::Debug for PhysicalMapping<H, T>
where
    H: Handler,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhysicalMapping")
            .field("physical_start", &self.physical_start)
            .field("virtual_start", &self.virtual_start)
            .field("region_length", &self.region_length)
            .field("mapped_length", &self.mapped_length)
            .field("handler", &())
            .finish()
    }
}

unsafe impl<H: Handler + Send, T: Send> Send for PhysicalMapping<H, T> {}

impl<H, T> Deref for PhysicalMapping<H, T>
where
    T: Unpin,
    H: Handler,
{
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.virtual_start.as_ref() }
    }
}

impl<H, T> DerefMut for PhysicalMapping<H, T>
where
    T: Unpin,
    H: Handler,
{
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.virtual_start.as_mut() }
    }
}

impl<H, T> Drop for PhysicalMapping<H, T>
where
    H: Handler,
{
    fn drop(&mut self) {
        H::unmap_physical_region(self)
    }
}

/// A `Handle` is an opaque reference to an object that is managed by the host on behalf of this
/// library.
///
/// The library will treat the value of a handle as entirely opaque. You may manage handles
/// however you wish, and the same value can be used to refer to objects of different types, if
/// desired.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Handle(pub u32);

/// An implementation of this trait must be provided to allow `acpi` to perform operations that
/// interface with the underlying hardware and other systems in your host implementation. This
/// interface is designed to be flexible to allow usage of the library from a variety of settings.
///
/// Depending on your usage of this library, not all functionality may be required. If you do not
/// provide certain functionality, you should return [`AcpiError::HostUnimplemented`]. The library
/// will attempt to propagate this error back to the host if an operation cannot be performed
/// without that functionality.
///
/// The `Handler` must be cheaply clonable (e.g. a reference, `Arc`, marker struct, etc.) as a copy
/// of the handler is stored in various structures, such as in each [`PhysicalMapping`] to
/// facilitate unmapping.
pub trait Handler: Clone {
    /// Given a physical address and a size, map a region of physical memory that contains `T` (note: the passed
    /// size may be larger than `size_of::<T>()`). The address is not neccessarily page-aligned, so the
    /// implementation may need to map more than `size` bytes. The virtual address the region is mapped to does not
    /// matter, as long as it is accessible to `acpi`. Refer to the fields on [`PhysicalMapping`] to understand how
    /// to produce one properly.
    ///
    /// ## Safety
    ///
    /// - `physical_address` must point to a valid `T` in physical memory.
    /// - `size` must be at least `size_of::<T>()`.
    unsafe fn map_physical_region<T>(&self, physical_address: usize, size: usize) -> PhysicalMapping<Self, T>;

    /// Unmap the given physical mapping. This is called when a `PhysicalMapping` is dropped, you should **not** manually call this.
    ///
    /// Note: A reference to the `Handler` used to construct `region` can be acquired by calling [`PhysicalMapping::mapper`].
    fn unmap_physical_region<T>(region: &PhysicalMapping<Self, T>);

    // TODO: maybe we should map stuff ourselves in the AML interpreter and do this internally?
    // Maybe provide a hook for tracing the IO / emit trace events ourselves if we do do that?
    fn read_u8(&self, address: usize) -> u8;
    fn read_u16(&self, address: usize) -> u16;
    fn read_u32(&self, address: usize) -> u32;
    fn read_u64(&self, address: usize) -> u64;

    fn write_u8(&self, address: usize, value: u8);
    fn write_u16(&self, address: usize, value: u16);
    fn write_u32(&self, address: usize, value: u32);
    fn write_u64(&self, address: usize, value: u64);

    // TODO: would be nice to provide defaults that just do the actual port IO on x86?
    fn read_io_u8(&self, port: u16) -> u8;
    fn read_io_u16(&self, port: u16) -> u16;
    fn read_io_u32(&self, port: u16) -> u32;

    fn write_io_u8(&self, port: u16, value: u8);
    fn write_io_u16(&self, port: u16, value: u16);
    fn write_io_u32(&self, port: u16, value: u32);

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8;
    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16;
    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32;

    fn write_pci_u8(&self, address: PciAddress, offset: u16, value: u8);
    fn write_pci_u16(&self, address: PciAddress, offset: u16, value: u16);
    fn write_pci_u32(&self, address: PciAddress, offset: u16, value: u32);

    /// Returns a monotonically-increasing value of nanoseconds.
    fn nanos_since_boot(&self) -> u64;

    /// Stall for at least the given number of **microseconds**. An implementation should not relinquish control of
    /// the processor during the stall, and for this reason, firmwares should not stall for periods of more than
    /// 100 microseconds.
    fn stall(&self, microseconds: u64);

    /// Sleep for at least the given number of **milliseconds**. An implementation may round to the closest sleep
    /// time supported, and should relinquish the processor.
    fn sleep(&self, milliseconds: u64);
}

/// Host operations needed by the AML interpreter.
///
/// This is deliberately separate from [`Handler`], so users that only need
/// static-table parsing do not have to provide AML mutex/debug policy. Hosts
/// that instantiate [`aml::Interpreter`] must implement this trait explicitly.
pub trait AmlHandler: Handler {
    fn create_mutex(&self) -> Handle;

    /// Acquire the mutex referred to by the given handle. `timeout` is a millisecond timeout value
    /// with the following meaning:
    ///    - `0` - try to acquire the mutex once, in a non-blocking manner. If the mutex cannot be
    ///      acquired immediately, return `Err(AmlError::MutexAcquireTimeout)`
    ///    - `1-0xfffe` - try to acquire the mutex for at least `timeout` milliseconds.
    ///    - `0xffff` - try to acquire the mutex indefinitely. Should not return `MutexAcquireTimeout`.
    ///
    /// AML mutexes are **reentrant** - that is, a thread may acquire the same mutex more than once
    /// without causing a deadlock.
    fn acquire(&self, mutex: Handle, timeout: u16) -> Result<(), aml::AmlError>;

    fn release(&self, mutex: Handle);

    fn breakpoint(&self) {}

    fn handle_debug(&self, _object: &aml::object::Object) {}

    fn handle_fatal_error(&self, fatal_type: u8, fatal_code: u32, fatal_arg: u64) {
        panic!(
            "Fatal error while executing AML (encountered DefFatalOp). fatal_type = {}, fatal_code = {}, fatal_arg = {}",
            fatal_type, fatal_code, fatal_arg
        );
    }
}
